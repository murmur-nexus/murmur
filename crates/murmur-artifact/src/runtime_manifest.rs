use std::{fs, net::IpAddr, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::{
    manifest_path::MANIFEST_FILENAME,
    trace_capture::{resolve_trace_capture, TraceCapture},
    unknown_manifest_keys::{nearest_known_key, UnknownManifestKey},
};

// ── Lifecycle types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAcceptance {
    None,
    #[default]
    Single,
    Queue,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterTask {
    #[default]
    Exit,
    Sleep,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMode {
    #[default]
    Stateless,
    Threaded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    pub task_acceptance: TaskAcceptance,
    pub after_task: AfterTask,
    pub queue_depth: usize,
    /// If set, request-input calls that receive no follow-up within this many seconds
    /// transition the task to "failed" with reason "input-timeout". Absent = wait indefinitely.
    pub input_timeout_secs: Option<u64>,
    /// Whether tasks sharing a contextId accumulate conversation history across turns.
    /// Stateless (default): each task is fully independent; Threaded: history is loaded
    /// and persisted per contextId within a session.
    pub conversation_mode: ConversationMode,
    /// Maximum times an `on-task-end` hook may reopen a single task (re-run its agent
    /// loop with injected feedback). Defaults to 1 when absent. `0` disables reopening
    /// entirely. Unlike `inference.max_turns`, `0` is a valid explicit value. Reopening
    /// never grants turns past `inference.max_turns` — the two budgets share one
    /// cumulative turn count.
    pub max_task_reopens: u32,
    /// How long a shell command runs in the foreground before it is demoted to the background.
    /// Defaults to 10 seconds. A command that exits inside this window returns its output to the
    /// turn as it always has; one that outruns it hands the turn a handle, keeps running, and
    /// reports back later as a `completion`-origin task. `0` demotes at the first poll after the
    /// spawn, so effectively every command detaches.
    pub shell_grace_secs: u64,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            task_acceptance: TaskAcceptance::Single,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            conversation_mode: ConversationMode::Stateless,
            max_task_reopens: 1,
            shell_grace_secs: 10,
        }
    }
}

/// Lifecycle override: allows a parent (mur-roost) or CLI flag to constrain
/// the lifecycle down from what the manifest declares.
#[derive(Debug, Clone, Default)]
pub struct LifecycleOverride {
    pub task_acceptance: Option<TaskAcceptance>,
    pub after_task: Option<AfterTask>,
}

/// Artifact taxonomy for entries declared in a capsule manifest.
///
/// Variant meanings:
///
/// - `Tool`: agent-callable tool (WASM component or native binary). Visible in MURMUR.md and the
///   LLM tool list. Implementation details (wasm vs native) are declared in the artifact's own
///   murmur.yaml via `implementation:`.
/// - `Driver`: inference driver implemented as a WASM component. The runtime calls it as a
///   processor. Hidden from the LLM.
/// - `Hook`: event-triggered artifact implemented as a WASM component. The runtime calls it at
///   lifecycle points; behavioral contract (binding, execution_mode, commit_policy) is declared
///   in the artifact's own murmur.yaml. Hidden from the LLM.
/// - `Skill`: documentation artifact; no executable binary. Visible in the LLM tool inventory.
///   Invoking a skill by name returns its `skill.md` content as the tool result (just-in-time
///   injection). No WASM/native dispatch occurs. A skill bound via
///   `inference.system_prompt_artifact` is excluded from the callable inventory because it is
///   already part of the system prompt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactRuntime {
    Tool,
    Driver,
    Hook,
    Skill,
}

impl ArtifactRuntime {
    /// Whether artifacts of this role appear in the LLM's tool inventory.
    ///
    /// Deliberately an exhaustive match with one arm per variant and no wildcard: a new
    /// `ArtifactRuntime` variant must state its own visibility here, so adding a role that
    /// should be LLM-visible cannot silently default to hidden.
    pub fn is_llm_visible(&self) -> bool {
        match self {
            ArtifactRuntime::Tool => true,
            ArtifactRuntime::Skill => true,
            ArtifactRuntime::Driver => false,
            ArtifactRuntime::Hook => false,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactRuntime::Tool => "tool",
            ArtifactRuntime::Driver => "driver",
            ArtifactRuntime::Hook => "hook",
            ArtifactRuntime::Skill => "skill",
        }
    }
}

/// How a `runtime: tool` artifact is implemented.
///
/// Declared via `implementation:` in the artifact's own murmur.yaml.
/// Defaults to `Wasm` when the field is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactImplementation {
    Wasm,
    Native,
}

/// Read the `implementation:` field from an artifact's murmur.yaml.
/// Defaults to `Wasm` when the field is absent or unparseable.
pub fn read_tool_implementation(
    artifact_manifest_path: &Path,
) -> Result<ArtifactImplementation, RuntimeManifestError> {
    let content =
        fs::read_to_string(artifact_manifest_path).map_err(|source| RuntimeManifestError::Io {
            path: artifact_manifest_path.display().to_string(),
            source,
        })?;
    Ok(parse_tool_implementation_from_yaml(&content))
}

/// Parse `implementation:` from an in-memory manifest YAML string.
/// Defaults to `Wasm` when the field is absent or unparseable.
pub fn parse_tool_implementation_from_yaml(yaml: &str) -> ArtifactImplementation {
    serde_yaml::from_str::<serde_yaml::Value>(yaml)
        .map(|v| {
            v.as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("implementation".to_string())))
                .and_then(serde_yaml::Value::as_str)
                .map(|s| match s {
                    "native" => ArtifactImplementation::Native,
                    _ => ArtifactImplementation::Wasm,
                })
                .unwrap_or(ArtifactImplementation::Wasm)
        })
        .unwrap_or(ArtifactImplementation::Wasm)
}

// ── Hook configuration types ──────────────────────────────────────────────────

/// Which lifecycle event(s) a hook binds to.
///
/// Declared via `binding:` in the artifact's own murmur.yaml.
/// Defaults to `All` when the field is absent (receives all session events).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookBinding {
    /// `"on-stage"` — fires during `stage_session`; always blocking.
    OnStage,
    /// `"on-session-start"` — fires once before the first inference turn.
    OnSessionStart,
    /// `"on-task-start"` — fires once per task, before that task's first inference turn.
    OnTaskStart,
    /// `"on-inference"` — fires after each inference response is parsed.
    OnInference,
    /// `"on-tool-call"` — fires after each model-requested tool invocation.
    OnToolCall,
    /// `"on-shell"` — fires after each shell command returns.
    OnShell,
    /// `"on-compaction"` — fires when the session token threshold is reached.
    OnCompaction,
    /// `"on-task-end"` — fires once per task, after that task's agent loop returns.
    OnTaskEnd,
    /// `"on-session-end"` — fires at session teardown.
    OnSessionEnd,
    /// No `binding:` field — receives all session events (not on-stage).
    All,
}

impl HookBinding {
    /// The manifest spelling of this binding, which is also the WIT lifecycle function
    /// name the runtime dispatches to — the key `capsule-runtime`'s honored-arm table is
    /// written against. [`HookBinding::All`] has no spelling of its own (it is what an
    /// omitted `binding:` means), so it renders as `"all"` for diagnostics only.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OnStage => "on-stage",
            Self::OnSessionStart => "on-session-start",
            Self::OnTaskStart => "on-task-start",
            Self::OnInference => "on-inference",
            Self::OnToolCall => "on-tool-call",
            Self::OnShell => "on-shell",
            Self::OnCompaction => "on-compaction",
            Self::OnTaskEnd => "on-task-end",
            Self::OnSessionEnd => "on-session-end",
            Self::All => "all",
        }
    }
}

/// Whether the runtime waits for the hook result.
///
/// Declared via `execution_mode:` in the artifact's own murmur.yaml.
/// Defaults to `Blocking` when the field is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookExecutionMode {
    /// `"blocking"` — runtime waits for result before proceeding.
    Blocking,
    /// `"async"` — runtime proceeds immediately; result not committed.
    Async,
}

/// What the runtime does with a successful hook output.
///
/// Declared via `commit_policy:` in the artifact's own murmur.yaml.
/// Defaults to `None` when the field is absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCommitPolicy {
    /// `"none"` — output discarded.
    None,
    /// `"replace-context"` — runtime replaces conversation history. Valid only with on-compaction.
    ReplaceContext,
    /// `"write-manifests"` — runtime writes tool manifests to workdir/tools/. Valid only with on-stage.
    WriteManifests,
    /// `"reopen-task"` — runtime re-runs the task's agent loop with the hook's feedback.
    /// Valid only with on-task-end.
    ReopenTask,
    /// `"seed-context"` — runtime places the hook's messages at the head of the task's
    /// first message list, under the `context.seed_budget` ceiling. Valid only with
    /// on-task-start.
    SeedContext,
    /// `"deny"` — the hook decides, immediately before the call is dispatched, whether the
    /// call happens at all. Valid only with on-shell and on-tool-call, and only with an
    /// explicit `binding:` naming one of them.
    ///
    /// The only policy that *subtracts*: every other value commits something the hook
    /// produced, this one refuses something the capsule asked for. It narrows the manifest's
    /// grant and can never widen it.
    Deny,
}

impl HookCommitPolicy {
    /// The manifest spelling of this policy, for diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReplaceContext => "replace-context",
            Self::WriteManifests => "write-manifests",
            Self::ReopenTask => "reopen-task",
            Self::SeedContext => "seed-context",
            Self::Deny => "deny",
        }
    }
}

/// What the runtime does with a lifecycle event for an `execution_mode: async` hook
/// whose job queue is already full.
///
/// Declared via `on_overflow:` on the hook's entry in the **capsule operator's own**
/// manifest — not in the hook artifact's bundled murmur.yaml, because keeping up with
/// the agent loop is the operator's trade-off, not the hook author's. Inert on a hook
/// that turns out to be `execution_mode: blocking` (a blocking hook is never queued);
/// the operator-manifest parser cannot tell the two apart, since the mode lives in the
/// artifact's own manifest and is only known at staging time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOverflowPolicy {
    /// `"drop"` (the default) — discard the event and count it. The agent loop never
    /// waits on a slow hook; telemetry is lossy under sustained overload, and the loss
    /// is reported rather than silent.
    #[default]
    Drop,
    /// `"block"` — the agent loop waits for the hook's worker to make room. No event is
    /// lost, at the cost of putting a slow hook back on the critical path.
    Block,
}

impl HookOverflowPolicy {
    /// The manifest spelling of this policy, for diagnostics.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Drop => "drop",
            Self::Block => "block",
        }
    }
}

/// Behavioral contract for a hook artifact, read from its own murmur.yaml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookConfig {
    pub binding: HookBinding,
    pub execution_mode: HookExecutionMode,
    pub commit_policy: HookCommitPolicy,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            binding: HookBinding::All,
            execution_mode: HookExecutionMode::Blocking,
            commit_policy: HookCommitPolicy::None,
        }
    }
}

/// The one `commit_policy` a hook bound to `binding` can actually have committed, or
/// `None` when no `commit_policy` value other than `none` is ever honored for it.
///
/// This is the manifest-side mirror of `capsule-runtime`'s `HONORED_OUTPUT_ARM` table,
/// which decides — keyed on the same lifecycle function names [`HookBinding::as_str`]
/// produces — which `hook-output` arm the runtime commits for each event. Two bindings
/// return `None` for different reasons, and the distinction matters:
///
/// - `on-session-start` and `on-session-end` honor no arm at all; every output from them
///   is discarded.
/// - `on-tool-call` and `on-shell` honor `deny`, and only at their decision-point dispatch
///   — the one that runs before the call. Their post-call observation dispatch honors
///   nothing, which is why the runtime's table is keyed on the dispatch phase as well as
///   the event name.
/// - `on-inference` *does* honor an arm (`artifact`), but that arm has no
///   `commit_policy` spelling — [`HookCommitPolicy`] has no `Artifact` variant — so no
///   non-`none` policy is declarable for it either.
///
/// [`HookBinding::All`] (an omitted `binding:`) also returns `None`, and that is a third
/// reason again: an `All`-bound hook is dispatched to *every* event, so there is no single
/// honored policy to name. It is deliberately **not** validated against this function —
/// [`parse_hook_config_from_yaml`] accepts every `commit_policy` for it, with the single
/// exception of [`HookCommitPolicy::Deny`], which that function rejects outright for `All`.
#[must_use]
pub fn commit_policy_for_binding(binding: &HookBinding) -> Option<HookCommitPolicy> {
    match binding {
        HookBinding::OnStage => Some(HookCommitPolicy::WriteManifests),
        HookBinding::OnCompaction => Some(HookCommitPolicy::ReplaceContext),
        HookBinding::OnTaskEnd => Some(HookCommitPolicy::ReopenTask),
        HookBinding::OnTaskStart => Some(HookCommitPolicy::SeedContext),
        HookBinding::OnToolCall | HookBinding::OnShell => Some(HookCommitPolicy::Deny),
        HookBinding::OnSessionStart
        | HookBinding::OnInference
        | HookBinding::OnSessionEnd
        | HookBinding::All => None,
    }
}

/// Read a hook's behavioral contract from its murmur.yaml at the given path.
pub fn read_hook_config(artifact_manifest_path: &Path) -> Result<HookConfig, RuntimeManifestError> {
    let content =
        fs::read_to_string(artifact_manifest_path).map_err(|source| RuntimeManifestError::Io {
            path: artifact_manifest_path.display().to_string(),
            source,
        })?;
    parse_hook_config_from_yaml(&content).map_err(|msg| {
        RuntimeManifestError::YamlSyntax(format!("{}: {msg}", artifact_manifest_path.display()))
    })
}

/// Parse hook behavioral contract from an in-memory manifest YAML string.
///
/// `commit_policy: deny` is the one policy an omitted `binding:` cannot carry. Every other
/// policy is accepted for [`HookBinding::All`] because an `All`-bound hook reaches every
/// event and the runtime simply discards what a given event does not honor. `deny` is not
/// discardable in that way: it is answered at a *decision point* standing in front of a call,
/// and an `All`-bound hook would be asked to decide on shell and tool calls it was never
/// written to judge — with every failure to answer, including a trap or a timeout, refusing
/// the call. The binding is therefore required to name which of the two events the hook gates.
pub fn parse_hook_config_from_yaml(yaml: &str) -> Result<HookConfig, String> {
    let v: serde_yaml::Value =
        serde_yaml::from_str(yaml).map_err(|e| format!("YAML parse error: {e}"))?;

    let binding = v
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("binding".to_string())))
        .and_then(serde_yaml::Value::as_str)
        .map(|s| match s {
            "on-stage" => Ok(HookBinding::OnStage),
            "on-session-start" => Ok(HookBinding::OnSessionStart),
            "on-task-start" => Ok(HookBinding::OnTaskStart),
            "on-inference" => Ok(HookBinding::OnInference),
            "on-tool-call" => Ok(HookBinding::OnToolCall),
            "on-shell" => Ok(HookBinding::OnShell),
            "on-compaction" => Ok(HookBinding::OnCompaction),
            "on-task-end" => Ok(HookBinding::OnTaskEnd),
            "on-session-end" => Ok(HookBinding::OnSessionEnd),
            other => Err(format!(
                "unknown binding '{other}'; expected: on-stage, on-session-start, on-task-start, \
                 on-inference, on-tool-call, on-shell, on-compaction, on-task-end, on-session-end"
            )),
        })
        .transpose()?
        .unwrap_or(HookBinding::All);

    let execution_mode = v
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("execution_mode".to_string())))
        .and_then(serde_yaml::Value::as_str)
        .map(|s| match s {
            "blocking" => Ok(HookExecutionMode::Blocking),
            "async" => Ok(HookExecutionMode::Async),
            other => Err(format!(
                "unknown execution_mode '{other}'; expected: blocking, async"
            )),
        })
        .transpose()?
        .unwrap_or(HookExecutionMode::Blocking);

    let commit_policy = v
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("commit_policy".to_string())))
        .and_then(serde_yaml::Value::as_str)
        .map(|s| match s {
            "none" => Ok(HookCommitPolicy::None),
            "replace-context" => Ok(HookCommitPolicy::ReplaceContext),
            "write-manifests" => Ok(HookCommitPolicy::WriteManifests),
            "reopen-task" => Ok(HookCommitPolicy::ReopenTask),
            "seed-context" => Ok(HookCommitPolicy::SeedContext),
            "deny" => Ok(HookCommitPolicy::Deny),
            other => Err(format!(
                "unknown commit_policy '{other}'; expected: none, replace-context, write-manifests, \
                 reopen-task, seed-context, deny"
            )),
        })
        .transpose()?
        .unwrap_or(HookCommitPolicy::None);

    // Validation
    if execution_mode == HookExecutionMode::Async && commit_policy != HookCommitPolicy::None {
        return Err(format!(
            "async-with-commit not supported: execution_mode 'async' requires commit_policy 'none' \
             (got '{}')",
            commit_policy.as_str()
        ));
    }
    if binding == HookBinding::OnStage && execution_mode == HookExecutionMode::Async {
        return Err(
            "on-stage hooks must be blocking; execution_mode 'async' is not valid for binding \
             'on-stage'"
                .to_string(),
        );
    }
    // The one policy `All` cannot carry: a hook that does not name which of the two gated
    // events it decides on would be dispatched at decision points it was never written for,
    // and every non-`none` answer there refuses a call.
    if binding == HookBinding::All && commit_policy == HookCommitPolicy::Deny {
        return Err(
            "commit_policy 'deny' requires an explicit binding: 'on-shell' or 'on-tool-call'; \
             a hook with no binding: is dispatched at every event, including decision points \
             it was not written to decide"
                .to_string(),
        );
    }
    // `binding` is the single source of truth for what the runtime commits; a declared
    // `commit_policy` the binding can never honor is a mistake that is fully knowable here,
    // at staging time, rather than mid-session as a `hook_dispatch_error`. An omitted
    // `binding:` (`All`) reaches every event, so every other policy stays valid for it.
    if binding != HookBinding::All && commit_policy != HookCommitPolicy::None {
        let honored = commit_policy_for_binding(&binding);
        if honored.as_ref() != Some(&commit_policy) {
            let note = if binding == HookBinding::OnInference {
                " (on-inference commits an 'artifact' output, which has no commit_policy spelling)"
            } else {
                ""
            };
            return Err(format!(
                "commit_policy '{}' is not valid for binding '{}'; binding '{}' honors \
                 commit_policy '{}'{note}",
                commit_policy.as_str(),
                binding.as_str(),
                binding.as_str(),
                honored.as_ref().map_or("none", HookCommitPolicy::as_str)
            ));
        }
    }

    Ok(HookConfig {
        binding,
        execution_mode,
        commit_policy,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeManifest {
    pub name: String,
    pub version: String,
    pub artifacts: Vec<RuntimeArtifact>,
    pub capabilities: Option<Capabilities>,
    pub inference: Option<InferenceConfig>,
    pub context: Option<ContextConfig>,
    pub observability: Option<ObservabilityConfig>,
    pub trace: Option<TraceConfig>,
    pub network: Option<NetworkConfig>,
    pub lifecycle: Option<LifecycleConfig>,
    /// Read-only views onto the workdir that the operator opens to processes outside the capsule.
    /// `None` means nothing is exported and every request to the resource plane is denied.
    pub exports: Option<Exports>,
    /// Pins the mur runtime version required by this capsule.
    /// Used by `mur deploy` to select the binary version to install on the VM,
    /// and by `mur run` to warn on version mismatch.
    /// If absent, the running mur binary's version is used.
    pub mur_version: Option<String>,
    /// Every key the manifest declared that this build does not recognize, captured by each
    /// `Raw*` block's `#[serde(flatten)]` overflow map instead of dropped.
    ///
    /// Ordered by the walk rather than by the manifest's own line order: a containing block's own
    /// keys precede the blocks nested inside it, and one block's keys are alphabetical.
    ///
    /// Empty for a manifest written entirely in keys this build knows. Never a reason to refuse a
    /// manifest — it is reported as `W-SEC-019` by
    /// [`crate::unknown_manifest_keys::warn_on_unknown_manifest_keys`] and nothing else reads it.
    pub unknown_keys: Vec<UnknownManifestKey>,
}

impl RuntimeManifest {
    pub fn effective_lifecycle(&self) -> LifecycleConfig {
        self.lifecycle.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Internal port the capsule expects to listen on. Default 14159 when absent.
    pub internal_port: Option<u16>,
}

// ── Exports ───────────────────────────────────────────────────────────────────

/// Per-file read ceiling applied when `exports.files.max_bytes` is absent: 10Mi.
pub const DEFAULT_EXPORT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// What the operator discloses to processes *outside* the capsule, declared under the top-level
/// `exports:` key.
///
/// A sibling of `capabilities:` rather than a member of it: a capability is something the guest
/// holds and an export is a disclosure the operator makes. Declaring one
/// gives the agent nothing it did not already have — it widens only the operator's reach inward,
/// which is why it never enters the achieved-containment computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exports {
    /// The declared read-only file surface. `None` — an `exports:` block with no `files:` —
    /// means the resource plane is not declared and every request to it is denied.
    pub files: Option<FileExport>,
    /// The declared peer-handoff surface. A separate authoriser from [`Self::files`], over a
    /// separate subtree: declaring one grants nothing about the other, and a capsule may declare
    /// either, both or neither. `None` means the capsule mints no handles and its peer plane
    /// answers `no_peer_plane`.
    pub peer_files: Option<PeerFilesExport>,
}

/// The `exports.files` block: one subtree of the workdir, readable and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileExport {
    /// Subtree of the capsule's accessible workdir this export opens, verbatim as declared
    /// (`out/`). Relative, non-empty and free of `..`, checked here; resolving it against a real
    /// directory belongs to the runtime, which is the only component that may decide a path is
    /// inside the root.
    ///
    /// Not required to exist when the capsule launches — the agent may create it during a task.
    pub root: String,
    /// Required, with exactly one legal value. A required field with one value is what makes the
    /// read-only posture an explicit operator statement rather than an unstated default, and
    /// leaves a future write mode somewhere to be declared rather than somewhere to leak in.
    pub mode: ExportMode,
    /// Per-file read ceiling in bytes, defaulting to [`DEFAULT_EXPORT_MAX_BYTES`]. Never an
    /// aggregate budget: each file is judged on its own size, and a subtree whose total exceeds
    /// this is still listed in full.
    pub max_bytes: u64,
}

/// Handle lifetime applied when `exports.peer_files.max_ttl` is absent and the capsule is
/// ephemeral (`lifecycle.after_task: exit`): 1h.
pub const DEFAULT_PEER_HANDLE_TTL_SECS: u64 = 3600;

/// The largest `exports.peer_files.max_ttl` a *persistent* capsule
/// (`lifecycle.after_task: sleep`) may declare: 15m.
///
/// An ephemeral capsule needs no ceiling because teardown destroys the minting key and with it
/// every outstanding handle. A persistent one has withdrawn that bound, so the declared lifetime
/// becomes the only one — and a handle is not a durability mechanism. Enforced by the runtime,
/// not here, because the rule reads `lifecycle.after_task` and this parser sees one block at a
/// time.
pub const PERSISTENT_PEER_HANDLE_TTL_CEILING_SECS: u64 = 900;

/// Per-file read ceiling applied when `exports.peer_files.max_bytes` is absent: 10Mi.
pub const DEFAULT_PEER_FILES_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The `exports.peer_files` block: the one subtree a `share-file` handle may name.
///
/// Deliberately not a mode of [`FileExport`]. The operator plane addresses files by path and
/// enumerates them; this plane has no `list` verb and no path addressing at all, and its audience
/// is another capsule rather than the operator. Two authorisers over two subtrees, so opening one
/// never widens the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerFilesExport {
    /// Subtree of the capsule's accessible workdir a handle may name, verbatim as declared
    /// (`out/`). Relative, non-empty and free of `..`, checked here; resolving it against a real
    /// directory belongs to the runtime. Not required to exist when the capsule launches.
    pub root: String,
    /// The declared `max_ttl`, in seconds, or `None` when the manifest declared none.
    ///
    /// Kept undefaulted because the two ephemerality cases answer an absent value differently:
    /// an ephemeral capsule falls back to [`DEFAULT_PEER_HANDLE_TTL_SECS`], and a persistent one
    /// refuses to launch. Substituting the default here would erase the difference before
    /// anything could act on it. See [`Self::effective_max_ttl_secs`].
    pub max_ttl_secs: Option<u64>,
    /// Per-file ceiling on a redeemed read, defaulting to [`DEFAULT_PEER_FILES_MAX_BYTES`].
    /// Per file, never an aggregate budget — the same terms as [`FileExport::max_bytes`], and a
    /// separate value from it.
    pub max_bytes: u64,
}

impl PeerFilesExport {
    /// The handle lifetime ceiling this export actually applies: the declared `max_ttl`, or the
    /// ephemeral default when none was declared.
    ///
    /// Correct for a persistent capsule too, because one that declared no `max_ttl` never
    /// launches — the runtime refuses it before a plane is built.
    pub fn effective_max_ttl_secs(&self) -> u64 {
        self.max_ttl_secs.unwrap_or(DEFAULT_PEER_HANDLE_TTL_SECS)
    }
}

/// The `capabilities.peer_fetch` block: which peers this capsule may redeem a handle against.
///
/// A sibling of [`NetworkCapabilities`] rather than a member of it. Fetching a peer's bytes lands
/// a file in this capsule's own workdir, so it is an ingestion path and a prompt-injection
/// surface, and it gets its own operator control. Declaring a destination here does not widen
/// `capabilities.network.allow`, and a destination allowed there is not redeemable unless it is
/// also named here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerFetchCapabilities {
    /// Network destinations a handle may be redeemed against. Required and non-empty when the
    /// block is present: an empty list is a parse error, never a silent deny. Same syntax as
    /// `capabilities.network.allow`, and matched by the same runtime rule matcher.
    pub allow: Vec<String>,
}

/// The accepted spelling of every duration in the manifest, stated once so the parser and the
/// error it produces cannot drift apart.
pub const DURATION_ACCEPTED_FORM: &str =
    "must be a duration: an integer, optionally suffixed s/m/h/d";

/// Parses `90`, `30s`, `15m`, `1h`, `14d` into a whole number of seconds.
///
/// A bare integer is seconds. Suffixes are lowercase and single-character; `5 minutes` and `30S`
/// are rejected rather than guessed at, on the same terms as [`parse_byte_size`]. Returns the
/// accepted-form sentence as its error so every call site reports the same rule.
pub fn parse_duration_secs(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    let (digits, multiplier) = match trimmed.strip_suffix('s') {
        Some(digits) => (digits, 1u64),
        None => match trimmed.strip_suffix('m') {
            Some(digits) => (digits, 60),
            None => match trimmed.strip_suffix('h') {
                Some(digits) => (digits, 3600),
                None => match trimmed.strip_suffix('d') {
                    Some(digits) => (digits, 86_400),
                    None => (trimmed, 1),
                },
            },
        },
    };
    // Not trimmed again: the whole input was trimmed above, so a space surviving here is *inside*
    // the value (`5 minutes`, `30 m`), which is not a spelling this accepts.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("'{input}' {DURATION_ACCEPTED_FORM}"));
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| format!("'{input}' overflows a 64-bit second count"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExportMode {
    #[serde(rename = "read-only")]
    ReadOnly,
}

impl ExportMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ExportMode::ReadOnly => "read-only",
        }
    }
}

impl std::fmt::Display for ExportMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The accepted spelling of every byte count in the manifest, stated once so the parser and the
/// error it produces cannot drift apart.
pub const BYTE_SIZE_ACCEPTED_FORM: &str = "must be a byte count, optionally suffixed Ki/Mi/Gi";

