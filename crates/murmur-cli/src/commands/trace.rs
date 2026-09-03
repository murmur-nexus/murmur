use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Subcommand;
use serde::Deserialize;

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::session_address::{self, ses_entries, SessionQuery};

const E_TRC_001: &str = "E-TRC-001";
const E_TRC_002: &str = "E-TRC-002";

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
}

#[derive(Debug, Deserialize)]
struct InferenceEvent {
    turn: u32,
    decision: String,
    #[serde(default)]
    tool_name: Option<String>,
    /// `hook:<name>` when a hook produced this completion through `run-inference`. Absent on
    /// an agent-loop turn, which is how the Wire section and the divergence comparison — both
    /// of which pair records by turn — keep a hook's completion out of a turn's own record.
    #[serde(default)]
    origin: Option<String>,
    /// The provider's own counts, each written only when the driver reported it.
    #[serde(default)]
    input_tokens_actual: Option<u64>,
    #[serde(default)]
    output_tokens_actual: Option<u64>,
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
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
}

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

/// One `delegation_late` record: a released child that ended after its deadline had already been
/// reported. Joined to the row its `delegation_start` opened by the same `dlg_` id.
#[derive(Debug, Deserialize)]
struct DelegationLateEvent {
    #[serde(default)]
    delegation_id: Option<String>,
    status: String,
    after_deadline_ms: u64,
    #[serde(default)]
    result_path: Option<String>,
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
    CallDenied(CallDeniedEvent),
    ProtectedPathDenied(ProtectedPathDeniedEvent),
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
    DelegationLate(DelegationLateEvent),
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
}

