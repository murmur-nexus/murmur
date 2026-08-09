use std::{fs, net::IpAddr, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::manifest_path::MANIFEST_FILENAME;

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
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            task_acceptance: TaskAcceptance::Single,
            after_task: AfterTask::Exit,
            queue_depth: 1,
            input_timeout_secs: None,
            conversation_mode: ConversationMode::Stateless,
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
            other => Err(format!(
                "unknown commit_policy '{other}'; expected: none, replace-context, write-manifests, \
                 reopen-task"
            )),
        })
        .transpose()?
        .unwrap_or(HookCommitPolicy::None);

    // Validation
    if execution_mode == HookExecutionMode::Async && commit_policy != HookCommitPolicy::None {
        return Err(format!(
            "async-with-commit not supported: execution_mode 'async' requires commit_policy 'none' \
             (got '{}')",
            match &commit_policy {
                HookCommitPolicy::None => "none",
                HookCommitPolicy::ReplaceContext => "replace-context",
                HookCommitPolicy::WriteManifests => "write-manifests",
                HookCommitPolicy::ReopenTask => "reopen-task",
            }
        ));
    }
    if binding == HookBinding::OnStage && execution_mode == HookExecutionMode::Async {
        return Err(
            "on-stage hooks must be blocking; execution_mode 'async' is not valid for binding \
             'on-stage'"
                .to_string(),
        );
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
    /// Pins the mur runtime version required by this capsule.
    /// Used by `mur deploy` to select the binary version to install on the VM,
    /// and by `mur run` to warn on version mismatch.
    /// If absent, the running mur binary's version is used.
    pub mur_version: Option<String>,
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
    /// Maximum times an `on-task-end` hook may reopen a single task (re-run its agent
    /// loop with injected feedback). Defaults to 1 when absent. `0` disables reopening
    /// entirely. Unlike `max_turns`, `0` is a valid explicit value. Reopening never
    /// grants turns past `max_turns` — the two budgets share one cumulative turn count.
    pub max_task_reopens: u32,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextConfig {
    /// Token budget for this session; None disables compaction.
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub otel_endpoint: Option<String>,
    pub eval: Option<EvalConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfig {
    /// When true, the raw tool output is captured in each tool_call trace event.
    /// Defaults to false because tool output can be large (file diffs, shell dumps).
    pub include_tool_output: bool,
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
    pub filesystem: Option<FilesystemCapabilities>,
    pub shell: Option<ShellCapabilities>,
    pub spawn: Option<SpawnCapabilities>,
    pub env: Option<EnvCapabilities>,
    pub limits: Option<ResourceLimits>,
    pub resources: Option<ResourceCapabilities>,
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
    #[error("{}: invalid artifact declaration at index {index}: {message}", MANIFEST_FILENAME)]
    InvalidArtifact { index: usize, message: String },
    #[error("{}: invalid inference config for '{field}': {message}", MANIFEST_FILENAME)]
    InvalidInferenceConfig { field: String, message: String },
    #[error("{}: invalid capability config for '{field}': {message}", MANIFEST_FILENAME)]
    InvalidCapabilities { field: String, message: String },
    #[error(
        "{}: inference.api_key references {reference} but the environment variable is not set",
        MANIFEST_FILENAME
    )]
    MissingInferenceEnvVar {
        field: String,
        reference: String,
        variable: String,
    },
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
    mur_version: Option<String>,
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
}