/// Parses `4096`, `1Ki`, `10Mi`, `2Gi` into a byte count.
///
/// Binary suffixes only, and case-sensitively: `10MB` is rejected rather than guessed at, because
/// a manifest that means 10 000 000 and a manifest that means 10 485 760 must not both parse.
/// Returns the accepted-form sentence as its error so every call site reports the same rule.
pub fn parse_byte_size(input: &str) -> Result<u64, String> {
    let trimmed = input.trim();
    let (digits, multiplier) = match trimmed.strip_suffix("Ki") {
        Some(digits) => (digits, 1024u64),
        None => match trimmed.strip_suffix("Mi") {
            Some(digits) => (digits, 1024 * 1024),
            None => match trimmed.strip_suffix("Gi") {
                Some(digits) => (digits, 1024 * 1024 * 1024),
                None => (trimmed, 1),
            },
        },
    };
    // Deliberately not trimmed again: the whole input was trimmed above, so any space left here
    // is *inside* the value (`10 Mi`), which is not a spelling this accepts.
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("'{input}' {BYTE_SIZE_ACCEPTED_FORM}"));
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| format!("'{input}' overflows a 64-bit byte count"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeArtifact {
    pub name: String,
    pub version: String,
    pub runtime: ArtifactRuntime,
    /// Optional local source path. When set, the runtime resolves the artifact directly from
    /// this path instead of from the registry — no `.mur.zip`, no publish. For a skill the path
    /// points at a `skill.md` file or at a directory containing one (case-insensitive).
    /// Relative paths resolve against the directory containing `murmur.yaml`.
    /// Only permitted when `local_source` is true.
    pub source: Option<String>,
    /// Whether this artifact may be resolved from a local `source:` path. Declared via
    /// `local_source:` in the project manifest's artifact entry; when absent it defaults to
    /// true for `runtime: skill` and false for every other role, which is exactly the role-based
    /// gate this field replaced.
    pub local_source: bool,
    /// Whether this artifact's payload may be bound as the system prompt via
    /// `inference.system_prompt_artifact`. Declared via `prompt_payload:` in the project
    /// manifest's artifact entry; when absent it defaults to true for `runtime: skill` and
    /// false for every other role.
    pub prompt_payload: bool,
    /// Per-artifact capability grant, declared via `capabilities:` on this entry in the
    /// **capsule operator's own** manifest. Recognized on `runtime: hook`, `runtime: tool`,
    /// and `runtime: driver` entries; a `runtime: skill` entry carrying the key is rejected
    /// at parse time, because nothing would enforce it and a silently-ignored grant reads
    /// like a scoped artifact.
    ///
    /// What `None` (the key absent) means depends on the role, deliberately:
    /// - `hook`: full default-deny — no network and no preopened directory.
    /// - `tool`/`driver`: the unchanged capsule-wide ceiling. A declared block *narrows*
    ///   from that ceiling and can never widen past it.
    ///
    /// Deliberately never sourced from the artifact's own bundled `murmur.yaml` (see
    /// [`parse_hook_config_from_yaml`], which parses that file and knows nothing about
    /// capabilities) — an artifact pulled from a registry cannot self-grant.
    ///
    /// Reuses the whole [`Capabilities`] type for vocabulary consistency with the
    /// capsule-wide block, but only `network` and `filesystem` are consumed by the runtime
    /// per-artifact; the other sub-blocks are inert here.
    pub capabilities: Option<Capabilities>,
    /// What the runtime does when this hook's async job queue is full, declared via
    /// `on_overflow:` on this entry. Defaults to [`HookOverflowPolicy::Drop`] when absent.
    /// Accepted only on `runtime: hook` entries — every other role is rejected at parse
    /// time, since nothing would ever consult it there.
    pub on_overflow: HookOverflowPolicy,
    /// Operator-authored configuration for this artifact alone, declared via `config:` on this
    /// entry. Held as the raw YAML node: this crate accepts any mapping, and the runtime is what
    /// lowers it to the JSON delivered as `MURMUR_ARTIFACT_CONFIG` and refuses a shape that
    /// cannot travel that way.
    ///
    /// `None` is the key being absent, which means the variable is absent from the guest
    /// environment. `Some(Value::Null)` is `config:` written with nothing under it — a
    /// declaration that carries nothing, refused by the runtime rather than treated as absent.
    ///
    /// Recognized on `runtime: hook`, `runtime: tool` and `runtime: driver` entries; a
    /// `runtime: skill` entry carrying the key is rejected at parse time, on the same terms as
    /// [`Self::capabilities`]. Operator-sourced only, and never read from the artifact's own
    /// bundled `murmur.yaml` — an artifact pulled from a registry cannot configure itself.
    ///
    /// Plaintext in a file that is also an audit record: secrets belong in `${VAR}` references
    /// and the credential-stripping path, not here.
    pub config: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceDriver {
    pub artifact: String,
    pub config: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceConfig {
    pub transport: String,
    /// HTTP endpoint for the WASM driver. Present for `transport: http`, absent for `transport: process`.
    pub endpoint: Option<String>,
    pub model: String,
    pub api_key: Option<String>,
    /// WASM driver artifact. Present for `transport: http`, absent for `transport: process`.
    pub driver: Option<InferenceDriver>,
    /// CLI binary to spawn. Present for `transport: process`, absent for `transport: http`.
    pub command: Option<String>,
    pub compaction: Option<CompactionConfig>,
    pub system_prompt: Option<String>,
    pub system_prompt_file: Option<String>,
    /// Name of a skill artifact declared in `artifacts:` whose `skill.md` is used as the system
    /// prompt. Mutually exclusive with `system_prompt` and `system_prompt_file`. The skill is
    /// excluded from the callable tool inventory (it's already in the system prompt).
    pub system_prompt_artifact: Option<String>,
    /// Maximum LLM turns per capsule task. Defaults to 10 when absent in the manifest.
    /// Enforced as a hard ceiling by the runtime.
    pub max_turns: u32,
    /// Maximum output tokens the model may generate per turn (`max_tokens` in the driver wire
    /// payload). None means the manifest didn't set it; the runtime applies its own default.
    /// `transport: http` only — rejected at parse time under `transport: process`.
    ///
    /// Unrelated to [`ContextConfig::max_tokens`], which is the session-wide token budget that
    /// drives compaction. This one is a per-turn *output* cap.
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactionConfig {
    /// Fraction of context_window at which compaction fires (0.0–1.0]. Default: 0.98.
    pub threshold: Option<f32>,
    /// Model for compaction. None means use the primary inference model.
    pub model: Option<String>,
    /// System prompt for compaction. None means the compaction hook picks its own default.
    /// Mutually exclusive with `system_prompt_file`.
    pub system_prompt: Option<String>,
    /// Path to a local file whose contents are used as the compaction system prompt,
    /// resolved relative to the manifest directory when the session launches. Mutually
    /// exclusive with `system_prompt`.
    pub system_prompt_file: Option<String>,
    /// When true, every committed compaction appends one JSON line recording the
    /// replacement summary to `out/compaction-summaries.jsonl` in the session workdir.
    /// None (the key absent) is equivalent to false: nothing is written.
    pub dump_summaries: Option<bool>,
}

// Threshold values are validated to (0.0, 1.0] at parse time so NaN is impossible.
impl Eq for CompactionConfig {}

/// `context.seed_budget` when the manifest leaves it out: a tenth of the window.
/// Small enough that a seed cannot crowd out the task it precedes, large enough that a
/// memory hook has room to say something.
pub const DEFAULT_SEED_BUDGET: f32 = 0.10;

/// `context.seed_overflow_margin` when the manifest leaves it out. Slack above
/// `seed_budget` within which an over-budget seed is trimmed rather than summarized —
/// a few tokens over must not buy an inference call.
pub const DEFAULT_SEED_OVERFLOW_MARGIN: f32 = 0.10;

#[derive(Debug, Clone, PartialEq)]
pub struct ContextConfig {
    /// Token budget for this session; None disables compaction.
    pub max_tokens: Option<u32>,
    /// Whether the runtime keeps a durable conversation record for this capsule. `true` unless
    /// the manifest says `record: off`; `false` turns the mechanism off entirely, creating
    /// nothing under `~/.murmur/conversations/`.
    pub record: bool,
    /// Directory segment under `~/.murmur/conversations/` this capsule's records live in.
    /// `None` means the capsule name. Validated as one path segment by the runtime, on the same
    /// terms as `capabilities.state.store`. Inert when [`Self::record`] is `false`.
    pub record_store: Option<String>,
    /// Fraction of [`Self::max_tokens`] an `on-task-start` hook's `seed-context` may
    /// occupy, in `0.0..=1.0`. Always populated; [`DEFAULT_SEED_BUDGET`] when the key is
    /// absent. Inert without `max_tokens`: a fraction of no ceiling is no ceiling, and a
    /// seed with no ceiling is rejected rather than committed unbounded.
    pub seed_budget: f32,
    /// Slack above the seed budget, as a fraction of it, within which an over-budget seed
    /// is trimmed from its front rather than handed to the compaction hook, in
    /// `0.0..=1.0`. Always populated; [`DEFAULT_SEED_OVERFLOW_MARGIN`] when the key is
    /// absent.
    pub seed_overflow_margin: f32,
    /// What bounds this capsule's conversation records. `None` — the `retain:` block was absent
    /// — keeps every record whole and forever. Inert when [`Self::record`] is `false`.
    pub retain: Option<ContextRetainConfig>,
}

// Both fractions are validated to 0.0..=1.0 at parse time, so NaN is impossible.
impl Eq for ContextConfig {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub otel_endpoint: Option<String>,
    pub eval: Option<EvalConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceConfig {
    /// How much of each turn's driver request this session's trace keeps — resolved from
    /// `trace.capture`, or from the retired `trace.include_tool_output` alias, by
    /// [`crate::trace_capture::resolve_trace_capture`].
    pub capture: TraceCapture,
    /// What bounds the session directories under the workdir. `None` — the `retain:` block was
    /// absent — keeps every session forever; there is no default policy, because a mechanism
    /// that deletes an operator's traces unless they opt out is the one default-allow in a
    /// runtime that is default-deny everywhere else.
    pub retain: Option<TraceRetainConfig>,
}

/// The `trace.retain` block: what bounds the set of session directories beside the running one.
///
/// Both keys are optional and ANDed — a session survives only if it is inside every limit
/// declared. An empty block is a parse error, not a no-op: omitting `retain:` is how a capsule
/// says "keep everything".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceRetainConfig {
    /// Newest session directories to keep, counting the running session itself. `None` means
    /// the count is unbounded. Never `Some(0)`: truncating the set to nothing would delete the
    /// running session's own trace, so it is refused at parse time.
    pub max_sessions: Option<u32>,
    /// Age beyond which a session directory is removed, in seconds, measured from the
    /// millisecond timestamp inside its own uuid-v7 `ses_` id. `None` means no age limit.
    pub max_age_secs: Option<u64>,
}

/// The `context.retain` block: what bounds this capsule's conversation records.
///
/// Both keys are optional and ANDed. `max_messages` truncates the front of the record the
/// launch opens; `max_age_secs` removes a context directory this capsule owns and has not
/// written to inside the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRetainConfig {
    /// Messages to keep at the tail of a record. `None` means the record grows without bound.
    /// Never `Some(0)`: truncating a record to nothing is `mur conversation rm`, not retention.
    pub max_messages: Option<u32>,
    /// Age beyond which a record this capsule owns is removed, in seconds, measured from the
    /// last write to its `conversation.jsonl`. `None` means no age limit.
    pub max_age_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EvalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    pub scorers: Vec<ScorerConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScorerConfig {
    ExitOk { name: String },
    MaxTurns { name: String, max: u32 },
    MaxTokens { name: String, max: u64 },
    ToolSequence { name: String, expected: Vec<String> },
    LlmJudge { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkCapabilities {
    pub allow: Vec<String>,
    /// Whether the capsule's shell subprocess tree may create `AF_UNIX` sockets. `false` unless
    /// the manifest says otherwise: a local daemon socket — `/var/run/docker.sock` above all —
    /// is an unmediated path to host root, and the `allow` list above only governs IP
    /// destinations, so nothing else in this block constrains it. Declaring `true` is the
    /// deliberate, auditable widening for a capsule that genuinely needs a local daemon socket;
    /// it is capsule-wide and coarse (a domain, not a per-path allowlist). `AF_NETLINK` and
    /// `AF_PACKET` have no corresponding key and are always denied.
    pub unix_sockets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemCapabilities {
    pub scope: Option<String>,
    /// Whether anything written *into* the session workdir may be executed. `false` unless the
    /// manifest says otherwise, and that default is the whole point: with it, the workdir's own
    /// Landlock `PathBeneath` rule withholds the `Execute` right, so the kernel — evaluating the
    /// resolved path itself, with no userspace round trip — refuses to exec a binary the capsule
    /// produced, whatever it is named. That is what makes `capabilities.shell.allow` a complete
    /// and sound statement rather than a name-matching convention.
    ///
    /// `true` takes the `Execute` right back for compile-and-run workflows (a capsule that
    /// `gcc`/`cargo build`s inside its workdir and then runs the result). The cost is not
    /// negotiable and is not hidden: anything the capsule writes into the workdir can then run
    /// regardless of `shell.allow`, so the allowlist stops being an enforceable property of that
    /// capsule. A capsule declaring it can therefore never achieve
    /// [`ContainmentClass::Scoped`] — see `capsule_runtime::containment` — and pairing it with
    /// `capabilities.containment: scoped` is refused at launch.
    pub workdir_exec: bool,
    /// Workdir-relative subtrees the capsule may read but must not write, in the same vocabulary
    /// [`Self::scope`] uses. Empty when the key is absent, which is the whole workdir writable —
    /// the behaviour of every capsule that declares nothing here.
    ///
    /// Entries are trimmed and empty ones dropped at parse; path *shape* is judged by the
    /// runtime (`capsule_runtime::protected_paths`), exactly as `scope`'s shape is.
    pub read_only: Vec<String>,
}

/// The `capabilities.task_io` block on a `runtime: hook` artifact entry — the operator's
/// grant of the `murmur:task-io/read` host import to that one hook. Recognized nowhere else:
/// on any other role, and in the capsule-wide block, it is rejected at parse time rather than
/// accepted and silently inert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskIoCapabilities {
    /// Whether this hook may read the in-scope task's input and result text. Never inferred:
    /// a `task_io:` block that omits `read:` is rejected, on the same terms as
    /// `capabilities.shell.interpreter_runtime[].dirs[].list_dir`.
    pub read: bool,
}

/// The `capabilities.conversation` block on a `runtime: hook` artifact entry — the operator's
/// grant of the `murmur:conversation/read` host import to that one hook. Rejected at parse time
/// on any other artifact role; accepted but inert in the capsule-wide block, which warns
/// `W-SEC-016` at staging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationCapabilities {
    /// Whether this hook may read the capsule's durable conversation record. Never inferred: a
    /// `conversation:` block that omits `read:` is rejected, exactly as `task_io:` is.
    pub read: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCapabilities {
    pub allow: Vec<String>,
    pub strip_env: Option<Vec<String>>,
    pub baseline_env: Option<Vec<String>>,
    /// Typed grants that widen an already-allowlisted `allow` binary's Landlock filesystem
    /// scope to the exact host directories its import machinery needs *outside* the workdir
    /// (a path-based interpreter like CPython, whose stdlib the `DT_NEEDED` closure cannot
    /// reach). Empty unless the manifest declares `capabilities.shell.interpreter_runtime`.
    /// This can only ever name specific directories with an explicit per-directory
    /// enumerability flag — it has no field that expands a whole install prefix.
    pub interpreter_runtime: Vec<InterpreterRuntimeGrant>,
    /// Typed staged-runtime grants that name a pinned host runtime tree to bind-mount read-only
    /// into a `sealed` capsule's composed root. Empty unless the manifest declares
    /// `capabilities.shell.staged_runtime`. Mutually exclusive with [`Self::interpreter_runtime`]
    /// per binary: staging the tree *into* the root is what makes widening Landlock *out to* the
    /// host unnecessary, so declaring both for one binary is a contradiction, not a layering.
    pub staged_runtime: Vec<StagedRuntimeGrant>,
}

/// One `capabilities.shell.interpreter_runtime` entry: an already-allowlisted binary plus the
/// exact host directories outside the workdir its import machinery must reach. Declaring one
/// couples the capsule to a specific host interpreter-version layout, so it fires `W-SEC-009`
/// at staging; it exists only to bridge until the staged runtime bind-mount ships.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterRuntimeGrant {
    /// A binary that MUST already appear in this same block's `allow`. This mechanism can only
    /// narrow filesystem access alongside an exec grant that already exists — it never itself
    /// grants exec.
    pub binary: String,
    /// The host directories to grant. Never empty (a grant with no directories is rejected at
    /// parse time).
    pub dirs: Vec<InterpreterRuntimeDir>,
}

/// One directory inside an [`InterpreterRuntimeGrant`], with its author-declared enumerability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterRuntimeDir {
    /// An absolute host path (must start with `/`) outside the workdir.
    pub path: String,
    /// `true` grants `Execute + ReadFile + ReadDir` — the directory's own entries can be
    /// enumerated (what CPython's `FileFinder` needs for a `sys.path` entry). `false` grants
    /// `Execute + ReadFile` only — files inside can still be opened by exact name, but the
    /// directory itself cannot be listed. Never inferred: the author must write it explicitly.
    pub list_dir: bool,
}

/// One `capabilities.shell.staged_runtime` entry: an already-allowlisted binary, the absolute host
/// path of a pinned runtime tree that already exists on the launch host, and the `pin` string
/// identifying which build that tree is.
///
/// The mechanism this describes is the inverse of [`InterpreterRuntimeGrant`]. That one widens a
/// capsule's Landlock scope *outwards* so an interpreter can reach its stdlib where the host
/// happens to keep it — which couples the capsule to one host's directory layout and is why it
/// fires `W-SEC-009`. This one moves the tree *inwards*: the composed root of a `sealed` capsule
/// carries the runtime at the same absolute path, bind-mounted read-only, so nothing outside the
/// root has to stay reachable. Declaring it therefore requires an effective `sealed` floor — there
/// is no composed root to stage into below that — and it is refused alongside an
/// `interpreter_runtime` grant for the same binary.
///
/// `source_path` is deliberately a path to an *already-pinned tree* (a vendored toolchain
/// directory, a baked-in conda env), never a bare install prefix guessed at random: the runtime
/// does not resolve, discover or version-sniff it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedRuntimeGrant {
    /// A binary that MUST already appear in this same block's `allow`. Like
    /// [`InterpreterRuntimeGrant::binary`], this mechanism never itself grants exec — it only says
    /// where the runtime behind an existing exec grant comes from.
    pub binary: String,
    /// Absolute host path of the runtime tree to stage. Bind-mounted read-only at this same
    /// absolute path inside the composed root, so `sys.prefix`, shebangs and anything a previous
    /// turn recorded keep resolving.
    pub source_path: String,
    /// Non-empty, explicit identifier of *which build* the tree at `source_path` is. Never
    /// inferred from the tree's contents: it exists so a human can compare the declared pin across
    /// two hosts and confirm the same interpreter build shipped to both. The runtime treats it as
    /// an opaque string and does not parse it.
    pub pin: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnCapabilities {
    pub allow: Vec<String>,
}

/// Host environment variables a WASM guest may observe, from `capabilities.env`.
///
/// An empty `allow` is legitimate (a declared-but-empty block is a no-op), so unlike
/// `capabilities.shell.allow` this is not rejected at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvCapabilities {
    pub allow: Vec<String>,
}

/// Per-guest execution limits from `capabilities.limits`.
///
/// Every field is `Option` because this type carries only what the manifest actually
/// declared — an omitted field stays `None` here and the runtime substitutes its own
/// default (see `capsule_runtime::limits::ExecutionLimits`). Keeping the "absent" state
/// distinct from "explicitly set" is what lets the runtime own the defaults in one place
/// instead of duplicating them into the manifest parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceLimits {
    /// Cap on a guest's linear-memory growth, in bytes.
    pub memory_bytes: Option<usize>,
    /// Cap on a guest's table growth, in elements.
    pub table_elements: Option<usize>,
    /// Cap on the number of instances a single store may create.
    pub instances: Option<usize>,
    /// Wall-clock budget for a single guest invocation, in seconds.
    pub deadline_seconds: Option<u64>,
}

/// Host-process (OS-level) resource bounds from `capabilities.resources`.
///
/// Distinct from [`ResourceLimits`] (`capabilities.limits`) in both mechanism and subject:
/// that block bounds a WASM *guest* inside its wasmtime store, this one bounds every *native
/// subprocess* the runtime spawns — `rlimit(2)` ceilings applied before `execve`, a Linux
/// cgroup v2 scope around the whole process tree, and a periodic workdir-size check. A capsule
/// that cannot escape containment can still wedge its host by forking, allocating, or writing
/// without bound; this is the block that stops that.
///
/// Every field is `Option` for the same reason [`ResourceLimits`]'s are: this type carries only
/// what the manifest actually declared, and the runtime substitutes its own default for
/// anything omitted (see `capsule_runtime::resources::HostResourceLimits`). An omitted field —
/// or an omitted block — means defaults, never "unlimited".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceCapabilities {
    /// `RLIMIT_NPROC` hard ceiling on each spawned subprocess.
    pub max_processes: Option<u64>,
    /// `RLIMIT_NOFILE` hard ceiling on each spawned subprocess.
    pub max_open_files: Option<u64>,
    /// `RLIMIT_FSIZE` hard ceiling, in bytes, on each spawned subprocess.
    pub max_file_size_bytes: Option<u64>,
    /// `RLIMIT_CPU` hard ceiling, in CPU-seconds, on each spawned subprocess.
    pub cpu_seconds: Option<u64>,
    /// `RLIMIT_AS` (Linux) / `RLIMIT_DATA` (macOS) hard ceiling, in bytes, on each spawned
    /// subprocess.
    pub memory_bytes: Option<u64>,
    /// cgroup v2 `memory.max`, in bytes — aggregate across the whole subprocess tree. Linux only.
    pub cgroup_memory_bytes: Option<u64>,
    /// cgroup v2 `pids.max` — aggregate across the whole subprocess tree. Linux only.
    pub cgroup_pids_max: Option<u64>,
    /// cgroup v2 `cpu.max` quota as a percentage of one core (200 = two cores). Linux only.
    pub cgroup_cpu_percent: Option<u32>,
    /// cgroup v2 `io.max` read+write bytes/sec on the workdir's backing device. Linux only,
    /// best-effort (the backing device cannot always be resolved).
    pub cgroup_io_bytes_per_sec: Option<u64>,
    /// Ceiling on total workdir size, in bytes, enforced by a periodic check on every platform.
    pub workdir_max_bytes: Option<u64>,
}

/// Durable, capsule-scoped state for one tool, driver or hook: a host directory outside every
/// session workdir, preopened into the guest as `state/`.
///
/// Absent is deny — no declaration means no second preopen and no directory anywhere. The store
/// is keyed by capsule rather than by workdir precisely so it survives a launch that gets a fresh
/// session workdir, and so two capsules launched in the same directory never share one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StateCapabilities {
    /// Directory name under `~/.murmur/state/`. `None` means "use the capsule name", which is
    /// what the overwhelming majority of declarations want.
    ///
    /// Read from the capsule operator's own manifest entry and never from the artifact's bundled
    /// manifest, so a registry-pulled tool cannot claim a store that already exists. The *shape*
    /// of the name is not checked here — a store name is a runtime concern, refused as
    /// `E-CAP-009`, exactly as `capabilities.filesystem.scope`'s shape is refused as `E-CAP-002`.
    pub store: Option<String>,
}

// ── Containment class ─────────────────────────────────────────────────────────

/// How strongly the host must contain a capsule's *subprocess* tree, declared as a floor
/// by the operator and satisfied (or not) by the host's kernel.
///
/// Declaration order is enforcement order — the `Ord` derive is load-bearing, so variants
/// must stay listed weakest-first. Combining floors from several sources is `max`, never
/// `min` (see [`effective_containment_floor`]): no source can lower what another raised.
///
/// The class an operator *declares* is a requirement. The class a host actually *achieves*
/// is derived only from a live kernel probe (`capsule_runtime::containment`), never from
/// this declaration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Deserialize, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ContainmentClass {
    /// No kernel mechanism required. Capability declarations are honored by the runtime's
    /// own dispatch layer and by convention; anything the host cannot mediate is warned
    /// about, not refused. Every host satisfies this, including macOS. The default.
    #[default]
    Advisory,
    /// Landlock filesystem mediation + seccomp exec/network allowlisting over the *host*
    /// filesystem: every path outside the workdir must be an explicit manifest grant.
    /// Requires Linux 5.13+ with a usable Landlock ABI.
    Scoped,
    /// A private filesystem root: mount-namespace + `pivot_root` isolation onto a composed
    /// root, so the host filesystem is not merely mediated but absent. Achievable on an
    /// uncontainerised Linux host with a usable Landlock ABI, unprivileged user namespaces,
    /// and — where AppArmor's `restrict_unprivileged_userns` is active — the shipped
    /// `mur-sealed` profile loaded. A host that cannot back it refuses the launch with a
    /// mechanism-specific reason rather than silently degrading to `Scoped` or `Advisory`.
    /// See `docs/content/reference/sealed-containment-manual-verification.md`.
    Sealed,
}

impl ContainmentClass {
    /// The lowercase wire name, identical in the manifest, the workspace config, the
    /// `--containment` flag, `trace.jsonl`, and `--explain-scope` output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Scoped => "scoped",
            Self::Sealed => "sealed",
        }
    }

    /// Every value, weakest first — the single source for the "must be one of: …" text.
    pub const ALL: [ContainmentClass; 3] = [Self::Advisory, Self::Scoped, Self::Sealed];
}

impl std::fmt::Display for ContainmentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not one of the three containment class names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseContainmentClassError {
    pub value: String,
}

impl std::fmt::Display for ParseContainmentClassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "must be one of: advisory, scoped, sealed; got '{}'",
            self.value
        )
    }
}

impl std::error::Error for ParseContainmentClassError {}

impl std::str::FromStr for ContainmentClass {
    type Err = ParseContainmentClassError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim() {
            "advisory" => Ok(Self::Advisory),
            "scoped" => Ok(Self::Scoped),
            "sealed" => Ok(Self::Sealed),
            other => Err(ParseContainmentClassError {
                value: other.to_string(),
            }),
        }
    }
}

/// The one floor a single `mur run` invocation must clear: the *strongest* class any source
/// asked for, defaulting to [`ContainmentClass::Advisory`] when no source declares one.
///
/// Monotonic by construction — an absent source does not participate, and no present source
/// can lower what another raised. Argument order carries no precedence.
pub fn effective_containment_floor(
    workspace_default: Option<ContainmentClass>,
    manifest_declared: Option<ContainmentClass>,
    cli_flag: Option<ContainmentClass>,
) -> ContainmentClass {
    [workspace_default, manifest_declared, cli_flag]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub network: Option<NetworkCapabilities>,
    /// Which peers this capsule may redeem a peer-file handle against. `None` means no
    /// `fetch-peer-file` tool exists and nothing can be fetched — absent is deny.
    pub peer_fetch: Option<PeerFetchCapabilities>,
    pub filesystem: Option<FilesystemCapabilities>,
    pub shell: Option<ShellCapabilities>,
    pub spawn: Option<SpawnCapabilities>,
    pub env: Option<EnvCapabilities>,
    pub limits: Option<ResourceLimits>,
    pub resources: Option<ResourceCapabilities>,
    /// Durable state store for this artifact. `None` — the overwhelmingly common case — is deny:
    /// no `state/` preopen, and no directory created anywhere. See [`StateCapabilities`].
    pub state: Option<StateCapabilities>,
    /// Per-hook grant of the `murmur:task-io/read` host import. Only ever `Some` on a
    /// `runtime: hook` artifact entry — see [`TaskIoCapabilities`].
    pub task_io: Option<TaskIoCapabilities>,
    /// Per-hook grant of the `murmur:conversation/read` host import — see
    /// [`ConversationCapabilities`].
    pub conversation: Option<ConversationCapabilities>,
    /// Minimum containment class this capsule declares. `None` (the overwhelmingly common
    /// case) means the capsule states no requirement and inherits whatever the workspace
    /// config or `--containment` asks for, defaulting to `advisory`.
    pub containment: Option<ContainmentClass>,
}

#[derive(Debug, Error)]
pub enum RuntimeManifestError {
    #[error("{} not found at {}", MANIFEST_FILENAME, .0)]
    NotFound(String),
    #[error("{0}")]
    YamlSyntax(String),
    #[error("{}: missing required field '{field}'", MANIFEST_FILENAME)]
    MissingField { field: String },
    #[error(
        "{}: invalid artifact declaration at index {index}: {message}",
        MANIFEST_FILENAME
    )]
    InvalidArtifact { index: usize, message: String },
    #[error(
        "{}: invalid inference config for '{field}': {message}",
        MANIFEST_FILENAME
    )]
    InvalidInferenceConfig { field: String, message: String },
    #[error(
        "{}: invalid capability config for '{field}': {message}",
        MANIFEST_FILENAME
    )]
    InvalidCapabilities { field: String, message: String },
    #[error(
        "{}: invalid exports config for '{field}': {message}",
        MANIFEST_FILENAME
    )]
    InvalidExports { field: String, message: String },
    #[error(
        "{}: inference.api_key references {reference} but the environment variable is not set",
        MANIFEST_FILENAME
    )]
    MissingInferenceEnvVar {
        field: String,
        reference: String,
        variable: String,
    },
    #[error("{}: invalid trace config for '{field}': {message}", MANIFEST_FILENAME)]
    InvalidTraceConfig { field: String, message: String },
    #[error("failed to read {} at {path}: {source}", MANIFEST_FILENAME)]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Deserialize)]
