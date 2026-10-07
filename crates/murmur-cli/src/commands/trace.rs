use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Subcommand;
use serde::Deserialize;

use capsule_runtime::formation_launch::formation_member_roots;
use capsule_runtime::{formation::FORMATION_ID_PREFIX, FormationId};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::formation_trace::{
    fold_member_call, fold_member_call_busy, fold_member_call_start, formation_calls,
    search_formation, CallEnding, FormationCall, FormationSearch, MemberCallBusyLine,
    MemberCallLine, MemberCallRecord, MemberCallStartLine, RecordedMember,
};
use crate::session_address::{self, ses_entries, SessionQuery};

const E_TRC_001: &str = "E-TRC-001";
const E_TRC_002: &str = "E-TRC-002";

/// The `inference.stop_reason` value a provider reports for a turn it cut off at the output cap.
const MAX_TOKENS_STOP_REASON: &str = "max_tokens";

/// What a capped turn's rendered line ends on, so a fragment is visible without reading the raw
/// event. Names the manifest field that decided the cap.
const TRUNCATED_TURN_MARKER: &str = "  [truncated at inference.max_tokens]";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Debug, Subcommand)]
pub(crate) enum TraceCommand {
    /// Show a human-readable summary of a single trace file
    Show {
        /// Session ID (full or last 4+ chars as suffix), or omit for the most recent session.
        /// A literal path is also accepted for backward compatibility.
        session: Option<String>,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Print the recorded body behind one hash and nothing else: `system`, `tools`,
        /// `response`, `message:<i>` (each needing --turn), or a sha256 — full, or a
        /// prefix of 8+ characters naming exactly one hash in the trace.
        #[arg(long, value_name = "SELECTOR")]
        body: Option<String>,
        /// The turn whose hashes `--body system|tools|response|message:<i>` names.
        #[arg(long, value_name = "N")]
        turn: Option<u32>,
    },
    /// Compare two trace sessions side-by-side.
    ///
    /// Each argument accepts: a full session ID (ses_<32hex>), the last 4+ characters
    /// of a session ID as a suffix, an ordinal shortcut (@1 = most recent, @2 = second
    /// most recent, …), or a literal file path for backward compatibility.
    /// The most common diff: `mur trace diff @2 @1`
    Diff {
        /// Before session: full ID, suffix (4+ chars), @N ordinal, or literal path.
        /// Omit both arguments to diff the two most recent sessions (@2 vs @1).
        before: Option<String>,
        /// After session: full ID, suffix (4+ chars), @N ordinal, or literal path.
        after: Option<String>,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Show a turn-by-turn summary of what the agent did in a session
    Steps {
        /// Session ID (full or last 4+ chars as suffix), or omit for the most recent session.
        /// A literal path is also accepted for backward compatibility.
        session: Option<String>,
        /// Include a truncated summary of each tool's input
        #[arg(long)]
        verbose: bool,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Aggregate statistics across trace sessions.
    ///
    /// Without arguments, all sessions in the workdir are included.
    /// Use --last or --since to narrow the set, or pass explicit session IDs,
    /// suffixes (4+ chars), or @N ordinals to select specific sessions.
    Report {
        /// Sessions to include: full IDs, suffixes (4+ chars), or @N ordinals.
        /// When given, --last and --since are not allowed.
        sessions: Vec<String>,
        /// Limit to the N most recently created sessions.
        #[arg(long)]
        last: Option<usize>,
        /// Limit to sessions created within a duration, e.g. 2h, 30m, 1d.
        #[arg(long)]
        since: Option<String>,
        /// Directory containing session subdirectories (default: ./workdir)
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
}

// ── Event model ───────────────────────────────────────────────────────────────

/// The identity every runtime-written line carries, read from the same line as the event
/// itself so the payload structs below stay payload-only. `mur trace steps` follows
/// `parent_id` to rebuild the session → task → turn tree; a trace written before these
/// fields existed carries none of them and renders flat.
#[derive(Debug, Default, Deserialize)]
struct EventIdentity {
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    parent_id: Option<String>,
    /// The task a turn-level line belongs to. Used only as the fallback attribution when a
    /// line's `parent_id` names no event in this file.
    #[serde(default)]
    task_id: Option<String>,
}

/// One trace line: what this build understands of it, plus where it hangs in the tree.
struct TraceRecord {
    identity: EventIdentity,
    event: TraceEvent,
}

#[derive(Debug, Deserialize)]
struct SessionStartEvent {
    session_id: String,
    capsule_name: String,
    capsule_version: String,
    model: String,
    max_turns: u32,
    /// Capability categories the manifest granted anything under.
    #[serde(default)]
    capabilities: Vec<String>,
    /// Names of the tools offered to the model.
    #[serde(default)]
    tools_declared: Vec<String>,
    /// The strongest containment class asked for, and the class this host could enforce.
    /// Absent on a trace from a runtime predating the keys.
    #[serde(default)]
    containment_declared: Option<String>,
    #[serde(default)]
    containment_achieved: Option<String>,
    #[serde(default)]
    workdir_exec: Option<bool>,
    /// Where the host's permission to create an unprivileged user namespace came from;
    /// `null` off Linux.
    #[serde(default)]
    userns_grant: Option<String>,
    /// `"manifest"`, `"cli"` or `"none"` — where the system prompt in effect came from.
    #[serde(default)]
    system_prompt_source: Option<String>,
    /// The resolved prompt's hash, before the runtime prepends its `[Capsule]` block, and a
    /// blob name in its own right under `trace.capture: content`.
    #[serde(default)]
    system_prompt_sha256: Option<String>,
    /// The session that spawned this one, and the delegation that created it. Both absent from
    /// the record for a capsule nobody delegated.
    #[serde(default)]
    spawned_by: Option<String>,
    #[serde(default)]
    delegation_id: Option<String>,
    /// The formation this session is a member of. Absent for a session in no formation.
    #[serde(default)]
    formation_id: Option<String>,
    /// The staged artifacts whose `murmur.lock` pin a running capsule fetched. Absent on a trace
    /// from a runtime predating the key, which reads as none.
    #[serde(default)]
    runtime_artifacts: Vec<RuntimeArtifactEntry>,
}

/// One element of `session_start.runtime_artifacts`.
#[derive(Debug, Deserialize)]
struct RuntimeArtifactEntry {
    name: String,
    version: String,
    /// The session whose `manage.pull()` wrote the pin.
    #[serde(default)]
    session: String,
}

#[derive(Debug, Deserialize)]
struct InferenceEvent {
    turn: u32,
    decision: String,
    /// The provider's own stop reason for this turn. Absent on a record no driver response was
    /// parsed for — a hook's `run-inference` and the `process` transport.
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    /// `hook:<name>` when a hook produced this completion through `run-inference`. Absent on
    /// an agent-loop turn, which is how the Wire section and the divergence comparison — both
    /// of which pair records by turn — keep a hook's completion out of a turn's own record.
    #[serde(default)]
    origin: Option<String>,
    /// The driver choice that served an agent-loop turn, written only by a capsule that declares
    /// `inference.alternates`, beside `model`.
    #[serde(default)]
    driver_choice: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// The provider's own counts, each written only when the driver reported it.
    #[serde(default)]
    input_tokens_actual: Option<u64>,
    #[serde(default)]
    output_tokens_actual: Option<u64>,
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
    #[serde(default)]
    thinking_tokens: Option<u64>,
    /// The four wire hashes. All absent under `trace.capture: none`, and on a record the
    /// runtime did not build the request for.
    #[serde(default)]
    system_sha: Option<String>,
    #[serde(default)]
    tools_sha: Option<String>,
    #[serde(default)]
    response_sha: Option<String>,
    #[serde(default)]
    message_shas: Vec<String>,
    /// Why the call failed, on a record whose `decision` is `"error"`. Absent on every call that
    /// returned an answer.
    #[serde(default)]
    error_code: Option<String>,
    /// The failure in words, credentials already redacted by the runtime.
    #[serde(default)]
    error: Option<String>,
    /// The HTTP status the runtime's credential gateway received for the failed call.
    #[serde(default)]
    provider_status: Option<u16>,
}

impl InferenceEvent {
    /// The call's failure, when the record says it failed.
    fn failure(&self) -> Option<FailedCall> {
        self.error_code.as_ref().map(|code| FailedCall {
            error_code: code.clone(),
            error: self.error.clone().unwrap_or_default(),
            provider_status: self.provider_status,
        })
    }

    /// A failed call of the agent loop's own: recorded on its turn, never counted as one.
    fn is_failed_agent_loop_call(&self) -> bool {
        self.origin.is_none() && self.error_code.is_some()
    }

    /// Whether `mur trace steps` counts this record as one of the session's turns.
    fn is_steps_turn(&self) -> bool {
        self.origin.is_none() && !self.is_failed_agent_loop_call()
    }
}

/// One model call that failed, as its `inference` record names it.
#[derive(Debug, Clone)]
struct FailedCall {
    error_code: String,
    error: String,
    provider_status: Option<u16>,
}

impl FailedCall {
    /// `credential_rejected  HTTP 401`: the code, then the status when the provider answered.
    fn label(&self) -> String {
        match self.provider_status {
            Some(status) => format!("{}  HTTP {status}", self.error_code),
            None => self.error_code.clone(),
        }
    }
}

/// The most of a failed call's `error` the Turns section prints, in characters.
const FAILED_CALL_ERROR_CHARS: usize = 200;

#[derive(Debug, Deserialize)]
struct ToolCallEvent {
    #[serde(default)]
    turn: u32,
    tool_name: String,
    #[serde(default)]
    input: Option<serde_json::Value>,
    duration_ms: u64,
    status: String,
    /// The tool's self-declared state effect for this call (`read`/`mutate`), as recorded
    /// by the runtime from `tool-result.metadata`. Absent when the tool declared nothing.
    #[serde(default)]
    state_effect: Option<String>,
    /// The resource this call addressed, as declared by the tool and recorded verbatim by
    /// the runtime from `tool-result.metadata`. Absent when the tool declared nothing, in
    /// which case identity falls back to [`extract_tracked_path`].
    #[serde(default)]
    resource_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SkillCallEvent {
    #[serde(default)]
    turn: u32,
    skill_name: String,
    duration_ms: u64,
    status: String,
    /// The skill's `murmur.lock` origin (`operator` / `runtime`) and the trust class derived
    /// from it. Both empty on a trace from a runtime predating the keys, which renders as an
    /// operator skill always did.
    #[serde(default)]
    origin: String,
    #[serde(default)]
    trust: String,
}

impl SkillCallEvent {
    /// ` (runtime/untrusted)`-style annotation for a skill whose guidance reached the model
    /// fenced, and nothing for every other skill.
    fn untrusted_marker(&self) -> Option<String> {
        (capsule_runtime::TrustClass::parse(&self.trust)
            == Some(capsule_runtime::TrustClass::Untrusted))
        .then(|| format!("{}/{}", self.origin, self.trust))
    }
}

/// One `artifact_pulled` record: an artifact a running capsule fetched through `manage.pull()`.
#[derive(Debug, Deserialize)]
struct ArtifactPulledEvent {
    name: String,
    version: String,
    #[serde(default)]
    runtime: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    trust: String,
}

#[derive(Debug, Deserialize)]
struct ShellEvent {
    exit_code: i32,
    duration_ms: u64,
    /// The program that ran, as the runtime resolved it. Rendered on the shell row of the
    /// `steps` tree; absent on a trace from a runtime predating the key.
    #[serde(default)]
    binary: Option<String>,
}

/// A shell command that outran `lifecycle.shell_grace_secs` and moved to the background.
#[derive(Debug, Deserialize)]
struct ShellDetachedEvent {
    work_id: String,
    #[serde(default)]
    binary: Option<String>,
    grace_ms: u64,
}

/// That command finishing, carrying the same `work_id` and the id of the task it was enqueued as.
#[derive(Debug, Deserialize)]
struct ShellCompletedEvent {
    work_id: String,
    #[serde(default)]
    binary: Option<String>,
    exit_code: i32,
    duration_ms: u64,
    output_path: String,
    status: String,
}

/// That command still running when the session ended, so its result was lost.
#[derive(Debug, Deserialize)]
struct ShellAbandonedEvent {
    work_id: String,
    #[serde(default)]
    binary: Option<String>,
    running_ms: u64,
}

/// A demoted command a later resume found unaccounted for, appended to the trace of the session
/// that started it. Carries no exit code, duration or output path, because none exists.
#[derive(Debug, Deserialize)]
struct ShellLostEvent {
    work_id: String,
    #[serde(default)]
    binary: Option<String>,
    detached_at_ms: u64,
    reconciled_by_session: String,
}

#[derive(Debug, Deserialize)]
struct CompactionEvent {
    turn: u32,
    tokens_before: u64,
    tokens_after: u64,
}

/// Compaction was attempted and declined; the session continued over budget. Zero or more
/// per session, each naming the turn that tripped the threshold and why the context was left
/// alone.
#[derive(Debug, Deserialize)]
struct CompactionDeclinedEvent {
    turn: u32,
    tokens: u64,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct SessionEndEvent {
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    duration_ms: u64,
    exit_status: String,
}

#[derive(Debug, Deserialize)]
struct TaskStartEvent {
    task_id: String,
    /// Rendered on the task row of the `steps` tree. All five default to the empty string so a
    /// trace written before they existed still parses — an empty `context_id` is also what
    /// `mur run --resume` reports as a session it cannot continue.
    #[serde(default)]
    context_id: String,
    #[serde(default)]
    source: String,
    /// Why the task ran, and how far its content is trusted. Rendered together, inside the same
    /// parentheses as `source`, since neither answers the other's question.
    #[serde(default)]
    origin: String,
    #[serde(default)]
    trust: String,
    /// The queue lane the task waited in, which is what decided it ran when it did.
    #[serde(default)]
    lane: String,
    /// The delegation whose completion this task is. Absent on every task but a completion from
    /// a child this capsule launched, so it is rendered only when the record carries one.
    #[serde(default)]
    delegation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskEndEvent {
    task_id: String,
    exit_status: String,
    duration_ms: u64,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    /// Times an `on-task-end` hook reopened this task before it ended. Absent in
    /// pre-slice traces, so it defaults to 0.
    #[serde(default)]
    reopen_count: u32,
}

/// A person stopped this task, and what the runtime left running when it stopped.
///
/// `task_id` is not captured: `mur trace show` renders these in file order alongside the task
/// they belong to, and serde ignores unknown JSON fields, so omitting it is not a parse risk.
#[derive(Debug, Deserialize)]
struct TaskCanceledEvent {
    /// `"queued"`, `"turn"`, `"inference"`, `"input"` or `"delegation"`.
    phase: String,
    #[serde(default)]
    detached_work_ids: Vec<String>,
    #[serde(default)]
    delegation_ids: Vec<String>,
}

/// A queued task the session refused when it stopped taking work. It never started, so this is
/// its only record.
#[derive(Debug, Deserialize)]
struct TaskRejectedEvent {
    task_id: String,
    /// `"a2a"`, `"detached_shell"` or `"detached_lost"`.
    #[serde(default)]
    source: Option<String>,
    /// `"session_ended"` or `"session_stopped"`.
    cause: String,
}

/// A formation member's lifeline closed: its formation ended, and that began this session's
/// wind-down.
#[derive(Debug, Deserialize)]
struct FormationEndedEvent {
    formation_id: String,
}

/// A delegated child's spawner lifeline closed: the session that delegated to it ended, and that
/// began this session's wind-down. Both lineage keys are absent for a session launched with a
/// lifeline but no spawner handle.
#[derive(Debug, Deserialize)]
struct SpawnerEndedEvent {
    #[serde(default)]
    spawned_by: Option<String>,
    #[serde(default)]
    delegation_id: Option<String>,
}

/// A task attempt failed, and why.
#[derive(Debug, Deserialize)]
struct TaskFailedEvent {
    /// Absent for a run no task was started for.
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    turn: Option<u32>,
    cause: String,
    reason: String,
}

/// One `on-task-end` hook reopened the task. New event type; older `mur` binaries
/// route it through the `Unknown` catch-all, this one surfaces it.
///
/// `task_id` is not captured here: `mur trace show` never needs to distinguish
/// reopens by task, and serde ignores unknown JSON fields by default (no
/// `deny_unknown_fields` on this struct), so omitting it is not a parse risk.
#[derive(Debug, Deserialize)]
struct TaskReopenedEvent {
    hook_name: String,
    reason: String,
    reopen_number: u32,
    /// `"continued"` or `"restarted"`: what the next attempt starts from. Absent from a trace
    /// whose runtime did not record it.
    #[serde(default)]
    attempt_context: Option<String>,
    /// The turns the next attempt is handed. Absent on the same terms as `attempt_context`.
    #[serde(default)]
    turns_remaining: Option<u32>,
}

/// What an `on-task-start` hook proposed as context and what the runtime did with it. One
/// per task that had a seeding hook return something, including a rejection.
#[derive(Debug, Deserialize)]
struct ContextSeedEvent {
    hook_name: String,
    /// Tokens actually committed to the head of the context; `0` on a rejection.
    tokens: u64,
    proposed_tokens: u64,
    budget_tokens: u64,
    /// `"seeded"`, `"trimmed"`, `"compacted"` or `"rejected"`.
    outcome: String,
    /// Why nothing was committed. Written on `"rejected"` only.
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    message_ids: Vec<String>,
}

/// Retention deleted something. Rendered where it cannot be scrolled past: a session directory
/// that vanished with no explanation makes "where did my trace go" unanswerable, and this line is
/// the answer.
#[derive(Debug, Deserialize)]
struct RetentionEvent {
    /// `"sessions"` or `"records"`.
    store: String,
    /// `"max_sessions"`, `"max_age"` or `"max_messages"`.
    reason: String,
    removed: u32,
    #[serde(default)]
    targets: Vec<String>,
    /// Written for `"max_messages"` only.
    #[serde(default)]
    messages_dropped: Option<u64>,
}

/// A policy hook refused a call before it ran. Rendered where it cannot be scrolled past:
/// there is no `tool_call` or `shell` line for a denied call, so this record is the only
/// account of a call the model asked for and never got.
#[derive(Debug, Deserialize)]
struct CallDeniedEvent {
    turn: u32,
    /// `"on-shell"` or `"on-tool-call"`.
    event: String,
    hook_name: String,
    /// The resolved executable path for a shell call, the tool name otherwise.
    target: String,
    reason: String,
}

/// A tool call was refused before it ran because its input lacked a field the tool's
/// `input_schema` requires. A `tool_call` line with `status: "error"` sits beside it on both
/// transports; this record is the one that names the missing fields under every capture mode.
///
/// Only the fields its `steps` row reads; `turn`, `tool_call_id` and `reason` are on the record
/// too, and the tree places the row under its turn by `parent_id`.
#[derive(Debug, Deserialize)]
struct ToolInputRefusedEvent {
    tool_name: String,
    missing: Vec<String>,
}

/// A spend ceiling refused a driver call before it was sent. There is no `inference` line for a
/// refused call.
#[derive(Debug, Deserialize)]
struct SpendCeilingReachedEvent {
    /// `"session"` or `"machine"`.
    limit: String,
    ceiling: u64,
    used: u64,
    requested: u64,
    /// `hook:<name>` for a hook's `run-inference` call; absent for an agent-loop turn.
    #[serde(default)]
    origin: Option<String>,
}

/// The capsule manifest's own `capabilities.filesystem.read_only` rule refused a call before it
/// ran. Rendered where it cannot be scrolled past, and counted: a run where the capsule attempted
/// a protected write four times and was refused is a different result from one where it never
/// tried.
#[derive(Debug, Deserialize)]
struct ProtectedPathDeniedEvent {
    turn: u32,
    /// `"shell"` or `"tool"`.
    call: String,
    /// The resolved executable path for a shell call, the tool name otherwise.
    target: String,
    /// The resolved workdir-relative path.
    path: String,
    /// The declared `read_only` entry that covers `path`.
    rule: String,
    /// What identified the call as a write.
    signal: String,
}

/// A hook call that failed in a way the session survived. Rendered where it cannot be
/// scrolled past, because nothing else in the session says the hook did not run.
#[derive(Debug, Deserialize)]
struct HookDispatchErrorEvent {
    hook_name: String,
    /// The WIT lifecycle function the fault is attributed to, or `"drain"`.
    event: String,
    /// The unsupported `hook-output` arm, or the async failure that surfaced here.
    arm: String,
}

/// A warning a `transport: process` run raised. Rendered where it cannot be scrolled past: the
/// stderr line it was printed beside is long gone by the time a trace is read back.
#[derive(Debug, Deserialize)]
struct HarnessWarningEvent {
    code: String,
    message: String,
}

/// A turn the harness, or this runtime, ended in failure. Rendered for the same reason: it is
/// why the session has no result.
#[derive(Debug, Deserialize)]
struct HarnessFailedEvent {
    kind: String,
    message: String,
}

/// How a cancelled task's harness was stopped. Rendered so that "a kill was needed" — a harness
/// session that may not resume cleanly — is readable without reading the raw trace.
#[derive(Debug, Deserialize)]
struct HarnessInterruptEvent {
    method: String,
    delivered: bool,
    grace_ms: u64,
}

/// The resource-plane and peer-file records are rendered as counts by outcome, so `outcome`
/// is the only field five of the nine event types contribute.
#[derive(Debug, Deserialize)]
struct OutcomeEvent {
    outcome: String,
}

#[derive(Debug, Deserialize)]
struct A2aSendEvent {
    peer_url: String,
}

/// One `delegation_start` record: a child that was launched, whatever became of it.
#[derive(Debug, Deserialize)]
struct DelegationStartEvent {
    delegation_id: String,
    capsule: String,
    version: String,
    child_session_id: String,
    child_workdir: String,
}

/// One `delegation` record: how a delegation ended. A refusal carries no ids, because it made no
/// delegation.
#[derive(Debug, Deserialize)]
struct DelegationEvent {
    capsule: String,
    version: String,
    #[serde(default)]
    delegation_id: Option<String>,
    #[serde(default)]
    child_session_id: Option<String>,
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
}

/// One step of a plan's DAG as `plan_start` recorded it.
#[derive(Debug, Deserialize)]
struct PlanStepShape {
    step_id: String,
    /// `"tool"`, `"shell"` or `"capsule"`.
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    has_condition: bool,
}

#[derive(Debug, Deserialize)]
struct PlanStartEvent {
    plan_id: String,
    step_count: usize,
    #[serde(default)]
    steps: Vec<PlanStepShape>,
}

#[derive(Debug, Deserialize)]
struct PlanStepStartEvent {
    step_id: String,
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PlanStepEvent {
    plan_id: String,
    step_id: String,
    kind: String,
    /// `"success"`, `"failed"` or `"skipped"` — what the plan's own report settled this step as.
    status: String,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    error: Option<String>,
    /// The interpolated tool input. Absent for every kind but a tool step.
    #[serde(default)]
    input: Option<serde_json::Value>,
    /// What the tool declared about the call. Both absent when it declared nothing; both feed
    /// the same redundant-call analysis `tool_call` does.
    #[serde(default)]
    state_effect: Option<String>,
    #[serde(default)]
    resource_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlanEndEvent {
    plan_id: String,
    /// `"completed"` or `"failed"`.
    outcome: String,
    #[serde(default)]
    failed_step: Option<String>,
    #[serde(default)]
    steps_total: usize,
    #[serde(default)]
    steps_succeeded: usize,
    #[serde(default)]
    steps_failed: usize,
    #[serde(default)]
    steps_skipped: usize,
    #[serde(default)]
    duration_ms: u64,
    /// Why the run ended when the reason was not a step's own failure.
    #[serde(default)]
    reason: Option<String>,
}

/// A controller changed a setting or a secret over the control surface. A secret's line carries
/// no value.
#[derive(Debug, Deserialize)]
struct ControlChangeEvent {
    event_id: String,
    /// `"controller"` or `"agent"`.
    #[serde(default)]
    principal: Option<String>,
    /// `"setting"` or `"secret"`.
    kind: String,
    name: String,
    /// `"set"` or `"forget"`.
    action: String,
    #[serde(default)]
    previous: Option<serde_json::Value>,
    #[serde(default)]
    value: Option<serde_json::Value>,
    #[serde(default)]
    replaced: Option<bool>,
}

/// The control surface refused a request. `name` is absent when the caller did not authenticate.
#[derive(Debug, Deserialize)]
struct ControlRefusedEvent {
    #[serde(default)]
    principal: Option<String>,
    status: u16,
    reason: String,
    #[serde(default)]
    name: Option<String>,
}

/// The first inference call to use a setting value a controller set.
#[derive(Debug, Deserialize)]
struct ControlAppliedEvent {
    turn: u32,
    name: String,
    value: serde_json::Value,
    change_id: String,
}

/// The first inference call to send a tool array rebuilt after a mid-session install.
#[derive(Debug, Deserialize)]
struct ToolsRefreshedEvent {
    turn: u32,
    trigger: String,
    #[serde(default)]
    added: Vec<String>,
    #[serde(default)]
    removed: Vec<String>,
}

impl ToolsRefreshedEvent {
    /// `<trigger>  +<added>…  -<removed>…`, the part `show` and `steps` render alike.
    fn changes(&self) -> String {
        let added = self.added.iter().map(|name| format!("  +{name}"));
        let removed = self.removed.iter().map(|name| format!("  -{name}"));
        std::iter::once(self.trigger.clone())
            .chain(added)
            .chain(removed)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
enum TraceEvent {
    SessionStart(SessionStartEvent),
    Inference(InferenceEvent),
    ToolCall(ToolCallEvent),
    SkillCall(SkillCallEvent),
    Shell(ShellEvent),
    ShellDetached(ShellDetachedEvent),
    ShellCompleted(ShellCompletedEvent),
    ShellAbandoned(ShellAbandonedEvent),
    ShellLost(ShellLostEvent),
    Compaction(CompactionEvent),
    CompactionDeclined(CompactionDeclinedEvent),
    ContextSeed(ContextSeedEvent),
    SessionEnd(SessionEndEvent),
    TaskStart(TaskStartEvent),
    TaskEnd(TaskEndEvent),
    TaskReopened(TaskReopenedEvent),
    TaskCanceled(TaskCanceledEvent),
    TaskRejected(TaskRejectedEvent),
    TaskFailed(TaskFailedEvent),
    FormationEnded(FormationEndedEvent),
    SpawnerEnded(SpawnerEndedEvent),
    CallDenied(CallDeniedEvent),
    ProtectedPathDenied(ProtectedPathDeniedEvent),
    ToolInputRefused(ToolInputRefusedEvent),
    HookDispatchError(HookDispatchErrorEvent),
    Retention(RetentionEvent),
    ResourceList(OutcomeEvent),
    ResourceRead(OutcomeEvent),
    PeerHandleMint(OutcomeEvent),
    PeerHandleRedeem(OutcomeEvent),
    PeerFileFetch(OutcomeEvent),
    /// Counted, not detailed: the A2A section reports how many tasks arrived, and nothing
    /// on the record beyond its own existence is rendered.
    A2aTaskReceived,
    A2aSend(A2aSendEvent),
    DelegationStart(DelegationStartEvent),
    Delegation(DelegationEvent),
    MemberCallBusy(MemberCallBusyLine),
    MemberCallStart(MemberCallStartLine),
    MemberCall(MemberCallLine),
    PlanStart(PlanStartEvent),
    PlanStepStart(PlanStepStartEvent),
    PlanStep(PlanStepEvent),
    PlanEnd(PlanEndEvent),
    SpendCeilingReached(SpendCeilingReachedEvent),
    HarnessWarning(HarnessWarningEvent),
    HarnessFailed(HarnessFailedEvent),
    HarnessInterrupt(HarnessInterruptEvent),
    ControlChange(ControlChangeEvent),
    ControlRefused(ControlRefusedEvent),
    ControlApplied(ControlAppliedEvent),
    ToolsRefreshed(ToolsRefreshedEvent),
    ArtifactPulled(ArtifactPulledEvent),
    #[serde(other)]
    Unknown,
}

// ── Computed metrics ──────────────────────────────────────────────────────────

struct CompactionRecord {
    turn: u32,
    tokens_before: u64,
    tokens_after: u64,
}

/// One `compaction_declined` record, surfaced in `mur trace show`.
struct CompactionDeclinedRecord {
    turn: u32,
    tokens: u64,
    reason: String,
}

struct ToolCallRecord {
    turn: u32,
    tool_name: String,
    input: Option<serde_json::Value>,
    status: String,
    duration_ms: u64,
}

/// One `inference` line, kept whole because three sections read different parts of it: the
/// Tool calls breakdown wants the decision, the Wire section wants the hashes, and `--body`
/// resolves a selector against them.
struct InferenceRecord {
    turn: u32,
    decision: String,
    /// `hook:<name>` when a hook produced this completion. A hook's record never carries
    /// hashes, and is never a turn of the agent loop.
    origin: Option<String>,
    system_sha: Option<String>,
    tools_sha: Option<String>,
    response_sha: Option<String>,
    message_shas: Vec<String>,
    /// The driver choice and model that served an agent-loop turn, on a capsule that declares
    /// alternates. `None` on every other record.
    served_by: Option<(String, String)>,
    /// Why the call failed, on a record that says it did.
    failure: Option<FailedCall>,
}

impl InferenceRecord {
    fn from_event(e: InferenceEvent) -> Self {
        let served_by = e
            .driver_choice
            .clone()
            .map(|choice| (choice, e.model.clone().unwrap_or_default()));
        let failure = e.failure();
        InferenceRecord {
            turn: e.turn,
            decision: e.decision,
            origin: e.origin,
            system_sha: e.system_sha,
            tools_sha: e.tools_sha,
            response_sha: e.response_sha,
            message_shas: e.message_shas,
            served_by,
            failure,
        }
    }

    /// This record belongs to the agent loop's own turn sequence rather than to a hook.
    fn is_agent_loop(&self) -> bool {
        self.origin.is_none()
    }

    /// Whether the turn recorded any wire hash at all. `false` means the session ran under
    /// `trace.capture: none` — a different situation from a hash whose body was not stored.
    fn has_hashes(&self) -> bool {
        self.system_sha.is_some()
            || self.tools_sha.is_some()
            || self.response_sha.is_some()
            || !self.message_shas.is_empty()
    }
}

/// The provider's own token counts, summed over every turn that reported them. Absent when
/// no turn did — the runtime writes these keys only when the driver returned a `usage` block.
///
/// Each member is summed separately and stays absent until some turn reports it, so a transport
/// that carries only part of the set does not read as having reported the rest as zero. A
/// `transport: process` record carries no `*_actual` pair at all: the harness's own counts are
/// the session totals above, not a second measurement beside them.
#[derive(Default)]
struct ProviderTokens {
    input: Option<u64>,
    output: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    thinking: Option<u64>,
}

impl ProviderTokens {
    /// Add one record's counts, leaving members it did not report untouched.
    fn add(&mut self, event: &InferenceEvent) {
        for (total, reported) in [
            (&mut self.input, event.input_tokens_actual),
            (&mut self.output, event.output_tokens_actual),
            (&mut self.cached, event.cached_tokens),
            (&mut self.cache_write, event.cache_write_tokens),
            (&mut self.thinking, event.thinking_tokens),
        ] {
            if let Some(reported) = reported {
                *total = Some(total.unwrap_or(0) + reported);
            }
        }
    }

    /// The reported members, each as `<label> <count>`, in a fixed order.
    fn parts(&self) -> Vec<String> {
        [
            ("in", self.input),
            ("out", self.output),
            ("cached", self.cached),
            ("cache write", self.cache_write),
            ("thinking", self.thinking),
        ]
        .into_iter()
        .filter_map(|(label, total)| total.map(|total| format!("{label} {}", fmt_thousands(total))))
        .collect()
    }
}

/// One `context_seed` record: what a seeding hook proposed, and what survived the budget.
struct ContextSeedRecord {
    hook_name: String,
    tokens: u64,
    proposed_tokens: u64,
    budget_tokens: u64,
    outcome: String,
    reason: Option<String>,
    message_ids: Vec<String>,
}

/// One `harness_warning`, `harness_failed` or `harness_interrupt` record, already rendered as the
/// single line `mur trace show` prints for it.
struct HarnessLine(String);

/// One `hook_dispatch_error` record — a hook that failed without failing the session.
struct HookFailureRecord {
    hook_name: String,
    event: String,
    arm: String,
}

/// Outcome tallies for one plane's records, in outcome order so the rendering is stable.
type OutcomeCounts = BTreeMap<String, u32>;

struct SkillCallRecord {
    turn: u32,
    skill_name: String,
    status: String,
    duration_ms: u64,
    /// `runtime/untrusted` for a runtime-origin skill, `None` for every other.
    untrusted: Option<String>,
}

/// A call that re-observed a resource already observed earlier in the session with no
/// intervening call that changed it. "Observed" vs. "changed" is decided entirely by each
/// call's self-declared `state_effect` (see [`StateEffect`]); *which* resource was addressed
/// comes from [`resolve_resource_identity`]. The detector recognizes no tool or operation by
/// name, so a brand-new tool is handled correctly the moment its author declares its effects.
struct RedundantCallRecord {
    site: CallSite,
    /// The tool that made the call. `None` for a plan step, whose record names the step rather
    /// than the tool behind it — [`CallSite::label`] already identifies it.
    tool_name: Option<String>,
    /// The resolved resource identity — a tool-declared `resource_id` when present,
    /// otherwise a path sniffed from the call's input. Rendered verbatim, unlabeled.
    resource_id: String,
    /// The earlier site whose read of the same resource this call duplicates.
    prior: CallSite,
}

/// Where a call that touched a resource happened: a turn of the agent loop, or a step of a plan
/// run. Both are scored against one resource history, so a plan step that re-reads what an agent
/// turn already read is flagged, and the other way round.
#[derive(Clone)]
enum CallSite {
    Turn(u32),
    PlanStep { plan_id: String, step_id: String },
}

impl CallSite {
    fn label(&self) -> String {
        match self {
            CallSite::Turn(turn) => format!("turn {turn}"),
            CallSite::PlanStep { plan_id, step_id } => format!("plan {plan_id}/{step_id}"),
        }
    }
}

/// One plan run, as its `plan_start`, per-step lines and `plan_end` join up on the plan id.
///
/// A run with no `plan_end` is one this trace never saw finish; a step line naming a plan with no
/// `plan_start` opens a run of its own rather than being dropped.
struct PlanRunRecord {
    plan_id: String,
    /// How many steps the plan declared, from `plan_start`. `0` for a run with none.
    step_count: usize,
    /// The DAG as authored, from `plan_start`. Rendered in this order, so a step that never ran
    /// is still listed with what it would have waited on.
    declared: Vec<PlanStepShape>,
    /// Every settled step, in the order they settled.
    steps: Vec<PlanStepRecord>,
    /// `None` while a run is still in flight — the shape a process that died mid-plan leaves.
    outcome: Option<String>,
    failed_step: Option<String>,
    steps_succeeded: usize,
    steps_failed: usize,
    steps_skipped: usize,
    duration_ms: u64,
    reason: Option<String>,
}

/// One settled step of a plan run, surfaced in `mur trace show`.
struct PlanStepRecord {
    step_id: String,
    kind: String,
    status: String,
    attempts: u32,
    duration_ms: u64,
    error: Option<String>,
}

struct TraceMetrics {
    session_id: String,
    capsule_name: String,
    capsule_version: String,
    model: String,
    max_turns: u32,
    capabilities: Vec<String>,
    tools_declared: Vec<String>,
    containment_declared: Option<String>,
    containment_achieved: Option<String>,
    workdir_exec: Option<bool>,
    userns_grant: Option<String>,
    system_prompt_source: Option<String>,
    system_prompt_sha256: Option<String>,
    exit_status: String,
    duration_ms: u64,
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    tool_ok: u32,
    tool_error: u32,
    tool_latencies_ms: Vec<u64>,
    tool_call_records: Vec<ToolCallRecord>,
    redundant_calls: Vec<RedundantCallRecord>,
    inference_records: Vec<InferenceRecord>,
    provider_tokens: Option<ProviderTokens>,
    total_shell_calls: u32,
    shell_exit_codes: HashMap<i32, u32>,
    shell_latencies_ms: Vec<u64>,
    skill_ok: u32,
    skill_error: u32,
    skill_latencies_ms: Vec<u64>,
    skill_call_records: Vec<SkillCallRecord>,
    compaction: Option<CompactionRecord>,
    /// Every `compaction_declined` record, in file order. A decline leaves the session running
    /// over budget, so all of them are kept rather than just the last.
    compactions_declined: Vec<CompactionDeclinedRecord>,
    /// Every `tools_refreshed` record, in file order: each is a turn whose `tools` hash changed.
    tool_refreshes: Vec<ToolsRefreshedEvent>,
    /// Every `task_canceled` record, in file order — one per task a person stopped.
    cancels: Vec<CancelRecord>,
    /// Every `task_rejected` record, in file order — one per queued task the session refused.
    rejections: Vec<TaskRejectedEvent>,
    /// `formation_ended.formation_id`, when the session wound down because its formation ended.
    formation_ended: Option<String>,
    /// The `spawner_ended` record, when the session wound down because its spawner ended.
    spawner_ended: Option<SpawnerEndedEvent>,
    /// Every `task_failed` record, in file order — one per failing attempt.
    failures: Vec<TaskFailedEvent>,
    /// Every `task_reopened` record, in file order — one per `on-task-end` reopen.
    reopens: Vec<ReopenRecord>,
    /// Every `context_seed` record, in file order — one per seeded task.
    context_seeds: Vec<ContextSeedRecord>,
    /// Every `call_denied` record, in file order — one per call a policy hook refused.
    denials: Vec<DenialRecord>,
    /// Every `protected_path_denied` record, in file order — one per call the manifest's own
    /// `capabilities.filesystem.read_only` refused.
    protected_path_denials: Vec<ProtectedPathDenialRecord>,
    /// Every `hook_dispatch_error` record, in file order.
    hook_failures: Vec<HookFailureRecord>,
    /// The `harness_warning`, `harness_failed` and `harness_interrupt` lines of a
    /// `transport: process` run, in the order they were written.
    harness_lines: Vec<HarnessLine>,
    /// Every `retention` record, in file order — one per (store, reason) pair that removed
    /// anything at this session's launch.
    retentions: Vec<RetentionRecord>,
    resource_lists: OutcomeCounts,
    resource_reads: OutcomeCounts,
    peer_mints: OutcomeCounts,
    peer_redeems: OutcomeCounts,
    peer_fetches: OutcomeCounts,
    a2a_tasks_received: u32,
    /// The peer URL of every `a2a_send`, in file order.
    a2a_sends: Vec<String>,
    /// `session_start.runtime_artifacts`, in file order.
    runtime_artifacts: Vec<RuntimeArtifactEntry>,
    /// Every `artifact_pulled` record, in file order.
    artifacts_pulled: Vec<ArtifactPulledEvent>,
    /// The session that spawned this one, and the delegation that created it. Both `None` for a
    /// capsule nobody delegated.
    spawned_by: Option<String>,
    spawned_by_delegation: Option<String>,
    /// `session_start.formation_id`: the formation this session is a member of, if any.
    formation_id: Option<String>,
    /// Every delegation this session made, in the order it started them.
    delegations: Vec<DelegationRecord>,
    /// Every `call-member` call this session made, in the order it made them.
    member_calls: Vec<MemberCallRecord>,
    /// Every plan run this trace records, in the order they started.
    plan_runs: Vec<PlanRunRecord>,
    /// Every `control_change`, in file order, joined to the `control_applied` that names it.
    control_changes: Vec<ControlChangeRecord>,
    /// `control_refused` records, counted by status.
    control_refusals: OutcomeCounts,
}

/// One accepted control change, and the turn that first used it when a `control_applied` says so.
struct ControlChangeRecord {
    change: ControlChangeEvent,
    applied_turn: Option<u32>,
}

/// One line for a control change: what changed and, for a setting whose `applied` turn is known,
/// the turn that first used it.
fn control_change_line(c: &ControlChangeEvent, applied: Option<Option<u32>>) -> String {
    // A driver choice is a name, printed bare; any other value as JSON.
    let value = |value: &Option<serde_json::Value>| match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => "?".to_string(),
    };
    match (c.kind.as_str(), c.action.as_str()) {
        ("setting", _) => format!(
            "setting {}  {} \u{2192} {}{}{}",
            c.name,
            value(&c.previous),
            value(&c.value),
            match applied {
                Some(Some(turn)) => format!("  applied from turn {turn}"),
                Some(None) => "  not yet used by an inference call".to_string(),
                None => String::new(),
            },
            c.principal
                .as_deref()
                .map(|principal| format!("  by {principal}"))
                .unwrap_or_default()
        ),
        (_, "forget") => format!("secret {}  forgotten", c.name),
        _ => format!(
            "secret {}  set ({})",
            c.name,
            if c.replaced == Some(true) {
                "replaced"
            } else {
                "new"
            }
        ),
    }
}

/// One delegation this session made, as the two lines that record it join up.
///
/// Joined on the `dlg_` id, and never across files: a `delegation_start` with no terminal line is
/// a delegation this trace never saw end, and a terminal line with no start is one the daemon
/// refused. Both are rendered as what they are.
struct DelegationRecord {
    /// `None` only for a refusal, which named no delegation.
    delegation_id: Option<String>,
    capsule: String,
    version: String,
    child_session_id: Option<String>,
    /// Where the child's own trace is, relative to this capsule's accessible workdir. `None` for
    /// a delegation with no `delegation_start`.
    child_workdir: Option<String>,
    /// `None` while a delegation is still in flight — the shape a parent that died mid-delegation
    /// leaves behind.
    outcome: Option<String>,
    reason: Option<String>,
}

/// One row of `mur trace show`'s Member calls section: the call id, the member, the callee's
/// task, and the status with its duration.
fn member_call_show_row(call: &MemberCallRecord) -> String {
    let status = match &call.status {
        Some(status) => format!(
            "{status} in {}{}",
            fmt_dur(call.duration_ms),
            if call.delivered {
                ""
            } else {
                ", not delivered"
            }
        ),
        None => "outstanding".to_string(),
    };
    let mut row = format!(
        "{}  {}  {}  {status}",
        call.call_id,
        call.member,
        call.member_task_id.as_deref().unwrap_or("(not started)")
    );
    if call.busy_offers > 0 {
        row.push_str(&format!("  {}", busy_note(call.busy_offers)));
    }
    row
}

/// How a call row says its callee turned `offers` offers away busy.
fn busy_note(offers: u32) -> String {
    format!("busy ×{offers}")
}

/// One `retention` trace record, surfaced in `mur trace show`.
struct RetentionRecord {
    store: String,
    reason: String,
    removed: u32,
    targets: Vec<String>,
    messages_dropped: Option<u64>,
}

/// One `task_rejected` record's row under `mur trace show`'s `Rejected` section. A record
/// written without a `source` shows `source unknown`.
fn rejected_show_row(r: &TaskRejectedEvent) -> String {
    format!(
        "task_rejected  {}  {}  source {}",
        r.task_id,
        r.cause,
        r.source.as_deref().unwrap_or("unknown")
    )
}

/// The `formation_ended` record's row under `mur trace show`'s `Formation ended` section.
fn formation_ended_show_row(formation_id: &str) -> String {
    format!("formation_ended  {formation_id}  the formation ended; this session wound down")
}

/// The `spawner_ended` record's row under `mur trace show`'s `Spawner ended` section, with `-`
/// for an absent lineage key.
fn spawner_ended_show_row(e: &SpawnerEndedEvent) -> String {
    format!(
        "spawner_ended  {}  {}  the session that delegated to this one ended; this session wound \
         down",
        e.spawned_by.as_deref().unwrap_or("-"),
        e.delegation_id.as_deref().unwrap_or("-")
    )
}

/// The Session block's line naming every runtime-origin artifact and the session that pulled
/// it, or `None` when the session staged none.
fn runtime_pins_line(pins: &[RuntimeArtifactEntry]) -> Option<String> {
    if pins.is_empty() {
        return None;
    }
    let pins: Vec<String> = pins
        .iter()
        .map(|pin| format!("{}@{} (pulled by {})", pin.name, pin.version, pin.session))
        .collect();
    Some(format!("{:<11} {}", "runtime pins:", pins.join(", ")))
}

/// One skill call's entry on its turn's row under `mur trace show`'s `Skill calls` section. A
/// runtime-origin skill's entry ends in its `origin/trust`; every other entry is unannotated.
fn skill_call_show_entry(rec: &SkillCallRecord) -> String {
    let icon = if rec.status == "ok" { "✓" } else { "✗" };
    let untrusted = rec
        .untrusted
        .as_deref()
        .map(|marker| format!(" {marker}"))
        .unwrap_or_default();
    format!(
        "{} {} {}{}",
        rec.skill_name,
        fmt_dur(rec.duration_ms),
        icon,
        untrusted
    )
}

/// One `artifact_pulled` record's row under `mur trace show`'s `Pulled at runtime` section.
fn artifact_pulled_show_row(e: &ArtifactPulledEvent) -> String {
    format!(
        "  {}@{}  {}  {}/{}",
        e.name, e.version, e.runtime, e.origin, e.trust
    )
}

/// What a `task_canceled` record names, as `mur trace show` prints it. A detached shell command
/// keeps running after the cancel. A delegation is named as in flight rather than as running: a
/// current runtime's cancelled task ends it, and its terminal `delegation` record follows, while a
/// trace from an older runtime left it running.
fn cancel_residue(detached_work_ids: &[String], delegation_ids: &[String]) -> String {
    let mut named = Vec::new();
    if !detached_work_ids.is_empty() {
        named.push(format!("still running: {}", detached_work_ids.join(", ")));
    }
    if !delegation_ids.is_empty() {
        named.push(format!(
            "delegations in flight: {}",
            delegation_ids.join(", ")
        ));
    }
    if named.is_empty() {
        "nothing left running".to_string()
    } else {
        named.join("; ")
    }
}

/// One `task_canceled` trace record, surfaced in `mur trace show`.
struct CancelRecord {
    phase: String,
    /// Detached shell commands, which keep running after the cancel.
    detached_work_ids: Vec<String>,
    /// Delegations in flight when the task stopped.
    delegation_ids: Vec<String>,
}

/// One `task_reopened` trace record, surfaced in `mur trace show`.
struct ReopenRecord {
    reopen_number: u32,
    hook_name: String,
    reason: String,
    attempt_context: Option<String>,
    turns_remaining: Option<u32>,
}

impl ReopenRecord {
    /// What the next attempt starts from and how many turns it has, e.g. `continued, 7 turns
    /// left`, or `None` for a record that carries neither.
    fn next_attempt(&self) -> Option<String> {
        let turns = self.turns_remaining.map(|n| match n {
            1 => "1 turn left".to_string(),
            n => format!("{n} turns left"),
        });
        let parts: Vec<String> = self.attempt_context.iter().cloned().chain(turns).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// One `call_denied` trace record, surfaced in `mur trace show`.
struct DenialRecord {
    turn: u32,
    event: String,
    hook_name: String,
    target: String,
    reason: String,
}

/// One `protected_path_denied` trace record, surfaced in `mur trace show`.
struct ProtectedPathDenialRecord {
    turn: u32,
    call: String,
    target: String,
    path: String,
    rule: String,
    signal: String,
}

impl TraceMetrics {
    fn tool_success_rate(&self) -> Option<f64> {
        if self.total_tool_calls == 0 {
            return None;
        }
        Some(100.0 * self.tool_ok as f64 / self.total_tool_calls as f64)
    }

    fn avg_tool_latency_ms(&self) -> Option<f64> {
        if self.tool_latencies_ms.is_empty() {
            return None;
        }
        Some(
            self.tool_latencies_ms.iter().sum::<u64>() as f64 / self.tool_latencies_ms.len() as f64,
        )
    }

    fn total_skill_calls(&self) -> u32 {
        self.skill_ok + self.skill_error
    }

    fn skill_success_rate(&self) -> Option<f64> {
        let total = self.total_skill_calls();
        if total == 0 {
            return None;
        }
        Some(100.0 * self.skill_ok as f64 / total as f64)
    }

    fn avg_skill_latency_ms(&self) -> Option<f64> {
        if self.skill_latencies_ms.is_empty() {
            return None;
        }
        Some(
            self.skill_latencies_ms.iter().sum::<u64>() as f64
                / self.skill_latencies_ms.len() as f64,
        )
    }

    fn avg_shell_latency_ms(&self) -> Option<f64> {
        if self.shell_latencies_ms.is_empty() {
            return None;
        }
        Some(
            self.shell_latencies_ms.iter().sum::<u64>() as f64
                / self.shell_latencies_ms.len() as f64,
        )
    }

    fn avg_input_per_turn(&self) -> Option<f64> {
        if self.total_turns == 0 {
            return None;
        }
        Some(self.total_input_tokens as f64 / self.total_turns as f64)
    }

    fn avg_output_per_turn(&self) -> Option<f64> {
        if self.total_turns == 0 {
            return None;
        }
        Some(self.total_output_tokens as f64 / self.total_turns as f64)
    }
}

struct TaskMetrics {
    task_id: String,
    exit_status: String,
    duration_ms: u64,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    reopen_count: u32,
}

// ── Parsing ───────────────────────────────────────────────────────────────────

/// The context id the first task of `session_dir` ran under, read off its `trace.jsonl`.
///
/// `mur run --resume` turns a session address into the `--context` value it would otherwise have
/// been given by hand, and this is the lookup: `task_start` is where the runtime already records
/// the context, so the resolution needs no new trace field and no index. `None` means the trace
/// holds no `task_start` carrying one — a session that never ran a task, or one written by a
/// runtime that predates the key, which reaches `task_start.context_id` as the empty string.
pub(crate) fn first_task_context_id(session_dir: &Path) -> Result<Option<String>, CliError> {
    let path = session_dir.join("trace.jsonl");
    let file = fs::File::open(&path).map_err(|e| trace_read_error(&path, &e))?;

    // Read no further than the answer. The runtime appends one whole record per `write_all`
    // under `O_APPEND`, so a writer killed mid-write leaves a truncated *tail* and nothing
    // else — and a resolver that has already found its `task_start` never reaches it. That
    // narrower appetite is this caller's alone: [`parse_trace_records`] takes the whole file as
    // its subject and a torn line there stays `E-TRC-001`.
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| trace_read_error(&path, &e))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceEvent>(line) {
            Ok(TraceEvent::TaskStart(task)) if !task.context_id.is_empty() => {
                return Ok(Some(task.context_id));
            }
            Ok(_) => {}
            // Before the answer there is nothing to weigh an unreadable line against, and no
            // killed writer produces one there anyway.
            Err(err) => {
                return Err(CliError::new(
                    E_TRC_001,
                    format!("{}:{}: {err}", path.display(), i + 1),
                ));
            }
        }
    }

    Ok(None)
}

/// The diagnostic for a trace file that could not be opened or read.
///
/// Shared so that reaching the same file by different routes — streamed by
/// [`first_task_context_id`], read whole by [`parse_trace_records`] — cannot report an absent or
/// unreadable trace two different ways.
fn trace_read_error(path: &Path, err: &std::io::Error) -> CliError {
    match err.kind() {
        std::io::ErrorKind::NotFound => CliError::new(
            E_IO_001,
            format!("trace file not found: {}", path.display()),
        ),
        _ => CliError::new(
            E_IO_003,
            format!("failed to read {}: {err}", path.display()),
        ),
    }
}

fn parse_trace_file(path: &Path) -> Result<Vec<TraceEvent>, CliError> {
    Ok(parse_trace_records(path)?
        .into_iter()
        .map(|record| record.event)
        .collect())
}

/// Parse every line into the event this build understands plus its place in the tree.
///
/// Tolerance and strictness are the file's contract: an unknown key on a known event type is
/// ignored, an unknown event type becomes [`TraceEvent::Unknown`], and a line that is not
/// valid JSON aborts with `E-TRC-001` naming `file:line`.
fn parse_trace_records(path: &Path) -> Result<Vec<TraceRecord>, CliError> {
    let content = fs::read_to_string(path).map_err(|e| trace_read_error(path, &e))?;

    let mut events = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceEvent>(line) {
            // Identity is read in a second pass over the same line, so the event structs stay
            // payload-only rather than repeating three keys apiece. Every field defaults, so
            // this cannot fail where the event parse succeeded on an object.
            Ok(event) => events.push(TraceRecord {
                identity: serde_json::from_str::<EventIdentity>(line).unwrap_or_default(),
                event,
            }),
            Err(err) => {
                return Err(CliError::new(
                    E_TRC_001,
                    format!("{}:{}: {err}", path.display(), i + 1),
                ));
            }
        }
    }

    Ok(events)
}

/// Recognized field names (matched case-insensitively on the *field name* only)
/// under which a tool's `input` blob conventionally carries the resource it addresses.
/// This names *where the address lives*, not *which tool* is calling — every tool that
/// puts its target under one of these keys is handled identically.
const PATH_FIELD_NAMES: [&str; 5] = ["path", "file", "file_path", "filepath", "filename"];

/// Extract the addressed resource from a tool call's `input` blob for redundancy tracking.
///
/// This is an *identity* heuristic — unlike [`extract_input_summary`] (a display
/// heuristic that grabs the first string anywhere in the tree), this matches only
/// the specific top-level field names in [`PATH_FIELD_NAMES`], case-insensitively
/// on the key. Returns `None` when `input` is absent, is not a JSON object, or has
/// no recognized string-valued path field — such calls are skipped entirely for
/// redundancy purposes (neither flagged nor establishing tracking state).
fn extract_tracked_path(input: Option<&serde_json::Value>) -> Option<String> {
    let obj = input?.as_object()?;
    for field in PATH_FIELD_NAMES {
        for (key, value) in obj {
            if key.to_lowercase() == field {
                if let serde_json::Value::String(s) = value {
                    return Some(s.clone());
                }
            }
        }
    }
    None
}

/// Resolve which resource a call addressed, for redundancy tracking.
///
/// Precedence, in order:
/// 1. The tool's own declared `resource_id` (from `tool-result.metadata`), when present and
///    non-empty. Taken verbatim — opaque, never parsed, and `input` is not inspected at all.
///    This is how a tool whose resource is not a filesystem path (a symbol, a URI, a query)
///    gets detection: it declares what it addressed, in whatever scheme it uses.
/// 2. Otherwise, [`extract_tracked_path`]'s input-sniffing heuristic — a fallback that can
///    only recognize a handful of English path-like field names, kept so that every tool
///    written before `resource_id` existed behaves exactly as it did before.
///
/// An empty-string `resource_id` means "undeclared" and falls through to the fallback,
/// matching the convention `state_effect`/`continuation_id` already use. `None` means the
/// call is skipped for redundancy entirely — neither flagged nor establishing tracking state.
fn resolve_resource_identity(
    resource_id: Option<&str>,
    input: Option<&serde_json::Value>,
) -> Option<String> {
    match resource_id.filter(|id| !id.is_empty()) {
        Some(declared) => Some(declared.to_string()),
        None => extract_tracked_path(input),
    }
}

/// How a call affected the resource it addressed, as classified from the call's
/// self-declared `state_effect` metadata. The detector's entire tool-awareness lives in
/// [`StateEffect::classify`] — it maps a declared string to behavior and knows nothing
/// about any specific tool, operation, or use case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StateEffect {
    /// The call only observed the resource — a repeat against the same unchanged resource
    /// is redundant.
    Read,
    /// The call changed the resource — it invalidates any earlier read of that resource.
    Mutate,
    /// The tool declared nothing recognizable. Treated conservatively: like a mutate for
    /// invalidation (so an undeclared call is never assumed harmless), and never credited
    /// as a redundant read (so an undeclared tool gets no detection of its own, but also
    /// never produces a false positive).
    Unknown,
}

impl StateEffect {
    fn classify(declared: Option<&str>) -> Self {
        match declared {
            Some("read") => StateEffect::Read,
            Some("mutate") => StateEffect::Mutate,
            _ => StateEffect::Unknown,
        }
    }
}

/// The run a step or summary line belongs to: the most recent one opened under the same plan id.
///
/// A line naming a plan this trace holds no `plan_start` for opens a run of its own — a partial
/// trace still reports the steps it does hold rather than dropping them.
fn plan_run_mut<'a>(runs: &'a mut Vec<PlanRunRecord>, plan_id: &str) -> &'a mut PlanRunRecord {
    match runs.iter().rposition(|run| run.plan_id == plan_id) {
        Some(index) => &mut runs[index],
        None => {
            runs.push(PlanRunRecord {
                plan_id: plan_id.to_string(),
                step_count: 0,
                declared: Vec::new(),
                steps: Vec::new(),
                outcome: None,
                failed_step: None,
                steps_succeeded: 0,
                steps_failed: 0,
                steps_skipped: 0,
                duration_ms: 0,
                reason: None,
            });
            runs.last_mut().expect("just pushed")
        }
    }
}

fn compute_metrics(
    path: &Path,
    events: Vec<TraceEvent>,
) -> Result<(TraceMetrics, Vec<TaskMetrics>), CliError> {
    let mut ss: Option<SessionStartEvent> = None;
    let mut se: Option<SessionEndEvent> = None;
    let mut tool_ok = 0u32;
    let mut tool_error = 0u32;
    let mut tool_latencies: Vec<u64> = Vec::new();
    let mut tool_call_records: Vec<ToolCallRecord> = Vec::new();
    let mut redundant_calls: Vec<RedundantCallRecord> = Vec::new();
    // resource → the site of the most recent call that declared it *read* that resource;
    // invalidated when a later call declares it *mutated* (or is undeclared, treated
    // conservatively as a mutate) against the same resource. One map for both kinds of site: a
    // plan-driven run and an ad-hoc run are measured on one efficiency axis, not two.
    let mut resource_last_access: HashMap<String, CallSite> = HashMap::new();
    let mut inference_records: Vec<InferenceRecord> = Vec::new();
    let mut provider_tokens: Option<ProviderTokens> = None;
    let mut shell_exit_codes: HashMap<i32, u32> = HashMap::new();
    let mut shell_latencies: Vec<u64> = Vec::new();
    let mut skill_ok = 0u32;
    let mut skill_error = 0u32;
    let mut skill_latencies: Vec<u64> = Vec::new();
    let mut skill_call_records: Vec<SkillCallRecord> = Vec::new();
    let mut compaction: Option<CompactionRecord> = None;
    let mut compactions_declined: Vec<CompactionDeclinedRecord> = Vec::new();
    let mut tool_refreshes: Vec<ToolsRefreshedEvent> = Vec::new();
    // Task ids seen on a `task_start`, so a `task_end` with no opening line is ignored rather
    // than counted as a task.
    let mut task_starts: HashSet<String> = HashSet::new();
    let mut task_metrics: Vec<TaskMetrics> = Vec::new();
    let mut cancels: Vec<CancelRecord> = Vec::new();
    let mut rejections: Vec<TaskRejectedEvent> = Vec::new();
    let mut formation_ended: Option<String> = None;
    let mut spawner_ended: Option<SpawnerEndedEvent> = None;
    let mut failures: Vec<TaskFailedEvent> = Vec::new();
    let mut reopens: Vec<ReopenRecord> = Vec::new();
    let mut context_seeds: Vec<ContextSeedRecord> = Vec::new();
    let mut denials: Vec<DenialRecord> = Vec::new();
    let mut protected_path_denials: Vec<ProtectedPathDenialRecord> = Vec::new();
    let mut hook_failures: Vec<HookFailureRecord> = Vec::new();
    let mut harness_lines: Vec<HarnessLine> = Vec::new();
    let mut retentions: Vec<RetentionRecord> = Vec::new();
    let mut resource_lists = OutcomeCounts::new();
    let mut resource_reads = OutcomeCounts::new();
    let mut peer_mints = OutcomeCounts::new();
    let mut peer_redeems = OutcomeCounts::new();
    let mut peer_fetches = OutcomeCounts::new();
    let mut a2a_tasks_received = 0u32;
    let mut a2a_sends: Vec<String> = Vec::new();
    let mut artifacts_pulled: Vec<ArtifactPulledEvent> = Vec::new();
    let mut delegations: Vec<DelegationRecord> = Vec::new();
    let mut member_calls: Vec<MemberCallRecord> = Vec::new();
    let mut plan_runs: Vec<PlanRunRecord> = Vec::new();
    let mut control_changes: Vec<ControlChangeRecord> = Vec::new();
    let mut control_refusals = OutcomeCounts::new();

    for event in events {
        match event {
            TraceEvent::SessionStart(e) => ss = Some(e),
            TraceEvent::Inference(e) => {
                // Summed over every record that reported them, a hook's `run-inference`
                // included: these are what the provider billed the session for.
                if e.input_tokens_actual.is_some()
                    || e.output_tokens_actual.is_some()
                    || e.cached_tokens.is_some()
                    || e.cache_write_tokens.is_some()
                    || e.thinking_tokens.is_some()
                {
                    provider_tokens
                        .get_or_insert_with(ProviderTokens::default)
                        .add(&e);
                }
                inference_records.push(InferenceRecord::from_event(e));
            }
            TraceEvent::ToolCall(e) => {
                tool_latencies.push(e.duration_ms);
                if e.status == "ok" {
                    tool_ok += 1;
                } else {
                    tool_error += 1;
                }
                // Redundant-call tracking, driven entirely by what each call declares about
                // itself — no tool or operation is recognized by name. `resource_id` says
                // *what* was addressed (falling back to sniffing a path out of the input for
                // tools that declare nothing), `state_effect` says *how*. Reads against a
                // resource share one history keyed by that identity; a mutate (or an
                // undeclared effect, treated conservatively as a mutate) invalidates it.
                if let Some(resource) =
                    resolve_resource_identity(e.resource_id.as_deref(), e.input.as_ref())
                {
                    match StateEffect::classify(e.state_effect.as_deref()) {
                        StateEffect::Read => {
                            if let Some(prior) = resource_last_access.get(&resource) {
                                redundant_calls.push(RedundantCallRecord {
                                    site: CallSite::Turn(e.turn),
                                    tool_name: Some(e.tool_name.clone()),
                                    resource_id: resource.clone(),
                                    prior: prior.clone(),
                                });
                            }
                            resource_last_access.insert(resource, CallSite::Turn(e.turn));
                        }
                        StateEffect::Mutate | StateEffect::Unknown => {
                            resource_last_access.remove(&resource);
                        }
                    }
                }
                tool_call_records.push(ToolCallRecord {
                    turn: e.turn,
                    tool_name: e.tool_name,
                    input: e.input,
                    status: e.status,
                    duration_ms: e.duration_ms,
                });
            }
            TraceEvent::SkillCall(e) => {
                skill_latencies.push(e.duration_ms);
                if e.status == "ok" {
                    skill_ok += 1;
                } else {
                    skill_error += 1;
                }
                skill_call_records.push(SkillCallRecord {
                    turn: e.turn,
                    untrusted: e.untrusted_marker(),
                    skill_name: e.skill_name,
                    status: e.status,
                    duration_ms: e.duration_ms,
                });
            }
            TraceEvent::Shell(e) => {
                shell_latencies.push(e.duration_ms);
                *shell_exit_codes.entry(e.exit_code).or_insert(0) += 1;
            }
            // A demoted command's exit code and duration exist only on its completion, so that
            // is where its latency and exit code are counted — once, as a foreground command is
            // counted once from its own `shell` record. A command abandoned at session end, and
            // one a later resume found unaccounted for, contribute neither, because neither ever
            // produced either.
            TraceEvent::ShellCompleted(e) => {
                shell_latencies.push(e.duration_ms);
                *shell_exit_codes.entry(e.exit_code).or_insert(0) += 1;
            }
            TraceEvent::ShellDetached(_)
            | TraceEvent::ShellAbandoned(_)
            | TraceEvent::ShellLost(_) => {}
            TraceEvent::Compaction(e) => {
                compaction = Some(CompactionRecord {
                    turn: e.turn,
                    tokens_before: e.tokens_before,
                    tokens_after: e.tokens_after,
                });
            }
            TraceEvent::CompactionDeclined(e) => {
                compactions_declined.push(CompactionDeclinedRecord {
                    turn: e.turn,
                    tokens: e.tokens,
                    reason: e.reason,
                });
            }
            TraceEvent::SessionEnd(e) => se = Some(e),
            TraceEvent::TaskStart(e) => {
                task_starts.insert(e.task_id.clone());
            }
            TraceEvent::TaskEnd(e) => {
                if task_starts.remove(&e.task_id) {
                    task_metrics.push(TaskMetrics {
                        task_id: e.task_id,
                        exit_status: e.exit_status,
                        duration_ms: e.duration_ms,
                        turns: e.turns,
                        input_tokens: e.input_tokens,
                        output_tokens: e.output_tokens,
                        reopen_count: e.reopen_count,
                    });
                }
            }
            TraceEvent::TaskCanceled(e) => {
                cancels.push(CancelRecord {
                    phase: e.phase,
                    detached_work_ids: e.detached_work_ids,
                    delegation_ids: e.delegation_ids,
                });
            }
            TraceEvent::TaskRejected(e) => rejections.push(e),
            TraceEvent::FormationEnded(e) => formation_ended = Some(e.formation_id),
            TraceEvent::SpawnerEnded(e) => spawner_ended = Some(e),
            TraceEvent::TaskFailed(e) => failures.push(e),
            TraceEvent::TaskReopened(e) => {
                reopens.push(ReopenRecord {
                    reopen_number: e.reopen_number,
                    hook_name: e.hook_name,
                    reason: e.reason,
                    attempt_context: e.attempt_context,
                    turns_remaining: e.turns_remaining,
                });
            }
            TraceEvent::ContextSeed(e) => {
                context_seeds.push(ContextSeedRecord {
                    hook_name: e.hook_name,
                    tokens: e.tokens,
                    proposed_tokens: e.proposed_tokens,
                    budget_tokens: e.budget_tokens,
                    outcome: e.outcome,
                    reason: e.reason,
                    message_ids: e.message_ids,
                });
            }
            TraceEvent::CallDenied(e) => {
                denials.push(DenialRecord {
                    turn: e.turn,
                    event: e.event,
                    hook_name: e.hook_name,
                    target: e.target,
                    reason: e.reason,
                });
            }
            TraceEvent::ProtectedPathDenied(e) => {
                protected_path_denials.push(ProtectedPathDenialRecord {
                    turn: e.turn,
                    call: e.call,
                    target: e.target,
                    path: e.path,
                    rule: e.rule,
                    signal: e.signal,
                });
            }
            TraceEvent::HookDispatchError(e) => {
                hook_failures.push(HookFailureRecord {
                    hook_name: e.hook_name,
                    event: e.event,
                    arm: e.arm,
                });
            }
            TraceEvent::HarnessWarning(e) => {
                harness_lines.push(HarnessLine(format!("warning {}: {}", e.code, e.message)));
            }
            TraceEvent::HarnessFailed(e) => {
                harness_lines.push(HarnessLine(format!(
                    "harness failed ({}): {}",
                    e.kind, e.message
                )));
            }
            TraceEvent::HarnessInterrupt(e) => {
                let outcome = if e.delivered {
                    format!("sent, {}ms grace", e.grace_ms)
                } else {
                    "not sent; harness killed at once".to_string()
                };
                harness_lines.push(HarnessLine(format!("interrupt {}: {outcome}", e.method)));
            }
            TraceEvent::Retention(e) => {
                retentions.push(RetentionRecord {
                    store: e.store,
                    reason: e.reason,
                    removed: e.removed,
                    targets: e.targets,
                    messages_dropped: e.messages_dropped,
                });
            }
            TraceEvent::ResourceList(e) => *resource_lists.entry(e.outcome).or_insert(0) += 1,
            TraceEvent::ResourceRead(e) => *resource_reads.entry(e.outcome).or_insert(0) += 1,
            TraceEvent::PeerHandleMint(e) => *peer_mints.entry(e.outcome).or_insert(0) += 1,
            TraceEvent::PeerHandleRedeem(e) => *peer_redeems.entry(e.outcome).or_insert(0) += 1,
            TraceEvent::PeerFileFetch(e) => *peer_fetches.entry(e.outcome).or_insert(0) += 1,
            TraceEvent::A2aTaskReceived => a2a_tasks_received += 1,
            TraceEvent::A2aSend(e) => a2a_sends.push(e.peer_url),
            TraceEvent::ArtifactPulled(e) => artifacts_pulled.push(e),
            TraceEvent::DelegationStart(e) => delegations.push(DelegationRecord {
                delegation_id: Some(e.delegation_id),
                capsule: e.capsule,
                version: e.version,
                child_session_id: Some(e.child_session_id),
                child_workdir: Some(e.child_workdir),
                outcome: None,
                reason: None,
            }),
            TraceEvent::Delegation(e) => {
                // The terminal line closes the row its `delegation_start` opened. A refusal names
                // no id and closes nothing, so it becomes a row of its own.
                let opened = e.delegation_id.as_ref().and_then(|id| {
                    delegations.iter_mut().find(|record| {
                        record.outcome.is_none() && record.delegation_id.as_deref() == Some(id)
                    })
                });
                match opened {
                    Some(record) => {
                        record.outcome = Some(e.outcome);
                        record.reason = e.reason;
                    }
                    None => delegations.push(DelegationRecord {
                        delegation_id: e.delegation_id,
                        capsule: e.capsule,
                        version: e.version,
                        child_session_id: e.child_session_id,
                        child_workdir: None,
                        outcome: Some(e.outcome),
                        reason: e.reason,
                    }),
                }
            }
            TraceEvent::MemberCallBusy(e) => fold_member_call_busy(&mut member_calls, e),
            TraceEvent::MemberCallStart(e) => fold_member_call_start(&mut member_calls, e),
            TraceEvent::MemberCall(e) => fold_member_call(&mut member_calls, e),
            TraceEvent::PlanStart(e) => plan_runs.push(PlanRunRecord {
                plan_id: e.plan_id,
                step_count: e.step_count,
                declared: e.steps,
                steps: Vec::new(),
                outcome: None,
                failed_step: None,
                steps_succeeded: 0,
                steps_failed: 0,
                steps_skipped: 0,
                duration_ms: 0,
                reason: None,
            }),
            // The dispatch line carries nothing the terminal line does not; it exists so
            // `mur trace steps` can show when a step was handed to a worker and what it waited on.
            TraceEvent::PlanStepStart(_) => {}
            // Rendered by `mur trace steps` under its turn; the accompanying failed `tool_call`
            // is what this summary counts.
            TraceEvent::ToolInputRefused(_) => {}
            TraceEvent::PlanStep(e) => {
                // A plan step's declared read shares the agent loop's resource history, on the
                // same terms and through the same resolver. Only a step that succeeded took part:
                // one that failed or was skipped observed nothing.
                if e.status == "success" {
                    if let Some(resource) =
                        resolve_resource_identity(e.resource_id.as_deref(), e.input.as_ref())
                    {
                        let site = CallSite::PlanStep {
                            plan_id: e.plan_id.clone(),
                            step_id: e.step_id.clone(),
                        };
                        match StateEffect::classify(e.state_effect.as_deref()) {
                            StateEffect::Read => {
                                if let Some(prior) = resource_last_access.get(&resource) {
                                    redundant_calls.push(RedundantCallRecord {
                                        site: site.clone(),
                                        tool_name: None,
                                        resource_id: resource.clone(),
                                        prior: prior.clone(),
                                    });
                                }
                                resource_last_access.insert(resource, site);
                            }
                            StateEffect::Mutate | StateEffect::Unknown => {
                                resource_last_access.remove(&resource);
                            }
                        }
                    }
                }
                plan_run_mut(&mut plan_runs, &e.plan_id)
                    .steps
                    .push(PlanStepRecord {
                        step_id: e.step_id,
                        kind: e.kind,
                        status: e.status,
                        attempts: e.attempts,
                        duration_ms: e.duration_ms,
                        error: e.error,
                    });
            }
            TraceEvent::PlanEnd(e) => {
                let run = plan_run_mut(&mut plan_runs, &e.plan_id);
                run.outcome = Some(e.outcome);
                run.failed_step = e.failed_step;
                run.steps_succeeded = e.steps_succeeded;
                run.steps_failed = e.steps_failed;
                run.steps_skipped = e.steps_skipped;
                run.duration_ms = e.duration_ms;
                run.reason = e.reason;
                if run.step_count == 0 {
                    run.step_count = e.steps_total;
                }
            }
            // Rendered in the step list; the summary has no spend section.
            TraceEvent::SpendCeilingReached(_) => {}
            TraceEvent::ControlChange(change) => control_changes.push(ControlChangeRecord {
                change,
                applied_turn: None,
            }),
            TraceEvent::ControlApplied(applied) => {
                if let Some(record) = control_changes
                    .iter_mut()
                    .find(|record| record.change.event_id == applied.change_id)
                {
                    record.applied_turn = Some(applied.turn);
                }
            }
            TraceEvent::ToolsRefreshed(refresh) => tool_refreshes.push(refresh),
            TraceEvent::ControlRefused(refused) => {
                *control_refusals
                    .entry(format!("HTTP {}", refused.status))
                    .or_insert(0) += 1;
            }
            TraceEvent::Unknown => {}
        }
    }

    let ss = ss.ok_or_else(|| {
        CliError::new(
            E_TRC_001,
            format!("{}: no session_start event found", path.display()),
        )
    })?;
    let se = se.ok_or_else(|| {
        // A member killed before it could write its ending is still findable through its
        // formation, which is the one place the rest of what it was part of is listed.
        let formation = match &ss.formation_id {
            Some(id) => format!(
                "; this session is a member of formation {id} — `mur trace show {id}` lists the formation"
            ),
            None => String::new(),
        };
        CliError::new(
            E_TRC_001,
            format!("{}: no session_end event found{formation}", path.display()),
        )
    })?;

    Ok((
        TraceMetrics {
            session_id: ss.session_id,
            capsule_name: ss.capsule_name,
            capsule_version: ss.capsule_version,
            model: ss.model,
            max_turns: ss.max_turns,
            capabilities: ss.capabilities,
            tools_declared: ss.tools_declared,
            containment_declared: ss.containment_declared,
            containment_achieved: ss.containment_achieved,
            workdir_exec: ss.workdir_exec,
            userns_grant: ss.userns_grant,
            system_prompt_source: ss.system_prompt_source,
            system_prompt_sha256: ss.system_prompt_sha256,
            exit_status: se.exit_status,
            duration_ms: se.duration_ms,
            total_turns: se.total_turns,
            total_input_tokens: se.total_input_tokens,
            total_output_tokens: se.total_output_tokens,
            total_tool_calls: se.total_tool_calls,
            tool_ok,
            tool_error,
            tool_latencies_ms: tool_latencies,
            tool_call_records,
            redundant_calls,
            inference_records,
            provider_tokens,
            total_shell_calls: se.total_shell_calls,
            shell_exit_codes,
            shell_latencies_ms: shell_latencies,
            skill_ok,
            skill_error,
            skill_latencies_ms: skill_latencies,
            skill_call_records,
            compaction,
            compactions_declined,
            tool_refreshes,
            cancels,
            rejections,
            formation_ended,
            spawner_ended,
            failures,
            reopens,
            context_seeds,
            denials,
            protected_path_denials,
            hook_failures,
            harness_lines,
            retentions,
            resource_lists,
            resource_reads,
            peer_mints,
            peer_redeems,
            peer_fetches,
            a2a_tasks_received,
            a2a_sends,
            runtime_artifacts: ss.runtime_artifacts,
            artifacts_pulled,
            spawned_by: ss.spawned_by,
            spawned_by_delegation: ss.delegation_id,
            formation_id: ss.formation_id,
            delegations,
            member_calls,
            plan_runs,
            control_changes,
            control_refusals,
        },
        task_metrics,
    ))
}

fn load_metrics(path: &Path) -> Result<(TraceMetrics, Vec<TaskMetrics>), CliError> {
    let events = parse_trace_file(path)?;
    if events.is_empty() {
        return Err(CliError::new(
            E_TRC_001,
            format!(
                "{}: trace file is empty (incomplete or zero-event session)",
                path.display()
            ),
        ));
    }
    compute_metrics(path, events)
}

// ── Session resolution ────────────────────────────────────────────────────────

/// `trace.jsonl` addressing: the shared vocabulary, this command's diagnostic code, and the
/// argument a failure should name.
fn trace_query(label: Option<&str>) -> SessionQuery<'_> {
    SessionQuery {
        record_file: "trace.jsonl",
        code: E_TRC_002,
        label,
    }
}

/// The `trace.jsonl` a session address names, or the most recent session's when none is given.
pub(crate) fn resolve_session(
    session: Option<String>,
    workdir: &Path,
) -> Result<PathBuf, CliError> {
    session_address::resolve(session.as_deref(), workdir, &trace_query(None))
}

/// The session directory an address names, under `workdir`.
///
/// The whole `mur trace diff` address vocabulary — a full `ses_` id, a 4+-character
/// case-insensitive suffix, an `@N` ordinal, a literal path — resolved by exactly the resolver
/// `diff` uses, so `mur run --resume` and `mur trace diff` can never disagree about what an
/// address names or how an unresolvable one reads. `label` prefixes the `E-TRC-002` message with
/// the flag the operator wrote.
pub(crate) fn resolve_session_dir(
    arg: &str,
    workdir: &Path,
    label: &str,
) -> Result<PathBuf, CliError> {
    let resolved = resolve_diff_arg(arg, workdir, label)?;
    // Every non-literal address resolves to `<session dir>/trace.jsonl`; the literal-path form
    // passes through whatever was written, which is a session directory as often as it is the
    // file inside it.
    if resolved.is_dir() {
        return Ok(resolved);
    }
    resolved
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::new(E_TRC_002, format!("{label}: '{arg}' names no session")))
}

/// One side of a `mur trace diff`, with `label` naming which side when it will not resolve.
fn resolve_diff_arg(arg: &str, workdir: &Path, label: &str) -> Result<PathBuf, CliError> {
    session_address::resolve(Some(arg), workdir, &trace_query(Some(label)))
}

// ── Report helpers ────────────────────────────────────────────────────────────

fn parse_since(s: &str) -> Result<u64, CliError> {
    let (n_str, multiplier) = if let Some(n) = s.strip_suffix('m') {
        (n, 60_000u64)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000u64)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000u64)
    } else {
        return Err(CliError::new(
            E_TRC_002,
            format!(
                "unrecognised --since format '{}' — expected <N>m, <N>h, or <N>d",
                s
            ),
        ));
    };
    let n: u64 = n_str.parse().map_err(|_| {
        CliError::new(
            E_TRC_002,
            format!(
                "unrecognised --since format '{}' — expected <N>m, <N>h, or <N>d",
                s
            ),
        )
    })?;
    Ok(n * multiplier)
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn fmt_dur(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

const INPUT_INLINE_LIMIT: usize = 120;

/// Compact JSON for a tool call's input, truncated to 120 characters, prefixed
/// with two spaces so it appends directly onto a tool-call part. `None` renders
/// as an empty string so records without input gain no stray segment.
fn fmt_input_inline(input: Option<&serde_json::Value>) -> String {
    let Some(v) = input else {
        return String::new();
    };
    let s = serde_json::to_string(v).unwrap_or_default();
    match s.char_indices().nth(INPUT_INLINE_LIMIT) {
        Some((i, _)) => format!("  {}…", &s[..i]),
        None => format!("  {}", s),
    }
}

fn fmt_opt_f(opt: Option<f64>) -> String {
    match opt {
        None => "—".to_string(),
        Some(v) => format!("{:.0}", v),
    }
}

fn fmt_opt_dur(opt: Option<f64>) -> String {
    match opt {
        None => "—".to_string(),
        Some(v) => fmt_dur(v as u64),
    }
}

fn fmt_exit_codes(codes: &HashMap<i32, u32>) -> String {
    let ok_count = codes.get(&0).copied().unwrap_or(0);
    let mut parts: Vec<String> = Vec::new();
    if ok_count > 0 {
        parts.push(format!("{} ok", ok_count));
    }
    let mut failed: Vec<(&i32, &u32)> = codes.iter().filter(|(k, _)| **k != 0).collect();
    failed.sort_by_key(|(k, _)| *k);
    for (code, count) in &failed {
        parts.push(format!("{} failed (exit {})", count, code));
    }
    parts.join(", ")
}

/// How many characters of a sha256 the human-readable sections print. Long enough to
/// distinguish two hashes at a glance, short enough to leave a turn on one line; a `--body`
/// selector accepts any prefix of 8 or more.
const SHA_DISPLAY_LEN: usize = 12;

/// A hash abbreviated for reading, with an ellipsis marking what was cut. Never the value a
/// reader should copy back into `--body` blindly — though a 12-character prefix is accepted.
fn fmt_sha_short(sha: &str) -> String {
    fmt_id_short(sha, SHA_DISPLAY_LEN)
}

/// An id abbreviated to `len` characters, with an ellipsis when anything was cut.
fn fmt_id_short(id: &str, len: usize) -> String {
    match id.char_indices().nth(len) {
        Some((i, _)) => format!("{}…", &id[..i]),
        None => id.to_string(),
    }
}

/// `<n> ok, <n> refused-with-this-code`, in outcome order.
fn fmt_outcomes(counts: &OutcomeCounts) -> String {
    counts
        .iter()
        .map(|(outcome, count)| format!("{count} {outcome}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn fmt_session_short(id: &str) -> String {
    let short_len = "ses_".len() + 8;
    if id.len() > short_len {
        format!("{}...", &id[..short_len])
    } else {
        id.to_string()
    }
}

// ── Show ──────────────────────────────────────────────────────────────────────

fn print_show(m: &TraceMetrics) {
    capsule_runtime::report_println!("── Session ──────────────────────────────────────");
    capsule_runtime::report_println!("session:    {}", m.session_id);
    // Only for a capsule another capsule launched. One level up from here is where "why did this
    // run?" is answered, and this is the only line in the file that names it.
    if let Some(parent) = &m.spawned_by {
        match &m.spawned_by_delegation {
            Some(id) => capsule_runtime::report_println!("Spawned by {parent} (delegation {id})"),
            None => capsule_runtime::report_println!("Spawned by {parent}"),
        }
    }
    if let Some(formation) = &m.formation_id {
        capsule_runtime::report_println!("formation:  {formation}");
    }
    capsule_runtime::report_println!("capsule:    {} v{}", m.capsule_name, m.capsule_version);
    capsule_runtime::report_println!("model:      {}", m.model);
    capsule_runtime::report_println!("status:     {}", m.exit_status);
    capsule_runtime::report_println!("duration:   {}", fmt_dur(m.duration_ms));
    if !m.capabilities.is_empty() {
        capsule_runtime::report_println!("{:<11} {}", "capabilities:", m.capabilities.join(", "));
    }
    if !m.tools_declared.is_empty() {
        capsule_runtime::report_println!("{:<11} {}", "tools:", m.tools_declared.join(", "));
    }
    // Artifacts a running capsule pinned rather than the operator: each reached the model
    // marked untrusted.
    if let Some(line) = runtime_pins_line(&m.runtime_artifacts) {
        capsule_runtime::report_println!("{line}");
    }
    // What was asked for against what this host could enforce. The two are read together:
    // a capsule that declared `sealed` and achieved `advisory` ran with neither.
    if let (Some(declared), Some(achieved)) = (&m.containment_declared, &m.containment_achieved) {
        capsule_runtime::report_println!("{:<11} {} → {}", "containment:", declared, achieved);
    }
    if let Some(exec) = m.workdir_exec {
        capsule_runtime::report_println!(
            "{:<11} {}",
            "workdir exec:",
            if exec { "yes" } else { "no" }
        );
    }
    if let Some(grant) = &m.userns_grant {
        capsule_runtime::report_println!("{:<11} {}", "userns:", grant);
    }
    if let Some(source) = &m.system_prompt_source {
        let sha = match &m.system_prompt_sha256 {
            Some(sha) => format!("  {}", fmt_sha_short(sha)),
            None => String::new(),
        };
        capsule_runtime::report_println!("{:<11} {}{}", "prompt:", source, sha);
    }
    capsule_runtime::report_println!();

    // Beside the hook failures: on a `transport: process` session these are the only account of
    // an untested harness version, an unexpected auth mode, or a turn the harness refused.
    if !m.harness_lines.is_empty() {
        capsule_runtime::report_println!("── Harness ──────────────────────────────────────");
        for line in &m.harness_lines {
            capsule_runtime::report_println!("{}", line.0);
        }
        capsule_runtime::report_println!();
    }

    // Placed where it cannot be scrolled past: a hook that failed left the session running
    // as if it had returned nothing, and no other section says so.
    if !m.hook_failures.is_empty() {
        capsule_runtime::report_println!("── Hook failures ────────────────────────────────");
        for f in &m.hook_failures {
            capsule_runtime::report_println!("✗ {}  {}  {}", f.hook_name, f.event, f.arm);
        }
        capsule_runtime::report_println!();
    }

    // Beside the hook failures, and for the same reason: this is the only record that something
    // an operator might go looking for is gone, and why.
    if !m.retentions.is_empty() {
        capsule_runtime::report_println!("── Retention ────────────────────────────────────");
        for r in &m.retentions {
            let dropped = r
                .messages_dropped
                .map(|n| format!(", {n} messages dropped"))
                .unwrap_or_default();
            capsule_runtime::report_println!(
                "{}  {}  removed {}{}",
                r.store,
                r.reason,
                r.removed,
                dropped
            );
            if !r.targets.is_empty() {
                capsule_runtime::report_println!("  {}", r.targets.join(", "));
            }
        }
        capsule_runtime::report_println!();
    }

    if !m.context_seeds.is_empty() {
        capsule_runtime::report_println!("── Context ──────────────────────────────────────");
        for seed in &m.context_seeds {
            capsule_runtime::report_println!(
                "{}  {}  {} tokens (proposed {}, budget {})",
                seed.hook_name,
                seed.outcome,
                fmt_thousands(seed.tokens),
                fmt_thousands(seed.proposed_tokens),
                fmt_thousands(seed.budget_tokens)
            );
            if let Some(reason) = &seed.reason {
                capsule_runtime::report_println!("  reason:   {}", reason);
            }
            if !seed.message_ids.is_empty() {
                capsule_runtime::report_println!("  messages: {}", seed.message_ids.join(", "));
            }
        }
        capsule_runtime::report_println!();
    }

    capsule_runtime::report_println!("── Turns ────────────────────────────────────────");
    capsule_runtime::report_println!("count:      {}  (max: {})", m.total_turns, m.max_turns);
    for line in failed_call_lines(&m.inference_records) {
        capsule_runtime::report_println!("{line}");
    }
    capsule_runtime::report_println!();

    capsule_runtime::report_println!("── Tokens ───────────────────────────────────────");
    capsule_runtime::report_println!(
        "input:      {}  (avg {}/turn)",
        fmt_thousands(m.total_input_tokens),
        fmt_opt_f(m.avg_input_per_turn())
    );
    capsule_runtime::report_println!(
        "output:     {}  (avg {}/turn)",
        fmt_thousands(m.total_output_tokens),
        fmt_opt_f(m.avg_output_per_turn())
    );
    capsule_runtime::report_println!(
        "total:      {}",
        fmt_thousands(m.total_input_tokens + m.total_output_tokens)
    );
    // The provider's or harness's own counts, beside the totals above. Under `transport: http`
    // those totals are the runtime's tiktoken estimate and the difference between the two lines
    // is estimator drift; under `process` only the cache and thinking members appear here,
    // because the harness's input and output counts are already the totals above.
    if let Some(p) = &m.provider_tokens {
        capsule_runtime::report_println!("provider:   {}", p.parts().join(", "));
    }
    capsule_runtime::report_println!();

    let wire_turns: Vec<&InferenceRecord> = m
        .inference_records
        .iter()
        .filter(|rec| rec.is_agent_loop() && rec.has_hashes())
        .collect();
    if !wire_turns.is_empty() || !m.tool_refreshes.is_empty() {
        capsule_runtime::report_println!("── Wire ─────────────────────────────────────────");
        for rec in &wire_turns {
            capsule_runtime::report_println!(
                "turn {}  system {}  tools {}  response {}  {} message{}",
                rec.turn,
                rec.system_sha
                    .as_deref()
                    .map(fmt_sha_short)
                    .unwrap_or_else(|| "—".to_string()),
                rec.tools_sha
                    .as_deref()
                    .map(fmt_sha_short)
                    .unwrap_or_else(|| "—".to_string()),
                rec.response_sha
                    .as_deref()
                    .map(fmt_sha_short)
                    .unwrap_or_else(|| "—".to_string()),
                rec.message_shas.len(),
                if rec.message_shas.len() == 1 { "" } else { "s" }
            );
        }
        for refresh in &m.tool_refreshes {
            capsule_runtime::report_println!(
                "refreshed:  turn {}  {}",
                refresh.turn,
                refresh.changes()
            );
        }
        if let Some(first) = wire_turns.first() {
            capsule_runtime::report_println!(
                "bodies:     mur trace show --body system --turn {}",
                first.turn
            );
        }
        capsule_runtime::report_println!();
    }

    capsule_runtime::report_println!("── Tool calls ───────────────────────────────────");
    if m.total_tool_calls == 0 {
        capsule_runtime::report_println!("count:      0");
    } else {
        capsule_runtime::report_println!(
            "count:      {}  ({} ok, {} error)  success {:.1}%",
            m.total_tool_calls,
            m.tool_ok,
            m.tool_error,
            m.tool_success_rate().unwrap_or(0.0)
        );
        capsule_runtime::report_println!(
            "latency:    avg {}",
            fmt_opt_dur(m.avg_tool_latency_ms())
        );
        let mut by_turn: BTreeMap<u32, Vec<&ToolCallRecord>> = BTreeMap::new();
        for rec in &m.tool_call_records {
            by_turn.entry(rec.turn).or_default().push(rec);
        }
        let inference_map: BTreeMap<u32, &str> = m
            .inference_records
            .iter()
            .map(|rec| (rec.turn, rec.decision.as_str()))
            .collect();
        let all_turns: BTreeSet<u32> = by_turn
            .keys()
            .chain(inference_map.keys())
            .copied()
            .collect();
        for turn in all_turns {
            if let Some(records) = by_turn.get(&turn) {
                let parts: Vec<String> = records
                    .iter()
                    .map(|rec| {
                        let icon = if rec.status == "ok" { "✓" } else { "✗" };
                        format!(
                            "{} {} {}{}",
                            rec.tool_name,
                            fmt_dur(rec.duration_ms),
                            icon,
                            fmt_input_inline(rec.input.as_ref())
                        )
                    })
                    .collect();
                capsule_runtime::report_println!("  turn {}  {}", turn, parts.join("  "));
            } else if let Some(decision) = inference_map.get(&turn) {
                capsule_runtime::report_println!("  turn {}  {}", turn, decision);
            }
        }
    }
    capsule_runtime::report_println!();

    capsule_runtime::report_println!("── Redundant calls ──────────────────────────────");
    capsule_runtime::report_println!("count:      {}", m.redundant_calls.len());
    for rec in &m.redundant_calls {
        // A plan step names no tool of its own, so its row is the site, the resource and the
        // prior site — one column shorter than a turn's.
        let caller = match &rec.tool_name {
            Some(name) => format!("{name}  "),
            None => String::new(),
        };
        capsule_runtime::report_println!(
            "  {}  {}{}  (re-reads {})",
            rec.site.label(),
            caller,
            rec.resource_id,
            rec.prior.label()
        );
    }
    capsule_runtime::report_println!();

    capsule_runtime::report_println!("── Skill calls ──────────────────────────────────");
    let total_skill = m.total_skill_calls();
    if total_skill == 0 {
        capsule_runtime::report_println!("count:      0");
    } else {
        capsule_runtime::report_println!(
            "count:      {}  ({} ok, {} error)  success {:.1}%",
            total_skill,
            m.skill_ok,
            m.skill_error,
            m.skill_success_rate().unwrap_or(0.0)
        );
        capsule_runtime::report_println!(
            "latency:    avg {}",
            fmt_opt_dur(m.avg_skill_latency_ms())
        );
        let mut by_turn: BTreeMap<u32, Vec<&SkillCallRecord>> = BTreeMap::new();
        for rec in &m.skill_call_records {
            by_turn.entry(rec.turn).or_default().push(rec);
        }
        for (turn, records) in &by_turn {
            let parts: Vec<String> = records
                .iter()
                .map(|rec| skill_call_show_entry(rec))
                .collect();
            capsule_runtime::report_println!("  turn {}  {}", turn, parts.join("  "));
        }
    }
    capsule_runtime::report_println!();

    if !m.artifacts_pulled.is_empty() {
        capsule_runtime::report_println!("── Pulled at runtime ────────────────────────────");
        for pulled in &m.artifacts_pulled {
            capsule_runtime::report_println!("{}", artifact_pulled_show_row(pulled));
        }
        capsule_runtime::report_println!();
    }

    capsule_runtime::report_println!("── Shell calls ──────────────────────────────────");
    if m.total_shell_calls == 0 {
        capsule_runtime::report_println!("count:      0");
    } else {
        capsule_runtime::report_println!("count:      {}", m.total_shell_calls);
        capsule_runtime::report_println!("exit codes: {}", fmt_exit_codes(&m.shell_exit_codes));
        capsule_runtime::report_println!(
            "latency:    avg {}",
            fmt_opt_dur(m.avg_shell_latency_ms())
        );
    }
    capsule_runtime::report_println!();

    capsule_runtime::report_println!("── Compaction ───────────────────────────────────");
    match &m.compaction {
        None => capsule_runtime::report_println!("fired:      no"),
        Some(c) => capsule_runtime::report_println!(
            "fired:      yes  at turn {}  ({} → {} tokens)",
            c.turn,
            fmt_thousands(c.tokens_before),
            fmt_thousands(c.tokens_after)
        ),
    }
    for d in &m.compactions_declined {
        capsule_runtime::report_println!(
            "declined:   at turn {}  ({} tokens)  {}",
            d.turn,
            fmt_thousands(d.tokens),
            d.reason
        );
    }

    if let Some(formation) = &m.formation_ended {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Formation ended ──────────────────────────────");
        capsule_runtime::report_println!("{}", formation_ended_show_row(formation));
    }

    if let Some(spawner) = &m.spawner_ended {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Spawner ended ────────────────────────────────");
        capsule_runtime::report_println!("{}", spawner_ended_show_row(spawner));
    }

    if !m.cancels.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Cancelled ────────────────────────────────────");
        for c in &m.cancels {
            capsule_runtime::report_println!(
                "task_canceled  at {}  {}",
                c.phase,
                cancel_residue(&c.detached_work_ids, &c.delegation_ids)
            );
        }
    }

    if !m.rejections.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Rejected ─────────────────────────────────────");
        for r in &m.rejections {
            capsule_runtime::report_println!("{}", rejected_show_row(r));
        }
    }

    if !m.failures.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Failed ───────────────────────────────────────");
        for f in &m.failures {
            let task = f
                .task_id
                .as_deref()
                .map(|task_id| format!("  {task_id}"))
                .unwrap_or_default();
            let turn = f
                .turn
                .map(|turn| format!("  at turn {turn}"))
                .unwrap_or_default();
            capsule_runtime::report_println!("task_failed  {}{task}{turn}", f.cause);
            capsule_runtime::report_println!("  reason: {}", f.reason);
        }
    }

    if !m.reopens.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Reopens ──────────────────────────────────────");
        for r in &m.reopens {
            let reason: String = r.reason.chars().take(80).collect();
            let next_attempt = r
                .next_attempt()
                .map(|next| format!("{next}  "))
                .unwrap_or_default();
            capsule_runtime::report_println!(
                "reopen {}  by {}  {next_attempt}“{}”",
                r.reopen_number,
                r.hook_name,
                reason
            );
        }
    }

    if !m.denials.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Denied calls ─────────────────────────────────");
        for d in &m.denials {
            let reason: String = d.reason.chars().take(80).collect();
            capsule_runtime::report_println!(
                "turn {}  {}  {}  by {}  “{}”",
                d.turn,
                d.event,
                d.target,
                d.hook_name,
                reason
            );
        }
    }