#[derive(Debug, Deserialize)]
struct RawNetworkConfig {
    internal_port: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct RawContextConfig {
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawObservabilityConfig {
    #[serde(default)]
    otel_endpoint: Option<String>,
    #[serde(default)]
    eval: Option<RawEvalConfig>,
}

#[derive(Debug, Deserialize)]
struct RawTraceConfig {
    #[serde(default)]
    include_tool_output: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawEvalConfig {
    #[serde(default)]
    dataset_id: Option<String>,
    #[serde(default)]
    scorers: Vec<RawScorerConfig>,
}

#[derive(Debug, Deserialize)]
struct RawScorerConfig {
    #[serde(rename = "type")]
    scorer_type: String,
    name: Option<String>,
    max: Option<u64>,
    expected: Option<Vec<String>>,
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
}

#[derive(Debug, Deserialize)]
struct RawCapabilities {
    #[serde(default)]
    network: Option<RawNetworkCapabilities>,
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
    /// Kept as a raw `String` rather than a `ContainmentClass` so a typo reports through
    /// `InvalidCapabilities` like every other bad capability value, instead of a bare serde
    /// "unknown variant" error attributed to the whole `capabilities:` block.
    #[serde(default)]
    containment: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSpawnCapabilities {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawEnvCapabilities {
    #[serde(default)]
    allow: Vec<String>,
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
}

#[derive(Debug, Deserialize)]
struct RawNetworkCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    unix_sockets: bool,
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
}

#[derive(Debug, Deserialize)]
struct RawInterpreterRuntimeGrant {
    #[serde(default)]
    binary: Option<String>,
    #[serde(default)]
    dirs: Vec<RawInterpreterRuntimeDir>,
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
    #[serde(default)]
    max_task_reopens: Option<u32>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawCompactionConfig {
    threshold: Option<f32>,
    model: Option<String>,
    system_prompt: Option<String>,
    system_prompt_file: Option<String>,
    dump_summaries: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawInferenceDriver {
    artifact: Option<String>,
    #[serde(default)]
    config: Option<serde_yaml::Value>,
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
                        parse_capabilities(Some(raw_caps))?
                    }
                };

                Ok(RuntimeArtifact {
                    name,
                    version,
                    runtime,
                    source,
                    local_source,
                    prompt_payload,
                    capabilities,
                })
            })
            .collect::<Result<Vec<_>, RuntimeManifestError>>()?;

        let capabilities = parse_capabilities(raw.capabilities)?;
        let inference = parse_inference(raw.inference)?;

        // Validate system_prompt_artifact: must name a declared artifact whose payload may be
        // bound as the system prompt (`prompt_payload`, defaulted from the role when absent).
        if let Some(ref sp_art) = inference.as_ref().and_then(|i| i.system_prompt_artifact.clone()) {
            let matching = artifacts
                .iter()
                .find(|a| &a.name == sp_art);
            match matching {
                None => {
                    return Err(RuntimeManifestError::InvalidInferenceConfig {
                        field: "inference.system_prompt_artifact".to_string(),
                        message: format!(
                            "artifact '{sp_art}' is not declared in artifacts:"
                        ),
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
        let trace = raw.trace.map(|raw_trace| TraceConfig {
            include_tool_output: raw_trace.include_tool_output.unwrap_or(false),
        });
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
            }
        });

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
            mur_version: raw.mur_version.filter(|s| !s.trim().is_empty()),
        })
    }
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

    let spawn = raw_caps
        .spawn
        .map(|raw_spawn| SpawnCapabilities { allow: raw_spawn.allow });

    let env = raw_caps.env.map(|raw_env| EnvCapabilities {
        allow: raw_env.allow,
    });

    let limits = raw_caps.limits.map(parse_resource_limits).transpose()?;

    let resources = raw_caps
        .resources
        .map(parse_resource_capabilities)
        .transpose()?;

    let containment = raw_caps
        .containment
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<ContainmentClass>()
                .map_err(|error| RuntimeManifestError::InvalidCapabilities {
                    field: "capabilities.containment".to_string(),
                    message: error.to_string(),
                })
        })
        .transpose()?;

    Ok(Some(Capabilities {
        network,
        filesystem,
        shell,
        spawn,
        env,
        limits,
        resources,
        containment,
    }))
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

        let binary = raw_grant.binary.filter(|b| !b.trim().is_empty()).ok_or_else(|| {
            RuntimeManifestError::InvalidCapabilities {
                field: format!("{base}.binary"),
                message: "must name a binary".to_string(),
            }
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

            let path = raw_dir.path.filter(|p| !p.trim().is_empty()).ok_or_else(|| {
                RuntimeManifestError::InvalidCapabilities {
                    field: format!("{dir_base}.path"),
                    message: "must name a host directory".to_string(),
                }
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

            let list_dir = raw_dir.list_dir.ok_or_else(|| {
                RuntimeManifestError::InvalidCapabilities {
                    field: format!("{dir_base}.list_dir"),
                    message: format!(
                        "'{path}' must set list_dir explicitly to true or false — \
                         enumerability is never inferred"
                    ),
                }
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

    // Unlike `max_turns`, `0` is a valid explicit value here: it disables reopening.
    // Absent defaults to 1 (one reopen permitted).
    let max_task_reopens = raw.max_task_reopens.unwrap_or(1);

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
                max_task_reopens,
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
            if raw.endpoint.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
                return Err(RuntimeManifestError::InvalidInferenceConfig {
                    field: "inference.endpoint".to_string(),
                    message: "is not valid with transport: process".to_string(),
                });
            }
            if raw.api_key.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false) {
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
                max_task_reopens,
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

    Ok(Some(ContextConfig {
        max_tokens: raw.max_tokens,
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
            let yaml = format!(
                "name: cap\nversion: 0.0.1\ncapabilities:\n  resources:\n    {field}: 0\n"
            );
            let error = RuntimeManifest::from_yaml_str(&yaml)
                .expect_err("a zero {field} must not parse");

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

        assert!(!manifest
            .capabilities
            .unwrap()
            .filesystem
            .unwrap()
            .workdir_exec);
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
        assert!(msg.contains("capabilities.shell.interpreter_runtime[0].binary"), "{msg}");
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
        let msg = interpreter_runtime_reject(
            "      - binary: python3\n        dirs: []\n",
        );
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
        assert!(msg.contains("capabilities.shell.staged_runtime[0].binary"), "{msg}");
        assert!(msg.contains("ruby"), "{msg}");
        assert!(msg.contains("capabilities.shell.allow"), "{msg}");
    }

    #[test]
    fn staged_runtime_rejects_binary_also_in_interpreter_runtime() {
        let msg = staged_runtime_reject(
            "      - binary: python3\n        source_path: /opt/py\n        pin: cpython-3.9.19\n",
            "    interpreter_runtime:\n      - binary: python3\n        dirs:\n          - path: /usr/lib/python3.9\n            list_dir: true\n",
        );
        assert!(msg.contains("capabilities.shell.staged_runtime[0].binary"), "{msg}");
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
        assert_eq!(inference.endpoint, Some("http://127.0.0.1:8080".to_string()));
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
            manifest.inference.unwrap().driver.as_ref().unwrap().artifact,
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
            manifest.inference.unwrap().driver.as_ref().unwrap().artifact,
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
        assert_eq!(artifact.source.as_deref(), Some("./skills/my-skill/skill.md"));
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

        assert_eq!(manifest.artifacts[0].source.as_deref(), Some("./skills/my-skill/"));
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
        assert_eq!(inference.model, "", "absent model resolves to empty (provider default)");
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
        assert!(msg.contains("inference.driver.artifact"), "error was: {msg}");
        assert!(msg.contains("not valid with transport: process"), "error was: {msg}");
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
        assert!(msg.contains("not valid with transport: process"), "error was: {msg}");
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
        assert!(msg.contains("not valid with transport: process"), "error was: {msg}");
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
        assert!(msg.contains("not valid with transport: process"), "error was: {msg}");
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

    /// The existing, unmodified async/commit validation must also reject
    /// `commit_policy: reopen-task` combined with `execution_mode: async`.
    #[test]
    fn hook_config_async_reopen_task_is_rejected() {
        let yaml =
            "name: gate\nruntime: hook\nexecution_mode: async\ncommit_policy: reopen-task\n";
        let err = parse_hook_config_from_yaml(yaml).unwrap_err();
        assert!(err.contains("async-with-commit"), "error was: {err}");
        assert!(err.contains("reopen-task"), "error was: {err}");
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
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.1.0\nartifacts: []\n",
        )
        .unwrap();
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

    #[test]
    fn inference_max_task_reopens_defaults_to_1() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_task_reopens, 1);
    }

    #[test]
    fn inference_max_task_reopens_explicit_value() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  max_task_reopens: 3\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_task_reopens, 3);
    }

    /// Unlike `max_turns`, `0` is a valid explicit value — it disables reopening.
    #[test]
    fn inference_max_task_reopens_zero_is_accepted() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  endpoint: http://127.0.0.1:8080\n  model: test-model\n  max_task_reopens: 0\n  driver:\n    artifact: murmur-driver-anthropic\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_task_reopens, 0);
    }

    /// `max_task_reopens` threads through the `transport: process` construction site too.
    #[test]
    fn inference_max_task_reopens_process_transport() {
        let manifest = RuntimeManifest::from_yaml_str(
            "name: cap\nversion: 0.0.1\nartifacts: []\ninference:\n  transport: process\n  command: claude\n  max_task_reopens: 2\n",
        ).unwrap();
        assert_eq!(manifest.inference.unwrap().max_task_reopens, 2);
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
        assert!(
            msg.contains("system_prompt_artifact"),
            "error was: {msg}"
        );
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
        assert!(
            msg.contains("system_prompt_artifact"),
            "error was: {msg}"
        );
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
        assert_eq!(effective_containment_floor(Some(Scoped), None, None), Scoped);
        assert_eq!(effective_containment_floor(None, Some(Scoped), None), Scoped);
        assert_eq!(effective_containment_floor(None, None, Some(Scoped)), Scoped);
        assert_eq!(effective_containment_floor(Some(Sealed), None, None), Sealed);
        assert_eq!(effective_containment_floor(None, Some(Sealed), None), Sealed);
        assert_eq!(effective_containment_floor(None, None, Some(Sealed)), Sealed);

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
}