struct RawRuntimeManifest {
    name: Option<String>,
    version: Option<String>,
    #[serde(default)]
    artifacts: Vec<RawArtifact>,
    #[serde(default)]
    capabilities: Option<RawCapabilities>,
    #[serde(default)]
    inference: Option<RawInferenceConfig>,
    #[serde(default)]
    context: Option<RawContextConfig>,
    #[serde(default)]
    observability: Option<RawObservabilityConfig>,
    #[serde(default)]
    trace: Option<RawTraceConfig>,
    #[serde(default)]
    network: Option<RawNetworkConfig>,
    #[serde(default)]
    lifecycle: Option<RawLifecycleConfig>,
    #[serde(default)]
    exports: Option<RawExports>,
    #[serde(default)]
    mur_version: Option<String>,
    /// Captured only to refuse it. `config:` is delivered to one artifact through that artifact's
    /// own grant, so a capsule-wide block reaches nothing; without this field it would be one of
    /// the unrecognized top-level keys this manifest silently ignores.
    #[serde(default, deserialize_with = "deserialize_present")]
    config: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawExports {
    #[serde(default)]
    files: Option<RawFileExport>,
    #[serde(default)]
    peer_files: Option<RawPeerFilesExport>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawPeerFilesExport {
    #[serde(default)]
    root: Option<String>,
    /// Untyped for the same reason as [`RawFileExport::max_bytes`]: `30m` is a YAML string and a
    /// bare `1800` is a YAML integer, and a wrong-shaped value has to reach the parser to be
    /// reported as an [`RuntimeManifestError::InvalidExports`] naming the field.
    #[serde(default)]
    max_ttl: Option<serde_yaml::Value>,
    #[serde(default)]
    max_bytes: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawFileExport {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    /// Deliberately untyped: `max_bytes` accepts both a bare integer and a suffixed string, and
    /// a wrong-shaped value has to reach [`parse_exports`] to be reported as an
    /// [`RuntimeManifestError::InvalidExports`] naming the field rather than as a serde type
    /// error naming a line number.
    #[serde(default)]
    max_bytes: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawLifecycleConfig {
    #[serde(default)]
    task_acceptance: Option<TaskAcceptance>,
    #[serde(default)]
    after_task: Option<AfterTask>,
    #[serde(default)]
    queue_depth: Option<usize>,
    #[serde(default)]
    input_timeout_secs: Option<u64>,
    #[serde(default)]
    conversation: Option<ConversationMode>,
    #[serde(default)]
    max_task_reopens: Option<u32>,
    #[serde(default)]
    shell_grace_secs: Option<u64>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawNetworkConfig {
    internal_port: Option<u16>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawContextConfig {
    max_tokens: Option<u32>,
    /// `on` | `off`. Kept as a raw `String` so an unrecognized value reports as an invalid
    /// value for this key rather than as a serde variant error against the whole block.
    #[serde(default)]
    record: Option<String>,
    #[serde(default)]
    record_store: Option<String>,
    #[serde(default)]
    seed_budget: Option<f32>,
    #[serde(default)]
    seed_overflow_margin: Option<f32>,
    /// Untyped so an empty block, a null block and an unknown key inside it are each reported
    /// as an invalid `context.retain` rather than as a serde message about a struct field.
    #[serde(default, deserialize_with = "present_yaml_value")]
    retain: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawObservabilityConfig {
    #[serde(default)]
    otel_endpoint: Option<String>,
    #[serde(default)]
    eval: Option<RawEvalConfig>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawTraceConfig {
    /// Untyped so an unparseable mode reaches [`resolve_trace_capture`] and is reported as a
    /// [`RuntimeManifestError::InvalidTraceConfig`] naming the field and the accepted values,
    /// rather than as a serde message about an unknown variant.
    #[serde(default)]
    capture: Option<String>,
    /// The retired alias. `None` means the key was absent, which is what separates a manifest
    /// that opted out from one that never mentioned it — see [`resolve_trace_capture`].
    #[serde(default)]
    include_tool_output: Option<bool>,
    /// Untyped for the same reason as `context.retain` — see [`RawContextConfig::retain`].
    #[serde(default, deserialize_with = "present_yaml_value")]
    retain: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawEvalConfig {
    #[serde(default)]
    dataset_id: Option<String>,
    #[serde(default)]
    scorers: Vec<RawScorerConfig>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawScorerConfig {
    #[serde(rename = "type")]
    scorer_type: String,
    name: Option<String>,
    max: Option<u64>,
    expected: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawArtifact {
    name: Option<String>,
    version: Option<String>,
    runtime: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    local_source: Option<bool>,
    #[serde(default)]
    prompt_payload: Option<bool>,
    #[serde(default)]
    capabilities: Option<RawCapabilities>,
    #[serde(default)]
    on_overflow: Option<String>,
    /// Untyped, and deserialized through [`deserialize_present`] rather than plain `Option`, so
    /// `config:` written with nothing under it arrives as `Some(Value::Null)` instead of
    /// collapsing into the absent case. The two mean different things: absent grants no variable,
    /// while an empty declaration is a written statement that carries nothing and is refused.
    #[serde(default, deserialize_with = "deserialize_present")]
    config: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

/// Deserialize a present key into `Some(value)` even when its value is YAML null, leaving `None`
/// to mean the key was absent and `#[serde(default)]` supplied it.
///
/// serde's own `Option` impl maps a null value to `None`, which erases exactly the distinction
/// `config:` rests on.
fn deserialize_present<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
struct RawCapabilities {
    #[serde(default)]
    network: Option<RawNetworkCapabilities>,
    #[serde(default)]
    peer_fetch: Option<RawPeerFetchCapabilities>,
    #[serde(default)]
    filesystem: Option<RawFilesystemCapabilities>,
    #[serde(default)]
    shell: Option<RawShellCapabilities>,
    #[serde(default)]
    spawn: Option<RawSpawnCapabilities>,
    #[serde(default)]
    env: Option<RawEnvCapabilities>,
    #[serde(default)]
    limits: Option<RawResourceLimits>,
    #[serde(default)]
    resources: Option<RawResourceCapabilities>,
    #[serde(default)]
    state: Option<RawStateCapabilities>,
    #[serde(default)]
    task_io: Option<RawTaskIoCapabilities>,
    #[serde(default)]
    conversation: Option<RawConversationCapabilities>,
    /// Kept as a raw `String` rather than a `ContainmentClass` so a typo reports through
    /// `InvalidCapabilities` like every other bad capability value, instead of a bare serde
    /// "unknown variant" error attributed to the whole `capabilities:` block.
    #[serde(default)]
    containment: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawTaskIoCapabilities {
    // `Option` so an omitted `read` is distinguishable from an explicit `false`: the omission
    // is rejected outright rather than defaulted, because a capability is never inferred.
    #[serde(default)]
    read: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawConversationCapabilities {
    // `Option` for the reason `RawTaskIoCapabilities::read` is: an omitted key is refused rather
    // than defaulted.
    #[serde(default)]
    read: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawSpawnCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawEnvCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawResourceLimits {
    #[serde(default)]
    memory_bytes: Option<usize>,
    #[serde(default)]
    table_elements: Option<usize>,
    #[serde(default)]
    instances: Option<usize>,
    #[serde(default)]
    deadline_seconds: Option<u64>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawStateCapabilities {
    #[serde(default)]
    store: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawResourceCapabilities {
    #[serde(default)]
    max_processes: Option<u64>,
    #[serde(default)]
    max_open_files: Option<u64>,
    #[serde(default)]
    max_file_size_bytes: Option<u64>,
    #[serde(default)]
    cpu_seconds: Option<u64>,
    #[serde(default)]
    memory_bytes: Option<u64>,
    #[serde(default)]
    cgroup_memory_bytes: Option<u64>,
    #[serde(default)]
    cgroup_pids_max: Option<u64>,
    #[serde(default)]
    cgroup_cpu_percent: Option<u32>,
    #[serde(default)]
    cgroup_io_bytes_per_sec: Option<u64>,
    #[serde(default)]
    workdir_max_bytes: Option<u64>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawNetworkCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    unix_sockets: bool,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawPeerFetchCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawFilesystemCapabilities {
    #[serde(default)]
    scope: Option<String>,
    /// Same shape and defaulting convention as `RawNetworkCapabilities::unix_sockets`: a plain
    /// `bool` that is always present after parsing and defaults to `false`, never an `Option`.
    /// "The key is absent" and "the key says `false`" are the same declaration.
    #[serde(default)]
    workdir_exec: bool,
    /// Always a list after parsing, never an `Option`: an absent key and an empty list are the
    /// same declaration — nothing is protected.
    #[serde(default)]
    read_only: Vec<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawShellCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    strip_env: Option<Vec<String>>,
    #[serde(default)]
    baseline_env: Option<Vec<String>>,
    #[serde(default)]
    interpreter_runtime: Vec<RawInterpreterRuntimeGrant>,
    #[serde(default)]
    staged_runtime: Vec<RawStagedRuntimeGrant>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawInterpreterRuntimeGrant {
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    dirs: Vec<RawInterpreterRuntimeDir>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawInterpreterRuntimeDir {
    #[serde(default)]
    path: Option<String>,
    // `Option` so an omitted `list_dir` is distinguishable from an explicit `false`: the
    // omission is rejected outright rather than defaulted, because enumerability is never
    // inferred.
    #[serde(default)]
    list_dir: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawStagedRuntimeGrant {
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    source_path: Option<String>,
    // `Option` so an omitted `pin` reaches `parse_staged_runtime` and is rejected there with a
    // field-naming message, rather than defaulting to an empty string that would silently mean
    // "unpinned" — the one thing this field exists to make impossible.
    #[serde(default)]
    pin: Option<String>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawInferenceConfig {
    transport: Option<String>,
    endpoint: Option<String>,
    model: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
    #[serde(default)]
    system_prompt_file: Option<String>,
    #[serde(default)]
    system_prompt_artifact: Option<String>,
    #[serde(default)]
    driver: Option<RawInferenceDriver>,
    #[serde(default)]
    provider: Option<RawInferenceDriver>,
    #[serde(default)]
    compaction: Option<RawCompactionConfig>,
    #[serde(default)]
    max_turns: Option<u32>,
    /// The `inference` spelling of the reopen budget is not a live field. It is deserialized
    /// only so `parse_inference` can refuse it; without it serde would ignore the key.
    #[serde(default)]
    max_task_reopens: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawCompactionConfig {
    threshold: Option<f32>,
    model: Option<String>,
    system_prompt: Option<String>,
    system_prompt_file: Option<String>,
    dump_summaries: Option<bool>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

#[derive(Debug, Deserialize)]
struct RawInferenceDriver {
    artifact: Option<String>,
    #[serde(default)]
    config: Option<serde_yaml::Value>,
    #[serde(flatten)]
    unknown: UnknownKeys,
}

/// The keys one `Raw*` block carried that no field of it claims.
///
/// A `BTreeMap` so the capture order is the manifest's key order sorted, which keeps the
/// `W-SEC-019` lines for one manifest identical on every run. The values are discarded: a key this
/// build does not recognize is reported by name and by containing block, and printing whatever an
/// operator wrote under it would put manifest contents into a diagnostic that has no use for them.
type UnknownKeys = std::collections::BTreeMap<String, serde::de::IgnoredAny>;

/// A `Raw*` deserialization block that captures the keys this build does not recognize.
///
/// Implemented for every `Raw*` struct in this file, which is asserted structurally by
/// `every_raw_struct_captures_unknown_keys_and_declares_its_own_field_names` rather than left to
/// review: a manifest key added without its block learning about it is exactly the defect
/// `W-SEC-019` exists to report.
trait RawBlock {
    /// Every key serde accepts on this block, in declaration order and under the name serde
    /// matches (so a `#[serde(rename)]` field appears under its renamed spelling). This is the
    /// candidate set [`crate::unknown_manifest_keys::nearest_known_key`] suggests from.
    const KNOWN_KEYS: &'static [&'static str];

    fn unknown_keys(&self) -> &UnknownKeys;

    /// Descends into the blocks this one owns, in declaration order, so the reported paths read
    /// down the manifest the way the document does.
    ///
    /// Defaulted to a no-op, which is correct for a block that owns no other block. A block that
    /// does own one must override this: `every_raw_struct_descends_into_the_blocks_it_owns` reads
    /// this file's own text and fails, naming the struct and the field, if it does not — an
    /// unwalked block reports none of its keys, silently.
    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        let _ = (path, out);
    }
}

impl RawBlock for RawRuntimeManifest {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "name",
        "version",
        "artifacts",
        "capabilities",
        "inference",
        "context",
        "observability",
        "trace",
        "network",
        "lifecycle",
        "exports",
        "mur_version",
        "config",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        let artifacts = child_path(path, "artifacts");
        for (index, artifact) in self.artifacts.iter().enumerate() {
            collect_block(artifact, &format!("{artifacts}[{index}]"), out);
        }
        if let Some(capabilities) = &self.capabilities {
            collect_block(capabilities, &child_path(path, "capabilities"), out);
        }
        if let Some(inference) = &self.inference {
            collect_block(inference, &child_path(path, "inference"), out);
        }
        if let Some(context) = &self.context {
            collect_block(context, &child_path(path, "context"), out);
        }
        if let Some(observability) = &self.observability {
            collect_block(observability, &child_path(path, "observability"), out);
        }
        if let Some(trace) = &self.trace {
            collect_block(trace, &child_path(path, "trace"), out);
        }
        if let Some(network) = &self.network {
            collect_block(network, &child_path(path, "network"), out);
        }
        if let Some(lifecycle) = &self.lifecycle {
            collect_block(lifecycle, &child_path(path, "lifecycle"), out);
        }
        if let Some(exports) = &self.exports {
            collect_block(exports, &child_path(path, "exports"), out);
        }
    }
}

impl RawBlock for RawExports {
    const KNOWN_KEYS: &'static [&'static str] = &["files", "peer_files"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        if let Some(files) = &self.files {
            collect_block(files, &child_path(path, "files"), out);
        }
        if let Some(peer_files) = &self.peer_files {
            collect_block(peer_files, &child_path(path, "peer_files"), out);
        }
    }
}

impl RawBlock for RawPeerFilesExport {
    const KNOWN_KEYS: &'static [&'static str] = &["root", "max_ttl", "max_bytes"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawFileExport {
    const KNOWN_KEYS: &'static [&'static str] = &["root", "mode", "max_bytes"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawLifecycleConfig {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "task_acceptance",
        "after_task",
        "queue_depth",
        "input_timeout_secs",
        "conversation",
        "max_task_reopens",
        "shell_grace_secs",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawNetworkConfig {
    const KNOWN_KEYS: &'static [&'static str] = &["internal_port"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawContextConfig {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "max_tokens",
        "record",
        "record_store",
        "seed_budget",
        "seed_overflow_margin",
        "retain",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawObservabilityConfig {
    const KNOWN_KEYS: &'static [&'static str] = &["otel_endpoint", "eval"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        if let Some(eval) = &self.eval {
            collect_block(eval, &child_path(path, "eval"), out);
        }
    }
}

impl RawBlock for RawTraceConfig {
    const KNOWN_KEYS: &'static [&'static str] = &["capture", "include_tool_output", "retain"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawEvalConfig {
    const KNOWN_KEYS: &'static [&'static str] = &["dataset_id", "scorers"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        let scorers = child_path(path, "scorers");
        for (index, scorer) in self.scorers.iter().enumerate() {
            collect_block(scorer, &format!("{scorers}[{index}]"), out);
        }
    }
}

impl RawBlock for RawScorerConfig {
    const KNOWN_KEYS: &'static [&'static str] = &["type", "name", "max", "expected"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawArtifact {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "name",
        "version",
        "runtime",
        "source",
        "local_source",
        "prompt_payload",
        "capabilities",
        "on_overflow",
        "config",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        if let Some(capabilities) = &self.capabilities {
            collect_block(capabilities, &child_path(path, "capabilities"), out);
        }
    }
}

impl RawBlock for RawCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "network",
        "peer_fetch",
        "filesystem",
        "shell",
        "spawn",
        "env",
        "limits",
        "resources",
        "state",
        "task_io",
        "conversation",
        "containment",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    /// Reached from both [`RawRuntimeManifest`] and [`RawArtifact`], so the capsule-wide block and
    /// a per-artifact one report through identical code and differ only in the `path` they are
    /// handed — `capabilities.shell` against `artifacts[0].capabilities.shell`.
    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        if let Some(network) = &self.network {
            collect_block(network, &child_path(path, "network"), out);
        }
        if let Some(peer_fetch) = &self.peer_fetch {
            collect_block(peer_fetch, &child_path(path, "peer_fetch"), out);
        }
        if let Some(filesystem) = &self.filesystem {
            collect_block(filesystem, &child_path(path, "filesystem"), out);
        }
        if let Some(shell) = &self.shell {
            collect_block(shell, &child_path(path, "shell"), out);
        }
        if let Some(spawn) = &self.spawn {
            collect_block(spawn, &child_path(path, "spawn"), out);
        }
        if let Some(env) = &self.env {
            collect_block(env, &child_path(path, "env"), out);
        }
        if let Some(limits) = &self.limits {
            collect_block(limits, &child_path(path, "limits"), out);
        }
        if let Some(resources) = &self.resources {
            collect_block(resources, &child_path(path, "resources"), out);
        }
        if let Some(state) = &self.state {
            collect_block(state, &child_path(path, "state"), out);
        }
        if let Some(task_io) = &self.task_io {
            collect_block(task_io, &child_path(path, "task_io"), out);
        }
        if let Some(conversation) = &self.conversation {
            collect_block(conversation, &child_path(path, "conversation"), out);
        }
    }
}

impl RawBlock for RawTaskIoCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["read"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawConversationCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["read"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawSpawnCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["allow"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawEnvCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["allow"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawResourceLimits {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "memory_bytes",
        "table_elements",
        "instances",
        "deadline_seconds",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawStateCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["store"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawResourceCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "max_processes",
        "max_open_files",
        "max_file_size_bytes",
        "cpu_seconds",
        "memory_bytes",
        "cgroup_memory_bytes",
        "cgroup_pids_max",
        "cgroup_cpu_percent",
        "cgroup_io_bytes_per_sec",
        "workdir_max_bytes",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawNetworkCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["allow", "unix_sockets"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawPeerFetchCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["allow"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawFilesystemCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &["scope", "workdir_exec", "read_only"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawShellCapabilities {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "allow",
        "strip_env",
        "baseline_env",
        "interpreter_runtime",
        "staged_runtime",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        let interpreter_runtime = child_path(path, "interpreter_runtime");
        for (index, grant) in self.interpreter_runtime.iter().enumerate() {
            collect_block(grant, &format!("{interpreter_runtime}[{index}]"), out);
        }
        let staged_runtime = child_path(path, "staged_runtime");
        for (index, grant) in self.staged_runtime.iter().enumerate() {
            collect_block(grant, &format!("{staged_runtime}[{index}]"), out);
        }
    }
}

impl RawBlock for RawInterpreterRuntimeGrant {
    const KNOWN_KEYS: &'static [&'static str] = &["binary", "dirs"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        let dirs = child_path(path, "dirs");
        for (index, dir) in self.dirs.iter().enumerate() {
            collect_block(dir, &format!("{dirs}[{index}]"), out);
        }
    }
}

impl RawBlock for RawInterpreterRuntimeDir {
    const KNOWN_KEYS: &'static [&'static str] = &["path", "list_dir"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawStagedRuntimeGrant {
    const KNOWN_KEYS: &'static [&'static str] = &["binary", "source_path", "pin"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawInferenceConfig {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "transport",
        "endpoint",
        "model",
        "api_key",
        "command",
        "system_prompt",
        "system_prompt_file",
        "system_prompt_artifact",
        "driver",
        "provider",
        "compaction",
        "max_turns",
        "max_task_reopens",
        "max_tokens",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }

    fn walk_children(&self, path: &str, out: &mut Vec<UnknownManifestKey>) {
        if let Some(driver) = &self.driver {
            collect_block(driver, &child_path(path, "driver"), out);
        }
        if let Some(provider) = &self.provider {
            collect_block(provider, &child_path(path, "provider"), out);
        }
        if let Some(compaction) = &self.compaction {
            collect_block(compaction, &child_path(path, "compaction"), out);
        }
    }
}

impl RawBlock for RawCompactionConfig {
    const KNOWN_KEYS: &'static [&'static str] = &[
        "threshold",
        "model",
        "system_prompt",
        "system_prompt_file",
        "dump_summaries",
    ];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

impl RawBlock for RawInferenceDriver {
    const KNOWN_KEYS: &'static [&'static str] = &["artifact", "config"];
    fn unknown_keys(&self) -> &UnknownKeys {
        &self.unknown
    }
}

/// Walks a parsed raw manifest and reports every key no block of it claimed, each block's own
/// keys before the blocks it owns, in the order the blocks appear in the manifest's own type.
///
/// Called once, immediately after `serde_yaml` returns and before any field is moved out, so the
/// result covers the whole document rather than the parts validation happens to reach. Reporting
/// only; nothing here can refuse a manifest.
fn collect_unknown_keys(raw: &RawRuntimeManifest) -> Vec<UnknownManifestKey> {
    let mut out = Vec::new();
    collect_block(raw, "", &mut out);
    out
}

/// One block's captured keys, each paired with the nearest key that block does recognize, then
/// the same for every block it owns.
///
/// `path` is empty for the manifest root, which is what
/// [`crate::unknown_manifest_keys::UnknownManifestKey`] renders as "at the top level".
fn collect_block<T: RawBlock>(block: &T, path: &str, out: &mut Vec<UnknownManifestKey>) {
    for key in block.unknown_keys().keys() {
        out.push(UnknownManifestKey {
            key: key.clone(),
            block_path: path.to_string(),
            nearest_known: nearest_known_key(key, T::KNOWN_KEYS),
        });
    }
    block.walk_children(path, out);
}

/// The dotted path of `field` inside the block at `parent`, built in one place so no
/// [`RawBlock::walk_children`] has to reproduce the rule that the manifest root has no prefix.
/// An indexed child appends its own `[index]` to the result.
fn child_path(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        return field.to_string();
    }
    format!("{parent}.{field}")
}

#[must_use = "validated runtime manifest is required before run"]
pub fn load_runtime_manifest(path: &Path) -> Result<RuntimeManifest, RuntimeManifestError> {
    let content = fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            return RuntimeManifestError::NotFound(path.display().to_string());
        }

        RuntimeManifestError::Io {
            path: path.display().to_string(),
            source,
        }
    })?;

    RuntimeManifest::from_yaml_str(&content)
}

impl RuntimeManifest {
    pub fn from_yaml_str(input: &str) -> Result<Self, RuntimeManifestError> {
        let raw: RawRuntimeManifest = serde_yaml::from_str(input).map_err(|err| {
            if let Some(location) = err.location() {
                RuntimeManifestError::YamlSyntax(format!(
                    "{}: YAML syntax error at line {}, column {}: {}",
                    MANIFEST_FILENAME,
                    location.line(),
                    location.column(),
                    err
                ))
            } else {
                RuntimeManifestError::YamlSyntax(format!(
                    "{MANIFEST_FILENAME}: YAML syntax error: {err}"
                ))
            }
        })?;

        // Collected before any field is moved out of `raw`, so the walk sees the whole document
        // rather than the part validation happens to reach before its first refusal.
        let unknown_keys = collect_unknown_keys(&raw);

        let name = raw.name.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            RuntimeManifestError::MissingField {
                field: "name".to_string(),
            }
        })?;

        let version = raw
            .version
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RuntimeManifestError::MissingField {
                field: "version".to_string(),
            })?;

        let artifacts = raw
            .artifacts
            .into_iter()
            .enumerate()
            .map(|(index, artifact)| {
                let name = artifact
                    .name
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| RuntimeManifestError::InvalidArtifact {
                        index,
                        message: "missing required field 'name'".to_string(),
                    })?;

                let source = artifact.source.filter(|s| !s.trim().is_empty());

                let runtime = match artifact.runtime.as_deref() {
                    None | Some("tool") => ArtifactRuntime::Tool,
                    Some("wasm") | Some("native") => {
                        return Err(RuntimeManifestError::InvalidArtifact {
                            index,
                            message: format!(
                                "use 'runtime: tool'; implementation is declared in the \
                                 artifact's own manifest (got '{}')",
                                artifact.runtime.as_deref().unwrap_or("")
                            ),
                        });
                    }
                    Some("driver") => ArtifactRuntime::Driver,
                    Some("hook") => ArtifactRuntime::Hook,
                    Some("skill") => ArtifactRuntime::Skill,
                    Some(other) => {
                        return Err(RuntimeManifestError::InvalidArtifact {
                            index,
                            message: format!(
                                "unknown runtime '{other}'; expected one of: tool, driver, hook, skill"
                            ),
                        });
                    }
                };

                // Capabilities are keyed on declared properties, not on the role. When the
                // property is absent we derive it from the role, which reproduces the previous
                // skill-only gate exactly and leaves every existing manifest unchanged.
                let local_source = artifact
                    .local_source
                    .unwrap_or(runtime == ArtifactRuntime::Skill);
                let prompt_payload = artifact
                    .prompt_payload
                    .unwrap_or(runtime == ArtifactRuntime::Skill);

                // `source:` requires the artifact to declare `local_source: true`.
                if source.is_some() && !local_source {
                    return Err(RuntimeManifestError::InvalidArtifact {
                        index,
                        message: format!(
                            "artifact '{name}' declares 'source:' but does not declare \
                             'local_source: true' (runtime: {})",
                            runtime.as_str()
                        ),
                    });
                }

                // When `source:` is present, `version:` is optional — substitute "local".
                // If both are set, `version:` is ignored and a warning is printed to stderr.
                let version = if source.is_some() {
                    if let Some(explicit) = artifact.version.filter(|s| !s.trim().is_empty()) {
                        eprintln!(
                            "warning: artifact '{name}' declares both 'source:' and \
                             'version: {explicit}'; version is ignored for local-source skills \
                             (using 'local')"
                        );
                    }
                    "local".to_string()
                } else {
                    artifact
                        .version
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| RuntimeManifestError::InvalidArtifact {
                            index,
                            message: "missing required field 'version'".to_string(),
                        })?
                };

                // Per-artifact `capabilities:` is enforced on every role that actually
                // executes — `hook` (default-deny grant) and `tool`/`driver` (narrowing
                // below the capsule ceiling). A `skill` has no execution surface, so the
                // block would be silently ignored there; that reads like a scoped artifact
                // when it is not, hence a parse error rather than a no-op.
                let capabilities = match artifact.capabilities {
                    None => None,
                    Some(raw_caps) => {
                        if runtime == ArtifactRuntime::Skill {
                            return Err(RuntimeManifestError::InvalidArtifact {
                                index,
                                message: format!(
                                    "artifact '{name}' declares per-artifact 'capabilities:' but \
                                     has 'runtime: {}'; per-artifact capabilities are only \
                                     recognized on 'runtime: hook', 'runtime: tool', and \
                                     'runtime: driver' entries — use the capsule-wide top-level \
                                     'capabilities:' block instead",
                                    runtime.as_str()
                                ),
                            });
                        }
                        // `task_io` grants the `murmur:task-io/read` host import, which only
                        // hook components can be handed. On a tool or driver entry nothing
                        // would enforce it, and a silently-inert grant reads like a scoped
                        // artifact — the same reason `on_overflow:` below is a parse error off
                        // a hook.
                        if raw_caps.task_io.is_some() && runtime != ArtifactRuntime::Hook {
                            return Err(RuntimeManifestError::InvalidArtifact {
                                index,
                                message: format!(
                                    "artifact '{name}' declares 'capabilities.task_io:' but has \
                                     'runtime: {}'; the key grants the murmur:task-io/read host \
                                     import and is only recognized on 'runtime: hook' entries",
                                    runtime.as_str()
                                ),
                            });
                        }
                        // `conversation` grants the `murmur:conversation/read` host import, on the
                        // same terms as `task_io` above: only a hook component's world can import
                        // it, so on any other role the grant would be silently inert.
                        if raw_caps.conversation.is_some() && runtime != ArtifactRuntime::Hook {
                            return Err(RuntimeManifestError::InvalidArtifact {
                                index,
                                message: format!(
                                    "artifact '{name}' declares 'capabilities.conversation:' but \
                                     has 'runtime: {}'; the key grants the \
                                     murmur:conversation/read host import and is only recognized \
                                     on 'runtime: hook' entries",
                                    runtime.as_str()
                                ),
                            });
                        }
                        parse_capabilities(Some(raw_caps))?
                    }
                };

                // `on_overflow:` governs an async hook's job queue, which only exists for
                // `runtime: hook`. On any other role it would be silently ignored, so it is
                // a parse error for the same reason per-artifact `capabilities:` is on a
                // skill. It stays legal on a *blocking* hook entry (simply inert): the
                // execution mode lives in the artifact's own manifest and is unknown here.
                let on_overflow = match artifact.on_overflow.as_deref() {
                    None => HookOverflowPolicy::default(),
                    Some(raw) => {
                        if runtime != ArtifactRuntime::Hook {
                            return Err(RuntimeManifestError::InvalidArtifact {
                                index,
                                message: format!(
                                    "artifact '{name}' declares 'on_overflow:' but has \
                                     'runtime: {}'; the key governs an async hook's job queue \
                                     and is only recognized on 'runtime: hook' entries",
                                    runtime.as_str()
                                ),
                            });
                        }
                        match raw {
                            "drop" => HookOverflowPolicy::Drop,
                            "block" => HookOverflowPolicy::Block,
                            other => {
                                return Err(RuntimeManifestError::InvalidArtifact {
                                    index,
                                    message: format!(
                                        "artifact '{name}' declares unknown on_overflow \
                                         '{other}'; expected: drop, block"
                                    ),
                                });
                            }
                        }
                    }
                };

                // `config:` reaches an artifact through that artifact's own grant, and a skill
                // holds no grant and runs no component — nothing would ever deliver it. Refused
                // rather than ignored, for the reason the two gates above are: a key that reads
                // like configuration and is silently dropped is worse than one that is refused.
                // The block's *shape* is the runtime's business, not this parser's; only the
                // role is decided here.
                if artifact.config.is_some() && runtime == ArtifactRuntime::Skill {
                    return Err(RuntimeManifestError::InvalidArtifact {
                        index,
                        message: format!(
                            "artifact '{name}' declares 'config:' but has 'runtime: {}'; the key \
                             is recognized only on 'runtime: hook', 'runtime: tool', and \
                             'runtime: driver' entries",
                            runtime.as_str()
                        ),
                    });
                }

                Ok(RuntimeArtifact {
                    name,
                    version,
                    runtime,
                    source,
                    local_source,
                    prompt_payload,
                    capabilities,
                    on_overflow,
                    config: artifact.config,
                })
            })
            .collect::<Result<Vec<_>, RuntimeManifestError>>()?;

        // The capsule-wide block reaches capsule, tool and driver components, none of which
        // can be handed the murmur:task-io/read import. Rejected rather than ignored, on the
        // same terms as declaring it on a non-hook artifact entry.
        if raw
            .capabilities
            .as_ref()
            .is_some_and(|caps| caps.task_io.is_some())
        {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: "capabilities.task_io".to_string(),
                message: "is recognized only on 'runtime: hook' artifact entries, not in the \
                          capsule-wide capabilities block — the grant is per-hook"
                    .to_string(),
            });
        }
        // Config is delivered on one artifact's own grant, so there is no capsule-wide form of
        // it: the capsule's guest holds no artifact grant, and a top-level block would reach no
        // component at all. Refused rather than ignored, on the same terms as the capsule-wide
        // `task_io` block above.
        if raw.config.is_some() {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: "config".to_string(),
                message: "is declared per artifact, on the 'runtime: hook', 'runtime: tool' or \
                          'runtime: driver' entry that reads it — there is no capsule-wide \
                          config block"
                    .to_string(),
            });
        }
        let capabilities = parse_capabilities(raw.capabilities)?;
        let inference = parse_inference(raw.inference)?;

        // Validate system_prompt_artifact: must name a declared artifact whose payload may be
        // bound as the system prompt (`prompt_payload`, defaulted from the role when absent).
        if let Some(ref sp_art) = inference
            .as_ref()
            .and_then(|i| i.system_prompt_artifact.clone())
        {
            let matching = artifacts.iter().find(|a| &a.name == sp_art);
            match matching {
                None => {
                    return Err(RuntimeManifestError::InvalidInferenceConfig {
                        field: "inference.system_prompt_artifact".to_string(),
                        message: format!("artifact '{sp_art}' is not declared in artifacts:"),
                    });
                }
                Some(a) if !a.prompt_payload => {
                    return Err(RuntimeManifestError::InvalidInferenceConfig {
                        field: "inference.system_prompt_artifact".to_string(),
                        message: format!(
                            "artifact '{sp_art}' (runtime: {}) does not declare \
                             'prompt_payload: true', which system_prompt_artifact requires",
                            a.runtime.as_str()
                        ),
                    });
                }
                Some(_) => {} // valid
            }
        }
        let context = parse_context(raw.context)?;
        let observability = parse_observability(raw.observability);
        let trace = raw
            .trace
            .map(|raw_trace| {
                let capture = resolve_trace_capture(
                    raw_trace.capture.as_deref(),
                    raw_trace.include_tool_output,
                )?;
                let retain = parse_trace_retain(raw_trace.retain)?;
                Ok::<_, RuntimeManifestError>(TraceConfig { capture, retain })
            })
            .transpose()?;
        let network = raw.network.map(|n| NetworkConfig {
            internal_port: n.internal_port,
        });
        let lifecycle = raw.lifecycle.map(|raw_lc| {
            let defaults = LifecycleConfig::default();
            LifecycleConfig {
                task_acceptance: raw_lc.task_acceptance.unwrap_or(defaults.task_acceptance),
                after_task: raw_lc.after_task.unwrap_or(defaults.after_task),
                queue_depth: raw_lc.queue_depth.unwrap_or(defaults.queue_depth),
                input_timeout_secs: raw_lc.input_timeout_secs,
                conversation_mode: raw_lc.conversation.unwrap_or(defaults.conversation_mode),
                max_task_reopens: raw_lc.max_task_reopens.unwrap_or(defaults.max_task_reopens),
                shell_grace_secs: raw_lc.shell_grace_secs.unwrap_or(defaults.shell_grace_secs),
            }
        });

        let exports = parse_exports(raw.exports)?;

        Ok(Self {
            name,
            version,
            artifacts,
            capabilities,
            inference,
            context,
            observability,
            trace,
            network,
            lifecycle,
            exports,
            mur_version: raw.mur_version.filter(|s| !s.trim().is_empty()),
            unknown_keys,
        })
    }
}

/// Lowers the top-level `exports:` block, rejecting anything the runtime would otherwise have to
/// decide about later.
///
/// The `root` rules are checked here and never again by a caller: a gateway that "cleans up" a
/// path is a gateway that has taken over the containment boundary, so the only legal answer to a
/// root naming somewhere outside the workdir is a refusal.
fn parse_exports(raw: Option<RawExports>) -> Result<Option<Exports>, RuntimeManifestError> {
    let Some(raw_exports) = raw else {
        return Ok(None);
    };
    let files = raw_exports.files.map(parse_file_export).transpose()?;
    let peer_files = raw_exports
        .peer_files
        .map(parse_peer_files_export)
        .transpose()?;
    Ok(Some(Exports { files, peer_files }))
}

/// Lowers `exports.peer_files`, on exactly the same terms as [`parse_file_export`]: a root that
/// could name somewhere outside the workdir is refused here and never repaired later.
///
/// `max_ttl` is left as the operator declared it, including absent. Whether absent is legal
/// depends on `lifecycle.after_task`, which is a different block of the same manifest and a
/// different kind of question — one about what survives teardown rather than about whether a
/// value is well-formed. The runtime asks it, at launch, and refuses with `E-CAP-008`.
fn parse_peer_files_export(
    raw: RawPeerFilesExport,
) -> Result<PeerFilesExport, RuntimeManifestError> {
    let invalid = |field: &str, message: String| RuntimeManifestError::InvalidExports {
        field: field.to_string(),
        message,
    };

    let root = raw
        .root
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .ok_or_else(|| {
            invalid(
                "exports.peer_files.root",
                format!("is required and {EXPORT_ROOT_ACCEPTED_FORM}"),
            )
        })?;
    check_export_root_shape("exports.peer_files.root", &root)?;

    let max_ttl_secs = match raw.max_ttl {
        None => None,
        Some(value) => {
            let text = scalar_text(&value).ok_or_else(|| {
                invalid(
                    "exports.peer_files.max_ttl",
                    format!("'{value:?}' {DURATION_ACCEPTED_FORM}"),
                )
            })?;
            let parsed = parse_duration_secs(&text)
                .map_err(|message| invalid("exports.peer_files.max_ttl", message))?;
            if parsed == 0 {
                return Err(invalid(
                    "exports.peer_files.max_ttl",
                    format!("must be greater than zero; {DURATION_ACCEPTED_FORM}"),
                ));
            }
            Some(parsed)
        }
    };

    let max_bytes = match raw.max_bytes {
        None => DEFAULT_PEER_FILES_MAX_BYTES,
        Some(value) => {
            let text = scalar_text(&value).ok_or_else(|| {
                invalid(
                    "exports.peer_files.max_bytes",
                    format!("'{value:?}' {BYTE_SIZE_ACCEPTED_FORM}"),
                )
            })?;
            let parsed = parse_byte_size(&text)
                .map_err(|message| invalid("exports.peer_files.max_bytes", message))?;
            if parsed == 0 {
                return Err(invalid(
                    "exports.peer_files.max_bytes",
                    format!("must be greater than zero; {BYTE_SIZE_ACCEPTED_FORM}"),
                ));
            }
            parsed
        }
    };

    Ok(PeerFilesExport {
        root,
        max_ttl_secs,
        max_bytes,
    })
}

/// A YAML scalar as the text the byte-size and duration parsers expect, or `None` for a mapping
/// or a sequence — a shape neither parser could report on without quoting the whole node.
/// `Some` for a key that is present, whatever its value — including an explicit null.
///
/// The plain `Option<serde_yaml::Value>` a `#[serde(default)]` field gets collapses `retain:`
/// with nothing under it into the same `None` an absent key produces, and those two say opposite
/// things: absent means "keep everything", and empty is refused.
fn present_yaml_value<'de, D>(deserializer: D) -> Result<Option<serde_yaml::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde_yaml::Value::deserialize(deserializer).map(Some)
}

fn scalar_text(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(text) => Some(text.clone()),
        serde_yaml::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The shared `root` shape rule: relative, no `..`, no absolute prefix. Both export blocks
/// resolve their root against the same accessible workdir, so both refuse the same shapes with
/// the same sentence.
fn check_export_root_shape(field: &str, root: &str) -> Result<(), RuntimeManifestError> {
    for component in Path::new(root).components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                return Err(RuntimeManifestError::InvalidExports {
                    field: field.to_string(),
                    message: format!(
                        "'{root}' contains a '..' component; {EXPORT_ROOT_ACCEPTED_FORM}"
                    ),
                });
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(RuntimeManifestError::InvalidExports {
                    field: field.to_string(),
                    message: format!("'{root}' is absolute; {EXPORT_ROOT_ACCEPTED_FORM}"),
                });
            }
        }
    }
    Ok(())
}

fn parse_file_export(raw: RawFileExport) -> Result<FileExport, RuntimeManifestError> {
    let invalid = |field: &str, message: String| RuntimeManifestError::InvalidExports {
        field: field.to_string(),
        message,
    };

    let root = raw
        .root
        .map(|root| root.trim().to_string())
        .filter(|root| !root.is_empty())
        .ok_or_else(|| {
            invalid(
                "exports.files.root",
                format!("is required and {EXPORT_ROOT_ACCEPTED_FORM}"),
            )
        })?;
    check_export_root_shape("exports.files.root", &root)?;

    let mode = match raw.mode.as_deref().map(str::trim) {
        None | Some("") => {
            return Err(invalid(
                "exports.files.mode",
                "is required and must be 'read-only'".to_string(),
            ));
        }
        Some("read-only") => ExportMode::ReadOnly,
        Some(other) => {
            return Err(invalid(
                "exports.files.mode",
                format!("'{other}' must be 'read-only'"),
            ));
        }
    };

    let max_bytes = match raw.max_bytes {
        None => DEFAULT_EXPORT_MAX_BYTES,
        Some(value) => {
            let Some(text) = scalar_text(&value) else {
                return Err(invalid(
                    "exports.files.max_bytes",
                    format!("'{value:?}' {BYTE_SIZE_ACCEPTED_FORM}"),
                ));
            };
            let parsed = parse_byte_size(&text)
                .map_err(|message| invalid("exports.files.max_bytes", message))?;
            if parsed == 0 {
                return Err(invalid(
                    "exports.files.max_bytes",
                    format!("must be greater than zero; {BYTE_SIZE_ACCEPTED_FORM}"),
                ));
            }
            parsed
        }
    };

    Ok(FileExport {
        root,
        mode,
        max_bytes,
    })
}

/// The accepted spelling of `exports.files.root`, stated once so every rejection says the same
/// thing about what a legal root looks like.
const EXPORT_ROOT_ACCEPTED_FORM: &str = "must be a relative path inside the workdir";

/// The accepted spelling of `capabilities.peer_fetch.allow`, stated once.
pub const PEER_FETCH_ALLOW_ACCEPTED_FORM: &str = "must be a non-empty list of network destinations";

/// Structural check on one network destination: an `http`/`https` URL with no path, query or
/// fragment, or a bare `host[:port]`.
///
/// Deliberately shape-only. The authoritative matcher is the runtime's network-rule parser, which
/// this crate cannot see; what this catches is the class of entry that could never become a rule
/// at all, at the line that wrote it rather than at launch.
fn check_network_destination_shape(entry: &str) -> Result<(), String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "'{entry}' is empty; {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
        ));
    }

    if trimmed.contains("://") {
        let url = Url::parse(trimmed).map_err(|error| {
            format!("'{entry}' is not a valid URL ({error}); {PEER_FETCH_ALLOW_ACCEPTED_FORM}")
        })?;
        let scheme = url.scheme().to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!(
                "'{entry}' has unsupported scheme '{scheme}' (expected http or https); \
                 {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(format!(
                "'{entry}' names no host; {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
            ));
        }
        if url.query().is_some() || url.fragment().is_some() || !matches!(url.path(), "" | "/") {
            return Err(format!(
                "'{entry}' must not include a path, query or fragment; \
                 {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
            ));
        }
        return Ok(());
    }

    if trimmed.contains('/') || trimmed.split_whitespace().count() != 1 {
        return Err(format!(
            "'{entry}' is not a host[:port]; {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
        ));
    }
    // Split from the right so an IPv6 literal's own colons are not mistaken for a port separator.
    if let Some((host, port)) = trimmed.rsplit_once(':') {
        if !host.ends_with(']') {
            if host.is_empty() {
                return Err(format!(
                    "'{entry}' names no host; {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
                ));
            }
            if port.parse::<u16>().is_err() {
                return Err(format!(
                    "'{entry}' has an unparseable port '{port}'; {PEER_FETCH_ALLOW_ACCEPTED_FORM}"
                ));
            }
        }
    }
    Ok(())
}

fn parse_capabilities(
    raw: Option<RawCapabilities>,
) -> Result<Option<Capabilities>, RuntimeManifestError> {
    let Some(raw_caps) = raw else {
        return Ok(None);
    };

    let network = raw_caps.network.map(|raw_network| NetworkCapabilities {
        allow: raw_network.allow,
        unix_sockets: raw_network.unix_sockets,
    });

    let peer_fetch = raw_caps
        .peer_fetch
        .map(|raw_peer_fetch| {
            if raw_peer_fetch.allow.is_empty() {
                return Err(RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.peer_fetch.allow".to_string(),
                    message: PEER_FETCH_ALLOW_ACCEPTED_FORM.to_string(),
                });
            }
            for entry in &raw_peer_fetch.allow {
                check_network_destination_shape(entry).map_err(|message| {
                    RuntimeManifestError::InvalidCapabilities {
                        field: "capabilities.peer_fetch.allow".to_string(),
                        message,
                    }
                })?;
            }
            Ok(PeerFetchCapabilities {
                allow: raw_peer_fetch.allow,
            })
        })
        .transpose()?;

    let filesystem = raw_caps
        .filesystem
        .map(|raw_filesystem| FilesystemCapabilities {
            scope: raw_filesystem.scope.and_then(|scope| {
                let trimmed = scope.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }),
            // Copied straight across: a bool has no empty/whitespace case to normalise, and there
            // is nothing to validate here — the *consequence* of declaring it (no `scoped`, and a
            // refusal when `capabilities.containment: scoped` is also declared) belongs to the
            // runtime's containment layer, not to manifest parsing.
            workdir_exec: raw_filesystem.workdir_exec,
            // Normalised on exactly the terms `scope` above is: trimmed, and an entry left empty
            // by that trim is dropped rather than carried as a rule covering nothing. Shape is
            // not judged here either — an absolute or escaping entry is the runtime's refusal.
            read_only: raw_filesystem
                .read_only
                .into_iter()
                .filter_map(|entry| {
                    let trimmed = entry.trim();
                    (!trimmed.is_empty()).then(|| trimmed.to_string())
                })
                .collect(),
        });

    let shell = raw_caps
        .shell
        .map(|raw_shell| {
            if raw_shell.allow.is_empty() {
                return Err(RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.shell.allow".to_string(),
                    message: "must contain at least one binary".to_string(),
                });
            }

            let interpreter_runtime =
                parse_interpreter_runtime(&raw_shell.allow, raw_shell.interpreter_runtime)?;
            // Parsed after `interpreter_runtime` because the mutual-exclusion rule needs the
            // already-validated grant list to test against.
            let staged_runtime = parse_staged_runtime(
                &raw_shell.allow,
                &interpreter_runtime,
                raw_shell.staged_runtime,
            )?;

            Ok(ShellCapabilities {
                allow: raw_shell.allow,
                strip_env: raw_shell.strip_env,
                baseline_env: raw_shell.baseline_env,
                interpreter_runtime,
                staged_runtime,
            })
        })
        .transpose()?;

    let spawn = raw_caps.spawn.map(|raw_spawn| SpawnCapabilities {
        allow: raw_spawn.allow,
    });

    let env = raw_caps.env.map(|raw_env| EnvCapabilities {
        allow: raw_env.allow,
    });

    let limits = raw_caps.limits.map(parse_resource_limits).transpose()?;

    let resources = raw_caps
        .resources
        .map(parse_resource_capabilities)
        .transpose()?;

    let state = raw_caps.state.map(parse_state_capabilities);

    let task_io = raw_caps
        .task_io
        .map(|raw_task_io| {
            raw_task_io
                .read
                .map(|read| TaskIoCapabilities { read })
                .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.task_io.read".to_string(),
                    message: "must be set explicitly to true or false — a capability is never \
                              inferred"
                        .to_string(),
                })
        })
        .transpose()?;

    let conversation = raw_caps
        .conversation
        .map(|raw_conversation| {
            raw_conversation
                .read
                .map(|read| ConversationCapabilities { read })
                .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.conversation.read".to_string(),
                    message: "must be set explicitly to true or false — a capability is never \
                              inferred"
                        .to_string(),
                })
        })
        .transpose()?;

    let containment = raw_caps
        .containment
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value.parse::<ContainmentClass>().map_err(|error| {
                RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.containment".to_string(),
                    message: error.to_string(),
                }
            })
        })
        .transpose()?;

    Ok(Some(Capabilities {
        network,
        peer_fetch,
        filesystem,
        shell,
        spawn,
        env,
        limits,
        resources,
        state,
        task_io,
        conversation,
        containment,
    }))
}

