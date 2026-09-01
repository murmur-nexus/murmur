use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use murmur_artifact::{ContainmentClass, TraceCapture};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
};

use crate::{
    agent::DriverUsage,
    containment::ScopeReport,
    lanes::TaskLane,
    origin::{TaskProvenance, TrustClass},
    trace_blobs::BlobStore,
};

pub(crate) struct TraceWriter {
    writer: BufWriter<File>,
    session_id: String,
    capsule_name: String,
    capsule_version: String,
    model: String,
    capabilities: Vec<String>,
    /// The session's complete effective grant set, computed once at stage time. Written whole to
    /// `session_start` as `effective_grants`, and also the source of that event's
    /// `containment_declared`/`containment_achieved`/`workdir_exec` summary fields — they are read
    /// off this report rather than passed in beside it, so a trace cannot claim one containment
    /// class at the top level and another inside the report.
    effective_grants: ScopeReport,
    /// How much of each turn's driver request this trace keeps — see [`TraceCapture`]. Decides
    /// whether an `inference` event carries content hashes at all, whether the bodies behind
    /// them reach [`Self::blobs`], and whether `tool_call.output` is written.
    capture: TraceCapture,
    /// The content-addressed bodies this session stored, at `<workdir>/blobs/<sha256>`. Only
    /// written to under [`TraceCapture::Content`]; the directory is created on the first blob.
    blobs: BlobStore,
    /// Where the effective system prompt came from: `"manifest"`, `"cli"` or `"none"`. Derived
    /// once in [`TraceWriter::open`] from the resolved prompt and the override flag, then repeated
    /// on every `session_start` the same way `model` is — the prompt cannot change between the
    /// tasks of one session.
    system_prompt_source: &'static str,
    /// SHA-256 (lowercase hex) of the resolved system prompt, or `None` when no prompt is in
    /// effect. Always written to `session_start` (as `null` when absent) so a trace records *that*
    /// a prompt was in effect and which one, even when the text itself is withheld.
    system_prompt_sha256: Option<String>,
    /// The session `mur run --resume` continued, verbatim as the operator's address resolved it.
    /// `None` on every ordinary launch. Written to `session_start` on both, so its absence
    /// identifies a trace from a runtime that predates the key.
    resumed_from: Option<String>,
    /// The launch-scoped context id: the `--context` value, or the id `--resume` resolved to.
    /// `None` when each task of this launch mints its own.
    context_id: Option<String>,
    session_start_time: Instant,
    /// The session node of the event tree: the `event_id` `session_start` carries, and the
    /// `parent_id` every launch-scoped event names. Minted in [`TraceWriter::open`] rather than
    /// at write time so [`ResourceTraceAppender`], which is opened before the frame is written,
    /// can be handed the same value.
    session_event_id: String,
    session_started: bool,
    session_ended: bool,
    /// The task node: the `event_id` of the active task's `task_start`. Set by
    /// [`Self::write_task_start`], cleared by [`Self::write_task_end`], `None` between tasks.
    task_event_id: Option<String>,
    /// The turn node: the `event_id` of the active turn's own agent-loop `inference`. Every
    /// event a turn produces hangs off it. Cleared at both task boundaries so a new task never
    /// parents its first events to the previous task's last turn.
    turn_event_id: Option<String>,
    // Running totals — updated as events are written, used for fallback session_end on error exit
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    // Per-task counters — reset on each write_task_start; consumed by write_task_end
    task_turns: u32,
    task_input_tokens: u64,
    task_output_tokens: u64,
    task_tool_calls: u32,
    task_shell_calls: u32,
    task_start_instant: Option<Instant>,
    pub(crate) active_task_id: Option<String>,
}

/// Provenance of an inference record that did **not** come from the agent
/// loop's own turn — today, only a hook's `run-inference` call. `None` at a
/// `write_inference`/`emit_inference` call site means "ordinary agent-loop
/// turn", which serializes exactly as it did before this existed.
#[derive(Debug, Clone)]
pub(crate) struct InferenceOrigin {
    /// `hook:<manifest name>` of the hook that made the call.
    pub(crate) source: String,
    /// Model string actually sent (the attempted model, on failure).
    pub(crate) model: String,
}

/// `compaction_declined.reason` when the threshold was crossed but no bound hook returned
/// `replace-context`.
pub(crate) const COMPACTION_DECLINED_NO_HOOK_REPLACEMENT: &str = "no_hook_replacement";

/// `compaction_declined.reason` when a hook's replacement was rejected because its tool calls
/// and tool results did not pair up.
pub(crate) const COMPACTION_DECLINED_UNRESOLVED_TOOL_CALL: &str = "unresolved_tool_call";

/// `context_seed.outcome` when the whole proposal fit the budget and was committed as-is.
pub(crate) const SEED_OUTCOME_SEEDED: &str = "seeded";

/// `context_seed.outcome` when the proposal was over budget and the oldest messages were
/// dropped from its front until the rest fit.
pub(crate) const SEED_OUTCOME_TRIMMED: &str = "trimmed";

/// `context_seed.outcome` when the overflowing front was handed to the compaction hook and
/// its summary became the seed's first message.
pub(crate) const SEED_OUTCOME_COMPACTED: &str = "compacted";

/// `context_seed.outcome` when nothing was committed. Always paired with a `reason`.
pub(crate) const SEED_OUTCOME_REJECTED: &str = "rejected";

/// `context_seed.reason` when one message alone is wider than the whole budget. Trimming
/// cannot help — dropping everything else still leaves it over — so nothing is committed.
pub(crate) const SEED_REJECTED_MESSAGE_OVER_BUDGET: &str = "message_over_budget";

/// `context_seed.reason` when the proposal overflows the budget by more multiples of it than
/// the runtime will summarize. A seed that far over is a broken hook, not a full memory.
pub(crate) const SEED_REJECTED_OVERFLOW_OVER_LIMIT: &str = "overflow_over_limit";

/// `context_seed.reason` when the capsule declares no `context.max_tokens`, so
/// `context.seed_budget` is a fraction of nothing and there is no ceiling to enforce.
pub(crate) const SEED_REJECTED_NO_BUDGET: &str = "no_budget";

/// `context_seed.reason` when the session runs `inference.transport: process`, which owns its
/// own context and has no host-side message list to seed.
pub(crate) const SEED_REJECTED_UNSUPPORTED_TRANSPORT: &str = "unsupported_transport";

/// `retention.store` for the session directories under a workdir.
pub(crate) const RETENTION_STORE_SESSIONS: &str = "sessions";

/// `retention.store` for the conversation records under `~/.murmur/conversations/`.
pub(crate) const RETENTION_STORE_RECORDS: &str = "records";

/// `retention.reason` when `trace.retain.max_sessions` put a session outside the newest N.
pub(crate) const RETENTION_REASON_MAX_SESSIONS: &str = "max_sessions";

/// `retention.reason` when `trace.retain.max_age` or `context.retain.max_age` put a session or a
/// record outside the window.
pub(crate) const RETENTION_REASON_MAX_AGE: &str = "max_age";

/// `retention.reason` when `context.retain.max_messages` truncated the front of a record.
pub(crate) const RETENTION_REASON_MAX_MESSAGES: &str = "max_messages";

/// Mint one event identity: `evt_` + a UUID v7 in simple form, the same scheme `ses_`, `tsk_`,
/// `ctx_`, `dep_` and `req_` use. Ids therefore sort by mint time and carry their own
/// millisecond timestamp, so a trace can be ordered and time-bounded without reading any
/// payload field.
///
/// Called once per line at the moment of the write. An id is never reused, never derived from
/// the event's content, and never reconstructed from anything else in the file.
fn new_event_id() -> String {
    format!("evt_{}", uuid::Uuid::now_v7().simple())
}

// ── Event structs (Serialize → JSONL lines) ──────────────────────────────────
//
// Every struct opens with `event_type`, `event_id`, `parent_id`, `session_id` and `timestamp`,
// in that order, so a human scanning the file reads a line's identity before its payload.
// `parent_id` is the `event_id` of the event this one hangs off — see [`TraceWriter`]'s
// `session_parent`/`task_parent`/`turn_parent` for which node each event names, and
// [`new_event_id`] for the id format.
//
// The turn-level events (`inference`, `tool_call`, `skill_call`, `shell`, `compaction`,
// `compaction_declined`) additionally carry `task_id`, taken from the writer's active task and
// always written — `null` when no task is in scope. `task_start`/`task_end` carry their own and
// are not duplicated.

#[derive(Serialize)]
struct SessionStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    capsule_name: String,
    capsule_version: String,
    model: String,
    max_turns: u32,
    capabilities: Vec<String>,
    tools_declared: Vec<String>,
    /// Strongest containment class any source asked for. A *requirement*, not an observation.
    containment_declared: ContainmentClass,
    /// What this session actually ran under: the probed enforcement tier's ceiling, capped by
    /// `workdir_exec` below. Reading these two together is how a session's trace shows whether the
    /// capsule ran under the containment its operator asked for.
    containment_achieved: ContainmentClass,
    /// `capabilities.filesystem.workdir_exec`. Always written, including `false`, so its absence
    /// identifies a trace from a runtime that predates the key rather than a capsule that declined
    /// it.
    ///
    /// `true` is the reason a trace can show `containment_achieved: advisory` on a host whose
    /// `session_start` would otherwise say `scoped` — and it is the record that this session's
    /// `capabilities.shell.allow` was advisory too, since anything in the workdir could run.
    workdir_exec: bool,
    /// Where this host's permission to create an unprivileged user namespace came from, as
    /// `UsernsGrant::wire_name` — `null` only off Linux, where AppArmor does not exist.
    ///
    /// Mirrored to the top level from `effective_grants` for the same reason `workdir_exec` is:
    /// an auditor should not have to descend into a nested object to answer it. Recorded because
    /// `containment_achieved: sealed` reached through the shipped AppArmor profile and the same
    /// class reached on a host whose unprivileged-userns hardening is switched off for every
    /// binary are two very different records, and this key is the only thing separating them.
    ///
    /// Always written, on the same terms as `workdir_exec`: its absence identifies a trace from a
    /// runtime that predates the key.
    userns_grant: Option<crate::sealed::UsernsGrant>,
    /// The complete grant set this session ran under — every destination, binary, path and
    /// environment variable the policy actually opened, plus the probed enforcement tier — in the
    /// exact shape `mur run --explain-scope --json` prints for the same policy on the same host.
    ///
    /// `capabilities` above stays what it always was: a list of category *names*. It answers "did
    /// this session have network access?" but not "to where?", which is the question an auditor
    /// reading a finished trace actually has. This field answers it without re-parsing the
    /// manifest, which by then may have changed or moved.
    effective_grants: ScopeReport,
    /// Where the system prompt this session ran with came from: `"manifest"` when the manifest's
    /// own `inference.system_prompt`/`system_prompt_file`/`system_prompt_artifact` supplied it,
    /// `"cli"` when `mur run --system-prompt` overrode it (including when the override was empty
    /// and therefore cleared it), `"none"` when no prompt was in effect at all.
    ///
    /// Always written, on the same terms as `workdir_exec`: its absence identifies a trace from a
    /// runtime that predates the key, not a session that had no prompt.
    system_prompt_source: &'static str,
    /// SHA-256 (lowercase hex) of the resolved prompt — the text as `resolve_system_prompt`
    /// returned it, before the `[Capsule]` identity block is prepended — or `null` when no prompt
    /// was in effect. Always written, so two sessions can be compared for prompt equality without
    /// either trace having to carry the prompt itself.
    ///
    /// Deliberately not the same value as `inference.system_sha`, which covers the augmented
    /// prompt that actually went on the wire: this one names the prompt the operator wrote.
    /// Under `trace.capture: content` those resolved bytes are also the blob this hash names.
    system_prompt_sha256: Option<String>,
    /// The session `mur run --resume` continued, or `null` on an ordinary launch. Together with
    /// `context_id` below it is what makes a resumed conversation followable back through the
    /// sessions that built it.
    ///
    /// Always written, on the same terms as `workdir_exec`: its absence identifies a trace from a
    /// runtime that predates the key.
    resumed_from: Option<String>,
    /// The context id every task of this launch runs under — the `mur run --context` value, or
    /// the id `--resume` resolved to — and `null` when each task mints its own. `task_start`
    /// carries the id a task actually ran under either way.
    ///
    /// Always written, on the same terms as `resumed_from`.
    context_id: Option<String>,
}

