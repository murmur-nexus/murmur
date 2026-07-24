use std::{fs, net::IpAddr, path::Path};

use serde::Deserialize;
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
            other => Err(format!(
                "unknown commit_policy '{other}'; expected: none, replace-context, write-manifests"
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemCapabilities {
    pub scope: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCapabilities {
    pub allow: Vec<String>,
    pub strip_env: Option<Vec<String>>,
    pub baseline_env: Option<Vec<String>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub network: Option<NetworkCapabilities>,
    pub filesystem: Option<FilesystemCapabilities>,
    pub shell: Option<ShellCapabilities>,
    pub spawn: Option<SpawnCapabilities>,
    pub env: Option<EnvCapabilities>,
    pub limits: Option<ResourceLimits>,
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
struct RawNetworkCapabilities {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawFilesystemCapabilities {
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawShellCapabilities {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    strip_env: Option<Vec<String>>,
    #[serde(default)]
    baseline_env: Option<Vec<String>>,
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

                Ok(RuntimeArtifact {
                    name,
                    version,
                    runtime,
                    source,
                    local_source,
                    prompt_payload,
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
    });

    let filesystem = raw_caps
        .filesystem
        .map(|raw_filesystem| FilesystemCapabilities {
            scope: raw_filesystem.scope.and_then(|scope| {
                let trimmed = scope.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }),
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

            Ok(ShellCapabilities {
                allow: raw_shell.allow,
                strip_env: raw_shell.strip_env,
                baseline_env: raw_shell.baseline_env,
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

    Ok(Some(Capabilities {
        network,
        filesystem,
        shell,
        spawn,
        env,
        limits,
    }))
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
}