/// Lower `capabilities.state`. Infallible: the only field is a name, and a name's *shape* is a
/// runtime question (does it resolve to one directory under `~/.murmur/state/`?) rather than a
/// manifest-parse one — `capsule_runtime::state_store::validate_store_name` answers it and refuses
/// with `E-CAP-009`.
///
/// Surrounding whitespace is trimmed but an emptied value is *kept*, unlike
/// `capabilities.filesystem.scope`, which normalises empty to absent. `store: ""` is a written
/// declaration that names nothing, and silently defaulting it to the capsule name would hand a
/// store to someone who asked for a different one; it reaches the runtime and is refused there.
fn parse_state_capabilities(raw: RawStateCapabilities) -> StateCapabilities {
    StateCapabilities {
        store: raw.store.map(|store| store.trim().to_string()),
    }
}

/// Lower and validate `capabilities.shell.interpreter_runtime`. Every rejection is an
/// [`RuntimeManifestError::InvalidCapabilities`] naming the offending value, matching the rest
/// of this file's "structurally present but semantically invalid" reporting. Nothing here can
/// expand a whole install prefix: each grant names specific directories, and each directory
/// carries an explicit `list_dir` the author had to write — enumerability is never inferred.
fn parse_interpreter_runtime(
    allow: &[String],
    raw_grants: Vec<RawInterpreterRuntimeGrant>,
) -> Result<Vec<InterpreterRuntimeGrant>, RuntimeManifestError> {
    let mut grants = Vec::with_capacity(raw_grants.len());
    for (index, raw_grant) in raw_grants.into_iter().enumerate() {
        let base = format!("capabilities.shell.interpreter_runtime[{index}]");

        let binary = raw_grant
            .binary
            .filter(|b| !b.trim().is_empty())
            .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: "must name a binary".to_string(),
            })?;

        // The whole point of this mechanism: it narrows filesystem access alongside an exec
        // grant that already exists. It can never itself grant exec, so the binary must already
        // be allowlisted in this same block's `allow`.
        if !allow.contains(&binary) {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: format!(
                    "'{binary}' is not in capabilities.shell.allow — interpreter_runtime can \
                     only narrow filesystem access for an already-allowlisted binary, never \
                     grant exec"
                ),
            });
        }

        if raw_grant.dirs.is_empty() {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.dirs"),
                message: "must name at least one directory".to_string(),
            });
        }

        let mut dirs = Vec::with_capacity(raw_grant.dirs.len());
        for (dir_index, raw_dir) in raw_grant.dirs.into_iter().enumerate() {
            let dir_base = format!("{base}.dirs[{dir_index}]");

            let path = raw_dir
                .path
                .filter(|p| !p.trim().is_empty())
                .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                    field: format!("{dir_base}.path"),
                    message: "must name a host directory".to_string(),
                })?;
            if !path.starts_with('/') {
                return Err(RuntimeManifestError::InvalidCapabilities {
                    field: format!("{dir_base}.path"),
                    message: format!(
                        "'{path}' must be an absolute host path (start with '/') — these are \
                         filesystem paths outside the workdir"
                    ),
                });
            }

            let list_dir =
                raw_dir
                    .list_dir
                    .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                        field: format!("{dir_base}.list_dir"),
                        message: format!(
                            "'{path}' must set list_dir explicitly to true or false — \
                         enumerability is never inferred"
                        ),
                    })?;

            dirs.push(InterpreterRuntimeDir { path, list_dir });
        }

        grants.push(InterpreterRuntimeGrant { binary, dirs });
    }

    Ok(grants)
}

/// Lower and validate `capabilities.shell.staged_runtime`, given this same block's `allow` list
/// and its already-validated `interpreter_runtime` grants. Every rejection is a
/// [`RuntimeManifestError::InvalidCapabilities`] naming the offending field, matching
/// [`parse_interpreter_runtime`] and the rest of this file.
///
/// Four rules, each of them a thing that would otherwise be silently wrong at launch rather than
/// loudly wrong here:
///
///   1. `binary` must already be in `allow` — staging a runtime tree never itself grants exec.
///   2. `binary` must not also carry an `interpreter_runtime` grant — the two are alternatives,
///      and declaring both means the author expects a host-widening grant that a composed root
///      makes both unnecessary and (once Landlock re-installs inside the root) meaningless.
///   3. `source_path` must be absolute — it is a host path outside the workdir, resolved by the
///      launch host and not relative to anything the runtime knows.
///   4. `pin` must be present and non-empty — it is the value a human compares across two hosts,
///      so an absent one defeats the field's only purpose.
///
/// Nothing here touches the filesystem: whether `source_path` exists on *this* host is a launch
/// -time fact, not a manifest-validity one, and a manifest must stay parseable on a machine that
/// will never run it (`mur build` on a laptop for a Linux fleet).
fn parse_staged_runtime(
    allow: &[String],
    interpreter_runtime: &[InterpreterRuntimeGrant],
    raw_grants: Vec<RawStagedRuntimeGrant>,
) -> Result<Vec<StagedRuntimeGrant>, RuntimeManifestError> {
    let mut grants = Vec::with_capacity(raw_grants.len());
    for (index, raw_grant) in raw_grants.into_iter().enumerate() {
        let base = format!("capabilities.shell.staged_runtime[{index}]");

        let binary = raw_grant
            .binary
            .filter(|b| !b.trim().is_empty())
            .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: "must name a binary".to_string(),
            })?;

        if !allow.contains(&binary) {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: format!(
                    "'{binary}' is not in capabilities.shell.allow — staged_runtime can only \
                     stage the runtime tree behind an already-allowlisted binary, never grant exec"
                ),
            });
        }

        if interpreter_runtime
            .iter()
            .any(|grant| grant.binary == binary)
        {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: format!(
                    "'{binary}' also has a capabilities.shell.interpreter_runtime grant — the two \
                     are mutually exclusive per binary: staged_runtime bind-mounts the runtime \
                     tree into the capsule's own root, which is what makes widening the capsule's \
                     host filesystem scope unnecessary rather than something to add to it. Remove \
                     the interpreter_runtime grant for '{binary}'"
                ),
            });
        }

        let source_path = raw_grant
            .source_path
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.source_path"),
                message: "must name the host directory holding the pinned runtime tree".to_string(),
            })?;
        if !source_path.starts_with('/') {
            return Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.source_path"),
                message: format!(
                    "'{source_path}' must be an absolute host path (start with '/') — \
                     staged-runtime sources are host filesystem paths outside the workdir"
                ),
            });
        }

        let pin = raw_grant
            .pin
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.pin"),
                message: format!(
                    "'{source_path}' must declare a non-empty pin identifying which build this \
                     tree is — it is never inferred, because it is the value a human compares \
                     across two hosts to confirm the same runtime shipped to both"
                ),
            })?;

        grants.push(StagedRuntimeGrant {
            binary,
            source_path,
            pin,
        });
    }

    Ok(grants)
}

/// Lower `capabilities.limits`, rejecting values that can only ever produce a guest that
/// traps on its first instruction. A zero cap is always a manifest authoring mistake, and
/// catching it here means `mur run` fails at parse time with a field name rather than
/// surfacing much later as an opaque wasm trap.
fn parse_resource_limits(raw: RawResourceLimits) -> Result<ResourceLimits, RuntimeManifestError> {
    let reject_zero_usize =
        |value: Option<usize>, field: &str| -> Result<(), RuntimeManifestError> {
            match value {
                Some(0) => Err(RuntimeManifestError::InvalidCapabilities {
                    field: format!("capabilities.limits.{field}"),
                    message: "must be greater than zero".to_string(),
                }),
                _ => Ok(()),
            }
        };

    reject_zero_usize(raw.memory_bytes, "memory_bytes")?;
    reject_zero_usize(raw.table_elements, "table_elements")?;
    reject_zero_usize(raw.instances, "instances")?;
    if raw.deadline_seconds == Some(0) {
        return Err(RuntimeManifestError::InvalidCapabilities {
            field: "capabilities.limits.deadline_seconds".to_string(),
            message: "must be greater than zero".to_string(),
        });
    }

    Ok(ResourceLimits {
        memory_bytes: raw.memory_bytes,
        table_elements: raw.table_elements,
        instances: raw.instances,
        deadline_seconds: raw.deadline_seconds,
    })
}

/// Lower and validate `capabilities.resources`. Mirrors [`parse_resource_limits`]: nothing is
/// defaulted here (the runtime owns the defaults, so "omitted" must stay distinguishable from
/// "declared"), and a declared `0` is rejected outright on every field — a zero ceiling is
/// never what an author means, and letting it through would turn a typo into a subprocess that
/// cannot fork, open a file, or run for a single CPU-second.
fn parse_resource_capabilities(
    raw: RawResourceCapabilities,
) -> Result<ResourceCapabilities, RuntimeManifestError> {
    let reject_zero = |value: Option<u64>, field: &str| -> Result<(), RuntimeManifestError> {
        match value {
            Some(0) => Err(RuntimeManifestError::InvalidCapabilities {
                field: format!("capabilities.resources.{field}"),
                message: "must be greater than zero".to_string(),
            }),
            _ => Ok(()),
        }
    };

    reject_zero(raw.max_processes, "max_processes")?;
    reject_zero(raw.max_open_files, "max_open_files")?;
    reject_zero(raw.max_file_size_bytes, "max_file_size_bytes")?;
    reject_zero(raw.cpu_seconds, "cpu_seconds")?;
    reject_zero(raw.memory_bytes, "memory_bytes")?;
    reject_zero(raw.cgroup_memory_bytes, "cgroup_memory_bytes")?;
    reject_zero(raw.cgroup_pids_max, "cgroup_pids_max")?;
    reject_zero(raw.cgroup_cpu_percent.map(u64::from), "cgroup_cpu_percent")?;
    reject_zero(raw.cgroup_io_bytes_per_sec, "cgroup_io_bytes_per_sec")?;
    reject_zero(raw.workdir_max_bytes, "workdir_max_bytes")?;

    Ok(ResourceCapabilities {
        max_processes: raw.max_processes,
        max_open_files: raw.max_open_files,
        max_file_size_bytes: raw.max_file_size_bytes,
        cpu_seconds: raw.cpu_seconds,
        memory_bytes: raw.memory_bytes,
        cgroup_memory_bytes: raw.cgroup_memory_bytes,
        cgroup_pids_max: raw.cgroup_pids_max,
        cgroup_cpu_percent: raw.cgroup_cpu_percent,
        cgroup_io_bytes_per_sec: raw.cgroup_io_bytes_per_sec,
        workdir_max_bytes: raw.workdir_max_bytes,
    })
}

fn parse_inference(
    raw: Option<RawInferenceConfig>,
) -> Result<Option<InferenceConfig>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let transport = raw
        .transport
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http".to_string());

    let compaction = parse_compaction(raw.compaction)?;
    let system_prompt = optional_trimmed_string(raw.system_prompt);
    let system_prompt_file = optional_trimmed_string(raw.system_prompt_file);
    let system_prompt_artifact = optional_trimmed_string(raw.system_prompt_artifact);

    let prompt_sources_set = [
        system_prompt.is_some(),
        system_prompt_file.is_some(),
        system_prompt_artifact.is_some(),
    ]
    .iter()
    .filter(|&&b| b)
    .count();
    if prompt_sources_set > 1 {
        return Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.system_prompt".to_string(),
            message: "at most one of system_prompt, system_prompt_file, system_prompt_artifact \
                      may be set"
                .to_string(),
        });
    }

    let max_turns = match raw.max_turns {
        None => 10,
        Some(0) => {
            return Err(RuntimeManifestError::InvalidInferenceConfig {
                field: "inference.max_turns".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }
        Some(n) => n,
    };

    // The reopen budget is a lifecycle knob, read from `lifecycle.max_task_reopens`. Nothing
    // here consumes the `inference` spelling, so it would be silently inert — reject it rather
    // than let it look effective.
    if raw.max_task_reopens.is_some() {
        return Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.max_task_reopens".to_string(),
            message: "is not a valid inference field; set lifecycle.max_task_reopens instead"
                .to_string(),
        });
    }

    match transport.as_str() {
        "http" => {
            let endpoint = required_inference_field(raw.endpoint, "endpoint")?;
            validate_inference_endpoint(&endpoint)?;
            let model = required_inference_field(raw.model, "model")?;

            let api_key = match raw.api_key {
                None => None,
                Some(value) => {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(resolve_inference_api_key(trimmed)?)
                    }
                }
            };

            let driver_raw = raw.driver.or(raw.provider).ok_or_else(|| {
                RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.driver".to_string(),
                    message: "missing required field".to_string(),
                }
            })?;
            let driver = parse_inference_driver(driver_raw)?;

            // Advisory only: catch an authoring typo at parse time rather than as a confusing
            // provider 400. Large values are deliberately NOT clamped — a ceiling here would
            // block a model whose real limit is higher than anything we could hard-code.
            if raw.max_tokens == Some(0) {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.max_tokens".to_string(),
                    message: "must be greater than 0".to_string(),
                });
            }

            Ok(Some(InferenceConfig {
                transport,
                endpoint: Some(endpoint),
                model,
                api_key,
                driver: Some(driver),
                command: None,
                compaction,
                system_prompt,
                system_prompt_file,
                system_prompt_artifact,
                max_turns,
                max_tokens: raw.max_tokens,
            }))
        }
        "process" => {
            // Fields that are invalid with transport: process
            if raw.driver.is_some() || raw.provider.is_some() {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.driver.artifact".to_string(),
                    message: "is not valid with transport: process".to_string(),
                });
            }
            if raw
                .endpoint
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.endpoint".to_string(),
                    message: "is not valid with transport: process".to_string(),
                });
            }
            if raw
                .api_key
                .as_ref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
            {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.api_key".to_string(),
                    message: "is not valid with transport: process".to_string(),
                });
            }
            // The CLI subprocess path never builds a driver payload, so this value would be
            // silently inert here — reject it rather than let it look effective.
            if raw.max_tokens.is_some() {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.max_tokens".to_string(),
                    message: "is not valid with transport: process".to_string(),
                });
            }

            let command = required_inference_field(raw.command, "command")?;
            // model is OPTIONAL for transport: process — an empty string means "use the CLI's
            // configured/account-default model" (e.g. a codex subscription's default; passing an
            // unsupported model there is a hard 400). The Claude dialect still needs a real model
            // and will surface the CLI's own error if given none.
            let model = optional_trimmed_string(raw.model).unwrap_or_default();

            Ok(Some(InferenceConfig {
                transport,
                endpoint: None,
                model,
                api_key: None,
                driver: None,
                command: Some(command),
                compaction,
                system_prompt,
                system_prompt_file,
                system_prompt_artifact,
                max_turns,
                max_tokens: None,
            }))
        }
        other => Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.transport".to_string(),
            message: format!("unknown value '{other}'"),
        }),
    }
}

fn parse_context(
    raw: Option<RawContextConfig>,
) -> Result<Option<ContextConfig>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    if let Some(max_tokens) = raw.max_tokens {
        if max_tokens == 0 {
            return Err(RuntimeManifestError::InvalidInferenceConfig {
                field: "context.max_tokens".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }
    }

    let fraction = |field: &str, value: Option<f32>, default: f32| match value {
        None => Ok(default),
        Some(v) if (0.0..=1.0).contains(&v) => Ok(v),
        Some(_) => Err(RuntimeManifestError::InvalidInferenceConfig {
            field: field.to_string(),
            message: "must be between 0.0 and 1.0".to_string(),
        }),
    };

    let record = match raw.record.as_deref().map(str::trim) {
        None => true,
        Some("on") => true,
        Some("off") => false,
        Some(other) => {
            return Err(RuntimeManifestError::InvalidInferenceConfig {
                field: "context.record".to_string(),
                message: format!("unknown value '{other}'; expected: on, off"),
            })
        }
    };

    Ok(Some(ContextConfig {
        max_tokens: raw.max_tokens,
        record,
        record_store: raw.record_store.map(|store| store.trim().to_string()),
        seed_budget: fraction("context.seed_budget", raw.seed_budget, DEFAULT_SEED_BUDGET)?,
        seed_overflow_margin: fraction(
            "context.seed_overflow_margin",
            raw.seed_overflow_margin,
            DEFAULT_SEED_OVERFLOW_MARGIN,
        )?,
        retain: parse_context_retain(raw.retain)?,
    }))
}

/// The keys `trace.retain` accepts, in the order an error lists them.
const TRACE_RETAIN_KEYS: [&str; 2] = ["max_sessions", "max_age"];

/// The keys `context.retain` accepts, in the order an error lists them.
const CONTEXT_RETAIN_KEYS: [&str; 2] = ["max_messages", "max_age"];

/// What both `retain:` blocks say when they carry nothing. Stated once so the two errors, which
/// travel through different `RuntimeManifestError` variants, cannot drift apart.
const RETAIN_EMPTY_BLOCK: &str = "must declare at least one key — omit the block entirely to \
                                  keep everything, which is what no policy means";

/// One `retain:` block, reduced to its two recognized keys.
///
/// Shared by `trace.retain` and `context.retain`: they differ only in the name of the count key,
/// so the shape check, the unknown-key refusal and the empty-block refusal are written once.
/// `count_key` is `max_sessions` or `max_messages`; `block` is the dotted path an error names.
fn parse_retain_block(
    block: &str,
    count_key: &str,
    accepted: &[&str],
    raw: serde_yaml::Value,
    invalid: &dyn Fn(String, String) -> RuntimeManifestError,
) -> Result<(Option<u32>, Option<u64>), RuntimeManifestError> {
    let mapping = match raw {
        serde_yaml::Value::Mapping(mapping) => mapping,
        // `retain:` with nothing under it parses as null and means exactly `retain: {}`.
        serde_yaml::Value::Null => {
            return Err(invalid(block.to_string(), RETAIN_EMPTY_BLOCK.to_string()))
        }
        other => {
            return Err(invalid(
                block.to_string(),
                format!(
                    "must be a block of keys ({}), got '{other:?}'",
                    accepted.join(", ")
                ),
            ))
        }
    };
    if mapping.is_empty() {
        return Err(invalid(block.to_string(), RETAIN_EMPTY_BLOCK.to_string()));
    }

    let mut count: Option<u32> = None;
    let mut max_age_secs: Option<u64> = None;
    for (key, value) in &mapping {
        let key = key.as_str().unwrap_or_default();
        let field = format!("{block}.{key}");
        if key == count_key {
            let parsed = value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    invalid(
                        field.clone(),
                        "must be a whole number of at least 1".to_string(),
                    )
                })?;
            if parsed == 0 {
                return Err(invalid(
                    field,
                    "must be at least 1 — a limit of zero would delete what retention exists \
                     to keep; remove the key to leave it unbounded"
                        .to_string(),
                ));
            }
            count = Some(parsed);
        } else if key == "max_age" {
            let text = scalar_text(value).ok_or_else(|| {
                invalid(
                    field.clone(),
                    format!("'{value:?}' {DURATION_ACCEPTED_FORM}"),
                )
            })?;
            let parsed =
                parse_duration_secs(&text).map_err(|message| invalid(field.clone(), message))?;
            if parsed == 0 {
                return Err(invalid(
                    field,
                    format!("must be greater than zero; {DURATION_ACCEPTED_FORM}"),
                ));
            }
            max_age_secs = Some(parsed);
        } else {
            return Err(invalid(
                block.to_string(),
                format!("unknown key '{key}'; expected: {}", accepted.join(", ")),
            ));
        }
    }
    Ok((count, max_age_secs))
}

fn parse_trace_retain(
    raw: Option<serde_yaml::Value>,
) -> Result<Option<TraceRetainConfig>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (max_sessions, max_age_secs) = parse_retain_block(
        "trace.retain",
        "max_sessions",
        &TRACE_RETAIN_KEYS,
        raw,
        &|field, message| RuntimeManifestError::InvalidTraceConfig { field, message },
    )?;
    Ok(Some(TraceRetainConfig {
        max_sessions,
        max_age_secs,
    }))
}

fn parse_context_retain(
    raw: Option<serde_yaml::Value>,
) -> Result<Option<ContextRetainConfig>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let (max_messages, max_age_secs) = parse_retain_block(
        "context.retain",
        "max_messages",
        &CONTEXT_RETAIN_KEYS,
        raw,
        &|field, message| RuntimeManifestError::InvalidInferenceConfig { field, message },
    )?;
    Ok(Some(ContextRetainConfig {
        max_messages,
        max_age_secs,
    }))
}

fn parse_observability(raw: Option<RawObservabilityConfig>) -> Option<ObservabilityConfig> {
    raw.map(|raw| ObservabilityConfig {
        otel_endpoint: raw
            .otel_endpoint
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        eval: raw.eval.map(parse_eval_config),
    })
}

fn parse_eval_config(raw: RawEvalConfig) -> EvalConfig {
    let scorers = raw
        .scorers
        .into_iter()
        .filter_map(|raw| {
            let name = raw.name.unwrap_or_else(|| raw.scorer_type.clone());
            match raw.scorer_type.as_str() {
                "exit_ok" => Some(ScorerConfig::ExitOk { name }),
                "max_turns" => Some(ScorerConfig::MaxTurns {
                    name,
                    max: raw.max.unwrap_or(10) as u32,
                }),
                "max_tokens" => Some(ScorerConfig::MaxTokens {
                    name,
                    max: raw.max.unwrap_or(100_000),
                }),
                "tool_sequence" => Some(ScorerConfig::ToolSequence {
                    name,
                    expected: raw.expected.unwrap_or_default(),
                }),
                "llm_judge" => Some(ScorerConfig::LlmJudge { name }),
                other => {
                    eprintln!("[murmur-artifact] unknown scorer type '{other}' — skipping");
                    None
                }
            }
        })
        .collect();

    EvalConfig {
        dataset_id: raw.dataset_id,
        scorers,
    }
}

fn parse_compaction(
    raw: Option<RawCompactionConfig>,
) -> Result<Option<CompactionConfig>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    if let Some(threshold) = raw.threshold {
        if threshold <= 0.0 || threshold > 1.0 {
            return Err(RuntimeManifestError::InvalidInferenceConfig {
                field: "inference.compaction.threshold".to_string(),
                message: format!("must be in (0.0, 1.0] but got {threshold}"),
            });
        }
    }

    if raw.system_prompt.is_some() && raw.system_prompt_file.is_some() {
        return Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.compaction.system_prompt".to_string(),
            message: "at most one of inference.compaction.system_prompt, \
                      inference.compaction.system_prompt_file may be set"
                .to_string(),
        });
    }

    Ok(Some(CompactionConfig {
        threshold: raw.threshold,
        model: raw.model,
        system_prompt: raw.system_prompt,
        system_prompt_file: optional_trimmed_string(raw.system_prompt_file),
        // A plain optional boolean: nothing to validate, and deliberately independent of the
        // threshold-range and prompt-source checks above.
        dump_summaries: raw.dump_summaries,
    }))
}

fn optional_trimmed_string(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn required_inference_field(
    value: Option<String>,
    key: &str,
) -> Result<String, RuntimeManifestError> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RuntimeManifestError::InvalidInferenceConfig {
            field: format!("inference.{key}"),
            message: "missing required field".to_string(),
        })
}

/// Rejects `inference.endpoint` values that would let a manifest redirect the
/// inference trust root to an unencrypted, non-loopback host (murmur-security-assessment.md C-3).
///
/// Accepted: any `https://` URL, or `http://` URLs whose host is `localhost` or an
/// IP literal for which [`IpAddr::is_loopback`] returns true (e.g. `127.0.0.1`, `::1`).
/// Rejected: malformed URLs, unsupported schemes (anything but `http`/`https`), and
/// `http://` URLs whose host is not loopback.
fn validate_inference_endpoint(endpoint: &str) -> Result<(), RuntimeManifestError> {
    let url =
        Url::parse(endpoint).map_err(|source| RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.endpoint".to_string(),
            message: format!("failed to parse '{endpoint}' as a URL: {source}"),
        })?;

    match url.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = url.host_str().unwrap_or("");
            let is_loopback = host == "localhost"
                || host
                    .parse::<IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false);

            if is_loopback {
                Ok(())
            } else {
                Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.endpoint".to_string(),
                    message: format!(
                        "'{endpoint}' uses plain http:// with non-loopback host '{host}' — \
                         use https:// for remote endpoints, or http://localhost or \
                         a loopback IP literal for local inference"
                    ),
                })
            }
        }
        other => Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.endpoint".to_string(),
            message: format!(
                "unsupported scheme '{other}' in '{endpoint}' — expected http or https"
            ),
        }),
    }
}

fn parse_inference_driver(
    raw: RawInferenceDriver,
) -> Result<InferenceDriver, RuntimeManifestError> {
    let artifact = raw
        .artifact
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.driver.artifact".to_string(),
            message: "missing required field".to_string(),
        })?;

    let config = parse_inference_driver_config(raw.config)?;

    Ok(InferenceDriver { artifact, config })
}

fn parse_inference_driver_config(
    raw: Option<serde_yaml::Value>,
) -> Result<Option<String>, RuntimeManifestError> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    if matches!(raw, serde_yaml::Value::Null) {
        return Ok(None);
    }

    if !matches!(raw, serde_yaml::Value::Mapping(_)) {
        return Err(RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.driver.config".to_string(),
            message: "expected mapping/object".to_string(),
        });
    }

    let value =
        serde_json::to_value(&raw).map_err(|err| RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.driver.config".to_string(),
            message: format!("failed to convert to JSON: {err}"),
        })?;

    serde_json::to_string(&value).map(Some).map_err(|err| {
        RuntimeManifestError::InvalidInferenceConfig {
            field: "inference.driver.config".to_string(),
            message: format!("failed to encode JSON: {err}"),
        }
    })
}