#[derive(Serialize)]
struct InferenceEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    /// The runtime's own tiktoken estimate of the request, counted before the request was
    /// sent — not the provider's count, which arrives afterwards as `input_tokens_actual`.
    input_tokens: u64,
    /// The runtime's own tiktoken estimate of the raw driver response.
    output_tokens: u64,
    decision: String,
    tool_name: Option<String>,
    /// The provider's own counts for this call, each written only when the driver reported
    /// it. Absent means the driver reported nothing, never zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens_actual: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens_actual: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_write_tokens: Option<u64>,
    /// Where this inference came from. Absent for an ordinary agent-loop turn,
    /// so every pre-existing consumer sees a byte-identical record; `"hook:<name>"`
    /// for a completion that hook ran through `murmur:runtime/inference`'s
    /// `run-inference`.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    /// Model string actually sent for this call (the attempted model, on
    /// failure). Only written alongside `origin`: an agent-loop turn's model is
    /// already on the session-start record and is not repeated per turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Identities of the messages this request embedded, in the order they sat in it. Under an
    /// active driver continuation only the tail past the acked length is sent, and this names
    /// exactly that tail. Empty — and therefore absent — for a hook's own `run-inference` and for
    /// the `process` transport, neither of which sends a message list the runtime minted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    message_ids: Vec<String>,
    /// SHA-256 (lowercase hex) of the UTF-8 bytes of this request's `system` string — the
    /// augmented prompt with the `[Capsule]` block already in it.
    ///
    /// These are the bytes Murmur **sent**, not what the model **saw**: provider-side injection,
    /// tokenizer differences and safety layers all happen past the wire and are invisible to the
    /// runtime. Absent under `trace.capture: none`, and for a record the runtime did not build
    /// the payload for — a hook's `run-inference`, and the `process` transport.
    #[serde(skip_serializing_if = "Option::is_none")]
    system_sha: Option<String>,
    /// SHA-256 (lowercase hex) of this request's serialized `tools` array. The bytes Murmur
    /// sent, on the same terms as [`Self::system_sha`].
    #[serde(skip_serializing_if = "Option::is_none")]
    tools_sha: Option<String>,
    /// SHA-256 (lowercase hex) of the raw driver response body, as the loop read it before
    /// parsing. What the driver returned to Murmur, byte for byte.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_sha: Option<String>,
    /// SHA-256 (lowercase hex) of each message this request embedded, in send order — one entry
    /// per `message_ids` entry, over the same messages after the runtime's identity keys are
    /// stripped. The bytes Murmur sent, on the same terms as [`Self::system_sha`].
    ///
    /// Not redundant with `message_ids`: an id names an entity and is freshly minted every run,
    /// so comparing two runs' id arrays only says every id differs. Comparing these says where
    /// the two prompts stopped agreeing — the divergence index is the first unequal position.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    message_shas: Vec<String>,
}

/// The bodies one driver request put on the wire, split into the pieces an `inference` event
/// hashes.
///
/// Built from the payload [`crate::agent::build_driver_payload`] produced and the raw response
/// string the driver returned, so what the trace names and what the driver received cannot
/// drift: there is no second serialization for them to drift apart in.
///
/// Every piece is the bytes Murmur **sent** (or, for the response, was handed back), never what
/// the model saw.
pub(crate) struct WireCapture {
    /// The payload's `system` string value as UTF-8 text — the augmented prompt, `[Capsule]`
    /// block included. Stored as text so its blob `cat`s as a readable prompt.
    system: Vec<u8>,
    /// The payload's `tools` array, serialized.
    tools: Vec<u8>,
    /// Each element of the payload's `messages` array, serialized, in send order. That array is
    /// the `wire_messages` slice after `strip_message_identity`, so no message identity key can
    /// reach a hash or a blob.
    messages: Vec<Vec<u8>>,
    /// The raw driver response body, exactly as the loop read it.
    response: Vec<u8>,
}

impl WireCapture {
    /// Split a built driver payload and its raw response into the four bodies the trace hashes.
    ///
    /// `payload` must be the value that was serialized and handed to the driver; taking the
    /// pieces out of it, rather than rebuilding them from the loop's own state, is what makes a
    /// blob byte-identical to the substring of the request it came from.
    pub(crate) fn from_driver_payload(payload: &Value, response: &str) -> Self {
        Self {
            system: payload
                .get("system")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
            tools: payload
                .get("tools")
                .map(|tools| serde_json::to_vec(tools).unwrap_or_default())
                .unwrap_or_default(),
            messages: payload
                .get("messages")
                .and_then(Value::as_array)
                .map(|messages| {
                    messages
                        .iter()
                        .map(|message| serde_json::to_vec(message).unwrap_or_default())
                        .collect()
                })
                .unwrap_or_default(),
            response: response.as_bytes().to_vec(),
        }
    }
}

#[derive(Serialize)]
struct ToolCallEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    tool_name: String,
    /// The provider's own id for this call, as it appeared on the driver response block that
    /// asked for it. Recorded verbatim and never parsed — the same contract `resource_id` has.
    /// `null` when the provider named none, which is how the codex dialect's inline tool items
    /// and every host-synthesized call read.
    tool_call_id: Option<String>,
    input: Value,
    input_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
    /// The tool's self-declared `state_effect` for this call (`read`/`mutate`),
    /// lifted verbatim from `tool-result.metadata`. Absent when the tool declared
    /// nothing; consumers treat absence conservatively. See `wit/tool.wit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state_effect: Option<String>,
    /// The resource this call addressed, as declared by the tool and lifted verbatim
    /// from `tool-result.metadata`. An opaque, tool-defined string — never parsed here.
    /// Absent when the tool declared nothing, in which case consumers fall back to
    /// guessing the resource from the call's input. See `wit/tool.wit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_id: Option<String>,
}

#[derive(Serialize)]
struct SkillCallEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    skill_name: String,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
}

#[derive(Serialize)]
struct ShellEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    /// The program that ran — a canonical absolute path when the host `PATH` resolved
    /// the invoked name, else the bare name. `command` carries the argument list alone,
    /// so this is the only field that says *what* ran.
    binary: String,
    command: String,
    exit_code: i32,
    stdout_bytes: u64,
    stderr_bytes: u64,
    duration_ms: u64,
    /// The `capabilities.resources` field this subprocess was killed for exceeding, when the
    /// kernel's own evidence names exactly one (`SIGXCPU`/`SIGXFSZ`, or a cgroup
    /// `memory.events`/`pids.events` counter that moved). Omitted from the JSONL entirely
    /// otherwise — including for a subprocess that died for a reason no single limit can be
    /// pinned to, which must not read as "no limit was involved".
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_limit: Option<String>,
}

/// A shell command that outran its grace period and moved to the background. Hangs off the turn
/// that started it, so `mur trace steps` shows it where the model saw it.
#[derive(Serialize)]
struct ShellDetachedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    work_id: String,
    binary: String,
    command: String,
    /// The `lifecycle.shell_grace_secs` in force, in milliseconds — what this command outran.
    grace_ms: u64,
}

/// A detached command finishing, written where the task loop drains it. Hangs off the session
/// rather than a turn: by the time it lands, the turn that started the command is over.
#[derive(Serialize)]
struct ShellCompletedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    work_id: String,
    binary: String,
    command: String,
    exit_code: i32,
    duration_ms: u64,
    /// Workdir-relative path of the file holding the command's full stdout and stderr.
    output_path: String,
    output_bytes: u64,
    /// Attributed the same way [`ShellEvent::resource_limit`] is, and omitted on the same terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_limit: Option<String>,
    /// `"ok"` or `"error"` — the field that tells a failed background command from a successful
    /// one without reading its prose.
    status: String,
    /// The `completion`-origin task this completion was enqueued as, which is what makes "which
    /// command produced this task" answerable from `trace.jsonl` alone.
    completion_task_id: String,
}

/// A detached command still running when its session ended. Its result is lost; this record and
/// the stderr line beside it are what keep the loss visible.
#[derive(Serialize)]
struct ShellAbandonedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    work_id: String,
    binary: String,
    command: String,
    /// How long the command had been running when the session gave up on it.
    running_ms: u64,
}

/// A demoted command a later launch found unaccounted for, appended to the `trace.jsonl` of the
/// session that started it.
///
/// Carries no `exit_code`, no `status`, no `duration_ms`, no `output_path` and no `output_bytes`:
/// none of them exists for a command whose runtime died, and a record that named any of them
/// would be readable as a result. The event type is the whole discriminator against
/// [`ShellCompletedEvent`].
///
/// Its presence is also what stops the same work id being reported by a second resume, so this
/// line and the `shell_detached` it answers live in the same file and are pruned together by
/// [`crate::retention::prune_sessions`].
#[derive(Serialize)]
struct ShellLostEvent {
    event_type: &'static str,
    event_id: String,
    /// The prior session's own `session_start` node. Omitted from the line when that record could
    /// not be read back — a line must never name a parent the file does not hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    /// The prior session's id, not the resuming one's, so the file stays internally consistent.
    session_id: String,
    timestamp: u64,
    work_id: String,
    binary: String,
    command: String,
    /// The `shell_detached` line's own timestamp.
    detached_at_ms: u64,
    /// The session that found the work unaccounted for and reported it.
    reconciled_by_session: String,
    /// The `completion`-origin task that session enqueued to report it, which is what makes "which
    /// task reported this loss" answerable from the two files alone.
    reconciled_task_id: String,
}

/// A demotion whose own `shell_detached` line could not be written.
///
/// Best-effort and usually absent: it goes to the file that just failed. When it does land, it is
/// the record that the work id below has no `shell_detached` line and so will never be reported
/// as lost.
#[derive(Serialize)]
struct ShellDetachUnrecordedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    work_id: String,
    binary: String,
    /// The write error, as the operating system reported it.
    reason: String,
}

#[derive(Serialize)]
struct CompactionEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    tokens_before: u64,
    tokens_after: u64,
}

/// The compaction threshold was crossed and the context was left as it was. The session
/// continues over budget, so the record of *why* is the only thing separating this from a
/// session that never needed compacting at all.
#[derive(Serialize)]
struct CompactionDeclinedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    /// Context occupancy at the moment of the decline — the same measurement `compaction`
    /// records as `tokens_before`, and the budget the session went on running over.
    tokens: u64,
    /// `"no_hook_replacement"` when no bound hook returned `replace-context`;
    /// `"unresolved_tool_call"` when the replacement's tool calls and tool results did not pair.
    reason: String,
}

/// An `on-task-start` hook proposed context and the runtime decided what to do with it.
///
/// Written once per task that had a seeding hook return something, whatever the decision —
/// including a rejection, where `tokens` is `0`. A seed that was silently discarded is
/// indistinguishable from a capsule with no memory hook at all, which is the failure this
/// record exists to make impossible.
#[derive(Serialize)]
struct ContextSeedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    /// Manifest name of the hook that returned the `seed-context`.
    hook_name: String,
    /// Tokens actually committed to the head of the context: `0` for a rejection, the
    /// proposal's own count for `seeded`, and the surviving count for `trimmed`/`compacted`.
    tokens: u64,
    /// Tokens the hook proposed, before any trim or summarization. Together with
    /// `budget_tokens` this is what makes a rejection readable without the hook's own log.
    proposed_tokens: u64,
    /// The ceiling in force: `floor(context.max_tokens * context.seed_budget)`, or `0` when
    /// the capsule declares no `context.max_tokens`.
    budget_tokens: u64,
    /// `"seeded"`, `"trimmed"`, `"compacted"` or `"rejected"` — see the `SEED_OUTCOME_*`
    /// constants.
    outcome: String,
    /// Why nothing was committed, on `"rejected"` only — see the `SEED_REJECTED_*`
    /// constants. Absent from the JSONL for every other outcome.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The `msg_`-prefixed id of every message committed, in the order they were placed.
    /// Empty on a rejection. These are the ids the runtime minted; they never reach the
    /// driver payload, so this record is the only place they are visible.
    message_ids: Vec<String>,
}

/// Retention deleted something. One line per (store, reason) pair that removed anything, in the
/// trace of the session that performed the deletion — the only place the pruning of session N's
/// predecessors can go.
///
/// Session-scoped, and written immediately after `session_start`: a session directory that
/// vanishes with no explanation makes "where did my trace go" unanswerable, and the answer has to
/// be in the trace of the run that did it.
#[derive(Serialize)]
struct RetentionEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// [`RETENTION_STORE_SESSIONS`] or [`RETENTION_STORE_RECORDS`].
    store: String,
    /// [`RETENTION_REASON_MAX_SESSIONS`], [`RETENTION_REASON_MAX_AGE`] or
    /// [`RETENTION_REASON_MAX_MESSAGES`].
    reason: String,
    /// How many units this reason removed from this store: session directories, context
    /// directories, or — for a truncation — the one record that was rewritten. Never zero;
    /// nothing removed means no event.
    removed: u32,
    /// What went: `ses_` directory names, or context ids.
    targets: Vec<String>,
    /// Messages dropped from the front of the record. Written for
    /// [`RETENTION_REASON_MAX_MESSAGES`] only, and absent from the JSONL otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    messages_dropped: Option<u64>,
}

#[derive(Serialize)]
struct A2aTaskReceivedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    message_id: String,
    traceparent_from_caller: Option<String>,
}

#[derive(Serialize)]
struct A2aSendEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    peer_url: String,
    message_id: String,
    task_id: String,
    context_id: String,
    traceparent: Option<String>,
    /// The class this runtime stamped on the outgoing request, so a chain of capsules reads the
    /// same way from the sending end as the receiving end's `task_start` reads it.
    trust: String,
}

#[derive(Serialize)]
struct SessionEndEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    total_turns: u32,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    duration_ms: u64,
    exit_status: String,
}

#[derive(Serialize)]
struct TaskStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    /// Which door the task came through. `source` and `origin` answer different questions —
    /// `"a2a"` covers both a trusted peer and an untyped webhook.
    source: String,
    origin: String,
    trust: String,
    /// The queue lane this task waited in, derived from `origin`. Recorded so the order two
    /// tasks ran in is answerable from the trace alone.
    lane: String,
    /// The delegation whose completion this task is, for a `completion`-origin task that arrived
    /// from a child this capsule launched. Omitted from the record entirely for every other task:
    /// the field is absent, not null.
    #[serde(skip_serializing_if = "Option::is_none")]
    delegation_id: Option<String>,
    message_parts_bytes: u64,
}

#[derive(Serialize)]
struct TaskEndEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    exit_status: String,
    duration_ms: u64,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    tool_calls: u32,
    shell_calls: u32,
    /// How many times this task's agent loop was reopened by an `on-task-end` hook
    /// before the terminal outcome. `0` for a task that ran once (the common case).
    reopen_count: u32,
}