impl InferenceRecord {
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
#[derive(Default)]
struct ProviderTokens {
    input: u64,
    output: u64,
    cached: u64,
    cache_write: u64,
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
}

/// A call that re-observed a resource already observed earlier in the session with no
/// intervening call that changed it. "Observed" vs. "changed" is decided entirely by each
/// call's self-declared `state_effect` (see [`StateEffect`]); *which* resource was addressed
/// comes from [`resolve_resource_identity`]. The detector recognizes no tool or operation by
/// name, so a brand-new tool is handled correctly the moment its author declares its effects.
struct RedundantCallRecord {
    turn: u32,
    tool_name: String,
    /// The resolved resource identity — a tool-declared `resource_id` when present,
    /// otherwise a path sniffed from the call's input. Rendered verbatim, unlabeled.
    resource_id: String,
    /// The earlier turn whose read of the same resource this call duplicates.
    prior_turn: u32,
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
    /// The session that spawned this one, and the delegation that created it. Both `None` for a
    /// capsule nobody delegated.
    spawned_by: Option<String>,
    spawned_by_delegation: Option<String>,
    /// Every delegation this session made, in the order it started them.
    delegations: Vec<DelegationRecord>,
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
    /// How a released child ended after its `timed_out` outcome was already recorded, and how long
    /// after the deadline. `None` for every delegation that was not released, and for a released
    /// one whose child has not ended.
    late: Option<(String, u64, Option<String>)>,
}

/// One `retention` trace record, surfaced in `mur trace show`.
struct RetentionRecord {
    store: String,
    reason: String,
    removed: u32,
    targets: Vec<String>,
    messages_dropped: Option<u64>,
}

/// One `task_reopened` trace record, surfaced in `mur trace show`.
struct ReopenRecord {
    reopen_number: u32,
    hook_name: String,
    reason: String,
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
    // resource → the turn of the most recent call that declared it *read* that resource;
    // invalidated when a later call declares it *mutated* (or is undeclared, treated
    // conservatively as a mutate) against the same resource.
    let mut resource_last_access: HashMap<String, u32> = HashMap::new();
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
    // Task ids seen on a `task_start`, so a `task_end` with no opening line is ignored rather
    // than counted as a task.
    let mut task_starts: HashSet<String> = HashSet::new();
    let mut task_metrics: Vec<TaskMetrics> = Vec::new();
    let mut reopens: Vec<ReopenRecord> = Vec::new();
    let mut context_seeds: Vec<ContextSeedRecord> = Vec::new();
    let mut denials: Vec<DenialRecord> = Vec::new();
    let mut protected_path_denials: Vec<ProtectedPathDenialRecord> = Vec::new();
    let mut hook_failures: Vec<HookFailureRecord> = Vec::new();
    let mut retentions: Vec<RetentionRecord> = Vec::new();
    let mut resource_lists = OutcomeCounts::new();
    let mut resource_reads = OutcomeCounts::new();
    let mut peer_mints = OutcomeCounts::new();
    let mut peer_redeems = OutcomeCounts::new();
    let mut peer_fetches = OutcomeCounts::new();
    let mut a2a_tasks_received = 0u32;
    let mut a2a_sends: Vec<String> = Vec::new();
    let mut delegations: Vec<DelegationRecord> = Vec::new();

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
                {
                    let totals = provider_tokens.get_or_insert_with(ProviderTokens::default);
                    totals.input += e.input_tokens_actual.unwrap_or(0);
                    totals.output += e.output_tokens_actual.unwrap_or(0);
                    totals.cached += e.cached_tokens.unwrap_or(0);
                    totals.cache_write += e.cache_write_tokens.unwrap_or(0);
                }
                inference_records.push(InferenceRecord {
                    turn: e.turn,
                    decision: e.decision,
                    origin: e.origin,
                    system_sha: e.system_sha,
                    tools_sha: e.tools_sha,
                    response_sha: e.response_sha,
                    message_shas: e.message_shas,
                });
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
                            if let Some(&prior_turn) = resource_last_access.get(&resource) {
                                redundant_calls.push(RedundantCallRecord {
                                    turn: e.turn,
                                    tool_name: e.tool_name.clone(),
                                    resource_id: resource.clone(),
                                    prior_turn,
                                });
                            }
                            resource_last_access.insert(resource, e.turn);
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
            TraceEvent::TaskReopened(e) => {
                reopens.push(ReopenRecord {
                    reopen_number: e.reopen_number,
                    hook_name: e.hook_name,
                    reason: e.reason,
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
            TraceEvent::DelegationStart(e) => delegations.push(DelegationRecord {
                delegation_id: Some(e.delegation_id),
                capsule: e.capsule,
                version: e.version,
                child_session_id: Some(e.child_session_id),
                child_workdir: Some(e.child_workdir),
                outcome: None,
                reason: None,
                late: None,
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
                        late: None,
                    }),
                }
            }
            // Annotates the row it belongs to rather than opening one: a late outcome is the same
            // delegation ending, never a second delegation.
            TraceEvent::DelegationLate(e) => {
                if let Some(record) = delegations
                    .iter_mut()
                    .find(|record| record.late.is_none() && record.delegation_id == e.delegation_id)
                {
                    record.late = Some((e.status, e.after_deadline_ms, e.result_path));
                }
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
        CliError::new(
            E_TRC_001,
            format!("{}: no session_end event found", path.display()),
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
            reopens,
            context_seeds,
            denials,
            protected_path_denials,
            hook_failures,
            retentions,
            resource_lists,
            resource_reads,
            peer_mints,
            peer_redeems,
            peer_fetches,
            a2a_tasks_received,
            a2a_sends,
            spawned_by: ss.spawned_by,
            spawned_by_delegation: ss.delegation_id,
            delegations,
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
    println!("── Session ──────────────────────────────────────");
    println!("session:    {}", m.session_id);
    // Only for a capsule another capsule launched. One level up from here is where "why did this
    // run?" is answered, and this is the only line in the file that names it.
    if let Some(parent) = &m.spawned_by {
        match &m.spawned_by_delegation {
            Some(id) => println!("Spawned by {parent} (delegation {id})"),
            None => println!("Spawned by {parent}"),
        }
    }
    println!("capsule:    {} v{}", m.capsule_name, m.capsule_version);
    println!("model:      {}", m.model);
    println!("status:     {}", m.exit_status);
    println!("duration:   {}", fmt_dur(m.duration_ms));
    if !m.capabilities.is_empty() {
        println!("{:<11} {}", "capabilities:", m.capabilities.join(", "));
    }
    if !m.tools_declared.is_empty() {
        println!("{:<11} {}", "tools:", m.tools_declared.join(", "));
    }
    // What was asked for against what this host could enforce. The two are read together:
    // a capsule that declared `sealed` and achieved `advisory` ran with neither.
    if let (Some(declared), Some(achieved)) = (&m.containment_declared, &m.containment_achieved) {
        println!("{:<11} {} → {}", "containment:", declared, achieved);
    }
    if let Some(exec) = m.workdir_exec {
        println!(
            "{:<11} {}",
            "workdir exec:",
            if exec { "yes" } else { "no" }
        );
    }
    if let Some(grant) = &m.userns_grant {
        println!("{:<11} {}", "userns:", grant);
    }
    if let Some(source) = &m.system_prompt_source {
        let sha = match &m.system_prompt_sha256 {
            Some(sha) => format!("  {}", fmt_sha_short(sha)),
            None => String::new(),
        };
        println!("{:<11} {}{}", "prompt:", source, sha);
    }
    println!();

    // Placed where it cannot be scrolled past: a hook that failed left the session running
    // as if it had returned nothing, and no other section says so.
    if !m.hook_failures.is_empty() {
        println!("── Hook failures ────────────────────────────────");
        for f in &m.hook_failures {
            println!("✗ {}  {}  {}", f.hook_name, f.event, f.arm);
        }
        println!();
    }

    // Beside the hook failures, and for the same reason: this is the only record that something
    // an operator might go looking for is gone, and why.
    if !m.retentions.is_empty() {
        println!("── Retention ────────────────────────────────────");
        for r in &m.retentions {
            let dropped = r
                .messages_dropped
                .map(|n| format!(", {n} messages dropped"))
                .unwrap_or_default();
            println!(
                "{}  {}  removed {}{}",
                r.store, r.reason, r.removed, dropped
            );
            if !r.targets.is_empty() {
                println!("  {}", r.targets.join(", "));
            }
        }
        println!();
    }

    if !m.context_seeds.is_empty() {
        println!("── Context ──────────────────────────────────────");
        for seed in &m.context_seeds {
            println!(
                "{}  {}  {} tokens (proposed {}, budget {})",
                seed.hook_name,
                seed.outcome,
                fmt_thousands(seed.tokens),
                fmt_thousands(seed.proposed_tokens),
                fmt_thousands(seed.budget_tokens)
            );
            if let Some(reason) = &seed.reason {
                println!("  reason:   {}", reason);
            }
            if !seed.message_ids.is_empty() {
                println!("  messages: {}", seed.message_ids.join(", "));
            }
        }
        println!();
    }

    println!("── Turns ────────────────────────────────────────");
    println!("count:      {}  (max: {})", m.total_turns, m.max_turns);
    println!();

    println!("── Tokens ───────────────────────────────────────");
    println!(
        "input:      {}  (avg {}/turn)",
        fmt_thousands(m.total_input_tokens),
        fmt_opt_f(m.avg_input_per_turn())
    );
    println!(
        "output:     {}  (avg {}/turn)",
        fmt_thousands(m.total_output_tokens),
        fmt_opt_f(m.avg_output_per_turn())
    );
    println!(
        "total:      {}",
        fmt_thousands(m.total_input_tokens + m.total_output_tokens)
    );
    // The provider's own counts, beside the runtime's tiktoken estimates above rather than
    // replacing them: the difference between the two lines is estimator drift.
    if let Some(p) = &m.provider_tokens {
        println!(
            "provider:   in {}, out {}, cached {}, cache write {}",
            fmt_thousands(p.input),
            fmt_thousands(p.output),
            fmt_thousands(p.cached),
            fmt_thousands(p.cache_write)
        );
    }
    println!();

    let wire_turns: Vec<&InferenceRecord> = m
        .inference_records
        .iter()
        .filter(|rec| rec.is_agent_loop() && rec.has_hashes())
        .collect();
    if !wire_turns.is_empty() {
        println!("── Wire ─────────────────────────────────────────");
        for rec in &wire_turns {
            println!(
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
        println!(
            "bodies:     mur trace show --body system --turn {}",
            wire_turns[0].turn
        );
        println!();
    }

    println!("── Tool calls ───────────────────────────────────");
    if m.total_tool_calls == 0 {
        println!("count:      0");
    } else {
        println!(
            "count:      {}  ({} ok, {} error)  success {:.1}%",
            m.total_tool_calls,
            m.tool_ok,
            m.tool_error,
            m.tool_success_rate().unwrap_or(0.0)
        );
        println!("latency:    avg {}", fmt_opt_dur(m.avg_tool_latency_ms()));
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
                println!("  turn {}  {}", turn, parts.join("  "));
            } else if let Some(decision) = inference_map.get(&turn) {
                println!("  turn {}  {}", turn, decision);
            }
        }
    }
    println!();

    println!("── Redundant calls ──────────────────────────────");
    println!("count:      {}", m.redundant_calls.len());
    for rec in &m.redundant_calls {
        println!(
            "  turn {}  {}  {}  (re-reads turn {})",
            rec.turn, rec.tool_name, rec.resource_id, rec.prior_turn
        );
    }
    println!();

    println!("── Skill calls ──────────────────────────────────");
    let total_skill = m.total_skill_calls();
    if total_skill == 0 {
        println!("count:      0");
    } else {
        println!(
            "count:      {}  ({} ok, {} error)  success {:.1}%",
            total_skill,
            m.skill_ok,
            m.skill_error,
            m.skill_success_rate().unwrap_or(0.0)
        );
        println!("latency:    avg {}", fmt_opt_dur(m.avg_skill_latency_ms()));
        let mut by_turn: BTreeMap<u32, Vec<&SkillCallRecord>> = BTreeMap::new();
        for rec in &m.skill_call_records {
            by_turn.entry(rec.turn).or_default().push(rec);
        }
        for (turn, records) in &by_turn {
            let parts: Vec<String> = records
                .iter()
                .map(|rec| {
                    let icon = if rec.status == "ok" { "✓" } else { "✗" };
                    format!("{} {} {}", rec.skill_name, fmt_dur(rec.duration_ms), icon)
                })
                .collect();
            println!("  turn {}  {}", turn, parts.join("  "));
        }
    }
    println!();

    println!("── Shell calls ──────────────────────────────────");
    if m.total_shell_calls == 0 {
        println!("count:      0");
    } else {
        println!("count:      {}", m.total_shell_calls);
        println!("exit codes: {}", fmt_exit_codes(&m.shell_exit_codes));
        println!("latency:    avg {}", fmt_opt_dur(m.avg_shell_latency_ms()));
    }
    println!();

    println!("── Compaction ───────────────────────────────────");
    match &m.compaction {
        None => println!("fired:      no"),
        Some(c) => println!(
            "fired:      yes  at turn {}  ({} → {} tokens)",
            c.turn,
            fmt_thousands(c.tokens_before),
            fmt_thousands(c.tokens_after)
        ),
    }
    for d in &m.compactions_declined {
        println!(
            "declined:   at turn {}  ({} tokens)  {}",
            d.turn,
            fmt_thousands(d.tokens),
            d.reason
        );
    }

    if !m.reopens.is_empty() {
        println!();
        println!("── Reopens ──────────────────────────────────────");
        for r in &m.reopens {
            let reason: String = r.reason.chars().take(80).collect();
            println!(
                "reopen {}  by {}  “{}”",
                r.reopen_number, r.hook_name, reason
            );
        }
    }

    if !m.denials.is_empty() {
        println!();
        println!("── Denied calls ─────────────────────────────────");
        for d in &m.denials {
            let reason: String = d.reason.chars().take(80).collect();
            println!(
                "turn {}  {}  {}  by {}  “{}”",
                d.turn, d.event, d.target, d.hook_name, reason
            );
        }
    }

    // Beside the hook denials and for the same reason: no `shell` or `tool_call` line exists for
    // a refused call, so this is the only account of it. The count is the comparable number — a
    // run that attempted a protected write and was refused is a different result from one that
    // never tried.
    if !m.protected_path_denials.is_empty() {
        println!();
        println!("── Protected paths ──────────────────────────────");
        println!(
            "protected-path refusals: {}",
            m.protected_path_denials.len()
        );
        for d in &m.protected_path_denials {
            println!(
                "turn {}  {}  {}  path {}  rule {}  ({})",
                d.turn, d.call, d.target, d.path, d.rule, d.signal
            );
        }
    }

    if !m.resource_lists.is_empty() || !m.resource_reads.is_empty() {
        println!();
        println!("── Resource plane ───────────────────────────────");
        if !m.resource_lists.is_empty() {
            println!("list:       {}", fmt_outcomes(&m.resource_lists));
        }
        if !m.resource_reads.is_empty() {
            println!("read:       {}", fmt_outcomes(&m.resource_reads));
        }
    }

    if !m.peer_mints.is_empty() || !m.peer_redeems.is_empty() || !m.peer_fetches.is_empty() {
        println!();
        println!("── Peer files ───────────────────────────────────");
        if !m.peer_mints.is_empty() {
            println!("minted:     {}", fmt_outcomes(&m.peer_mints));
        }
        if !m.peer_redeems.is_empty() {
            println!("redeemed:   {}", fmt_outcomes(&m.peer_redeems));
        }
        if !m.peer_fetches.is_empty() {
            println!("fetched:    {}", fmt_outcomes(&m.peer_fetches));
        }
    }

    if !m.delegations.is_empty() {
        println!();
        println!("── Delegations ──────────────────────────────────");
        for d in &m.delegations {
            println!(
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
                println!("  {reason}");
            }
            // A released child that ended anyway. The parent was told twice about this one
            // delegation, and the second telling is what this line is.
            if let Some((status, after_deadline_ms, result_path)) = &d.late {
                println!("  ended {status} {after_deadline_ms}ms after the deadline");
                if let Some(path) = result_path {
                    println!("  late result: {path}");
                }
            }
            // The one join a reader would otherwise have to compose by hand, and the only thing
            // in this file that points outside it. Relative to this capsule's accessible workdir.
            if let (Some(workdir), Some(child)) = (&d.child_workdir, &d.child_session_id) {
                println!("  child trace: {workdir}/.murmur/{child}/trace.jsonl");
            }
        }
    }

    if m.a2a_tasks_received > 0 || !m.a2a_sends.is_empty() {
        println!();
        println!("── A2A ──────────────────────────────────────────");
        println!(
            "received:   {} task{}",
            m.a2a_tasks_received,
            if m.a2a_tasks_received == 1 { "" } else { "s" }
        );
        println!(
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
            println!("  → {}", url);
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
    turns: Vec<InferenceRecord>,
    /// In file order, deduplicated: `session_start.system_prompt_sha256` first, then each
    /// turn's system, tools, response and message hashes.
    known_hashes: Vec<String>,
}

impl WireIndex {
    fn build(events: Vec<TraceEvent>) -> Self {
        let mut turns = Vec::new();
        let mut known_hashes: Vec<String> = Vec::new();
        fn note(hash: Option<&String>, known: &mut Vec<String>) {
            if let Some(h) = hash {
                if !known.iter().any(|k| k == h) {
                    known.push(h.clone());
                }
            }
        }
        for event in events {
            match event {
                TraceEvent::SessionStart(e) => {
                    note(e.system_prompt_sha256.as_ref(), &mut known_hashes);
                }
                TraceEvent::Inference(e) => {
                    let record = InferenceRecord {
                        turn: e.turn,
                        decision: e.decision,
                        origin: e.origin,
                        system_sha: e.system_sha,
                        tools_sha: e.tools_sha,
                        response_sha: e.response_sha,
                        message_shas: e.message_shas,
                    };
                    note(record.system_sha.as_ref(), &mut known_hashes);
                    note(record.tools_sha.as_ref(), &mut known_hashes);
                    note(record.response_sha.as_ref(), &mut known_hashes);
                    for sha in &record.message_shas {
                        note(Some(sha), &mut known_hashes);
                    }
                    if record.is_agent_loop() {
                        turns.push(record);
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

    fn turn(&self, n: u32) -> Option<&InferenceRecord> {
        self.turns.iter().find(|rec| rec.turn == n)
    }

    /// `1, 2, 3` — the turns a `--turn` argument can name.
    fn turn_list(&self) -> String {
        self.turns
            .iter()
            .map(|rec| rec.turn.to_string())
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
    let record = index.turn(n).ok_or_else(|| {
        CliError::new(
            E_TRC_001,
            format!("turn {n} has no inference record in this trace"),
        )
    })?;
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

/// Print the recorded body behind one hash to stdout, and nothing else — no header, no added
/// newline — so the output pipes into `sha256sum` and matches the blob's own name.
fn print_body(path: &Path, arg: &str, turn: Option<u32>) -> Result<(), CliError> {
    let selector = parse_body_selector(arg)?;
    let index = WireIndex::build(parse_trace_file(path)?);
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

    let mut out = std::io::stdout();
    out.write_all(&bytes)
        .and_then(|()| out.flush())
        .map_err(|e| CliError::new(E_IO_003, format!("failed to write body to stdout: {e}")))
}

pub(crate) fn run_trace_show(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
    body: Option<String>,
    turn: Option<u32>,
) -> Result<(), CliError> {
    if body.is_none() && turn.is_some() {
        return Err(CliError::new(
            E_TRC_001,
            "--turn has no meaning without --body",
        ));
    }
    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_session(session, &workdir)?;
    if let Some(arg) = &body {
        return print_body(&path, arg, turn);
    }
    let (metrics, tasks) = load_metrics(&path)?;
    print_show(&metrics);
    if tasks.len() > 1 {
        println!("── Tasks ───────────────────────────────────────");
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
            println!(
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
    Ok(())
}

pub(crate) fn run_trace_steps(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
    verbose: bool,
) -> Result<(), CliError> {
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
            Some(origin) => format!("{}{}  {}", kind("inference"), origin, e.decision),
            None => match &e.tool_name {
                Some(tool) => format!("turn {}  {}  {}", e.turn, e.decision, tool),
                None => format!("turn {}  {}", e.turn, e.decision),
            },
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
            "{}{}  {}  {}",
            kind("skill_call"),
            e.skill_name,
            fmt_dur(e.duration_ms),
            if e.status == "ok" { "✓" } else { "✗" }
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
            "{}{}  reopen {}",
            kind("task_reopened"),
            e.hook_name,
            e.reopen_number
        ),
        TraceEvent::CallDenied(e) => format!(
            "{}{}  {}  denied by {}",
            kind("call_denied"),
            e.event,
            e.target,
            e.hook_name
        ),
        TraceEvent::ProtectedPathDenied(e) => format!(
            "{}{}  {}  rule {}",
            kind("protected_path_denied"),
            e.call,
            e.path,
            e.rule
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
            TraceEvent::Inference(e) if e.origin.is_none() => turns += 1,
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
    println!(
        "Session {}  ({}{} turn{})",
        session_id,
        task_note,
        turns,
        if turns == 1 { "" } else { "s" }
    );

    for root in roots {
        walk_steps_tree(records, &children, root, 0, verbose);
    }
    println!();
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
                println!();
            }
            println!("{}{}", "  ".repeat(depth), row);
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
    println!(
        "Session {}  ({} turn{})",
        session_id,
        n,
        if n == 1 { "" } else { "s" }
    );
    println!();

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
            println!(
                "  {:<3}{:<13}{}{:<5}   {}",
                turn, decision, tool_padded, dur_str, input_summary
            );
        } else {
            println!("  {:<3}{:<13}{}{}", turn, decision, tool_padded, dur_str);
        }
    }

    println!();
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
    println!(
        "Session {}  {}  {} turn{}",
        fmt_session_short(&m.session_id),
        fmt_dur(m.duration_ms),
        m.total_turns,
        if m.total_turns == 1 { "" } else { "s" }
    );
    println!();

    if m.tool_error > 0 {
        println!(
            "  {:<14} {}  ({} ok, {} error)",
            "Tool calls:", m.total_tool_calls, m.tool_ok, m.tool_error
        );
    } else {
        println!("  {:<14} {}", "Tool calls:", m.total_tool_calls);
    }

    if m.total_shell_calls > 0 {
        println!(
            "  {:<14} {}  exit codes: {}",
            "Shell calls:",
            m.total_shell_calls,
            fmt_exit_codes(&m.shell_exit_codes)
        );
    }

    if !m.redundant_calls.is_empty() {
        println!("  Redundant calls: {}", m.redundant_calls.len());
    }

    if let Some(avg_tool) = m.avg_tool_latency_ms() {
        let shell_part = m
            .avg_shell_latency_ms()
            .map(|v| format!("  shell {}", fmt_dur(v as u64)))
            .unwrap_or_default();
        println!(
            "  {:<14} tool {}{}",
            "Avg latency:",
            fmt_dur(avg_tool as u64),
            shell_part
        );
    }

    println!();
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

    println!(
        "{:<COL$} {:<VAL$} {:<VAL$} Delta",
        "Metric", "Run A", "Run B"
    );
    println!(
        "{} {} {} {}",
        "─".repeat(COL),
        "─".repeat(VAL),
        "─".repeat(VAL),
        "─".repeat(26)
    );

    macro_rules! row {
        ($label:expr, $va:expr, $vb:expr, $delta:expr) => {
            println!(
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
    println!("{:<15}{}", label, body);
}

/// Where two runs' prompts stopped agreeing — the answer to "why did my cache miss".
///
/// Divergence has no polarity: neither run is better for having a longer or shorter agreeing
/// prefix, so no `(A better)`/`(B better)` marker appears anywhere in this section.
fn print_prefix_divergence(a: &TraceMetrics, b: &TraceMetrics) {
    println!();
    println!("── Prefix divergence ────────────────────────────");

    let turns_a = hashed_turns(a);
    let turns_b = hashed_turns(b);
    // Nothing to compare: `trace.capture: none` writes no hashes at all, which is a different
    // record from a session that recorded hashes and stored no bodies.
    match (turns_a.is_empty(), turns_b.is_empty()) {
        (true, true) => {
            println!(
                "runs A and B recorded no content hashes — both ran under trace.capture: none"
            );
            return;
        }
        (true, false) => {
            println!("run A recorded no content hashes — it ran under trace.capture: none");
            return;
        }
        (false, true) => {
            println!("run B recorded no content hashes — it ran under trace.capture: none");
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
            println!(
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
        println!("turn {}:  {}", turn, line);
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
    println!(
        "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} {}",
        label,
        format_fn(stat_mean(values)),
        format_fn(stat_stddev(values)),
        format_fn(stat_min(values)),
        format_fn(stat_max(values))
    );
}

fn print_report(workdir: &Path, stats: &[RunStats]) {
    println!("Sessions: {}  ({})", stats.len(), workdir.display());
    println!();

    const COL: usize = 22;
    const VAL: usize = 14;
    println!(
        "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} Max",
        "Metric", "Mean", "StdDev", "Min"
    );
    println!(
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

    println!();
    println!("Exit status:");
    let mut exit_dist: HashMap<String, usize> = HashMap::new();
    for s in stats {
        *exit_dist.entry(s.exit_status.clone()).or_insert(0) += 1;
    }
    let total = stats.len();
    let mut sorted: Vec<(&String, &usize)> = exit_dist.iter().collect();
    sorted.sort_by_key(|(k, _)| k.as_str());
    for (status, count) in &sorted {
        println!(
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
        eprintln!("note: skipped {} incomplete session(s)", skipped);
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
        println!();
        println!("── Per-task averages (multi-task sessions only) ──────────────");
        const COL: usize = 22;
        const VAL: usize = 14;
        println!(
            "{:<COL$} {:<VAL$} {:<VAL$} {:<VAL$} Max",
            "Metric", "Mean", "StdDev", "Min"
        );
        println!(
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
        println!("Tasks: {}", all_task_metrics.len());
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
}