fn resolve_inference_api_key(value: &str) -> Result<String, RuntimeManifestError> {
    if let Some(variable) = parse_env_reference(value) {
        return std::env::var(variable).map_err(|_| RuntimeManifestError::MissingInferenceEnvVar {
            field: "inference.api_key".to_string(),
            reference: value.to_string(),
            variable: variable.to_string(),
        });
    }

    Ok(value.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace_capture::TRACE_CAPTURE_ACCEPTED_VALUES;

    fn manifest_with_trace(block: &str) -> String {
        format!("name: cap\nversion: 0.0.1\ntrace:\n{block}")
    }

    /// No `trace:` block at all leaves the config absent; the runtime substitutes
    /// [`TraceCapture::default`] rather than the manifest carrying a synthesized one.
    #[test]
    fn absent_trace_block_stays_absent_and_defaults_to_meta() {
        let manifest = RuntimeManifest::from_yaml_str("name: cap\nversion: 0.0.1\n").unwrap();
        assert_eq!(manifest.trace, None);
        assert_eq!(TraceCapture::default(), TraceCapture::Meta);
    }

    #[test]
    fn each_capture_mode_parses_from_the_trace_block() {
        for (yaml, expected) in [
            ("none", TraceCapture::None),
            ("meta", TraceCapture::Meta),
            ("content", TraceCapture::Content),
        ] {
            let manifest = RuntimeManifest::from_yaml_str(&manifest_with_trace(&format!(
                "  capture: {yaml}\n"
            )))
            .unwrap_or_else(|e| panic!("'{yaml}' must parse: {e}"));
            assert_eq!(
                manifest.trace,
                Some(TraceConfig {
                    capture: expected,
                    retain: None
                })
            );
        }
    }

    // ── retain ───────────────────────────────────────────────────────────────

    /// A `trace:` block with only `capture` and a `context:` block with only `record` declare no
    /// retention at all: an absent `retain:` is the only way to say "keep everything", and it is
    /// what a capsule that never mentions retention gets.
    #[test]
    fn no_retain_block_anywhere_parses_as_no_policy() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ntrace:\n  capture: content\ncontext:\n  record: on\n",
        )
        .unwrap();
        assert_eq!(manifest.trace.unwrap().retain, None);
        assert_eq!(manifest.context.unwrap().retain, None);
    }

    #[test]
    fn trace_retain_parses_both_keys_and_the_day_suffix() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ntrace:\n  capture: meta\n  retain:\n    max_sessions: 50\n    max_age: 14d\n",
        )
        .unwrap();
        assert_eq!(
            manifest.trace.unwrap().retain,
            Some(TraceRetainConfig {
                max_sessions: Some(50),
                max_age_secs: Some(14 * 86_400),
            })
        );
    }

    #[test]
    fn context_retain_parses_both_keys_and_either_alone() {
        let both = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncontext:\n  record: on\n  retain:\n    max_messages: 2000\n    max_age: 90d\n",
        )
        .unwrap();
        assert_eq!(
            both.context.unwrap().retain,
            Some(ContextRetainConfig {
                max_messages: Some(2000),
                max_age_secs: Some(90 * 86_400),
            })
        );

        let count_only = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncontext:\n  retain:\n    max_messages: 10\n",
        )
        .unwrap();
        assert_eq!(
            count_only.context.unwrap().retain,
            Some(ContextRetainConfig {
                max_messages: Some(10),
                max_age_secs: None,
            })
        );
    }

    /// An empty block is not a way to declare no policy — omitting the block is. Both spellings
    /// of empty are refused, naming the block an operator has to go and change.
    #[test]
    fn an_empty_retain_block_is_refused_naming_the_block() {
        for (yaml, expect_trace) in [
            (
                "name: cap\nversion: 0.0.1\ntrace:\n  capture: meta\n  retain: {}\n",
                true,
            ),
            (
                "name: cap\nversion: 0.0.1\ntrace:\n  capture: meta\n  retain:\n",
                true,
            ),
            (
                "name: cap\nversion: 0.0.1\ncontext:\n  record: on\n  retain: {}\n",
                false,
            ),
            (
                "name: cap\nversion: 0.0.1\ncontext:\n  record: on\n  retain:\n",
                false,
            ),
        ] {
            let err = RuntimeManifest::from_yaml_str(yaml)
                .expect_err("an empty retain block must be refused");
            let (field, message) = match (&err, expect_trace) {
                (RuntimeManifestError::InvalidTraceConfig { field, message }, true) => {
                    (field, message)
                }
                (RuntimeManifestError::InvalidInferenceConfig { field, message }, false) => {
                    (field, message)
                }
                _ => panic!("wrong variant for {yaml:?}: {err:?}"),
            };
            assert_eq!(
                field,
                if expect_trace {
                    "trace.retain"
                } else {
                    "context.retain"
                }
            );
            assert!(message.contains("at least one key"), "{message}");
        }
    }

    /// A limit of zero would delete what retention exists to keep, so it is
    /// refused naming the key rather than accepted as "keep nothing".
    #[test]
    fn a_zero_retain_limit_is_refused_naming_the_key() {
        let trace = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ntrace:\n  capture: meta\n  retain:\n    max_sessions: 0\n",
        )
        .expect_err("max_sessions: 0 must be refused");
        assert!(
            matches!(&trace, RuntimeManifestError::InvalidTraceConfig { field, .. }
                     if field == "trace.retain.max_sessions"),
            "{trace:?}"
        );

        let context = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncontext:\n  retain:\n    max_messages: 0\n",
        )
        .expect_err("max_messages: 0 must be refused");
        assert!(
            matches!(&context, RuntimeManifestError::InvalidInferenceConfig { field, .. }
                     if field == "context.retain.max_messages"),
            "{context:?}"
        );

        let age = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ntrace:\n  retain:\n    max_age: 0\n",
        )
        .expect_err("max_age: 0 must be refused");
        assert!(
            matches!(&age, RuntimeManifestError::InvalidTraceConfig { field, .. }
                     if field == "trace.retain.max_age"),
            "{age:?}"
        );
    }

    #[test]
    fn an_unknown_retain_key_is_refused_listing_the_accepted_ones() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ntrace:\n  retain:\n    max_traces: 3\n",
        )
        .expect_err("an unknown key must be refused");
        match err {
            RuntimeManifestError::InvalidTraceConfig { field, message } => {
                assert_eq!(field, "trace.retain");
                assert!(message.contains("max_traces"), "{message}");
                assert!(message.contains("max_sessions"), "{message}");
            }
            other => panic!("expected InvalidTraceConfig, got {other:?}"),
        }
    }

    #[test]
    fn an_unparseable_retain_duration_reports_the_accepted_form() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncontext:\n  retain:\n    max_age: 5 weeks\n",
        )
        .expect_err("'5 weeks' must be refused");
        match err {
            RuntimeManifestError::InvalidInferenceConfig { field, message } => {
                assert_eq!(field, "context.retain.max_age");
                assert!(message.contains(DURATION_ACCEPTED_FORM), "{message}");
            }
            other => panic!("expected InvalidInferenceConfig, got {other:?}"),
        }
    }

    /// The retired boolean keeps working, mapping `true` to `Content` and `false` to `Meta`.
    #[test]
    fn include_tool_output_still_resolves_to_content_and_meta() {
        let opted_in =
            RuntimeManifest::from_yaml_str(&manifest_with_trace("  include_tool_output: true\n"))
                .unwrap();
        assert_eq!(
            opted_in.trace,
            Some(TraceConfig {
                capture: TraceCapture::Content,
                retain: None
            })
        );

        let opted_out =
            RuntimeManifest::from_yaml_str(&manifest_with_trace("  include_tool_output: false\n"))
                .unwrap();
        assert_eq!(
            opted_out.trace,
            Some(TraceConfig {
                capture: TraceCapture::Meta,
                retain: None
            })
        );
    }

    #[test]
    fn setting_both_capture_keys_is_refused_naming_both() {
        let err = RuntimeManifest::from_yaml_str(&manifest_with_trace(
            "  capture: content\n  include_tool_output: true\n",
        ))
        .expect_err("both keys must be refused even when they agree");
        assert!(matches!(
            err,
            RuntimeManifestError::InvalidTraceConfig { .. }
        ));
        let rendered = err.to_string();
        assert!(rendered.contains("trace.capture"), "{rendered}");
        assert!(rendered.contains("trace.include_tool_output"), "{rendered}");
    }

    #[test]
    fn unparseable_capture_value_names_the_field_and_accepted_values() {
        let err = RuntimeManifest::from_yaml_str(&manifest_with_trace("  capture: verbose\n"))
            .expect_err("'verbose' is not a capture mode");
        let rendered = err.to_string();
        assert!(rendered.contains("trace.capture"), "{rendered}");
        for accepted in TRACE_CAPTURE_ACCEPTED_VALUES {
            assert!(rendered.contains(accepted), "{rendered}");
        }
    }

    #[test]
    fn resources_block_is_optional_and_absent_stays_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  shell:
    allow: [bash]
"#,
        )
        .unwrap();

        // `None` here is "the manifest said nothing", not "unlimited" — the runtime substitutes
        // its own defaults (see `capsule_runtime::resources::HostResourceLimits::resolve`), and
        // keeping the two states distinct is what lets it own them in one place.
        let caps = manifest.capabilities.expect("shell block must parse");
        assert_eq!(caps.resources, None);
    }

    #[test]
    fn resources_fields_parse_and_each_stays_independently_optional() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  resources:
    max_processes: 32
    max_open_files: 16
    max_file_size_bytes: 1048576
    cpu_seconds: 30
    memory_bytes: 268435456
    cgroup_memory_bytes: 536870912
    cgroup_pids_max: 64
    cgroup_cpu_percent: 50
    cgroup_io_bytes_per_sec: 1048576
    workdir_max_bytes: 2097152
"#,
        )
        .unwrap();

        let resources = manifest
            .capabilities
            .and_then(|caps| caps.resources)
            .expect("resources block must parse");
        assert_eq!(resources.max_processes, Some(32));
        assert_eq!(resources.max_open_files, Some(16));
        assert_eq!(resources.max_file_size_bytes, Some(1_048_576));
        assert_eq!(resources.cpu_seconds, Some(30));
        assert_eq!(resources.memory_bytes, Some(268_435_456));
        assert_eq!(resources.cgroup_memory_bytes, Some(536_870_912));
        assert_eq!(resources.cgroup_pids_max, Some(64));
        assert_eq!(resources.cgroup_cpu_percent, Some(50));
        assert_eq!(resources.cgroup_io_bytes_per_sec, Some(1_048_576));
        assert_eq!(resources.workdir_max_bytes, Some(2_097_152));
    }

    #[test]
    fn state_block_is_optional_and_absent_stays_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: notes-tool
    version: 1.0.0
    runtime: tool
    capabilities:
      filesystem:
        scope: cache
"#,
        )
        .unwrap();

        // Absent is deny: no store name to default, and nothing downstream creates a directory.
        let caps = manifest.artifacts[0]
            .capabilities
            .as_ref()
            .expect("filesystem block must parse");
        assert_eq!(caps.state, None);
    }

    /// A declared-but-empty `state:` block is a real grant — it means "give me a store, named
    /// after the capsule" — so it must lower to `Some(..)` with no name, distinguishable from
    /// the absent block above.
    #[test]
    fn state_block_declared_empty_grants_the_capsule_named_store() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: notes-tool
    version: 1.0.0
    runtime: tool
    capabilities:
      state: {}
"#,
        )
        .unwrap();

        let state = manifest.artifacts[0]
            .capabilities
            .as_ref()
            .and_then(|caps| caps.state.as_ref())
            .expect("state block must parse");
        assert_eq!(state.store, None);
    }

    #[test]
    fn state_store_name_parses_and_stays_independently_optional() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: notes-tool
    version: 1.0.0
    runtime: tool
    capabilities:
      state:
        store: "  shey  "
      network:
        allow: [https://api.example.com]
"#,
        )
        .unwrap();

        let caps = manifest.artifacts[0]
            .capabilities
            .as_ref()
            .expect("capabilities must parse");
        // Trimmed, but otherwise verbatim: whether the name is a usable directory segment is
        // decided by the runtime (`E-CAP-009`), not here.
        assert_eq!(
            caps.state.as_ref().and_then(|state| state.store.as_deref()),
            Some("shey")
        );
        // Declaring a store never touches any sibling sub-block.
        assert_eq!(
            caps.network
                .as_ref()
                .map(|network| network.allow.as_slice()),
            Some(["https://api.example.com".to_string()].as_slice())
        );
        assert_eq!(caps.filesystem, None);
    }

    /// An explicitly empty name is kept rather than normalised to "default to the capsule name":
    /// the operator wrote a name, and handing them a different store than the one they wrote
    /// would be worse than the refusal the runtime raises for it.
    #[test]
    fn state_store_name_declared_empty_is_kept_for_the_runtime_to_refuse() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: notes-tool
    version: 1.0.0
    runtime: tool
    capabilities:
      state:
        store: ""
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.artifacts[0]
                .capabilities
                .as_ref()
                .and_then(|caps| caps.state.as_ref())
                .and_then(|state| state.store.as_deref()),
            Some("")
        );
    }

    #[test]
    fn resources_partial_block_leaves_undeclared_fields_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  resources:
    max_open_files: 16
"#,
        )
        .unwrap();

        let resources = manifest
            .capabilities
            .and_then(|caps| caps.resources)
            .expect("resources block must parse");
        assert_eq!(resources.max_open_files, Some(16));
        assert_eq!(resources.max_processes, None);
        assert_eq!(resources.workdir_max_bytes, None);
    }

    /// Every field rejects `0` at parse time, naming its own path. A zero ceiling is never what
    /// an author means, and accepting one would turn a typo into a subprocess that cannot fork,
    /// open a file, or run for a single CPU-second — discovered at runtime instead of at parse.
    #[test]
    fn resources_zero_is_rejected_on_every_field() {
        for field in [
            "max_processes",
            "max_open_files",
            "max_file_size_bytes",
            "cpu_seconds",
            "memory_bytes",
            "cgroup_memory_bytes",
            "cgroup_pids_max",
            "cgroup_cpu_percent",
            "cgroup_io_bytes_per_sec",
            "workdir_max_bytes",
        ] {
            let yaml =
                format!("name: cap\nversion: 0.0.1\ncapabilities:\n  resources:\n    {field}: 0\n");
            let error =
                RuntimeManifest::from_yaml_str(&yaml).expect_err("a zero {field} must not parse");

            match error {
                RuntimeManifestError::InvalidCapabilities {
                    field: reported,
                    message,
                } => {
                    assert_eq!(reported, format!("capabilities.resources.{field}"));
                    assert_eq!(message, "must be greater than zero");
                }
                other => panic!("expected InvalidCapabilities for {field}, got {other:?}"),
            }
        }
    }

    #[test]
    fn artifact_runtime_defaults_to_tool() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].runtime, ArtifactRuntime::Tool);
    }

    #[test]
    fn artifact_runtime_tool_explicit() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
    runtime: tool
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].runtime, ArtifactRuntime::Tool);
    }

    #[test]
    fn artifact_runtime_wasm_is_error() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
    runtime: wasm
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("use 'runtime: tool'"), "error was: {msg}");
        assert!(msg.contains("wasm"), "error was: {msg}");
    }

    #[test]
    fn artifact_runtime_native_is_error() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
    runtime: native
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("use 'runtime: tool'"), "error was: {msg}");
        assert!(msg.contains("native"), "error was: {msg}");
    }

    #[test]
    fn invalid_artifact_reports_index() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: ""
    version: 1.2.3
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("index 0"));
        assert!(msg.contains("missing required field 'name'"));
    }

    #[test]
    fn capabilities_parse_when_all_fields_present() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    allow:
      - https://api.anthropic.com
      - http://127.0.0.1:8080
  filesystem:
    scope: ./sandbox
"#,
        )
        .unwrap();

        let capabilities = manifest.capabilities.expect("capabilities should exist");
        assert_eq!(
            capabilities.network.unwrap().allow,
            vec![
                "https://api.anthropic.com".to_string(),
                "http://127.0.0.1:8080".to_string()
            ]
        );
        assert_eq!(
            capabilities.filesystem.unwrap().scope,
            Some("./sandbox".to_string())
        );
    }

    #[test]
    fn env_capabilities_parse_allowlist() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  env:
    allow:
      - MY_APP_REGION
      - GITHUB_TOKEN
"#,
        )
        .unwrap();

        let capabilities = manifest.capabilities.expect("capabilities should exist");
        assert_eq!(
            capabilities.env.unwrap().allow,
            vec!["MY_APP_REGION".to_string(), "GITHUB_TOKEN".to_string()]
        );
    }

    #[test]
    fn env_capabilities_absent_when_block_omitted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
        )
        .unwrap();

        assert!(manifest.capabilities.unwrap().env.is_none());
    }

    /// Unlike `capabilities.shell.allow`, an empty `env.allow` is a legitimate no-op rather
    /// than a manifest error.
    #[test]
    fn env_capabilities_allow_empty_list_is_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  env: {}
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.capabilities.unwrap().env,
            Some(EnvCapabilities { allow: vec![] })
        );
    }

    #[test]
    fn missing_capabilities_block_is_backward_compatible() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
"#,
        )
        .unwrap();

        assert!(manifest.capabilities.is_none());
    }

    #[test]
    fn network_only_capabilities_parse() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    allow:
      - https://allowed.example.com
"#,
        )
        .unwrap();

        let capabilities = manifest.capabilities.unwrap();
        assert_eq!(
            capabilities.network.unwrap().allow,
            vec!["https://allowed.example.com".to_string()]
        );
        assert!(capabilities.filesystem.is_none());
    }

    #[test]
    fn network_allow_defaults_to_empty_when_present_without_allow_entries() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network: {}
"#,
        )
        .unwrap();

        let capabilities = manifest.capabilities.unwrap();
        assert_eq!(capabilities.network.unwrap().allow, Vec::<String>::new());
    }

    /// `unix_sockets` is optional and denies by default. A capsule that never heard of the key
    /// must not silently get `AF_UNIX` sockets — that is the whole `/var/run/docker.sock` escape.
    #[test]
    fn network_unix_sockets_defaults_to_false_when_key_is_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
        )
        .unwrap();

        let network = manifest.capabilities.unwrap().network.unwrap();
        assert_eq!(network.allow, vec!["https://api.anthropic.com".to_string()]);
        assert!(!network.unix_sockets);
    }

    #[test]
    fn network_unix_sockets_parses_explicit_false() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    unix_sockets: false
"#,
        )
        .unwrap();

        assert!(!manifest.capabilities.unwrap().network.unwrap().unix_sockets);
    }

    #[test]
    fn network_unix_sockets_parses_explicit_true() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    unix_sockets: true
"#,
        )
        .unwrap();

        let network = manifest.capabilities.unwrap().network.unwrap();
        assert!(network.unix_sockets);
        // The opt-in is orthogonal to the IP allowlist — declaring it does not imply any host.
        assert_eq!(network.allow, Vec::<String>::new());
    }

    /// `read_only` is normalised on exactly the terms `scope` is — trimmed, with an entry left
    /// empty by that trim dropped rather than carried as a rule covering the whole workdir — and
    /// path shape is deliberately not judged here.
    #[test]
    fn filesystem_read_only_is_trimmed_and_empty_entries_are_dropped() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  filesystem:
    read_only:
      - tests
      - "  bench/fixtures  "
      - ""
      - "   "
"#,
        )
        .unwrap();

        let filesystem = manifest.capabilities.unwrap().filesystem.unwrap();
        assert_eq!(filesystem.read_only, vec!["tests", "bench/fixtures"]);
    }

    /// An absent key and an empty list are one declaration: nothing is protected.
    #[test]
    fn filesystem_read_only_defaults_to_empty_when_the_key_is_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  filesystem:
    scope: workdir
"#,
        )
        .unwrap();

        assert!(manifest
            .capabilities
            .unwrap()
            .filesystem
            .unwrap()
            .read_only
            .is_empty());
    }

    /// Path shape is the runtime's refusal, not the parser's: an absolute or escaping entry parses
    /// here exactly as an absolute `scope` does, and fails at launch with `E-CAP-012`.
    #[test]
    fn filesystem_read_only_does_not_judge_path_shape() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  filesystem:
    read_only:
      - /etc
      - ../outside
"#,
        )
        .expect("shape is judged by the runtime, not the parser");

        assert_eq!(
            manifest.capabilities.unwrap().filesystem.unwrap().read_only,
            vec!["/etc", "../outside"]
        );
    }

    /// The default every pre-existing manifest gets: a `filesystem:` block that has never heard of
    /// `workdir_exec` must not silently keep the workdir's `Execute` right, because that right is
    /// exactly what makes `capabilities.shell.allow` bypassable from inside the workdir.
    #[test]
    fn filesystem_workdir_exec_defaults_to_false_when_key_is_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  filesystem:
    scope: workdir
"#,
        )
        .unwrap();

        let filesystem = manifest.capabilities.unwrap().filesystem.unwrap();
        assert_eq!(filesystem.scope.as_deref(), Some("workdir"));
        assert!(!filesystem.workdir_exec);
    }

    /// The other absent case: no `filesystem:` block at all. There is nothing to default here —
    /// the whole sub-block stays `None` — but a capsule with no filesystem declaration must still
    /// read as "workdir exec denied" downstream, which `capability_policy_from_runtime_manifest`
    /// covers with its own test.
    #[test]
    fn filesystem_block_stays_absent_when_undeclared() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
        )
        .unwrap();

        assert!(manifest.capabilities.unwrap().filesystem.is_none());
    }

    #[test]
    fn filesystem_workdir_exec_parses_explicit_false() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  filesystem:
    workdir_exec: false
"#,
        )
        .unwrap();

        assert!(
            !manifest
                .capabilities
                .unwrap()
                .filesystem
                .unwrap()
                .workdir_exec
        );
    }

    #[test]
    fn filesystem_workdir_exec_parses_explicit_true() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: echo
    version: 1.2.3
capabilities:
  filesystem:
    workdir_exec: true
"#,
        )
        .unwrap();

        let filesystem = manifest.capabilities.unwrap().filesystem.unwrap();
        assert!(filesystem.workdir_exec);
        // Orthogonal to `scope`, exactly as `unix_sockets` is orthogonal to `network.allow`:
        // declaring one says nothing about the other.
        assert_eq!(filesystem.scope, None);
    }

    #[test]
    fn network_internal_port_parses() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: worker
version: 0.1.0
artifacts: []

network:
  internal_port: 58172
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.network.unwrap().internal_port,
            Some(58172),
            "network.internal_port should parse to Some(58172)"
        );
    }

    #[test]
    fn network_internal_port_absent_when_omitted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: worker
version: 0.1.0
artifacts: []
"#,
        )
        .unwrap();

        assert!(manifest.network.is_none());
    }

    #[test]
    fn shell_capabilities_parse_with_allow_and_env_fields() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - bash
      - git
    strip_env:
      - "*_TOKEN"
      - SECRET_KEY
    baseline_env:
      - SSH_AUTH_SOCK
"#,
        )
        .unwrap();

        let shell = manifest
            .capabilities
            .unwrap()
            .shell
            .expect("shell capabilities should be present");
        assert_eq!(shell.allow, vec!["bash".to_string(), "git".to_string()]);
        assert_eq!(
            shell.strip_env,
            Some(vec!["*_TOKEN".to_string(), "SECRET_KEY".to_string()])
        );
        assert_eq!(shell.baseline_env, Some(vec!["SSH_AUTH_SOCK".to_string()]));
    }

    #[test]
    fn shell_capabilities_require_non_empty_allowlist() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow: []
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("capabilities.shell.allow"));
        assert!(msg.contains("at least one"));
    }

    #[test]
    fn interpreter_runtime_parses_accepted_shape() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - python3
    interpreter_runtime:
      - binary: python3
        dirs:
          - path: /usr/lib/python3.11
            list_dir: true
          - path: /usr/lib/python3.11/lib-dynload
            list_dir: false
"#,
        )
        .unwrap();

        let shell = manifest.capabilities.unwrap().shell.unwrap();
        assert_eq!(
            shell.interpreter_runtime,
            vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![
                    InterpreterRuntimeDir {
                        path: "/usr/lib/python3.11".to_string(),
                        list_dir: true,
                    },
                    InterpreterRuntimeDir {
                        path: "/usr/lib/python3.11/lib-dynload".to_string(),
                        list_dir: false,
                    },
                ],
            }]
        );
    }

    #[test]
    fn interpreter_runtime_defaults_to_empty_when_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - python3
"#,
        )
        .unwrap();

        let shell = manifest.capabilities.unwrap().shell.unwrap();
        assert!(shell.interpreter_runtime.is_empty());
    }

    /// Shared helper: parse a manifest whose sole `interpreter_runtime` grant is `grant_yaml`
    /// (indented to sit under `interpreter_runtime:`), returning the error message.
    fn interpreter_runtime_reject(grant_yaml: &str) -> String {
        let yaml = format!(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - python3
    interpreter_runtime:
{grant_yaml}
"#,
        );
        RuntimeManifest::from_yaml_str(&yaml)
            .expect_err("grant should be rejected")
            .to_string()
    }

    #[test]
    fn interpreter_runtime_rejects_binary_not_in_allow() {
        let msg = interpreter_runtime_reject(
            "      - binary: ruby\n        dirs:\n          - path: /usr/lib/ruby\n            list_dir: true\n",
        );
        assert!(
            msg.contains("capabilities.shell.interpreter_runtime[0].binary"),
            "{msg}"
        );
        assert!(msg.contains("ruby"), "{msg}");
        assert!(msg.contains("capabilities.shell.allow"), "{msg}");
    }

    #[test]
    fn interpreter_runtime_rejects_relative_path() {
        let msg = interpreter_runtime_reject(
            "      - binary: python3\n        dirs:\n          - path: usr/lib/python3.11\n            list_dir: true\n",
        );
        assert!(
            msg.contains("capabilities.shell.interpreter_runtime[0].dirs[0].path"),
            "{msg}"
        );
        assert!(msg.contains("absolute"), "{msg}");
    }

    // ── capabilities.task_io (per-hook, honored only on runtime: hook) ────────

    /// A capsule manifest with one artifact entry carrying `capabilities: task_io: <block>`.
    fn manifest_with_task_io(runtime: &str, block: &str) -> String {
        format!(
            "name: cap\nversion: 0.0.1\nartifacts:\n  - name: gate\n    version: 1.2.3\n    \
             runtime: {runtime}\n    capabilities:\n      task_io:\n{block}"
        )
    }

    #[test]
    fn task_io_read_true_grants_the_hook() {
        let manifest =
            RuntimeManifest::from_yaml_str(&manifest_with_task_io("hook", "        read: true\n"))
                .expect("task_io on a hook entry parses");
        assert_eq!(
            manifest.artifacts[0].capabilities.as_ref().unwrap().task_io,
            Some(TaskIoCapabilities { read: true })
        );
    }

    /// `read: false` and an absent `task_io:` block both leave the hook ungranted; the
    /// difference between them is only whether the operator wrote the denial down.
    #[test]
    fn task_io_read_false_and_an_absent_block_both_leave_the_hook_ungranted() {
        let explicit =
            RuntimeManifest::from_yaml_str(&manifest_with_task_io("hook", "        read: false\n"))
                .expect("an explicit denial parses");
        assert_eq!(
            explicit.artifacts[0].capabilities.as_ref().unwrap().task_io,
            Some(TaskIoCapabilities { read: false })
        );

        let absent = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts:\n  - name: gate\n    version: 1.2.3\n    \
             runtime: hook\n",
        )
        .expect("no capabilities block parses");
        assert!(absent.artifacts[0].capabilities.is_none());
    }

    /// The capability is never inferred: a `task_io:` block that omits `read:` is a parse
    /// error naming the key, following `interpreter_runtime[].dirs[].list_dir`.
    #[test]
    fn task_io_without_read_is_rejected() {
        let err = RuntimeManifest::from_yaml_str(&manifest_with_task_io("hook", "        {}\n"))
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("capabilities.task_io.read"),
            "error was: {msg}"
        );
        assert!(msg.contains("explicitly"), "error was: {msg}");
    }

    /// Nothing outside a hook can be handed the import, so declaring the key there is a parse
    /// error rather than a silently inert grant.
    #[test]
    fn task_io_outside_a_hook_entry_is_rejected() {
        for runtime in ["tool", "driver", "skill"] {
            let err = RuntimeManifest::from_yaml_str(&manifest_with_task_io(
                runtime,
                "        read: true\n",
            ))
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("capabilities") && msg.contains("runtime: hook"),
                "{runtime} entry: error was: {msg}"
            );
        }
    }

    /// The capsule-wide block reaches capsule, tool and driver components, none of which can
    /// receive the import — so it is rejected there too, naming the key.
    #[test]
    fn task_io_in_the_capsule_wide_block_is_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncapabilities:\n  task_io:\n    read: true\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("capabilities.task_io"), "error was: {msg}");
        assert!(msg.contains("runtime: hook"), "error was: {msg}");
    }

    // ── capabilities.conversation (per-hook, honored only on runtime: hook) ───

    /// A capsule manifest with one artifact entry carrying `capabilities: conversation: <block>`.
    fn manifest_with_conversation(runtime: &str, block: &str) -> String {
        format!(
            "name: cap\nversion: 0.0.1\nartifacts:\n  - name: memory\n    version: 1.2.3\n    \
             runtime: {runtime}\n    capabilities:\n      conversation:\n{block}"
        )
    }

    #[test]
    fn conversation_read_lowers_onto_the_hook_entry() {
        for (declared, expected) in [("true", true), ("false", false)] {
            let manifest = RuntimeManifest::from_yaml_str(&manifest_with_conversation(
                "hook",
                &format!("        read: {declared}\n"),
            ))
            .expect("conversation on a hook entry parses");
            assert_eq!(
                manifest.artifacts[0]
                    .capabilities
                    .as_ref()
                    .unwrap()
                    .conversation,
                Some(ConversationCapabilities { read: expected })
            );
        }
    }

    /// The capability is never inferred: a block that omits `read:` is a parse error naming the
    /// key, exactly as `capabilities.task_io.read` is.
    #[test]
    fn conversation_without_read_is_rejected() {
        let err =
            RuntimeManifest::from_yaml_str(&manifest_with_conversation("hook", "        {}\n"))
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("capabilities.conversation.read"), "was: {msg}");
        assert!(msg.contains("explicitly"), "was: {msg}");
    }

    /// Only a hook's world can import the interface, so declaring the grant on any other role is
    /// a parse error rather than a silently inert block.
    #[test]
    fn conversation_outside_a_hook_entry_is_rejected() {
        for runtime in ["tool", "driver", "skill"] {
            let err = RuntimeManifest::from_yaml_str(&manifest_with_conversation(
                runtime,
                "        read: true\n",
            ))
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("capabilities") && msg.contains("runtime: hook"),
                "{runtime} entry: error was: {msg}"
            );
        }
    }

    /// Unlike `task_io`, the capsule-wide block parses: it is inert, and the runtime says so once
    /// as `W-SEC-016` rather than refusing a manifest an operator can fix at leisure.
    #[test]
    fn conversation_in_the_capsule_wide_block_parses_and_is_inert() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncapabilities:\n  conversation:\n    read: true\n",
        )
        .expect("a capsule-wide block is accepted");
        assert_eq!(
            manifest.capabilities.unwrap().conversation,
            Some(ConversationCapabilities { read: true })
        );
    }

    // ── context.record / context.record_store ────────────────────────────────

    /// The record is on unless the manifest says otherwise, and `record_store` defaults to
    /// nothing — the runtime substitutes the capsule name.
    #[test]
    fn a_context_block_records_by_default() {
        let context = context_of("context:\n  max_tokens: 1000\n").unwrap();
        assert!(context.record);
        assert_eq!(context.record_store, None);
    }

    #[test]
    fn record_off_and_on_are_the_two_accepted_values() {
        assert!(!context_of("context:\n  record: off\n").unwrap().record);
        assert!(context_of("context:\n  record: on\n").unwrap().record);

        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\ncontext:\n  record: sometimes\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("context.record"), "was: {msg}");
        assert!(msg.contains("sometimes"), "was: {msg}");
    }

    /// `record_store` is a name the runtime turns into a directory, so it is kept verbatim apart
    /// from surrounding whitespace; its *shape* is refused at launch, where the path is built.
    #[test]
    fn record_store_is_kept_verbatim_and_trimmed() {
        assert_eq!(
            context_of("context:\n  record_store: '  shey  '\n")
                .unwrap()
                .record_store,
            Some("shey".to_string())
        );
        assert_eq!(
            context_of("context:\n  record: off\n  record_store: shey\n")
                .unwrap()
                .record_store,
            Some("shey".to_string()),
            "a store declared beside record: off is kept and simply never used"
        );
    }

    #[test]
    fn interpreter_runtime_rejects_missing_list_dir() {
        let msg = interpreter_runtime_reject(
            "      - binary: python3\n        dirs:\n          - path: /usr/lib/python3.11\n",
        );
        assert!(
            msg.contains("capabilities.shell.interpreter_runtime[0].dirs[0].list_dir"),
            "{msg}"
        );
        assert!(msg.contains("explicitly"), "{msg}");
    }

    #[test]
    fn interpreter_runtime_rejects_empty_dirs() {
        let msg = interpreter_runtime_reject("      - binary: python3\n        dirs: []\n");
        assert!(
            msg.contains("capabilities.shell.interpreter_runtime[0].dirs"),
            "{msg}"
        );
        assert!(msg.contains("at least one"), "{msg}");
    }

    #[test]
    fn staged_runtime_parses_accepted_shape() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  containment: sealed
  shell:
    allow:
      - python3
      - bash
    staged_runtime:
      - binary: python3
        source_path: /opt/testbed/conda/envs/django__django
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01
"#,
        )
        .unwrap();

        let shell = manifest.capabilities.unwrap().shell.unwrap();
        assert_eq!(
            shell.staged_runtime,
            vec![StagedRuntimeGrant {
                binary: "python3".to_string(),
                source_path: "/opt/testbed/conda/envs/django__django".to_string(),
                pin: "conda-4.10.3/python-3.9.19/testbed-2024-05-01".to_string(),
            }]
        );
        // The two mechanisms are alternatives, so an accepted `staged_runtime` grant leaves the
        // interpreter_runtime list untouched rather than populating it as a side effect.
        assert!(shell.interpreter_runtime.is_empty());
    }

    #[test]
    fn staged_runtime_defaults_to_empty_when_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - python3