/// One `on-task-end` hook reopened the task: its agent loop is about to re-run with
/// the hook's feedback injected. Written between two agent-loop attempts for the same
/// task, so `mur trace show` can show which hook drove each reopen and why.
#[derive(Serialize)]
struct TaskReopenedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    /// Manifest name of the `on-task-end` hook that returned `reopen-task`.
    hook_name: String,
    /// Feedback text the hook asked to inject into the reopened task content.
    reason: String,
    /// 1-based ordinal of this reopen within the task (first reopen = 1).
    reopen_number: u32,
}

/// A policy hook refused a call before it ran. The one record that says a call the model asked
/// for never happened — there is no `tool_call` or `shell` line for a denied call, because
/// nothing ran.
#[derive(Serialize)]
struct CallDeniedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    /// The gated lifecycle function whose decision point refused: `"on-shell"` or
    /// `"on-tool-call"`.
    event: String,
    /// Manifest name of the policy hook that refused.
    hook_name: String,
    /// What was refused: the resolved executable path for a shell call, the tool name
    /// otherwise.
    target: String,
    /// The hook's own reason, or the runtime's when the hook supplied none it could use.
    reason: String,
}

#[derive(Serialize)]
struct HookDispatchErrorEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    hook_name: String,
    event: String,
    arm: String,
}

#[derive(Serialize)]
struct ResourceListEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// `exports.files.root` verbatim, or `""` when the capsule declares no export — a refusal on
    /// an undeclared plane is still a complete record of what was asked for.
    root: String,
    entry_count: usize,
    total_bytes: u64,
    /// Which completed turn the listing is as of.
    generation: u64,
    containment_achieved: ContainmentClass,
    /// `"ok"`, or the wire error code the request was refused with.
    outcome: String,
    /// `null` on `ok`, one short sentence otherwise.
    reason: Option<String>,
}

#[derive(Serialize)]
struct ResourceReadEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The requested path *after* percent-decoding, so `%2e%2e%2f` and `../` read as the same
    /// attempt rather than as two unrelated findings.
    path: String,
    outcome: String,
    /// `null` on every non-`ok` outcome.
    bytes: Option<u64>,
    /// `null` on every non-`ok` outcome.
    sha256: Option<String>,
    generation: u64,
    containment_achieved: ContainmentClass,
    reason: Option<String>,
}

/// One `share-file` call, served or refused, written at the moment of the mint.
#[derive(Serialize)]
struct PeerHandleMintEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// `null` on every non-`ok` outcome: a mint that was refused produced no token, so there is
    /// no handle to identify.
    handle_id: Option<String>,
    /// The path relative to `exports.peer_files.root`, canonicalised, or the path the agent asked
    /// for when the mint was refused. Never a host path and never the workdir-relative form.
    path: String,
    audience: String,
    /// `null` on every non-`ok` outcome.
    expires_at_ms: Option<u64>,
    outcome: String,
    reason: Option<String>,
}

/// One redeem against `/resources/peer/<handle>`, written by the listener concurrently with a
/// possibly-running task.
#[derive(Serialize)]
struct PeerHandleRedeemEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    handle_id: String,
    /// `null` until the MAC has verified. A payload that failed the MAC is caller-controlled and
    /// must not be copied into this capsule's own audit record as if it were fact.
    path: Option<String>,
    /// The runtime's own current counter, never a value taken from the token — so it is always
    /// present and always true.
    generation: u64,
    /// The `x-murmur-audience` header exactly as asserted, or `null` when none was sent.
    audience_asserted: Option<String>,
    /// `null` on every non-`ok` outcome.
    bytes: Option<u64>,
    /// `null` on every non-`ok` outcome.
    sha256: Option<String>,
    outcome: String,
    reason: Option<String>,
}

/// One `fetch-peer-file` call on the ingesting side, served or refused.
#[derive(Serialize)]
struct PeerFileFetchEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    peer: String,
    handle_id: String,
    /// Where the bytes landed, relative to the accessible workdir. `null` on every non-`ok`
    /// outcome.
    stored_path: Option<String>,
    bytes: Option<u64>,
    sha256: Option<String>,
    outcome: String,
    reason: Option<String>,
}

/// One `delegate-task` call, written when the delegation ends.
///
/// One line per call whatever happened, including a call the daemon refused: a delegation that was
/// asked for and not made is as much a fact of the run as one that produced an answer. Carries
/// neither the task text nor the child's answer — both are the agent's own conversation, which the
/// `tool_call` line for the same call already records under the session's `trace.capture` setting.
#[derive(Serialize)]
struct DelegationEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The sub-capsule the agent named, and the exact version it named.
    capsule: String,
    version: String,
    /// The `dlg_` id naming this delegation. `null` on a `refused` outcome, which made none.
    delegation_id: Option<String>,
    /// The child's own session id, so its trace is findable. `null` when no child ran.
    child_session_id: Option<String>,
    /// Wall-clock time from the first request to the daemon until the delegation ended.
    duration_ms: u64,
    /// `completed`, `failed`, `timed_out` or `refused` — `DelegationStatus::as_str`.
    outcome: String,
    /// `null` on `completed`. On every other outcome, the same sentence the model was given.
    reason: Option<String>,
}

// ── TraceWriter impl ─────────────────────────────────────────────────────────