    // Beside the hook denials and for the same reason: no `shell` or `tool_call` line exists for
    // a refused call, so this is the only account of it. The count is the comparable number — a
    // run that attempted a protected write and was refused is a different result from one that
    // never tried.
    if !m.protected_path_denials.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Protected paths ──────────────────────────────");
        capsule_runtime::report_println!(
            "protected-path refusals: {}",
            m.protected_path_denials.len()
        );
        for d in &m.protected_path_denials {
            capsule_runtime::report_println!(
                "turn {}  {}  {}  path {}  rule {}  ({})",
                d.turn,
                d.call,
                d.target,
                d.path,
                d.rule,
                d.signal
            );
        }
    }

    if !m.resource_lists.is_empty() || !m.resource_reads.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Resource plane ───────────────────────────────");
        if !m.resource_lists.is_empty() {
            capsule_runtime::report_println!("list:       {}", fmt_outcomes(&m.resource_lists));
        }
        if !m.resource_reads.is_empty() {
            capsule_runtime::report_println!("read:       {}", fmt_outcomes(&m.resource_reads));
        }
    }

    // Only a capsule that declares alternates names a choice per turn, so a cost change between
    // two turns can be traced to the model that served each.
    let served: Vec<&InferenceRecord> = m
        .inference_records
        .iter()
        .filter(|rec| rec.is_agent_loop() && rec.served_by.is_some())
        .collect();
    if !served.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Driver choices ───────────────────────────────");
        for rec in served {
            if let Some((choice, model)) = &rec.served_by {
                capsule_runtime::report_println!("turn {}  on {choice} ({model})", rec.turn);
            }
        }
    }

    if !m.control_changes.is_empty() || !m.control_refusals.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Control ──────────────────────────────────────");
        for record in &m.control_changes {
            capsule_runtime::report_println!(
                "{}",
                control_change_line(&record.change, Some(record.applied_turn))
            );
        }
        if !m.control_refusals.is_empty() {
            capsule_runtime::report_println!("refused:    {}", fmt_outcomes(&m.control_refusals));
        }
    }

    if !m.peer_mints.is_empty() || !m.peer_redeems.is_empty() || !m.peer_fetches.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Peer files ───────────────────────────────────");
        if !m.peer_mints.is_empty() {
            capsule_runtime::report_println!("minted:     {}", fmt_outcomes(&m.peer_mints));
        }
        if !m.peer_redeems.is_empty() {
            capsule_runtime::report_println!("redeemed:   {}", fmt_outcomes(&m.peer_redeems));
        }
        if !m.peer_fetches.is_empty() {
            capsule_runtime::report_println!("fetched:    {}", fmt_outcomes(&m.peer_fetches));
        }
    }

    if !m.delegations.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Delegations ──────────────────────────────────");
        for d in &m.delegations {
            capsule_runtime::report_println!(
                "{}  {}@{}  {}  {}",
                d.delegation_id.as_deref().unwrap_or("(none)"),
                d.capsule,
                d.version,
                d.child_session_id.as_deref().unwrap_or("(no child)"),
                d.outcome.as_deref().unwrap_or("in flight"),
            );
            // Carried on every outcome that is not `completed`, which is exactly where the
            // runtime writes one: a delegation that did nothing shows why rather than nothing.
            if let Some(reason) = &d.reason {
                capsule_runtime::report_println!("  {reason}");
            }
            // The one join a reader would otherwise have to compose by hand, and the only thing
            // in this file that points outside it. Relative to this capsule's accessible workdir.
            if let (Some(workdir), Some(child)) = (&d.child_workdir, &d.child_session_id) {
                capsule_runtime::report_println!(
                    "  child trace: {workdir}/.murmur/{child}/trace.jsonl"
                );
            }
        }
    }

    if !m.member_calls.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Member calls ─────────────────────────────────");
        for call in &m.member_calls {
            capsule_runtime::report_println!("{}", member_call_show_row(call));
        }
    }

    if !m.plan_runs.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── Plan ─────────────────────────────────────────");
        for run in &m.plan_runs {
            capsule_runtime::report_println!(
                "{}  {}  {} step{}  {}",
                run.plan_id,
                run.outcome.as_deref().unwrap_or("in flight"),
                run.step_count,
                if run.step_count == 1 { "" } else { "s" },
                fmt_dur(run.duration_ms)
            );
            capsule_runtime::report_println!(
                "steps:      {} succeeded, {} failed, {} skipped",
                run.steps_succeeded,
                run.steps_failed,
                run.steps_skipped
            );
            if let Some(step) = &run.failed_step {
                capsule_runtime::report_println!("failed at:  {step}");
            }
            // Written only when the run ended for a reason no step's own line carries — a plan
            // that would not parse, a host that refused the scope, a DAG with nothing to run.
            if let Some(reason) = &run.reason {
                capsule_runtime::report_println!("  {reason}");
            }
            // In authored order, so the DAG reads the way it was written and a step that never
            // ran is still listed. A settled step the `plan_start` does not name — a partial
            // trace, or one holding only step lines — is appended rather than dropped.
            let declared: Vec<&str> = run
                .declared
                .iter()
                .map(|shape| shape.step_id.as_str())
                .collect();
            let extra = run
                .steps
                .iter()
                .map(|step| step.step_id.as_str())
                .filter(|id| !declared.contains(id));
            for step_id in declared.iter().copied().chain(extra) {
                let shape = run.declared.iter().find(|shape| shape.step_id == step_id);
                let settled = run.steps.iter().find(|step| step.step_id == step_id);
                let kind = settled
                    .map(|step| step.kind.as_str())
                    .or(shape.map(|shape| shape.kind.as_str()))
                    .unwrap_or("—");
                let status = match settled {
                    Some(step) => format!("{}  {}", step.status, fmt_dur(step.duration_ms)),
                    None => "not run".to_string(),
                };
                let attempts = match settled {
                    Some(step) if step.attempts > 1 => format!("  {} attempts", step.attempts),
                    _ => String::new(),
                };
                let depends_on = shape
                    .map(|shape| fmt_depends_on(&shape.depends_on))
                    .unwrap_or_default();
                // Marked because it is the one kind of step whose `skipped` needs no failure to
                // explain it: an `if` that evaluated false settles the step without dispatch.
                let conditional = match shape {
                    Some(shape) if shape.has_condition => "  conditional",
                    _ => "",
                };
                capsule_runtime::report_println!(
                    "  {step_id}  {kind}  {status}{attempts}{depends_on}{conditional}"
                );
                if let Some(error) = settled.and_then(|step| step.error.as_deref()) {
                    capsule_runtime::report_println!("    {error}");
                }
            }
        }
    }

    if m.a2a_tasks_received > 0 || !m.a2a_sends.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("── A2A ──────────────────────────────────────────");
        capsule_runtime::report_println!(
            "received:   {} task{}",
            m.a2a_tasks_received,
            if m.a2a_tasks_received == 1 { "" } else { "s" }
        );
        capsule_runtime::report_println!(
            "sent:       {} message{}",
            m.a2a_sends.len(),
            if m.a2a_sends.len() == 1 { "" } else { "s" }
        );
        let mut peers: Vec<&String> = Vec::new();
        for url in &m.a2a_sends {
            if !peers.contains(&url) {
                peers.push(url);
            }
        }
        for url in peers {
            capsule_runtime::report_println!("  → {}", url);
        }
    }
}