"#,
        )
        .unwrap();

        let shell = manifest.capabilities.unwrap().shell.unwrap();
        assert!(shell.staged_runtime.is_empty());
    }

    /// Shared helper: parse a manifest whose sole `staged_runtime` grant is `grant_yaml` (indented
    /// to sit under `staged_runtime:`), returning the error message. `extra` is spliced into the
    /// same `shell` block, which is how the mutual-exclusion case adds an `interpreter_runtime`.
    fn staged_runtime_reject(grant_yaml: &str, extra: &str) -> String {
        let yaml = format!(
            r#"
name: cap
version: 0.0.1
artifacts: []
capabilities:
  shell:
    allow:
      - python3
{extra}    staged_runtime:
{grant_yaml}
"#,
        );
        RuntimeManifest::from_yaml_str(&yaml)
            .expect_err("grant should be rejected")
            .to_string()
    }

    #[test]
    fn staged_runtime_rejects_binary_not_in_allow() {
        let msg = staged_runtime_reject(
            "      - binary: ruby\n        source_path: /opt/ruby\n        pin: ruby-3.2.2\n",
            "",
        );
        assert!(
            msg.contains("capabilities.shell.staged_runtime[0].binary"),
            "{msg}"
        );
        assert!(msg.contains("ruby"), "{msg}");
        assert!(msg.contains("capabilities.shell.allow"), "{msg}");
    }

    #[test]
    fn staged_runtime_rejects_binary_also_in_interpreter_runtime() {
        let msg = staged_runtime_reject(
            "      - binary: python3\n        source_path: /opt/py\n        pin: cpython-3.9.19\n",
            "    interpreter_runtime:\n      - binary: python3\n        dirs:\n          - path: /usr/lib/python3.9\n            list_dir: true\n",
        );
        assert!(
            msg.contains("capabilities.shell.staged_runtime[0].binary"),
            "{msg}"
        );
        assert!(msg.contains("python3"), "{msg}");
        assert!(msg.contains("interpreter_runtime"), "{msg}");
        assert!(msg.contains("mutually exclusive"), "{msg}");
    }

    #[test]
    fn staged_runtime_rejects_relative_source_path() {
        let msg = staged_runtime_reject(
            "      - binary: python3\n        source_path: opt/testbed/conda\n        pin: cpython-3.9.19\n",
            "",
        );
        assert!(
            msg.contains("capabilities.shell.staged_runtime[0].source_path"),
            "{msg}"
        );
        assert!(msg.contains("absolute"), "{msg}");
    }

    #[test]
    fn staged_runtime_rejects_missing_or_empty_pin() {
        // Omitted entirely, and present-but-blank: both are "unpinned", and the field exists
        // precisely so that state is unrepresentable.
        for grant in [
            "      - binary: python3\n        source_path: /opt/testbed/conda\n",
            "      - binary: python3\n        source_path: /opt/testbed/conda\n        pin: \"   \"\n",
        ] {
            let msg = staged_runtime_reject(grant, "");
            assert!(
                msg.contains("capabilities.shell.staged_runtime[0].pin"),
                "{msg}"
            );
            assert!(msg.contains("never inferred"), "{msg}");
        }
    }

    #[test]
    fn parses_inference_block() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://127.0.0.1:8080
  model: test-model
  api_key: literal-key
  driver:
    artifact: murmur-driver-anthropic
    config:
      temperature: 0.2
      headers:
        x-test: true
"#,
        )
        .unwrap();

        let inference = manifest.inference.expect("inference should exist");
        assert_eq!(inference.transport, "http");
        assert_eq!(
            inference.endpoint,
            Some("http://127.0.0.1:8080".to_string())
        );
        assert_eq!(inference.model, "test-model");
        assert_eq!(inference.api_key, Some("literal-key".to_string()));
        let driver = inference.driver.as_ref().expect("driver should be present");
        assert_eq!(driver.artifact, "murmur-driver-anthropic");
        assert_eq!(
            driver.config,
            Some("{\"headers\":{\"x-test\":true},\"temperature\":0.2}".to_string())
        );
        assert!(inference.system_prompt.is_none());
        assert!(inference.system_prompt_file.is_none());
    }

    #[test]
    fn https_endpoint_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.inference.unwrap().endpoint,
            Some("https://api.anthropic.com".to_string())
        );
    }

    #[test]
    fn http_localhost_endpoint_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://localhost:11434
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.inference.unwrap().endpoint,
            Some("http://localhost:11434".to_string())
        );
    }

    #[test]
    fn http_loopback_ip_endpoint_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://127.0.0.1:11434
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.inference.unwrap().endpoint,
            Some("http://127.0.0.1:11434".to_string())
        );
    }

    #[test]
    fn http_non_localhost_endpoint_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://api.attacker.example.com
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.endpoint"), "error was: {msg}");
        assert!(msg.contains("api.attacker.example.com"), "error was: {msg}");
    }

    #[test]
    fn schemeless_endpoint_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: api.anthropic.com
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.endpoint"), "error was: {msg}");
        assert!(msg.contains("failed to parse"), "error was: {msg}");
    }

    #[test]
    fn malformed_endpoint_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: "not a url"
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.endpoint"), "error was: {msg}");
        assert!(msg.contains("failed to parse"), "error was: {msg}");
    }

    #[test]
    fn unsupported_scheme_endpoint_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: ftp://example.com
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.endpoint"), "error was: {msg}");
        assert!(msg.contains("unsupported scheme 'ftp'"), "error was: {msg}");
    }

    #[test]
    fn parses_inline_system_prompt() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt: "Always begin with CONFIRMED:"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let inference = manifest.inference.unwrap();
        assert_eq!(
            inference.system_prompt,
            Some("Always begin with CONFIRMED:".to_string())
        );
        assert!(inference.system_prompt_file.is_none());
    }

    #[test]
    fn parses_compaction_system_prompt() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    model: compaction-model
    system_prompt: "task = X, currently editing Y, already tried Z."
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let compaction = manifest.inference.unwrap().compaction.unwrap();
        assert_eq!(
            compaction.system_prompt,
            Some("task = X, currently editing Y, already tried Z.".to_string())
        );
        assert_eq!(compaction.model, Some("compaction-model".to_string()));
    }

    /// The two override fields resolve independently: `model` set, `system_prompt`
    /// absent leaves the latter `None` with no default string substituted.
    #[test]
    fn compaction_system_prompt_absent_stays_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    model: compaction-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let compaction = manifest.inference.unwrap().compaction.unwrap();
        assert!(compaction.system_prompt.is_none());
        assert!(compaction.system_prompt_file.is_none());
        assert_eq!(compaction.model, Some("compaction-model".to_string()));
    }

    /// `system_prompt_file` parses as the second, independent prompt source: the path is
    /// carried through verbatim (resolution against the manifest dir happens in the
    /// runtime), the inline field stays `None`, and `model` is unaffected.
    #[test]
    fn parses_compaction_system_prompt_file() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    model: compaction-model
    system_prompt_file: "compaction-instructions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let compaction = manifest.inference.unwrap().compaction.unwrap();
        assert_eq!(
            compaction.system_prompt_file,
            Some("compaction-instructions.md".to_string())
        );
        assert!(compaction.system_prompt.is_none());
        assert_eq!(compaction.model, Some("compaction-model".to_string()));
    }

    /// `system_prompt_file` alone, with no `model`, still parses — the two fields do not
    /// depend on each other in either direction.
    #[test]
    fn compaction_system_prompt_file_independent_of_model() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    threshold: 0.5
    system_prompt_file: "compaction-instructions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let compaction = manifest.inference.unwrap().compaction.unwrap();
        assert_eq!(
            compaction.system_prompt_file,
            Some("compaction-instructions.md".to_string())
        );
        assert!(compaction.model.is_none());
        assert_eq!(compaction.threshold, Some(0.5));
    }

    /// `dump_summaries` is a plain optional boolean: present-true, present-false and absent
    /// are three distinguishable states, so the runtime can tell "explicitly off" from
    /// "never configured" even though both resolve to the same behavior.
    #[test]
    fn parses_compaction_dump_summaries() {
        let parse = |line: &str| {
            RuntimeManifest::from_yaml_str(&format!(
                r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    model: compaction-model
{line}  driver:
    artifact: murmur-driver-anthropic
"#
            ))
            .unwrap()
            .inference
            .unwrap()
            .compaction
            .unwrap()
        };

        assert_eq!(
            parse("    dump_summaries: true\n").dump_summaries,
            Some(true)
        );
        assert_eq!(
            parse("    dump_summaries: false\n").dump_summaries,
            Some(false)
        );
        assert_eq!(parse("").dump_summaries, None);
    }

    /// `dump_summaries` is orthogonal to every other compaction field: it neither trips the
    /// prompt-source exclusivity check nor the threshold range check, and does not disturb
    /// the values those fields parse to.
    #[test]
    fn compaction_dump_summaries_does_not_interact_with_other_fields() {
        let compaction = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    threshold: 0.5
    model: compaction-model
    system_prompt_file: "compaction-instructions.md"
    dump_summaries: true
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap()
        .inference
        .unwrap()
        .compaction
        .unwrap();

        assert_eq!(compaction.dump_summaries, Some(true));
        assert_eq!(compaction.threshold, Some(0.5));
        assert_eq!(compaction.model, Some("compaction-model".to_string()));
        assert_eq!(
            compaction.system_prompt_file,
            Some("compaction-instructions.md".to_string())
        );
        assert!(compaction.system_prompt.is_none());

        // Still mutually exclusive with dump_summaries in the mix...
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    dump_summaries: true
    system_prompt: "inline"
    system_prompt_file: "compaction-instructions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .expect_err("dump_summaries must not suppress the prompt-source exclusivity check");
        assert!(matches!(
            err,
            RuntimeManifestError::InvalidInferenceConfig { ref field, .. }
                if field == "inference.compaction.system_prompt"
        ));

        // ...and the threshold range is still enforced.
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    dump_summaries: true
    threshold: 1.5
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .expect_err("dump_summaries must not suppress the threshold range check");
        assert!(matches!(
            err,
            RuntimeManifestError::InvalidInferenceConfig { ref field, .. }
                if field == "inference.compaction.threshold"
        ));
    }

    /// Setting both compaction prompt sources is a parse error naming the compaction
    /// fields — distinguishable from the top-level three-way `inference.system_prompt`
    /// exclusivity error, and with neither value silently preferred.
    #[test]
    fn compaction_prompt_sources_are_mutually_exclusive() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  compaction:
    system_prompt: "inline"
    system_prompt_file: "compaction-instructions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .expect_err("both compaction prompt sources set must fail");

        match err {
            RuntimeManifestError::InvalidInferenceConfig { field, message } => {
                assert_eq!(field, "inference.compaction.system_prompt");
                assert_eq!(
                    message,
                    "at most one of inference.compaction.system_prompt, \
                     inference.compaction.system_prompt_file may be set"
                );
            }
            other => panic!("expected InvalidInferenceConfig, got {other:?}"),
        }
    }

    /// The compaction-level exclusivity check is separate from the top-level one: an
    /// inline top-level `system_prompt` alongside a compaction `system_prompt_file` is a
    /// perfectly legal manifest.
    #[test]
    fn top_level_and_compaction_prompt_sources_do_not_collide() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt: "be helpful"
  compaction:
    system_prompt_file: "compaction-instructions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let inference = manifest.inference.unwrap();
        assert_eq!(inference.system_prompt, Some("be helpful".to_string()));
        assert_eq!(
            inference.compaction.unwrap().system_prompt_file,
            Some("compaction-instructions.md".to_string())
        );
    }

    #[test]
    fn parses_system_prompt_file() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_file: "conventions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let inference = manifest.inference.unwrap();
        assert_eq!(
            inference.system_prompt_file,
            Some("conventions.md".to_string())
        );
        assert!(inference.system_prompt.is_none());
    }

    #[test]
    fn rejects_both_inline_and_file_system_prompt() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt: "inline"
  system_prompt_file: "conventions.md"
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.system_prompt"));
        assert!(msg.contains("system_prompt_file"));
    }

    #[test]
    fn resolves_api_key_env_reference() {
        let key = "MURMUR_TEST_INFERENCE_KEY_RESOLVE";
        std::env::set_var(key, "resolved-secret");

        let manifest = RuntimeManifest::from_yaml_str(&format!(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: http\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  api_key: ${{{key}}}\n  driver:\n    artifact: murmur-driver-anthropic\n"
        ))
        .unwrap();

        assert_eq!(
            manifest.inference.unwrap().api_key,
            Some("resolved-secret".to_string())
        );

        std::env::remove_var(key);
    }

    #[test]
    fn missing_api_key_env_reference_reports_clear_error() {
        let key = "MURMUR_TEST_INFERENCE_KEY_MISSING";
        std::env::remove_var(key);

        let err = RuntimeManifest::from_yaml_str(&format!(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: http\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  api_key: ${{{key}}}\n  driver:\n    artifact: murmur-driver-anthropic\n"
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.api_key"));
        assert!(msg.contains(&format!("${{{key}}}")));
        assert!(msg.contains("environment variable is not set"));
    }

    #[test]
    fn accepts_legacy_inference_provider_field() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://127.0.0.1:8080
  model: test-model
  provider:
    artifact: murmur-driver-openai
"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .inference
                .unwrap()
                .driver
                .as_ref()
                .unwrap()
                .artifact,
            "murmur-driver-openai"
        );
    }

    #[test]
    fn prefers_driver_field_over_legacy_provider_field() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://127.0.0.1:8080
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
  provider:
    artifact: murmur-driver-openai
"#,
        )
        .unwrap();

        assert_eq!(
            manifest
                .inference
                .unwrap()
                .driver
                .as_ref()
                .unwrap()
                .artifact,
            "murmur-driver-anthropic"
        );
    }

    #[test]
    fn inference_requires_driver_artifact() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: http://127.0.0.1:8080
  model: test-model
"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("inference.driver"));
    }

    #[test]
    fn test_runtime_driver_deserializes() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-provider
    version: 1.0.0
    runtime: driver
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].runtime, ArtifactRuntime::Driver);
    }

    #[test]
    fn test_runtime_unknown_is_error() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: foo
    version: 1.0.0
    runtime: unknown_type
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("runtime"), "error should mention 'runtime'");
        assert!(
            msg.contains("unknown_type"),
            "error should identify the bad value"
        );
    }

    #[test]
    fn test_runtime_all_variants_roundtrip() {
        for variant in [
            ArtifactRuntime::Tool,
            ArtifactRuntime::Driver,
            ArtifactRuntime::Hook,
            ArtifactRuntime::Skill,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ArtifactRuntime = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn skill_source_without_version_defaults_to_local() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    source: ./skills/my-skill/skill.md
    runtime: skill
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts.len(), 1);
        let artifact = &manifest.artifacts[0];
        assert_eq!(artifact.runtime, ArtifactRuntime::Skill);
        assert_eq!(
            artifact.source.as_deref(),
            Some("./skills/my-skill/skill.md")
        );
        assert_eq!(artifact.version, "local");
    }

    #[test]
    fn skill_source_directory_path_is_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    source: ./skills/my-skill/
    runtime: skill
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.artifacts[0].source.as_deref(),
            Some("./skills/my-skill/")
        );
        assert_eq!(manifest.artifacts[0].version, "local");
    }

    #[test]
    fn skill_source_with_explicit_version_substitutes_local() {
        // Both set is allowed; version is ignored and "local" is substituted.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    source: ./skills/my-skill/skill.md
    version: 9.9.9
    runtime: skill
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].version, "local");
    }

    #[test]
    fn local_source_true_allows_source_on_non_skill_runtime() {
        for runtime in ["tool", "driver", "hook"] {
            let yaml = format!(
                r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-thing
    source: ./local/path
    local_source: true
    runtime: {runtime}
"#
            );
            let manifest = RuntimeManifest::from_yaml_str(&yaml)
                .unwrap_or_else(|err| panic!("runtime {runtime}: {err}"));
            let artifact = &manifest.artifacts[0];
            assert!(artifact.local_source, "runtime {runtime}");
            assert_eq!(artifact.source.as_deref(), Some("./local/path"));
            assert_eq!(artifact.version, "local", "runtime {runtime}");
        }
    }

    #[test]
    fn local_source_false_on_skill_with_source_is_error() {
        // The gate reads the declared field, not the role: an explicit `local_source: false`
        // overrides the skill default and rejects `source:`.
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    source: ./skills/my-skill
    local_source: false
    runtime: skill
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'local_source: true'"), "error was: {msg}");
        assert!(msg.contains("my-skill"), "error was: {msg}");
    }

    #[test]
    fn local_source_defaults_from_role_when_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    source: ./skills/my-skill
    runtime: skill
  - name: my-tool
    version: 1.0.0
    runtime: tool
"#,
        )
        .unwrap();
        assert!(manifest.artifacts[0].local_source, "skill defaults to true");
        assert!(
            !manifest.artifacts[1].local_source,
            "tool defaults to false"
        );
    }

    #[test]
    fn prompt_payload_true_allows_non_skill_system_prompt_artifact() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-tool
    version: 1.0.0
    runtime: tool
    prompt_payload: true
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_artifact: my-tool
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();
        assert!(manifest.artifacts[0].prompt_payload);
    }

    #[test]
    fn prompt_payload_false_on_skill_system_prompt_artifact_is_error() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    version: 1.0.0
    runtime: skill
    prompt_payload: false
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_artifact: my-skill
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'prompt_payload: true'"), "error was: {msg}");
    }

    #[test]
    fn prompt_payload_defaults_from_role_when_absent() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    version: 1.0.0
    runtime: skill
  - name: my-tool
    version: 1.0.0
    runtime: tool
"#,
        )
        .unwrap();
        assert!(
            manifest.artifacts[0].prompt_payload,
            "skill defaults to true"
        );
        assert!(
            !manifest.artifacts[1].prompt_payload,
            "tool defaults to false"
        );
    }

    #[test]
    fn source_on_non_skill_runtime_is_error() {
        for runtime in ["tool", "driver", "hook"] {
            let yaml = format!(
                r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-thing
    source: ./local/path
    runtime: {runtime}
"#
            );
            let err = RuntimeManifest::from_yaml_str(&yaml).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("'local_source: true'"),
                "runtime {runtime}: error was: {msg}"
            );
            assert!(
                msg.contains("my-thing"),
                "runtime {runtime}: error was: {msg}"
            );
        }
    }

    #[test]
    fn registry_skill_still_requires_version() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    runtime: skill
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("missing required field 'version'"),
            "error was: {msg}"
        );
    }

    #[test]
    fn rejects_unknown_inference_transport() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: grpc
  endpoint: ignored
  model: test-model
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.transport"), "error was: {msg}");
        assert!(msg.contains("unknown value"), "error was: {msg}");
        assert!(msg.contains("grpc"), "error was: {msg}");
    }

    #[test]
    fn parses_process_transport() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: claude
  model: claude-haiku-4-5-20251001
  max_turns: 20