impl TraceWriter {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        workdir: &Path,
        session_id: String,
        capsule_name: String,
        capsule_version: String,
        model: String,
        capabilities: Vec<String>,
        effective_grants: ScopeReport,
        capture: TraceCapture,
        system_prompt: Option<String>,
        system_prompt_overridden: bool,
        resumed_from: Option<String>,
        context_id: Option<String>,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(workdir.join("trace.jsonl"))
            .await?;
        // `"cli"` wins over `"none"` when the override cleared the prompt: the operator passed
        // `--system-prompt ""`, which is a decision worth recording, not an absence.
        let system_prompt_source = match (system_prompt_overridden, system_prompt.is_some()) {
            (true, _) => "cli",
            (false, true) => "manifest",
            (false, false) => "none",
        };
        let system_prompt_sha256 = system_prompt
            .as_ref()
            .map(|prompt| murmur_artifact::sha256_hex(prompt.as_bytes()));
        let blobs = BlobStore::new(workdir);
        // The prompt the operator wrote, kept under the hash `session_start` already carried.
        // Written here rather than at `session_start` because a session writes that frame once
        // per task and the blob is the same one every time.
        if let (true, Some(prompt), Some(sha)) = (
            capture.captures_content(),
            system_prompt.as_ref(),
            system_prompt_sha256.as_deref(),
        ) {
            blobs.put(sha, prompt.as_bytes()).await?;
        }
        Ok(Self {
            writer: BufWriter::new(file),
            session_id,
            capsule_name,
            capsule_version,
            model,
            capabilities,
            effective_grants,
            capture,
            blobs,
            system_prompt_source,
            system_prompt_sha256,
            resumed_from,
            context_id,
            session_start_time: Instant::now(),
            session_event_id: new_event_id(),
            session_started: false,
            session_ended: false,
            task_event_id: None,
            turn_event_id: None,
            total_turns: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tool_calls: 0,
            total_shell_calls: 0,
            task_turns: 0,
            task_input_tokens: 0,
            task_output_tokens: 0,
            task_tool_calls: 0,
            task_shell_calls: 0,
            task_start_instant: None,
            active_task_id: None,
        })
    }

    /// The session node of this writer's event tree — the `event_id` its `session_start` will
    /// carry, available before that line is written so [`ResourceTraceAppender`] can be opened
    /// against the same node.
    pub(crate) fn session_event_id(&self) -> &str {
        &self.session_event_id
    }

    /// Whether this session's `inference` records carry content hashes — everything but
    /// `trace.capture: none`. The agent loop asks before splitting a payload into a
    /// [`WireCapture`], so `none` costs no serialization at all.
    pub(crate) fn captures_wire(&self) -> bool {
        self.capture.captures_hashes()
    }

    /// Parent for a launch-scoped event: the session node, or `None` on a writer that has not
    /// written `session_start`. A trace line must never name a parent that has no line behind
    /// it, and the script-capsule `a2a_send` drain opens a writer over a file with no session
    /// frame at all.
    fn session_parent(&self) -> Option<String> {
        self.session_started.then(|| self.session_event_id.clone())
    }

    /// Parent for a task-scoped event: the task node, falling back to the session node when no
    /// task is active.
    fn task_parent(&self) -> Option<String> {
        self.task_event_id.clone().or_else(|| self.session_parent())
    }

    /// Parent for a turn-scoped event: the turn node, falling back to the task node and then to
    /// the session node. The fallbacks are what keeps a tool call written outside any turn — a
    /// hook's, or the process transport's, before its first `inference` — attached to the tree.
    fn turn_parent(&self) -> Option<String> {
        self.turn_event_id.clone().or_else(|| self.task_parent())
    }

    pub(crate) async fn write_session_start(
        &mut self,
        max_turns: u32,
        tools_declared: Vec<String>,
    ) -> std::io::Result<()> {
        let event = SessionStartEvent {
            event_type: "session_start",
            event_id: self.session_event_id.clone(),
            parent_id: None,
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            capsule_name: self.capsule_name.clone(),
            capsule_version: self.capsule_version.clone(),
            model: self.model.clone(),
            max_turns,
            capabilities: self.capabilities.clone(),
            tools_declared,
            containment_declared: self.effective_grants.declared_containment,
            containment_achieved: self.effective_grants.achieved_containment,
            workdir_exec: self.effective_grants.workdir_exec,
            userns_grant: self.effective_grants.userns_grant,
            effective_grants: self.effective_grants.clone(),
            system_prompt_source: self.system_prompt_source,
            system_prompt_sha256: self.system_prompt_sha256.clone(),
            resumed_from: self.resumed_from.clone(),
            context_id: self.context_id.clone(),
        };
        self.write_event(&event).await?;
        self.session_started = true;
        Ok(())
    }

    /// `input_tokens`/`output_tokens` are the runtime's tiktoken estimates and are what the
    /// session and task totals accumulate; `usage` is the provider's own report for the same
    /// call, written verbatim beside them and accumulated into nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_inference(
        &mut self,
        turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        decision: String,
        tool_name: Option<String>,
        origin: Option<&InferenceOrigin>,
        usage: Option<&DriverUsage>,
        message_ids: Vec<String>,
        wire: Option<&WireCapture>,
    ) -> std::io::Result<()> {
        let event_id = new_event_id();
        // The agent loop's own inference *is* the turn node — there is no separate turn line —
        // so it hangs off the task and everything the turn goes on to produce hangs off it. A
        // hook's `run-inference` (`origin: Some(_)`) is one of those products, not a new turn.
        let parent_id = if origin.is_none() {
            let parent = self.task_parent();
            self.turn_event_id = Some(event_id.clone());
            parent
        } else {
            self.turn_parent()
        };
        let (system_sha, tools_sha, response_sha, message_shas) =
            match wire.filter(|_| self.capture.captures_hashes()) {
                Some(wire) => self.record_wire(wire).await?,
                None => (None, None, None, Vec::new()),
            };
        let event = InferenceEvent {
            event_type: "inference",
            event_id,
            parent_id,
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            input_tokens,
            output_tokens,
            decision,
            tool_name,
            input_tokens_actual: usage.and_then(|u| u.input_tokens),
            output_tokens_actual: usage.and_then(|u| u.output_tokens),
            cached_tokens: usage.and_then(|u| u.cached_tokens),
            cache_write_tokens: usage.and_then(|u| u.cache_write_tokens),
            origin: origin.map(|o| o.source.clone()),
            model: origin.map(|o| o.model.clone()),
            message_ids,
            system_sha,
            tools_sha,
            response_sha,
            message_shas,
        };
        self.write_event(&event).await?;
        self.total_turns = self.total_turns.saturating_add(1);
        self.total_input_tokens = self.total_input_tokens.saturating_add(input_tokens);
        self.total_output_tokens = self.total_output_tokens.saturating_add(output_tokens);
        self.task_turns = self.task_turns.saturating_add(1);
        self.task_input_tokens = self.task_input_tokens.saturating_add(input_tokens);
        self.task_output_tokens = self.task_output_tokens.saturating_add(output_tokens);
        Ok(())
    }

    /// Hash the four bodies of one request and, under [`TraceCapture::Content`], store each
    /// behind its hash.
    ///
    /// The hash is computed in every capture mode that reaches here; only the blob write is
    /// gated, so `meta` and `content` name the same bytes and differ solely in whether those
    /// bytes are also on disk.
    async fn record_wire(
        &mut self,
        wire: &WireCapture,
    ) -> std::io::Result<(Option<String>, Option<String>, Option<String>, Vec<String>)> {
        let system_sha = murmur_artifact::sha256_hex(&wire.system);
        let tools_sha = murmur_artifact::sha256_hex(&wire.tools);
        let response_sha = murmur_artifact::sha256_hex(&wire.response);
        let message_shas: Vec<String> = wire
            .messages
            .iter()
            .map(|message| murmur_artifact::sha256_hex(message))
            .collect();

        if self.capture.captures_content() {
            // Verbatim, unredacted. `write_tool_call` redacts peer handles out of its own summary
            // fields; a blob is not a summary, and redacting one would make its hash name bytes
            // no file holds — destroying the only property this store exists to provide.
            self.blobs.put(&system_sha, &wire.system).await?;
            self.blobs.put(&tools_sha, &wire.tools).await?;
            self.blobs.put(&response_sha, &wire.response).await?;
            for (sha, body) in message_shas.iter().zip(&wire.messages) {
                self.blobs.put(sha, body).await?;
            }
        }

        Ok((
            Some(system_sha),
            Some(tools_sha),
            Some(response_sha),
            message_shas,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_tool_call(
        &mut self,
        turn: u32,
        tool_name: String,
        tool_call_id: Option<String>,
        mut input: Value,
        input_bytes: u64,
        output: &str,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
        state_effect: Option<String>,
        resource_id: Option<String>,
    ) -> std::io::Result<()> {
        // A peer handle reaches this event by an ordinary route — it is an argument the model
        // passed to `fetch-peer-file`, and every call's arguments are recorded. It is also a
        // credential, and the trace is durable, so the two must not meet: the `handle_id` goes in
        // instead, which is what correlates the record with the mint and the redeem anyway.
        crate::peer_handoff::redact_handles_in_json(&mut input);
        let event = ToolCallEvent {
            event_type: "tool_call",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            tool_name,
            // An absent or empty provider id records as `null`: `""` would read as an id the
            // provider actually issued.
            tool_call_id: tool_call_id.filter(|id| !id.is_empty()),
            input,
            input_bytes,
            output: self
                .capture
                .captures_content()
                .then(|| crate::peer_handoff::redact_handle_tokens(output).into_owned()),
            output_bytes,
            duration_ms,
            status,
            state_effect,
            resource_id,
        };
        self.write_event(&event).await?;
        self.total_tool_calls = self.total_tool_calls.saturating_add(1);
        self.task_tool_calls = self.task_tool_calls.saturating_add(1);
        Ok(())
    }

    /// Records a skill invocation as a `skill_call` event. Skill calls are NOT counted in
    /// `total_tool_calls` — they appear in a separate `── Skill calls ──` section in `mur trace show`.
    pub(crate) async fn write_skill_call(
        &mut self,
        turn: u32,
        skill_name: String,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
    ) -> std::io::Result<()> {
        let event = SkillCallEvent {
            event_type: "skill_call",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            skill_name,
            output_bytes,
            duration_ms,
            status,
        };
        self.write_event(&event).await
        // Intentionally does not increment total_tool_calls or task_tool_calls.
    }

    // One positional parameter per JSONL column, as every other `write_*` here does; the
    // sibling `write_tool_call` carries the same allowance.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_shell(
        &mut self,
        turn: u32,
        binary: String,
        command: String,
        exit_code: i32,
        stdout_bytes: u64,
        stderr_bytes: u64,
        duration_ms: u64,
        resource_limit: Option<String>,
    ) -> std::io::Result<()> {
        let event = ShellEvent {
            event_type: "shell",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            binary,
            command,
            exit_code,
            stdout_bytes,
            stderr_bytes,
            duration_ms,
            resource_limit,
        };
        self.write_event(&event).await?;
        self.total_shell_calls = self.total_shell_calls.saturating_add(1);
        self.task_shell_calls = self.task_shell_calls.saturating_add(1);
        Ok(())
    }

    /// A command demoted to the background, written from the agent loop at the moment of
    /// demotion.
    ///
    /// Counts as this session's shell call, exactly as a foreground one does — the pair with
    /// [`Self::write_shell_completed`], which deliberately does not count, so `mur trace show`
    /// reports each shell command once whichever way it ran.
    pub(crate) async fn write_shell_detached(
        &mut self,
        turn: u32,
        work_id: &str,
        binary: &str,
        command: &str,
        grace_ms: u64,
    ) -> std::io::Result<()> {
        let event = ShellDetachedEvent {
            event_type: "shell_detached",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            work_id: work_id.to_string(),
            binary: binary.to_string(),
            command: command.to_string(),
            grace_ms,
        };
        self.write_event(&event).await?;
        self.total_shell_calls = self.total_shell_calls.saturating_add(1);
        self.task_shell_calls = self.task_shell_calls.saturating_add(1);
        Ok(())
    }

    /// A detached command finishing, written by the task loop as it turns the completion into a
    /// task. `completion_task_id` is that task's id, so the two records join.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_shell_completed(
        &mut self,
        work_id: &str,
        binary: &str,
        command: &str,
        exit_code: i32,
        duration_ms: u64,
        output_path: &str,
        output_bytes: u64,
        resource_limit: Option<String>,
        status: &str,
        completion_task_id: &str,
    ) -> std::io::Result<()> {
        let event = ShellCompletedEvent {
            event_type: "shell_completed",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            work_id: work_id.to_string(),
            binary: binary.to_string(),
            command: command.to_string(),
            exit_code,
            duration_ms,
            output_path: output_path.to_string(),
            output_bytes,
            resource_limit,
            status: status.to_string(),
            completion_task_id: completion_task_id.to_string(),
        };
        self.write_event(&event).await
    }

    /// A detached command the session ended without waiting for.
    pub(crate) async fn write_shell_abandoned(
        &mut self,
        work_id: &str,
        binary: &str,
        command: &str,
        running_ms: u64,
    ) -> std::io::Result<()> {
        let event = ShellAbandonedEvent {
            event_type: "shell_abandoned",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            work_id: work_id.to_string(),
            binary: binary.to_string(),
            command: command.to_string(),
            running_ms,
        };
        self.write_event(&event).await
    }

    /// A demotion the trace could not record, written where the failure was seen.
    pub(crate) async fn write_shell_detach_unrecorded(
        &mut self,
        turn: u32,
        work_id: &str,
        binary: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = ShellDetachUnrecordedEvent {
            event_type: "shell_detach_unrecorded",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            work_id: work_id.to_string(),
            binary: binary.to_string(),
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_a2a_task_received(
        &mut self,
        task_id: &str,
        context_id: &str,
        message_id: &str,
        traceparent_from_caller: Option<&str>,
    ) -> std::io::Result<()> {
        let event = A2aTaskReceivedEvent {
            event_type: "a2a_task_received",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            message_id: message_id.to_string(),
            traceparent_from_caller: traceparent_from_caller.map(str::to_string),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_a2a_send(
        &mut self,
        peer_url: &str,
        message_id: &str,
        task_id: &str,
        context_id: &str,
        traceparent: Option<&str>,
        trust: TrustClass,
    ) -> std::io::Result<()> {
        let event = A2aSendEvent {
            event_type: "a2a_send",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            peer_url: peer_url.to_string(),
            message_id: message_id.to_string(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            traceparent: traceparent.map(str::to_string),
            trust: trust.as_str().to_string(),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_start(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
        provenance: TaskProvenance,
        delegation_id: Option<&str>,
        message_parts_bytes: u64,
    ) -> std::io::Result<()> {
        // Reset per-task counters for this task
        self.task_turns = 0;
        self.task_input_tokens = 0;
        self.task_output_tokens = 0;
        self.task_tool_calls = 0;
        self.task_shell_calls = 0;
        self.task_start_instant = Some(Instant::now());
        self.active_task_id = Some(task_id.to_string());

        let event_id = new_event_id();
        self.task_event_id = Some(event_id.clone());
        self.turn_event_id = None;

        let event = TaskStartEvent {
            event_type: "task_start",
            event_id: event_id.clone(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            source: source.to_string(),
            origin: provenance.origin().as_str().to_string(),
            trust: provenance.trust().as_str().to_string(),
            // One rule for every call site, so the recorded lane cannot disagree with the lane
            // `LaneQueue` filed the task under — both read the same provenance.
            lane: TaskLane::for_origin(provenance.origin())
                .as_str()
                .to_string(),
            delegation_id: delegation_id.map(str::to_string),
            message_parts_bytes,
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_end(
        &mut self,
        task_id: &str,
        exit_status: &str,
        reopen_count: u32,
    ) -> std::io::Result<()> {
        let duration_ms = self
            .task_start_instant
            .take()
            .map(|t| t.elapsed().as_millis().try_into().unwrap_or(u64::MAX))
            .unwrap_or(0);

        let event = TaskEndEvent {
            event_type: "task_end",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            exit_status: exit_status.to_string(),
            duration_ms,
            turns: self.task_turns,
            input_tokens: self.task_input_tokens,
            output_tokens: self.task_output_tokens,
            tool_calls: self.task_tool_calls,
            shell_calls: self.task_shell_calls,
            reopen_count,
        };
        self.active_task_id = None;
        self.task_event_id = None;
        self.turn_event_id = None;
        self.write_event(&event).await
    }

    /// Record that an `on-task-end` hook reopened the task. Written by the runtime's
    /// per-task reopen loop between two agent-loop attempts, once per reopen, before
    /// the terminal `task_end` record. `reopen_number` is 1-based.
    pub(crate) async fn write_task_reopened(
        &mut self,
        task_id: &str,
        hook_name: &str,
        reason: &str,
        reopen_number: u32,
    ) -> std::io::Result<()> {
        let event = TaskReopenedEvent {
            event_type: "task_reopened",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            hook_name: hook_name.to_string(),
            reason: reason.to_string(),
            reopen_number,
        };
        self.write_event(&event).await
    }

    /// Cumulative turns consumed by the active task so far, across however many
    /// agent-loop attempts have run since the last [`Self::write_task_start`]. Read by
    /// the reopen loop to compute the remaining `max_turns` budget for the next attempt
    /// so reopening never grants turns past the capsule's ceiling.
    pub(crate) fn task_turns(&self) -> u32 {
        self.task_turns
    }

    pub(crate) async fn write_compaction(
        &mut self,
        turn: u32,
        tokens_before: u64,
        tokens_after: u64,
    ) -> std::io::Result<()> {
        let event = CompactionEvent {
            event_type: "compaction",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            tokens_before,
            tokens_after,
        };
        self.write_event(&event).await
    }

    /// Record that compaction was attempted and declined, leaving the context over budget.
    /// `tokens` is the occupancy at the moment of the decline; `reason` is
    /// [`COMPACTION_DECLINED_NO_HOOK_REPLACEMENT`] or
    /// [`COMPACTION_DECLINED_UNRESOLVED_TOOL_CALL`].
    pub(crate) async fn write_compaction_declined(
        &mut self,
        turn: u32,
        tokens: u64,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = CompactionDeclinedEvent {
            event_type: "compaction_declined",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            tokens,
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record what the runtime did with an `on-task-start` hook's `seed-context`.
    ///
    /// Written for every outcome, committed or not: `tokens` is what reached the context
    /// (`0` on a rejection), `proposed_tokens` is what the hook returned, and
    /// `budget_tokens` is the ceiling it was measured against. `reason` is `Some` only for
    /// [`SEED_OUTCOME_REJECTED`]. Task-scoped, like `task_start`/`task_end`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_context_seed(
        &mut self,
        hook_name: &str,
        tokens: u64,
        proposed_tokens: u64,
        budget_tokens: u64,
        outcome: &str,
        reason: Option<&str>,
        message_ids: Vec<String>,
    ) -> std::io::Result<()> {
        let event = ContextSeedEvent {
            event_type: "context_seed",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            hook_name: hook_name.to_string(),
            tokens,
            proposed_tokens,
            budget_tokens,
            outcome: outcome.to_string(),
            reason: reason.map(str::to_string),
            message_ids,
        };
        self.write_event(&event).await
    }

    /// Record one store's deletion, naming the reason and everything that went.
    ///
    /// Session-scoped: `parent_id` is the session node, so the deletion hangs off the launch that
    /// performed it rather than off whatever task happened to be open. `messages_dropped` is
    /// `Some` only for [`RETENTION_REASON_MAX_MESSAGES`].
    pub(crate) async fn write_retention(
        &mut self,
        store: &str,
        reason: &str,
        targets: Vec<String>,
        messages_dropped: Option<u64>,
    ) -> std::io::Result<()> {
        let event = RetentionEvent {
            event_type: "retention",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            store: store.to_string(),
            reason: reason.to_string(),
            removed: u32::try_from(targets.len()).unwrap_or(u32::MAX),
            targets,
            messages_dropped,
        };
        self.write_event(&event).await
    }

    /// Record that a policy hook refused a call before it ran. The only account of a call the
    /// model asked for and never got: nothing ran, so no `tool_call` or `shell` record
    /// accompanies it.
    pub(crate) async fn write_call_denied(
        &mut self,
        turn: u32,
        event: &str,
        hook_name: &str,
        target: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = CallDeniedEvent {
            event_type: "call_denied",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            event: event.to_string(),
            hook_name: hook_name.to_string(),
            target: target.to_string(),
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record that a hook returned a `hook-output` arm the lifecycle event does not
    /// honor. Written by the agent loop from a buffered [`crate::hooks::DispatchFault`]
    /// drained just before `session_end`. Non-fatal: the session already continued as
    /// if the hook had returned `none`; this only makes the discard visible to
    /// `mur trace show` and anything reading the session trace.
    pub(crate) async fn write_hook_dispatch_error(
        &mut self,
        hook_name: &str,
        event: &str,
        arm: &str,
    ) -> std::io::Result<()> {
        let event = HookDispatchErrorEvent {
            event_type: "hook_dispatch_error",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            hook_name: hook_name.to_string(),
            event: event.to_string(),
            arm: arm.to_string(),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_session_end(&mut self, exit_status: &str) -> std::io::Result<()> {
        let duration_ms = self
            .session_start_time
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let event = SessionEndEvent {
            event_type: "session_end",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            total_turns: self.total_turns,
            total_input_tokens: self.total_input_tokens,
            total_output_tokens: self.total_output_tokens,
            total_tool_calls: self.total_tool_calls,
            total_shell_calls: self.total_shell_calls,
            duration_ms,
            exit_status: exit_status.to_string(),
        };
        self.write_event(&event).await?;
        self.session_ended = true;
        Ok(())
    }

    /// Writes session_end with the accumulated totals if session_start was written but
    /// session_end was not (error exit paths that bypass the normal Ok() return sites).
    pub(crate) async fn write_session_end_if_not_ended(
        &mut self,
        exit_status: &str,
    ) -> std::io::Result<()> {
        if self.session_started && !self.session_ended {
            self.write_session_end(exit_status).await?;
        }
        Ok(())
    }

    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush().await
    }

    /// Writes one event as one line, and flushes.
    ///
    /// The flush is not an optimisation choice: [`ResourceTraceAppender`] appends to this same
    /// `trace.jsonl` while a task runs, so a line left half-buffered here would be split around
    /// whatever the appender wrote in between and the file would stop parsing as JSONL. The
    /// two writers agree on one rule — a complete line per write, then flush — and that rule is
    /// what makes concurrent reads during a running task safe to record.
    async fn write_event(&mut self, event: &impl Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await
    }
}

// ── Resource-plane appender ───────────────────────────────────────────────────

/// A second, independent `O_APPEND` handle to the session's `trace.jsonl`, used by the resource
/// plane, by the peer plane's listener, and by the peer-handoff tools in the agent loop.
///
/// [`TraceWriter`] is `&mut`-owned by the agent loop and cannot be shared, and the motivating
/// case for a resource-plane event — a gateway reading a finished-but-alive capsule — happens
/// after `session_end` has already been written. So the plane gets its own handle rather than a
/// borrow of the loop's, and every event is written at the moment of the request rather than
/// deferred to a task boundary: a denied read that only surfaced at the next task end would be a
/// record of the wrong thing at the wrong time.
///
/// Interleaving is safe because both writers emit exactly one complete line per `write_all` and
/// flush it, and `O_APPEND` makes each such write atomic against the other's.
pub struct ResourceTraceAppender {
    /// Async rather than a `std::sync::Mutex`: this is held across an `await` on the write.
    file: tokio::sync::Mutex<File>,
    session_id: String,
    /// The session node every line this appender writes hangs off — [`TraceWriter`]'s own
    /// [`TraceWriter::session_event_id`], handed over at open time. The plane has no turn or
    /// task of its own: a read served after `session_end` belongs to the launch, not to
    /// whatever task happened to run last.
    session_event_id: String,
}

impl ResourceTraceAppender {
    pub async fn open(
        workdir: &Path,
        session_id: String,
        session_event_id: String,
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(workdir.join("trace.jsonl"))
            .await?;
        Ok(Self {
            file: tokio::sync::Mutex::new(file),
            session_id,
            session_event_id,
        })
    }

    /// Records one `list`. Failures are swallowed: a trace that cannot be written must not turn
    /// a served read into an error, and the alternative — refusing the read — would let anyone
    /// who can fill the disk take the plane down.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_resource_list(
        &self,
        root: &str,
        entry_count: usize,
        total_bytes: u64,
        generation: u64,
        containment_achieved: ContainmentClass,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = ResourceListEvent {
            event_type: "resource_list",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            root: root.to_string(),
            entry_count,
            total_bytes,
            generation,
            containment_achieved,
            outcome: outcome.to_string(),
            reason,
        };
        self.append(&event).await;
    }

    /// Records one `read`, served or refused. `bytes` and `sha256` are `None` on every
    /// non-`ok` outcome — a refusal must not carry a hash of something it did not serve.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_resource_read(
        &self,
        path: &str,
        outcome: &str,
        bytes: Option<u64>,
        sha256: Option<String>,
        generation: u64,
        containment_achieved: ContainmentClass,
        reason: Option<String>,
    ) {
        let event = ResourceReadEvent {
            event_type: "resource_read",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            path: path.to_string(),
            outcome: outcome.to_string(),
            bytes,
            sha256,
            generation,
            containment_achieved,
            reason,
        };
        self.append(&event).await;
    }

    /// Records one mint. `handle_id` and `expires_at_ms` are `None` on every non-`ok` outcome —
    /// a refused mint produced no token, and an audit record must not imply one exists.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_peer_handle_mint(
        &self,
        handle_id: Option<String>,
        path: &str,
        audience: &str,
        expires_at_ms: Option<u64>,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = PeerHandleMintEvent {
            event_type: "peer_handle_mint",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            handle_id,
            path: path.to_string(),
            audience: audience.to_string(),
            expires_at_ms,
            outcome: outcome.to_string(),
            reason,
        };
        self.append(&event).await;
    }

    /// Records one redeem, served or refused. The token itself is never written — only its
    /// `handle_id`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_peer_handle_redeem(
        &self,
        handle_id: &str,
        path: Option<String>,
        generation: u64,
        audience_asserted: Option<String>,
        bytes: Option<u64>,
        sha256: Option<String>,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = PeerHandleRedeemEvent {
            event_type: "peer_handle_redeem",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            handle_id: handle_id.to_string(),
            path,
            generation,
            audience_asserted,
            bytes,
            sha256,
            outcome: outcome.to_string(),
            reason,
        };
        self.append(&event).await;
    }

    /// Records one fetch on the ingesting side, served or refused.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_peer_file_fetch(
        &self,
        peer: &str,
        handle_id: &str,
        stored_path: Option<String>,
        bytes: Option<u64>,
        sha256: Option<String>,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = PeerFileFetchEvent {
            event_type: "peer_file_fetch",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            peer: peer.to_string(),
            handle_id: handle_id.to_string(),
            stored_path,
            bytes,
            sha256,
            outcome: outcome.to_string(),
            reason,
        };
        self.append(&event).await;
    }

    /// Records one delegation. `delegation_id` and `child_session_id` are `None` when no child was
    /// launched — a refused delegation names no id, and an audit record must not imply one exists.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_delegation(
        &self,
        capsule: &str,
        version: &str,
        delegation_id: Option<String>,
        child_session_id: Option<String>,
        duration_ms: u64,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = DelegationEvent {
            event_type: "delegation",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            capsule: capsule.to_string(),
            version: version.to_string(),
            delegation_id,
            child_session_id,
            duration_ms,
            outcome: outcome.to_string(),
            reason,
        };
        self.append(&event).await;
    }

    async fn append(&self, event: &impl Serialize) {
        let Ok(mut line) = serde_json::to_string(event) else {
            return;
        };
        line.push('\n');
        let mut file = self.file.lock().await;
        if file.write_all(line.as_bytes()).await.is_ok() {
            let _ = file.flush().await;
        }
    }
}

// -- Prior-session appender ---------------------------------------------------

/// An `O_APPEND` handle to a *previous* session's `trace.jsonl`, opened by a resuming launch to
/// account for demoted commands that session left unreported.
///
/// The marker goes in the file that holds the unmatched `shell_detached` line rather than in the
/// resuming session's own trace, which keeps clearing O(1) instead of a scan of every sibling
/// session, and means [`crate::retention::prune_sessions`] removes a marker and the work it
/// accounts for together.
///
/// Writing to another session's trace is safe because that session's writer is dead — that is the
/// precondition for there being anything unaccounted — and because this obeys the same rule
/// [`ResourceTraceAppender`] does: one complete line per `write_all`, then flush, which `O_APPEND`
/// makes atomic against any other writer.
pub(crate) struct PriorSessionTraceAppender {
    file: File,
    /// The prior session's id, which is what its own lines carry.
    session_id: String,
    /// That session's `session_start` node, read back from the same file.
    session_event_id: Option<String>,
    /// Whether the file's last record is unterminated, which a writer killed mid-`write_all`
    /// leaves behind. The first appended line opens with a newline when it is, so the marker is
    /// a line of its own rather than spliced onto the torn one.
    terminate_torn_tail: bool,
}

impl PriorSessionTraceAppender {
    /// `trace_path` is the prior session's `trace.jsonl`. Errors when it cannot be opened for
    /// appending, which the caller reports rather than reporting work it could not mark.
    pub(crate) async fn open(
        trace_path: &Path,
        session_id: String,
        session_event_id: Option<String>,
    ) -> std::io::Result<Self> {
        let terminate_torn_tail = !ends_with_newline(trace_path)?;
        let file = OpenOptions::new().append(true).open(trace_path).await?;
        Ok(Self {
            file,
            session_id,
            session_event_id,
            terminate_torn_tail,
        })
    }

    /// Records one demoted command as lost, and reports whether the line landed.
    ///
    /// The result is not swallowed: a work id whose marker did not land must be dropped from the
    /// report rather than reported unmarked, because an unmarked work id is reported again by
    /// every later resume of the same session.
    pub(crate) async fn write_shell_lost(
        &mut self,
        work: &crate::detached::LostWork,
        reconciled_by_session: &str,
        reconciled_task_id: &str,
    ) -> std::io::Result<()> {
        let event = ShellLostEvent {
            event_type: "shell_lost",
            event_id: new_event_id(),
            parent_id: self.session_event_id.clone(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            work_id: work.work_id.clone(),
            binary: work.binary.clone(),
            command: work.command.clone(),
            detached_at_ms: work.detached_at_ms,
            reconciled_by_session: reconciled_by_session.to_string(),
            reconciled_task_id: reconciled_task_id.to_string(),
        };
        let mut line = if self.terminate_torn_tail {
            "\n".to_string()
        } else {
            String::new()
        };
        line.push_str(&serde_json::to_string(&event).map_err(std::io::Error::other)?);
        line.push('\n');
        self.file.write_all(line.as_bytes()).await?;
        self.file.flush().await?;
        self.terminate_torn_tail = false;
        Ok(())
    }
}

/// Whether the file's last byte is a newline. An empty file counts as terminated: there is no
/// record for an appended line to run into.
fn ends_with_newline(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

pub(crate) fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        containment::scope_report_for_tier, sandbox::EnforcementTier, sealed::UsernsGrant,
        trace_blobs::BLOB_DIR_NAME, types::CapabilityPolicy,
    };
    use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant};
    use serde_json::Value;

    use crate::origin::TaskOrigin;

    /// The class an A2A task carrying no origin claim resolves to.
    fn event_provenance() -> TaskProvenance {
        TaskProvenance::derive(TaskOrigin::Event, None)
    }

    /// A [`ScopeReport`] built the way production builds one — through
    /// `containment::scope_report_for_tier` with an *explicit* tier, never a live probe, so these
    /// tests assert the same values on a Landlock-capable host and a laptop that has none.
    fn report_for(
        declared: ContainmentClass,
        tier: EnforcementTier,
        workdir_exec: bool,
    ) -> ScopeReport {
        scope_report_for_tier(
            &CapabilityPolicy {
                workdir_exec_allowed: workdir_exec,
                ..CapabilityPolicy::default()
            },
            declared,
            tier,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
        )
    }

    fn demotion_info() -> crate::detached::DetachedDispatchInfo {
        crate::detached::DetachedDispatchInfo {
            work_id: "wrk_0a1b2c3d4e5f6a7b".to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "make -j8".to_string(),
            grace_ms: 1_000,
        }
    }

    /// A demotion whose marker cannot be written stands anyway: the write failure is reported and
    /// swallowed, and the session carries on with the command running in the background.
    #[tokio::test]
    async fn a_demotion_marker_that_cannot_be_written_does_not_fail_the_demotion() {
        let dir = tempfile::tempdir().unwrap();
        // `/dev/full` opens and fails every write with `ENOSPC`, which is the one way to get a
        // trace handle that is valid and unwritable without root.
        std::os::unix::fs::symlink("/dev/full", dir.path().join("trace.jsonl")).unwrap();
        let mut writer = make_writer(dir.path()).await;
        let info = demotion_info();

        assert!(
            writer
                .write_shell_detached(1, &info.work_id, &info.binary, &info.command, info.grace_ms)
                .await
                .is_err(),
            "the sink under this writer must be unwritable for the claim below to mean anything"
        );
        crate::agent::record_demotion(&mut writer, 1, &info).await;
    }

    /// The ordinary path writes one `shell_detached` and nothing else.
    #[tokio::test]
    async fn a_recorded_demotion_writes_only_its_own_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        crate::agent::record_demotion(&mut writer, 3, &demotion_info()).await;

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "shell_detached");
        assert_eq!(events[0]["work_id"], "wrk_0a1b2c3d4e5f6a7b");
        assert_eq!(events[0]["turn"], 3);
    }

    #[tokio::test]
    async fn an_unrecorded_demotion_names_the_work_id_and_the_write_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        writer
            .write_shell_detach_unrecorded(2, "wrk_0a1b", "/usr/bin/bash", "No space left")
            .await
            .unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "shell_detach_unrecorded");
        assert_eq!(events[0]["turn"], 2);
        assert_eq!(events[0]["work_id"], "wrk_0a1b");
        assert_eq!(events[0]["binary"], "/usr/bin/bash");
        assert_eq!(events[0]["reason"], "No space left");
    }

    async fn make_writer(dir: &std::path::Path) -> TraceWriter {
        make_writer_with_opts(dir, TraceCapture::Meta).await
    }

    async fn make_writer_with_opts(dir: &std::path::Path, capture: TraceCapture) -> TraceWriter {
        make_writer_with_prompt(dir, capture, None, false).await
    }

    async fn make_writer_with_prompt(
        dir: &std::path::Path,
        capture: TraceCapture,
        system_prompt: Option<&str>,
        system_prompt_overridden: bool,
    ) -> TraceWriter {
        TraceWriter::open(
            dir,
            "test-session-id".to_string(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
            "claude-test".to_string(),
            vec!["shell".to_string()],
            report_for(
                ContainmentClass::Advisory,
                EnforcementTier::EnvironmentOnly,
                false,
            ),
            capture,
            system_prompt.map(str::to_string),
            system_prompt_overridden,
            None,
            None,
        )
        .await
        .unwrap()
    }

    fn read_events(dir: &std::path::Path) -> Vec<Value> {
        let content = std::fs::read_to_string(dir.join("trace.jsonl")).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn session_start_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec!["bash".to_string()])
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e["event_type"], "session_start");
        assert_eq!(e["session_id"], "test-session-id");
        assert_eq!(e["capsule_name"], "test-capsule");
        assert_eq!(e["capsule_version"], "1.0.0");
        assert_eq!(e["model"], "claude-test");
        assert_eq!(e["max_turns"], 10);
        assert_eq!(e["capabilities"], serde_json::json!(["shell"]));
        assert_eq!(e["tools_declared"], serde_json::json!(["bash"]));
        assert_eq!(e["containment_declared"], "advisory");
        assert_eq!(e["containment_achieved"], "advisory");
        assert_eq!(e["workdir_exec"], false);
        assert!(e["timestamp"].as_u64().unwrap() > 0);
    }

    /// The three `system_prompt_source` values, each paired with the hash the same call derives.
    /// A manifest prompt and a CLI override carrying the *same* text differ only in the source
    /// field — which is the whole reason the override flag is threaded through separately rather
    /// than inferred from the resolved prompt.
    #[tokio::test]
    async fn session_start_records_system_prompt_source_and_hash() {
        let expected_sha = murmur_artifact::sha256_hex(b"Be terse.");

        for (prompt, overridden, source, sha) in [
            (Some("Be terse."), false, "manifest", Some(&expected_sha)),
            (Some("Be terse."), true, "cli", Some(&expected_sha)),
            (None, false, "none", None),
            // `--system-prompt ""` cleared the manifest's prompt: no prompt is in effect, but the
            // operator made that call, so the source is the CLI and not `"none"`.
            (None, true, "cli", None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut w =
                make_writer_with_prompt(dir.path(), TraceCapture::Meta, prompt, overridden).await;
            w.write_session_start(1, Vec::new()).await.unwrap();
            w.flush().await.unwrap();

            let e = &read_events(dir.path())[0];
            assert_eq!(e["system_prompt_source"], source, "prompt={prompt:?}");
            match sha {
                Some(sha) => assert_eq!(e["system_prompt_sha256"], sha.as_str()),
                None => assert!(
                    e["system_prompt_sha256"].is_null(),
                    "no prompt in effect must hash to null, got {}",
                    e["system_prompt_sha256"]
                ),
            }
        }
    }

    /// `session_start` never carries the prompt text. The hash is written in every capture
    /// mode; under `content` the bytes behind it are a blob named by that same hash, which is
    /// how a reader gets from the record to the prompt.
    #[tokio::test]
    async fn session_start_records_the_prompt_by_hash_and_stores_it_only_under_content() {
        let sha = murmur_artifact::sha256_hex(b"Be terse.");

        let dir = tempfile::tempdir().unwrap();
        let mut w =
            make_writer_with_prompt(dir.path(), TraceCapture::Meta, Some("Be terse."), false).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        assert!(
            e.get("system_prompt").is_none(),
            "the verbatim prompt is not a session_start field, got {e}"
        );
        assert_eq!(e["system_prompt_sha256"], sha.as_str());
        assert!(
            !dir.path().join(BLOB_DIR_NAME).exists(),
            "meta stores no bodies"
        );

        let opted_in = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(
            opted_in.path(),
            TraceCapture::Content,
            Some("Be terse."),
            false,
        )
        .await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let event = &read_events(opted_in.path())[0];
        assert!(event.get("system_prompt").is_none());
        assert_eq!(event["system_prompt_sha256"], sha.as_str());
        let blob = opted_in.path().join(BLOB_DIR_NAME).join(&sha);
        assert_eq!(std::fs::read(&blob).unwrap(), b"Be terse.");
    }

    /// Opting in cannot conjure a prompt that was never in effect — and a session with no
    /// prompt and nothing else to store leaves no blob directory at all.
    #[tokio::test]
    async fn content_capture_stores_no_prompt_blob_when_there_is_no_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(dir.path(), TraceCapture::Content, None, false).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        assert!(e.get("system_prompt").is_none());
        assert!(e["system_prompt_sha256"].is_null());
        assert!(!dir.path().join(BLOB_DIR_NAME).exists());
    }

    /// The prompt is a session constant, like `model` — every task's `session_start` repeats it.
    #[tokio::test]
    async fn every_task_session_start_repeats_the_same_system_prompt_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut w =
            make_writer_with_prompt(dir.path(), TraceCapture::Meta, Some("Be terse."), true).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0]["system_prompt_source"],
            events[1]["system_prompt_source"]
        );
        assert_eq!(
            events[0]["system_prompt_sha256"],
            events[1]["system_prompt_sha256"]
        );
        assert_eq!(events[1]["system_prompt_source"], "cli");
    }

    /// The pairing an operator reads a trace for: `workdir_exec: true` next to the `advisory` it
    /// forces, on a session whose *declared* floor was higher. Without the flag on the record, a
    /// trace showing `achieved: advisory` is indistinguishable from one written on a host with no
    /// Landlock at all.
    #[tokio::test]
    async fn session_start_records_workdir_exec_next_to_the_class_it_forced() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TraceWriter::open(
            dir.path(),
            "test-session-id".to_string(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
            "claude-test".to_string(),
            Vec::new(),
            // A host that could back `scoped`, capped to `advisory` by the declaration alone.
            report_for(
                ContainmentClass::Advisory,
                EnforcementTier::KernelFull,
                true,
            ),
            TraceCapture::Meta,
            None,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events[0]["workdir_exec"], true);
        assert_eq!(events[0]["containment_achieved"], "advisory");
    }

    /// A declared floor and an achieved class are recorded independently — the trace shows
    /// what was asked for next to what the host actually gave, not one derived from the other.
    #[tokio::test]
    async fn session_start_records_declared_and_achieved_containment_separately() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = TraceWriter::open(
            dir.path(),
            "test-session-id".to_string(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
            "claude-test".to_string(),
            Vec::new(),
            report_for(ContainmentClass::Sealed, EnforcementTier::KernelFull, false),
            TraceCapture::Meta,
            None,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events[0]["containment_declared"], "sealed");
        assert_eq!(events[0]["containment_achieved"], "scoped");
    }

    /// The audit property the containment feature exists to provide: two sessions that reached
    /// the *same* achieved class through *different* host permissions must not produce the same
    /// record. A `sealed` result obtained through the shipped AppArmor profile and one obtained on
    /// a host whose unprivileged-userns hardening is switched off for every binary differ in
    /// nothing else the event carries, so `userns_grant` is what keeps the two apart.
    ///
    /// The tier is a literal on both sides, so the only thing that differs is the grant.
    #[tokio::test]
    async fn session_start_distinguishes_two_hosts_with_the_same_achieved_class() {
        async fn event_for(grant: UsernsGrant) -> Value {
            let report = scope_report_for_tier(
                &CapabilityPolicy::default(),
                ContainmentClass::Sealed,
                EnforcementTier::KernelSealed,
                None,
                Some(grant),
                None,
                Vec::new(),
                Vec::new(),
            );
            let dir = tempfile::tempdir().unwrap();
            let mut w = TraceWriter::open(
                dir.path(),
                "test-session-id".to_string(),
                "test-capsule".to_string(),
                "1.0.0".to_string(),
                "claude-test".to_string(),
                Vec::new(),
                report,
                TraceCapture::Meta,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap();
            w.write_session_start(1, Vec::new()).await.unwrap();
            w.flush().await.unwrap();
            let mut event = read_events(dir.path()).remove(0);
            // The one field that legitimately differs between two runs, removed so the assertion
            // below is about the grant and nothing else.
            event.as_object_mut().unwrap().remove("timestamp");
            event
        }

        let through_profile = event_for(UsernsGrant::ProfileConfining).await;
        let host_wide = event_for(UsernsGrant::RestrictionDisabledHostWide).await;

        assert_eq!(through_profile["containment_achieved"], "sealed");
        assert_eq!(host_wide["containment_achieved"], "sealed");
        assert_eq!(through_profile["userns_grant"], "profile_confining");
        assert_eq!(host_wide["userns_grant"], "restriction_disabled_host_wide");
        assert_ne!(
            through_profile, host_wide,
            "two hosts granting the user namespace by different mechanisms must not write the \
             same session_start record"
        );

        // Written for every Linux host, never skipped — its absence identifies a runtime that
        // predates the key rather than a host that was not asked.
        let unprobed = scope_report_for_tier(
            &CapabilityPolicy::default(),
            ContainmentClass::Advisory,
            EnforcementTier::EnvironmentOnly,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
        );
        assert!(
            serde_json::to_value(&unprobed).unwrap()["userns_grant"].is_null(),
            "off Linux the key is present and null, not absent"
        );
    }

    /// Central property: `session_start.effective_grants` is the *whole* `ScopeReport`,
    /// serialized byte-for-byte as `mur run --explain-scope --json` prints it — not a re-derived
    /// summary of it. Asserted by comparing against `serde_json::to_value` of the very report
    /// handed to the writer, so any field added to `ScopeReport` later is covered without this
    /// test being touched.
    ///
    /// The tier is a literal, never a probe: this asserts the recording, not the host.
    #[tokio::test]
    async fn session_start_records_the_whole_scope_report_as_effective_grants() {
        let policy = CapabilityPolicy {
            network_allow: vec!["https://api.example.com".to_string()],
            unix_sockets_allowed: false,
            filesystem_scope: Some("workdir".to_string()),
            shell_allow: vec!["python3".to_string()],
            spawn_allow: vec!["helper".to_string()],
            env_allow: vec!["TZ".to_string()],
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![InterpreterRuntimeDir {
                    path: "/usr/lib/python3.11".to_string(),
                    list_dir: true,
                }],
            }],
            ..CapabilityPolicy::default()
        };
        let expected = scope_report_for_tier(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            // Populated rather than empty so the whole-report assertion below really covers this
            // field: an empty vec would pass whether or not it was written at all.
            vec![crate::containment::StateStoreReport {
                artifact: "notes-tool".to_string(),
                store: "shey".to_string(),
                host_path: "/home/dev/.murmur/state/shey".to_string(),
            }],
            // Populated for the same reason as `state_stores` above.
            vec!["config-echo".to_string()],
        );

        let dir = tempfile::tempdir().unwrap();
        let mut w = TraceWriter::open(
            dir.path(),
            "test-session-id".to_string(),
            "test-capsule".to_string(),
            "1.0.0".to_string(),
            "claude-test".to_string(),
            vec!["network".to_string(), "shell".to_string()],
            expected.clone(),
            TraceCapture::Meta,
            None,
            false,
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["effective_grants"],
            serde_json::to_value(&expected).unwrap()
        );

        // The category-name summary that predates this field is unchanged, and the three
        // top-level containment fields agree with the report they are now read from.
        assert_eq!(
            events[0]["capabilities"],
            serde_json::json!(["network", "shell"])
        );
        assert_eq!(
            events[0]["effective_grants"]["state_stores"],
            serde_json::json!([{
                "artifact": "notes-tool",
                "store": "shey",
                "host_path": "/home/dev/.murmur/state/shey",
            }])
        );
        // Names only: what an artifact is configured *with* is operator plaintext and reaches
        // no trace record.
        assert_eq!(
            events[0]["effective_grants"]["configured_artifacts"],
            serde_json::json!(["config-echo"])
        );
        assert_eq!(events[0]["containment_declared"], "scoped");
        assert_eq!(events[0]["containment_achieved"], "scoped");
        assert_eq!(events[0]["workdir_exec"], false);
        assert_eq!(
            events[0]["effective_grants"]["network_allow"],
            serde_json::json!(["https://api.example.com"])
        );
        assert_eq!(
            events[0]["effective_grants"]["interpreter_runtime_grants"],
            serde_json::json!(["python3: /usr/lib/python3.11 (list_dir)"])
        );
    }

    #[tokio::test]
    async fn inference_fields_and_snake_case() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            100,
            50,
            "tool_call".to_string(),
            Some("bash".to_string()),
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "inference");
        assert_eq!(
            e["message_ids"],
            serde_json::json!(["msg_0000000000000000000000000000000a"]),
            "an agent-loop turn names the messages its request embedded"
        );
        assert_eq!(e["turn"], 0);
        assert_eq!(e["input_tokens"], 100);
        assert_eq!(e["output_tokens"], 50);
        assert_eq!(e["decision"], "tool_call");
        assert_eq!(e["tool_name"], "bash");
        // Confirm snake_case (not camelCase)
        assert!(e.get("inputTokens").is_none(), "must use snake_case");
        assert!(e.get("toolName").is_none(), "must use snake_case");
        // No driver reported usage, so no key claims one was reported.
        for key in [
            "input_tokens_actual",
            "output_tokens_actual",
            "cached_tokens",
            "cache_write_tokens",
        ] {
            assert!(e.get(key).is_none(), "{key} must be omitted, not null or 0");
        }
    }

    /// A partially-populated `usage` writes only the members the driver reported: a member it
    /// left out stays off the line entirely rather than being recorded as a zero.
    #[tokio::test]
    async fn inference_writes_only_the_reported_usage_members() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let usage = DriverUsage {
            input_tokens: Some(12043),
            output_tokens: None,
            cached_tokens: Some(0),
            cache_write_tokens: None,
        };
        w.write_inference(
            0,
            100,
            50,
            "end_turn".to_string(),
            None,
            None,
            Some(&usage),
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["input_tokens"], 100, "the estimate is untouched");
        assert_eq!(e["input_tokens_actual"], 12043);
        assert_eq!(e["cached_tokens"], 0, "a reported zero is written");
        assert!(e.get("output_tokens_actual").is_none());
        assert!(e.get("cache_write_tokens").is_none());
    }

    #[tokio::test]
    async fn tool_call_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            1,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "echo hi"}),
            42,
            "hi\n",
            100,
            55,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "tool_call");
        assert_eq!(e["turn"], 1);
        assert_eq!(e["tool_name"], "bash");
        assert_eq!(e["input_bytes"], 42);
        assert_eq!(e["output_bytes"], 100);
        assert_eq!(e["duration_ms"], 55);
        assert_eq!(e["status"], "ok");
        assert!(
            e.get("state_effect").is_none(),
            "state_effect must be omitted when the tool declared nothing"
        );
    }

    #[tokio::test]
    async fn tool_call_records_declared_state_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "murmur-tool-editor".to_string(),
            None,
            serde_json::json!({"operation": "read_file", "path": "a.txt"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            Some("read".to_string()),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["state_effect"], "read",
            "declared state_effect must be recorded verbatim on the tool_call event"
        );
    }

    #[tokio::test]
    async fn tool_call_records_declared_resource_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "murmur-tool-code-graph".to_string(),
            None,
            serde_json::json!({"symbol": "Foo::bar"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            Some("read".to_string()),
            Some("sym:Foo".to_string()),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["resource_id"], "sym:Foo",
            "declared resource_id must reach trace.jsonl verbatim, unparsed"
        );
    }

    #[tokio::test]
    async fn tool_call_omits_undeclared_resource_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "echo hi"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert!(
            events[0].get("resource_id").is_none(),
            "resource_id must be omitted entirely (not null) when the tool declared nothing"
        );
    }

    #[tokio::test]
    async fn shell_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_shell(
            0,
            "/usr/bin/echo".to_string(),
            "echo hi".to_string(),
            0,
            7,
            0,
            10,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "shell");
        // `binary` names what ran; `command` still carries only the argument list, so
        // neither can be derived from the other.
        assert_eq!(e["binary"], "/usr/bin/echo");
        assert_eq!(e["command"], "echo hi");
        assert_eq!(e["exit_code"], 0);
        assert_eq!(e["stdout_bytes"], 7);
        assert_eq!(e["stderr_bytes"], 0);
        assert_eq!(e["duration_ms"], 10);
    }

    #[tokio::test]
    async fn compaction_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_compaction(3, 80000, 20000).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "compaction");
        assert_eq!(e["turn"], 3);
        assert_eq!(e["tokens_before"], 80000);
        assert_eq!(e["tokens_after"], 20000);
    }

    #[tokio::test]
    async fn hook_dispatch_error_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_hook_dispatch_error("my-hook", "on-tool-call", "write-manifests")
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "hook_dispatch_error");
        assert_eq!(e["session_id"], "test-session-id");
        assert_eq!(e["hook_name"], "my-hook");
        assert_eq!(e["event"], "on-tool-call");
        assert_eq!(e["arm"], "write-manifests");
        assert!(e["timestamp"].as_u64().unwrap() > 0);
        // snake_case checks
        assert!(e.get("hookName").is_none(), "must use snake_case");
    }

    #[tokio::test]
    async fn session_end_fields_and_snake_case() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            100,
            50,
            "tool_call".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "ls"}),
            10,
            "file.txt\n",
            20,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_shell(0, "/bin/ls".to_string(), "ls".to_string(), 0, 3, 0, 2, None)
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events.last().unwrap();
        assert_eq!(se["event_type"], "session_end");
        assert_eq!(se["total_turns"], 1);
        assert_eq!(se["total_input_tokens"], 100);
        assert_eq!(se["total_output_tokens"], 50);
        assert_eq!(se["total_tool_calls"], 1);
        assert_eq!(se["total_shell_calls"], 1);
        assert_eq!(se["exit_status"], "ok");
        assert!(se["duration_ms"].as_u64().is_some());
        // snake_case checks
        assert!(se.get("exitStatus").is_none(), "must use snake_case");
        assert!(se.get("totalTurns").is_none(), "must use snake_case");
    }

    #[tokio::test]
    async fn write_session_end_if_not_ended_is_noop_when_already_ended() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_session_end("ok").await.unwrap();
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event_type"] == "session_end")
                .count(),
            1,
            "session_end should appear exactly once"
        );
    }

    #[tokio::test]
    async fn write_session_end_if_not_ended_writes_when_started_not_ended() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        // Simulate error path — session_end_if_not_ended in launch_session
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events
            .iter()
            .find(|e| e["event_type"] == "session_end")
            .unwrap();
        assert_eq!(se["exit_status"], "failed");
    }

    #[tokio::test]
    async fn no_session_end_written_when_session_never_started() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        // session_start never written — fallback should be no-op
        w.write_session_end_if_not_ended("failed").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert!(
            events.iter().all(|e| e["event_type"] != "session_end"),
            "session_end must not appear when session_start was never written"
        );
    }

    #[tokio::test]
    async fn session_id_on_every_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec!["bash".to_string()])
            .await
            .unwrap();
        w.write_inference(
            0,
            10,
            5,
            "end_turn".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        for e in &events {
            assert_eq!(e["session_id"], "test-session-id");
        }
    }

    #[tokio::test]
    async fn counts_match_per_event_sum() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_inference(
            0,
            50,
            25,
            "tool_call".to_string(),
            Some("bash".to_string()),
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            None,
        )
        .await
        .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "echo"}),
            5,
            "ok",
            10,
            3,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_shell(
            0,
            "/bin/echo".to_string(),
            "echo".to_string(),
            0,
            4,
            0,
            1,
            None,
        )
        .await
        .unwrap();
        w.write_inference(
            1,
            60,
            30,
            "end_turn".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let inference_count = events
            .iter()
            .filter(|e| e["event_type"] == "inference")
            .count();
        let tool_count = events
            .iter()
            .filter(|e| e["event_type"] == "tool_call")
            .count();
        let shell_count = events.iter().filter(|e| e["event_type"] == "shell").count();

        let se = events
            .iter()
            .find(|e| e["event_type"] == "session_end")
            .unwrap();
        assert_eq!(
            se["total_turns"].as_u64().unwrap() as usize,
            inference_count
        );
        assert_eq!(
            se["total_tool_calls"].as_u64().unwrap() as usize,
            tool_count
        );
        assert_eq!(
            se["total_shell_calls"].as_u64().unwrap() as usize,
            shell_count
        );
    }

    #[tokio::test]
    async fn tool_call_input_always_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await; // TraceCapture::Meta
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "ls -la"}),
            20,
            "total 8\n",
            80,
            12,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "tool_call");
        assert_eq!(e["input"]["command"], "ls -la");
        assert!(e.get("input").is_some(), "input must always be present");
    }

    #[tokio::test]
    async fn tool_call_output_absent_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await; // TraceCapture::Meta
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "ls"}),
            10,
            "file.txt\n",
            90,
            8,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert!(
            e.get("output").is_none(),
            "output must be absent under a capture mode that stores no bodies"
        );
        assert_eq!(e["output_bytes"], 90, "output_bytes must still be recorded");
    }

    #[tokio::test]
    async fn skill_call_writes_skill_call_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_skill_call(2, "my-skill".to_string(), 1024, 8, "ok".to_string())
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "skill_call");
        assert_eq!(e["turn"], 2);
        assert_eq!(e["skill_name"], "my-skill");
        assert_eq!(e["output_bytes"], 1024);
        assert_eq!(e["duration_ms"], 8);
        assert_eq!(e["status"], "ok");
    }

    #[tokio::test]
    async fn skill_call_does_not_increment_total_tool_calls() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec![]).await.unwrap();
        w.write_tool_call(
            0,
            "real-tool".to_string(),
            None,
            serde_json::json!({}),
            2,
            "data",
            4,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_skill_call(1, "my-skill".to_string(), 512, 3, "ok".to_string())
            .await
            .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let se = events
            .iter()
            .find(|e| e["event_type"] == "session_end")
            .unwrap();
        assert_eq!(
            se["total_tool_calls"], 1,
            "skill_call must not inflate total_tool_calls"
        );
    }

    #[tokio::test]
    async fn tool_call_output_present_when_opted_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        w.write_tool_call(
            0,
            "bash".to_string(),
            None,
            serde_json::json!({"command": "echo hello"}),
            22,
            "hello\n",
            60,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(
            e["output"].as_str().unwrap(),
            "hello\n",
            "output must be present under capture: content"
        );
        assert_eq!(e["output_bytes"], 60);
    }

    #[tokio::test]
    async fn task_end_carries_reopen_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_task_end("tsk_1", "ok", 2).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let end = events
            .iter()
            .find(|e| e["event_type"] == "task_end")
            .unwrap();
        assert_eq!(end["task_id"], "tsk_1");
        assert_eq!(end["exit_status"], "ok");
        assert_eq!(end["reopen_count"], 2);
    }

    #[tokio::test]
    async fn task_reopened_names_hook_reason_and_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_task_reopened("tsk_1", "gatekeeper", "tests still fail", 1)
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let re = events
            .iter()
            .find(|e| e["event_type"] == "task_reopened")
            .unwrap();
        assert_eq!(re["task_id"], "tsk_1");
        assert_eq!(re["hook_name"], "gatekeeper");
        assert_eq!(re["reason"], "tests still fail");
        assert_eq!(re["reopen_number"], 1);
        assert!(re["timestamp"].as_u64().unwrap() > 0);
    }

    /// `task_turns()` reports the cumulative per-task turn count the reopen loop reads to
    /// compute the remaining `max_turns` budget: it advances with each `write_inference`
    /// and resets on `write_task_start`.
    #[tokio::test]
    async fn task_turns_accessor_tracks_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
        w.write_inference(
            0,
            10,
            5,
            "end_turn".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_inference(
            1,
            10,
            5,
            "end_turn".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(w.task_turns(), 2);
        // A new task resets the per-task counter.
        w.write_task_start("tsk_2", "ctx_2", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
    }

    // ── Event identity and parenting ──────────────────────────────────────────

    /// The full tree one task produces, walked the way a reader walks it: every line is
    /// identified, every non-null `parent_id` names a line already written, and the shape is
    /// session → task → turn → {tool_call, shell}.
    #[tokio::test]
    async fn every_event_carries_a_unique_id_and_parents_to_an_earlier_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, vec!["bash".to_string()])
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_inference(
            0,
            10,
            5,
            "tool_call".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
            Some("toolu_1".to_string()),
            serde_json::json!({"command": "echo hi"}),
            10,
            "hi\n",
            3,
            5,
            "ok".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_shell(
            0,
            "/bin/bash".to_string(),
            "echo hi".to_string(),
            0,
            3,
            0,
            2,
            None,
        )
        .await
        .unwrap();
        w.write_task_end("tsk_1", "ok", 0).await.unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (i, event) in events.iter().enumerate() {
            let id = event["event_id"].as_str().unwrap();
            assert!(id.starts_with("evt_") && id.len() == 36, "event {i}: {id}");
            assert!(id[4..].chars().all(|c| c.is_ascii_hexdigit()), "event {i}");
            assert!(seen.insert(id.to_string()), "event {i} reuses {id}");
            assert!(
                event.get("parent_id").is_some(),
                "event {i} has no parent_id"
            );
            if let Some(parent) = event["parent_id"].as_str() {
                assert!(seen.contains(parent), "event {i} names dangling {parent}");
            }
        }

        let by_type = |ty: &str| {
            events
                .iter()
                .find(|e| e["event_type"] == ty)
                .unwrap_or_else(|| panic!("no {ty}"))
                .clone()
        };
        let session = by_type("session_start");
        assert!(session["parent_id"].is_null());
        assert_eq!(session["event_id"], w.session_event_id());

        let task = by_type("task_start");
        assert_eq!(task["parent_id"], session["event_id"]);

        let turn = by_type("inference");
        assert_eq!(turn["parent_id"], task["event_id"]);

        assert_eq!(by_type("tool_call")["parent_id"], turn["event_id"]);
        assert_eq!(by_type("shell")["parent_id"], turn["event_id"]);
        assert_eq!(by_type("task_end")["parent_id"], task["event_id"]);
        assert_eq!(by_type("session_end")["parent_id"], session["event_id"]);
    }

    /// A hook's `run-inference` record is a product of the turn it ran inside, not a turn of
    /// its own: it parents to the agent loop's inference and never replaces it as the node the
    /// turn's other events hang off.
    #[tokio::test]
    async fn hook_origin_inference_hangs_off_the_turn_rather_than_becoming_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, Vec::new()).await.unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_inference(
            0,
            10,
            5,
            "tool_call".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        let origin = InferenceOrigin {
            source: "hook:gatekeeper".to_string(),
            model: "claude-test".to_string(),
        };
        w.write_inference(
            0,
            1,
            1,
            "end_turn".to_string(),
            None,
            Some(&origin),
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let turn = &events[2];
        let hook = &events[3];
        assert!(turn["origin"].is_null());
        assert_eq!(hook["origin"], "hook:gatekeeper");
        assert_eq!(hook["parent_id"], turn["event_id"]);
        // The hook sent a message list the runtime never minted, so the key is absent rather
        // than empty.
        assert!(hook.get("message_ids").is_none(), "{hook}");
    }

    /// A writer with no session frame behind it names no parent. This is the script-capsule
    /// `a2a_send` drain, which opens a writer purely to flush buffered sends into a file that
    /// has no `session_start` line — naming one would dangle.
    #[tokio::test]
    async fn a2a_send_parents_to_null_without_a_session_frame_and_to_it_with_one() {
        let bare = tempfile::tempdir().unwrap();
        let mut w = make_writer(bare.path()).await;
        w.write_a2a_send(
            "http://peer",
            "msg_1",
            "tsk_1",
            "ctx_1",
            None,
            TrustClass::Untrusted,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        assert!(read_events(bare.path())[0]["parent_id"].is_null());

        let framed = tempfile::tempdir().unwrap();
        let mut w = make_writer(framed.path()).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.write_a2a_send(
            "http://peer",
            "msg_1",
            "tsk_1",
            "ctx_1",
            None,
            TrustClass::Untrusted,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        let events = read_events(framed.path());
        assert_eq!(events[1]["parent_id"], events[0]["event_id"]);
    }

    /// Turn-level events carry the enclosing task's id, and `null` once the task has ended —
    /// `task_id` is a record of scope, not a value the writer holds on to.
    #[tokio::test]
    async fn turn_level_events_carry_the_active_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, Vec::new()).await.unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_inference(
            0,
            10,
            5,
            "tool_call".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_skill_call(0, "house-style".to_string(), 12, 1, "ok".to_string())
            .await
            .unwrap();
        w.write_compaction(0, 100, 40).await.unwrap();
        w.write_task_end("tsk_1", "ok", 0).await.unwrap();
        w.write_inference(
            1,
            1,
            1,
            "end_turn".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        for event in events
            .iter()
            .filter(|e| {
                matches!(
                    e["event_type"].as_str(),
                    Some("inference" | "skill_call" | "compaction")
                )
            })
            .take(3)
        {
            assert_eq!(event["task_id"], "tsk_1", "{event}");
        }
        let after = events.last().unwrap();
        assert_eq!(after["event_type"], "inference");
        assert!(
            after["task_id"].is_null(),
            "an inference outside any task carries a null task_id, not the last task's"
        );
        // Once the task has ended, its events fall back to the session node.
        assert_eq!(after["parent_id"], events[0]["event_id"]);
    }

    /// The provider's id is recorded verbatim; an absent or empty one records as `null`, never
    /// as an id the provider never issued.
    #[tokio::test]
    async fn tool_call_records_the_provider_id_verbatim_and_empty_as_null() {
        for (given, expected) in [
            (Some("toolu_1".to_string()), Some("toolu_1")),
            (Some(String::new()), None),
            (None, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            w.write_tool_call(
                0,
                "bash".to_string(),
                given.clone(),
                serde_json::json!({}),
                2,
                "",
                0,
                1,
                "ok".to_string(),
                None,
                None,
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            let e = &read_events(dir.path())[0];
            match expected {
                Some(id) => assert_eq!(e["tool_call_id"], id, "given={given:?}"),
                None => assert!(e["tool_call_id"].is_null(), "given={given:?}"),
            }
        }
    }

    /// A declined compaction is a full record of the decline, not a bare marker: it names the
    /// turn, the task, the occupancy the session went on running over, and the reason.
    #[tokio::test]
    async fn compaction_declined_records_turn_task_tokens_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(10, Vec::new()).await.unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), None, 3)
            .await
            .unwrap();
        w.write_inference(
            2,
            10,
            5,
            "tool_call".to_string(),
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.write_compaction_declined(2, 4321, COMPACTION_DECLINED_UNRESOLVED_TOOL_CALL)
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let declined: Vec<&Value> = events
            .iter()
            .filter(|e| e["event_type"] == "compaction_declined")
            .collect();
        assert_eq!(declined.len(), 1);
        let d = declined[0];
        assert_eq!(d["reason"], "unresolved_tool_call");
        assert_eq!(d["turn"], 2);
        assert_eq!(d["task_id"], "tsk_1");
        assert_eq!(d["tokens"], 4321);
        assert!(d["event_id"].as_str().unwrap().starts_with("evt_"));
        assert_eq!(
            d["parent_id"],
            events
                .iter()
                .find(|e| e["event_type"] == "inference")
                .unwrap()["event_id"]
        );
    }

    /// `evt_` ids sort by mint time and carry their own millisecond timestamp, the same
    /// property `mur trace list --since` reads off a `ses_` id.
    #[tokio::test]
    async fn event_ids_sort_by_mint_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for turn in 0..5 {
            w.write_inference(
                turn,
                1,
                1,
                "end_turn".to_string(),
                None,
                None,
                None,
                Vec::new(),
                None,
            )
            .await
            .unwrap();
        }
        w.flush().await.unwrap();

        let ids: Vec<String> = read_events(dir.path())
            .iter()
            .map(|e| e["event_id"].as_str().unwrap().to_string())
            .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "evt_ ids must sort into mint order");
    }

    // ── Wire capture: hashes, blobs and the capture gate ─────────────────────

    /// A payload in the shape `build_driver_payload` produces, with `n` messages.
    fn wire_payload(system: &str, messages: usize) -> Value {
        let messages: Vec<Value> = (0..messages)
            .map(|i| serde_json::json!({"role": "user", "content": format!("m{i}")}))
            .collect();
        serde_json::json!({
            "model": "claude-test",
            "max_tokens": 1024,
            "messages": messages,
            "tools": [{"name": "bash"}],
            "params": {},
            "system": system,
        })
    }

    async fn write_turn(w: &mut TraceWriter, turn: u32, payload: &Value, response: &str) {
        let wire = WireCapture::from_driver_payload(payload, response);
        w.write_inference(
            turn,
            10,
            5,
            "end_turn".to_string(),
            None,
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            Some(&wire),
        )
        .await
        .unwrap();
    }

    fn blob_path(dir: &std::path::Path, sha: &str) -> std::path::PathBuf {
        dir.join(BLOB_DIR_NAME).join(sha)
    }

    /// Every hash an `inference` event names under `content` is the filename of a real file whose
    /// own digest is that filename — the whole point of the store.
    #[tokio::test]
    async fn content_capture_names_blobs_that_exist_and_rehash_to_their_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let payload = wire_payload("You are a capsule.", 3);
        write_turn(&mut w, 0, &payload, r#"{"stop_reason":"end_turn"}"#).await;
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        let mut named: Vec<String> = ["system_sha", "tools_sha", "response_sha"]
            .iter()
            .map(|key| {
                e[*key]
                    .as_str()
                    .unwrap_or_else(|| panic!("{key} missing from {e}"))
                    .to_string()
            })
            .collect();
        let shas = e["message_shas"].as_array().expect("message_shas");
        assert_eq!(shas.len(), 3);
        named.extend(shas.iter().map(|s| s.as_str().unwrap().to_string()));

        for sha in &named {
            let path = blob_path(dir.path(), sha);
            let bytes =
                std::fs::read(&path).unwrap_or_else(|err| panic!("blob {sha} must exist: {err}"));
            assert_eq!(murmur_artifact::sha256_hex(&bytes), *sha);
            // A bare sha256 names content; `evt_`/`msg_`/`ses_` name entities. Neither a prefix
            // nor an extension may creep onto a blob name.
            assert_eq!(sha.len(), 64, "{sha}");
            assert!(
                sha.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{sha}"
            );
        }
    }

    /// The hashed system body is the payload's `system` string, as text — so the blob reads as a
    /// prompt rather than as a JSON-quoted one.
    #[tokio::test]
    async fn the_system_blob_holds_the_prompt_as_readable_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let payload = wire_payload("[Capsule] you are cap.\nBe terse.", 1);
        write_turn(&mut w, 0, &payload, "{}").await;
        w.flush().await.unwrap();

        let sha = read_events(dir.path())[0]["system_sha"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            std::fs::read_to_string(blob_path(dir.path(), &sha)).unwrap(),
            "[Capsule] you are cap.\nBe terse."
        );
    }

    /// `meta` is the default and hashes exactly what `content` hashes; the difference is only
    /// whether the bodies are on disk.
    #[tokio::test]
    async fn meta_writes_the_same_hashes_and_no_bodies() {
        let payload = wire_payload("You are a capsule.", 2);
        let response = r#"{"stop_reason":"end_turn"}"#;

        let meta = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(meta.path(), TraceCapture::Meta).await;
        write_turn(&mut w, 0, &payload, response).await;
        w.flush().await.unwrap();

        let content = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(content.path(), TraceCapture::Content).await;
        write_turn(&mut w, 0, &payload, response).await;
        w.flush().await.unwrap();

        let meta_event = read_events(meta.path()).remove(0);
        let content_event = read_events(content.path()).remove(0);
        for key in ["system_sha", "tools_sha", "response_sha", "message_shas"] {
            assert_eq!(meta_event[key], content_event[key], "{key}");
        }
        assert!(
            !meta.path().join(BLOB_DIR_NAME).exists(),
            "meta must not create the blob directory"
        );
        assert!(content.path().join(BLOB_DIR_NAME).is_dir());
    }

    /// The default a writer gets when the manifest declares no `trace:` block.
    #[tokio::test]
    async fn the_default_capture_mode_is_meta() {
        assert_eq!(TraceCapture::default(), TraceCapture::Meta);
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(dir.path(), TraceCapture::default(), None, false).await;
        write_turn(&mut w, 0, &wire_payload("s", 1), "{}").await;
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        assert!(e.get("system_sha").is_some());
        assert!(!dir.path().join(BLOB_DIR_NAME).exists());
    }

    /// Under `none` not one of the four hash keys is present, and every other field is.
    #[tokio::test]
    async fn none_omits_every_hash_and_leaves_the_rest_of_the_event_intact() {
        let payload = wire_payload("You are a capsule.", 2);

        let off = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(off.path(), TraceCapture::None).await;
        write_turn(&mut w, 3, &payload, "{}").await;
        w.flush().await.unwrap();

        let e = read_events(off.path()).remove(0);
        for key in ["system_sha", "tools_sha", "response_sha", "message_shas"] {
            assert!(
                e.get(key).is_none(),
                "{key} must be absent under none, got {e}"
            );
        }
        assert!(!off.path().join(BLOB_DIR_NAME).exists());
        for key in [
            "event_type",
            "event_id",
            "parent_id",
            "session_id",
            "timestamp",
            "turn",
            "task_id",
            "input_tokens",
            "output_tokens",
            "decision",
            "tool_name",
            "message_ids",
        ] {
            assert!(
                e.get(key).is_some(),
                "{key} must survive under none, got {e}"
            );
        }
        assert_eq!(e["turn"], 3);
        assert_eq!(e["message_ids"].as_array().unwrap().len(), 1);
    }

    /// A prompt that does not change across ten turns is one blob, written once and never
    /// rewritten — the file's bytes are replaced by hand after the first turn and must survive.
    #[tokio::test]
    async fn an_unchanged_system_prompt_is_stored_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let payload = wire_payload("You are a capsule.", 1);
        write_turn(&mut w, 0, &payload, "{}").await;

        let sha = {
            w.flush().await.unwrap();
            read_events(dir.path())[0]["system_sha"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let path = blob_path(dir.path(), &sha);
        std::fs::write(&path, b"sentinel").unwrap();

        for turn in 1..10 {
            write_turn(&mut w, turn, &payload, "{}").await;
        }
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 10);
        for e in &events {
            assert_eq!(e["system_sha"], sha.as_str());
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"sentinel",
            "an existing blob is never rewritten"
        );
        let matching = std::fs::read_dir(dir.path().join(BLOB_DIR_NAME))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().file_name() == sha.as_str())
            .count();
        assert_eq!(matching, 1, "exactly one file bears that name");
    }

    /// A hook's `run-inference` sends a payload the runtime never built, so its record names no
    /// hashes — absent from the JSON, not empty strings.
    #[tokio::test]
    async fn a_hook_origin_record_carries_no_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let origin = InferenceOrigin {
            source: "hook:gatekeeper".to_string(),
            model: "claude-test".to_string(),
        };
        w.write_inference(
            0,
            1,
            1,
            "end_turn".to_string(),
            None,
            Some(&origin),
            None,
            Vec::new(),
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let e = read_events(dir.path()).remove(0);
        assert_eq!(e["origin"], "hook:gatekeeper");
        for key in ["system_sha", "tools_sha", "response_sha", "message_shas"] {
            assert!(e.get(key).is_none(), "{key} must be absent, got {e}");
        }
        assert!(!dir.path().join(BLOB_DIR_NAME).exists());
    }

    /// Two runs sharing a prefix agree on that prefix's `message_shas` and disagree from the
    /// changed message onwards — the divergence index is the first unequal position.
    #[tokio::test]
    async fn message_shas_locate_the_index_two_runs_diverge_at() {
        async fn shas_for(payload: &Value) -> Vec<String> {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer_with_opts(dir.path(), TraceCapture::Meta).await;
            write_turn(&mut w, 0, payload, "{}").await;
            w.flush().await.unwrap();
            read_events(dir.path())[0]["message_shas"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_string())
                .collect()
        }

        let first = wire_payload("You are a capsule.", 4);
        let mut second = first.clone();
        second["messages"][2]["content"] = serde_json::json!("diverged");

        let a = shas_for(&first).await;
        let b = shas_for(&second).await;
        assert_eq!(a.len(), 4);
        let divergence = a.iter().zip(&b).position(|(x, y)| x != y);
        assert_eq!(divergence, Some(2));
        assert_eq!(a[3], b[3], "only the changed message changes its own sha");
    }

    /// A message repeated inside one request hashes to one name and one file. Ids never repeat;
    /// shas repeat exactly when content does, and that is what makes them comparable.
    #[tokio::test]
    async fn identical_messages_share_one_sha_and_one_blob() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "same"}, {"role": "user", "content": "same"}],
            "tools": [],
            "system": "s",
        });
        write_turn(&mut w, 0, &payload, "{}").await;
        w.flush().await.unwrap();

        let shas = read_events(dir.path())[0]["message_shas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(shas[0], shas[1]);
        assert!(blob_path(dir.path(), &shas[0]).is_file());
    }

    /// Blobs are the bytes as sent, so a peer handle a tool call would have had redacted out of
    /// its summary stays in the body a `content` capture stores.
    #[tokio::test]
    async fn a_message_blob_is_the_body_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        let message = serde_json::json!({"role": "user", "content": "hello"});
        let payload = serde_json::json!({
            "messages": [message.clone()],
            "tools": [],
            "system": "s",
        });
        write_turn(&mut w, 0, &payload, "{}").await;
        w.flush().await.unwrap();

        let sha = read_events(dir.path())[0]["message_shas"][0]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            std::fs::read(blob_path(dir.path(), &sha)).unwrap(),
            serde_json::to_vec(&message).unwrap()
        );
    }
}