// ── Bodies ────────────────────────────────────────────────────────────────────

/// The content-addressed store the runtime writes beside `trace.jsonl` under
/// `trace.capture: content`, one file per distinct body named by its own sha256. Spelled out
/// here because the runtime's own constant is private to `capsule-runtime`.
const BLOB_DIR_NAME: &str = "blobs";

/// The shortest `--body <sha>` prefix that is accepted. Eight hex characters is where a
/// prefix stops being a plausible typo for one of the named selectors.
const SHA_PREFIX_MIN: usize = 8;

/// The length of a full lowercase-hex sha256 — a blob's whole filename.
const SHA_FULL_LEN: usize = 64;

/// One piece of the request a turn put on the wire.
#[derive(Clone, Copy)]
enum WirePiece {
    System,
    Tools,
    Response,
    /// The message at this 0-based position in the turn's `message_shas`.
    Message(usize),
}

impl WirePiece {
    /// How the piece is named in a failure message, reading as prose after a turn number.
    fn label(self) -> String {
        match self {
            WirePiece::System => "system prompt".to_string(),
            WirePiece::Tools => "tool schemas".to_string(),
            WirePiece::Response => "response".to_string(),
            WirePiece::Message(i) => format!("message {i}"),
        }
    }
}

/// What a `--body` argument names.
enum BodySelector {
    /// A piece of one named turn's request, resolved against that turn's `inference` record.
    Piece(WirePiece),
    /// A hash given directly: a full sha256, or a prefix of at least [`SHA_PREFIX_MIN`]
    /// characters naming exactly one hash in the trace.
    Sha(String),
}