"#,
        )
        .unwrap();

        let inference = manifest.inference.expect("inference should exist");
        assert_eq!(inference.transport, "process");
        assert_eq!(inference.command, Some("claude".to_string()));
        assert_eq!(inference.model, "claude-haiku-4-5-20251001");
        assert_eq!(inference.max_turns, 20);
        assert!(inference.driver.is_none());
        assert!(inference.endpoint.is_none());
        assert!(inference.api_key.is_none());
    }

    #[test]
    fn process_transport_requires_command() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  model: claude-haiku-4-5-20251001
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.command"), "error was: {msg}");
    }

    #[test]
    fn process_transport_allows_absent_model() {
        // model is optional for transport: process — an empty/absent model means "use the CLI's
        // account-default model" (e.g. a codex subscription default). The command is still required.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: codex
"#,
        )
        .expect("process transport should parse without a model");
        let inference = manifest.inference.expect("inference present");
        assert_eq!(inference.transport, "process");
        assert_eq!(inference.command.as_deref(), Some("codex"));
        assert_eq!(
            inference.model, "",
            "absent model resolves to empty (provider default)"
        );
    }

    #[test]
    fn process_transport_rejects_driver_artifact() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: claude
  model: claude-haiku-4-5-20251001
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("inference.driver.artifact"),
            "error was: {msg}"
        );
        assert!(
            msg.contains("not valid with transport: process"),
            "error was: {msg}"
        );
    }

    #[test]
    fn process_transport_rejects_endpoint() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: claude
  model: claude-haiku-4-5-20251001
  endpoint: http://localhost:8080
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.endpoint"), "error was: {msg}");
        assert!(
            msg.contains("not valid with transport: process"),
            "error was: {msg}"
        );
    }

    #[test]
    fn process_transport_rejects_api_key() {
        // Key assembled at runtime so the source never contains a
        // credential-shaped literal that secret scanners could flag.
        let key = ["sk-", "ant-", "secret"].concat();
        let err = RuntimeManifest::from_yaml_str(&format!(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: claude
  model: claude-haiku-4-5-20251001
  api_key: {key}
"#
        ))
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.api_key"), "error was: {msg}");
        assert!(
            msg.contains("not valid with transport: process"),
            "error was: {msg}"
        );
    }

    #[test]
    fn process_transport_rejects_max_tokens() {
        // Rejected even at a perfectly valid value: the CLI subprocess path never builds a
        // driver payload, so accepting it would leave it silently inert.
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: process
  command: claude
  model: claude-haiku-4-5-20251001
  max_tokens: 4096
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.max_tokens"), "error was: {msg}");
        assert!(
            msg.contains("not valid with transport: process"),
            "error was: {msg}"
        );
    }

    #[test]
    fn inference_max_tokens_round_trips() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  max_tokens: 4096
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let inference = manifest.inference.unwrap();
        assert_eq!(inference.max_tokens, Some(4096));
    }

    #[test]
    fn inference_max_tokens_absent_is_none() {
        // Absent means "the manifest didn't set it" all the way down; the 8192 default is
        // applied once, by the runtime, not smuggled in here.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        assert_eq!(manifest.inference.unwrap().max_tokens, None);
    }

    #[test]
    fn inference_max_tokens_zero_is_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  max_tokens: 0
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("inference.max_tokens"), "error was: {msg}");
        assert!(msg.contains("must be greater than 0"), "error was: {msg}");
    }

    #[test]
    fn inference_max_tokens_large_value_is_accepted() {
        // Manifest validation stays advisory: an over-large cap is the provider's to reject
        // at request time, not ours to clamp or block at load time.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  max_tokens: 999999
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        assert_eq!(manifest.inference.unwrap().max_tokens, Some(999_999));
    }

    #[test]
    fn inference_and_context_max_tokens_are_independent() {
        // Two unrelated concepts that happen to share a leaf name: per-turn output cap vs.
        // session compaction budget. Neither may leak into the other.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  max_tokens: 4096
  driver:
    artifact: murmur-driver-anthropic
context:
  max_tokens: 200000
"#,
        )
        .unwrap();

        assert_eq!(manifest.inference.unwrap().max_tokens, Some(4096));
        assert_eq!(manifest.context.unwrap().max_tokens, Some(200_000));
    }

    #[test]
    fn hook_config_defaults_when_fields_absent() {
        let config = parse_hook_config_from_yaml("name: my-hook\nruntime: hook\n").unwrap();
        assert_eq!(config.binding, HookBinding::All);
        assert_eq!(config.execution_mode, HookExecutionMode::Blocking);
        assert_eq!(config.commit_policy, HookCommitPolicy::None);
    }

    #[test]
    fn hook_config_parses_all_fields() {
        let yaml = "name: my-hook\nruntime: hook\nbinding: on-compaction\nexecution_mode: blocking\ncommit_policy: replace-context\n";
        let config = parse_hook_config_from_yaml(yaml).unwrap();
        assert_eq!(config.binding, HookBinding::OnCompaction);
        assert_eq!(config.execution_mode, HookExecutionMode::Blocking);
        assert_eq!(config.commit_policy, HookCommitPolicy::ReplaceContext);
    }

    #[test]
    fn hook_config_parses_on_task_start() {
        let yaml = "name: my-hook\nruntime: hook\nbinding: on-task-start\n";
        let config = parse_hook_config_from_yaml(yaml).unwrap();
        assert_eq!(config.binding, HookBinding::OnTaskStart);
    }

    #[test]
    fn hook_config_parses_on_task_end() {
        let yaml = "name: my-hook\nruntime: hook\nbinding: on-task-end\n";
        let config = parse_hook_config_from_yaml(yaml).unwrap();
        assert_eq!(config.binding, HookBinding::OnTaskEnd);
    }

    #[test]
    fn hook_config_unknown_binding_lists_task_events() {
        let yaml = "name: my-hook\nruntime: hook\nbinding: on-nonsense\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("on-task-start"), "error was: {err}");
        assert!(err.contains("on-task-end"), "error was: {err}");
    }

    #[test]
    fn hook_config_async_with_commit_is_rejected() {
        let yaml =
            "name: my-hook\nruntime: hook\nexecution_mode: async\ncommit_policy: replace-context\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("async-with-commit"), "error was: {err}");
    }

    #[test]
    fn hook_config_on_stage_async_is_rejected() {
        let yaml = "name: my-hook\nruntime: hook\nbinding: on-stage\nexecution_mode: async\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("on-stage"), "error was: {err}");
        assert!(err.contains("blocking"), "error was: {err}");
    }

    #[test]
    fn hook_config_async_none_is_valid() {
        let yaml = "name: debug-hook\nruntime: hook\nexecution_mode: async\ncommit_policy: none\n";
        let config = parse_hook_config_from_yaml(yaml).unwrap();
        assert_eq!(config.execution_mode, HookExecutionMode::Async);
        assert_eq!(config.commit_policy, HookCommitPolicy::None);
    }

    #[test]
    fn hook_config_parses_reopen_task_commit_policy() {
        let yaml = "name: gate\nruntime: hook\nbinding: on-task-end\ncommit_policy: reopen-task\n";
        let config = parse_hook_config_from_yaml(yaml).unwrap();
        assert_eq!(config.binding, HookBinding::OnTaskEnd);
        assert_eq!(config.commit_policy, HookCommitPolicy::ReopenTask);
    }

    #[test]
    fn hook_config_unknown_commit_policy_lists_reopen_task() {
        let yaml = "name: gate\nruntime: hook\ncommit_policy: on-nonsense\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("reopen-task"), "error was: {err}");
    }

    #[test]
    fn hook_config_parses_deny_commit_policy_for_both_gated_bindings() {
        for binding in ["on-shell", "on-tool-call"] {
            let yaml =
                format!("name: policy\nruntime: hook\nbinding: {binding}\ncommit_policy: deny\n");
            let config = parse_hook_config_from_yaml(&yaml).unwrap();
            assert_eq!(config.commit_policy, HookCommitPolicy::Deny);
        }
        assert_eq!(HookCommitPolicy::Deny.as_str(), "deny");
    }

    /// `deny` is the one policy an omitted `binding:` cannot carry: a hook that does not name
    /// which of the two gated events it decides on would be asked at both.
    #[test]
    fn hook_config_deny_without_a_binding_is_rejected() {
        let yaml = "name: policy\nruntime: hook\ncommit_policy: deny\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(
            err.contains("requires an explicit binding"),
            "error was: {err}"
        );
        assert!(err.contains("on-shell"), "error was: {err}");
        assert!(err.contains("on-tool-call"), "error was: {err}");
    }

    /// Every other binding refuses `deny` through the existing binding-mismatch check.
    #[test]
    fn hook_config_deny_on_an_ungated_binding_is_rejected() {
        let yaml = "name: policy\nruntime: hook\nbinding: on-compaction\ncommit_policy: deny\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("is not valid for binding"), "error was: {err}");
        assert!(err.contains("replace-context"), "error was: {err}");
    }

    /// The unchanged async/commit check covers `deny` for free; this pins that it does.
    #[test]
    fn hook_config_async_deny_is_rejected() {
        let yaml =
            "name: policy\nruntime: hook\nbinding: on-shell\nexecution_mode: async\ncommit_policy: deny\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("async-with-commit"), "error was: {err}");
        assert!(err.contains("deny"), "error was: {err}");
    }

    #[test]
    fn hook_config_unknown_commit_policy_lists_deny() {
        let yaml = "name: gate\nruntime: hook\ncommit_policy: on-nonsense\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("deny"), "error was: {err}");
    }

    /// The existing, unmodified async/commit validation must also reject
    /// `commit_policy: reopen-task` combined with `execution_mode: async`.
    #[test]
    fn hook_config_async_reopen_task_is_rejected() {
        let yaml = "name: gate\nruntime: hook\nexecution_mode: async\ncommit_policy: reopen-task\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("async-with-commit"), "error was: {err}");
        assert!(err.contains("reopen-task"), "error was: {err}");
    }

    // ── binding is the single source of truth for what a hook commits ─────────────

    /// The two bindings whose events honor no `hook-output` arm at all cannot declare
    /// any non-`none` `commit_policy`. The error names the binding, the declared policy,
    /// and `none` as what the binding honors.
    #[test]
    fn hook_config_non_committing_binding_rejects_any_commit_policy() {
        for binding in ["on-session-start", "on-session-end"] {
            let yaml = format!(
                "name: gate\nruntime: hook\nbinding: {binding}\ncommit_policy: replace-context\n"
            );
            let err = parse_hook_config_from_yaml(&yaml).unwrap_err();
            assert!(err.contains(binding), "error was: {err}");
            assert!(err.contains("replace-context"), "error was: {err}");
            assert!(
                err.contains("honors commit_policy 'none'"),
                "error was: {err}"
            );
        }
    }

    /// A binding that honors one arm rejects every *other* policy, naming the one it does
    /// honor so the fix is obvious from the message alone.
    #[test]
    fn hook_config_binding_rejects_a_different_bindings_commit_policy() {
        for (binding, declared, honored) in [
            ("on-stage", "reopen-task", "write-manifests"),
            ("on-compaction", "write-manifests", "replace-context"),
            ("on-task-end", "replace-context", "reopen-task"),
            ("on-task-start", "replace-context", "seed-context"),
            ("on-shell", "replace-context", "deny"),
            ("on-tool-call", "replace-context", "deny"),
        ] {
            let yaml = format!(
                "name: gate\nruntime: hook\nbinding: {binding}\ncommit_policy: {declared}\n"
            );
            let err = parse_hook_config_from_yaml(&yaml).unwrap_err();
            assert!(err.contains(binding), "error was: {err}");
            assert!(
                err.contains(&format!("commit_policy '{declared}' is not valid")),
                "error was: {err}"
            );
            assert!(
                err.contains(&format!("honors commit_policy '{honored}'")),
                "error was: {err}"
            );
        }
    }

    /// `on-inference` honors the `artifact` arm, which has no `commit_policy` spelling —
    /// so *no* non-`none` policy is ever valid for it, including `write-manifests`.
    #[test]
    fn hook_config_on_inference_rejects_every_non_none_commit_policy() {
        for declared in ["replace-context", "write-manifests", "reopen-task"] {
            let yaml = format!(
                "name: gate\nruntime: hook\nbinding: on-inference\ncommit_policy: {declared}\n"
            );
            let err = parse_hook_config_from_yaml(&yaml).unwrap_err();
            assert!(err.contains("on-inference"), "error was: {err}");
            assert!(err.contains(declared), "error was: {err}");
            assert!(
                err.contains("honors commit_policy 'none'"),
                "error was: {err}"
            );
            assert!(err.contains("artifact"), "error was: {err}");
        }
    }

    /// A binding declaring exactly the policy it honors is accepted, for every
    /// representable pair.
    #[test]
    fn hook_config_matching_binding_and_commit_policy_is_valid() {
        for (binding, policy, expected) in [
            (
                "on-stage",
                "write-manifests",
                HookCommitPolicy::WriteManifests,
            ),
            (
                "on-compaction",
                "replace-context",
                HookCommitPolicy::ReplaceContext,
            ),
            ("on-task-end", "reopen-task", HookCommitPolicy::ReopenTask),
            ("on-shell", "deny", HookCommitPolicy::Deny),
            ("on-tool-call", "deny", HookCommitPolicy::Deny),
        ] {
            let yaml =
                format!("name: gate\nruntime: hook\nbinding: {binding}\ncommit_policy: {policy}\n");
            let config = parse_hook_config_from_yaml(&yaml).unwrap();
            assert_eq!(config.commit_policy, expected);
        }
    }

    /// Every binding may declare `commit_policy: none` — including the ones that honor
    /// an arm, which simply means the hook opts out of committing.
    #[test]
    fn hook_config_commit_policy_none_is_valid_for_every_binding() {
        for binding in [
            "on-stage",
            "on-session-start",
            "on-task-start",
            "on-inference",
            "on-tool-call",
            "on-shell",
            "on-compaction",
            "on-task-end",
            "on-session-end",
        ] {
            let yaml =
                format!("name: gate\nruntime: hook\nbinding: {binding}\ncommit_policy: none\n");
            let config = parse_hook_config_from_yaml(&yaml).unwrap();
            assert_eq!(config.commit_policy, HookCommitPolicy::None);
        }
    }

    /// Resolved ambiguity: an omitted `binding:` is `HookBinding::All`, which is dispatched
    /// to every event — including every one that honors an arm — so every `commit_policy` is
    /// accepted for it. Deliberate: no narrowing for `All`. `deny` is the one exception,
    /// covered by [`hook_config_deny_without_a_binding_is_rejected`].
    #[test]
    fn hook_config_omitted_binding_accepts_every_commit_policy() {
        for (policy, expected) in [
            ("replace-context", HookCommitPolicy::ReplaceContext),
            ("write-manifests", HookCommitPolicy::WriteManifests),
            ("reopen-task", HookCommitPolicy::ReopenTask),
            ("none", HookCommitPolicy::None),
        ] {
            let yaml = format!("name: gate\nruntime: hook\ncommit_policy: {policy}\n");
            let config = parse_hook_config_from_yaml(&yaml).unwrap();
            assert_eq!(config.binding, HookBinding::All);
            assert_eq!(config.commit_policy, expected);
        }
    }

    /// Every hook artifact shipped in `default-artifacts` today must keep parsing after the
    /// binding/commit_policy check exists — this change is non-breaking for all eight. The
    /// field combinations are reproduced literally rather than read from that repo, so this
    /// suite stays self-contained.
    #[test]
    fn hook_config_shipped_default_artifact_manifests_still_parse() {
        let shipped: [(&str, &str, HookConfig); 8] = [
            (
                "murmur-hook-compact",
                "binding: on-compaction\nexecution_mode: blocking\ncommit_policy: replace-context\n",
                HookConfig {
                    binding: HookBinding::OnCompaction,
                    execution_mode: HookExecutionMode::Blocking,
                    commit_policy: HookCommitPolicy::ReplaceContext,
                },
            ),
            (
                "murmur-hook-shell-desc",
                "binding: on-stage\nexecution_mode: blocking\ncommit_policy: write-manifests\n",
                HookConfig {
                    binding: HookBinding::OnStage,
                    execution_mode: HookExecutionMode::Blocking,
                    commit_policy: HookCommitPolicy::WriteManifests,
                },
            ),
            (
                // No `binding:` line in the shipped manifest — `All`, not `on-task-end`.
                "murmur-hook-regression-verifier",
                "execution_mode: blocking\ncommit_policy: reopen-task\n",
                HookConfig {
                    binding: HookBinding::All,
                    execution_mode: HookExecutionMode::Blocking,
                    commit_policy: HookCommitPolicy::ReopenTask,
                },
            ),
            (
                "murmur-hook-memory",
                "binding: on-task-start\nexecution_mode: blocking\ncommit_policy: seed-context\n",
                HookConfig {
                    binding: HookBinding::OnTaskStart,
                    execution_mode: HookExecutionMode::Blocking,
                    commit_policy: HookCommitPolicy::SeedContext,
                },
            ),
            (
                "murmur-hook-debug",
                "execution_mode: async\ncommit_policy: none\n",
                HookConfig {
                    binding: HookBinding::All,
                    execution_mode: HookExecutionMode::Async,
                    commit_policy: HookCommitPolicy::None,
                },
            ),
            (
                "murmur-hook-diff-summary",
                "execution_mode: blocking\ncommit_policy: none\n",
                HookConfig {
                    binding: HookBinding::All,
                    execution_mode: HookExecutionMode::Blocking,
                    commit_policy: HookCommitPolicy::None,
                },
            ),
            ("murmur-hook-eval", "", HookConfig::default()),
            ("murmur-hook-grafana", "", HookConfig::default()),
        ];

        for (name, fields, expected) in shipped {
            let yaml = format!("name: {name}\nruntime: hook\n{fields}");
            let config = parse_hook_config_from_yaml(&yaml)
                .unwrap_or_else(|e| panic!("shipped hook {name} must still parse: {e}"));
            assert_eq!(config, expected, "shipped hook {name}");
        }
    }

    /// The manifest-side honored-policy table agrees with the manifest spellings the
    /// parser accepts; `capsule-runtime` separately cross-checks it against its own
    /// `HONORED_OUTPUT_ARM` dispatch table.
    #[test]
    fn commit_policy_for_binding_matches_the_declarable_pairs() {
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnStage),
            Some(HookCommitPolicy::WriteManifests)
        );
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnCompaction),
            Some(HookCommitPolicy::ReplaceContext)
        );
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnTaskEnd),
            Some(HookCommitPolicy::ReopenTask)
        );
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnTaskStart),
            Some(HookCommitPolicy::SeedContext)
        );
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnToolCall),
            Some(HookCommitPolicy::Deny)
        );
        assert_eq!(
            commit_policy_for_binding(&HookBinding::OnShell),
            Some(HookCommitPolicy::Deny)
        );
        // `on-inference` honors `artifact`, which has no `commit_policy` spelling; the
        // other two honor nothing. `All` is unconstrained by design, apart from the `deny`
        // carve-out the parser applies.
        for binding in [
            HookBinding::OnSessionStart,
            HookBinding::OnInference,
            HookBinding::OnSessionEnd,
            HookBinding::All,
        ] {
            assert_eq!(commit_policy_for_binding(&binding), None, "{binding:?}");
        }
    }

    /// The `on-task-start` pairing is declarable end to end: the parser accepts it, and
    /// refuses the same policy on any other binding by name.
    #[test]
    fn seed_context_commit_policy_is_declarable_only_on_on_task_start() {
        let config = parse_hook_config_from_yaml(
            "name: memory\nversion: 0.1.0\nruntime: hook\nbinding: on-task-start\n\
             commit_policy: seed-context\n",
        )
        .expect("on-task-start + seed-context is the declarable pairing");
        assert_eq!(config.binding, HookBinding::OnTaskStart);
        assert_eq!(config.commit_policy, HookCommitPolicy::SeedContext);

        let error = parse_hook_config_from_yaml(
            "name: memory\nversion: 0.1.0\nruntime: hook\nbinding: on-compaction\n\
             commit_policy: seed-context\n",
        )
        .expect_err("on-compaction cannot commit a seed");
        assert!(error.contains("on-compaction"), "{error}");
        assert!(error.contains("seed-context"), "{error}");
    }

    /// The unknown-value diagnostic lists every accepted spelling, so an operator who
    /// mistypes one reads the whole set rather than guessing.
    #[test]
    fn unknown_commit_policy_names_seed_context_among_the_accepted_values() {
        let error = parse_hook_config_from_yaml(
            "name: h\nversion: 0.1.0\nruntime: hook\ncommit_policy: seed-contxt\n",
        )
        .expect_err("a misspelled policy is rejected");
        assert!(error.contains("seed-context"), "{error}");
    }

    // ── context.seed_budget / context.seed_overflow_margin ───────────────────

    fn context_of(yaml_block: &str) -> Option<ContextConfig> {
        RuntimeManifest::from_yaml_str(&format!(
            "name: cap\nversion: 0.1.0\nartifacts: []\n{yaml_block}"
        ))
        .expect("the manifest under test parses")
        .context
    }

    #[test]
    fn seed_budget_and_overflow_margin_parse_from_the_context_block() {
        let context = context_of(
            "context:\n  max_tokens: 200000\n  seed_budget: 0.10\n  seed_overflow_margin: 0.10\n",
        )
        .expect("a declared context block parses to Some");
        assert_eq!(context.max_tokens, Some(200_000));
        assert!((context.seed_budget - 0.10).abs() < f32::EPSILON);
        assert!((context.seed_overflow_margin - 0.10).abs() < f32::EPSILON);
    }

    #[test]
    fn seed_budget_and_overflow_margin_default_when_omitted() {
        let context = context_of("context:\n  max_tokens: 200000\n")
            .expect("a declared context block parses to Some");
        assert!((context.seed_budget - DEFAULT_SEED_BUDGET).abs() < f32::EPSILON);
        assert!((context.seed_overflow_margin - DEFAULT_SEED_OVERFLOW_MARGIN).abs() < f32::EPSILON);
    }

    /// No `context:` block at all stays `None` — the two fractions default *within* a
    /// declared block, and never conjure one that the operator did not write.
    #[test]
    fn seed_budget_defaults_do_not_create_a_context_block() {
        assert_eq!(context_of(""), None);
    }

    #[test]
    fn seed_budget_outside_zero_to_one_is_rejected_by_field_name() {
        for (block, field) in [
            ("context:\n  seed_budget: 1.5\n", "context.seed_budget"),
            ("context:\n  seed_budget: -0.1\n", "context.seed_budget"),
            (
                "context:\n  seed_overflow_margin: 2.0\n",
                "context.seed_overflow_margin",
            ),
        ] {
            let error = RuntimeManifest::from_yaml_str(&format!(
                "name: cap\nversion: 0.1.0\nartifacts: []\n{block}"
            ))
            .expect_err("an out-of-range fraction is rejected");
            match error {
                RuntimeManifestError::InvalidInferenceConfig {
                    field: got,
                    ref message,
                } => {
                    assert_eq!(got, field);
                    assert!(message.contains("0.0 and 1.0"), "{message}");
                }
                other => panic!("expected InvalidInferenceConfig for {block:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_tool_implementation_defaults_to_wasm() {
        let impl_ = parse_tool_implementation_from_yaml("name: my-tool\nruntime: tool\n");
        assert_eq!(impl_, ArtifactImplementation::Wasm);
    }

    #[test]
    fn parse_tool_implementation_native() {
        let impl_ = parse_tool_implementation_from_yaml(
            "name: my-tool\nruntime: tool\nimplementation: native\n",
        );
        assert_eq!(impl_, ArtifactImplementation::Native);
    }

    #[test]
    fn mur_version_parses_correctly() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.1.0\nartifacts: []\nmur_version: \"0.4.5\"\n",
        )
        .unwrap();
        assert_eq!(manifest.mur_version, Some("0.4.5".to_string()));
    }

    #[test]
    fn mur_version_absent_is_none() {
        let manifest =
            RuntimeManifest::from_yaml_str("name: cap\nversion: 0.1.0\nartifacts: []\n").unwrap();
        assert_eq!(manifest.mur_version, None);
    }

    #[test]
    fn mur_version_empty_string_is_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.1.0\nartifacts: []\nmur_version: \"\"\n",
        )
        .unwrap();
        assert_eq!(manifest.mur_version, None);
    }

    #[test]
    fn inference_max_turns_defaults_to_10() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_turns, 10);
    }

    #[test]
    fn inference_max_turns_explicit_value() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  max_turns: 20\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_turns, 20);
    }

    #[test]
    fn inference_max_turns_zero_is_rejected() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  max_turns: 0\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("inference.max_turns"), "error was: {msg}");
        assert!(msg.contains("greater than 0"), "error was: {msg}");
    }

    /// A `lifecycle:` block that says nothing about the budget still permits one reopen.
    #[test]
    fn lifecycle_shell_grace_secs_lowers_the_declared_value() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  shell_grace_secs: 45\n",
        )
        .unwrap();
        let lifecycle = manifest.lifecycle.unwrap();
        assert_eq!(lifecycle.shell_grace_secs, 45);
        // Every other lifecycle field is untouched by declaring this one.
        assert_eq!(
            lifecycle.task_acceptance,
            LifecycleConfig::default().task_acceptance
        );
        assert_eq!(lifecycle.after_task, LifecycleConfig::default().after_task);
        assert_eq!(
            lifecycle.queue_depth,
            LifecycleConfig::default().queue_depth
        );
        assert_eq!(lifecycle.input_timeout_secs, None);
        assert_eq!(
            lifecycle.conversation_mode,
            LifecycleConfig::default().conversation_mode
        );
        assert_eq!(
            lifecycle.max_task_reopens,
            LifecycleConfig::default().max_task_reopens
        );
    }

    #[test]
    fn lifecycle_shell_grace_secs_defaults_to_10() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  queue_depth: 3\n",
        )
        .unwrap();
        let lifecycle = manifest.lifecycle.unwrap();
        assert_eq!(lifecycle.shell_grace_secs, 10);
        assert_eq!(lifecycle.queue_depth, 3);

        let without_block =
            RuntimeManifest::from_yaml_str("name: cap\nversion: 0.0.1\nartifacts: []\n").unwrap();
        assert_eq!(without_block.effective_lifecycle().shell_grace_secs, 10);
    }

    /// `0` is a valid explicit value, not an absent one: it demotes at the first poll after the
    /// spawn, so effectively every command detaches.
    #[test]
    fn lifecycle_shell_grace_secs_zero_is_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  shell_grace_secs: 0\n",
        )
        .unwrap();
        let lifecycle = manifest.lifecycle.unwrap();
        assert_eq!(lifecycle.shell_grace_secs, 0);
        assert_eq!(
            lifecycle.max_task_reopens,
            LifecycleConfig::default().max_task_reopens
        );
    }

    #[test]
    fn lifecycle_max_task_reopens_defaults_to_1() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  after_task: sleep\n",
        )
        .unwrap();
        assert_eq!(manifest.lifecycle.unwrap().max_task_reopens, 1);
    }

    /// No `lifecycle:` block at all lands on the same default through `effective_lifecycle`.
    #[test]
    fn lifecycle_max_task_reopens_defaults_to_1_without_lifecycle_block() {
        let manifest =
            RuntimeManifest::from_yaml_str("name: cap\nversion: 0.0.1\nartifacts: []\n").unwrap();
        assert!(manifest.lifecycle.is_none());
        assert_eq!(manifest.effective_lifecycle().max_task_reopens, 1);
    }

    #[test]
    fn lifecycle_max_task_reopens_explicit_value() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  max_task_reopens: 3\n",
        )
        .unwrap();
        assert_eq!(manifest.lifecycle.unwrap().max_task_reopens, 3);
    }

    /// Unlike `inference.max_turns`, `0` is a valid explicit value — it disables reopening.
    #[test]
    fn lifecycle_max_task_reopens_zero_is_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\nlifecycle:\n  max_task_reopens: 0\n",
        )
        .unwrap();
        assert_eq!(manifest.lifecycle.unwrap().max_task_reopens, 0);
    }

    /// The budget is independent of the inference transport in play.
    #[test]
    fn lifecycle_max_task_reopens_with_process_transport() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: process\n  command: claude\nlifecycle:\n  max_task_reopens: 2\n",
        ).unwrap();
        assert_eq!(manifest.lifecycle.unwrap().max_task_reopens, 2);
    }

    #[test]
    fn inference_max_task_reopens_is_rejected_under_http_transport() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  max_task_reopens: 3\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap_err();
        assert!(
            matches!(
                &err,
                RuntimeManifestError::InvalidInferenceConfig { field, .. }
                    if field == "inference.max_task_reopens"
            ),
            "error was: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("lifecycle.max_task_reopens"),
            "error was: {msg}"
        );
    }

    #[test]
    fn inference_max_task_reopens_is_rejected_under_process_transport() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: process\n  command: claude\n  max_task_reopens: 2\n",
        ).unwrap_err();
        assert!(
            matches!(
                &err,
                RuntimeManifestError::InvalidInferenceConfig { field, .. }
                    if field == "inference.max_task_reopens"
            ),
            "error was: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("lifecycle.max_task_reopens"),
            "error was: {msg}"
        );
    }

    /// The rejection is unconditional: the new key being present alongside it changes nothing.
    #[test]
    fn inference_max_task_reopens_is_rejected_even_alongside_the_lifecycle_key() {
        let err = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: process\n  command: claude\n  max_task_reopens: 2\nlifecycle:\n  max_task_reopens: 3\n",
        ).unwrap_err();
        assert!(
            matches!(
                &err,
                RuntimeManifestError::InvalidInferenceConfig { field, .. }
                    if field == "inference.max_task_reopens"
            ),
            "error was: {err:?}"
        );
    }

    #[test]
    fn skill_is_llm_visible() {
        assert!(ArtifactRuntime::Skill.is_llm_visible());
    }

    #[test]
    fn driver_and_hook_are_not_llm_visible() {
        assert!(!ArtifactRuntime::Driver.is_llm_visible());
        assert!(!ArtifactRuntime::Hook.is_llm_visible());
    }

    #[test]
    fn parses_system_prompt_artifact() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    version: 1.0.0
    runtime: skill
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_artifact: my-skill
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap();

        let inference = manifest.inference.unwrap();
        assert_eq!(
            inference.system_prompt_artifact,
            Some("my-skill".to_string())
        );
        assert!(inference.system_prompt.is_none());
        assert!(inference.system_prompt_file.is_none());
    }

    #[test]
    fn rejects_system_prompt_artifact_with_inline_prompt() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    version: 1.0.0
    runtime: skill
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt: "inline"
  system_prompt_artifact: my-skill
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("at most one"), "error was: {msg}");
    }

    #[test]
    fn rejects_system_prompt_artifact_naming_non_skill() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-tool
    version: 1.0.0
    runtime: tool
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_artifact: my-tool
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("system_prompt_artifact"), "error was: {msg}");
        assert!(msg.contains("'prompt_payload: true'"), "error was: {msg}");
    }

    #[test]
    fn rejects_system_prompt_artifact_naming_undeclared_artifact() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts: []
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt_artifact: missing-skill
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("system_prompt_artifact"), "error was: {msg}");
        assert!(msg.contains("not declared"), "error was: {msg}");
    }

    #[test]
    fn rejects_all_three_system_prompt_sources() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: my-skill
    version: 1.0.0
    runtime: skill
inference:
  endpoint: http://127.0.0.1:8080
  model: test-model
  system_prompt: "inline"
  system_prompt_file: "conventions.md"
  system_prompt_artifact: my-skill
  driver:
    artifact: murmur-driver-anthropic
"#,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("at most one"), "error was: {msg}");
    }

    // ── Per-artifact capability grants ───────────────────────────────────────

    #[test]
    fn hook_artifact_capabilities_parse_network_and_filesystem() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: telemetry-hook
    version: 1.0.0
    runtime: hook
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
      filesystem:
        scope: hook-state
"#,
        )
        .unwrap();

        let caps = manifest.artifacts[0]
            .capabilities
            .as_ref()
            .expect("hook entry carries a capability grant");
        assert_eq!(
            caps.network.as_ref().unwrap().allow,
            vec!["https://telemetry.example.com".to_string()]
        );
        assert_eq!(
            caps.filesystem.as_ref().unwrap().scope,
            Some("hook-state".to_string())
        );
    }

    /// Default-deny: a hook entry with no `capabilities:` key parses to `None`, which the
    /// runtime lowers to zero network and zero preopened directories.
    #[test]
    fn hook_artifact_without_capabilities_is_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: quiet-hook
    version: 1.0.0
    runtime: hook
"#,
        )
        .unwrap();

        assert!(manifest.artifacts[0].capabilities.is_none());
    }

    /// Tools and drivers execute, so they may carry a grant — the runtime reads it as a
    /// narrowing of the capsule-wide ceiling rather than as a default-deny baseline.
    #[test]
    fn tool_and_driver_artifact_capabilities_parse() {
        for role in ["tool", "driver"] {
            let yaml = format!(
                r#"
name: cap
version: 0.0.1
artifacts:
  - name: scoped
    version: 1.0.0
    runtime: {role}
    capabilities:
      network:
        allow:
          - https://api.example.com
      filesystem:
        scope: cache
"#
            );
            let manifest = RuntimeManifest::from_yaml_str(&yaml)
                .unwrap_or_else(|err| panic!("runtime: {role} may carry a grant, got: {err}"));

            let caps = manifest.artifacts[0]
                .capabilities
                .as_ref()
                .unwrap_or_else(|| panic!("runtime: {role} entry carries a capability grant"));
            assert_eq!(
                caps.network.as_ref().unwrap().allow,
                vec!["https://api.example.com".to_string()]
            );
            assert_eq!(
                caps.filesystem.as_ref().unwrap().scope,
                Some("cache".to_string())
            );
        }
    }

    /// A tool/driver entry with no `capabilities:` key parses to `None`, which the runtime
    /// lowers to the unchanged capsule ceiling.
    #[test]
    fn tool_artifact_without_capabilities_is_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: plain-tool
    version: 1.0.0
    runtime: tool
"#,
        )
        .unwrap();

        assert!(manifest.artifacts[0].capabilities.is_none());
    }

    /// A skill has no execution surface, so a grant on one would be silently unenforced —
    /// rejected at parse time, with a message naming the roles that do work.
    #[test]
    fn skill_artifact_capabilities_are_rejected() {
        let yaml = r#"
name: cap
version: 0.0.1
artifacts:
  - name: sneaky
    version: 1.0.0
    runtime: skill
    capabilities:
      network:
        allow:
          - https://evil.example.com
"#;
        let err = match RuntimeManifest::from_yaml_str(yaml) {
            Ok(_) => panic!("per-artifact capabilities must be rejected for runtime: skill"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("sneaky"), "error was: {msg}");
        assert!(
            msg.contains(
                "only recognized on 'runtime: hook', 'runtime: tool', and 'runtime: driver' \
                 entries"
            ),
            "error was: {msg}"
        );
        assert!(msg.contains("runtime: skill"), "error was: {msg}");
    }

    // ── Per-artifact `config:` (operator-authored artifact configuration) ────

    /// The block is carried verbatim as YAML. This crate accepts any mapping; the runtime is
    /// what lowers it to JSON and refuses a shape that cannot travel that way.
    #[test]
    fn artifact_config_is_carried_verbatim() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: murmur-tool-corpus
    version: 0.1.0
    runtime: tool
    config:
      read_recent: { default: 20, max: 100 }
"#,
        )
        .unwrap();

        let config = manifest.artifacts[0]
            .config
            .as_ref()
            .expect("a declared block parses");
        assert_eq!(
            config["read_recent"]["max"],
            serde_yaml::Value::Number(100.into())
        );
    }

    /// Absent means absent: no key, no variable, and nothing for the runtime to lower.
    #[test]
    fn an_undeclared_config_is_none() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: git
    version: 1.0.0
    runtime: tool
"#,
        )
        .unwrap();

        assert!(manifest.artifacts[0].config.is_none());
    }

    /// `config:` with nothing under it is a written declaration that carries nothing, and must
    /// stay distinguishable from the key being absent — the runtime refuses it, and can only do
    /// so if the parser does not collapse YAML null into `None`.
    #[test]
    fn an_empty_config_block_is_distinguishable_from_an_absent_one() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: git
    version: 1.0.0
    runtime: tool
    config:
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.artifacts[0].config,
            Some(serde_yaml::Value::Null),
            "an empty block must survive parsing as a declaration, not vanish into None"
        );
    }

    /// A skill runs no component and holds no grant, so nothing would ever deliver a config
    /// block to one — rejected at parse time, with a message naming the roles that do.
    #[test]
    fn skill_artifact_config_is_rejected() {
        let yaml = r#"
name: cap
version: 0.0.1
artifacts:
  - name: sneaky
    version: 1.0.0
    runtime: skill
    config:
      who: me
"#;
        let err = match RuntimeManifest::from_yaml_str(yaml) {
            Ok(_) => panic!("config: must be rejected for runtime: skill"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeManifestError::InvalidArtifact { index: 0, .. }
        ));
        let msg = err.to_string();
        assert!(msg.contains("sneaky"), "error was: {msg}");
        assert!(msg.contains("runtime: skill"), "error was: {msg}");
        assert!(
            msg.contains("only on 'runtime: hook', 'runtime: tool', and 'runtime: driver' entries"),
            "error was: {msg}"
        );
    }

    /// Config is delivered on one artifact's own grant, so a capsule-wide block reaches nothing.
    /// Refused rather than silently ignored, and the message points at the artifact entry.
    #[test]
    fn a_capsule_wide_config_block_is_rejected() {
        let yaml = r#"
name: cap
version: 0.0.1
config:
  who: capsule
artifacts:
  - name: git
    version: 1.0.0
    runtime: tool
"#;
        let err = match RuntimeManifest::from_yaml_str(yaml) {
            Ok(_) => panic!("a capsule-wide config block must be rejected"),
            Err(err) => err,
        };
        let msg = err.to_string();
        assert!(msg.contains("config"), "error was: {msg}");
        assert!(msg.contains("declared per artifact"), "error was: {msg}");
    }

    // ── Per-artifact `on_overflow:` (async hook queue policy) ────────────────

    /// `on_overflow: block` on a hook entry parses to the blocking policy.
    #[test]
    fn hook_on_overflow_block_parses() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: murmur-hook-grafana
    version: 1.0.0
    runtime: hook
    on_overflow: block
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].on_overflow, HookOverflowPolicy::Block);
    }

    /// Omitting the key means `drop`: an operator who says nothing gets the loop kept moving,
    /// not backpressure they never asked for.
    #[test]
    fn hook_on_overflow_defaults_to_drop() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: murmur-hook-grafana
    version: 1.0.0
    runtime: hook
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].on_overflow, HookOverflowPolicy::Drop);
        assert_eq!(HookOverflowPolicy::default(), HookOverflowPolicy::Drop);
    }

    /// Only a hook has a job queue, so the key is rejected on every other role rather than
    /// silently ignored — the same rule per-artifact `capabilities:` follows on a skill.
    #[test]
    fn on_overflow_on_a_non_hook_artifact_is_rejected() {
        let yaml = r#"
name: cap
version: 0.0.1
artifacts:
  - name: git
    version: 1.0.0
    runtime: tool
    on_overflow: block
"#;
        let err = match RuntimeManifest::from_yaml_str(yaml) {
            Ok(_) => panic!("on_overflow must be rejected outside runtime: hook"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeManifestError::InvalidArtifact { index: 0, .. }
        ));
        let msg = err.to_string();
        assert!(msg.contains("git"), "error was: {msg}");
        assert!(
            msg.contains("only recognized on 'runtime: hook' entries"),
            "error was: {msg}"
        );
    }

    /// An unknown value is a typo, not a policy — rejected with both spellings named.
    #[test]
    fn unknown_on_overflow_value_is_rejected() {
        let yaml = r#"
name: cap
version: 0.0.1
artifacts:
  - name: murmur-hook-grafana
    version: 1.0.0
    runtime: hook
    on_overflow: spill
"#;
        let err = RuntimeManifest::from_yaml_str(yaml).expect_err("unknown policy is rejected");
        let msg = err.to_string();
        assert!(msg.contains("spill"), "error was: {msg}");
        assert!(msg.contains("expected: drop, block"), "error was: {msg}");
    }

    /// The key is legal on a *blocking* hook entry and simply inert: the operator manifest
    /// cannot know a hook's execution mode, which lives in the artifact's own manifest.
    #[test]
    fn on_overflow_is_accepted_on_any_hook_entry() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: murmur-hook-eval
    version: 1.0.0
    runtime: hook
    on_overflow: drop
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
"#,
        )
        .unwrap();

        assert_eq!(manifest.artifacts[0].on_overflow, HookOverflowPolicy::Drop);
        assert!(manifest.artifacts[0].capabilities.is_some());
    }

    /// The capsule-wide top-level block is untouched by the per-artifact field: both may be
    /// present in one manifest and neither shadows the other.
    #[test]
    fn capsule_wide_and_hook_capabilities_coexist() {
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: telemetry-hook
    version: 1.0.0
    runtime: hook
    capabilities:
      network:
        allow:
          - https://telemetry.example.com
capabilities:
  network:
    allow:
      - https://api.anthropic.com
"#,
        )
        .unwrap();

        assert_eq!(
            manifest.capabilities.unwrap().network.unwrap().allow,
            vec!["https://api.anthropic.com".to_string()]
        );
        assert_eq!(
            manifest.artifacts[0]
                .capabilities
                .as_ref()
                .unwrap()
                .network
                .as_ref()
                .unwrap()
                .allow,
            vec!["https://telemetry.example.com".to_string()]
        );
    }

    /// No self-escalation: the hook artifact's *own* bundled murmur.yaml is parsed only by
    /// `parse_hook_config_from_yaml`, which has no capabilities field and no code path that
    /// reads one. A self-declared `capabilities:` block there is inert.
    #[test]
    fn hook_own_manifest_capabilities_are_not_read_into_config() {
        let config = parse_hook_config_from_yaml(
            r#"
name: telemetry-hook
version: 1.0.0
binding: on-inference
execution_mode: blocking
capabilities:
  network:
    allow:
      - https://evil.example.com
  filesystem:
    scope: /
"#,
        )
        .expect("a self-declared capabilities block is ignored, not an error");

        assert_eq!(
            config,
            HookConfig {
                binding: HookBinding::OnInference,
                execution_mode: HookExecutionMode::Blocking,
                commit_policy: HookCommitPolicy::None,
            }
        );
    }

    // ── Containment class ────────────────────────────────────────────────────

    #[test]
    fn containment_classes_order_weakest_to_strongest() {
        assert!(ContainmentClass::Advisory < ContainmentClass::Scoped);
        assert!(ContainmentClass::Scoped < ContainmentClass::Sealed);
        assert!(ContainmentClass::Advisory < ContainmentClass::Sealed);
        assert_eq!(
            ContainmentClass::ALL.iter().copied().max(),
            Some(ContainmentClass::Sealed)
        );
    }

    #[test]
    fn containment_class_defaults_to_advisory() {
        assert_eq!(ContainmentClass::default(), ContainmentClass::Advisory);
    }

    #[test]
    fn containment_class_round_trips_through_string() {
        for class in ContainmentClass::ALL {
            assert_eq!(
                class.to_string().parse::<ContainmentClass>(),
                Ok(class),
                "{class} must survive Display -> FromStr"
            );
        }
        assert_eq!(ContainmentClass::Advisory.as_str(), "advisory");
        assert_eq!(ContainmentClass::Scoped.as_str(), "scoped");
        assert_eq!(ContainmentClass::Sealed.as_str(), "sealed");
    }

    #[test]
    fn containment_class_rejects_unknown_names() {
        let err = "paranoid".parse::<ContainmentClass>().unwrap_err();
        assert_eq!(err.value, "paranoid");
        assert_eq!(
            err.to_string(),
            "must be one of: advisory, scoped, sealed; got 'paranoid'"
        );
        // Case-sensitive by design: the wire form is lowercase everywhere.
        assert!("Sealed".parse::<ContainmentClass>().is_err());
    }

    #[test]
    fn containment_class_serializes_as_its_wire_name() {
        assert_eq!(
            serde_json::to_string(&ContainmentClass::Scoped).unwrap(),
            "\"scoped\""
        );
        assert_eq!(
            serde_yaml::from_str::<ContainmentClass>("sealed").unwrap(),
            ContainmentClass::Sealed
        );
    }

    #[test]
    fn effective_floor_is_advisory_when_nothing_is_declared() {
        assert_eq!(
            effective_containment_floor(None, None, None),
            ContainmentClass::Advisory
        );
    }

    #[test]
    fn effective_floor_takes_the_strongest_across_every_presence_combination() {
        use ContainmentClass::{Advisory, Scoped, Sealed};

        // Exactly one source present: that source wins outright.
        assert_eq!(
            effective_containment_floor(Some(Scoped), None, None),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(None, Some(Scoped), None),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(None, None, Some(Scoped)),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(Some(Sealed), None, None),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(None, Some(Sealed), None),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(None, None, Some(Sealed)),
            Sealed
        );

        // Two sources present: the stronger wins regardless of which slot holds it, and a
        // weaker source can never pull the stronger one down.
        assert_eq!(
            effective_containment_floor(Some(Advisory), Some(Sealed), None),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(Some(Sealed), Some(Advisory), None),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(Some(Advisory), None, Some(Scoped)),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(Some(Scoped), None, Some(Advisory)),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(None, Some(Advisory), Some(Scoped)),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(None, Some(Scoped), Some(Advisory)),
            Scoped
        );

        // All three present: max wins from every slot, and all-equal is a fixed point.
        assert_eq!(
            effective_containment_floor(Some(Sealed), Some(Advisory), Some(Scoped)),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(Some(Advisory), Some(Sealed), Some(Scoped)),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(Some(Advisory), Some(Scoped), Some(Sealed)),
            Sealed
        );
        assert_eq!(
            effective_containment_floor(Some(Scoped), Some(Scoped), Some(Scoped)),
            Scoped
        );
        assert_eq!(
            effective_containment_floor(Some(Advisory), Some(Advisory), Some(Advisory)),
            Advisory
        );
    }

    #[test]
    fn manifest_parses_each_declared_containment_class() {
        for class in ContainmentClass::ALL {
            let manifest = RuntimeManifest::from_yaml_str(&format!(
                r#"
name: cap
version: 0.0.1
capabilities:
  containment: {class}
"#
            ))
            .expect("a valid containment class parses");

            assert_eq!(
                manifest.capabilities.unwrap().containment,
                Some(class),
                "capabilities.containment: {class}"
            );
        }
    }

    #[test]
    fn manifest_without_containment_key_declares_nothing() {
        // Absent `capabilities:` block entirely.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
"#,
        )
        .unwrap();
        assert!(manifest.capabilities.is_none());

        // Present `capabilities:` block that simply omits the key: still None, never a
        // silently-defaulted Advisory floor stored as if the operator had written it.
        let manifest = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  network:
    allow:
      - https://api.example.com