fn parse_body_selector(arg: &str) -> Result<BodySelector, CliError> {
    match arg {
        "system" => return Ok(BodySelector::Piece(WirePiece::System)),
        "tools" => return Ok(BodySelector::Piece(WirePiece::Tools)),
        "response" => return Ok(BodySelector::Piece(WirePiece::Response)),
        _ => {}
    }
    if let Some(index) = arg.strip_prefix("message:") {
        let i: usize = index.parse().map_err(|_| {
            CliError::new(
                E_TRC_001,
                format!("--body message:<i> expects a 0-based index, got '{index}'"),
            )
        })?;
        return Ok(BodySelector::Piece(WirePiece::Message(i)));
    }
    let is_hash = arg.len() >= SHA_PREFIX_MIN
        && arg.len() <= SHA_FULL_LEN
        && arg
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if is_hash {
        return Ok(BodySelector::Sha(arg.to_string()));
    }
    Err(CliError::new(
        E_TRC_001,
        format!(
            "unrecognised --body selector '{arg}' — expected system, tools, response, \
             message:<i>, or a sha256 (full, or a prefix of {SHA_PREFIX_MIN}+ characters)"
        ),
    ))
}

/// Every hash a trace names that a blob could be stored under, plus the agent loop's own
/// turns. Built without requiring a `session_end`, so a body can be pulled out of a session
/// that is still running.
struct WireIndex {
    turns: Vec<WireTurn>,
    /// In file order, deduplicated: `session_start.system_prompt_sha256` first, then each
    /// turn's system, tools, response and message hashes.
    known_hashes: Vec<String>,
}

/// One agent-loop turn `--turn` can name, with the task its line belongs to.
struct WireTurn {
    record: InferenceRecord,
    /// `None` on a line that carries no `task_id`.
    task_id: Option<String>,
}

impl WireIndex {
    fn build(records: Vec<TraceRecord>) -> Self {
        let mut turns = Vec::new();
        let mut known_hashes: Vec<String> = Vec::new();
        fn note(hash: Option<&String>, known: &mut Vec<String>) {
            if let Some(h) = hash {
                if !known.iter().any(|k| k == h) {
                    known.push(h.clone());
                }
            }
        }
        for TraceRecord { identity, event } in records {
            match event {
                TraceEvent::SessionStart(e) => {
                    note(e.system_prompt_sha256.as_ref(), &mut known_hashes);
                }
                // A failed call sent no body the runtime kept, so `--turn` never offers it.
                TraceEvent::Inference(e) if !e.is_failed_agent_loop_call() => {
                    let record = InferenceRecord {
                        turn: e.turn,
                        decision: e.decision,
                        origin: e.origin,
                        system_sha: e.system_sha,
                        tools_sha: e.tools_sha,
                        response_sha: e.response_sha,
                        message_shas: e.message_shas,
                        served_by: None,
                        failure: None,
                    };
                    note(record.system_sha.as_ref(), &mut known_hashes);
                    note(record.tools_sha.as_ref(), &mut known_hashes);
                    note(record.response_sha.as_ref(), &mut known_hashes);
                    for sha in &record.message_shas {
                        note(Some(sha), &mut known_hashes);
                    }
                    if record.is_agent_loop() {
                        turns.push(WireTurn {
                            record,
                            task_id: identity.task_id,
                        });
                    }
                }
                _ => {}
            }
        }
        WireIndex {
            turns,
            known_hashes,
        }
    }

    /// Every agent-loop turn numbered `n`, in file order. More than one in a session that ran
    /// several tasks, each numbered from 0, and in a trace whose runtime numbered a continued
    /// task's attempts each from 0.
    fn turns_numbered(&self, n: u32) -> Vec<&WireTurn> {
        self.turns.iter().filter(|t| t.record.turn == n).collect()
    }