"#,
        )
        .unwrap();
        assert_eq!(manifest.capabilities.unwrap().containment, None);
    }

    #[test]
    fn manifest_rejects_an_unknown_containment_class() {
        let err = RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
capabilities:
  containment: paranoid
"#,
        )
        .expect_err("an unknown containment class is rejected at parse time");

        match err {
            RuntimeManifestError::InvalidCapabilities { field, message } => {
                assert_eq!(field, "capabilities.containment");
                assert_eq!(
                    message,
                    "must be one of: advisory, scoped, sealed; got 'paranoid'"
                );
            }
            other => panic!("expected InvalidCapabilities, got {other:?}"),
        }
    }

    // ── exports.files ────────────────────────────────────────────────────────

    fn exports_manifest(block: &str) -> Result<RuntimeManifest, RuntimeManifestError> {
        RuntimeManifest::from_yaml_str(&format!("name: exporter\nversion: 0.0.1\n{block}"))
    }

    fn export_error(block: &str) -> (String, String) {
        match exports_manifest(block).expect_err("this exports block must not parse") {
            RuntimeManifestError::InvalidExports { field, message } => (field, message),
            other => panic!("expected InvalidExports, got {other:?}"),
        }
    }

    fn peer_fetch_error(block: &str) -> (String, String) {
        match exports_manifest(block).expect_err("this capabilities block must not parse") {
            RuntimeManifestError::InvalidCapabilities { field, message } => (field, message),
            other => panic!("expected InvalidCapabilities, got {other:?}"),
        }
    }

    // ── exports.peer_files ───────────────────────────────────────────────────

    #[test]
    fn parse_duration_secs_accepts_bare_integers_and_single_letter_suffixes() {
        assert_eq!(parse_duration_secs("90").unwrap(), 90);
        assert_eq!(parse_duration_secs("30s").unwrap(), 30);
        assert_eq!(parse_duration_secs("15m").unwrap(), 900);
        assert_eq!(parse_duration_secs("1h").unwrap(), 3600);
        assert_eq!(parse_duration_secs("14d").unwrap(), 14 * 86_400);
        assert_eq!(parse_duration_secs("  2h  ").unwrap(), 7200);
        assert_eq!(parse_duration_secs("0").unwrap(), 0);
    }

    /// Every spelling that is not the one accepted form, refused rather than guessed at: a
    /// manifest meaning 5 seconds and one meaning 5 minutes must not both parse.
    #[test]
    fn parse_duration_secs_rejects_every_other_spelling() {
        for input in [
            "",
            "  ",
            "5 minutes",
            "30S",
            "1H",
            "5min",
            "1.5h",
            "-1",
            "m",
            "h",
            "1h30m",
            "0x10",
            "30 s",
        ] {
            let error = parse_duration_secs(input)
                .expect_err(&format!("'{input}' must not parse as a duration"));
            assert!(
                error.contains(DURATION_ACCEPTED_FORM),
                "'{input}' must be refused with the accepted form; got: {error}"
            );
        }
    }

    #[test]
    fn parse_duration_secs_refuses_to_overflow() {
        assert!(parse_duration_secs("18446744073709551615h")
            .unwrap_err()
            .contains("overflows"));
    }

    #[test]
    fn peer_files_defaults_are_an_undeclared_ttl_and_ten_mebibytes() {
        let manifest = exports_manifest("exports:\n  peer_files:\n    root: out/\n")
            .expect("a minimal peer_files block parses");
        let peer = manifest.exports.unwrap().peer_files.unwrap();
        assert_eq!(peer.root, "out/");
        assert_eq!(peer.max_bytes, DEFAULT_PEER_FILES_MAX_BYTES);
        assert_eq!(peer.max_bytes, 10_485_760);
        // Undeclared, not defaulted: the two ephemerality cases answer an absent value
        // differently, and substituting here would erase the difference.
        assert_eq!(peer.max_ttl_secs, None);
        assert_eq!(peer.effective_max_ttl_secs(), DEFAULT_PEER_HANDLE_TTL_SECS);
        assert_eq!(peer.effective_max_ttl_secs(), 3600);
    }

    #[test]
    fn peer_files_accepts_every_duration_spelling() {
        for (declared, expected) in [("30m", 1800u64), ("2h", 7200), ("45s", 45), ("90", 90)] {
            let manifest = exports_manifest(&format!(
                "exports:\n  peer_files:\n    root: out/\n    max_ttl: {declared}\n"
            ))
            .expect("a declared max_ttl parses");
            let peer = manifest.exports.unwrap().peer_files.unwrap();
            assert_eq!(peer.max_ttl_secs, Some(expected), "max_ttl: {declared}");
            assert_eq!(peer.effective_max_ttl_secs(), expected);
        }
    }

    /// The two export blocks are separate authorisers: declaring one says nothing about the
    /// other, and both may be declared side by side over different subtrees.
    #[test]
    fn the_two_export_blocks_are_independent() {
        let both = exports_manifest(
            "exports:\n  files:\n    root: out/\n    mode: read-only\n  \
             peer_files:\n    root: out/handoff/\n",
        )
        .expect("both blocks parse together")
        .exports
        .unwrap();
        assert_eq!(both.files.unwrap().root, "out/");
        assert_eq!(both.peer_files.unwrap().root, "out/handoff/");

        let only_files =
            exports_manifest("exports:\n  files:\n    root: out/\n    mode: read-only\n")
                .unwrap()
                .exports
                .unwrap();
        assert!(only_files.peer_files.is_none());

        let only_peer = exports_manifest("exports:\n  peer_files:\n    root: out/\n")
            .unwrap()
            .exports
            .unwrap();
        assert!(only_peer.files.is_none());

        let neither = exports_manifest("exports: {}\n").unwrap().exports.unwrap();
        assert!(neither.files.is_none() && neither.peer_files.is_none());
    }

    #[test]
    fn peer_files_root_must_be_relative_and_inside_the_workdir() {
        for root in ["../out", "/etc", "out/../../etc"] {
            let (field, message) =
                export_error(&format!("exports:\n  peer_files:\n    root: {root}\n"));
            assert_eq!(field, "exports.peer_files.root", "root: {root}");
            assert!(
                message.contains(EXPORT_ROOT_ACCEPTED_FORM),
                "root: {root}; got: {message}"
            );
        }
    }

    #[test]
    fn peer_files_root_is_required() {
        let (field, message) = export_error("exports:\n  peer_files: {}\n");
        assert_eq!(field, "exports.peer_files.root");
        assert!(message.contains("is required"));
        assert!(message.contains(EXPORT_ROOT_ACCEPTED_FORM));
    }

    #[test]
    fn peer_files_max_ttl_states_its_accepted_form() {
        for declared in ["5 minutes", "30S", "5min", "1.5h", "abc"] {
            let (field, message) = export_error(&format!(
                "exports:\n  peer_files:\n    root: out/\n    max_ttl: \"{declared}\"\n"
            ));
            assert_eq!(field, "exports.peer_files.max_ttl", "max_ttl: {declared}");
            assert!(
                message.contains(DURATION_ACCEPTED_FORM),
                "max_ttl: {declared}; got: {message}"
            );
        }
    }

    /// A zero ceiling reads like a declared surface and refuses every mint. Better discovered at
    /// the line that wrote it than at the first `share-file` call.
    #[test]
    fn peer_files_max_ttl_rejects_zero() {
        let (field, message) =
            export_error("exports:\n  peer_files:\n    root: out/\n    max_ttl: 0\n");
        assert_eq!(field, "exports.peer_files.max_ttl");
        assert!(message.contains("greater than zero"));
    }

    #[test]
    fn peer_files_max_bytes_states_its_accepted_form() {
        let (field, message) =
            export_error("exports:\n  peer_files:\n    root: out/\n    max_bytes: 10MB\n");
        assert_eq!(field, "exports.peer_files.max_bytes");
        assert!(message.contains(BYTE_SIZE_ACCEPTED_FORM), "got: {message}");
    }

    #[test]
    fn peer_files_max_bytes_rejects_zero() {
        let (field, message) =
            export_error("exports:\n  peer_files:\n    root: out/\n    max_bytes: 0\n");
        assert_eq!(field, "exports.peer_files.max_bytes");
        assert!(message.contains("greater than zero"));
    }

    // ── capabilities.peer_fetch ──────────────────────────────────────────────

    #[test]
    fn peer_fetch_allow_carries_its_declared_destinations() {
        let manifest = exports_manifest(
            "capabilities:\n  peer_fetch:\n    allow:\n      - localhost:41234\n      \
             - reporting.internal\n      - https://gateway.example.com\n",
        )
        .expect("a peer_fetch block parses");
        assert_eq!(
            manifest.capabilities.unwrap().peer_fetch.unwrap().allow,
            vec![
                "localhost:41234".to_string(),
                "reporting.internal".to_string(),
                "https://gateway.example.com".to_string(),
            ]
        );
    }

    /// An empty list is a parse error rather than a silent deny: `allow: []` reads as a declared
    /// grant and would otherwise refuse every fetch with no line to point at.
    #[test]
    fn peer_fetch_allow_must_not_be_empty() {
        let (field, message) = peer_fetch_error("capabilities:\n  peer_fetch:\n    allow: []\n");
        assert_eq!(field, "capabilities.peer_fetch.allow");
        assert_eq!(message, PEER_FETCH_ALLOW_ACCEPTED_FORM);
    }

    #[test]
    fn peer_fetch_allow_refuses_an_entry_that_could_never_become_a_rule() {
        for entry in [
            "http://[not a host",
            "\"\"",
            "localhost:notaport",
            "http://example.com/path",
            "ftp://example.com",
        ] {
            let (field, message) = peer_fetch_error(&format!(
                "capabilities:\n  peer_fetch:\n    allow:\n      - {entry}\n"
            ));
            assert_eq!(field, "capabilities.peer_fetch.allow", "entry: {entry}");
            assert!(
                message.contains(PEER_FETCH_ALLOW_ACCEPTED_FORM),
                "entry: {entry}; got: {message}"
            );
        }
    }

    /// `peer_fetch` is a separate authoriser and never an alias: declaring it leaves
    /// `network.allow` exactly as the manifest wrote it, and vice versa.
    #[test]
    fn peer_fetch_does_not_widen_network_allow() {
        let capabilities = exports_manifest(
            "capabilities:\n  network:\n    allow:\n      - api.example.com\n  \
             peer_fetch:\n    allow:\n      - localhost:41234\n",
        )
        .unwrap()
        .capabilities
        .unwrap();
        assert_eq!(
            capabilities.network.unwrap().allow,
            vec!["api.example.com".to_string()]
        );
        assert_eq!(
            capabilities.peer_fetch.unwrap().allow,
            vec!["localhost:41234".to_string()]
        );
    }

    #[test]
    fn an_absent_peer_fetch_block_is_absent() {
        assert!(exports_manifest("capabilities: {}\n")
            .unwrap()
            .capabilities
            .unwrap()
            .peer_fetch
            .is_none());
    }

    #[test]
    fn parse_byte_size_accepts_bare_integers_and_binary_suffixes() {
        assert_eq!(parse_byte_size("0").unwrap(), 0);
        assert_eq!(parse_byte_size("4096").unwrap(), 4096);
        assert_eq!(parse_byte_size("1Ki").unwrap(), 1024);
        assert_eq!(parse_byte_size("10Mi").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_byte_size("2Gi").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_byte_size("  10Mi  ").unwrap(), 10 * 1024 * 1024);
    }

    /// Decimal suffixes are refused rather than guessed at: a manifest meaning 10 000 000 and one
    /// meaning 10 485 760 must not both parse.
    #[test]
    fn parse_byte_size_rejects_every_other_spelling() {
        for input in [
            "", "  ", "10MB", "10mi", "10KB", "10 Mi", "Mi", "-1", "1.5Mi", "10Ti", "0x10", "1e3",
            "1Ki1",
        ] {
            let error = parse_byte_size(input)
                .expect_err(&format!("'{input}' must not parse as a byte size"));
            assert!(
                error.contains(BYTE_SIZE_ACCEPTED_FORM),
                "'{input}' must be refused with the accepted form; got: {error}"
            );
        }
    }

    #[test]
    fn parse_byte_size_refuses_to_overflow() {
        assert!(parse_byte_size("18446744073709551615Gi")
            .unwrap_err()
            .contains("overflows"));
    }

    #[test]
    fn exports_files_defaults_max_bytes_to_ten_mebibytes() {
        let manifest =
            exports_manifest("exports:\n  files:\n    root: out/\n    mode: read-only\n")
                .expect("a minimal exports block parses");
        let files = manifest.exports.unwrap().files.unwrap();
        assert_eq!(files.root, "out/");
        assert_eq!(files.mode, ExportMode::ReadOnly);
        assert_eq!(files.max_bytes, DEFAULT_EXPORT_MAX_BYTES);
        assert_eq!(files.max_bytes, 10_485_760);
    }

    #[test]
    fn exports_files_accepts_both_byte_spellings() {
        for (declared, expected) in [("1Ki", 1024u64), ("4096", 4096)] {
            let manifest = exports_manifest(&format!(
                "exports:\n  files:\n    root: out/\n    mode: read-only\n    max_bytes: {declared}\n"
            ))
            .expect("a declared max_bytes parses");
            assert_eq!(
                manifest.exports.unwrap().files.unwrap().max_bytes,
                expected,
                "max_bytes: {declared}"
            );
        }
    }

    /// An `exports:` block with no `files:` is not a resource plane. It parses, and the plane
    /// stays undeclared — which is the deny case, not a defaulted-open one.
    #[test]
    fn an_exports_block_without_files_declares_no_plane() {
        let manifest = exports_manifest("exports: {}\n").expect("an empty exports block parses");
        assert!(manifest.exports.unwrap().files.is_none());
    }

    #[test]
    fn an_absent_exports_block_is_absent() {
        let manifest = exports_manifest("").expect("a manifest without exports parses");
        assert!(manifest.exports.is_none());
    }

    #[test]
    fn exports_files_root_must_be_relative_and_inside_the_workdir() {
        for root in ["../out", "/etc", "out/../../etc", "''"] {
            let (field, message) = export_error(&format!(
                "exports:\n  files:\n    root: {root}\n    mode: read-only\n"
            ));
            assert_eq!(field, "exports.files.root", "root: {root}");
            assert!(
                message.contains("must be a relative path inside the workdir"),
                "root: {root}; got: {message}"
            );
        }
    }

    #[test]
    fn exports_files_root_is_required() {
        let (field, message) = export_error("exports:\n  files:\n    mode: read-only\n");
        assert_eq!(field, "exports.files.root");
        assert_eq!(
            message,
            "is required and must be a relative path inside the workdir"
        );
    }

    #[test]
    fn exports_files_mode_is_required_and_read_only() {
        let (field, message) = export_error("exports:\n  files:\n    root: out/\n");
        assert_eq!(field, "exports.files.mode");
        assert_eq!(message, "is required and must be 'read-only'");

        let (field, message) =
            export_error("exports:\n  files:\n    root: out/\n    mode: read-write\n");
        assert_eq!(field, "exports.files.mode");
        assert_eq!(message, "'read-write' must be 'read-only'");
    }

    #[test]
    fn exports_files_max_bytes_states_its_accepted_form() {
        let (field, message) = export_error(
            "exports:\n  files:\n    root: out/\n    mode: read-only\n    max_bytes: 10MB\n",
        );
        assert_eq!(field, "exports.files.max_bytes");
        assert_eq!(
            message,
            "'10MB' must be a byte count, optionally suffixed Ki/Mi/Gi"
        );
    }

    /// A zero ceiling would refuse every read while reading like a declared export, so it is
    /// refused where it is written rather than at the first request.
    #[test]
    fn exports_files_max_bytes_rejects_zero() {
        let (field, message) = export_error(
            "exports:\n  files:\n    root: out/\n    mode: read-only\n    max_bytes: 0\n",
        );
        assert_eq!(field, "exports.files.max_bytes");
        assert!(message.contains("must be greater than zero"), "{message}");
    }

    /// A leading `./` is a relative path like any other and is not an escape.
    #[test]
    fn exports_files_root_accepts_a_current_dir_prefix() {
        let manifest =
            exports_manifest("exports:\n  files:\n    root: ./out\n    mode: read-only\n")
                .expect("./out is a legal root");
        assert_eq!(manifest.exports.unwrap().files.unwrap().root, "./out");
    }

    // ── Unrecognized manifest keys (W-SEC-019) ────────────────────────────────

    fn unknown_keys_of(yaml: &str) -> Vec<(String, String, Option<String>)> {
        RuntimeManifest::from_yaml_str(yaml)
            .expect("fixture must parse")
            .unknown_keys
            .into_iter()
            .map(|key| (key.key, key.block_path, key.nearest_known))
            .collect()
    }

    /// The typo the whole warning exists for: a hyphen where the key takes an underscore, named
    /// with the block that held it and with the key it should have been.
    #[test]
    fn a_typo_in_a_capability_block_is_captured_with_its_path_and_suggestion() {
        assert_eq!(
            unknown_keys_of(
                "name: cap\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    read-only:\n      - tests\n"
            ),
            vec![(
                "read-only".to_string(),
                "capabilities.filesystem".to_string(),
                Some("read_only".to_string())
            )]
        );
    }

    /// A manifest exercising every layer the walk descends through — capsule-wide capabilities,
    /// a per-artifact block, shell, network and an artifact entry — reports nothing. A warning
    /// that fires on correct input is noise, and noise is ignored.
    #[test]
    fn a_manifest_of_recognized_keys_reports_nothing() {
        assert!(unknown_keys_of(
            r#"
name: cap
version: 0.1.0
mur_version: "0.2.0"
artifacts:
  - name: notes-tool
    version: 0.1.0
    runtime: tool
    capabilities:
      filesystem:
        scope: notes
      network:
        allow:
          - example.com:443
capabilities:
  filesystem:
    read_only:
      - tests
      - bench/fixtures
    workdir_exec: false
  shell:
    allow:
      - git
      - python3
  network:
    allow:
      - example.com:443
    unix_sockets: false
"#
        )
        .is_empty());
    }

    /// A per-artifact capability block reports under its artifact's index, so an operator is not
    /// sent to the capsule-wide block for a key they wrote on one entry.
    #[test]
    fn a_per_artifact_capability_block_carries_its_index_in_the_path() {
        assert_eq!(
            unknown_keys_of(
                "name: cap\nversion: 0.1.0\nartifacts:\n  - name: t\n    version: 0.1.0\n    \
                 runtime: tool\n    capabilities:\n      shell:\n        allow:\n          - git\n        \
                 allwo:\n          - git\n"
            ),
            vec![(
                "allwo".to_string(),
                "artifacts[0].capabilities.shell".to_string(),
                Some("allow".to_string())
            )]
        );
    }

    /// A top-level key with no near neighbour reports an empty path and no suggestion — the two
    /// facts the emitter needs to word it as a possibly-newer key rather than a misspelling.
    #[test]
    fn an_unrelated_top_level_key_has_an_empty_path_and_no_suggestion() {
        assert_eq!(
            unknown_keys_of("name: cap\nversion: 0.1.0\nquantum_teleport: true\n"),
            vec![("quantum_teleport".to_string(), String::new(), None)]
        );
    }

    /// The overflow map captures the key without consuming the block: the rest of it still parses
    /// and still applies. A capsule with an unrecognized key launches.
    #[test]
    fn capturing_a_key_does_not_disturb_the_rest_of_its_block() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.1.0\ncapabilities:\n  filesystem:\n    scope: notes\n    \
             read-only:\n      - tests\n",
        )
        .expect("an unrecognized key never refuses a manifest");
        assert_eq!(
            manifest
                .capabilities
                .as_ref()
                .and_then(|caps| caps.filesystem.as_ref())
                .and_then(|fs| fs.scope.as_deref()),
            Some("notes")
        );
        assert_eq!(manifest.unknown_keys.len(), 1);
    }

    /// Every block on the way down is walked, not just the capability sub-blocks.
    #[test]
    fn nested_blocks_outside_capabilities_are_walked_too() {
        let keys = unknown_keys_of(
            "name: cap\nversion: 0.1.0\ncontext:\n  max_tokns: 100\nexports:\n  files:\n    \
             root: out\n    mode: read-only\n    max_byts: 10\n",
        );
        assert_eq!(
            keys,
            vec![
                (
                    "max_tokns".to_string(),
                    "context".to_string(),
                    Some("max_tokens".to_string())
                ),
                (
                    "max_byts".to_string(),
                    "exports.files".to_string(),
                    Some("max_bytes".to_string())
                ),
            ]
        );
    }

    /// Source-derived, so a `Raw*` struct added later cannot skip the overflow map, and a field
    /// added to one cannot skip its `KNOWN_KEYS`: the suggester would then never propose it.
    ///
    /// Reads this file's own text rather than a hand-written list for the same reason the
    /// `CapabilityPolicy` coverage test in `capsule-runtime` does — a list maintained by hand
    /// records what someone remembered, not what the code says.
    #[test]
    fn every_raw_struct_captures_unknown_keys_and_declares_its_own_field_names() {
        let source = include_str!("runtime_manifest.rs");
        let lines: Vec<&str> = source.lines().collect();

        let mut checked = 0usize;
        for (start, line) in lines.iter().enumerate() {
            let Some(name) = line
                .strip_prefix("struct Raw")
                .and_then(|rest| rest.strip_suffix(" {"))
            else {
                continue;
            };
            let name = format!("Raw{name}");
            let end = (start + 1..lines.len())
                .find(|index| lines[*index] == "}")
                .unwrap_or_else(|| panic!("{name} has no closing brace at column 0"));
            let body = &lines[start + 1..end];

            assert!(
                body.contains(&"    #[serde(flatten)]")
                    && body.contains(&"    unknown: UnknownKeys,"),
                "{name} does not carry the `#[serde(flatten)] unknown: UnknownKeys` overflow map, \
                 so a key it does not recognize would be dropped instead of reported"
            );

            let declared = serde_field_names(body);
            let known = known_keys_of(source, &name);
            assert_eq!(
                declared, known,
                "{name}'s KNOWN_KEYS does not match its serde field names; the W-SEC-019 \
                 suggester would propose the wrong set for that block"
            );
            checked += 1;
        }
        assert!(
            checked >= 30,
            "expected every Raw* struct to be scanned, found only {checked}"
        );
    }

    /// Source-derived for the same reason its sibling above is: a `Raw*`-typed field added to a
    /// block and left unwalked reports none of the keys inside that block, and reports nothing
    /// about having done so. The scan is keyed on the *parent's field*, not on the child's type,
    /// because `inference.driver` and `inference.provider` are the same type — a check keyed on
    /// the type would call `provider` covered because `driver` is walked.
    #[test]
    fn every_raw_struct_descends_into_the_blocks_it_owns() {
        let source = include_str!("runtime_manifest.rs");
        let lines: Vec<&str> = source.lines().collect();

        let mut structs = 0usize;
        let mut children = 0usize;
        for (start, line) in lines.iter().enumerate() {
            let Some(name) = line
                .strip_prefix("struct Raw")
                .and_then(|rest| rest.strip_suffix(" {"))
            else {
                continue;
            };
            let name = format!("Raw{name}");
            let end = (start + 1..lines.len())
                .find(|index| lines[*index] == "}")
                .unwrap_or_else(|| panic!("{name} has no closing brace at column 0"));
            let owned = block_typed_fields(&lines[start + 1..end]);
            structs += 1;

            if owned.is_empty() {
                continue;
            }
            let block = raw_block_impl_of(source, &name);
            assert!(
                block.contains("fn walk_children"),
                "{name} owns the nested block{} {}, but its `impl RawBlock` declares no \
                 `walk_children`, so every key written inside {} would go unreported",
                if owned.len() == 1 { "" } else { "s" },
                owned.join(", "),
                if owned.len() == 1 { "it" } else { "them" }
            );
            for field in &owned {
                assert!(
                    block.contains(&format!("self.{field}")),
                    "{name}'s `walk_children` never touches `self.{field}`, so every key written \
                     inside that block would go unreported — descend into it with \
                     `collect_block(.., &child_path(path, \"{field}\"), out)`"
                );
                children += 1;
            }
        }
        assert!(
            structs >= 30,
            "expected every Raw* struct to be scanned, found only {structs}"
        );
        assert!(
            children >= 31,
            "expected every nested block field to be scanned, found only {children}"
        );
    }

    /// The fields of one struct body whose declared type names another `Raw*` type, in
    /// declaration order. Comment and attribute lines are skipped, so a doc comment that mentions
    /// a `Raw*` type by name is not read as a field.
    fn block_typed_fields(body: &[&str]) -> Vec<String> {
        let mut fields = Vec::new();
        for line in body {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            let Some((field, declared)) = line
                .strip_prefix("    ")
                .and_then(|rest| rest.split_once(':'))
            else {
                continue;
            };
            if field.is_empty()
                || !field
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            {
                continue;
            }
            if declared.contains("Raw") {
                fields.push(field.to_string());
            }
        }
        fields
    }

    /// The text of `impl RawBlock for <name>`, cut at the next line that is exactly `}` at column
    /// 0 so one impl's body cannot be read as the next one's.
    fn raw_block_impl_of(source: &str, name: &str) -> String {
        let header = format!("impl RawBlock for {name} {{");
        let body = source
            .split_once(&header)
            .unwrap_or_else(|| panic!("{name} has no `impl RawBlock` block"))
            .1;
        body.split_once("\n}\n")
            .unwrap_or_else(|| panic!("{name}'s RawBlock impl has no closing brace at column 0"))
            .0
            .to_string()
    }

    /// The walk's own result, taken straight off a deserialized `RawRuntimeManifest`.
    ///
    /// Deliberately not routed through [`RuntimeManifest::from_yaml_str`]: that validates, and a
    /// fixture that names every block at once would have to satisfy every semantic rule as well
    /// to reach the walk. What is under test here is which blocks are descended into, which the
    /// validator has no part in.
    fn raw_unknown_keys(yaml: &str) -> Vec<(String, String)> {
        let raw: RawRuntimeManifest = serde_yaml::from_str(yaml).expect("fixture must parse");
        collect_unknown_keys(&raw)
            .into_iter()
            .map(|key| (key.key, key.block_path))
            .collect()
    }

    /// Every block the manifest type owns, each carrying one key no field of it claims, reported
    /// once and at its own dotted path. The blocks a later slice adds are held to this by
    /// `every_raw_struct_descends_into_the_blocks_it_owns`; this is what "descended into" means.
    #[test]
    fn every_block_reports_its_own_unknown_key_at_its_own_path() {
        let keys = raw_unknown_keys(
            r#"
name: every-block
version: 0.1.0
probe_root: 1
artifacts:
  - name: t
    version: 0.1.0
    runtime: tool
    probe_artifact: 1
    capabilities:
      probe_artifact_capabilities: 1
      shell:
        probe_artifact_shell: 1
capabilities:
  probe_capabilities: 1
  network:
    probe_network: 1
  peer_fetch:
    probe_peer_fetch: 1
  filesystem:
    probe_filesystem: 1
  shell:
    probe_shell: 1
    interpreter_runtime:
      - binary: python3
        probe_interpreter_runtime: 1
        dirs:
          - path: /usr/lib/python3
            probe_interpreter_runtime_dir: 1
    staged_runtime:
      - binary: rg
        probe_staged_runtime: 1
  spawn:
    probe_spawn: 1
  env:
    probe_env: 1
  limits:
    probe_limits: 1
  resources:
    probe_resources: 1
  state:
    probe_state: 1
  task_io:
    probe_task_io: 1
  conversation:
    probe_conversation: 1
inference:
  probe_inference: 1
  driver:
    probe_driver: 1
  provider:
    probe_provider: 1
  compaction:
    probe_compaction: 1
context:
  probe_context: 1
observability:
  probe_observability: 1
  eval:
    probe_eval: 1
    scorers:
      - type: exact_match
        probe_scorer: 1
trace:
  probe_trace: 1
network:
  probe_network_config: 1
lifecycle:
  probe_lifecycle: 1
exports:
  probe_exports: 1
  files:
    probe_files: 1
  peer_files:
    probe_peer_files: 1
"#,
        );

        let expected: Vec<(String, String)> = [
            ("probe_root", ""),
            ("probe_artifact", "artifacts[0]"),
            ("probe_artifact_capabilities", "artifacts[0].capabilities"),
            ("probe_artifact_shell", "artifacts[0].capabilities.shell"),
            ("probe_capabilities", "capabilities"),
            ("probe_network", "capabilities.network"),
            ("probe_peer_fetch", "capabilities.peer_fetch"),
            ("probe_filesystem", "capabilities.filesystem"),
            ("probe_shell", "capabilities.shell"),
            (
                "probe_interpreter_runtime",
                "capabilities.shell.interpreter_runtime[0]",
            ),
            (
                "probe_interpreter_runtime_dir",
                "capabilities.shell.interpreter_runtime[0].dirs[0]",
            ),
            (
                "probe_staged_runtime",
                "capabilities.shell.staged_runtime[0]",
            ),
            ("probe_spawn", "capabilities.spawn"),
            ("probe_env", "capabilities.env"),
            ("probe_limits", "capabilities.limits"),
            ("probe_resources", "capabilities.resources"),
            ("probe_state", "capabilities.state"),
            ("probe_task_io", "capabilities.task_io"),
            ("probe_conversation", "capabilities.conversation"),
            ("probe_inference", "inference"),
            ("probe_driver", "inference.driver"),
            ("probe_provider", "inference.provider"),
            ("probe_compaction", "inference.compaction"),
            ("probe_context", "context"),
            ("probe_observability", "observability"),
            ("probe_eval", "observability.eval"),
            ("probe_scorer", "observability.eval.scorers[0]"),
            ("probe_trace", "trace"),
            ("probe_network_config", "network"),
            ("probe_lifecycle", "lifecycle"),
            ("probe_exports", "exports"),
            ("probe_files", "exports.files"),
            ("probe_peer_files", "exports.peer_files"),
        ]
        .into_iter()
        .map(|(key, path)| (key.to_string(), path.to_string()))
        .collect();

        assert_eq!(keys, expected);
    }

    /// The names serde matches on for one struct body: field names in declaration order, with a
    /// `#[serde(rename = "...")]` field under its renamed spelling, and the overflow map itself
    /// excluded because it claims no name of its own.
    fn serde_field_names(body: &[&str]) -> Vec<String> {
        let mut names = Vec::new();
        let mut rename: Option<String> = None;
        for line in body {
            let trimmed = line.trim();
            if trimmed.starts_with("#[") {
                if let Some(rest) = trimmed.split("rename = \"").nth(1) {
                    if !trimmed.contains("rename_all") {
                        rename = rest.split('"').next().map(str::to_string);
                    }
                }
                continue;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            let Some(field) = line
                .strip_prefix("    ")
                .and_then(|rest| rest.split(':').next())
            else {
                continue;
            };
            if field.is_empty()
                || !field
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
            {
                continue;
            }
            if field == "unknown" {
                rename = None;
                continue;
            }
            names.push(rename.take().unwrap_or_else(|| field.to_string()));
        }
        names
    }

    /// The `KNOWN_KEYS` array `impl RawBlock for <name>` declares, read out of the source text so
    /// the assertion compares two independent readings of the same file rather than one value
    /// against itself.
    fn known_keys_of(source: &str, name: &str) -> Vec<String> {
        let header = format!("impl RawBlock for {name} {{");
        let body = source
            .split_once(&header)
            .unwrap_or_else(|| panic!("{name} has no `impl RawBlock` block"))
            .1;
        let array = body
            .split_once("KNOWN_KEYS: &'static [&'static str] = &[")
            .unwrap_or_else(|| panic!("{name}'s RawBlock impl declares no KNOWN_KEYS"))
            .1
            .split_once("];")
            .expect("KNOWN_KEYS array is unterminated")
            .0;
        array
            .split('"')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    }
}