    /// `1, 2, 3` — the turns a `--turn` argument can name.
    fn turn_list(&self) -> String {
        self.turns
            .iter()
            .map(|t| t.record.turn.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The hash a selector names, and how that hash is described in a failure message.
fn resolve_body_hash(
    index: &WireIndex,
    arg: &str,
    selector: BodySelector,
    turn: Option<u32>,
    blob_dir: &Path,
) -> Result<(String, String), CliError> {
    let piece = match selector {
        BodySelector::Sha(prefix) => {
            let matches: Vec<&String> = index
                .known_hashes
                .iter()
                .filter(|h| h.starts_with(&prefix))
                .collect();
            return match matches.len() {
                1 => Ok((matches[0].clone(), matches[0].clone())),
                0 => {
                    // A full hash the trace never named is still resolvable when its body is
                    // on disk; otherwise the honest answer is that nothing named it.
                    if prefix.len() == SHA_FULL_LEN && blob_dir.join(&prefix).exists() {
                        Ok((prefix.clone(), prefix.clone()))
                    } else {
                        Err(CliError::new(
                            E_TRC_001,
                            format!("no hash in this trace matches {arg}"),
                        ))
                    }
                }
                n => Err(CliError::new(
                    E_TRC_001,
                    format!(
                        "{arg} matches {n} hashes in this trace — provide more characters\n{}",
                        matches
                            .iter()
                            .map(|h| format!("  {h}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                )),
            };
        }
        BodySelector::Piece(piece) => piece,
    };

    let Some(n) = turn else {
        let turns = if index.turns.is_empty() {
            "this trace has no inference records".to_string()
        } else {
            format!("this trace has turns {}", index.turn_list())
        };
        return Err(CliError::new(
            E_TRC_001,
            format!("--turn is required with --body {arg}; {turns}"),
        ));
    };
    let matches = index.turns_numbered(n);
    let Some(first) = matches.first() else {
        return Err(CliError::new(
            E_TRC_001,
            format!("turn {n} has no inference record in this trace"),
        ));
    };
    if matches.len() > 1 && matches.iter().any(|t| t.record.has_hashes()) {
        return Err(ambiguous_turn(n, &matches));
    }
    let record = &first.record;
    if !record.has_hashes() {
        return Err(CliError::new(
            E_TRC_001,
            format!(
                "turn {n} recorded no content hashes — the session ran under trace.capture: none"
            ),
        ));
    }

    let hash = match piece {
        WirePiece::System => record.system_sha.clone(),
        WirePiece::Tools => record.tools_sha.clone(),
        WirePiece::Response => record.response_sha.clone(),
        WirePiece::Message(i) => {
            if i >= record.message_shas.len() {
                return Err(CliError::new(
                    E_TRC_001,
                    format!(
                        "turn {n} recorded {} message{}; there is no message {i}",
                        record.message_shas.len(),
                        if record.message_shas.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                ));
            }
            Some(record.message_shas[i].clone())
        }
    };
    let label = piece.label();
    let hash =
        hash.ok_or_else(|| CliError::new(E_TRC_001, format!("turn {n} recorded no {label} hash")))?;
    Ok((hash.clone(), format!("turn {n} {label} {hash}")))
}

/// The refusal for a `--turn` that names several agent-loop turns, listing each one's hashes so
/// the reader can name exactly one with a `<sha256>` selector instead.
fn ambiguous_turn(n: u32, matches: &[&WireTurn]) -> CliError {
    fn hash(sha: &Option<String>) -> &str {
        sha.as_deref().unwrap_or("-")
    }
    let rows = matches
        .iter()
        .map(|t| {
            let task = t
                .task_id
                .as_ref()
                .map(|id| format!("task {id}  "))
                .unwrap_or_default();
            format!(
                "  {task}system {}  response {}",
                hash(&t.record.system_sha),
                hash(&t.record.response_sha)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    CliError::new(
        E_TRC_001,
        format!(
            "--turn {n} names {} turns in this trace: turn numbers restart at 0 for each task \
             in a session that ran several, and a trace written by a runtime that restarted \
             them when a task continued repeats them within one task\n{rows}\n\
             pass one of these hashes to --body instead of --turn",
            matches.len()
        ),
    )
}

/// Print the recorded body behind one hash to stdout, and nothing else — no header, no added
/// newline — so the output pipes into `sha256sum` and matches the blob's own name.
fn print_body(path: &Path, arg: &str, turn: Option<u32>) -> Result<(), CliError> {
    let selector = parse_body_selector(arg)?;
    let index = WireIndex::build(parse_trace_records(path)?);
    let blob_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BLOB_DIR_NAME);
    let (hash, described) = resolve_body_hash(&index, arg, selector, turn, &blob_dir)?;

    let blob = blob_dir.join(&hash);
    let bytes = fs::read(&blob).map_err(|e| match e.kind() {
        // The hash is recorded, so the session ran under `meta` or better; a body that is not
        // on disk means no body was ever stored, not that a file went missing.
        std::io::ErrorKind::NotFound => CliError::new(
            E_TRC_001,
            format!("{described}: recorded under capture: meta; no body was stored"),
        ),
        _ => CliError::new(E_IO_003, format!("failed to read {}: {e}", blob.display())),
    })?;

    // A reader that stops early has the bytes it wanted; any other refusal is reported by `main`.
    capsule_runtime::diagnostic::report_to_stdout(&bytes);
    Ok(())
}

pub(crate) fn run_trace_show(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
    body: Option<String>,
    turn: Option<u32>,
) -> Result<(), CliError> {
    let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let given_workdir = workdir_arg.is_some();
    let workdir = workdir_arg.unwrap_or_else(|| current_dir.join("workdir"));
    if let Some(formation) = session
        .as_deref()
        .filter(|arg| arg.starts_with(FORMATION_ID_PREFIX))
    {
        if body.is_some() || turn.is_some() {
            return Err(CliError::new(
                E_TRC_001,
                "--body and --turn read one session's trace; a formation id names several sessions",
            ));
        }
        // With no `--workdir`, the directory a roster runs from is searched too: its entry
        // member's `--workdir` is the roster's directory, so it records under `./.murmur`.
        let entry_root = (!given_workdir).then(|| current_dir.join(".murmur"));
        return run_formation_show(formation, &workdir, entry_root);
    }
    if body.is_none() && turn.is_some() {
        return Err(CliError::new(
            E_TRC_001,
            "--turn has no meaning without --body",
        ));
    }
    let path = resolve_session(session, &workdir)?;
    if let Some(arg) = &body {
        return print_body(&path, arg, turn);
    }
    let (metrics, tasks) = load_metrics(&path)?;
    print_show(&metrics);
    if tasks.len() > 1 {
        capsule_runtime::report_println!("── Tasks ───────────────────────────────────────");
        for (i, t) in tasks.iter().enumerate() {
            let short_id = if t.task_id.len() >= 12 {
                &t.task_id[..12]
            } else {
                &t.task_id
            };
            let reopen_note = if t.reopen_count > 0 {
                format!("  reopens: {}", t.reopen_count)
            } else {
                String::new()
            };
            capsule_runtime::report_println!(
                "task {}  {}  turns: {}  in: {}  out: {}  {}  {}{}",
                i + 1,
                short_id,
                t.turns,
                fmt_thousands(t.input_tokens),
                fmt_thousands(t.output_tokens),
                t.exit_status,
                fmt_dur(t.duration_ms),
                reopen_note
            );
        }
    }
    if let Some(formation) = &metrics.formation_id {
        // Written by a runtime, so it parses; a value that does not names no formation to list.
        if let (Ok(formation), Some(root)) = (FormationId::parse(formation), session_root(&path)) {
            let search = search_formation(&root, formation_member_roots(&formation), &formation)?;
            capsule_runtime::report_println!();
            print_formation(&formation, &search);
        }
    }
    Ok(())
}

/// `mur trace show frm_<id>`: every session under `root`, under `entry_root` when there is one,
/// and under each of the formation's peer directories in the murmur home, whose `session_start`
/// names the formation, and how each ended.
fn run_formation_show(arg: &str, root: &Path, entry_root: Option<PathBuf>) -> Result<(), CliError> {
    let formation = FormationId::parse(arg)
        .map_err(|_| CliError::new(E_TRC_002, format!("'{arg}' is not a formation id")))?;
    let more = entry_root
        .into_iter()
        .chain(formation_member_roots(&formation))
        .collect();
    let search = search_formation(root, more, &formation)?;
    if search.found.members.is_empty() {
        let roots: Vec<String> = search
            .searched
            .iter()
            .map(|searched| searched.root.display().to_string())
            .collect();
        return Err(CliError::new(
            E_TRC_002,
            format!(
                "no session under {} belongs to formation {formation}",
                roots.join(", ")
            ),
        ));
    }
    print_formation(&formation, &search);
    Ok(())
}

/// The session root a resolved `trace.jsonl` sits in: the parent of its session directory.
fn session_root(trace: &Path) -> Option<PathBuf> {
    let session_dir = trace.parent().filter(|dir| !dir.as_os_str().is_empty());
    match session_dir.and_then(Path::parent) {
        Some(root) if !root.as_os_str().is_empty() => Some(root.to_path_buf()),
        _ => fs::canonicalize(trace)
            .ok()?
            .parent()?
            .parent()
            .map(Path::to_path_buf),
    }
}

/// The Formation section: a `searched:` line per root, one row per member found under any of
/// them by session id, then every `call-member` call they record.
///
/// Follows no delegation edge out of the searched roots, so a member's child recorded elsewhere
/// is counted and pointed at rather than listed.
fn print_formation(formation: &FormationId, search: &FormationSearch) {
    capsule_runtime::report_println!("── Formation ────────────────────────────────────");
    capsule_runtime::report_println!("formation:  {formation}");
    for line in searched_lines(search) {
        capsule_runtime::report_println!("{line}");
    }
    let found = &search.found;
    for line in formation_member_lines(&found.members) {
        capsule_runtime::report_println!("{line}");
    }
    if !found.children_elsewhere.is_empty() {
        capsule_runtime::report_println!(
            "delegated children not under these roots: {} — `mur trace show <member>` names each child trace",
            found.children_elsewhere.len()
        );
    }
    let calls = formation_calls(&found.members);
    if !calls.is_empty() {
        capsule_runtime::report_println!("calls:      {}", calls.len());
        for line in formation_call_lines(&calls) {
            capsule_runtime::report_println!("{line}");
        }
    }
}

/// One row per formation member: its session id, its roster name (`-` for a session with none),
/// its capsule and how it ended, then who delegated it and whom it may call, in aligned columns.
fn formation_member_lines(members: &[RecordedMember]) -> Vec<String> {
    let name_width = members
        .iter()
        .map(|member| member.member.as_deref().unwrap_or("-").chars().count())
        .max()
        .unwrap_or(0);
    let capsule_width = members
        .iter()
        .map(|member| member.capsule.chars().count())
        .max()
        .unwrap_or(0);
    members
        .iter()
        .map(|member| {
            let mut line = format!(
                "{:<36}  {:<name_width$}  {:<capsule_width$}  {}",
                member.session_id,
                member.member.as_deref().unwrap_or("-"),
                member.capsule,
                member.exit_status.as_deref().unwrap_or("no session_end"),
            );
            if let Some(parent) = &member.spawned_by {
                line.push_str(&format!("  spawned by {parent}"));
            }
            if !member.callees.is_empty() {
                line.push_str(&format!("  may call {}", member.callees.join(", ")));
            }
            line
        })
        .collect()
}

/// One row per formation call, indented two spaces per depth: the call id, `caller → callee`, how
/// it ended, its duration, whether its answer was delivered, and the note for a missing side.
fn formation_call_lines(calls: &[FormationCall]) -> Vec<String> {
    let cells: Vec<[String; 6]> = calls
        .iter()
        .map(|call| {
            let id = format!(
                "{}{}",
                "  ".repeat(call.depth),
                call.call_id.as_deref().unwrap_or("(no call id)")
            );
            let pair = format!("{} → {}", call.caller, call.callee);
            let (status, duration, delivery) = match &call.ending {
                CallEnding::Ended {
                    status,
                    duration_ms,
                    delivered,
                } => (
                    status.clone(),
                    fmt_dur(*duration_ms),
                    if *delivered {
                        "delivered"
                    } else {
                        "not delivered"
                    },
                ),
                CallEnding::Outstanding => ("outstanding".to_string(), "-".to_string(), ""),
                CallEnding::Unknown => ("unknown".to_string(), "-".to_string(), ""),
            };
            let note = call
                .gap
                .as_ref()
                .map(|gap| gap.note(&call.caller, &call.callee))
                .into_iter()
                .chain((call.busy_offers > 0).then(|| busy_note(call.busy_offers)))
                .collect::<Vec<_>>()
                .join("; ");
            [id, pair, status, duration, delivery.to_string(), note]
        })
        .collect();
    let width = |column: usize| {
        cells
            .iter()
            .map(|row| row[column].chars().count())
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2), width(3), width(4)];
    cells
        .iter()
        .map(|row| {
            let mut line = String::new();
            for (cell, width) in row.iter().zip(widths) {
                line.push_str(&format!("{cell:<width$}  "));
            }
            line.push_str(&row[5]);
            line.trim_end().to_string()
        })
        .collect()
}

/// One `searched:` line per root a formation search read, naming why a root could not be read.
fn searched_lines(search: &FormationSearch) -> Vec<String> {
    search
        .searched
        .iter()
        .map(|searched| match &searched.unreadable {
            None => format!("searched:   {}", searched.root.display()),
            Some(why) => format!(
                "searched:   {}  could not be read: {why}",
                searched.root.display()
            ),
        })
        .collect()
}

/// `mur trace steps` with a formation id: it walks one session's turns, and a formation is
/// several sessions.
fn steps_refuses_a_formation(formation: &str) -> CliError {
    CliError::new(
        E_TRC_001,
        format!(
            "mur trace steps reads one session's trace; a formation id names several sessions — \
             `mur trace show {formation}` lists the formation's members and calls"
        ),
    )
}

pub(crate) fn run_trace_steps(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
    verbose: bool,
) -> Result<(), CliError> {
    if let Some(formation) = session
        .as_deref()
        .filter(|arg| arg.starts_with(FORMATION_ID_PREFIX))
    {
        return Err(steps_refuses_a_formation(formation));
    }
    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_session(session, &workdir)?;
    let records = parse_trace_records(&path)?;
    if records.is_empty() {
        return Err(CliError::new(
            E_TRC_001,
            format!(
                "{}: trace file is empty (incomplete or zero-event session)",
                path.display()
            ),
        ));
    }

    // A trace carrying no `event_id` on any line has no tree to walk, so it renders flat.
    if records.iter().any(|r| r.identity.event_id.is_some()) {
        print_steps_tree(&records, verbose);
    } else {
        print_steps_flat(&records, verbose);
    }
    Ok(())
}

/// The dependency edges a plan step waited on, as they read on its `steps` row. Empty for a step
/// that depends on nothing.
fn fmt_depends_on(depends_on: &[String]) -> String {
    if depends_on.is_empty() {
        String::new()
    } else {
        format!("  after {}", depends_on.join(", "))
    }
}

/// The column a `steps` tree row's own detail starts in, after the event type that opens it.
const STEPS_KIND_WIDTH: usize = 11;

/// One row of the `steps` tree, or `None` for a line the tree does not render — its children
/// (it has none) would hang off its own parent.
fn steps_row(record: &TraceRecord, verbose: bool) -> Option<String> {
    // Every row but a turn's own opens with the event type it came from, padded so the rows
    // under one turn line up; a name past the column keeps a single separating space.
    let kind = |name: &str| {
        if name.len() >= STEPS_KIND_WIDTH {
            format!("{name} ")
        } else {
            format!("{name:<STEPS_KIND_WIDTH$}")
        }
    };
    Some(match &record.event {
        TraceEvent::TaskStart(e) => {
            let provenance = match (e.origin.is_empty(), e.trust.is_empty()) {
                (true, true) => String::new(),
                (false, false) => format!("{}/{}", e.origin, e.trust),
                (false, true) => e.origin.clone(),
                (true, false) => e.trust.clone(),
            };
            let lane = if e.lane.is_empty() {
                String::new()
            } else {
                format!("lane {}", e.lane)
            };
            let delegation = match &e.delegation_id {
                Some(id) if !id.is_empty() => format!("delegation {id}"),
                _ => String::new(),
            };
            let annotations: Vec<&str> = [
                e.source.as_str(),
                provenance.as_str(),
                lane.as_str(),
                delegation.as_str(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
            let annotation = if annotations.is_empty() {
                String::new()
            } else {
                format!("  ({})", annotations.join(", "))
            };
            let context = if e.context_id.is_empty() {
                String::new()
            } else {
                format!("  {}", fmt_id_short(&e.context_id, 12))
            };
            format!(
                "task {}{}{}",
                fmt_id_short(&e.task_id, 12),
                context,
                annotation
            )
        }
        TraceEvent::Inference(e) => match &e.origin {
            // A hook's completion is not a turn of the agent loop; it hangs off the turn it
            // ran inside.
            Some(origin) => match e.failure() {
                Some(failure) => format!(
                    "{}{}  {}  {}",
                    kind("inference"),
                    origin,
                    e.decision,
                    failure.label()
                ),
                None => format!("{}{}  {}", kind("inference"), origin, e.decision),
            },
            None => {
                if let Some(failure) = e.failure() {
                    return Some(format!("turn {}  error  {}", e.turn, failure.label()));
                }
                // A turn the provider cut off at the output cap, named on the line rather than
                // left to be read out of the raw event.
                let capped = match e.stop_reason.as_deref() {
                    Some(MAX_TOKENS_STOP_REASON) => TRUNCATED_TURN_MARKER,
                    _ => "",
                };
                // The driver choice that served the turn, for a capsule that declares
                // alternates, so a cost change between two turns can be read off the list.
                let served = match (&e.driver_choice, &e.model) {
                    (Some(choice), Some(model)) => format!("  on {choice} ({model})"),
                    (Some(choice), None) => format!("  on {choice}"),
                    _ => String::new(),
                };
                match &e.tool_name {
                    Some(tool) => format!(
                        "turn {}  {}  {}{}{}",
                        e.turn, e.decision, tool, capped, served
                    ),
                    None => format!("turn {}  {}{}{}", e.turn, e.decision, capped, served),
                }
            }
        },
        TraceEvent::ToolCall(e) => format!(
            "{}{}  {}  {}{}",
            kind("tool_call"),
            e.tool_name,
            fmt_dur(e.duration_ms),
            if e.status == "ok" { "✓" } else { "✗" },
            if verbose {
                e.input
                    .as_ref()
                    .map(|v| {
                        let summary = extract_input_summary(v);
                        if summary.is_empty() {
                            String::new()
                        } else {
                            format!("  {summary}")
                        }
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            }
        ),
        TraceEvent::Shell(e) => format!(
            "{}{}  exit {}  {}",
            kind("shell"),
            e.binary.as_deref().unwrap_or("—"),
            e.exit_code,
            fmt_dur(e.duration_ms)
        ),
        TraceEvent::ShellDetached(e) => format!(
            "{}{}  {}  detached after {}",
            kind("shell_detached"),
            e.binary.as_deref().unwrap_or("—"),
            fmt_id_short(&e.work_id, 12),
            fmt_dur(e.grace_ms)
        ),
        TraceEvent::ShellCompleted(e) => format!(
            "{}{}  {}  exit {} {}  {}  {}",
            kind("shell_completed"),
            e.binary.as_deref().unwrap_or("—"),
            fmt_id_short(&e.work_id, 12),
            e.exit_code,
            if e.status == "ok" { "✓" } else { "✗" },
            fmt_dur(e.duration_ms),
            e.output_path
        ),
        TraceEvent::ShellAbandoned(e) => format!(
            "{}{}  {}  still running after {}  result lost",
            kind("shell_abandoned"),
            e.binary.as_deref().unwrap_or("—"),
            fmt_id_short(&e.work_id, 12),
            fmt_dur(e.running_ms)
        ),
        TraceEvent::ShellLost(e) => format!(
            "{}{}  {}  detached at {}  no result, reported by {}",
            kind("shell_lost"),
            e.binary.as_deref().unwrap_or("—"),
            fmt_id_short(&e.work_id, 12),
            e.detached_at_ms,
            fmt_id_short(&e.reconciled_by_session, 12)
        ),
        TraceEvent::SkillCall(e) => format!(
            "{}{}  {}  {}{}",
            kind("skill_call"),
            e.skill_name,
            fmt_dur(e.duration_ms),
            if e.status == "ok" { "✓" } else { "✗" },
            e.untrusted_marker()
                .map(|marker| format!(" ({marker})"))
                .unwrap_or_default()
        ),
        TraceEvent::ArtifactPulled(e) => format!(
            "{}{}@{} ({}/{})",
            kind("artifact_pulled"),
            e.name,
            e.version,
            e.origin,
            e.trust
        ),
        TraceEvent::Compaction(e) => format!(
            "{}{} → {} tokens",
            kind("compaction"),
            fmt_thousands(e.tokens_before),
            fmt_thousands(e.tokens_after)
        ),
        TraceEvent::CompactionDeclined(e) => format!(
            "{}{} tokens  {}",
            kind("compaction_declined"),
            fmt_thousands(e.tokens),
            e.reason
        ),
        TraceEvent::ContextSeed(e) => format!(
            "{}{}  {}  {} tokens",
            kind("context_seed"),
            e.hook_name,
            e.outcome,
            fmt_thousands(e.tokens)
        ),
        TraceEvent::TaskReopened(e) => format!(
            "{}{}  reopen {}{}",
            kind("task_reopened"),
            e.hook_name,
            e.reopen_number,
            e.attempt_context
                .as_deref()
                .map(|context| format!("  {context}"))
                .unwrap_or_default()
        ),
        TraceEvent::TaskCanceled(e) => format!(
            "{}{}  {}",
            kind("task_canceled"),
            e.phase,
            cancel_residue(&e.detached_work_ids, &e.delegation_ids)
        ),
        TraceEvent::TaskRejected(e) => format!(
            "{}{}  {}",
            kind("task_rejected"),
            e.cause,
            fmt_id_short(&e.task_id, 12)
        ),
        TraceEvent::FormationEnded(e) => format!("{}{}", kind("formation_ended"), e.formation_id),
        TraceEvent::SpawnerEnded(e) => format!(
            "{}{}",
            kind("spawner_ended"),
            e.spawned_by.as_deref().unwrap_or("-")
        ),
        TraceEvent::MemberCallBusy(e) => format!(
            "{}{}  {}  offer {} turned away busy",
            kind("member_call_busy"),
            e.member,
            e.call_id,
            e.offer
        ),
        TraceEvent::MemberCallStart(e) => format!(
            "{}{}  {}  task {}",
            kind("member_call_start"),
            e.member,
            e.call_id,
            e.member_task_id
        ),
        TraceEvent::MemberCall(e) => format!(
            "{}{}  {}  {}  {}{}",
            kind("member_call"),
            e.member,
            e.call_id,
            e.status,
            fmt_dur(e.duration_ms),
            if e.delivered { "" } else { "  not delivered" }
        ),
        TraceEvent::TaskFailed(e) => {
            let reason: String = e.reason.chars().take(120).collect();
            format!("{}{}  {reason}", kind("task_failed"), e.cause)
        }
        TraceEvent::CallDenied(e) => format!(
            "{}{}  {}  denied by {}",
            kind("call_denied"),
            e.event,
            e.target,
            e.hook_name
        ),
        TraceEvent::ToolInputRefused(e) => format!(
            "{}{}  missing {}",
            kind("tool_input_refused"),
            e.tool_name,
            e.missing.join(", ")
        ),
        TraceEvent::SpendCeilingReached(e) => format!(
            "{}{}  used {} of {}  needs {}{}",
            kind("spend_ceiling_reached"),
            e.limit,
            fmt_thousands(e.used),
            fmt_thousands(e.ceiling),
            fmt_thousands(e.requested),
            e.origin
                .as_deref()
                .map(|origin| format!("  {origin}"))
                .unwrap_or_default()
        ),
        TraceEvent::ProtectedPathDenied(e) => format!(
            "{}{}  {}  rule {}",
            kind("protected_path_denied"),
            e.call,
            e.path,
            e.rule
        ),
        TraceEvent::PlanStart(e) => format!(
            "{}{}  {} step{}",
            kind("plan_start"),
            e.plan_id,
            e.step_count,
            if e.step_count == 1 { "" } else { "s" }
        ),
        TraceEvent::PlanStepStart(e) => format!(
            "{}{}  {}{}",
            kind("plan_step_start"),
            e.step_id,
            e.kind,
            fmt_depends_on(&e.depends_on)
        ),
        TraceEvent::PlanStep(e) => format!(
            "{}{}  {}  {}  {}",
            kind("plan_step"),
            e.step_id,
            e.kind,
            e.status,
            fmt_dur(e.duration_ms)
        ),
        TraceEvent::ControlChange(e) => {
            format!("{}{}", kind("control_change"), control_change_line(e, None))
        }
        TraceEvent::ControlRefused(e) => format!(
            "{}{} {}{}{}",
            kind("control_refused"),
            e.status,
            e.reason,
            e.name
                .as_deref()
                .map(|name| format!("  {name}"))
                .unwrap_or_default(),
            e.principal
                .as_deref()
                .map(|principal| format!("  by {principal}"))
                .unwrap_or_default()
        ),
        TraceEvent::ControlApplied(e) => format!(
            "{}{} = {}  turn {}  from {}",
            kind("control_applied"),
            e.name,
            e.value,
            e.turn,
            fmt_id_short(&e.change_id, 12)
        ),
        TraceEvent::ToolsRefreshed(e) => {
            format!(
                "{}{}  turn {}",
                kind("tools_refreshed"),
                e.changes(),
                e.turn
            )
        }
        TraceEvent::PlanEnd(e) => format!(
            "{}{}{}",
            kind("plan_end"),
            e.outcome,
            match &e.failed_step {
                Some(step) => format!("  failed at {step}"),
                None => String::new(),
            }
        ),
        _ => return None,
    })
}

/// Render the session → task → turn tree by following `parent_id`, one indent level per
/// rendered ancestor, in file order within each parent.
fn print_steps_tree(records: &[TraceRecord], verbose: bool) {
    let mut session_id = String::new();
    let mut tasks = 0usize;
    let mut turns = 0usize;
    // `event_id` → index, and `task_id` → the index of that task's `task_start`, which is
    // how a turn-level line whose `parent_id` names nothing in this file is still attributed.
    let mut by_event_id: HashMap<&str, usize> = HashMap::new();
    let mut task_nodes: HashMap<&str, usize> = HashMap::new();
    for (i, record) in records.iter().enumerate() {
        if let Some(id) = &record.identity.event_id {
            by_event_id.insert(id.as_str(), i);
        }
        match &record.event {
            TraceEvent::SessionStart(e) => session_id = e.session_id.clone(),
            TraceEvent::TaskStart(e) => {
                tasks += 1;
                task_nodes.insert(e.task_id.as_str(), i);
            }
            TraceEvent::Inference(e) if e.is_steps_turn() => turns += 1,
            _ => {}
        }
    }

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, record) in records.iter().enumerate() {
        let parent = record
            .identity
            .parent_id
            .as_deref()
            .and_then(|pid| by_event_id.get(pid).copied())
            .or_else(|| {
                record
                    .identity
                    .task_id
                    .as_deref()
                    .and_then(|tid| task_nodes.get(tid).copied())
                    .filter(|&t| t != i)
            });
        match parent {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }

    let task_note = match tasks {
        0 => String::new(),
        1 => "1 task, ".to_string(),
        n => format!("{n} tasks, "),
    };
    capsule_runtime::report_println!(
        "Session {}  ({}{} turn{})",
        session_id,
        task_note,
        turns,
        if turns == 1 { "" } else { "s" }
    );

    for root in roots {
        walk_steps_tree(records, &children, root, 0, verbose);
    }
    capsule_runtime::report_println!();
}

fn walk_steps_tree(
    records: &[TraceRecord],
    children: &HashMap<usize, Vec<usize>>,
    index: usize,
    depth: usize,
    verbose: bool,
) {
    let child_depth = match steps_row(&records[index], verbose) {
        Some(row) => {
            if matches!(records[index].event, TraceEvent::TaskStart(_)) {
                capsule_runtime::report_println!();
            }
            capsule_runtime::report_println!("{}{}", "  ".repeat(depth), row);
            depth + 1
        }
        // A line the tree does not render — `session_start` itself, and the session-level
        // records `mur trace show` covers — leaves its children at its own depth.
        None => depth,
    };
    if let Some(kids) = children.get(&index) {
        for &child in kids {
            walk_steps_tree(records, children, child, child_depth, verbose);
        }
    }
}

/// The turn-per-row table `steps` prints for a trace carrying no identity fields.
fn print_steps_flat(records: &[TraceRecord], verbose: bool) {
    let mut session_id = String::new();
    let mut inferences: Vec<(u32, String, Option<String>)> = Vec::new();
    let mut tool_durations: HashMap<u32, u64> = HashMap::new();
    let mut tool_inputs: HashMap<u32, String> = HashMap::new();

    for record in records {
        match &record.event {
            TraceEvent::SessionStart(e) => session_id = e.session_id.clone(),
            TraceEvent::Inference(e) => {
                inferences.push((e.turn, e.decision.clone(), e.tool_name.clone()));
            }
            TraceEvent::ToolCall(e) => {
                tool_durations.entry(e.turn).or_insert(e.duration_ms);
                if verbose {
                    if let Some(input) = &e.input {
                        let summary = extract_input_summary(input);
                        if !summary.is_empty() {
                            tool_inputs.entry(e.turn).or_insert(summary);
                        }
                    }
                }
            }
            TraceEvent::SkillCall(e) => {
                tool_durations.entry(e.turn).or_insert(e.duration_ms);
            }
            _ => {}
        }
    }

    let n = inferences.len();
    capsule_runtime::report_println!(
        "Session {}  ({} turn{})",
        session_id,
        n,
        if n == 1 { "" } else { "s" }
    );
    capsule_runtime::report_println!();

    // Build rows first so we can compute max tool name width for alignment.
    let rows: Vec<(u32, String, String, String, String)> = inferences
        .iter()
        .map(|(turn, decision, tool_name)| {
            let tool_display = tool_name.as_deref().unwrap_or("—").to_string();
            let dur_str = match tool_durations.get(turn) {
                Some(&ms) => fmt_dur(ms),
                None => "—".to_string(),
            };
            let input_summary = if verbose {
                tool_inputs.get(turn).cloned().unwrap_or_default()
            } else {
                String::new()
            };
            (
                *turn,
                decision.clone(),
                tool_display,
                dur_str,
                input_summary,
            )
        })
        .collect();

    let max_tool_width = rows
        .iter()
        .map(|(_, _, t, _, _)| t.chars().count())
        .max()
        .unwrap_or(0)
        .max(12);

    for (turn, decision, tool_display, dur_str, input_summary) in &rows {
        let tool_padded = format!("{:<width$}", tool_display, width = max_tool_width);
        if verbose && !input_summary.is_empty() {
            capsule_runtime::report_println!(
                "  {:<3}{:<13}{}{:<5}   {}",
                turn,
                decision,
                tool_padded,
                dur_str,
                input_summary
            );
        } else {
            capsule_runtime::report_println!(
                "  {:<3}{:<13}{}{}",
                turn,
                decision,
                tool_padded,
                dur_str
            );
        }
    }

    capsule_runtime::report_println!();
}

fn extract_input_summary(v: &serde_json::Value) -> String {
    fn first_string(v: &serde_json::Value) -> Option<&str> {
        match v {
            serde_json::Value::String(s) => Some(s.as_str()),
            serde_json::Value::Object(m) => m.values().find_map(first_string),
            serde_json::Value::Array(a) => a.iter().find_map(first_string),
            _ => None,
        }
    }
    match first_string(v) {
        None => String::new(),
        Some(s) => {
            let end = s.char_indices().nth(60).map(|(i, _)| i);
            match end {
                Some(i) => format!("\"{}…\"", &s[..i]),
                None => format!("\"{}\"", s),
            }
        }
    }
}

fn print_session_block(m: &TraceMetrics) {
    capsule_runtime::report_println!(
        "Session {}  {}  {} turn{}",
        fmt_session_short(&m.session_id),
        fmt_dur(m.duration_ms),
        m.total_turns,
        if m.total_turns == 1 { "" } else { "s" }
    );
    capsule_runtime::report_println!();

    if m.tool_error > 0 {
        capsule_runtime::report_println!(
            "  {:<14} {}  ({} ok, {} error)",
            "Tool calls:",
            m.total_tool_calls,
            m.tool_ok,
            m.tool_error
        );
    } else {
        capsule_runtime::report_println!("  {:<14} {}", "Tool calls:", m.total_tool_calls);
    }

    if m.total_shell_calls > 0 {
        capsule_runtime::report_println!(
            "  {:<14} {}  exit codes: {}",
            "Shell calls:",
            m.total_shell_calls,
            fmt_exit_codes(&m.shell_exit_codes)
        );
    }

    if !m.redundant_calls.is_empty() {
        capsule_runtime::report_println!("  Redundant calls: {}", m.redundant_calls.len());
    }

    if let Some(avg_tool) = m.avg_tool_latency_ms() {
        let shell_part = m
            .avg_shell_latency_ms()
            .map(|v| format!("  shell {}", fmt_dur(v as u64)))
            .unwrap_or_default();
        capsule_runtime::report_println!(
            "  {:<14} tool {}{}",
            "Avg latency:",
            fmt_dur(avg_tool as u64),
            shell_part
        );
    }

    capsule_runtime::report_println!();
}

// ── Diff ──────────────────────────────────────────────────────────────────────

fn delta_u64(a: u64, b: u64, lower_is_better: bool) -> String {
    if a == b {
        return "=".to_string();
    }
    let diff = b as i64 - a as i64;
    let indicator = if lower_is_better {
        if diff < 0 {
            " (B better)"
        } else {
            " (A better)"
        }
    } else if diff > 0 {
        " (B better)"
    } else {
        " (A better)"
    };
    format!("{:+}{}", diff, indicator)
}

fn delta_ms(a_ms: u64, b_ms: u64) -> String {
    if a_ms == b_ms {
        return "=".to_string();
    }
    let diff = b_ms as i64 - a_ms as i64;
    let abs_diff = diff.unsigned_abs();
    let indicator = if diff < 0 {
        " (B better)"
    } else {
        " (A better)"
    };
    let sign = if diff < 0 { "-" } else { "+" };
    format!("{}{}{}", sign, fmt_dur(abs_diff), indicator)
}

fn delta_f(a: f64, b: f64, lower_is_better: bool) -> String {
    let diff = b - a;
    if diff.abs() < 0.05 {
        return "=".to_string();
    }
    let indicator = if lower_is_better {
        if diff < 0.0 {
            " (B better)"
        } else {
            " (A better)"
        }
    } else if diff > 0.0 {
        " (B better)"
    } else {
        " (A better)"
    };
    format!("{:+.1}{}", diff, indicator)
}

fn print_diff(a: &TraceMetrics, b: &TraceMetrics) {
    const COL: usize = 22;
    const VAL: usize = 16;

    capsule_runtime::report_println!(
        "{:<COL$} {:<VAL$} {:<VAL$} Delta",
        "Metric",
        "Run A",
        "Run B"
    );
    capsule_runtime::report_println!(
        "{} {} {} {}",
        "─".repeat(COL),
        "─".repeat(VAL),
        "─".repeat(VAL),
        "─".repeat(26)
    );

    macro_rules! row {
        ($label:expr, $va:expr, $vb:expr, $delta:expr) => {
            capsule_runtime::report_println!(
                "{:<COL$} {:<VAL$} {:<VAL$} {}",
                $label,
                $va.to_string(),
                $vb.to_string(),
                $delta
            );
        };
    }

    row!(
        "turns",
        a.total_turns,
        b.total_turns,
        delta_u64(a.total_turns as u64, b.total_turns as u64, true)
    );
    row!(
        "duration",
        fmt_dur(a.duration_ms),
        fmt_dur(b.duration_ms),
        delta_ms(a.duration_ms, b.duration_ms)
    );
    row!(
        "input tokens",
        fmt_thousands(a.total_input_tokens),
        fmt_thousands(b.total_input_tokens),
        delta_u64(a.total_input_tokens, b.total_input_tokens, true)
    );
    row!(
        "output tokens",
        fmt_thousands(a.total_output_tokens),
        fmt_thousands(b.total_output_tokens),
        delta_u64(a.total_output_tokens, b.total_output_tokens, true)
    );

    let ai = a.avg_input_per_turn().unwrap_or(0.0);
    let bi = b.avg_input_per_turn().unwrap_or(0.0);
    row!(
        "input/turn (avg)",
        format!("{:.0}", ai),
        format!("{:.0}", bi),
        delta_f(ai, bi, true)
    );

    let ao = a.avg_output_per_turn().unwrap_or(0.0);
    let bo = b.avg_output_per_turn().unwrap_or(0.0);
    row!(
        "output/turn (avg)",
        format!("{:.0}", ao),
        format!("{:.0}", bo),
        delta_f(ao, bo, true)
    );

    row!(
        "tool calls",
        a.total_tool_calls,
        b.total_tool_calls,
        delta_u64(a.total_tool_calls as u64, b.total_tool_calls as u64, true)
    );

    let ar = a
        .tool_success_rate()
        .map(|r| format!("{:.1}%", r))
        .unwrap_or_else(|| "—".to_string());
    let br = b
        .tool_success_rate()
        .map(|r| format!("{:.1}%", r))
        .unwrap_or_else(|| "—".to_string());
    let rate_delta = match (a.tool_success_rate(), b.tool_success_rate()) {
        (Some(av), Some(bv)) => delta_f(av, bv, false),
        _ => "—".to_string(),
    };
    row!("tool success rate", ar, br, rate_delta);

    let al = fmt_opt_dur(a.avg_tool_latency_ms());
    let bl = fmt_opt_dur(b.avg_tool_latency_ms());
    let tool_lat_delta = match (a.avg_tool_latency_ms(), b.avg_tool_latency_ms()) {
        (Some(av), Some(bv)) => delta_ms(av as u64, bv as u64),
        _ => "—".to_string(),
    };
    row!("avg tool latency", al, bl, tool_lat_delta);

    row!(
        "skill calls",
        a.total_skill_calls(),
        b.total_skill_calls(),
        delta_u64(
            a.total_skill_calls() as u64,
            b.total_skill_calls() as u64,
            true
        )
    );

    row!(
        "shell calls",
        a.total_shell_calls,
        b.total_shell_calls,
        delta_u64(a.total_shell_calls as u64, b.total_shell_calls as u64, true)
    );

    let asl = fmt_opt_dur(a.avg_shell_latency_ms());
    let bsl = fmt_opt_dur(b.avg_shell_latency_ms());
    let shell_lat_delta = match (a.avg_shell_latency_ms(), b.avg_shell_latency_ms()) {
        (Some(av), Some(bv)) => delta_ms(av as u64, bv as u64),
        _ => "—".to_string(),
    };
    row!("avg shell latency", asl, bsl, shell_lat_delta);

    let ac = match &a.compaction {
        None => "none".to_string(),
        Some(c) => format!("turn {}", c.turn),
    };
    let bc = match &b.compaction {
        None => "none".to_string(),
        Some(c) => format!("turn {}", c.turn),
    };
    row!("compaction", ac, bc, "—");

    row!(
        "exit status",
        a.exit_status.as_str(),
        b.exit_status.as_str(),
        "—"
    );
}

/// The Turns section's lines for every failed call, in trace order: the turn, the hook that made
/// it, its code and status, then its error, cut to [`FAILED_CALL_ERROR_CHARS`], on a line of its
/// own under the value column.
fn failed_call_lines(records: &[InferenceRecord]) -> Vec<String> {
    let mut lines = Vec::new();
    for rec in records {
        let Some(failure) = &rec.failure else {
            continue;
        };
        let origin = rec
            .origin
            .as_deref()
            .map(|origin| format!("{origin}  "))
            .unwrap_or_default();
        lines.push(format!(
            "failed:     turn {}  {origin}{}",
            rec.turn,
            failure.label()
        ));
        let mut error: String = failure
            .error
            .chars()
            .take(FAILED_CALL_ERROR_CHARS)
            .collect();
        if failure.error.chars().count() > FAILED_CALL_ERROR_CHARS {
            error.push('…');
        }
        if !error.is_empty() {
            lines.push(format!("            {error}"));
        }
    }
    lines
}

/// The agent loop's own turns that recorded wire hashes, in file order.
fn hashed_turns(m: &TraceMetrics) -> Vec<&InferenceRecord> {
    m.inference_records
        .iter()
        .filter(|rec| rec.is_agent_loop() && rec.has_hashes())
        .collect()
}

/// One run's answer for a piece that is fixed across the session, and the turn (if any) that
/// changed it mid-run.
fn prefix_piece<'a>(
    turns: &[&'a InferenceRecord],
    pick: fn(&'a InferenceRecord) -> Option<&'a String>,
) -> (Option<&'a String>, Option<(u32, &'a String)>) {
    let mut first: Option<&String> = None;
    let mut changed: Option<(u32, &String)> = None;
    for rec in turns {
        let Some(value) = pick(rec) else { continue };
        match first {
            None => first = Some(value),
            Some(seen) if seen != value && changed.is_none() => {
                changed = Some((rec.turn, value));
            }
            _ => {}
        }
    }
    (first, changed)
}

/// Render one fixed-across-the-session piece: what each run recorded, and whether they agree.
fn print_prefix_line(label: &str, a: Option<&String>, b: Option<&String>) {
    let body = match (a, b) {
        (Some(x), Some(y)) if x == y => format!("{:<10} {}", "identical", fmt_sha_short(x)),
        (Some(x), Some(y)) => format!(
            "{:<10} A {}  B {}",
            "differs",
            fmt_sha_short(x),
            fmt_sha_short(y)
        ),
        (Some(x), None) => format!("{:<10} A {}  B not recorded", "only in A", fmt_sha_short(x)),
        (None, Some(y)) => format!("{:<10} A not recorded  B {}", "only in B", fmt_sha_short(y)),
        (None, None) => format!("{:<10}", "not recorded"),
    };
    capsule_runtime::report_println!("{:<15}{}", label, body);
}

/// Where two runs' prompts stopped agreeing — the answer to "why did my cache miss".
///
/// Divergence has no polarity: neither run is better for having a longer or shorter agreeing
/// prefix, so no `(A better)`/`(B better)` marker appears anywhere in this section.
fn print_prefix_divergence(a: &TraceMetrics, b: &TraceMetrics) {
    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── Prefix divergence ────────────────────────────");

    let turns_a = hashed_turns(a);
    let turns_b = hashed_turns(b);
    // Nothing to compare: `trace.capture: none` writes no hashes at all, which is a different
    // record from a session that recorded hashes and stored no bodies.
    match (turns_a.is_empty(), turns_b.is_empty()) {
        (true, true) => {
            capsule_runtime::report_println!(
                "runs A and B recorded no content hashes — both ran under trace.capture: none"
            );
            return;
        }
        (true, false) => {
            capsule_runtime::report_println!(
                "run A recorded no content hashes — it ran under trace.capture: none"
            );
            return;
        }
        (false, true) => {
            capsule_runtime::report_println!(
                "run B recorded no content hashes — it ran under trace.capture: none"
            );
            return;
        }
        (false, false) => {}
    }

    let (sys_a, sys_changed_a) = prefix_piece(&turns_a, |rec| rec.system_sha.as_ref());
    let (sys_b, sys_changed_b) = prefix_piece(&turns_b, |rec| rec.system_sha.as_ref());
    print_prefix_line("system prompt:", sys_a, sys_b);
    let (tools_a, _) = prefix_piece(&turns_a, |rec| rec.tools_sha.as_ref());
    let (tools_b, _) = prefix_piece(&turns_b, |rec| rec.tools_sha.as_ref());
    print_prefix_line("tool schemas:", tools_a, tools_b);
    for (run, changed) in [("A", sys_changed_a), ("B", sys_changed_b)] {
        if let Some((turn, sha)) = changed {
            capsule_runtime::report_println!(
                "note:          run {} changes its system prompt at turn {} ({})",
                run,
                turn,
                fmt_sha_short(sha)
            );
        }
    }

    // Messages are paired by turn and compared element-wise: the first unequal position is
    // where the shared prefix — and any provider-side cache hit resting on it — ends.
    let turn_numbers: BTreeSet<u32> = turns_a
        .iter()
        .chain(turns_b.iter())
        .map(|rec| rec.turn)
        .collect();
    for turn in turn_numbers {
        let in_a = turns_a.iter().find(|rec| rec.turn == turn);
        let in_b = turns_b.iter().find(|rec| rec.turn == turn);
        let line = match (in_a, in_b) {
            (Some(ra), Some(rb)) => {
                let (ma, mb) = (&ra.message_shas, &rb.message_shas);
                match ma.iter().zip(mb.iter()).position(|(x, y)| x != y) {
                    Some(i) => format!(
                        "diverges at message {i}  A {}  B {}",
                        fmt_sha_short(&ma[i]),
                        fmt_sha_short(&mb[i])
                    ),
                    // Equal as far as both go: one array being a prefix of the other diverges
                    // at the shorter one's length, where a message exists in only one run.
                    None if ma.len() == mb.len() => {
                        format!(
                            "identical  ({} message{})",
                            ma.len(),
                            if ma.len() == 1 { "" } else { "s" }
                        )
                    }
                    None => format!(
                        "diverges at message {}  (A has {} messages, B has {})",
                        ma.len().min(mb.len()),
                        ma.len(),
                        mb.len()
                    ),
                }
            }
            (Some(_), None) => "only in run A".to_string(),
            (None, Some(_)) => "only in run B".to_string(),
            (None, None) => continue,
        };
        capsule_runtime::report_println!("turn {}:  {}", turn, line);
    }
}

pub(crate) fn run_trace_diff(
    before: Option<String>,
    after: Option<String>,
    workdir_arg: Option<PathBuf>,
) -> Result<(), CliError> {
    let (before, after) = match (before, after) {
        (None, None) => ("@2".to_string(), "@1".to_string()),
        (Some(b), Some(a)) => (b, a),
        _ => {
            return Err(CliError::new(
                E_TRC_002,
                "mur trace diff expects 0 or 2 arguments, got 1. Usage: mur trace diff [<before> <after>]",
            ));
        }
    };

    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });

    let result_a = resolve_diff_arg(&before, &workdir, "before");
    let result_b = resolve_diff_arg(&after, &workdir, "after");

    let (path_a, path_b) = match (result_a, result_b) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(ea), Err(eb)) => {
            return Err(CliError::new(
                E_TRC_002,
                format!("{}\n{}", ea.message, eb.message),
            ));
        }
        (Err(e), _) | (_, Err(e)) => return Err(e),
    };

    let (ma, _) = load_metrics(&path_a)?;
    let (mb, _) = load_metrics(&path_b)?;
    print_diff(&ma, &mb);
    print_prefix_divergence(&ma, &mb);
    Ok(())
}

// ── Report ────────────────────────────────────────────────────────────────────

struct RunStats {
    turns: f64,
    duration_ms: f64,
    input_tokens: f64,
    output_tokens: f64,
    tool_calls: f64,
    tool_success_rate: Option<f64>,
    shell_calls: f64,
    redundant_calls: f64,
    exit_status: String,
}

fn stat_mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

fn stat_stddev(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let m = stat_mean(values);
    let variance = values.iter().map(|v| (v - m).powi(2)).sum::<f64>() / values.len() as f64;
    variance.sqrt()
}

fn stat_min(values: &[f64]) -> f64 {
    values.iter().cloned().fold(f64::INFINITY, f64::min)
}

fn stat_max(values: &[f64]) -> f64 {
    values.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
}

fn print_stat_row(label: &str, values: &[f64], format_fn: &dyn Fn(f64) -> String) {
    const COL: usize = 22;
    const VAL: usize = 14;
    capsule_runtime::report_println!(
        "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} {}",
        label,
        format_fn(stat_mean(values)),
        format_fn(stat_stddev(values)),
        format_fn(stat_min(values)),
        format_fn(stat_max(values))
    );
}

fn print_report(workdir: &Path, stats: &[RunStats]) {
    capsule_runtime::report_println!("Sessions: {}  ({})", stats.len(), workdir.display());
    capsule_runtime::report_println!();

    const COL: usize = 22;
    const VAL: usize = 14;
    capsule_runtime::report_println!(
        "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} Max",
        "Metric",
        "Mean",
        "StdDev",
        "Min"
    );
    capsule_runtime::report_println!(
        "{} {} {} {} {}",
        "─".repeat(COL),
        "─".repeat(VAL),
        "─".repeat(VAL),
        "─".repeat(VAL),
        "─".repeat(VAL)
    );

    let turns: Vec<f64> = stats.iter().map(|s| s.turns).collect();
    let durations: Vec<f64> = stats.iter().map(|s| s.duration_ms).collect();
    let inputs: Vec<f64> = stats.iter().map(|s| s.input_tokens).collect();
    let outputs: Vec<f64> = stats.iter().map(|s| s.output_tokens).collect();
    let tools: Vec<f64> = stats.iter().map(|s| s.tool_calls).collect();
    let shells: Vec<f64> = stats.iter().map(|s| s.shell_calls).collect();
    let redundants: Vec<f64> = stats.iter().map(|s| s.redundant_calls).collect();
    let success_rates: Vec<f64> = stats.iter().filter_map(|s| s.tool_success_rate).collect();

    print_stat_row("turns", &turns, &|v| format!("{:.1}", v));
    print_stat_row("duration (ms)", &durations, &|v| fmt_thousands(v as u64));
    print_stat_row("input tokens", &inputs, &|v| fmt_thousands(v as u64));
    print_stat_row("output tokens", &outputs, &|v| fmt_thousands(v as u64));
    print_stat_row("tool calls", &tools, &|v| format!("{:.1}", v));
    if !success_rates.is_empty() {
        print_stat_row("tool success (%)", &success_rates, &|v| format!("{:.1}", v));
    }
    print_stat_row("shell calls", &shells, &|v| format!("{:.1}", v));
    print_stat_row("redundant calls", &redundants, &|v| format!("{:.1}", v));

    capsule_runtime::report_println!();
    capsule_runtime::report_println!("Exit status:");
    let mut exit_dist: HashMap<String, usize> = HashMap::new();
    for s in stats {
        *exit_dist.entry(s.exit_status.clone()).or_insert(0) += 1;
    }
    let total = stats.len();
    let mut sorted: Vec<(&String, &usize)> = exit_dist.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (status, count) in &sorted {
        capsule_runtime::report_println!(
            "  {:<24} {}  ({:.1}%)",
            status,
            count,
            100.0 * **count as f64 / total as f64
        );
    }
}

pub(crate) fn run_trace_report(
    sessions: Vec<String>,
    last: Option<usize>,
    since: Option<String>,
    workdir_arg: Option<PathBuf>,
) -> Result<(), CliError> {
    if !sessions.is_empty() && (last.is_some() || since.is_some()) {
        let flag = if since.is_some() { "--since" } else { "--last" };
        return Err(CliError::new(
            E_TRC_002,
            format!("{flag} cannot be combined with explicit session arguments"),
        ));
    }
    if let Some(0) = last {
        return Err(CliError::new(E_TRC_002, "--last must be at least 1"));
    }

    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });

    let trace_paths: Vec<PathBuf> = if sessions.is_empty() {
        if !workdir.exists() || !workdir.is_dir() {
            return Err(CliError::new(
                E_IO_001,
                format!("workdir not found: {}", workdir.display()),
            ));
        }
        let mut entries = ses_entries(&workdir)?;
        if entries.is_empty() {
            return Err(CliError::new(
                E_TRC_002,
                format!("no sessions found in workdir at {}", workdir.display()),
            ));
        }
        entries.sort();

        if let Some(since_str) = &since {
            let duration_ms = parse_since(since_str)?;
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let cutoff_ms = now_ms.saturating_sub(duration_ms);
            entries.retain(|e| {
                capsule_runtime::retention::session_id_timestamp_ms(e)
                    .map(|ts| ts >= cutoff_ms)
                    .unwrap_or(false)
            });
            if entries.is_empty() {
                return Err(CliError::new(
                    E_TRC_002,
                    format!(
                        "no sessions matched --since {} in workdir at {}",
                        since_str,
                        workdir.display()
                    ),
                ));
            }
        }

        if let Some(n) = last {
            let skip = entries.len().saturating_sub(n);
            entries = entries.into_iter().skip(skip).collect();
        }

        entries
            .into_iter()
            .map(|e| workdir.join(e).join("trace.jsonl"))
            .collect()
    } else {
        sessions
            .into_iter()
            .map(|s| resolve_session(Some(s), &workdir))
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut session_metrics: Vec<TraceMetrics> = Vec::new();
    let mut all_task_metrics: Vec<TaskMetrics> = Vec::new();
    let mut skipped = 0usize;
    for path in &trace_paths {
        match load_metrics(path) {
            Ok((m, tasks)) => {
                session_metrics.push(m);
                if tasks.len() > 1 {
                    all_task_metrics.extend(tasks);
                }
            }
            // Empty or mid-run sessions are common in a live workdir; skip them.
            Err(e) if e.code == E_TRC_001 => skipped += 1,
            Err(e) => return Err(e),
        }
    }

    if session_metrics.is_empty() {
        return Err(CliError::new(
            E_TRC_001,
            "no complete sessions to report — all traces are empty or incomplete",
        ));
    }
    if skipped > 0 {
        capsule_runtime::report_eprintln!("note: skipped {} incomplete session(s)", skipped);
    }

    for m in &session_metrics {
        print_session_block(m);
    }

    let run_stats: Vec<RunStats> = session_metrics
        .iter()
        .map(|m| RunStats {
            turns: m.total_turns as f64,
            duration_ms: m.duration_ms as f64,
            input_tokens: m.total_input_tokens as f64,
            output_tokens: m.total_output_tokens as f64,
            tool_calls: m.total_tool_calls as f64,
            tool_success_rate: m.tool_success_rate(),
            shell_calls: m.total_shell_calls as f64,
            redundant_calls: m.redundant_calls.len() as f64,
            exit_status: m.exit_status.clone(),
        })
        .collect();

    print_report(&workdir, &run_stats);

    if !all_task_metrics.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!(
            "── Per-task averages (multi-task sessions only) ──────────────"
        );
        const COL: usize = 22;
        const VAL: usize = 14;
        capsule_runtime::report_println!(
            "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} Max",
            "Metric",
            "Mean",
            "StdDev",
            "Min"
        );
        capsule_runtime::report_println!(
            "{} {} {} {} {}",
            "─".repeat(COL),
            "─".repeat(VAL),
            "─".repeat(VAL),
            "─".repeat(VAL),
            "─".repeat(VAL)
        );
        let task_turns: Vec<f64> = all_task_metrics.iter().map(|t| t.turns as f64).collect();
        let task_inputs: Vec<f64> = all_task_metrics
            .iter()
            .map(|t| t.input_tokens as f64)
            .collect();
        let task_outputs: Vec<f64> = all_task_metrics
            .iter()
            .map(|t| t.output_tokens as f64)
            .collect();
        let task_durations: Vec<f64> = all_task_metrics
            .iter()
            .map(|t| t.duration_ms as f64)
            .collect();
        print_stat_row("task turns", &task_turns, &|v| format!("{:.1}", v));
        print_stat_row("task input tokens", &task_inputs, &|v| {
            fmt_thousands(v as u64)
        });
        print_stat_row("task output tokens", &task_outputs, &|v| {
            fmt_thousands(v as u64)
        });
        print_stat_row("task duration (ms)", &task_durations, &|v| {
            fmt_thousands(v as u64)
        });
        capsule_runtime::report_println!("Tasks: {}", all_task_metrics.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `trace.jsonl`'s `shell` record gained a `binary` key (the invoked program's
    /// resolved path) alongside the existing `command`. [`ShellEvent`] declares neither
    /// and carries no `deny_unknown_fields`, so `mur trace show` keeps parsing the new
    /// records unchanged — this pins that tolerance so a later `deny_unknown_fields`
    /// cannot silently break reading every trace the runtime now writes.
    #[test]
    fn shell_record_with_binary_key_still_parses() {
        let line = r#"{"event_type":"shell","session_id":"s","timestamp":1,"turn":2,"binary":"/usr/bin/pytest","command":"-q tests/","exit_code":0,"stdout_bytes":7,"stderr_bytes":0,"duration_ms":12}"#;

        let parsed = serde_json::from_str::<TraceEvent>(line).expect("unknown keys are ignored");
        match parsed {
            TraceEvent::Shell(e) => {
                assert_eq!(e.exit_code, 0);
                assert_eq!(e.duration_ms, 12);
            }
            other => panic!("expected a shell event, got {other:?}"),
        }
    }

    fn task_row(line: &str) -> String {
        let record = TraceRecord {
            identity: serde_json::from_str::<EventIdentity>(line).unwrap_or_default(),
            event: serde_json::from_str::<TraceEvent>(line).expect("task_start should parse"),
        };
        steps_row(&record, false).expect("a task_start renders a row")
    }

    #[test]
    fn task_row_names_the_origin_and_trust_class() {
        let line = r#"{"event_type":"task_start","event_id":"evt_1","session_id":"s","timestamp":1,"task_id":"tsk_0a1b2c3d4e5f","context_id":"ctx_3c4d5e6f7a8b","source":"a2a","origin":"peer","trust":"untrusted","message_parts_bytes":9}"#;
        assert_eq!(
            task_row(line),
            "task tsk_0a1b2c3d…  ctx_3c4d5e6f…  (a2a, peer/untrusted)"
        );
    }

    #[test]
    fn task_row_names_the_lane_the_task_waited_in() {
        let line = r#"{"event_type":"task_start","event_id":"evt_1","session_id":"s","timestamp":1,"task_id":"tsk_0a1b2c3d4e5f","context_id":"ctx_3c4d5e6f7a8b","source":"a2a","origin":"peer","trust":"trusted","lane":"peer","message_parts_bytes":9}"#;
        assert_eq!(
            task_row(line),
            "task tsk_0a1b2c3d…  ctx_3c4d5e6f…  (a2a, peer/trusted, lane peer)"
        );
    }

    /// A trace written before `origin` and `trust` existed renders its task row without them,
    /// rather than inventing a class it has no record of.
    #[test]
    fn task_row_omits_provenance_a_trace_predates() {
        let line = r#"{"event_type":"task_start","event_id":"evt_1","session_id":"s","timestamp":1,"task_id":"tsk_0a1b2c3d4e5f","context_id":"ctx_3c4d5e6f7a8b","source":"task_md","message_parts_bytes":9}"#;
        assert_eq!(
            task_row(line),
            "task tsk_0a1b2c3d…  ctx_3c4d5e6f…  (task_md)"
        );
        let session_dir = tempfile::tempdir().unwrap();
        std::fs::write(session_dir.path().join("trace.jsonl"), format!("{line}\n")).unwrap();
        assert_eq!(
            first_task_context_id(session_dir.path()).unwrap(),
            Some("ctx_3c4d5e6f7a8b".to_string()),
            "an older trace must still resolve its context id for `mur run --resume`"
        );
    }

    /// One row for any record `steps_row` renders, so the detached-shell rows below are pinned
    /// exactly as the task rows above are.
    fn row(line: &str) -> String {
        let record = TraceRecord {
            identity: serde_json::from_str::<EventIdentity>(line).unwrap_or_default(),
            event: serde_json::from_str::<TraceEvent>(line).expect("the record should parse"),
        };
        steps_row(&record, false).expect("the record renders a row")
    }

    const FAILED_REJECTION: &str = r#"{"event_type":"inference","event_id":"evt_2","session_id":"s","timestamp":1,"turn":0,"task_id":"tsk_1","decision":"error","stop_reason":"error","tool_name":null,"error_code":"credential_rejected","error":"the provider rejected the inference credential credentials.KEY in /h/config.yaml (HTTP 401)","provider_status":401}"#;
    const FAILED_TRAP: &str = r#"{"event_type":"inference","event_id":"evt_3","session_id":"s","timestamp":1,"turn":1,"task_id":"tsk_1","decision":"error","stop_reason":"error","tool_name":null,"error_code":"driver_failed","error":"driver invocation failed: wasm trap: unreachable"}"#;
    const FAILED_HOOK: &str = r#"{"event_type":"inference","event_id":"evt_4","session_id":"s","timestamp":1,"turn":2,"task_id":"tsk_1","input_tokens":40,"decision":"error","stop_reason":"error","tool_name":null,"origin":"hook:compactor","model":"m","error_code":"driver_error","error":"inference driver returned an error: HTTP 500: boom","provider_status":500}"#;
    const ANSWERED_TURN: &str = r#"{"event_type":"inference","event_id":"evt_5","session_id":"s","timestamp":1,"turn":0,"task_id":"tsk_1","input_tokens":10,"output_tokens":5,"decision":"end_turn","stop_reason":"end_turn","tool_name":null}"#;

    fn inference_event(line: &str) -> InferenceEvent {
        match serde_json::from_str::<TraceEvent>(line).unwrap() {
            TraceEvent::Inference(e) => e,
            other => panic!("expected an inference event, got {other:?}"),
        }
    }

    #[test]
    fn the_turns_section_lists_each_failed_call_in_trace_order() {
        let records: Vec<InferenceRecord> =
            [ANSWERED_TURN, FAILED_REJECTION, FAILED_TRAP, FAILED_HOOK]
                .into_iter()
                .map(|line| InferenceRecord::from_event(inference_event(line)))
                .collect();
        assert_eq!(
            failed_call_lines(&records),
            [
                "failed:     turn 0  credential_rejected  HTTP 401",
                "            the provider rejected the inference credential credentials.KEY in /h/config.yaml (HTTP 401)",
                "failed:     turn 1  driver_failed",
                "            driver invocation failed: wasm trap: unreachable",
                "failed:     turn 2  hook:compactor  driver_error  HTTP 500",
                "            inference driver returned an error: HTTP 500: boom",
            ]
        );
    }

    #[test]
    fn a_long_failed_call_error_is_cut_to_two_hundred_characters() {
        let line = FAILED_TRAP.replace(
            "driver invocation failed: wasm trap: unreachable",
            &"é".repeat(250),
        );
        let lines = failed_call_lines(&[InferenceRecord::from_event(inference_event(&line))]);
        assert_eq!(lines[1], format!("            {}…", "é".repeat(200)));
    }

    #[test]
    fn a_failed_call_renders_its_code_and_status_on_its_steps_row() {
        assert_eq!(
            row(FAILED_REJECTION),
            "turn 0  error  credential_rejected  HTTP 401"
        );
        assert_eq!(row(FAILED_TRAP), "turn 1  error  driver_failed");
        assert!(
            row(FAILED_HOOK).ends_with("hook:compactor  error  driver_error  HTTP 500"),
            "{}",
            row(FAILED_HOOK)
        );
        assert_eq!(row(ANSWERED_TURN), "turn 0  end_turn");
    }

    #[test]
    fn a_failed_agent_loop_call_is_neither_a_steps_turn_nor_a_wire_turn() {
        assert!(!inference_event(FAILED_REJECTION).is_steps_turn());
        assert!(!inference_event(FAILED_HOOK).is_steps_turn());
        assert!(inference_event(ANSWERED_TURN).is_steps_turn());

        let records = [FAILED_REJECTION, FAILED_TRAP]
            .into_iter()
            .map(|line| TraceRecord {
                identity: EventIdentity::default(),
                event: serde_json::from_str::<TraceEvent>(line).unwrap(),
            })
            .collect();
        let index = WireIndex::build(records);
        assert!(
            index.turns_numbered(0).is_empty(),
            "--turn 0 does not offer a failed call"
        );
        assert!(index.turns.is_empty());
    }

    #[test]
    fn shell_detached_row_names_the_work_id_and_the_grace_it_outran() {
        let line = r#"{"event_type":"shell_detached","event_id":"evt_1","session_id":"s","timestamp":1,"turn":2,"task_id":"tsk_1","work_id":"wrk_0a1b2c3d4e5f6a7b","binary":"/usr/bin/bash","command":"make -j8","grace_ms":10000}"#;
        assert_eq!(
            row(line),
            "shell_detached /usr/bin/bash  wrk_0a1b2c3d…  detached after 10.0s"
        );
    }

    #[test]
    fn shell_completed_row_names_the_work_id_the_exit_code_and_the_output_path() {
        let line = r#"{"event_type":"shell_completed","event_id":"evt_2","session_id":"s","timestamp":2,"work_id":"wrk_0a1b2c3d4e5f6a7b","binary":"/usr/bin/bash","command":"make -j8","exit_code":0,"duration_ms":42000,"output_path":"logs/wrk_0a1b2c3d4e5f6a7b.log","output_bytes":900,"status":"ok","completion_task_id":"tsk_2"}"#;
        assert_eq!(
            row(line),
            "shell_completed /usr/bin/bash  wrk_0a1b2c3d…  exit 0 ✓  42.0s  logs/wrk_0a1b2c3d4e5f6a7b.log"
        );
    }

    #[test]
    fn shell_abandoned_row_says_the_result_is_lost() {
        let line = r#"{"event_type":"shell_abandoned","event_id":"evt_3","session_id":"s","timestamp":3,"work_id":"wrk_0a1b2c3d4e5f6a7b","binary":"/usr/bin/bash","command":"make -j8","running_ms":30000}"#;
        assert_eq!(
            row(line),
            "shell_abandoned /usr/bin/bash  wrk_0a1b2c3d…  still running after 30.0s  result lost"
        );
    }

    /// A lost command's row shares no shape with a completion's: no exit code, no status mark,
    /// no duration and no output path, because a lost command produced none of them.
    #[test]
    fn shell_lost_row_says_no_result_exists_and_names_who_reported_it() {
        let line = r#"{"event_type":"shell_lost","event_id":"evt_5","session_id":"ses_0a1b2c3d4e5f6a7b","timestamp":5,"work_id":"wrk_0a1b2c3d4e5f6a7b","binary":"/usr/bin/bash","command":"make -j8","detached_at_ms":1750,"reconciled_by_session":"ses_9f8e7d6c5b4a3210","reconciled_task_id":"tsk_3"}"#;
        let rendered = row(line);
        assert_eq!(
            rendered,
            "shell_lost /usr/bin/bash  wrk_0a1b2c3d…  detached at 1750  no result, reported by ses_9f8e7d6c…"
        );
        assert!(
            !rendered.contains("exit"),
            "a lost command asserts no exit code: {rendered}"
        );
    }

    /// A spend refusal names the limit, what was used of the ceiling and what the refused call
    /// needed, and a hook's refusal names the hook.
    #[test]
    fn spend_ceiling_reached_row_names_the_limit_and_the_numbers() {
        let line = r#"{"event_type":"spend_ceiling_reached","event_id":"evt_6","parent_id":"evt_1","session_id":"ses_0a1b2c3d4e5f6a7b","timestamp":6,"turn":3,"task_id":null,"limit":"session","ceiling":200000,"used":195400,"requested":12100}"#;
        assert_eq!(
            row(line),
            "spend_ceiling_reached session  used 195,400 of 200,000  needs 12,100"
        );
        let hook = line.replace(
            r#""requested":12100"#,
            r#""requested":12100,"origin":"hook:compact""#,
        );
        assert!(
            row(&hook).ends_with("needs 12,100  hook:compact"),
            "{}",
            row(&hook)
        );
    }

    /// A refused task's `steps` row names the cause and the task, and its `show` row names the
    /// task in full, the cause and the source. A record with no `reason` still parses.
    #[test]
    fn task_rejected_renders_a_steps_row_and_a_show_row() {
        let line = r#"{"event_type":"task_rejected","event_id":"evt_7","parent_id":"evt_1","session_id":"s","timestamp":7,"task_id":"tsk_0a1b2c3d4e5f6a7b","context_id":"ctx_1","source":"a2a","cause":"session_ended","reason":"task rejected: the session ended before this task started"}"#;
        assert_eq!(row(line), "task_rejected session_ended  tsk_0a1b2c3d…");

        let TraceEvent::TaskRejected(e) = serde_json::from_str::<TraceEvent>(line).unwrap() else {
            panic!("a task_rejected line parses as TaskRejected");
        };
        assert_eq!(
            rejected_show_row(&e),
            "task_rejected  tsk_0a1b2c3d4e5f6a7b  session_ended  source a2a"
        );

        let bare = r#"{"event_type":"task_rejected","task_id":"tsk_1","cause":"session_stopped"}"#;
        let TraceEvent::TaskRejected(e) = serde_json::from_str::<TraceEvent>(bare).unwrap() else {
            panic!("a task_rejected line without source or reason still parses");
        };
        assert_eq!(
            rejected_show_row(&e),
            "task_rejected  tsk_1  session_stopped  source unknown"
        );
    }

    /// A `task_canceled` row names a detached shell command as still running and a delegation as
    /// in flight, and says so when the record names nothing.
    #[test]
    fn task_canceled_names_detached_work_running_and_delegations_in_flight() {
        let line = r#"{"event_type":"task_canceled","task_id":"tsk_1","phase":"delegation","detached_work_ids":["wrk_1"],"delegation_ids":["dlg_1","dlg_2"]}"#;
        assert_eq!(
            row(line),
            "task_canceled delegation  still running: wrk_1; delegations in flight: dlg_1, dlg_2"
        );
        let bare = r#"{"event_type":"task_canceled","task_id":"tsk_1","phase":"turn","detached_work_ids":[],"delegation_ids":[]}"#;
        assert_eq!(row(bare), "task_canceled turn  nothing left running");
    }

    /// A `formation_ended` record's `steps` row and `show` row both name the formation.
    #[test]
    fn formation_ended_renders_a_steps_row_and_a_show_row() {
        let line = r#"{"event_type":"formation_ended","event_id":"evt_8","parent_id":"evt_1","session_id":"s","timestamp":8,"formation_id":"frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b"}"#;
        assert_eq!(
            row(line),
            "formation_ended frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b"
        );
        let TraceEvent::FormationEnded(e) = serde_json::from_str::<TraceEvent>(line).unwrap()
        else {
            panic!("a formation_ended line parses as FormationEnded");
        };
        assert_eq!(
            formation_ended_show_row(&e.formation_id),
            "formation_ended  frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b  the formation ended; this \
             session wound down"
        );
    }

    /// A `spawner_ended` record's `steps` row names the spawning session, and its `show` row names
    /// the lineage, with `-` for a key the record omits.
    #[test]
    fn spawner_ended_renders_a_steps_row_and_a_show_row() {
        let line = r#"{"event_type":"spawner_ended","event_id":"evt_8","parent_id":"evt_1","session_id":"s","timestamp":8,"spawned_by":"ses_parent","delegation_id":"dlg_abc"}"#;
        assert_eq!(row(line), "spawner_ended ses_parent");
        let TraceEvent::SpawnerEnded(e) = serde_json::from_str::<TraceEvent>(line).unwrap() else {
            panic!("a spawner_ended line parses as SpawnerEnded");
        };
        assert_eq!(
            spawner_ended_show_row(&e),
            "spawner_ended  ses_parent  dlg_abc  the session that delegated to this one ended; \
             this session wound down"
        );

        let bare = r#"{"event_type":"spawner_ended","event_id":"evt_8","parent_id":"evt_1","session_id":"s","timestamp":8}"#;
        assert_eq!(row(bare), "spawner_ended -");
        let TraceEvent::SpawnerEnded(e) = serde_json::from_str::<TraceEvent>(bare).unwrap() else {
            panic!("a bare spawner_ended line parses as SpawnerEnded");
        };
        assert_eq!(
            spawner_ended_show_row(&e),
            "spawner_ended  -  -  the session that delegated to this one ended; this session \
             wound down"
        );
    }

    /// A `member_call_start` and its `member_call` render a `steps` row each, and join into one
    /// `show` row naming the call, the member, the callee's task and the status with its
    /// duration. A call that never started shows no task.
    #[test]
    fn member_call_records_render_steps_rows_and_one_show_row_per_call() {
        let start = r#"{"event_type":"member_call_start","event_id":"evt_4","parent_id":"evt_1","session_id":"s","timestamp":4,"task_id":"tsk_lead","call_id":"mcl_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b","member":"worker","member_task_id":"tsk_worker"}"#;
        let end = r#"{"event_type":"member_call","event_id":"evt_9","parent_id":"evt_1","session_id":"s","timestamp":9,"task_id":"tsk_lead","call_id":"mcl_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b","member":"worker","member_task_id":"tsk_worker","status":"completed","duration_ms":1500,"output":"WORKER-1","truncated":false,"delivered":true}"#;
        let refused = r#"{"event_type":"member_call","event_id":"evt_10","parent_id":"evt_1","session_id":"s","timestamp":10,"task_id":"tsk_lead","call_id":"mcl_2","member":"worker","status":"failed","duration_ms":3,"output":"no","truncated":false,"delivered":true}"#;
        assert_eq!(
            row(start),
            "member_call_start worker  mcl_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b  task tsk_worker"
        );
        assert_eq!(
            row(end),
            "member_call worker  mcl_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b  completed  1.5s"
        );

        let mut calls = Vec::new();
        for line in [start, end, refused] {
            match serde_json::from_str::<TraceEvent>(line).unwrap() {
                TraceEvent::MemberCallStart(e) => fold_member_call_start(&mut calls, e),
                TraceEvent::MemberCall(e) => fold_member_call(&mut calls, e),
                _ => unreachable!("a member call line"),
            }
        }
        let rows: Vec<String> = calls.iter().map(member_call_show_row).collect();
        assert_eq!(
            rows,
            [
                "mcl_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b  worker  tsk_worker  completed in 1.5s",
                "mcl_2  worker  (not started)  failed in 3ms",
            ]
        );
    }

    /// A `member_call_busy` renders a `steps` row naming the member, the call and the offer, and
    /// folds into its call's `show` row as a busy count: a call later taken shows the callee's
    /// task, and one turned away busy and nothing since is outstanding and not started.
    #[test]
    fn member_call_busy_renders_a_steps_row_and_counts_on_its_call_s_show_row() {
        let busy = |offer: u32, call: &str, at: u64| {
            format!(
                r#"{{"event_type":"member_call_busy","event_id":"evt_{at}","parent_id":"evt_1","session_id":"s","timestamp":{at},"task_id":"tsk_lead","call_id":"{call}","member":"reviewer","offer":{offer},"waited_ms":{at},"message":"task rejected: capsule is busy"}}"#
            )
        };
        let start = r#"{"event_type":"member_call_start","event_id":"evt_40","parent_id":"evt_1","session_id":"s","timestamp":4000,"task_id":"tsk_lead","call_id":"mcl_1","member":"reviewer","member_task_id":"tsk_reviewer"}"#;
        let end = r#"{"event_type":"member_call","event_id":"evt_90","parent_id":"evt_1","session_id":"s","timestamp":9000,"task_id":"tsk_lead","call_id":"mcl_1","member":"reviewer","member_task_id":"tsk_reviewer","status":"completed","duration_ms":9000,"output":"ok","truncated":false,"delivered":true}"#;
        assert_eq!(
            row(&busy(2, "mcl_1", 1100)),
            "member_call_busy reviewer  mcl_1  offer 2 turned away busy"
        );

        let mut calls = Vec::new();
        for line in [
            busy(1, "mcl_1", 1),
            busy(1, "mcl_2", 2),
            busy(2, "mcl_1", 1100),
            busy(3, "mcl_1", 3200),
            start.to_string(),
            end.to_string(),
        ] {
            match serde_json::from_str::<TraceEvent>(&line).unwrap() {
                TraceEvent::MemberCallBusy(e) => fold_member_call_busy(&mut calls, e),
                TraceEvent::MemberCallStart(e) => fold_member_call_start(&mut calls, e),
                TraceEvent::MemberCall(e) => fold_member_call(&mut calls, e),
                _ => unreachable!("a member call line"),
            }
        }
        let rows: Vec<String> = calls.iter().map(member_call_show_row).collect();
        assert_eq!(
            rows,
            [
                "mcl_1  reviewer  tsk_reviewer  completed in 9.0s  busy ×3",
                "mcl_2  reviewer  (not started)  outstanding  busy ×1",
            ]
        );
    }

    /// A `tools_refreshed` line parses into its own variant rather than falling through to
    /// `Unknown`, and its row names the trigger, what entered and left the array, and the turn.
    #[test]
    fn tools_refreshed_renders_a_steps_row() {
        let line = r#"{"event_type":"tools_refreshed","event_id":"evt_5","parent_id":"evt_1","session_id":"s","timestamp":5,"turn":3,"task_id":"tsk_1","trigger":"compaction","added":["aaa-late-skill"],"removed":["old-tool"],"tools":["aaa-late-skill","zzz-existing-tool"]}"#;
        let TraceEvent::ToolsRefreshed(e) = serde_json::from_str::<TraceEvent>(line).unwrap()
        else {
            panic!("a tools_refreshed line parses as ToolsRefreshed");
        };
        assert_eq!(e.turn, 3);
        assert_eq!(e.trigger, "compaction");
        assert_eq!(
            row(line),
            "tools_refreshed compaction  +aaa-late-skill  -old-tool  turn 3"
        );
    }

    /// A `tool_input_refused` line parses into its own variant, and its row names the tool and
    /// every field the call lacked.
    #[test]
    fn tool_input_refused_renders_a_steps_row() {
        let line = r#"{"event_type":"tool_input_refused","event_id":"evt_6","parent_id":"evt_2","session_id":"s","timestamp":6,"turn":1,"tool_name":"murmur-tool-editor","tool_call_id":null,"missing":["operation","dest_path"],"reason":"murmur-tool-editor: missing required fields \"operation\", \"dest_path\""}"#;
        let TraceEvent::ToolInputRefused(e) = serde_json::from_str::<TraceEvent>(line).unwrap()
        else {
            panic!("a tool_input_refused line parses as ToolInputRefused");
        };
        assert_eq!(e.missing, ["operation", "dest_path"]);
        assert_eq!(
            row(line),
            "tool_input_refused murmur-tool-editor  missing operation, dest_path"
        );
    }

    /// A trace holding none of these records renders exactly as it did without them: the reader
    /// skips what it does not know rather than failing the parse.
    #[test]
    fn an_unknown_event_type_still_renders_nothing() {
        let line = r#"{"event_type":"not_an_event_this_reader_knows","event_id":"evt_4","session_id":"s","timestamp":4}"#;
        let record = TraceRecord {
            identity: serde_json::from_str::<EventIdentity>(line).unwrap_or_default(),
            event: serde_json::from_str::<TraceEvent>(line).expect("an unknown type still parses"),
        };
        assert!(steps_row(&record, false).is_none());
    }

    fn skill_record(line: &str) -> SkillCallRecord {
        let TraceEvent::SkillCall(e) = serde_json::from_str::<TraceEvent>(line).unwrap() else {
            panic!("a skill_call line parses as SkillCall");
        };
        SkillCallRecord {
            turn: e.turn,
            untrusted: e.untrusted_marker(),
            skill_name: e.skill_name,
            status: e.status,
            duration_ms: e.duration_ms,
        }
    }

    /// A runtime-origin skill call is annotated on both surfaces; an operator one, and one from a
    /// trace predating the keys, renders exactly as a skill call always did.
    #[test]
    fn runtime_origin_skill_call_rows_are_annotated_only_when_untrusted() {
        let runtime = r#"{"event_type":"skill_call","event_id":"evt_3","parent_id":"evt_2","session_id":"s","timestamp":3,"turn":1,"task_id":"tsk_1","skill_name":"pulled-style","output_bytes":40,"duration_ms":2,"status":"ok","origin":"runtime","trust":"untrusted"}"#;
        let operator = r#"{"event_type":"skill_call","event_id":"evt_4","parent_id":"evt_2","session_id":"s","timestamp":4,"turn":1,"task_id":"tsk_1","skill_name":"house-style","output_bytes":40,"duration_ms":2,"status":"ok","origin":"operator","trust":"trusted"}"#;
        let older = r#"{"event_type":"skill_call","session_id":"s","timestamp":4,"turn":1,"skill_name":"house-style","output_bytes":40,"duration_ms":2,"status":"ok"}"#;

        assert_eq!(
            row(runtime),
            "skill_call pulled-style  2ms  ✓ (runtime/untrusted)"
        );
        assert_eq!(row(operator), "skill_call house-style  2ms  ✓");
        assert_eq!(row(older), row(operator));

        assert_eq!(
            skill_call_show_entry(&skill_record(runtime)),
            "pulled-style 2ms ✓ runtime/untrusted"
        );
        assert_eq!(
            skill_call_show_entry(&skill_record(operator)),
            "house-style 2ms ✓"
        );
        assert_eq!(
            skill_call_show_entry(&skill_record(older)),
            "house-style 2ms ✓"
        );
    }

    #[test]
    fn runtime_origin_artifact_pulled_renders_a_steps_row_and_a_show_row() {
        let line = r#"{"event_type":"artifact_pulled","event_id":"evt_9","parent_id":null,"session_id":"ses_1","timestamp":9,"name":"pulled-style","version":"0.1.0","runtime":"skill","origin":"runtime","session":"ses_1","trust":"untrusted"}"#;
        assert_eq!(
            row(line),
            "artifact_pulled pulled-style@0.1.0 (runtime/untrusted)"
        );
        let TraceEvent::ArtifactPulled(e) = serde_json::from_str::<TraceEvent>(line).unwrap()
        else {
            panic!("an artifact_pulled line parses as ArtifactPulled");
        };
        assert_eq!(
            artifact_pulled_show_row(&e),
            "  pulled-style@0.1.0  skill  runtime/untrusted"
        );
    }

    /// The Session block names each runtime pin and its puller, and says nothing for a session
    /// with none — including one whose `session_start` predates the key.
    #[test]
    fn runtime_origin_session_block_names_each_runtime_pin() {
        let with = r#"{"event_type":"session_start","session_id":"s","capsule_name":"c","capsule_version":"0.1.0","model":"m","max_turns":5,"runtime_artifacts":[{"name":"pulled-style","version":"0.1.0","origin":"runtime","session":"ses_puller","trust":"untrusted"},{"name":"fetcher","version":"1.0.0","origin":"runtime","session":"ses_other","trust":"untrusted"}]}"#;
        let empty = r#"{"event_type":"session_start","session_id":"s","capsule_name":"c","capsule_version":"0.1.0","model":"m","max_turns":5,"runtime_artifacts":[]}"#;
        let older = r#"{"event_type":"session_start","session_id":"s","capsule_name":"c","capsule_version":"0.1.0","model":"m","max_turns":5}"#;
        let pins = |line: &str| {
            let TraceEvent::SessionStart(e) = serde_json::from_str::<TraceEvent>(line).unwrap()
            else {
                panic!("a session_start line parses as SessionStart");
            };
            runtime_pins_line(&e.runtime_artifacts)
        };

        assert_eq!(
            pins(with).as_deref(),
            Some(
                "runtime pins: pulled-style@0.1.0 (pulled by ses_puller), fetcher@1.0.0 (pulled by \
                 ses_other)"
            )
        );
        assert_eq!(pins(empty), None);
        assert_eq!(pins(older), None);
    }

    fn formation_member(
        session_id: &str,
        member: Option<&str>,
        capsule: &str,
        callees: &[&str],
    ) -> RecordedMember {
        RecordedMember {
            session_id: session_id.to_string(),
            capsule: capsule.to_string(),
            member: member.map(str::to_string),
            callees: callees.iter().map(|c| c.to_string()).collect(),
            spawned_by: None,
            exit_status: Some("completed".to_string()),
            delegated_children: Vec::new(),
            calls_made: Vec::new(),
            tasks_received: Vec::new(),
        }
    }

    /// A member row names the session, its roster name (`-` for none), its capsule and its
    /// ending in aligned columns, then who delegated it and whom it may call.
    #[test]
    fn formation_member_rows_name_each_member_and_whom_it_may_call() {
        let lead = formation_member("ses_a", Some("lead"), "team@0.1.0", &["w1", "w2"]);
        let w1 = formation_member("ses_b", Some("w1"), "worker@0.1.0", &[]);
        let mut child = formation_member("ses_c", None, "helper@1.2.30", &[]);
        child.spawned_by = Some("ses_b".to_string());
        child.exit_status = None;
        assert_eq!(
            formation_member_lines(&[lead, w1, child]),
            [
                "ses_a                                 lead  team@0.1.0     completed  may call w1, w2",
                "ses_b                                 w1    worker@0.1.0   completed",
                "ses_c                                 -     helper@1.2.30  no session_end  spawned by ses_b",
            ]
        );
    }

    fn formation_call(
        depth: usize,
        call_id: Option<&str>,
        pair: (&str, &str),
        ending: CallEnding,
        gap: Option<crate::formation_trace::CallGap>,
    ) -> FormationCall {
        FormationCall {
            depth,
            call_id: call_id.map(str::to_string),
            caller: pair.0.to_string(),
            callee: pair.1.to_string(),
            started_ms: 0,
            ending,
            gap,
            calling_task_id: None,
            member_task_id: None,
            busy_offers: 0,
        }
    }

    /// A call row reads `<call id>  <caller> → <callee>  <status>  <duration>  <delivery>` and its
    /// note, indented two spaces per depth with every column aligned; an outstanding or unknown
    /// call has no duration and no delivery.
    #[test]
    fn formation_call_rows_are_nested_aligned_and_carry_their_gap_note() {
        use crate::formation_trace::CallGap;
        let ended = |status: &str, duration_ms: u64, delivered: bool| CallEnding::Ended {
            status: status.to_string(),
            duration_ms,
            delivered,
        };
        let calls = [
            formation_call(
                0,
                Some("mcl_1"),
                ("chief", "lead-b"),
                ended("completed", 12_300, true),
                None,
            ),
            formation_call(
                1,
                Some("mcl_2"),
                ("lead-b", "worker-b4"),
                ended("rejected", 4, true),
                Some(CallGap::NeverHeld),
            ),
            FormationCall {
                busy_offers: 3,
                ..formation_call(
                    1,
                    Some("mcl_3"),
                    ("lead-b", "w"),
                    ended("abandoned", 900, false),
                    None,
                )
            },
            formation_call(
                0,
                Some("mcl_4"),
                ("chief", "lead-a"),
                CallEnding::Outstanding,
                Some(CallGap::CalleeTaskEnded(None)),
            ),
            formation_call(
                0,
                None,
                ("ghost", "lead-a"),
                CallEnding::Unknown,
                Some(CallGap::CallerTraceNotFound(Some("completed".to_string()))),
            ),
        ];
        assert_eq!(
            formation_call_lines(&calls),
            [
                "mcl_1         chief → lead-b      completed    12.3s  delivered",
                "  mcl_2       lead-b → worker-b4  rejected     4ms    delivered      never held by worker-b4",
                "  mcl_3       lead-b → w          abandoned    900ms  not delivered  busy ×3",
                "mcl_4         chief → lead-a      outstanding  -                     lead-a's task has no task_end",
                "(no call id)  ghost → lead-a      unknown      -                     ghost's trace not found; lead-a's task ended completed",
            ]
        );
    }

    /// `mur trace steps` refuses a formation id before resolving any session, and points at
    /// `mur trace show` for it.
    #[test]
    fn trace_steps_refuses_a_formation_id_and_points_at_trace_show() {
        let id = "frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b";
        let error = run_trace_steps(
            Some(id.to_string()),
            Some(PathBuf::from("/nonexistent/workdir")),
            false,
        )
        .unwrap_err();
        assert_eq!(error.code, E_TRC_001);
        assert_eq!(
            error.message,
            "mur trace steps reads one session's trace; a formation id names several sessions — \
             `mur trace show frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b` lists the formation's members \
             and calls"
        );
    }
}
