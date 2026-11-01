use std::time::{Instant, SystemTime, UNIX_EPOCH};

use murmur_artifact::{ContainmentClass, LockOrigin, TraceCapture};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    fs::File,
    io::{AsyncWriteExt, BufWriter},
};

use crate::{
    agent::{DriverUsage, OccupancyBasis},
    containment::ScopeReport,
    credential_gateway::GatewayOffsets,
    formation::FormationId,
    gateway_credential::CredentialChange,
    inference_retry::{InferenceRetryRecord, RetryNote, RetryStop},
    lanes::TaskLane,
    origin::{TaskProvenance, TrustClass},
    trace_blobs::BlobStore,
    types::InstalledArtifactSummary,
    workdir_fs::{WorkdirRoot, DEFAULT_FILE_MODE},
};

/// A session's trace file, directly under its session root.
pub(crate) const TRACE_FILE_NAME: &str = "trace.jsonl";

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
    /// Where the inference credential came from: `"config"`, `"environment"`, `"manifest"` or
    /// `"none"`. Set by [`Self::set_credential_source`] before `session_start` is written.
    credential_source: &'static str,
    /// Every credential gateway the session holds. Set by [`Self::set_gateways`] before
    /// `session_start` is written; empty until then.
    gateways: Vec<SessionGateway>,
    /// What a controller may change on this session. Set by [`Self::set_control`] before
    /// `session_start` is written; `None` for a capsule with no `control:` block.
    control: Option<SessionControl>,
    /// The driver choices. Set by [`Self::set_inference_choices`] before `session_start` is
    /// written; empty for a capsule with no alternates.
    inference_choices: Vec<SessionInferenceChoice>,
    /// `inference.tool_refresh` as `ToolRefresh::wire_name` spells it, or `None` under
    /// `transport: process`. Set by [`Self::set_tool_refresh`] before `session_start` is written.
    tool_refresh: Option<&'static str>,
    /// The staged artifacts whose `murmur.lock` pin a running capsule fetched. Set by
    /// [`Self::set_runtime_artifacts`] before `session_start` is written; empty until then.
    runtime_artifacts: Vec<RuntimeArtifact>,
    /// Where staging took the facts about each allowlisted binary it examined. Set by
    /// [`Self::set_host_facts`] before `session_start` is written; empty until then.
    host_facts: Vec<HostFactsRecord>,
    /// What staging read for `inference.instruction_files`, one record per entry. Set by
    /// [`Self::set_instruction_files`] before `session_start` is written; `None` until then and
    /// for a capsule without the key.
    instruction_files: Option<Vec<crate::instruction_files::InstructionFileRecord>>,
    /// The session `mur run --resume` continued, verbatim as the operator's address resolved it.
    /// `None` on every ordinary launch. Written to `session_start` on both, so its absence
    /// identifies a trace from a runtime that predates the key.
    resumed_from: Option<String>,
    /// The launch-scoped context id: the `--context` value, or the id `--resume` resolved to.
    /// `None` when each task of this launch mints its own.
    context_id: Option<String>,
    /// The session that spawned this one, and the delegation that created it, read out of the
    /// handle the spawning capsule's launcher injected. Both `None` for a capsule nobody
    /// delegated, and both omitted from `session_start` when absent rather than written as null.
    spawned_by: Option<String>,
    spawn_delegation_id: Option<String>,
    /// The formation this session is a member of. Set by [`Self::set_formation_id`] before
    /// `session_start` is written; `None` for a session in no formation.
    formation_id: Option<FormationId>,
    /// This session's roster name and callees, set by [`Self::set_formation_member`] for a
    /// formation member.
    formation_member: Option<(String, Vec<String>)>,
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
    /// Agent-loop `inference` lines (`origin` absent) written since `task_start`, counted or
    /// not; a hook's `run-inference` record is not one. Where an attempt's turn numbering starts,
    /// so a task's turns number straight through its reopens and continuations. Reset at both
    /// task boundaries, so a run outside any task frame starts at 0.
    task_agent_turns: u32,
    /// Counted hook `run-inference` lines (`origin` present) written since `task_start`: the part
    /// of [`Self::task_turns`] no agent-loop turn spent. Reset with the other per-task counters.
    task_hook_inferences: u32,
    /// Agent-loop `inference` lines whose `stop_reason` is `"max_tokens"`, written since
    /// `task_start`: `task_end.capped_turns`. Reset with the other per-task counters.
    task_capped_turns: u32,
    task_start_instant: Option<Instant>,
    pub(crate) active_task_id: Option<String>,
    /// How many `task_failed` lines this writer has written, so a caller can tell whether one run
    /// wrote its own without comparing reasons.
    task_failures_written: u64,
    /// The `reason` of the last `task_failed` line, as written (already capped).
    last_task_failure: Option<String>,
    /// The spend ceilings `session_start` records. Set by [`Self::set_spend_ceilings`].
    max_session_tokens: Option<u64>,
    machine_tokens_per_day: Option<u64>,
    /// What `session-status` reads, fed by this writer's `inference`, `task_start` and `task_end`
    /// writes and named by `session_start.deadline`. Set by [`Self::set_session_status`]; `None`
    /// feeds nothing.
    session_status: Option<std::sync::Arc<crate::session_status::SessionStatus>>,
    /// The session's record of component calls the wall-clock bound stopped, drained into
    /// `call_deadline` lines ahead of every line this writer writes. Set by
    /// [`Self::set_call_deadlines`]; `None` drains nothing.
    call_deadlines: Option<crate::limits::CallDeadlines>,
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

/// How one `transport: http` call was sent, as its `inference` line records it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InferenceCallSettings {
    /// The output cap the request carried as `max_tokens`, and the amount the spend meter
    /// reserved for it.
    pub(crate) max_output_tokens: u32,
    /// The input budget the call was measured against, `min(input ceiling, context window −
    /// max_output_tokens)`; `None` when its model has no window.
    pub(crate) input_budget: Option<u32>,
    /// The `MURMUR_INFERENCE_DRIVER_CONFIG` the call's driver ran with, from
    /// [`recorded_driver_config`]. `None` when the driver saw none.
    pub(crate) driver_config: Option<Value>,
}

/// The `MURMUR_INFERENCE_DRIVER_CONFIG` in `env`, as an `inference` line records it: `None` when
/// `env` has none, otherwise its JSON with every object key and string leaf passed through
/// `redact`, at any depth. A number leaf whose decimal text `redact` changes becomes the string
/// [`crate::gateway_credential::REDACTED`]; booleans and nulls are kept as they are.
///
/// A value that is not JSON is recorded as `redact(value)`, a JSON string: it is what the driver
/// saw. The manifest parser only produces JSON objects here.
pub(crate) fn recorded_driver_config(
    env: &[(String, String)],
    redact: impl Fn(&str) -> String,
) -> Option<Value> {
    fn redact_value(value: Value, redact: &impl Fn(&str) -> String) -> Value {
        match value {
            Value::String(text) => Value::String(redact(&text)),
            Value::Number(number) => {
                let text = number.to_string();
                if redact(&text) == text {
                    Value::Number(number)
                } else {
                    Value::String(crate::gateway_credential::REDACTED.to_string())
                }
            }
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| redact_value(item, redact))
                    .collect(),
            ),
            Value::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| (redact(&key), redact_value(item, redact)))
                    .collect(),
            ),
            other @ (Value::Bool(_) | Value::Null) => other,
        }
    }
    let (_, raw) = env
        .iter()
        .find(|(key, _)| key == "MURMUR_INFERENCE_DRIVER_CONFIG")?;
    Some(match serde_json::from_str::<Value>(raw) {
        Ok(value) => redact_value(value, &redact),
        Err(_) => Value::String(redact(raw)),
    })
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
pub(crate) fn new_event_id() -> String {
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
    /// `inference.max_turns`. Always written, `null` when no cap applies.
    max_turns: Option<u32>,
    /// The primary driver choice's output cap and context window in force, and where each came
    /// from. Always written, `null` under `transport: process`, which resolves none.
    model_limits: Option<crate::model_limits::ResolvedModelLimits>,
    /// `inference.max_session_tokens` as this session enforces it. Always written, `null` when
    /// no session ceiling applies.
    max_session_tokens: Option<u64>,
    /// `spend.machine_tokens_per_day` when this session keeps the machine ledger. Always written,
    /// `null` when the machine ceiling does not cover this session.
    machine_tokens_per_day: Option<u64>,
    /// The deadline this session runs under: the earliest of its own `mur run --deadline` and the
    /// one it inherited. Always written, `null` when it has neither.
    deadline: Option<SessionStartDeadline>,
    capabilities: Vec<String>,
    tools_declared: Vec<String>,
    /// Strongest containment class any source asked for. A *requirement*, not an observation.
    containment_declared: ContainmentClass,
    /// What this session's commands actually ran under: the class of the tier it installed (the
    /// probed host tier capped by the declared floor), capped at `advisory` by `workdir_exec` below
    /// or a runtime directory inside the workdir. Reading these two together is how a session's
    /// trace shows whether the capsule ran under the containment its operator asked for.
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
    /// environment variable the policy actually opened, plus the enforcement tier installed — in the
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
    /// Where the inference credential this session attaches came from: `"config"`,
    /// `"environment"`, `"manifest"` or `"none"`. Never the value.
    credential_source: &'static str,
    /// Every credential gateway the session holds, the inference gateway first. Always written,
    /// and empty for a session with none, so an auditor can tell an unmetered third-party
    /// upstream from spend the ceilings cover.
    gateways: Vec<SessionGateway>,
    /// What a controller holding this session's control token may change: the setting names and
    /// secret names `control:` declares. Omitted entirely for a capsule with no `control:` block.
    #[serde(skip_serializing_if = "Option::is_none")]
    control: Option<SessionControl>,
    /// Every driver choice the agent loop may be switched to, the primary first, each with its
    /// credential's source and whether it resolved at launch. Omitted entirely for a capsule that
    /// declares no `inference.alternates`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inference_choices: Vec<SessionInferenceChoice>,
    /// `inference.tool_refresh`: `"compaction"` or `"immediate"` under `transport: http`, `null`
    /// under `transport: process`, where the harness owns its tool list. Always written.
    tool_refresh: Option<&'static str>,
    /// Every staged artifact whose `murmur.lock` origin is `runtime` — pinned by a running
    /// capsule's `manage.pull()`, never vetted by the operator — in staging order. Each reaches
    /// the model marked untrusted. Always written, `[]` when there are none.
    runtime_artifacts: Vec<RuntimeArtifact>,
    /// One object per allowlisted binary staging examined, saying whether its facts came from a
    /// kept `~/.murmur/host_facts` entry or were worked out, and why. Always written, `[]` when
    /// no binary was examined.
    host_facts: Vec<HostFactsRecord>,
    /// One object per `inference.instruction_files` entry, in manifest order: its `path` as
    /// written and its `status`, with `resolved`, `bytes`, `included_bytes`, `truncated` and
    /// `sha256` (of the included text) on a `read` entry, `same_as` on a `same_file` one and
    /// `error` on an `unreadable` one. `null` for a capsule without the key, `[]` for an empty
    /// list. Always written.
    instruction_files: Option<Vec<crate::instruction_files::InstructionFileRecord>>,
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
    /// The `ses_` id of the session that spawned this one, for a capsule launched by another
    /// capsule's `delegate-task`. Omitted from the record entirely for every other launch: the
    /// field is absent, not null.
    #[serde(skip_serializing_if = "Option::is_none")]
    spawned_by: Option<String>,
    /// The `dlg_` id of the delegation that created this session, character-identical to the id
    /// in the spawning session's own `delegation_start`. Present exactly when `spawned_by` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    delegation_id: Option<String>,
    /// The formation this session is a member of. Omitted entirely for a session in no formation,
    /// absent rather than null, on the same terms as `spawned_by`. Being on `session_start`, it is
    /// on the trace's first line, which is the only line a reader grouping sessions reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    formation_id: Option<FormationId>,
    /// This session's roster name, for a session its formation launcher handed credentials to.
    /// Omitted for every other session, a delegated child of a member included.
    #[serde(skip_serializing_if = "Option::is_none")]
    formation_member: Option<String>,
    /// The members this one may call, in roster order: `[]` for a member that may call nobody.
    /// Present exactly when `formation_member` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    formation_callees: Option<Vec<String>>,
}

/// `session_start.deadline`.
#[derive(Serialize)]
struct SessionStartDeadline {
    /// When the runtime ends the session, RFC 3339 UTC.
    at: String,
    /// Seconds from `mur run`'s start to `at`: the `--deadline` value, or for an inherited
    /// deadline the whole seconds left at the start, rounded up.
    secs: u64,
    /// The notice margin in force, `lifecycle.deadline_notice_secs` or its default; `0` sends
    /// none.
    notice_secs: u64,
    /// `SessionDeadline::source`: `"own"` or `"inherited"`.
    source: &'static str,
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
    /// The input this turn cost, and the number the task and session totals accumulate.
    ///
    /// Under `transport: http` it is the runtime's own tiktoken estimate of the request, counted
    /// before the request was sent, and the provider's own count sits beside it in
    /// `input_tokens_actual`. Under `transport: process` the runtime estimates nothing — the
    /// harness holds the conversation — so this *is* the harness's own reported count and
    /// `input_tokens_actual` is absent: there is one measurement on that transport and it is
    /// recorded once.
    ///
    /// Absent means no count was made at all: a `process` driver reporting no usage, or a failed
    /// call that is not counted (see [`TraceWriter::write_failed_inference`]). Never conflate it
    /// with `0`, which is a count of none.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_tokens: Option<u64>,
    /// The output this turn cost, on the same terms as [`Self::input_tokens`]: the runtime's own
    /// tiktoken estimate of the raw driver response under `http`, the harness's own reported
    /// count under `process`.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_tokens: Option<u64>,
    decision: String,
    /// The provider's own stop reason for this turn, verbatim as the agent loop dispatched on
    /// it. Present on every agent-loop turn, including one whose driver reported nothing — an
    /// empty string then, because that is what the loop dispatched on. `"error"` on every failed
    /// call, on either transport and for a hook's `run-inference`. Otherwise absent means no
    /// driver response was parsed at all: a hook's `run-inference` and the `process` transport
    /// both name their `decision` locally and never saw a provider stop reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
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
    /// The part of the output the provider or harness reports as reasoning — a subset of
    /// `output_tokens`, never an addition to it. Absent when nothing reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_tokens: Option<u64>,
    /// Where this inference came from. Absent for an ordinary agent-loop turn,
    /// so every pre-existing consumer sees a byte-identical record; `"hook:<name>"`
    /// for a completion that hook ran through `murmur:runtime/inference`'s
    /// `run-inference`.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    /// The driver choice that served an agent-loop turn, by name. Written only for a capsule that
    /// declares `inference.alternates`, beside `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_choice: Option<String>,
    /// Model string actually sent for this call (the attempted model, on failure). Written
    /// alongside `origin` for a hook's completion, and alongside `driver_choice` for an agent-loop
    /// turn of a capsule that declares alternates. Any other turn's model is the session-start
    /// record's and is not repeated per turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The output cap this request carried as `max_tokens`, which is also what the spend meter
    /// reserved for it. Written on every `transport: http` line — agent-loop turn, hook
    /// `run-inference` and failed call alike — and on no `transport: process` line, where the
    /// harness runs the model and the runtime sets no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    /// The call's input budget, `min(input ceiling, context window − max_output_tokens)`.
    /// Present exactly when `max_output_tokens` is; `null` there means the model has no window.
    /// The outer `Option` is the key, the inner one its value.
    #[serde(skip_serializing_if = "Option::is_none")]
    input_budget: Option<Option<u32>>,
    /// The `MURMUR_INFERENCE_DRIVER_CONFIG` this call's driver ran with, every credential value
    /// the session holds replaced with `[redacted]`. Present exactly when `max_output_tokens` is;
    /// `null` there means the driver saw no config. The outer `Option` is the key, the inner one
    /// its value.
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_config: Option<Option<Value>>,
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
    /// Why the call failed, from the `INFERENCE_ERROR_*` vocabulary. Present exactly on a record
    /// whose `decision` and `stop_reason` are both `"error"`; absent on every call that returned
    /// an answer, however unusable the loop then found it.
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<String>,
    /// The failure in words, present exactly when `error_code` is. Every credential value the
    /// session holds is already replaced with `[redacted]`, and the text is capped at
    /// [`MAX_TASK_FAILURE_REASON_BYTES`] on a character boundary.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The HTTP status the runtime's credential gateway received for the failed call. Absent when
    /// no request reached the provider, on the `process` transport, and for a driver whose
    /// gateway the runtime does not own.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_status: Option<u16>,
    /// Milliseconds from the start of the attempt this record describes until the driver's
    /// request reached the runtime's credential gateway. Present exactly on a call made through
    /// a runtime-owned gateway; `null` there means no request reached it. The outer `Option` is
    /// the key, the inner one its value.
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway_reached_ms: Option<Option<u64>>,
    /// Milliseconds from the start of the attempt until the provider's response head arrived at
    /// the gateway, the last one when the gateway resent. Present exactly when
    /// `gateway_reached_ms` is; `null` there means no response head arrived.
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_responded_ms: Option<Option<u64>>,
    /// How many attempts the call took, written only when it took more than one. Each retry
    /// before the last attempt has its own `inference_retry` line.
    #[serde(skip_serializing_if = "Option::is_none")]
    attempts: Option<u32>,
    /// Why a call whose last attempt failed transiently was not made again:
    /// [`crate::inference_retry::RetryStop::wire_name`]. Absent on every other record.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_stopped: Option<&'static str>,
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
    /// `null` when the provider named none, which is how every host-synthesized call reads.
    tool_call_id: Option<String>,
    input: Value,
    input_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
    /// The runtime's own text when it could not run the call, such as a native tool or shell
    /// command whose sandbox setup failed before exec. Written under every [`TraceCapture`],
    /// unlike `output`; peer handle tokens are redacted and the text is capped at
    /// [`MAX_TASK_FAILURE_REASON_BYTES`] on a character boundary. Absent when the tool ran,
    /// including when it reported its own failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
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
    /// The images the call returned, accepted or refused, in the order it named them. Absent
    /// when it returned none. Never holds image bytes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentRecord>,
}

/// One image a tool call returned or a task message carried, as the trace records it.
///
/// An accepted image carries `bytes` and `sha256`, the name of its blob beside the conversation;
/// a refused one carries `refused`, the refusal code its note ends with. `path` is the work-folder
/// path a tool named, or `message.parts[<i>]` for an image an A2A message carried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AttachmentRecord {
    pub(crate) path: String,
    pub(crate) media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refused: Option<&'static str>,
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
    /// The skill's `murmur.lock` origin: `"operator"` or `"runtime"`. Always written.
    origin: &'static str,
    /// `"trusted"` for an operator pin, `"untrusted"` for a runtime one, whose guidance reached
    /// the model fenced. Always written, in the spelling `task_start.trust` uses.
    trust: &'static str,
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
    /// Absent when the session declares no grace, so the command was demoted by an abandoned
    /// call or a cancel.
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_ms: Option<u64>,
}

/// A detached command's completion reaching the agent, written once per work id by whichever path
/// delivered it: [`ResourceTraceAppender::write_shell_completed`] right after the
/// `background_command` line of the call that returned the finished state, or
/// [`TraceWriter::write_shell_completed`] where the task loop queues it as a task. Hangs off the
/// session rather than a turn: the turn that started the command may be long over.
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
    /// Epoch milliseconds at which the runtime recorded the command's exit, which is never after
    /// [`Self::timestamp`].
    exited_at_ms: u64,
    /// Absolute path of the file holding the command's full stdout and stderr,
    /// `logs/<work_id>.log` in the session's records directory.
    output_path: String,
    output_bytes: u64,
    /// Attributed the same way [`ShellEvent::resource_limit`] is, and omitted on the same terms.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_limit: Option<String>,
    /// `"ok"` or `"error"` — the field that tells a failed background command from a successful
    /// one without reading its prose.
    status: String,
    /// [`crate::detached::Delivery::as_str`]: `"background_command"` or `"task"`.
    delivered_via: &'static str,
    /// The `completion`-origin task this completion was enqueued as, which is what makes "which
    /// command produced this task" answerable from `trace.jsonl` alone. Present exactly when
    /// [`Self::delivered_via`] is `"task"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_task_id: Option<String>,
}

impl ShellCompletedEvent {
    fn new(
        session_id: String,
        parent_id: Option<String>,
        completion: &crate::detached::DetachedCompletion,
        delivery: crate::detached::Delivery,
        completion_task_id: Option<&str>,
    ) -> Self {
        Self {
            event_type: "shell_completed",
            event_id: new_event_id(),
            parent_id,
            session_id,
            timestamp: timestamp_ms(),
            work_id: completion.work_id.clone(),
            binary: completion.binary.clone(),
            command: completion.command.clone(),
            exit_code: completion.exit_code,
            duration_ms: completion.duration_ms,
            exited_at_ms: completion.exited_at_ms,
            output_path: completion.output_path.clone(),
            output_bytes: completion.output_bytes,
            resource_limit: completion.resource_limit.clone(),
            status: completion.status().to_string(),
            delivered_via: delivery.as_str(),
            completion_task_id: completion_task_id.map(str::to_string),
        }
    }
}

/// One answered `background-command` call. Hangs off the session, as `session_status` does.
#[derive(Serialize)]
struct BackgroundCommandEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The task that made the call. Absent outside a task.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    work_id: &'a str,
    /// `status`, `wait` or `stop`.
    operation: &'static str,
    /// `running`, `exited` or `stopped`, as the call returned it.
    state: &'static str,
    /// Present once the command has exited.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// How long a `wait` blocked. Absent for the other operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    waited_ms: Option<u64>,
    /// Bytes in the log's output section, as the call returned it.
    output_bytes: u64,
}

/// A detached command the session ended without carrying its result back. No task will ever report
/// it; this record and the operator report beside it are what keep the loss visible.
///
/// The command was either still running at the sweep or finished during teardown, and the second
/// case knows more: [`Self::exit_code`], [`Self::output_path`] and [`Self::output_bytes`] are
/// written together for it and omitted together for the first. A command still running therefore
/// produces a line byte-identical in shape to one written before those fields existed — omitted,
/// never `null`, because `null` would read as a known-absent exit code rather than an unknown one.
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
    /// How long the command had been running when the session gave up on it. For a command that
    /// finished during teardown, its full duration from spawn to exit.
    running_ms: u64,
    /// `128 + signal` for a signal kill, as on `shell_completed`. Absent for a command still
    /// running.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// Absolute path of the file holding the command's full stdout and stderr,
    /// `logs/<work_id>.log` in the session's records directory. Absent for a command still
    /// running, for which that file was never written and never will be.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_path: Option<String>,
    /// Size of that file. Present exactly when [`Self::output_path`] is.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_bytes: Option<u64>,
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

/// A tool call the driver attached safety checks to, and how they ended. Written once per such
/// call, after the person answered (or could not be asked) and, for an acknowledged call, before
/// its `tool_call` record. The person's reply is never written.
#[derive(Serialize)]
struct SafetyCheckEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    tool_call_id: String,
    tool: String,
    /// The block's `safety_checks` exactly as the driver sent it, a malformed one included.
    checks: serde_json::Value,
    /// `acknowledged`, `declined`, `timed_out`, `canceled`, `no_input_channel` or `malformed`.
    outcome: &'static str,
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
    /// The occupancy the trigger compared: the provider's count of the request and its
    /// completion, or the estimate when the call reported none.
    tokens_before: u64,
    /// Which kind of number `tokens_before` is.
    tokens_before_source: OccupancyBasis,
    /// The estimate of the committed context. No provider has counted it yet; the next
    /// `inference` line carries the first count.
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
    /// Which kind of number `tokens` is.
    tokens_source: OccupancyBasis,
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
    /// Who the door let the task in as: [`RECEIVED_FROM_OPERATOR`], [`RECEIVED_FROM_MEMBER`],
    /// [`RECEIVED_FROM_CREDENTIAL`] or [`RECEIVED_FROM_PUBLIC`].
    caller: &'static str,
    /// The formation member that called, present exactly with `caller: "member"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_member: Option<String>,
    /// The declared credential whose token let the task in, present exactly with
    /// `caller: "credential"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    caller_credential: Option<String>,
    /// The `sha256` of the task's message text, omitted under `capture: none`. Under `content`
    /// the text is stored as the blob of that name.
    #[serde(skip_serializing_if = "Option::is_none")]
    message_sha256: Option<String>,
}

/// `a2a_task_received.caller` for a task the operator token let in.
pub(crate) const RECEIVED_FROM_OPERATOR: &str = "operator";
/// `a2a_task_received.caller` for a task a formation token let in.
pub(crate) const RECEIVED_FROM_MEMBER: &str = "member";
/// `a2a_task_received.caller` for a task a declared credential's token let in.
pub(crate) const RECEIVED_FROM_CREDENTIAL: &str = "credential";
/// `a2a_task_received.caller` for a task a door with no `network.authentication` let in.
pub(crate) const RECEIVED_FROM_PUBLIC: &str = "public";

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

/// A successful pull, by `manage.pull()` or `pull-artifact`: the artifact a running capsule
/// fetched and pinned in `murmur.lock`. A refused or failed pull writes nothing.
#[derive(Serialize)]
struct ArtifactPulledEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    name: String,
    version: String,
    /// `"skill"` or `"tool"`, the only roles a pull installs.
    runtime: &'static str,
    /// The origin the lock entry holds after the pull: `"runtime"`, or `"operator"` when the
    /// operator had already pinned the same version.
    origin: &'static str,
    /// The session the lock entry names as the puller — this one, unless an earlier pull of the
    /// same pin recorded another. `null` for an `"operator"` entry.
    session: Option<String>,
    trust: &'static str,
    /// Where the payload came from: `"project store"`, `"global store"` or
    /// `"github:<owner>/<repo>"`.
    fetched_from: String,
    /// GitHub's numeric id of the scope's owner, written to the lock entry beside the pin.
    /// Absent when neither the source nor the stored payload records one.
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_id: Option<u64>,
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
    message_parts_bytes: u64,
    /// The kind of each part of the A2A message the task arrived as, `text`, `data` or `raw`, in
    /// order. Absent for a task that did not arrive over the door.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    part_kinds: Vec<&'static str>,
    /// The message's `referenceTaskIds`, each once. Absent when it referenced none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reference_task_ids: Vec<String>,
    /// The images the message's `raw` parts carried, each with its `path` as
    /// `message.parts[<i>]`. Absent when it carried none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<AttachmentRecord>,
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
    /// How many of this task's agent-loop turns stopped at the `inference.max_tokens` output
    /// cap. Always present; `0` when none did.
    capped_turns: u32,
    /// The work ids of the background shell commands still running when the task ended, oldest
    /// first. Absent when there were none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    running_commands: Vec<String>,
    /// How `lifecycle.keep_working` went for this task. Absent when the capsule does not set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    keep_working: Option<KeepWorkingSummary>,
    /// How `lifecycle.done_check` went for this task. Absent when the capsule does not set it.
    #[serde(skip_serializing_if = "Option::is_none")]
    done_check: Option<DoneCheckSummary>,
}

/// `task_end.done_check`: the runs of `lifecycle.done_check`'s command for this task, the reopens
/// they caused and whether the last run passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct DoneCheckSummary {
    /// Runs, across reopens.
    pub(crate) runs: u32,
    /// Reopens the failing runs caused.
    pub(crate) reopens: u32,
    /// `lifecycle.done_check.limit`.
    pub(crate) limit: u32,
    /// Whether the last run passed: `null` when the check never ran, `false` after a run a cancel
    /// or the session deadline stopped.
    pub(crate) met: Option<bool>,
    /// What ended the task with the check unmet: `limit`, `max_turns`, `spend_ceiling` or
    /// `deadline`. `null` otherwise.
    pub(crate) stopped_by: Option<&'static str>,
}

/// `task_end.keep_working`: the continuations `lifecycle.keep_working` sent this task and what
/// stopped them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct KeepWorkingSummary {
    /// Continuations sent, across reopens.
    pub(crate) used: u32,
    /// `lifecycle.keep_working.limit`.
    pub(crate) limit: u32,
    /// What stopped the task being kept working: `limit`, `max_turns`, `spend_ceiling`,
    /// `deadline` or `no_session`. `null` when it ended on anything else — a failure, a cancel, a
    /// turn end that handed off work.
    pub(crate) stopped_by: Option<&'static str>,
}

/// A person stopped this task. Written where the loop actually stopped, so the record names the
/// wait that was interrupted and everything that was still running at that moment.
///
/// Distinct from the `task_end` that follows it: `task_end` says the task is over and with which
/// `exit_status`, this says a person made that happen and what it left behind. A task cancelled
/// while still queued has no `task_start` and gets no `task_end`, so this is its only record.
#[derive(Serialize)]
struct TaskCanceledEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    /// The turn that was in flight, 0-based. Absent for a task cancelled before it ever ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<u32>,
    /// Where cancellation landed: `queued`, `turn`, `inference`, `input`, `delegation` or
    /// `harness`.
    phase: String,
    /// Demoted shell commands still running when the loop stopped. They keep their own
    /// lifecycle — nothing here was killed.
    detached_work_ids: Vec<String>,
    /// Delegations in flight when the loop stopped, named before the cancelled task ends them.
    delegation_ids: Vec<String>,
}

/// `task_rejected.cause` when the session ended on its own — close-out, `after_task: exit`, a
/// failed launch task or the idle timeout — with the task still queued.
pub(crate) const TASK_REJECTED_SESSION_ENDED: &str = "session_ended";

/// `task_rejected.cause` when `mur stop` (`SIGTERM`) ended the session with the task still queued.
pub(crate) const TASK_REJECTED_SESSION_STOPPED: &str = "session_stopped";

/// The session stopped taking work while this task was still queued, so it was refused rather
/// than run. Written once per refused task, after the task loop has ended and before
/// `session_end`.
///
/// A refused task never started, so it has no `task_start`, no `task_end` and never a
/// `task_failed`: this is its only record. It hangs off the session node.
#[derive(Serialize)]
struct TaskRejectedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    /// The `IncomingTask::source` the task was enqueued with: `a2a`, `detached_shell` or
    /// `detached_lost`.
    source: String,
    /// One of the `TASK_REJECTED_*` constants.
    cause: String,
    /// The `status.message` the task's final `rejected` frame carries.
    reason: String,
}

/// `task_failed.cause` when the driver returned an error status or a response whose
/// `stop_reason` is `"error"`, and no credential rejection is pending.
pub(crate) const TASK_FAILED_DRIVER_ERROR: &str = "driver_error";

/// `task_failed.cause` for a driver error while the gateway holds a credential rejection: the
/// provider keeps refusing the key, and the reason is the `E-RUN-027` message.
pub(crate) const TASK_FAILED_CREDENTIAL_REJECTED: &str = "credential_rejected";

/// `task_failed.cause` for a response the loop cannot act on: `stop_reason: "tool_call"` with no
/// `tool_call` block, or a `stop_reason` that is missing or unsupported.
pub(crate) const TASK_FAILED_MALFORMED_RESPONSE: &str = "malformed_response";

/// `task_failed.cause` when a bound compaction hook returned `Err`.
pub(crate) const TASK_FAILED_COMPACTION_HOOK: &str = "compaction_hook";

/// `task_failed.cause` when a `request-input` wait passed `lifecycle.input_timeout_secs`.
pub(crate) const TASK_FAILED_INPUT_TIMEOUT: &str = "input_timeout";

/// `task_failed.cause` when an `on-task-end` hook still asked to reopen and
/// `lifecycle.max_task_reopens` or `inference.max_turns` refused it.
pub(crate) const TASK_FAILED_REOPEN_BUDGET_EXHAUSTED: &str = "reopen_budget_exhausted";

/// `task_failed.cause` for an attempt that ended in a runtime error no other cause names. The
/// reason is the error's own text.
pub(crate) const TASK_FAILED_RUNTIME_ERROR: &str = "runtime_error";

/// `task_failed.cause` when two agent-loop turns in a row stopped at the `inference.max_tokens`
/// output cap with no text and no tool call. The reason names both turns and the cap.
pub(crate) const TASK_FAILED_OUTPUT_CAP: &str = "output_cap";

/// `task_failed.cause` for a task ended because a runtime write, append, read, create or removal
/// inside the workdir was refused. Always preceded by one `workdir_path_refused` line per refusal.
pub(crate) const TASK_FAILED_WORKDIR_PATH_REFUSED: &str = "workdir_path_refused";

/// `inference.error_code` when the provider refused the inference credential: it answered `401`
/// and the gateway's re-read and resend were refused too. The record's `error` is the runtime's
/// own `E-RUN-027` message, never the provider's body.
pub(crate) const INFERENCE_ERROR_CREDENTIAL_REJECTED: &str = TASK_FAILED_CREDENTIAL_REJECTED;

/// `inference.error_code` when the driver reported the call failed and no credential rejection is
/// pending: a provider HTTP error, the driver's own error, a gateway refusal surfaced to the
/// driver, or a response whose `stop_reason` is `"error"`.
pub(crate) const INFERENCE_ERROR_DRIVER_ERROR: &str = TASK_FAILED_DRIVER_ERROR;

/// `inference.error_code` when the driver component could not be invoked or trapped:
/// instantiation or import failure, the execution deadline, or a memory limit.
pub(crate) const INFERENCE_ERROR_DRIVER_FAILED: &str = "driver_failed";

/// `inference.error_code` when the driver reported success but returned no body, or a body that
/// is not JSON.
pub(crate) const INFERENCE_ERROR_MALFORMED_RESPONSE: &str = TASK_FAILED_MALFORMED_RESPONSE;

/// `inference.error_code` when a `process` harness reported a `turn-failed` of kind `auth`.
pub(crate) const INFERENCE_ERROR_HARNESS_AUTH: &str = "harness_auth";

/// `inference.error_code` when a `process` harness reported a `turn-failed` of kind `quota`.
pub(crate) const INFERENCE_ERROR_HARNESS_QUOTA: &str = "harness_quota";

/// `inference.error_code` when a `process` harness reported a `turn-failed` of kind
/// `harness-error`.
pub(crate) const INFERENCE_ERROR_HARNESS_ERROR: &str = "harness_error";

/// `inference.error_code` when a `process` harness reported a `turn-failed` of kind `other`.
pub(crate) const INFERENCE_ERROR_HARNESS_OTHER: &str = "harness_other";

/// `inference.error_code` when a `process` harness went silent past the inactivity timeout.
pub(crate) const INFERENCE_ERROR_HARNESS_INACTIVE: &str = "harness_inactive";

/// One model call that failed, as [`TraceWriter::write_failed_inference`] records it.
pub(crate) struct FailedInference<'a> {
    pub(crate) turn: u32,
    /// One of the `INFERENCE_ERROR_*` constants.
    pub(crate) error_code: &'static str,
    /// The failure in words, already redacted. Capped by the writer.
    pub(crate) error: &'a str,
    /// The HTTP status the credential gateway received for this call, when it received one.
    pub(crate) provider_status: Option<u16>,
    /// What the provider itself reported, when the failure carried a `usage` block.
    pub(crate) usage: Option<&'a DriverUsage>,
    /// `Some` for a hook's `run-inference`; `None` for an agent-loop call.
    pub(crate) origin: Option<&'a InferenceOrigin>,
    /// The driver choice the call was dispatched to, on a capsule that declares alternates.
    pub(crate) choice: Option<&'a crate::driver_choice::DriverChoice>,
    /// The messages the failed request embedded.
    pub(crate) message_ids: Vec<String>,
    pub(crate) tool_name: Option<String>,
    /// `(input_tokens, output_tokens)` this call counts toward the turn and token totals. `None`
    /// is a call that is not counted at all — no turn, no tokens, and neither key on the record.
    pub(crate) spent: Option<(Option<u64>, Option<u64>)>,
    /// How the call was sent; `None` on the `process` transport.
    pub(crate) settings: Option<&'a InferenceCallSettings>,
    /// The call's attempts and why its retrying stopped, as its retry loop ended.
    pub(crate) retry: RetryNote,
    /// The last attempt's gateway timing; `None` for a call made through no runtime gateway.
    pub(crate) gateway: Option<GatewayOffsets>,
}

/// One `inference` line, as the writers that share [`TraceWriter::write_inference_event`] hand
/// it over.
struct InferenceLine<'a> {
    turn: u32,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    decision: String,
    stop_reason: Option<&'a str>,
    tool_name: Option<String>,
    origin: Option<&'a InferenceOrigin>,
    usage: Option<&'a DriverUsage>,
    message_ids: Vec<String>,
    wire: Option<&'a WireCapture>,
    choice: Option<&'a crate::driver_choice::DriverChoice>,
    /// `(error_code, error, provider_status)` for a failed call.
    failure: Option<(&'static str, String, Option<u16>)>,
    /// Whether the line advances the turn and token counters.
    counted: bool,
    /// How the call was sent; `None` on the `process` transport, which writes neither key.
    settings: Option<&'a InferenceCallSettings>,
    retry: RetryNote,
    /// When the attempt's request reached the runtime's credential gateway and was answered;
    /// `None` for a call made through no runtime gateway, which writes neither key.
    gateway: Option<GatewayOffsets>,
}

/// The most `task_failed.reason` carries, in bytes. A driver's error text is provider-supplied
/// and unbounded; the whole of it stays in `out/result.txt`.
pub(crate) const MAX_TASK_FAILURE_REASON_BYTES: usize = 2_000;

/// One runtime write, append, read, create or removal inside the workdir that was refused and did
/// nothing. Written whatever `trace.capture` is; parented to the
/// task node in a task and to the session node outside one.
#[derive(Serialize)]
struct WorkdirPathRefusedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// The path the runtime meant to touch, relative to the accessible workdir.
    path: &'a str,
    operation: crate::workdir_fs::WorkdirOperation,
    /// The component that was refused, relative to the accessible workdir; the whole path for an
    /// invalid component.
    refused_at: &'a str,
    reason: crate::workdir_fs::RefusalReason,
}

/// A task attempt failed, and why. Written once per failing attempt, before its task's
/// `task_end`, by whichever part of the runtime saw the failure.
///
/// `task_end.exit_status` says `failed`; this names the cause from a closed vocabulary (the
/// `TASK_FAILED_*` constants) and carries the reason in words. Never written for a task that
/// ended `ok`, `max_turns_reached`, `spend_ceiling_reached` or `canceled`: those have their own
/// records.
#[derive(Serialize)]
struct TaskFailedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The task in progress. Absent for a failure recorded before any `task_start`.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// The turn that failed, 0-based. Absent when the failure is the task's rather than one
    /// turn's.
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<u32>,
    cause: String,
    /// Capped at [`MAX_TASK_FAILURE_REASON_BYTES`] on a character boundary.
    reason: String,
}

/// `task_reopened.attempt_context` for a next attempt that continues the task's own
/// conversation, with the hook's feedback as the one message it adds.
pub(crate) const REOPEN_CONTEXT_CONTINUED: &str = "continued";

/// `task_reopened.attempt_context` for a next attempt that starts from a fresh context built
/// from the rewritten `task.md`, because the previous attempt left no conversation to continue.
pub(crate) const REOPEN_CONTEXT_RESTARTED: &str = "restarted";

/// One `on-task-end` hook reopened the task: its agent loop is about to run again with
/// the hook's feedback. Written between two agent-loop attempts for the same task, so
/// `mur trace show` can show which hook drove each reopen, why, and what the next attempt
/// starts from.
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
    /// [`REOPEN_CONTEXT_CONTINUED`] or [`REOPEN_CONTEXT_RESTARTED`].
    attempt_context: &'static str,
    /// The turns the next attempt is handed: `inference.max_turns` less every turn the task's
    /// attempts have spent so far. `null` when
    /// `inference.max_turns` sets no cap.
    turns_remaining: Option<u32>,
}

/// A task continued once with everything it had handed off: its attempt ended with a turn left,
/// it waited until no `call-member` call and no `delegate-task` delegation was outstanding, and
/// the next attempt is handed every answer and outcome in one message. Written after that round's
/// `member_call` and `delegation` lines and before the continued attempt's first `inference`.
#[derive(Serialize)]
struct TaskContinuedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    /// 1-based ordinal of this continuation within the task, counted across reopens.
    continuation_number: u32,
    /// The `mcl_` id of every call whose outcome this continuation delivers, in arrival order.
    member_calls: &'a [String],
    /// The `dlg_` id of every delegation whose outcome this continuation delivers, arrived or
    /// ended by the backstop, in id order.
    delegations: &'a [String],
    /// How long the task waited, from its attempt's end until nothing was outstanding.
    waited_ms: u64,
    /// The turns the continued attempt is handed: `inference.max_turns` less every turn the
    /// task's attempts have spent so far. `null` when
    /// `inference.max_turns` sets no cap.
    turns_remaining: Option<u32>,
}

/// `lifecycle.keep_working` sent its message after the task's attempt ended at a turn end. Written
/// after that turn's `inference` and before the continued attempt's first one.
#[derive(Serialize)]
struct KeepWorkingEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    /// 1-based ordinal of this continuation within the task, counted across reopens.
    number: u32,
    /// `lifecycle.keep_working.limit`.
    limit: u32,
    /// The turns the continued attempt is handed: `inference.max_turns` less every turn the
    /// task's attempts have spent so far. `null` when `inference.max_turns` sets no cap.
    turns_remaining: Option<u32>,
    /// The byte length of the message sent.
    message_bytes: u64,
}

/// One run of `lifecycle.done_check`'s command at a turn end. Written after that turn's
/// `inference` and any `keep_working` record, before the next attempt's first `inference` or the
/// task's `task_end`.
#[derive(Serialize)]
struct DoneCheckEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    #[serde(flatten)]
    run: &'a DoneCheckRun<'a>,
}

/// The fields of one `done_check` record that describe the run.
#[derive(Debug, Serialize)]
pub(crate) struct DoneCheckRun<'a> {
    pub(crate) task_id: &'a str,
    /// 1-based ordinal of this run within the task, counted across reopens.
    pub(crate) number: u32,
    /// `lifecycle.done_check.command` as declared.
    pub(crate) command: &'a str,
    /// `passed`, `failed`, `timed_out`, `stopped` or `not_started`.
    pub(crate) outcome: &'static str,
    /// `null` for `timed_out`, `stopped` and `not_started`.
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: u64,
    /// Bytes returned after the 16 KiB-per-stream cap.
    pub(crate) stdout_bytes: u64,
    pub(crate) stderr_bytes: u64,
    pub(crate) truncated: bool,
    /// The `capabilities.resources` field whose limit killed the command.
    pub(crate) resource_limit: Option<&'a str>,
    /// Whether this run reopened the task.
    pub(crate) reopened: bool,
    /// The reopens the check may still cause in this task, after this run.
    pub(crate) reopens_left: u32,
    /// `inference.max_turns` less every turn the task has spent. `null` with no cap.
    pub(crate) turns_remaining: Option<u32>,
}

/// A component call the wall-clock bound on `capabilities.limits.deadline_seconds` stopped while
/// it waited on the host. Written once per firing, before the record of the call it cut and
/// before `session_end`; never for an epoch trap.
#[derive(Serialize)]
struct CallDeadlineEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// [`crate::limits::CallDeadline::component`].
    component: String,
    /// [`crate::limits::ComponentRole::wire_name`].
    role: &'static str,
    /// [`crate::limits::CallDeadline::call`].
    call: String,
    deadline_seconds: u64,
    /// How long the stopped phase had run, from the start of that phase.
    elapsed_ms: u64,
}

/// A driver call failed transiently and is about to be made again. The call's one `inference`
/// line follows its last attempt.
#[derive(Serialize)]
struct InferenceRetryEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    attempt: u32,
    max_retries: u32,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_status: Option<u16>,
    error: &'a str,
    delay_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    charged_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'a str>,
}

/// A spend ceiling refused a driver call before it was sent. No `inference` line accompanies it,
/// because no call was made.
#[derive(Serialize)]
struct SpendCeilingReachedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    #[serde(flatten)]
    refusal: &'a crate::spend::SpendRefusal,
    /// `hook:<name>` for a hook's `run-inference` call; absent for an agent-loop turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'a str>,
}

/// What a `transport: process` session spawned, and what the driver planned for it.
///
/// Separate from [`HarnessStartEvent`] so the runner builds one value rather than passing a dozen
/// positional arguments. Flattened into the event, so the record has no nested object.
#[derive(Serialize)]
pub(crate) struct HarnessStart {
    /// The process driver artifact and its version.
    pub driver: String,
    pub driver_version: String,
    /// What the driver said it drives, and the executable actually spawned.
    pub harness: String,
    /// The absolute path spawned — not the name the manifest or the driver asked for.
    pub binary: String,
    /// `inference.command`, or the process driver's `describe()`.
    pub binary_source: String,
    /// What the harness printed for its version arguments, or `null` when it could not be read.
    pub harness_version: Option<String>,
    /// Whether [`Self::harness_version`] matched one of the driver's `tested-versions`.
    pub version_tested: bool,
    /// The harness session id the runtime minted for this run, and whether it started or resumed.
    pub harness_session_id: String,
    pub session_mode: String,
    /// How many arguments the plan carried. Never the arguments themselves: a driver is free to
    /// put the bridge's bearer token in one.
    pub args_count: usize,
    /// The names of the variables actually delivered, sorted. Never their values.
    pub env_names: Vec<String>,
    /// Which of `USER` and `LOGNAME` the host did not set, sorted; empty when it set both. Never
    /// their values.
    pub account_env_unset: Vec<String>,
    /// The names of the files written into the run's private directory. Never their contents.
    pub files: Vec<String>,
    /// The capsule tool names the bridge offered, bare. Empty when no bridge was bound.
    pub bridge_tools: Vec<String>,
    /// How many bytes were written to the harness's stdin, and whether it stayed open after.
    pub stdin_bytes: usize,
    pub keep_stdin_open: bool,
    /// SHA-256 (lowercase hex) of the exact system prompt handed to the process driver: the
    /// fact lines, the resolved prompt and any instruction-file blocks. Unlike
    /// `session_start.system_prompt_sha256`, which names the prompt the operator wrote.
    pub system_prompt_sha256: String,
    /// The text [`Self::system_prompt_sha256`] hashes, stored as that blob under
    /// `trace.capture: content` and never written to the line.
    #[serde(skip)]
    pub system_prompt: String,
}

#[derive(Serialize)]
struct HarnessStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    #[serde(flatten)]
    start: HarnessStart,
}

/// A `W-*` warning the run raised. The same text goes to stderr; this is the durable copy, so a
/// run read back later still says what it warned about.
#[derive(Serialize)]
struct HarnessWarningEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    code: String,
    message: String,
}

/// The session the harness itself reports, which is not this session: the runtime's `session_id`
/// names the capsule run, `harness_session_id` names the conversation inside the harness.
#[derive(Serialize)]
struct HarnessSessionEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    harness_session_id: String,
    /// How the harness says it is billing. Anything but `subscription` is also warned about.
    auth: String,
    /// The model the harness says it is running, when it says.
    model: Option<String>,
}

/// The harness session a person asked this capsule to drop, recorded because forgetting is not
/// the same as never having known: the turn that follows starts a new conversation under a context
/// that had one, and this is what says so.
#[derive(Serialize)]
struct HarnessSessionForgottenEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    /// The context whose entry was dropped.
    context_id: String,
    /// The id that was dropped, which the harness is no longer asked to continue.
    harness_session_id: String,
    /// Which surface carried the request: `cli` or `a2a`.
    requested_by: String,
}

#[derive(Serialize)]
struct HarnessRetryEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    attempt: u32,
    reason: String,
}

/// A task-node record whose payload is one line of text: `harness_note` and
/// `harness_suggestion`.
#[derive(Serialize)]
struct HarnessTextEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    text: String,
}

/// One subscription window the harness reported, as `harness_quota` records it. A member the
/// harness did not report is left out, never written as zero or `null`.
#[derive(Serialize)]
struct HarnessQuotaWindow<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resets_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window_minutes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
}

#[derive(Serialize)]
struct HarnessQuotaEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    windows: Vec<HarnessQuotaWindow<'a>>,
}

#[derive(Serialize)]
struct HarnessFailedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    /// The driver's `failure-kind`, spelled as the WIT spells it, or `max-turns` from the runtime.
    kind: String,
    message: String,
    /// `harness` when the harness reported it, `runtime` when this runtime ended the turn.
    source: String,
}

/// How the runtime tried to stop a harness a person cancelled the task of.
#[derive(Serialize)]
struct HarnessInterruptEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    /// The driver's `interrupt-method`, spelled as the WIT spells it.
    method: String,
    /// Whether a graceful interrupt actually reached the harness.
    delivered: bool,
    /// How long the harness has to end on its own before it is killed. `0` when it is killed at
    /// once, which is every ending where `delivered` is false.
    grace_ms: u64,
}

/// How one spawned harness ended. Written once per spawn, on every path out of the run.
#[derive(Serialize)]
pub(crate) struct HarnessExit {
    /// The process's exit code, or `null` when a signal ended it.
    pub code: Option<i32>,
    /// The signal that ended the process, or `null` when it exited on its own.
    pub signal: Option<i32>,
    /// What ended the run: `terminal`, `eof`, `inactivity`, `max_turns`, `driver_error` or
    /// `canceled`.
    pub cause: String,
    /// Whether the runtime had to kill the process rather than wait for it.
    pub killed: bool,
    /// Wall time from spawn to reaped.
    pub duration_ms: u64,
}

#[derive(Serialize)]
struct HarnessExitEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: Option<String>,
    #[serde(flatten)]
    exit: HarnessExit,
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

/// A tool call was refused before it ran because its input lacked a name the tool's
/// `input_schema` lists in `required`. Written in every capture mode: under the default `meta`
/// the accompanying `tool_call` record carries no `output`, so this is where the reason lands.
#[derive(Serialize)]
struct ToolInputRefusedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    tool_name: String,
    /// The provider's id for the call, or `null` where the call arrived without one — every
    /// call bridged from a `transport: process` harness.
    tool_call_id: Option<String>,
    /// The required names the input lacked, in the schema's declared order.
    missing: Vec<String>,
    /// The refusal text the model was handed.
    reason: String,
}

/// The manifest's own read-only rule refused a call before it ran. Distinct from
/// [`CallDeniedEvent`] because it names a different authority and a different subject: no hook was
/// asked, and what it carries is the path, the declared rule and the evidence that identified the
/// call as a write.
#[derive(Serialize)]
struct ProtectedPathDeniedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    /// Which dispatch path was refused: `"shell"` or `"tool"`.
    call: String,
    /// What was refused: the resolved executable path for a shell call, the tool name otherwise.
    target: String,
    /// The resolved workdir-relative path, never the string the model typed, so two spellings of
    /// one file produce one comparable record.
    path: String,
    /// The `capabilities.filesystem.read_only` entry that covers `path`, as declared.
    rule: String,
    /// What identified the call as a write — a redirection operator, a write-verb argument
    /// position, or the tool-input key pairing.
    signal: String,
    /// The same text the model was given, so the trace and the agent agree on why.
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

/// A formation member's lifeline read EOF: the formation it belongs to has ended, and this
/// session is winding down because of it. Written only when the lifeline is what began the
/// session's termination.
#[derive(Serialize)]
struct FormationEndedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    formation_id: String,
}

/// One `session-status` call: the object the model was handed, measured at the call.
#[derive(Serialize)]
struct SessionStatusEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The task that made the call. Absent outside a task.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    status: &'a crate::session_status::StatusReport,
}

/// The one deadline notice, appended to the tool result named here.
#[derive(Serialize)]
struct DeadlineNoticeEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The task whose tool result carried it. Absent outside a task.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    /// The tool whose result carried it.
    tool_name: &'a str,
    /// When the runtime ends the session, RFC 3339 UTC.
    deadline_at: &'a str,
    /// Whole seconds left when the result was handed back.
    left_secs: u64,
    /// The margin it was due under.
    notice_secs: u64,
}

/// The session reached its deadline, own or inherited, and is winding down because of it. Written
/// by the termination routine before any task is cancelled.
#[derive(Serialize)]
struct DeadlineReachedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// When the deadline fell, RFC 3339 UTC.
    deadline_at: String,
    /// `session_start.deadline.secs`.
    deadline_secs: u64,
    /// `SessionDeadline::source`: `"own"` or `"inherited"`.
    source: &'static str,
}

/// A delegated child's spawner lifeline read EOF: the process that spawned this session has
/// ended, and this session is winding down because of it. Written only when that lifeline is what
/// began the session's termination.
///
/// The lineage keys are the ones `session_start` carries, and like there they are absent rather
/// than null for a session launched with a lifeline but no `MURMUR_SPAWNER`.
#[derive(Serialize)]
struct SpawnerEndedEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    spawned_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delegation_id: Option<String>,
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

/// One launched child, written the moment it is up.
///
/// Two surfaces launch children — the agent's `delegate-task` tool and a plan's `capsule` step —
/// and both write this line, off the same struct and off the same session node. Which one made a
/// given delegation is not recorded, because it is not a fact about the delegation.
///
/// Separate from the `delegation` line below because the two answer different questions and land
/// at different times: this one reaches disk while the delegation is still in flight, so a child
/// that hangs, crashes or is ended at its deadline is attributable from the parent's side whatever
/// happens next. A delegation the daemon refused writes none of these — nothing was launched.
#[derive(Serialize)]
struct DelegationStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The `dlg_` id the launcher minted, character-identical to the one on this delegation's
    /// terminal `delegation` line and in the child's own `session_start`.
    delegation_id: String,
    capsule: String,
    version: String,
    /// The session id the child's runtime minted for itself.
    child_session_id: String,
    /// The child's directory, relative to this capsule's accessible workdir.
    child_workdir: String,
    /// The child's records directory, absolute, as the child reported it: its own `trace.jsonl`
    /// and `completion.json` are there.
    child_records_dir: String,
    /// What was copied from this capsule's work folder into the child's before it started, or
    /// `null` for a child not listed under `capabilities.spawn.workdir_snapshot`. Always
    /// serialized.
    workdir_snapshot: Option<crate::workdir_snapshot::WorkdirSnapshot>,
}

impl DelegationStartEvent {
    /// The one place this record is built, so the two appenders that write it cannot drift into
    /// two shapes. Which surface launched the child is not an argument, because it is not a field.
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: &str,
        session_event_id: &str,
        delegation_id: &str,
        capsule: &str,
        version: &str,
        child_session_id: &str,
        child_workdir: &str,
        child_records_dir: &str,
        workdir_snapshot: Option<&crate::workdir_snapshot::WorkdirSnapshot>,
    ) -> Self {
        Self {
            event_type: "delegation_start",
            event_id: new_event_id(),
            parent_id: Some(session_event_id.to_string()),
            session_id: session_id.to_string(),
            timestamp: timestamp_ms(),
            delegation_id: delegation_id.to_string(),
            capsule: capsule.to_string(),
            version: version.to_string(),
            child_session_id: child_session_id.to_string(),
            child_workdir: child_workdir.to_string(),
            child_records_dir: child_records_dir.to_string(),
            workdir_snapshot: workdir_snapshot.cloned(),
        }
    }
}

/// A `call-member` call whose callee holds the task, written as the tool call returns — or later,
/// by the call's watcher, when a callee that was busy takes the task.
///
/// Carries the callee's roster name and the id of the task its door holds, never its door's URL
/// or the token the call presented.
#[derive(Serialize)]
struct MemberCallStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The calling task.
    task_id: String,
    /// The `mcl_` id the call returned to the model.
    call_id: String,
    /// The callee's roster name.
    member: String,
    /// The task the callee's door holds, as its `GetTask` names it.
    member_task_id: String,
}

/// One offer of a `call-member` call that the callee's door turned away busy: it answered
/// `SendMessage` with `rejected` and [`crate::a2a::REJECTED_BUSY_MESSAGE`]. The first is written
/// as the tool call returns, each later one by the call's watcher as it offers the task again.
///
/// Carries the callee's roster name, never its door's URL or the token the call presented.
#[derive(Serialize)]
struct MemberCallBusyEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    /// The calling task.
    task_id: String,
    /// The `mcl_` id the call returned to the model.
    call_id: String,
    /// The callee's roster name.
    member: String,
    /// Which offer of the call this was, from 1.
    offer: u32,
    /// From the tool call that made the call to this refusal.
    waited_ms: u64,
    /// The door's status message.
    message: String,
}

/// One `call-member` call, written once it is accounted for: delivered to the calling task, or
/// left behind when that task ended.
///
/// A call that never started writes only this, with no `member_task_id`. Carries no URL and no
/// token.
#[derive(Serialize)]
struct MemberCallEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    task_id: String,
    call_id: String,
    member: String,
    /// Absent, not null, for a call the callee never held.
    #[serde(skip_serializing_if = "Option::is_none")]
    member_task_id: Option<String>,
    /// `completed`, `failed`, `canceled`, `rejected`, `timed_out`, `unreachable`, `abandoned` or
    /// `no_answer`.
    status: String,
    duration_ms: u64,
    /// The callee's answer, its status message, or why the call ended.
    output: String,
    truncated: bool,
    /// Whether the calling task received `output`: as the tool result for a call that never
    /// started, as a continuation for one that did.
    delivered: bool,
    /// The members further down that gave the callee no answer, as its `GetTask` reported them.
    /// Absent when it reported none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    no_answer_below: Vec<NoAnswerBelow>,
}

/// One member a callee reported as giving it no answer.
#[derive(Serialize)]
struct NoAnswerBelow {
    member: String,
    /// A `member_call` status other than `completed`.
    status: String,
}

/// One delegation, written when it ends.
///
/// One line per call whatever happened, including a call the daemon refused: a delegation that was
/// asked for and not made is as much a fact of the run as one that produced an answer. When it is
/// written depends on how far the delegation got — a `delegate-task` call that started a child
/// writes it as that child's completion arrives, out of the child's own `completion.json`; one that
/// never started a child writes it within the call. A plan `capsule` step waits for its child's
/// answer, so it always writes this line within the step. Carries neither the task text nor the
/// child's answer — both are the agent's own conversation, which the `tool_call` line for the
/// same call already records under the session's `trace.capture` setting.
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
    /// How long the child ran, for a delegation that started; how long the call took, for one that
    /// never started.
    duration_ms: u64,
    /// How the delegation ended. Which vocabulary it is drawn from depends on whether the
    /// delegating call waited for the answer itself:
    ///
    /// - A delegation whose outcome arrived later as a completion reads `ok`, `error`, `crashed`
    ///   or `terminated` from [`crate::delegation::DelegationStatus`], or `unknown` when the
    ///   completion left no readable file.
    /// - One the caller waited out reads `completed` or `timed_out` from
    ///   [`crate::delegation_plane::DelegationStatus`].
    /// - One that never started reads `failed` or `refused`, from that same enum, either way.
    outcome: String,
    /// `null` where nothing needs saying. Otherwise the completion's `detail`, or the same
    /// sentence the model was given.
    reason: Option<String>,
}

impl DelegationEvent {
    /// The one place this record is built, so the two appenders that write it cannot drift into
    /// two shapes. Which surface launched the child is not an argument, because it is not a field.
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: &str,
        session_event_id: &str,
        capsule: &str,
        version: &str,
        delegation_id: Option<String>,
        child_session_id: Option<String>,
        duration_ms: u64,
        outcome: &str,
        reason: Option<String>,
    ) -> Self {
        Self {
            event_type: "delegation",
            event_id: new_event_id(),
            parent_id: Some(session_event_id.to_string()),
            session_id: session_id.to_string(),
            timestamp: timestamp_ms(),
            capsule: capsule.to_string(),
            version: version.to_string(),
            delegation_id,
            child_session_id,
            duration_ms,
            outcome: outcome.to_string(),
            reason,
        }
    }
}

// ── TraceWriter impl ─────────────────────────────────────────────────────────

impl TraceWriter {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn open(
        records_root: &WorkdirRoot,
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
        spawned_by: Option<String>,
        spawn_delegation_id: Option<String>,
    ) -> std::io::Result<Self> {
        let file = File::from_std(records_root.open_append(TRACE_FILE_NAME, DEFAULT_FILE_MODE)?);
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
        let blobs = BlobStore::new(records_root);
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
            credential_source: "none",
            gateways: Vec::new(),
            control: None,
            inference_choices: Vec::new(),
            tool_refresh: None,
            runtime_artifacts: Vec::new(),
            host_facts: Vec::new(),
            instruction_files: None,
            resumed_from,
            context_id,
            spawned_by,
            spawn_delegation_id,
            formation_id: None,
            formation_member: None,
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
            task_agent_turns: 0,
            task_hook_inferences: 0,
            task_capped_turns: 0,
            task_start_instant: None,
            active_task_id: None,
            task_failures_written: 0,
            last_task_failure: None,
            max_session_tokens: None,
            machine_tokens_per_day: None,
            session_status: None,
            call_deadlines: None,
        })
    }

    /// The session's [`crate::limits::CallDeadlines`], which this writer drains into one
    /// `call_deadline` line per firing ahead of each line it writes, `session_end` included.
    pub(crate) fn set_call_deadlines(&mut self, deadlines: crate::limits::CallDeadlines) {
        self.call_deadlines = Some(deadlines);
    }

    /// The session's standing, which every later `inference`, `task_start` and `task_end` write
    /// feeds and whose deadline every later `session_start` records.
    pub(crate) fn set_session_status(
        &mut self,
        status: std::sync::Arc<crate::session_status::SessionStatus>,
    ) {
        self.session_status = Some(status);
    }

    /// The spend ceilings this session runs under, written into every later `session_start`.
    pub(crate) fn set_spend_ceilings(
        &mut self,
        max_session_tokens: Option<u64>,
        machine_tokens_per_day: Option<u64>,
    ) {
        self.max_session_tokens = max_session_tokens;
        self.machine_tokens_per_day = machine_tokens_per_day;
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
    /// it.
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

    /// Records where the inference credential came from, as
    /// [`crate::gateway_credential::CredentialSource::trace_name`] names it. `"none"` until set.
    pub(crate) fn set_credential_source(&mut self, source: &'static str) {
        self.credential_source = source;
    }

    /// Records the session's credential gateways for `session_start.gateways`.
    pub(crate) fn set_gateways(&mut self, gateways: Vec<SessionGateway>) {
        self.gateways = gateways;
    }

    /// Records what `control:` declares for `session_start.control`.
    pub(crate) fn set_control(&mut self, control: Option<SessionControl>) {
        self.control = control;
    }

    /// Records the driver choices for `session_start.inference_choices`. Left empty, the key is
    /// not written.
    pub(crate) fn set_inference_choices(&mut self, choices: Vec<SessionInferenceChoice>) {
        self.inference_choices = choices;
    }

    /// Records the runtime-origin entries of `installed` for `session_start.runtime_artifacts`,
    /// in `installed`'s order. Operator-declared entries are left out.
    pub(crate) fn set_runtime_artifacts(&mut self, installed: &[InstalledArtifactSummary]) {
        self.runtime_artifacts = installed
            .iter()
            .filter_map(|artifact| match &artifact.origin {
                LockOrigin::Runtime { session } => Some(RuntimeArtifact {
                    name: artifact.name.clone(),
                    version: artifact.version.clone(),
                    origin: artifact.origin.as_str(),
                    session: session.clone(),
                    trust: crate::origin::artifact_trust(&artifact.origin).as_str(),
                }),
                LockOrigin::Operator => None,
            })
            .collect();
    }

    /// Records `uses`, in order, for `session_start.host_facts`.
    pub(crate) fn set_host_facts(&mut self, uses: &[crate::host_facts::HostFactsUse]) {
        self.host_facts = uses.iter().map(HostFactsRecord::of).collect();
    }

    /// Records that this inference call is the first to use a setting value a controller set:
    /// `turn` is the call's zero-based turn, the same number its `inference` line carries, and
    /// `change_id` the `event_id` of the `control_change` that set `value`.
    ///
    /// Written before the call is sent, so it precedes the turn's `inference` line and hangs off
    /// the task.
    pub(crate) async fn write_control_applied(
        &mut self,
        turn: u32,
        name: &str,
        value: Value,
        change_id: &str,
    ) -> std::io::Result<()> {
        let event = ControlAppliedEvent {
            event_type: "control_applied",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            name,
            value,
            change_id,
        };
        self.write_event(&event).await
    }

    /// Records the formation this session is a member of for `session_start.formation_id`.
    pub(crate) fn set_formation_id(&mut self, formation_id: Option<&FormationId>) {
        self.formation_id = formation_id.cloned();
    }

    /// Records `session_start.formation_member` and `session_start.formation_callees` for a
    /// session its formation launcher handed credentials to.
    pub(crate) fn set_formation_member(
        &mut self,
        member: Option<&crate::formation_credentials::FormationMember>,
    ) {
        self.formation_member = member.map(|member| {
            (
                member.name().to_string(),
                member.callees().map(str::to_string).collect(),
            )
        });
    }

    /// Records what staging read for `inference.instruction_files` for
    /// `session_start.instruction_files`; `None` writes `null`.
    pub(crate) fn set_instruction_files(
        &mut self,
        files: Option<&crate::instruction_files::InstructionFiles>,
    ) {
        self.instruction_files =
            files.map(crate::instruction_files::InstructionFiles::trace_records);
    }

    /// Records `inference.tool_refresh` for `session_start.tool_refresh`: the wire name under
    /// `transport: http`, `None` under `transport: process`.
    pub(crate) fn set_tool_refresh(&mut self, tool_refresh: Option<&'static str>) {
        self.tool_refresh = tool_refresh;
    }

    /// Records that the tool array the http agent loop sends changed before this call:
    /// `trigger` is why (`"compaction"`, `"agent"` or `"immediate"`), `added` and `removed` the
    /// sorted names that entered and left it, and `tools` every name it now offers, in array
    /// order.
    ///
    /// Written before the call is sent, so it precedes the turn's `inference` line and hangs off
    /// the task, as `control_applied` does.
    pub(crate) async fn write_tools_refreshed(
        &mut self,
        turn: u32,
        trigger: &str,
        added: &[String],
        removed: &[String],
        tools: &[String],
    ) -> std::io::Result<()> {
        let event = ToolsRefreshedEvent {
            event_type: "tools_refreshed",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            trigger,
            added,
            removed,
            tools,
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_session_start(
        &mut self,
        max_turns: Option<u32>,
        model_limits: Option<crate::model_limits::ResolvedModelLimits>,
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
            model_limits,
            max_session_tokens: self.max_session_tokens,
            machine_tokens_per_day: self.machine_tokens_per_day,
            deadline: self.session_status.as_ref().and_then(|status| {
                status.deadline().map(|deadline| SessionStartDeadline {
                    at: deadline.at_rfc3339(),
                    secs: deadline.secs(),
                    notice_secs: status.notice_secs(),
                    source: deadline.source().as_str(),
                })
            }),
            capabilities: self.capabilities.clone(),
            tools_declared,
            containment_declared: self.effective_grants.declared_containment,
            containment_achieved: self.effective_grants.achieved_containment,
            workdir_exec: self.effective_grants.workdir_exec,
            userns_grant: self.effective_grants.userns_grant,
            effective_grants: self.effective_grants.clone(),
            system_prompt_source: self.system_prompt_source,
            system_prompt_sha256: self.system_prompt_sha256.clone(),
            credential_source: self.credential_source,
            gateways: self.gateways.clone(),
            control: self.control.clone(),
            inference_choices: self.inference_choices.clone(),
            tool_refresh: self.tool_refresh,
            runtime_artifacts: self.runtime_artifacts.clone(),
            host_facts: self.host_facts.clone(),
            instruction_files: self.instruction_files.clone(),
            resumed_from: self.resumed_from.clone(),
            context_id: self.context_id.clone(),
            spawned_by: self.spawned_by.clone(),
            delegation_id: self.spawn_delegation_id.clone(),
            formation_id: self.formation_id.clone(),
            formation_member: self.formation_member.as_ref().map(|(name, _)| name.clone()),
            formation_callees: self
                .formation_member
                .as_ref()
                .map(|(_, callees)| callees.clone()),
        };
        self.write_event(&event).await?;
        self.session_started = true;
        // A firing from before the session frame existed — an `on-stage` hook's — hangs off it.
        self.write_call_deadlines().await
    }

    /// `input_tokens`/`output_tokens` are what the session and task totals accumulate: the
    /// runtime's own tiktoken estimates under `transport: http`, and the counts the harness's
    /// driver reported under `transport: process`, where the runtime measures nothing itself.
    /// `None` is a turn nothing counted, which only a process driver reporting no usage
    /// produces, and it adds nothing to the totals.
    ///
    /// `usage` is written verbatim beside them and accumulated into nothing. Under `http` it is
    /// the provider's own report for the same call; under `process` it carries only the cache and
    /// thinking counts, because the harness's input and output are already the two above.
    ///
    /// `settings` is how an `http` call was sent, written as `max_output_tokens` and
    /// `driver_config`; `None` under `process`, which writes neither key.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_inference(
        &mut self,
        turn: u32,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        decision: String,
        stop_reason: Option<&str>,
        tool_name: Option<String>,
        origin: Option<&InferenceOrigin>,
        usage: Option<&DriverUsage>,
        message_ids: Vec<String>,
        wire: Option<&WireCapture>,
        settings: Option<&InferenceCallSettings>,
    ) -> std::io::Result<()> {
        self.write_inference_event(InferenceLine {
            turn,
            input_tokens,
            output_tokens,
            decision,
            stop_reason,
            tool_name,
            origin,
            usage,
            message_ids,
            wire,
            choice: None,
            failure: None,
            counted: true,
            settings,
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
    }

    /// [`Self::write_inference`] for a hook's `run-inference` that succeeded, with the attempts
    /// its retry loop took.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_hook_inference(
        &mut self,
        turn: u32,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        decision: String,
        origin: &InferenceOrigin,
        usage: Option<&DriverUsage>,
        settings: Option<&InferenceCallSettings>,
        retry: RetryNote,
        gateway: Option<GatewayOffsets>,
    ) -> std::io::Result<()> {
        self.write_inference_event(InferenceLine {
            turn,
            input_tokens,
            output_tokens,
            decision,
            stop_reason: None,
            tool_name: None,
            origin: Some(origin),
            usage,
            message_ids: Vec::new(),
            wire: None,
            choice: None,
            failure: None,
            counted: true,
            settings,
            retry,
            gateway,
        })
        .await
    }

    /// [`Self::write_inference`] for an agent-loop turn, naming the driver choice that served it
    /// as `driver_choice` and `model`. `choice` is `None` on a capsule with no alternates, whose
    /// line is exactly what [`Self::write_inference`] writes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_turn_inference(
        &mut self,
        turn: u32,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        decision: String,
        stop_reason: Option<&str>,
        tool_name: Option<String>,
        usage: Option<&DriverUsage>,
        message_ids: Vec<String>,
        wire: Option<&WireCapture>,
        choice: Option<&crate::driver_choice::DriverChoice>,
        settings: Option<&InferenceCallSettings>,
        retry: RetryNote,
        gateway: Option<GatewayOffsets>,
    ) -> std::io::Result<()> {
        if stop_reason == Some("max_tokens") {
            self.task_capped_turns = self.task_capped_turns.saturating_add(1);
        }
        self.write_inference_event(InferenceLine {
            turn,
            input_tokens,
            output_tokens,
            decision,
            stop_reason,
            tool_name,
            origin: None,
            usage,
            message_ids,
            wire,
            choice,
            failure: None,
            counted: true,
            settings,
            retry,
            gateway,
        })
        .await
    }

    /// Record a model call that failed: `decision` and `stop_reason` `"error"`, the failure's
    /// `error_code`, `error` (capped at [`MAX_TASK_FAILURE_REASON_BYTES`]) and `provider_status`.
    ///
    /// Never hashes or stores a wire body, whatever `trace.capture` is. Writes `input_tokens` and
    /// `output_tokens` only from `failed.spent`, and advances the turn and token counters only
    /// when `spent` is `Some`, by exactly its counts. Parents like any other `inference`: an
    /// agent-loop record becomes the turn node, a hook's hangs off the turn.
    pub(crate) async fn write_failed_inference(
        &mut self,
        failed: FailedInference<'_>,
    ) -> std::io::Result<()> {
        let error =
            crate::agent::truncate_on_char_boundary(failed.error, MAX_TASK_FAILURE_REASON_BYTES)
                .to_string();
        let (input_tokens, output_tokens) = failed.spent.unwrap_or((None, None));
        self.write_inference_event(InferenceLine {
            turn: failed.turn,
            input_tokens,
            output_tokens,
            decision: "error".to_string(),
            stop_reason: Some("error"),
            tool_name: failed.tool_name,
            origin: failed.origin,
            usage: failed.usage,
            message_ids: failed.message_ids,
            wire: None,
            choice: failed.choice,
            failure: Some((failed.error_code, error, failed.provider_status)),
            counted: failed.spent.is_some(),
            settings: failed.settings,
            retry: failed.retry,
            gateway: failed.gateway,
        })
        .await
    }

    async fn write_inference_event(&mut self, line: InferenceLine<'_>) -> std::io::Result<()> {
        let InferenceLine {
            turn,
            input_tokens,
            output_tokens,
            decision,
            stop_reason,
            tool_name,
            origin,
            usage,
            message_ids,
            wire,
            choice,
            failure,
            counted,
            settings,
            retry,
            gateway,
        } = line;
        let (error_code, error, provider_status) = match failure {
            Some((code, error, status)) => (Some(code.to_string()), Some(error), status),
            None => (None, None, None),
        };
        // Drained under the previous turn, before this line becomes the turn node.
        self.write_call_deadlines().await?;
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
            stop_reason: stop_reason.map(str::to_string),
            tool_name,
            input_tokens_actual: usage.and_then(|u| u.input_tokens),
            output_tokens_actual: usage.and_then(|u| u.output_tokens),
            cached_tokens: usage.and_then(|u| u.cached_tokens),
            cache_write_tokens: usage.and_then(|u| u.cache_write_tokens),
            thinking_tokens: usage.and_then(|u| u.thinking_tokens),
            origin: origin.map(|o| o.source.clone()),
            driver_choice: choice.map(|choice| choice.name.clone()),
            model: origin
                .map(|o| o.model.clone())
                .or_else(|| choice.map(|choice| choice.model.clone())),
            max_output_tokens: settings.map(|settings| settings.max_output_tokens),
            input_budget: settings.map(|settings| settings.input_budget),
            driver_config: settings.map(|settings| settings.driver_config.clone()),
            message_ids,
            system_sha,
            tools_sha,
            response_sha,
            message_shas,
            error_code,
            error,
            provider_status,
            gateway_reached_ms: gateway.map(|gateway| gateway.reached_ms),
            upstream_responded_ms: gateway.map(|gateway| gateway.responded_ms),
            attempts: retry.attempts,
            retry_stopped: retry.stopped.map(RetryStop::wire_name),
        };
        self.write_event(&event).await?;
        if origin.is_none() {
            self.task_agent_turns = self.task_agent_turns.saturating_add(1);
        }
        if counted {
            // A turn nothing counted contributes nothing to the totals. The totals are plain
            // `u64` because a session's spend is a sum, and the sum of no measurements is zero;
            // the absent/zero distinction lives on the record itself.
            let (input, output) = (input_tokens.unwrap_or(0), output_tokens.unwrap_or(0));
            self.total_turns = self.total_turns.saturating_add(1);
            self.total_input_tokens = self.total_input_tokens.saturating_add(input);
            self.total_output_tokens = self.total_output_tokens.saturating_add(output);
            self.task_turns = self.task_turns.saturating_add(1);
            if origin.is_some() {
                self.task_hook_inferences = self.task_hook_inferences.saturating_add(1);
            }
            self.task_input_tokens = self.task_input_tokens.saturating_add(input);
            self.task_output_tokens = self.task_output_tokens.saturating_add(output);
        }
        // Every record, counted or not: the provider may have reported a call the turn and token
        // totals do not count.
        if let Some(status) = &self.session_status {
            status.record_inference(self.task_turns, input_tokens, output_tokens, usage);
        }
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
        input: Value,
        input_bytes: u64,
        output: &str,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
        state_effect: Option<String>,
        resource_id: Option<String>,
        error: Option<&str>,
    ) -> std::io::Result<()> {
        self.write_tool_call_with_attachments(
            turn,
            tool_name,
            tool_call_id,
            input,
            input_bytes,
            output,
            output_bytes,
            duration_ms,
            status,
            state_effect,
            resource_id,
            error,
            Vec::new(),
        )
        .await
    }

    /// [`Self::write_tool_call`] for a call that returned images: `attachments` becomes the
    /// record's `attachments` field, which an empty list omits.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_tool_call_with_attachments(
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
        error: Option<&str>,
        attachments: Vec<AttachmentRecord>,
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
            error: error.map(|error| {
                let redacted = crate::peer_handoff::redact_handle_tokens(error);
                crate::agent::truncate_on_char_boundary(&redacted, MAX_TASK_FAILURE_REASON_BYTES)
                    .to_string()
            }),
            state_effect,
            resource_id,
            attachments,
        };
        self.write_event(&event).await?;
        self.total_tool_calls = self.total_tool_calls.saturating_add(1);
        self.task_tool_calls = self.task_tool_calls.saturating_add(1);
        Ok(())
    }

    /// Records a skill invocation as a `skill_call` event. Skill calls are NOT counted in
    /// `total_tool_calls` — they appear in a separate `── Skill calls ──` section in `mur trace show`.
    ///
    /// `origin` is the skill's `murmur.lock` origin; the record's `trust` is derived from it
    /// through [`crate::origin::artifact_trust`], the same rule that decides the skill's fence.
    pub(crate) async fn write_skill_call(
        &mut self,
        turn: u32,
        skill_name: String,
        origin: &LockOrigin,
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
            origin: origin.as_str(),
            trust: crate::origin::artifact_trust(origin).as_str(),
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
        grace_ms: Option<u64>,
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

    /// A detached command's completion queued as the `completion`-origin task
    /// `completion_task_id`, written by the task loop as it enqueues it, so the two records join.
    pub(crate) async fn write_shell_completed(
        &mut self,
        completion: &crate::detached::DetachedCompletion,
        completion_task_id: &str,
    ) -> std::io::Result<()> {
        let event = ShellCompletedEvent::new(
            self.session_id.clone(),
            self.session_parent(),
            completion,
            crate::detached::Delivery::Task,
            Some(completion_task_id),
        );
        self.write_event(&event).await
    }

    /// A detached command the session ended without waiting for.
    ///
    /// `recovered` carries the exit code, the output path and that file's byte count for a command
    /// that finished during teardown, and is `None` for one still running. The three travel as one
    /// argument because they exist together: a command with an exit code has a written log, and
    /// one without has neither.
    pub(crate) async fn write_shell_abandoned(
        &mut self,
        work_id: &str,
        binary: &str,
        command: &str,
        running_ms: u64,
        recovered: Option<(i32, &str, u64)>,
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
            exit_code: recovered.map(|(exit_code, _, _)| exit_code),
            output_path: recovered.map(|(_, path, _)| path.to_string()),
            output_bytes: recovered.map(|(_, _, bytes)| bytes),
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

    /// Records how one tool call's safety checks ended. Parented to the turn.
    pub(crate) async fn write_safety_check(
        &mut self,
        turn: u32,
        tool_call_id: &str,
        tool: &str,
        checks: &serde_json::Value,
        outcome: &'static str,
    ) -> std::io::Result<()> {
        let event = SafetyCheckEvent {
            event_type: "safety_check",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            tool_call_id: tool_call_id.to_string(),
            tool: tool.to_string(),
            checks: checks.clone(),
            outcome,
        };
        self.write_event(&event).await
    }

    /// Records a task the door let in. `caller` is who the door let it in as and `caller_member`
    /// the roster name a formation token carried; a task with a member is recorded as the
    /// member's, whatever credential the token was minted for. The message text is hashed under
    /// `meta` and `content` and stored as a blob under `content`; the JSONL never holds it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_a2a_task_received(
        &mut self,
        task_id: &str,
        context_id: &str,
        message_id: &str,
        traceparent_from_caller: Option<&str>,
        caller: &crate::identity::TaskCaller,
        caller_member: Option<&str>,
        message_text: &str,
    ) -> std::io::Result<()> {
        use crate::identity::TaskCaller;
        let (received_from, caller_credential) = match (caller_member, caller) {
            (Some(_), _) => (RECEIVED_FROM_MEMBER, None),
            (None, TaskCaller::Operator) => (RECEIVED_FROM_OPERATOR, None),
            (None, TaskCaller::Public) => (RECEIVED_FROM_PUBLIC, None),
            (None, TaskCaller::Credential(name)) => {
                (RECEIVED_FROM_CREDENTIAL, Some(name.to_string()))
            }
        };
        let message_sha256 = if self.capture.captures_hashes() {
            let sha = murmur_artifact::sha256_hex(message_text.as_bytes());
            if self.capture.captures_content() {
                self.blobs.put(&sha, message_text.as_bytes()).await?;
            }
            Some(sha)
        } else {
            None
        };
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
            caller: received_from,
            caller_member: caller_member.map(str::to_string),
            caller_credential,
            message_sha256,
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

    /// Records a successful pull of `pulled` (`manage.pull()` or `pull-artifact`), as the store
    /// summarised it after the lock was written. Parented to the session node.
    pub(crate) async fn write_artifact_pulled(
        &mut self,
        pulled: &crate::runtime::PulledArtifact,
    ) -> std::io::Result<()> {
        let artifact = &pulled.summary;
        let session = match &artifact.origin {
            LockOrigin::Runtime { session } => Some(session.clone()),
            LockOrigin::Operator => None,
        };
        let event = ArtifactPulledEvent {
            event_type: "artifact_pulled",
            event_id: new_event_id(),
            parent_id: self.session_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.as_str(),
            origin: artifact.origin.as_str(),
            session,
            trust: crate::origin::artifact_trust(&artifact.origin).as_str(),
            fetched_from: pulled.fetched_from.clone(),
            owner_id: pulled.owner_id,
        };
        self.write_event(&event).await
    }

    /// `part_kinds` and `reference_task_ids` describe the A2A message the task arrived as, and are
    /// empty for any other task.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_task_start(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
        provenance: TaskProvenance,
        message_parts_bytes: u64,
        part_kinds: &[&'static str],
        reference_task_ids: &[String],
    ) -> std::io::Result<()> {
        self.write_task_start_with_attachments(
            task_id,
            context_id,
            source,
            provenance,
            message_parts_bytes,
            part_kinds,
            reference_task_ids,
            Vec::new(),
        )
        .await
    }

    /// The `task_start` record. `attachments` lists the images the task's message carried, and an
    /// empty list omits the field.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_task_start_with_attachments(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
        provenance: TaskProvenance,
        message_parts_bytes: u64,
        part_kinds: &[&'static str],
        reference_task_ids: &[String],
        attachments: Vec<AttachmentRecord>,
    ) -> std::io::Result<()> {
        // Drained under the ids in force before this task, which the lines below replace.
        self.write_call_deadlines().await?;
        // Reset per-task counters for this task
        self.task_turns = 0;
        self.task_input_tokens = 0;
        self.task_output_tokens = 0;
        self.task_tool_calls = 0;
        self.task_shell_calls = 0;
        self.task_agent_turns = 0;
        self.task_hook_inferences = 0;
        self.task_capped_turns = 0;
        let started = Instant::now();
        self.task_start_instant = Some(started);
        self.active_task_id = Some(task_id.to_string());
        if let Some(status) = &self.session_status {
            status.task_started(task_id, started);
        }

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
            message_parts_bytes,
            part_kinds: part_kinds.to_vec(),
            reference_task_ids: reference_task_ids.to_vec(),
            attachments,
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_end(
        &mut self,
        task_id: &str,
        exit_status: &str,
        reopen_count: u32,
        running_commands: Vec<String>,
        keep_working: Option<KeepWorkingSummary>,
        done_check: Option<DoneCheckSummary>,
    ) -> std::io::Result<()> {
        // Drained inside the task, before the lines below clear its ids.
        self.write_call_deadlines().await?;
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
            capped_turns: self.task_capped_turns,
            running_commands,
            keep_working,
            done_check,
        };
        self.active_task_id = None;
        self.task_event_id = None;
        self.turn_event_id = None;
        self.task_agent_turns = 0;
        if let Some(status) = &self.session_status {
            status.task_ended();
        }
        self.write_event(&event).await
    }

    /// Record that a person stopped this task, at the point the loop stopped.
    ///
    /// Leaves `active_task_id` and the task's parent id alone: for a running task the terminal
    /// `task_end` still follows and closes the frame. A task cancelled while queued was never
    /// started, so this record hangs off the session.
    pub(crate) async fn write_task_canceled(
        &mut self,
        task_id: &str,
        turn: Option<u32>,
        phase: &str,
        detached_work_ids: Vec<String>,
        delegation_ids: Vec<String>,
    ) -> std::io::Result<()> {
        let event = TaskCanceledEvent {
            event_type: "task_canceled",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            turn,
            phase: phase.to_string(),
            detached_work_ids,
            delegation_ids,
        };
        self.write_event(&event).await
    }

    /// Record that a queued task was refused when the session stopped taking work, with `cause`
    /// from the `TASK_REJECTED_*` vocabulary. Written whatever `trace.capture` is.
    ///
    /// Leaves `active_task_id` alone: the refused task never started, so there is no task frame
    /// to open or close, and with no task active the record hangs off the session.
    pub(crate) async fn write_task_rejected(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
        cause: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = TaskRejectedEvent {
            event_type: "task_rejected",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            source: source.to_string(),
            cause: cause.to_string(),
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record that the task attempt in progress failed, with `cause` from the `TASK_FAILED_*`
    /// vocabulary. Written whatever `trace.capture` is.
    ///
    /// Leaves the task frame open, as [`Self::write_task_canceled`] does: the `task_end` that
    /// follows closes it. The reason, capped, is kept for [`Self::last_task_failure`].
    pub(crate) async fn write_task_failed(
        &mut self,
        turn: Option<u32>,
        cause: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let reason = crate::agent::truncate_on_char_boundary(reason, MAX_TASK_FAILURE_REASON_BYTES)
            .to_string();
        let event = TaskFailedEvent {
            event_type: "task_failed",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            turn,
            cause: cause.to_string(),
            reason: reason.clone(),
        };
        self.task_failures_written += 1;
        self.last_task_failure = Some(reason);
        self.write_event(&event).await
    }

    /// Record one refusal the workdir helper latched.
    pub(crate) async fn write_workdir_path_refused(
        &mut self,
        refusal: &crate::workdir_fs::WorkdirRefusal,
    ) -> std::io::Result<()> {
        let event = WorkdirPathRefusedEvent {
            event_type: "workdir_path_refused",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            path: &refusal.path,
            operation: refusal.operation,
            refused_at: &refusal.refused_at,
            reason: refusal.reason,
        };
        self.write_event(&event).await
    }

    /// The `reason` of the last `task_failed` line this writer wrote, or `None` before the first.
    pub(crate) fn last_task_failure(&self) -> Option<&str> {
        self.last_task_failure.as_deref()
    }

    /// How many `task_failed` lines this writer has written. Compared across one run to tell
    /// whether that run wrote its own.
    pub(crate) fn task_failures_written(&self) -> u64 {
        self.task_failures_written
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
        attempt_context: &'static str,
        turns_remaining: Option<u32>,
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
            attempt_context,
            turns_remaining,
        };
        self.write_event(&event).await
    }

    /// Record that the task continues with one round of handed-off work's outcomes. Written by
    /// the runtime's per-task loop once per continuation, before the continued attempt's first
    /// `inference`. `continuation_number` is 1-based.
    pub(crate) async fn write_task_continued(
        &mut self,
        task_id: &str,
        continuation_number: u32,
        member_calls: &[String],
        delegations: &[String],
        waited_ms: u64,
        turns_remaining: Option<u32>,
    ) -> std::io::Result<()> {
        let event = TaskContinuedEvent {
            event_type: "task_continued",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            continuation_number,
            member_calls,
            delegations,
            waited_ms,
            turns_remaining,
        };
        self.write_event(&event).await
    }

    /// Record that `lifecycle.keep_working` sent its message as the task's `number`th continuation
    /// (1-based), before the continued attempt's first `inference`.
    pub(crate) async fn write_keep_working(
        &mut self,
        task_id: &str,
        number: u32,
        limit: u32,
        turns_remaining: Option<u32>,
        message_bytes: u64,
    ) -> std::io::Result<()> {
        let event = KeepWorkingEvent {
            event_type: "keep_working",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            number,
            limit,
            turns_remaining,
            message_bytes,
        };
        if let Some(status) = &self.session_status {
            status.keep_working_sent(number);
        }
        self.write_event(&event).await
    }

    /// Record one run of `lifecycle.done_check`'s command. A run that reopened the task feeds the
    /// session status the task's reopens so far, `reopens`.
    pub(crate) async fn write_done_check(
        &mut self,
        run: &DoneCheckRun<'_>,
        reopens: u32,
    ) -> std::io::Result<()> {
        let event = DoneCheckEvent {
            event_type: "done_check",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            run,
        };
        if run.reopened {
            if let Some(status) = &self.session_status {
                status.done_check_reopened(reopens);
            }
        }
        self.write_event(&event).await
    }

    /// Cumulative turns consumed by the active task so far, across however many
    /// agent-loop attempts have run since the last [`Self::write_task_start`]. Read by
    /// the reopen loop to compute the remaining `max_turns` budget for the next attempt
    /// so reopening never grants turns past the capsule's ceiling.
    ///
    /// A hook's counted `run-inference` call is one of them. `inference.max_turns` is the cap on
    /// inference calls per task, and `task_end.turns` and `session_end.total_turns` count the
    /// same calls; a hook that calls `run-inference` every turn is per-task spend that the cap
    /// exists to bound, and the spend ceiling is per capsule, not per task.
    pub(crate) fn task_turns(&self) -> u32 {
        self.task_turns
    }

    /// How many of [`Self::task_turns`] were hook `run-inference` calls.
    pub(crate) fn task_hook_inferences(&self) -> u32 {
        self.task_hook_inferences
    }

    /// The number the active task's next agent-loop turn takes: how many agent-loop `inference`
    /// lines it has written since [`Self::write_task_start`], across every attempt. 0 outside a
    /// task frame. Not the `max_turns` budget, which is [`Self::task_turns`].
    pub(crate) fn task_agent_turns(&self) -> u32 {
        self.task_agent_turns
    }

    pub(crate) async fn write_compaction(
        &mut self,
        turn: u32,
        tokens_before: u64,
        tokens_before_source: OccupancyBasis,
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
            tokens_before_source,
            tokens_after,
        };
        self.write_event(&event).await
    }

    /// Record that compaction was attempted and declined, leaving the context over budget.
    /// `tokens` is the occupancy at the moment of the decline and `tokens_source` the kind of
    /// number it is; `reason` is
    /// [`COMPACTION_DECLINED_NO_HOOK_REPLACEMENT`] or
    /// [`COMPACTION_DECLINED_UNRESOLVED_TOOL_CALL`].
    pub(crate) async fn write_compaction_declined(
        &mut self,
        turn: u32,
        tokens: u64,
        tokens_source: OccupancyBasis,
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
            tokens_source,
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

    /// Record that a call was refused because its input lacked a field its tool's `input_schema`
    /// requires. A `tool_call` record with `status: "error"` follows it on both transports, so
    /// unlike [`Self::write_call_denied`] this is not the only line the call leaves. An empty
    /// `tool_call_id` records as `null`, as in [`Self::write_tool_call`].
    pub(crate) async fn write_tool_input_refused(
        &mut self,
        turn: u32,
        tool_name: &str,
        tool_call_id: Option<&str>,
        missing: &[String],
        reason: &str,
    ) -> std::io::Result<()> {
        let event = ToolInputRefusedEvent {
            event_type: "tool_input_refused",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            tool_name: tool_name.to_string(),
            tool_call_id: tool_call_id.filter(|id| !id.is_empty()).map(str::to_string),
            missing: missing.to_vec(),
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record that a driver call failed transiently and is about to be made again after
    /// `retry.delay_ms`. `retry.origin` is the `hook:<name>` of a hook's `run-inference` call,
    /// `None` for an agent-loop turn. The error text is capped at
    /// [`MAX_TASK_FAILURE_REASON_BYTES`].
    pub(crate) async fn write_inference_retry(
        &mut self,
        turn: u32,
        retry: &InferenceRetryRecord,
    ) -> std::io::Result<()> {
        let event = InferenceRetryEvent {
            event_type: "inference_retry",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            attempt: retry.attempt,
            max_retries: retry.max_retries,
            reason: retry.reason.wire_name(),
            provider_status: retry.provider_status,
            error: crate::agent::truncate_on_char_boundary(
                &retry.error,
                MAX_TASK_FAILURE_REASON_BYTES,
            ),
            delay_ms: retry.delay_ms,
            retry_after_ms: retry.retry_after_ms,
            charged_tokens: retry.charged_tokens,
            origin: retry.origin.as_deref(),
        };
        self.write_event(&event).await
    }

    /// Record that a spend ceiling refused a driver call before it was sent. `origin` is the
    /// `hook:<name>` of a hook's `run-inference` call, `None` for an agent-loop turn.
    pub(crate) async fn write_spend_ceiling_reached(
        &mut self,
        turn: u32,
        refusal: &crate::spend::SpendRefusal,
        origin: Option<&str>,
    ) -> std::io::Result<()> {
        let event = SpendCeilingReachedEvent {
            event_type: "spend_ceiling_reached",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            task_id: self.active_task_id.clone(),
            refusal,
            origin,
        };
        self.write_event(&event).await
    }

    /// Record that the manifest's `capabilities.filesystem.read_only` refused a call before it
    /// ran. Like [`Self::write_call_denied`] it is the only account of a call the model asked for
    /// and never got — nothing ran, so no `tool_call` or `shell` record accompanies it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_protected_path_denied(
        &mut self,
        turn: u32,
        call: &str,
        target: &str,
        path: &str,
        rule: &str,
        signal: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = ProtectedPathDeniedEvent {
            event_type: "protected_path_denied",
            event_id: new_event_id(),
            parent_id: self.turn_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            call: call.to_string(),
            target: target.to_string(),
            path: path.to_string(),
            rule: rule.to_string(),
            signal: signal.to_string(),
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

    // ── The harness a `transport: process` session drives ────────────────────────────────
    //
    // Eight events account for one harness run: what was spawned, what it said about itself,
    // what it warned or noted, how it failed, how it was stopped, and how it ended. None of them
    // ever carries an argument value, an environment value or a file's contents — the bridge's
    // bearer token reaches the harness through exactly those three, and `trace.jsonl` is durable.

    /// Record the harness about to be spawned: written after the driver planned the run and
    /// before the process exists, so a spawn that goes wrong still has its plan on record.
    pub(crate) async fn write_harness_start(&mut self, start: HarnessStart) -> std::io::Result<()> {
        if self.capture.captures_content() {
            self.blobs
                .put(&start.system_prompt_sha256, start.system_prompt.as_bytes())
                .await?;
        }
        let event = HarnessStartEvent {
            event_type: "harness_start",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            start,
        };
        self.write_event(&event).await
    }

    /// Record a warning the run raised, beside the line it printed to stderr.
    pub(crate) async fn write_harness_warning(
        &mut self,
        code: &str,
        message: &str,
    ) -> std::io::Result<()> {
        let event = HarnessWarningEvent {
            event_type: "harness_warning",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            code: code.to_string(),
            message: message.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record the session the harness reports it opened, and how it is billing for it.
    pub(crate) async fn write_harness_session(
        &mut self,
        harness_session_id: &str,
        auth: &str,
        model: Option<&str>,
    ) -> std::io::Result<()> {
        let event = HarnessSessionEvent {
            event_type: "harness_session",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            harness_session_id: harness_session_id.to_string(),
            auth: auth.to_string(),
            model: model.map(str::to_string),
        };
        self.write_event(&event).await
    }

    /// Record the harness session a person asked this capsule to forget, and who asked.
    pub(crate) async fn write_harness_session_forgotten(
        &mut self,
        context_id: &str,
        harness_session_id: &str,
        requested_by: &str,
    ) -> std::io::Result<()> {
        let event = HarnessSessionForgottenEvent {
            event_type: "harness_session_forgotten",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            context_id: context_id.to_string(),
            harness_session_id: harness_session_id.to_string(),
            requested_by: requested_by.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record that the harness is retrying a request to its own provider.
    pub(crate) async fn write_harness_retry(
        &mut self,
        attempt: u32,
        reason: &str,
    ) -> std::io::Result<()> {
        let event = HarnessRetryEvent {
            event_type: "harness_retry",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            attempt,
            reason: reason.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record a diagnostic the driver produced for the trace alone — a line it could not read,
    /// or a result the runtime could not pair with a call.
    pub(crate) async fn write_harness_note(&mut self, text: &str) -> std::io::Result<()> {
        let event = HarnessTextEvent {
            event_type: "harness_note",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            text: text.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record one report of the subscription windows the harness says are used, already
    /// validated, and hand it to the session status, which keeps the latest window per name.
    pub(crate) async fn write_harness_quota(
        &mut self,
        windows: &[crate::process_driver::QuotaWindow],
    ) -> std::io::Result<()> {
        if let Some(status) = &self.session_status {
            status.record_quota(windows);
        }
        let event = HarnessQuotaEvent {
            event_type: "harness_quota",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            windows: windows
                .iter()
                .map(|window| HarnessQuotaWindow {
                    name: &window.name,
                    used: window.used,
                    resets_at_ms: window.resets_at,
                    window_minutes: window.window_minutes,
                    state: window
                        .state
                        .map(crate::process_driver::QuotaState::wire_name),
                })
                .collect(),
        };
        self.write_event(&event).await
    }

    /// Record the harness's prediction of what the person will ask next.
    pub(crate) async fn write_harness_suggestion(&mut self, text: &str) -> std::io::Result<()> {
        let event = HarnessTextEvent {
            event_type: "harness_suggestion",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            text: text.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record a failed turn. `source` is `"harness"` when the harness reported the failure and
    /// `"runtime"` when this runtime ended the turn itself.
    pub(crate) async fn write_harness_failed(
        &mut self,
        kind: &str,
        message: &str,
        source: &str,
    ) -> std::io::Result<()> {
        let event = HarnessFailedEvent {
            event_type: "harness_failed",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            kind: kind.to_string(),
            message: message.to_string(),
            source: source.to_string(),
        };
        self.write_event(&event).await
    }

    /// Record the interrupt a cancelled task sent its harness: the method the driver declared,
    /// whether it went out, and how long the harness has before it is killed.
    pub(crate) async fn write_harness_interrupt(
        &mut self,
        method: &str,
        delivered: bool,
        grace_ms: u64,
    ) -> std::io::Result<()> {
        let event = HarnessInterruptEvent {
            event_type: "harness_interrupt",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            method: method.to_string(),
            delivered,
            grace_ms,
        };
        self.write_event(&event).await
    }

    /// Record how the harness ended: once per spawned process, whatever ended it.
    pub(crate) async fn write_harness_exit(&mut self, exit: HarnessExit) -> std::io::Result<()> {
        let event = HarnessExitEvent {
            event_type: "harness_exit",
            event_id: new_event_id(),
            parent_id: self.task_parent(),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: self.active_task_id.clone(),
            exit,
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

    /// Write `event` as one line, after a `call_deadline` line for each firing the session's
    /// latch holds. Draining here, ahead of every line, is what puts a cut call's
    /// `call_deadline` before the record of that call and every one before `session_end`.
    async fn write_event(&mut self, event: &impl Serialize) -> std::io::Result<()> {
        self.write_call_deadlines().await?;
        self.write_line(event).await
    }

    /// One `call_deadline` line per firing [`Self::call_deadlines`] holds, parented to the turn
    /// as `inference_retry` is. Nothing before `session_start`, which every line hangs off.
    async fn write_call_deadlines(&mut self) -> std::io::Result<()> {
        let Some(deadlines) = self
            .call_deadlines
            .as_ref()
            .filter(|_| self.session_started)
        else {
            return Ok(());
        };
        for cut in deadlines.take() {
            let event = CallDeadlineEvent {
                event_type: "call_deadline",
                event_id: new_event_id(),
                parent_id: self.turn_parent(),
                session_id: self.session_id.clone(),
                timestamp: timestamp_ms(),
                task_id: self.active_task_id.clone(),
                component: cut.component,
                role: cut.role.wire_name(),
                call: cut.call,
                deadline_seconds: cut.deadline_seconds,
                elapsed_ms: cut.elapsed_ms,
            };
            self.write_line(&event).await?;
        }
        Ok(())
    }

    /// Writes one event as one line, and flushes.
    ///
    /// The flush is not an optimisation choice: [`ResourceTraceAppender`] appends to this same
    /// `trace.jsonl` while a task runs, so a line left half-buffered here would be split around
    /// whatever the appender wrote in between and the file would stop parsing as JSONL. The
    /// two writers agree on one rule — a complete line per write, then flush — and that rule is
    /// what makes concurrent reads during a running task safe to record.
    async fn write_line(&mut self, event: &impl Serialize) -> std::io::Result<()> {
        let mut line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await
    }
}

/// One element of `session_start.gateways`: an artifact's credential gateway. Never the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SessionGateway {
    /// The artifact whose entry declares the gateway.
    pub(crate) artifact: String,
    /// The upstream's host, with its port when `gateway.endpoint` wrote one.
    pub(crate) host: String,
    /// `"config"`, `"environment"`, `"manifest"`, or `"none"` when the entry names no key.
    pub(crate) credential_source: &'static str,
    /// Whether the gateway is admitted against the spend meter: true only for the configured
    /// `transport: http` driver's.
    pub(crate) metered: bool,
}

/// One element of `session_start.runtime_artifacts`: a staged artifact whose `murmur.lock` pin a
/// running capsule fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RuntimeArtifact {
    name: String,
    version: String,
    /// Always `"runtime"`: only runtime-origin entries are listed.
    origin: &'static str,
    /// The session the lock entry names as the puller.
    session: String,
    /// Always `"untrusted"`, derived from `origin`.
    trust: &'static str,
}

/// One element of `session_start.host_facts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HostFactsRecord {
    /// The binary, canonical.
    binary: String,
    /// `"kept"` or `"derived"`.
    source: &'static str,
    /// Why the facts were derived, as [`crate::host_facts::HostFactsReason::wire_name`] spells
    /// it. Omitted for `"kept"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// The first path whose stamp differed. Written only for the reason `"changed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_path: Option<String>,
}

impl HostFactsRecord {
    fn of(used: &crate::host_facts::HostFactsUse) -> Self {
        use crate::host_facts::HostFactsSource;

        let binary = used.binary.display().to_string();
        match &used.source {
            HostFactsSource::Kept => Self {
                binary,
                source: "kept",
                reason: None,
                changed_path: None,
            },
            HostFactsSource::Derived {
                reason,
                changed_path,
            } => Self {
                binary,
                source: "derived",
                reason: Some(reason.wire_name()),
                changed_path: changed_path.as_ref().map(|path| path.display().to_string()),
            },
        }
    }
}

/// `session_start.control`: the names a capsule's `control:` block declares, never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SessionControl {
    /// Controllable setting names, as `control.settings` lists them.
    pub(crate) settings: Vec<&'static str>,
    /// Controller-supplied secret names, as `control.secrets` lists them.
    pub(crate) secrets: Vec<String>,
    /// Setting names the agent may change, as `control.agent_settings` lists them. Omitted when
    /// the block grants the agent nothing.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) agent_settings: Vec<&'static str>,
}

/// One element of `session_start.inference_choices`: a driver choice and whether its credential
/// resolved at launch. Never a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SessionInferenceChoice {
    pub(crate) name: String,
    pub(crate) driver: String,
    pub(crate) model: String,
    /// Where the driver's gateway credential came from, as `session_start.gateways` names it, or
    /// `"none"` for one found nowhere.
    pub(crate) credential_source: &'static str,
    /// Whether the credential resolved at launch. An injected credential is `true` here and is
    /// checked again when a switch asks for it.
    pub(crate) available: bool,
    /// The choice's own output cap and context window in force, and where each came from.
    pub(crate) model_limits: Option<crate::model_limits::ResolvedModelLimits>,
}

/// One accepted setting or secret change, as [`ResourceTraceAppender::write_control_change`]
/// records it. `event_id` is minted by the caller, because the setting it changed carries it
/// forward to the `control_applied` that names it.
pub(crate) struct ControlChange<'a> {
    pub(crate) event_id: String,
    /// Who made the change.
    pub(crate) principal: crate::control_plane::Principal,
    /// The control token's id for a controller's change; `None` for the agent's, which presents
    /// no token.
    pub(crate) token_id: Option<&'a str>,
    /// `"setting"` or `"secret"`.
    pub(crate) kind: &'static str,
    pub(crate) name: &'a str,
    /// `"set"` or `"forget"`.
    pub(crate) action: &'static str,
    /// A setting's value before and after. Always `None` for a secret.
    pub(crate) previous: Option<Value>,
    pub(crate) value: Option<Value>,
    /// When a setting change takes effect. `None` for a secret.
    pub(crate) applies_from: Option<&'static str>,
    /// For a secret `set`: whether it replaced a held value.
    pub(crate) replaced: Option<bool>,
}

/// `control_change`: a controller changed a setting or a secret. A secret's line names it and
/// carries nothing of its value.
#[derive(Serialize)]
struct ControlChangeEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    principal: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<&'a str>,
    kind: &'static str,
    name: &'a str,
    action: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    applies_from: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<bool>,
}

/// `control_refused`: the control plane refused a request, or the runtime refused the agent's
/// change. `kind`, `name` and `token_id` are written only once a controller's request
/// authenticated, so a `401` names nothing; the agent's refusal names its setting and no token.
#[derive(Serialize)]
struct ControlRefusedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    principal: &'static str,
    status: u16,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<&'a str>,
}

/// `control_applied`: the first inference call to use a setting value a controller set.
#[derive(Serialize)]
struct ControlAppliedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    name: &'a str,
    value: Value,
    change_id: &'a str,
}

/// `tools_refreshed`: the first inference call to send a tool array rebuilt after an install.
#[derive(Serialize)]
struct ToolsRefreshedEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    turn: u32,
    task_id: Option<String>,
    trigger: &'a str,
    added: &'a [String],
    removed: &'a [String],
    tools: &'a [String],
}

/// `gateway_credential`: the `inference_credential` event for the credential of any artifact other
/// than the configured inference driver, with the artifact's name.
#[derive(Serialize)]
struct GatewayCredentialEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    artifact: &'a str,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<&'a str>,
    change: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retried: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// `inference_credential`: a rotation, rejection or unreadable source for the session's inference
/// credential. `trigger` is written only with `change: "rotated"`, `status` and `retried` only with
/// `"rejected"`, `reason` only with `"unreadable"`, and `credential` whenever the manifest named one.
#[derive(Serialize)]
struct InferenceCredentialEvent<'a> {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<&'a str>,
    change: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retried: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// A credential change as the `change`, `trigger`, `status`, `retried` and `reason` fields of an
/// `inference_credential` or `gateway_credential` event.
#[allow(clippy::type_complexity)]
fn credential_change_fields(
    change: CredentialChange,
) -> (
    &'static str,
    Option<&'static str>,
    Option<u16>,
    Option<bool>,
    Option<&'static str>,
) {
    match change {
        CredentialChange::Rotated { trigger } => ("rotated", Some(trigger), None, None, None),
        CredentialChange::Rejected { status, retried } => {
            ("rejected", None, Some(status), Some(retried), None)
        }
        CredentialChange::Unreadable { reason } => ("unreadable", None, None, None, Some(reason)),
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
        records_root: &WorkdirRoot,
        session_id: String,
        session_event_id: String,
    ) -> std::io::Result<Self> {
        let file = File::from_std(records_root.open_append(TRACE_FILE_NAME, DEFAULT_FILE_MODE)?);
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

    /// Records `formation_ended`, hung off the session node. Failures are swallowed: a member
    /// whose trace cannot be written still winds down.
    pub(crate) async fn write_formation_ended(&self, formation_id: &crate::FormationId) {
        let event = FormationEndedEvent {
            event_type: "formation_ended",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            formation_id: formation_id.as_str().to_string(),
        };
        self.append(&event).await;
    }

    /// Records one `session-status` call, hung off the session node. Failures are swallowed: the
    /// call is answered whether or not its record lands.
    pub(crate) async fn write_session_status(
        &self,
        task_id: Option<String>,
        status: &crate::session_status::StatusReport,
    ) {
        let event = SessionStatusEvent {
            event_type: "session_status",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id,
            status,
        };
        self.append(&event).await;
    }

    /// Records one answered `background-command` call, hung off the session node. Failures are
    /// swallowed: the call is answered whether or not its record lands.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_background_command(
        &self,
        task_id: Option<String>,
        work_id: &str,
        operation: &'static str,
        state: &'static str,
        exit_code: Option<i32>,
        waited_ms: Option<u64>,
        output_bytes: u64,
    ) {
        let event = BackgroundCommandEvent {
            event_type: "background_command",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id,
            work_id,
            operation,
            state,
            exit_code,
            waited_ms,
            output_bytes,
        };
        self.append(&event).await;
    }

    /// Records a detached command's completion that a `background-command` call just returned to
    /// the agent, hung off the session node with no `completion_task_id`. Written after that call's
    /// `background_command` line. Failures are swallowed, as that line's are.
    pub(crate) async fn write_shell_completed(
        &self,
        completion: &crate::detached::DetachedCompletion,
    ) {
        let event = ShellCompletedEvent::new(
            self.session_id.clone(),
            Some(self.session_event_id.clone()),
            completion,
            crate::detached::Delivery::BackgroundCommand,
            None,
        );
        self.append(&event).await;
    }

    /// Records the deadline notice `tool_name`'s result carried, hung off the session node.
    pub(crate) async fn write_deadline_notice(
        &self,
        task_id: Option<String>,
        tool_name: &str,
        notice: &crate::session_status::DeadlineNotice,
    ) {
        let event = DeadlineNoticeEvent {
            event_type: "deadline_notice",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id,
            tool_name,
            deadline_at: &notice.at,
            left_secs: notice.left_secs,
            notice_secs: notice.notice_secs,
        };
        self.append(&event).await;
    }

    /// Records `deadline_reached`, hung off the session node. Failures are swallowed: a session
    /// whose trace cannot be written still ends at its deadline.
    pub(crate) async fn write_deadline_reached(
        &self,
        deadline: &crate::session_status::SessionDeadline,
    ) {
        let event = DeadlineReachedEvent {
            event_type: "deadline_reached",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            deadline_at: deadline.at_rfc3339(),
            deadline_secs: deadline.secs(),
            source: deadline.source().as_str(),
        };
        self.append(&event).await;
    }

    /// Records `spawner_ended`, hung off the session node, naming the session that spawned this
    /// one and the delegation it was spawned under. Failures are swallowed: a child whose trace
    /// cannot be written still winds down.
    pub(crate) async fn write_spawner_ended(
        &self,
        spawned_by: Option<&str>,
        delegation_id: Option<&str>,
    ) {
        let event = SpawnerEndedEvent {
            event_type: "spawner_ended",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            spawned_by: spawned_by.map(str::to_string),
            delegation_id: delegation_id.map(str::to_string),
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

    /// Records one launched child, while its delegation is still running.
    ///
    /// Every field is known by the time the child has reported its session id, so none is
    /// optional: a delegation with nothing to say here was never launched and writes no line.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_delegation_start(
        &self,
        delegation_id: &str,
        capsule: &str,
        version: &str,
        child_session_id: &str,
        child_workdir: &str,
        child_records_dir: &str,
        workdir_snapshot: Option<&crate::workdir_snapshot::WorkdirSnapshot>,
    ) {
        let event = DelegationStartEvent::new(
            &self.session_id,
            &self.session_event_id,
            delegation_id,
            capsule,
            version,
            child_session_id,
            child_workdir,
            child_records_dir,
            workdir_snapshot,
        );
        self.append(&event).await;
    }

    /// Records one delegation ending. `delegation_id` and `child_session_id` are `None` when no
    /// child was launched — a delegation that never started names no id, and an audit record must
    /// not imply one exists.
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
        let event = DelegationEvent::new(
            &self.session_id,
            &self.session_event_id,
            capsule,
            version,
            delegation_id,
            child_session_id,
            duration_ms,
            outcome,
            reason,
        );
        self.append(&event).await;
    }

    /// Records a `call-member` call whose callee now holds the task.
    pub(crate) async fn write_member_call_start(
        &self,
        task_id: &str,
        call_id: &str,
        member: &str,
        member_task_id: &str,
    ) {
        let event = self.member_call_start(task_id, call_id, member, member_task_id);
        self.append(&event).await;
    }

    /// [`Self::write_member_call_start`] when `admit`, run with the trace held, returns `true`;
    /// returns what `admit` did. No other line lands between `admit` and this one.
    pub(crate) async fn write_member_call_start_if(
        &self,
        task_id: &str,
        call_id: &str,
        member: &str,
        member_task_id: &str,
        admit: impl FnOnce() -> bool,
    ) -> bool {
        let event = self.member_call_start(task_id, call_id, member, member_task_id);
        self.append_if(&event, admit).await
    }

    fn member_call_start(
        &self,
        task_id: &str,
        call_id: &str,
        member: &str,
        member_task_id: &str,
    ) -> MemberCallStartEvent {
        MemberCallStartEvent {
            event_type: "member_call_start",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            call_id: call_id.to_string(),
            member: member.to_string(),
            member_task_id: member_task_id.to_string(),
        }
    }

    /// Records the `offer`th offer of a `call-member` call turned away busy, `waited_ms` after the
    /// call was made, with the door's status `message`.
    pub(crate) async fn write_member_call_busy(
        &self,
        task_id: &str,
        call_id: &str,
        member: &str,
        offer: u32,
        waited_ms: u64,
        message: &str,
    ) {
        let event = MemberCallBusyEvent {
            event_type: "member_call_busy",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            call_id: call_id.to_string(),
            member: member.to_string(),
            offer,
            waited_ms,
            message: message.to_string(),
        };
        self.append(&event).await;
    }

    /// Records one `call-member` call's ending, and whether its outcome reached the calling task.
    pub(crate) async fn write_member_call(
        &self,
        task_id: &str,
        outcome: &crate::member_call::MemberCallOutcome,
        delivered: bool,
    ) {
        let event = MemberCallEvent {
            event_type: "member_call",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            call_id: outcome.call_id.clone(),
            member: outcome.member.clone(),
            member_task_id: outcome.member_task_id.clone(),
            status: outcome.status.as_str().to_string(),
            duration_ms: outcome.duration_ms,
            output: outcome.output.clone(),
            truncated: outcome.truncated,
            delivered,
            no_answer_below: outcome
                .below
                .iter()
                .map(|(member, status)| NoAnswerBelow {
                    member: member.clone(),
                    status: status.as_str().to_string(),
                })
                .collect(),
        };
        self.append(&event).await;
    }

    /// Records one change to the session's inference credential. Carries where the credential
    /// comes from and what happened to it, never the value.
    pub(crate) async fn write_inference_credential(
        &self,
        source: &'static str,
        credential: Option<&str>,
        change: CredentialChange,
    ) {
        let (change_name, trigger, status, retried, reason) = credential_change_fields(change);
        let event = InferenceCredentialEvent {
            event_type: "inference_credential",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            source,
            credential,
            change: change_name,
            trigger,
            status,
            retried,
            reason,
        };
        self.append(&event).await;
    }

    /// Writes a `gateway_credential` event for `artifact`'s credential: the fields
    /// [`Self::write_inference_credential`] writes, plus `artifact`.
    pub(crate) async fn write_gateway_credential(
        &self,
        artifact: &str,
        source: &'static str,
        credential: Option<&str>,
        change: CredentialChange,
    ) {
        let (change_name, trigger, status, retried, reason) = credential_change_fields(change);
        let event = GatewayCredentialEvent {
            event_type: "gateway_credential",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            artifact,
            source,
            credential,
            change: change_name,
            trigger,
            status,
            retried,
            reason,
        };
        self.append(&event).await;
    }

    /// Records one accepted control-plane change, under the `event_id` the caller minted.
    pub(crate) async fn write_control_change(&self, change: ControlChange<'_>) {
        let event = ControlChangeEvent {
            event_type: "control_change",
            event_id: change.event_id,
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            principal: change.principal.as_str(),
            token_id: change.token_id,
            kind: change.kind,
            name: change.name,
            action: change.action,
            previous: change.previous,
            value: change.value,
            applies_from: change.applies_from,
            replaced: change.replaced,
        };
        self.append(&event).await;
    }

    /// Records one refused change. `kind`, `name` and `token_id` are `None` for a controller's
    /// request that did not authenticate.
    pub(crate) async fn write_control_refused(
        &self,
        principal: crate::control_plane::Principal,
        status: u16,
        reason: &'static str,
        kind: Option<&'static str>,
        name: Option<&str>,
        token_id: Option<&str>,
    ) {
        let event = ControlRefusedEvent {
            event_type: "control_refused",
            event_id: new_event_id(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            principal: principal.as_str(),
            status,
            reason,
            kind,
            name,
            token_id,
        };
        self.append(&event).await;
    }

    async fn append(&self, event: &impl Serialize) {
        self.append_if(event, || true).await;
    }

    /// [`Self::append`] when `admit`, run with the trace held, returns `true`; returns what
    /// `admit` did. No other line lands between `admit` and this one.
    async fn append_if(&self, event: &impl Serialize, admit: impl FnOnce() -> bool) -> bool {
        let Ok(mut line) = serde_json::to_string(event) else {
            return admit();
        };
        line.push('\n');
        let mut file = self.file.lock().await;
        if !admit() {
            return false;
        }
        if file.write_all(line.as_bytes()).await.is_ok() {
            let _ = file.flush().await;
        }
        true
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
    /// `trace` is the resumed trace's file name in `trace_dir`, the directory
    /// [`crate::ResumeRequest::trace_dir`] names. Errors when it cannot be read or opened for
    /// appending, which the caller reports rather than reporting work it could not mark.
    pub(crate) async fn open(
        trace_dir: &WorkdirRoot,
        trace: &str,
        session_id: String,
        session_event_id: Option<String>,
    ) -> std::io::Result<Self> {
        let contents = trace_dir
            .read_file(trace, u64::MAX)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        // An empty file counts as terminated: there is no record for an appended line to run into.
        let terminate_torn_tail = contents.last().is_some_and(|last| *last != b'\n');
        let file = File::from_std(trace_dir.open_append(trace, DEFAULT_FILE_MODE)?);
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

pub(crate) fn timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ── Plan-scheduler events ────────────────────────────────────────────────────

/// One step of a plan's DAG as authored, written into `plan_start`.
///
/// The shape is recorded once, up front, so the run stays legible for a step that never ran:
/// the trace names every step the plan declared whether or not the scheduler reached it.
#[derive(Serialize, Clone)]
pub(crate) struct PlanStepShape {
    pub(crate) step_id: String,
    /// `"tool"`, `"shell"` or `"capsule"`; `"unknown"` for a step whose kind cannot be inferred,
    /// which is a step the plan validator refuses.
    pub(crate) kind: &'static str,
    pub(crate) depends_on: Vec<String>,
    /// Whether the step carries an `if`, and so may settle without ever being dispatched.
    pub(crate) has_condition: bool,
}

/// The plan's structure, written once before any step runs. Its `event_id` is the plan node
/// every other line of the run hangs off.
#[derive(Serialize)]
struct PlanStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    plan_id: String,
    step_count: usize,
    steps: Vec<PlanStepShape>,
}

/// One dispatched step, written as the scheduler hands it to a worker.
///
/// A step that settled without ever being dispatched — an `if` that evaluated false, a
/// dependency that never resolved — writes no line here, only a terminal `plan_step`. Joins to
/// that line on `(plan_id, step_id)`.
#[derive(Serialize)]
struct PlanStepStartEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    plan_id: String,
    step_id: String,
    kind: &'static str,
    depends_on: Vec<String>,
}

/// A settled step, written by the scheduler loop after the step's `on_error` policy has been
/// applied — which is why the worker thread does not write it: a step that failed under
/// `on_error: skip` settles as skipped in the returned report and must read as skipped here.
#[derive(Serialize)]
struct PlanStepEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    plan_id: String,
    step_id: String,
    kind: &'static str,
    status: &'static str,
    attempts: u32,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The interpolated input the step dispatched, with peer handle tokens redacted. Tool steps
    /// only — a shell step's command and a capsule step's task text are not carried here.
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    /// The tool's self-declared `state_effect` for this step, lifted verbatim from
    /// `tool-result.metadata`. Absent when the tool declared nothing. Feeds the same
    /// redundant-call analysis `tool_call.state_effect` does.
    #[serde(skip_serializing_if = "Option::is_none")]
    state_effect: Option<String>,
    /// The resource this step addressed, as the tool declared it. Opaque and never parsed here.
    #[serde(skip_serializing_if = "Option::is_none")]
    resource_id: Option<String>,
}

/// The run's summary, written once as `plan::execute` returns, whatever ended it.
#[derive(Serialize)]
struct PlanEndEvent {
    event_type: &'static str,
    event_id: String,
    parent_id: Option<String>,
    session_id: String,
    timestamp: u64,
    plan_id: String,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_step: Option<String>,
    steps_total: usize,
    steps_succeeded: usize,
    steps_failed: usize,
    steps_skipped: usize,
    duration_ms: u64,
    /// Why the run ended when the reason was not a step's own failure: a plan that would not
    /// parse, a cgroup scope the host refused, a DAG with nothing left to run.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Everything the scheduler loop knows about a settled step, as one argument.
pub(crate) struct PlanStepRecord {
    pub(crate) plan_id: String,
    pub(crate) step_id: String,
    pub(crate) kind: &'static str,
    /// `"success"`, `"failed"` or `"skipped"` — the status the returned `ExecutionReport` carries
    /// for this step, so the two can never disagree.
    pub(crate) status: &'static str,
    /// How many times the step was dispatched. `0` for a step that settled without dispatch.
    pub(crate) attempts: u32,
    pub(crate) duration_ms: u64,
    pub(crate) error: Option<String>,
    pub(crate) input: Option<Value>,
    pub(crate) state_effect: Option<String>,
    pub(crate) resource_id: Option<String>,
}

/// Everything the scheduler knows about a finished run, as one argument.
pub(crate) struct PlanEndRecord {
    pub(crate) plan_id: String,
    pub(crate) outcome: &'static str,
    pub(crate) failed_step: Option<String>,
    pub(crate) steps_total: usize,
    pub(crate) steps_succeeded: usize,
    pub(crate) steps_failed: usize,
    pub(crate) steps_skipped: usize,
    pub(crate) duration_ms: u64,
    pub(crate) reason: Option<String>,
}

/// A third `O_APPEND` handle to the session's `trace.jsonl`, held by a plan run.
///
/// Synchronous where [`ResourceTraceAppender`] is async, because [`crate::plan::execute`] is: it
/// runs its ready set on plain threads inside a `thread::scope`, so there is no runtime to await
/// on and no `await` for a lock to be held across. Interleaving with [`TraceWriter`] and with
/// the resource plane is safe on the same terms they interleave with each other — one complete
/// line per `write_all`, then flush, which `O_APPEND` makes atomic against the other writers.
///
/// Handing one of these to a [`crate::plan::SchedulerContext`] is the whole of the opt-in: a
/// context carrying `None` opens no file and writes no line.
pub struct PlanTraceAppender {
    file: std::sync::Mutex<std::fs::File>,
    session_id: String,
    /// The session node a run's `plan_start` hangs off — [`TraceWriter`]'s own
    /// `session_start`, handed over at open time so an appended line names a real parent.
    session_event_id: String,
}

impl PlanTraceAppender {
    pub fn open(
        records_root: &WorkdirRoot,
        session_id: String,
        session_event_id: String,
    ) -> std::io::Result<Self> {
        let file = records_root.open_append(TRACE_FILE_NAME, DEFAULT_FILE_MODE)?;
        Ok(Self {
            file: std::sync::Mutex::new(file),
            session_id,
            session_event_id,
        })
    }

    /// Records the plan's DAG and returns the plan node every later line of this run hangs off.
    pub(crate) fn write_plan_start(&self, plan_id: &str, steps: Vec<PlanStepShape>) -> String {
        let event_id = new_event_id();
        let event = PlanStartEvent {
            event_type: "plan_start",
            event_id: event_id.clone(),
            parent_id: Some(self.session_event_id.clone()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            plan_id: plan_id.to_string(),
            step_count: steps.len(),
            steps,
        };
        self.append(&event);
        event_id
    }

    pub(crate) fn write_plan_step_start(
        &self,
        plan_node: &str,
        plan_id: &str,
        step_id: &str,
        kind: &'static str,
        depends_on: Vec<String>,
    ) {
        let event = PlanStepStartEvent {
            event_type: "plan_step_start",
            event_id: new_event_id(),
            parent_id: Some(plan_node.to_string()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            plan_id: plan_id.to_string(),
            step_id: step_id.to_string(),
            kind,
            depends_on,
        };
        self.append(&event);
    }

    pub(crate) fn write_plan_step(&self, plan_node: &str, record: PlanStepRecord) {
        // On the same terms `TraceWriter::write_tool_call` redacts a tool call's input: a peer
        // handle is a credential and this file is durable.
        let input = record.input.map(|mut value| {
            crate::peer_handoff::redact_handles_in_json(&mut value);
            value
        });
        let event = PlanStepEvent {
            event_type: "plan_step",
            event_id: new_event_id(),
            parent_id: Some(plan_node.to_string()),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            plan_id: record.plan_id,
            step_id: record.step_id,
            kind: record.kind,
            status: record.status,
            attempts: record.attempts,
            duration_ms: record.duration_ms,
            error: record.error,
            input,
            state_effect: record.state_effect,
            resource_id: record.resource_id,
        };
        self.append(&event);
    }

    /// Records the run's summary. `plan_node` is `None` for a run that never had one — a plan
    /// file that would not parse describes no DAG — and the line hangs off the session instead.
    pub(crate) fn write_plan_end(&self, plan_node: Option<&str>, record: PlanEndRecord) {
        let event = PlanEndEvent {
            event_type: "plan_end",
            event_id: new_event_id(),
            parent_id: Some(
                plan_node
                    .map(str::to_string)
                    .unwrap_or_else(|| self.session_event_id.clone()),
            ),
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            plan_id: record.plan_id,
            outcome: record.outcome,
            failed_step: record.failed_step,
            steps_total: record.steps_total,
            steps_succeeded: record.steps_succeeded,
            steps_failed: record.steps_failed,
            steps_skipped: record.steps_skipped,
            duration_ms: record.duration_ms,
            reason: record.reason,
        };
        self.append(&event);
    }

    /// Records one child a `capsule` step launched, while its delegation is still running.
    ///
    /// The synchronous twin of [`ResourceTraceAppender::write_delegation_start`]. Both go through
    /// [`DelegationStartEvent::new`], so the two lines are identical but for their ids and
    /// clocks — held by there being one constructor rather than by a test.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_delegation_start(
        &self,
        delegation_id: &str,
        capsule: &str,
        version: &str,
        child_session_id: &str,
        child_workdir: &str,
        child_records_dir: &str,
        workdir_snapshot: Option<&crate::workdir_snapshot::WorkdirSnapshot>,
    ) {
        let event = DelegationStartEvent::new(
            &self.session_id,
            &self.session_event_id,
            delegation_id,
            capsule,
            version,
            child_session_id,
            child_workdir,
            child_records_dir,
            workdir_snapshot,
        );
        self.append(&event);
    }

    /// Records one `capsule` step's delegation ending. `delegation_id` and `child_session_id` are
    /// `None` when no child was launched, on the same terms
    /// [`ResourceTraceAppender::write_delegation`] passes them.
    ///
    /// One pair per *attempt*: a step with retries delegates once per attempt, so it writes one
    /// of these per attempt. Two `failed` lines and a `completed` one is what happened, not a
    /// duplicate of one thing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_delegation(
        &self,
        capsule: &str,
        version: &str,
        delegation_id: Option<String>,
        child_session_id: Option<String>,
        duration_ms: u64,
        outcome: &str,
        reason: Option<String>,
    ) {
        let event = DelegationEvent::new(
            &self.session_id,
            &self.session_event_id,
            capsule,
            version,
            delegation_id,
            child_session_id,
            duration_ms,
            outcome,
            reason,
        );
        self.append(&event);
    }

    /// Writes one complete line and flushes it. Every failure is swallowed, exactly as
    /// [`ResourceTraceAppender::append`] swallows one: a trace that cannot be written must never
    /// turn a step that ran into a step that failed.
    fn append(&self, event: &impl Serialize) {
        use std::io::Write as _;

        let Ok(mut line) = serde_json::to_string(event) else {
            return;
        };
        line.push('\n');
        let Ok(mut file) = self.file.lock() else {
            return;
        };
        if file.write_all(line.as_bytes()).is_ok() {
            let _ = file.flush();
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sandbox::EnforcementTier, sealed::UsernsGrant, trace_blobs::BLOB_DIR_NAME,
        types::CapabilityPolicy,
    };
    use murmur_artifact::{ArtifactRuntime, InterpreterRuntimeDir, InterpreterRuntimeGrant};
    use serde_json::Value;
    use std::path::Path;

    use crate::origin::TaskOrigin;

    /// The class an A2A task carrying no origin claim resolves to.
    fn event_provenance() -> TaskProvenance {
        TaskProvenance::derive(TaskOrigin::Event, None)
    }

    /// A [`ScopeReport`] built the way production builds one — through
    /// `containment::report_for` with an *explicit* tier, never a live probe, so these
    /// tests assert the same values on a Landlock-capable host and a laptop that has none.
    fn report_for(
        declared: ContainmentClass,
        tier: EnforcementTier,
        workdir_exec: bool,
    ) -> ScopeReport {
        crate::containment::report_for(
            &CapabilityPolicy {
                workdir_exec_allowed: workdir_exec,
                ..CapabilityPolicy::default()
            },
            declared,
            tier,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(tier, declared)),
            crate::cgroup::IoMaxReport::default(),
        )
    }

    fn demotion_info() -> crate::detached::DetachedDispatchInfo {
        crate::detached::DetachedDispatchInfo {
            work_id: "wrk_0a1b2c3d4e5f6a7b".to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "make -j8".to_string(),
            grace_ms: Some(1_000),
        }
    }

    /// A demotion whose marker cannot be written stands anyway: the write failure is reported and
    /// swallowed, and the session carries on with the command running in the background.
    #[tokio::test]
    async fn a_demotion_marker_that_cannot_be_written_does_not_fail_the_demotion() {
        let dir = tempfile::tempdir().unwrap();
        // `/dev/full` opens and fails every write with `ENOSPC`, which is the one way to get a
        // trace handle that is valid and unwritable without root. Swapped in under the writer,
        // because a link at `trace.jsonl` is refused when the writer opens it.
        let mut writer = make_writer(dir.path()).await;
        let full = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        writer.writer = BufWriter::new(File::from_std(full));
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
        assert_eq!(events[0]["grace_ms"], 1_000);
    }

    /// A demotion with no grace in force carries no `grace_ms` key at all.
    #[tokio::test]
    async fn a_demotion_with_no_grace_writes_no_grace_ms() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        let info = crate::detached::DetachedDispatchInfo {
            grace_ms: None,
            ..demotion_info()
        };
        crate::agent::record_demotion(&mut writer, 3, &info).await;

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "shell_detached");
        assert!(events[0].get("grace_ms").is_none(), "{}", events[0]);
    }

    #[tokio::test]
    async fn a_safety_check_event_carries_the_checks_verbatim_and_its_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        let checks = serde_json::json!([{"id": "sc_1", "code": "c", "message": "m", "x": 1}]);
        writer
            .write_safety_check(2, "call_1_0", "computer", &checks, "declined")
            .await
            .unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "safety_check");
        assert_eq!(events[0]["turn"], 2);
        assert_eq!(events[0]["tool_call_id"], "call_1_0");
        assert_eq!(events[0]["tool"], "computer");
        assert_eq!(events[0]["checks"], checks);
        assert_eq!(events[0]["outcome"], "declined");
        assert!(events[0]["event_id"].is_string() && events[0]["session_id"].is_string());
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

    /// A command still running when the session ended has no exit code, no output file and no
    /// byte count, and its line carries none of the three — omitted rather than `null`, so the
    /// shape is identical to a line written before those fields existed.
    #[tokio::test]
    async fn an_abandoned_command_still_running_omits_the_fields_it_cannot_know() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        writer
            .write_shell_abandoned(
                "wrk_0a1b2c3d4e5f6a7b",
                "/usr/bin/bash",
                "sleep 45; echo done",
                45_012,
                None,
            )
            .await
            .unwrap();

        let events = read_events(dir.path());
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event["event_type"], "shell_abandoned");
        assert_eq!(event["work_id"], "wrk_0a1b2c3d4e5f6a7b");
        assert_eq!(event["binary"], "/usr/bin/bash");
        assert_eq!(event["command"], "sleep 45; echo done");
        assert_eq!(event["running_ms"], 45_012);
        for absent in ["exit_code", "output_path", "output_bytes"] {
            assert!(
                event.get(absent).is_none(),
                "{absent} must be omitted, not written as null: {event}"
            );
        }
    }

    /// A command that finished during teardown carries the exit code, the output path and the
    /// byte count that do exist for it, so a trace reader can tell it from one still running
    /// without reading anything else.
    #[tokio::test]
    async fn an_abandoned_command_that_finished_carries_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = make_writer(dir.path()).await;
        writer
            .write_shell_abandoned(
                "wrk_0a1b2c3d4e5f6a7b",
                "/usr/bin/bash",
                "sleep 2; echo done",
                2_014,
                Some((
                    0,
                    "/home/test/.murmur/records/work/ses_1/logs/wrk_0a1b2c3d4e5f6a7b.log",
                    91,
                )),
            )
            .await
            .unwrap();

        let event = &read_events(dir.path())[0];
        assert_eq!(event["event_type"], "shell_abandoned");
        assert_eq!(event["running_ms"], 2_014);
        assert_eq!(event["exit_code"], 0);
        assert_eq!(
            event["output_path"],
            "/home/test/.murmur/records/work/ses_1/logs/wrk_0a1b2c3d4e5f6a7b.log"
        );
        assert_eq!(event["output_bytes"], 91);
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
            &crate::workdir_fs::WorkdirRoot::open(dir).unwrap(),
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
        w.write_session_start(Some(10), None, vec!["bash".to_string()])
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
        assert_eq!(e["model_limits"], serde_json::Value::Null);
    }

    /// No turn cap is written as `null`, beside the primary's limits and their sources.
    #[tokio::test]
    async fn session_start_writes_no_turn_cap_and_the_model_limits() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let limits = crate::model_limits::ResolvedModelLimits {
            max_output_tokens: 65536,
            max_output_tokens_source: crate::model_limits::LimitSource::Driver,
            context_window: None,
            context_window_source: crate::model_limits::LimitSource::None,
            max_input_tokens: None,
            max_input_tokens_source: crate::model_limits::LimitSource::None,
            input_budget: None,
            input_budget_basis: crate::model_limits::BudgetBasis::None,
        };
        w.write_session_start(None, Some(limits), Vec::new())
            .await
            .unwrap();
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        assert!(e.get("max_turns").is_some_and(serde_json::Value::is_null));
        assert_eq!(
            e["model_limits"],
            serde_json::json!({
                "max_output_tokens": 65536,
                "max_output_tokens_source": "driver",
                "context_window": null,
                "context_window_source": "none",
                "max_input_tokens": null,
                "max_input_tokens_source": "none",
                "input_budget": null,
                "input_budget_basis": "none",
            })
        );
    }

    /// A member's formation is on the trace's first line, which is all a reader grouping sessions
    /// reads; a session in no formation writes no key at all.
    #[tokio::test]
    async fn session_start_is_the_first_line_and_carries_a_members_formation() {
        let formation = FormationId::mint();
        for member in [Some(&formation), None] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            w.set_formation_id(member);
            w.write_session_start(Some(10), None, Vec::new())
                .await
                .unwrap();
            w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
                .await
                .unwrap();
            w.flush().await.unwrap();

            let content = std::fs::read_to_string(dir.path().join("trace.jsonl")).unwrap();
            let first_line = content.lines().next().unwrap();
            let first: Value = serde_json::from_str(first_line).unwrap();
            assert_eq!(first["event_type"], "session_start");
            match member {
                Some(id) => {
                    assert_eq!(first["formation_id"], id.as_str());
                    assert!(
                        first_line.contains(&format!("\"formation_id\":\"{id}\"")),
                        "{first_line}"
                    );
                }
                None => assert!(!first_line.contains("formation_id"), "{first_line}"),
            }
            // Only the session frame carries it.
            assert_eq!(
                content.matches("formation_id").count(),
                usize::from(member.is_some())
            );
        }
    }

    /// A member's `session_start` names it and its callees, `[]` for none, and only a member's
    /// does; `a2a_task_received.caller_member` is present only for a task a formation token let
    /// in.
    #[tokio::test]
    async fn a_members_trace_names_it_its_callees_and_its_callers() {
        let authority = crate::formation_credentials::FormationAuthority::for_test();
        let cases = [
            (Some(("coder", vec!["reviewer"])), Some("planner")),
            (Some(("reviewer", vec![])), None),
            (None, None),
        ];
        for (member, caller) in cases {
            let member = member.map(|(name, callees)| {
                crate::formation_credentials::FormationMember::from_bundle(
                    authority.member_bundle(name, &callees),
                )
            });
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            w.set_formation_member(member.as_ref());
            w.write_session_start(Some(10), None, Vec::new())
                .await
                .unwrap();
            let door_caller = match caller {
                Some(caller) => crate::identity::TaskCaller::Credential(format!("member:{caller}")),
                None => crate::identity::TaskCaller::Operator,
            };
            w.write_a2a_task_received("tsk_1", "ctx_1", "msg_1", None, &door_caller, caller, "hi")
                .await
                .unwrap();
            w.flush().await.unwrap();
            let content = std::fs::read_to_string(dir.path().join("trace.jsonl")).unwrap();
            let lines: Vec<Value> = content
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let start = &lines[0];
            let received = lines
                .iter()
                .find(|line| line["event_type"] == "a2a_task_received")
                .unwrap();
            match &member {
                Some(member) => {
                    assert_eq!(start["formation_member"], member.name());
                    assert_eq!(
                        start["formation_callees"],
                        serde_json::json!(member.callees().collect::<Vec<_>>())
                    );
                }
                None => {
                    assert!(start.get("formation_member").is_none(), "{start}");
                    assert!(start.get("formation_callees").is_none(), "{start}");
                }
            }
            match caller {
                Some(caller) => {
                    assert_eq!(received["caller"], "member");
                    assert_eq!(received["caller_member"], caller);
                }
                None => {
                    assert_eq!(received["caller"], "operator");
                    assert!(received.get("caller_member").is_none(), "{received}");
                }
            }
            assert!(received.get("caller_credential").is_none(), "{received}");
            assert!(!content.contains("mft1."), "{content}");
        }
    }

    /// `a2a_task_received.caller` names each way a door lets a task in, with `caller_member` only
    /// for a member and `caller_credential` only for a declared credential; a formation token's
    /// task is the member's whatever credential the token carries.
    #[tokio::test]
    async fn a_received_task_names_who_the_door_let_it_in_as() {
        use crate::identity::TaskCaller;
        let cases = [
            (TaskCaller::Operator, None, "operator", None),
            (TaskCaller::Public, None, "public", None),
            (
                TaskCaller::Credential("alice".to_string()),
                None,
                "credential",
                Some("alice"),
            ),
            (
                TaskCaller::Credential("member:planner".to_string()),
                Some("planner"),
                "member",
                None,
            ),
        ];
        for (door_caller, member, caller, credential) in cases {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            w.write_session_start(Some(10), None, Vec::new())
                .await
                .unwrap();
            w.write_a2a_task_received("tsk_1", "ctx_1", "msg_1", None, &door_caller, member, "hi")
                .await
                .unwrap();
            w.flush().await.unwrap();
            let received = read_events(dir.path())
                .into_iter()
                .find(|line| line["event_type"] == "a2a_task_received")
                .unwrap();
            assert_eq!(received["caller"], caller, "{received}");
            match member {
                Some(member) => assert_eq!(received["caller_member"], member, "{received}"),
                None => assert!(received.get("caller_member").is_none(), "{received}"),
            }
            match credential {
                Some(name) => assert_eq!(received["caller_credential"], name, "{received}"),
                None => assert!(received.get("caller_credential").is_none(), "{received}"),
            }
        }
    }

    /// The received message's hash is written under `meta` and `content`, and only `content`
    /// stores the text behind it; `none` writes neither. The text never reaches the JSONL.
    #[tokio::test]
    async fn a_received_message_is_hashed_and_stored_by_capture_mode() {
        let text = "Name three colours, please.";
        let sha = murmur_artifact::sha256_hex(text.as_bytes());
        for capture in [
            TraceCapture::None,
            TraceCapture::Meta,
            TraceCapture::Content,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer_with_opts(dir.path(), capture).await;
            w.write_a2a_task_received(
                "tsk_1",
                "ctx_1",
                "msg_1",
                None,
                &crate::identity::TaskCaller::Operator,
                None,
                text,
            )
            .await
            .unwrap();
            w.flush().await.unwrap();
            let content = std::fs::read_to_string(dir.path().join("trace.jsonl")).unwrap();
            assert!(!content.contains(text), "{content}");
            let received = read_events(dir.path())
                .into_iter()
                .find(|line| line["event_type"] == "a2a_task_received")
                .unwrap();
            let blob = std::fs::read(blob_path(dir.path(), &sha)).ok();
            match capture {
                TraceCapture::None => {
                    assert!(received.get("message_sha256").is_none(), "{received}");
                    assert_eq!(blob, None);
                }
                TraceCapture::Meta => {
                    assert_eq!(received["message_sha256"], sha.as_str(), "{received}");
                    assert_eq!(blob, None);
                }
                TraceCapture::Content => {
                    assert_eq!(received["message_sha256"], sha.as_str(), "{received}");
                    assert_eq!(blob.as_deref(), Some(text.as_bytes()));
                }
            }
        }
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
            w.write_session_start(Some(1), None, Vec::new())
                .await
                .unwrap();
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
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
            &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
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
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
            &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
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
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
            let report = crate::containment::report_for(
                &CapabilityPolicy::default(),
                ContainmentClass::Sealed,
                EnforcementTier::KernelSealed,
                None,
                Some(grant),
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    EnforcementTier::KernelSealed,
                    ContainmentClass::Sealed,
                )),
                crate::cgroup::IoMaxReport::default(),
            );
            let dir = tempfile::tempdir().unwrap();
            let mut w = TraceWriter::open(
                &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
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
                None,
                None,
            )
            .await
            .unwrap();
            w.write_session_start(Some(1), None, Vec::new())
                .await
                .unwrap();
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
        let unprobed = crate::containment::report_for(
            &CapabilityPolicy::default(),
            ContainmentClass::Advisory,
            EnforcementTier::EnvironmentOnly,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::EnvironmentOnly,
                ContainmentClass::Advisory,
            )),
            crate::cgroup::IoMaxReport::default(),
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
        let expected = crate::containment::report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            // A kernel above the cap, so `landlock_abi` is written as an integer, not `null`.
            Some(8),
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
            vec![crate::containment::PreopenReport {
                artifact: "notes-tool".to_string(),
                role: "tool".to_string(),
                scope: None,
                surface: crate::containment::PreopenSurface::WholeWorkdir,
                runtime_dir_reach: Some(crate::containment::RuntimeDirReach::MurmurRuntime),
            }],
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            crate::cgroup::IoMaxReport::default(),
        );

        let dir = tempfile::tempdir().unwrap();
        let mut w = TraceWriter::open(
            &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
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
            None,
            None,
        )
        .await
        .unwrap();
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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
        // The per-artifact preopen set reaches the trace with no trace-side plumbing: the report
        // is cloned whole, so `filesystem_scope` above (which narrows no guest) and the surface a
        // guest is actually opened into are both on the record.
        assert_eq!(
            events[0]["effective_grants"]["preopens"],
            serde_json::json!([{
                "artifact": "notes-tool",
                "role": "tool",
                "scope": null,
                "surface": "whole-workdir",
                "runtime_dir_reach": "murmur-runtime",
            }])
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
            Some(100),
            Some(50),
            "tool_call".to_string(),
            Some("tool_call"),
            Some("bash".to_string()),
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            None,
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
            thinking_tokens: None,
        };
        w.write_inference(
            0,
            Some(100),
            Some(50),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            Some(&usage),
            Vec::new(),
            None,
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
    async fn failed_inference_writes_its_keys_and_counts_only_what_it_spent() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_opts(dir.path(), TraceCapture::Content).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        let usage = DriverUsage {
            input_tokens: Some(7),
            ..DriverUsage::default()
        };
        let long = "é".repeat(MAX_TASK_FAILURE_REASON_BYTES);
        w.write_failed_inference(FailedInference {
            turn: 0,
            error_code: INFERENCE_ERROR_CREDENTIAL_REJECTED,
            error: &long,
            provider_status: Some(401),
            usage: Some(&usage),
            origin: None,
            choice: None,
            message_ids: vec!["msg_1".to_string()],
            tool_name: None,
            spent: None,
            settings: None,
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
        .unwrap();
        assert_eq!(w.task_turns(), 0, "an uncounted record moves no counter");
        assert_eq!((w.total_turns, w.total_input_tokens), (0, 0));
        assert_eq!((w.task_input_tokens, w.task_output_tokens), (0, 0));

        w.write_failed_inference(FailedInference {
            turn: 1,
            error_code: INFERENCE_ERROR_HARNESS_AUTH,
            error: "bad login",
            provider_status: None,
            usage: None,
            origin: None,
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: Some((Some(10), Some(5))),
            settings: None,
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
        .unwrap();
        assert_eq!(w.task_turns(), 1);
        assert_eq!(w.total_turns, 1);
        assert_eq!((w.total_input_tokens, w.total_output_tokens), (10, 5));
        assert_eq!((w.task_input_tokens, w.task_output_tokens), (10, 5));
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let failed: Vec<&Value> = events
            .iter()
            .filter(|e| e["event_type"] == "inference")
            .collect();
        let keys = |e: &Value| {
            let mut keys: Vec<String> = e.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(
            keys(failed[0]),
            [
                "decision",
                "error",
                "error_code",
                "event_id",
                "event_type",
                "input_tokens_actual",
                "message_ids",
                "parent_id",
                "provider_status",
                "session_id",
                "stop_reason",
                "task_id",
                "timestamp",
                "tool_name",
                "turn",
            ]
        );
        assert_eq!(failed[0]["decision"], "error");
        assert_eq!(failed[0]["stop_reason"], "error");
        assert_eq!(failed[0]["error_code"], "credential_rejected");
        assert_eq!(failed[0]["provider_status"], 401);
        assert_eq!(failed[0]["input_tokens_actual"], 7);
        let error = failed[0]["error"].as_str().unwrap();
        assert!(error.len() <= MAX_TASK_FAILURE_REASON_BYTES);
        assert!(error.len() > MAX_TASK_FAILURE_REASON_BYTES - 2);

        assert_eq!(failed[1]["error_code"], "harness_auth");
        assert_eq!(failed[1]["error"], "bad login");
        assert_eq!(failed[1]["input_tokens"], 10);
        assert_eq!(failed[1]["output_tokens"], 5);
        assert!(failed[1].get("provider_status").is_none());
        assert!(
            !dir.path().join("blobs").exists(),
            "a failed record stores no blob, even under trace.capture: content"
        );
        // The agent-loop failed record is the turn node, parented to the task.
        let task_start = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        assert_eq!(failed[0]["parent_id"], task_start["event_id"]);
    }

    /// The gateway timing keys: absent with no runtime gateway, `null` for a moment that never
    /// came, numbers for one that did.
    #[tokio::test]
    async fn gateway_timing_keys_follow_the_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let failed = |turn, gateway| FailedInference {
            turn,
            error_code: INFERENCE_ERROR_DRIVER_FAILED,
            error: "stopped",
            provider_status: None,
            usage: None,
            origin: None,
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: None,
            settings: None,
            retry: RetryNote::default(),
            gateway,
        };
        w.write_failed_inference(failed(0, None)).await.unwrap();
        w.write_failed_inference(failed(1, Some(GatewayOffsets::default())))
            .await
            .unwrap();
        let answered = GatewayOffsets {
            reached_ms: Some(4),
            responded_ms: Some(90),
        };
        w.write_failed_inference(failed(2, Some(answered)))
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert!(events[0].get("gateway_reached_ms").is_none());
        assert!(events[0].get("upstream_responded_ms").is_none());
        assert_eq!(events[1]["gateway_reached_ms"], Value::Null);
        assert_eq!(events[1]["upstream_responded_ms"], Value::Null);
        assert!(events[1]
            .as_object()
            .unwrap()
            .contains_key("gateway_reached_ms"));
        assert!(events[1]
            .as_object()
            .unwrap()
            .contains_key("upstream_responded_ms"));
        assert_eq!(events[2]["gateway_reached_ms"], 4);
        assert_eq!(events[2]["upstream_responded_ms"], 90);
    }

    /// A firing in the latch is written as one `call_deadline` line ahead of the next line the
    /// writer writes, inside the task and under the previous turn, and is not written twice. One
    /// from before the session frame follows `session_start`.
    #[tokio::test]
    async fn call_deadlines_are_drained_ahead_of_the_next_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let deadlines = crate::limits::CallDeadlines::default();
        w.set_call_deadlines(deadlines.clone());
        let staged = |component: &str, call: &str| crate::limits::CallDeadline {
            component: component.to_string(),
            role: crate::limits::ComponentRole::Hook,
            call: call.to_string(),
            deadline_seconds: 2,
            elapsed_ms: 4_000,
        };
        deadlines.push_for_test(staged("stager", "on-stage"));
        w.write_session_start(None, None, Vec::new()).await.unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        deadlines.push_for_test(crate::limits::CallDeadline {
            component: "stall-driver".to_string(),
            role: crate::limits::ComponentRole::Driver,
            call: "run".to_string(),
            deadline_seconds: 3,
            elapsed_ms: 5_001,
        });
        w.write_failed_inference(FailedInference {
            turn: 0,
            error_code: INFERENCE_ERROR_DRIVER_FAILED,
            error: "stopped",
            provider_status: None,
            usage: None,
            origin: None,
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: None,
            settings: None,
            retry: RetryNote::default(),
            gateway: Some(GatewayOffsets::default()),
        })
        .await
        .unwrap();
        w.write_session_end("ok").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let types: Vec<&str> = events
            .iter()
            .map(|e| e["event_type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "session_start",
                "call_deadline",
                "task_start",
                "call_deadline",
                "inference",
                "session_end"
            ]
        );
        assert_eq!(events[1]["call"], "on-stage");
        assert_eq!(events[1]["parent_id"], events[0]["event_id"]);
        assert!(events[1].get("task_id").is_none(), "outside a task");
        let cut = &events[3];
        assert_eq!(cut["component"], "stall-driver");
        assert_eq!(cut["role"], "driver");
        assert_eq!(cut["call"], "run");
        assert_eq!(cut["deadline_seconds"], 3);
        assert_eq!(cut["elapsed_ms"], 5_001);
        assert_eq!(cut["task_id"], "tsk_1");
        assert_eq!(cut["parent_id"], events[2]["event_id"]);
        assert!(cut["event_id"].as_str().is_some());
        assert!(cut["timestamp"].as_u64().is_some());
    }

    #[tokio::test]
    async fn a_failed_hook_record_hangs_off_the_turn_and_keeps_hook_accounting() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        let origin = InferenceOrigin {
            source: "hook:compactor".to_string(),
            model: "m".to_string(),
        };
        w.write_failed_inference(FailedInference {
            turn: 0,
            error_code: INFERENCE_ERROR_DRIVER_ERROR,
            error: "HTTP 500: boom",
            provider_status: Some(500),
            usage: None,
            origin: Some(&origin),
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: Some((Some(40), None)),
            settings: None,
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
        .unwrap();
        assert_eq!(w.total_turns, 2);
        assert_eq!((w.total_input_tokens, w.total_output_tokens), (50, 5));
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let turn = events.iter().find(|e| e["decision"] == "end_turn").unwrap();
        let hook = events.iter().find(|e| e["decision"] == "error").unwrap();
        assert_eq!(hook["parent_id"], turn["event_id"]);
        assert_eq!(hook["origin"], "hook:compactor");
        assert_eq!(hook["model"], "m");
        assert_eq!(hook["input_tokens"], 40);
        assert!(hook.get("output_tokens").is_none());
    }

    /// Every `http` line names how its call was sent: the cap, the input budget and the driver
    /// config, every key present, `input_budget` written as `null` with no window and
    /// `driver_config` as `null` when the driver saw none.
    #[tokio::test]
    async fn inference_call_settings_are_written_on_every_kind_of_http_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let turn = InferenceCallSettings {
            max_output_tokens: 4096,
            input_budget: Some(195_904),
            driver_config: Some(serde_json::json!({"reasoning_effort": "high"})),
        };
        let unconfigured = InferenceCallSettings {
            max_output_tokens: 1024,
            input_budget: None,
            driver_config: None,
        };
        let hook = InferenceCallSettings {
            max_output_tokens: 2000,
            input_budget: Some(198_000),
            driver_config: Some(serde_json::json!({"reasoning_effort": "low"})),
        };
        let origin = InferenceOrigin {
            source: "hook:compactor".to_string(),
            model: "m".to_string(),
        };
        w.write_turn_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            Vec::new(),
            None,
            None,
            Some(&turn),
            RetryNote::default(),
            None,
        )
        .await
        .unwrap();
        w.write_turn_inference(
            1,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            Vec::new(),
            None,
            None,
            Some(&unconfigured),
            RetryNote::default(),
            None,
        )
        .await
        .unwrap();
        w.write_inference(
            1,
            Some(30),
            Some(8),
            "end_turn".to_string(),
            None,
            None,
            Some(&origin),
            None,
            Vec::new(),
            None,
            Some(&hook),
        )
        .await
        .unwrap();
        w.write_failed_inference(FailedInference {
            turn: 2,
            error_code: INFERENCE_ERROR_DRIVER_ERROR,
            error: "HTTP 500: boom",
            provider_status: Some(500),
            usage: None,
            origin: None,
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: None,
            settings: Some(&turn),
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let settings: Vec<(Value, Value, Value, Value)> = events
            .iter()
            .filter(|e| e["event_type"] == "inference")
            .map(|e| {
                (
                    e["decision"].clone(),
                    e["max_output_tokens"].clone(),
                    e.get("input_budget")
                        .cloned()
                        .expect("input_budget is written, null or not"),
                    e.get("driver_config")
                        .cloned()
                        .expect("driver_config is written, null or not"),
                )
            })
            .collect();
        assert_eq!(
            settings,
            [
                (
                    serde_json::json!("end_turn"),
                    serde_json::json!(4096),
                    serde_json::json!(195_904),
                    serde_json::json!({"reasoning_effort": "high"})
                ),
                (
                    serde_json::json!("end_turn"),
                    serde_json::json!(1024),
                    Value::Null,
                    Value::Null
                ),
                (
                    serde_json::json!("end_turn"),
                    serde_json::json!(2000),
                    serde_json::json!(198_000),
                    serde_json::json!({"reasoning_effort": "low"})
                ),
                (
                    serde_json::json!("error"),
                    serde_json::json!(4096),
                    serde_json::json!(195_904),
                    serde_json::json!({"reasoning_effort": "high"})
                ),
            ]
        );
    }

    /// A line written without settings — every `process` line — carries none of the keys.
    #[tokio::test]
    async fn an_inference_line_without_call_settings_writes_neither_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_failed_inference(FailedInference {
            turn: 1,
            error_code: INFERENCE_ERROR_HARNESS_AUTH,
            error: "bad login",
            provider_status: None,
            usage: None,
            origin: None,
            choice: None,
            message_ids: Vec::new(),
            tool_name: None,
            spent: None,
            settings: None,
            retry: RetryNote::default(),
            gateway: None,
        })
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let lines: Vec<&Value> = events
            .iter()
            .filter(|e| e["event_type"] == "inference")
            .collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert!(line.get("max_output_tokens").is_none(), "{line}");
            assert!(line.get("input_budget").is_none(), "{line}");
            assert!(line.get("driver_config").is_none(), "{line}");
        }
    }

    fn driver_config_env(value: &str) -> Vec<(String, String)> {
        vec![
            ("MURMUR_INFERENCE_MODEL".to_string(), "gpt-5".to_string()),
            (
                "MURMUR_INFERENCE_DRIVER_CONFIG".to_string(),
                value.to_string(),
            ),
        ]
    }

    fn redact_secret(text: &str) -> String {
        text.replace("sk-secret", crate::gateway_credential::REDACTED)
    }

    #[test]
    fn recorded_driver_config_is_none_without_the_variable() {
        let env = vec![("MURMUR_INFERENCE_MODEL".to_string(), "gpt-5".to_string())];
        assert_eq!(recorded_driver_config(&env, redact_secret), None);
    }

    #[test]
    fn recorded_driver_config_is_the_parsed_object() {
        assert_eq!(
            recorded_driver_config(
                &driver_config_env(r#"{"reasoning_effort":"low"}"#),
                redact_secret
            ),
            Some(serde_json::json!({"reasoning_effort": "low"}))
        );
    }

    #[test]
    fn recorded_driver_config_redacts_keys_and_string_leaves_at_any_depth() {
        let env = driver_config_env(
            r#"{"note":"x-sk-secret-y","nested":{"list":["sk-secret"]},"sk-secret":1}"#,
        );
        assert_eq!(
            recorded_driver_config(&env, redact_secret),
            Some(serde_json::json!({
                "note": "x-[redacted]-y",
                "nested": {"list": ["[redacted]"]},
                "[redacted]": 1,
            }))
        );
    }

    #[test]
    fn recorded_driver_config_replaces_a_number_the_redactor_changes() {
        let redact_pin = |text: &str| text.replace("424242", crate::gateway_credential::REDACTED);
        let env = driver_config_env(r#"{"pin":424242,"retries":3,"store":false,"seed":null}"#);
        assert_eq!(
            recorded_driver_config(&env, redact_pin),
            Some(serde_json::json!({
                "pin": "[redacted]",
                "retries": 3,
                "store": false,
                "seed": null,
            }))
        );
    }

    #[test]
    fn recorded_driver_config_records_text_that_is_not_json_as_a_redacted_string() {
        assert_eq!(
            recorded_driver_config(&driver_config_env("effort=sk-secret"), redact_secret),
            Some(Value::String("effort=[redacted]".to_string()))
        );
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
            None,
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
        w.write_compaction(3, 80000, OccupancyBasis::Actual, 20000)
            .await
            .unwrap();
        w.write_compaction(4, 30000, OccupancyBasis::Estimate, 10000)
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "compaction");
        assert_eq!(e["turn"], 3);
        assert_eq!(e["tokens_before"], 80000);
        assert_eq!(e["tokens_before_source"], "actual");
        assert_eq!(e["tokens_after"], 20000);
        assert_eq!(events[1]["tokens_before_source"], "estimate");
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
            Some(100),
            Some(50),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
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
        w.write_session_start(Some(10), None, vec![]).await.unwrap();
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
        w.write_session_start(Some(10), None, vec![]).await.unwrap();
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
        w.write_session_start(Some(10), None, vec!["bash".to_string()])
            .await
            .unwrap();
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
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
        w.write_session_start(Some(10), None, vec![]).await.unwrap();
        w.write_inference(
            0,
            Some(50),
            Some(25),
            "tool_call".to_string(),
            Some("tool_call"),
            Some("bash".to_string()),
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            None,
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
            Some(60),
            Some(30),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
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

    /// The `tool_call` record `w` writes for one failed call carrying `error`, read back.
    async fn tool_call_with_error(
        dir: &std::path::Path,
        capture: TraceCapture,
        error: Option<&str>,
    ) -> Value {
        let mut w = make_writer_with_opts(dir, capture).await;
        w.write_tool_call(
            0,
            "tool-fixture".to_string(),
            Some("toolu_1".to_string()),
            serde_json::json!({}),
            2,
            "native tool execution failed",
            28,
            3,
            "error".to_string(),
            None,
            None,
            error,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        read_events(dir).remove(0)
    }

    #[tokio::test]
    async fn tool_call_error_is_written_without_output_under_meta() {
        let dir = tempfile::tempdir().unwrap();
        let text = "sandbox: native tool enforcement setup failed before exec: Landlock: \
                    Too many open files (os error 24)";
        let e = tool_call_with_error(dir.path(), TraceCapture::Meta, Some(text)).await;
        assert_eq!(e["error"], text);
        assert!(e.get("output").is_none(), "meta stores no output: {e}");
    }

    #[tokio::test]
    async fn tool_call_error_is_written_under_every_capture() {
        for capture in [
            TraceCapture::None,
            TraceCapture::Meta,
            TraceCapture::Content,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let e = tool_call_with_error(dir.path(), capture, Some("spawn failed")).await;
            assert_eq!(e["error"], "spawn failed", "{capture:?}: {e}");
        }
    }

    #[tokio::test]
    async fn tool_call_error_is_capped_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // 3,000 characters: one ASCII byte, then two-byte characters, so the cap's byte offset
        // falls inside a character.
        let long = format!("a{}", "é".repeat(2_999));
        assert_eq!(long.chars().count(), 3_000);
        let e = tool_call_with_error(dir.path(), TraceCapture::Meta, Some(&long)).await;
        let error = e["error"].as_str().unwrap();
        assert!(error.len() <= MAX_TASK_FAILURE_REASON_BYTES);
        assert!(error.len() > MAX_TASK_FAILURE_REASON_BYTES - 2);
        assert!(long.starts_with(error));
    }

    #[tokio::test]
    async fn tool_call_error_redacts_a_peer_handle() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::peer_handoff::mint(
            &crate::peer_handoff::PeerMintKey::generate().unwrap(),
            &crate::peer_handoff::HandlePayload {
                v: 1,
                iss: "ses_1".to_string(),
                p: "report.md".to_string(),
                exp: timestamp_ms() + 3_600_000,
                n: "0123456789abcdef0123456789abcdef".to_string(),
            },
            "a@b",
        )
        .unwrap();
        let text = format!("fetch-peer-file {token} failed");
        let e = tool_call_with_error(dir.path(), TraceCapture::Meta, Some(&text)).await;
        let id = crate::peer_handoff::handle_id(&token);
        assert_eq!(e["error"], format!("fetch-peer-file <handle:{id}> failed"));
    }

    #[tokio::test]
    async fn tool_call_without_an_error_has_no_error_key() {
        let dir = tempfile::tempdir().unwrap();
        let e = tool_call_with_error(dir.path(), TraceCapture::Meta, None).await;
        assert!(e.get("error").is_none(), "{e}");
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
        w.write_skill_call(
            2,
            "my-skill".to_string(),
            &LockOrigin::Operator,
            1024,
            8,
            "ok".to_string(),
        )
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
        w.write_session_start(Some(10), None, vec![]).await.unwrap();
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
            None,
        )
        .await
        .unwrap();
        w.write_skill_call(
            1,
            "my-skill".to_string(),
            &LockOrigin::Operator,
            512,
            3,
            "ok".to_string(),
        )
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
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_task_end("tsk_1", "ok", 2, Vec::new(), None, None)
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let end = events
            .iter()
            .find(|e| e["event_type"] == "task_end")
            .unwrap();
        assert_eq!(end["task_id"], "tsk_1");
        assert_eq!(end["exit_status"], "ok");
        assert_eq!(end["reopen_count"], 2);
        assert_eq!(
            end["capped_turns"], 0,
            "always present, 0 when no turn was capped"
        );
    }

    /// `task_end.capped_turns` counts the task's agent-loop `inference` lines with
    /// `stop_reason: "max_tokens"`, and a new task starts it at 0.
    #[tokio::test]
    async fn task_end_counts_capped_turns_per_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for task in ["tsk_1", "tsk_2"] {
            w.write_task_start(task, "ctx_1", "a2a", event_provenance(), 3, &[], &[])
                .await
                .unwrap();
            let stops: &[&str] = if task == "tsk_1" {
                &["max_tokens", "tool_call", "max_tokens", "end_turn"]
            } else {
                &["end_turn"]
            };
            for (turn, stop) in stops.iter().enumerate() {
                let decision = if *stop == "max_tokens" {
                    "capped"
                } else {
                    stop
                };
                w.write_turn_inference(
                    u32::try_from(turn).unwrap(),
                    Some(10),
                    Some(5),
                    decision.to_string(),
                    Some(stop),
                    None,
                    None,
                    Vec::new(),
                    None,
                    None,
                    None,
                    RetryNote::default(),
                    None,
                )
                .await
                .unwrap();
            }
            w.write_task_end(task, "ok", 0, Vec::new(), None, None)
                .await
                .unwrap();
        }
        w.flush().await.unwrap();

        let capped: Vec<_> = read_events(dir.path())
            .iter()
            .filter(|e| e["event_type"] == "task_end")
            .map(|e| e["capped_turns"].clone())
            .collect();
        assert_eq!(capped, vec![serde_json::json!(2), serde_json::json!(0)]);
    }

    /// `task_end.keep_working` is the summary the runtime passes, `stopped_by` `null` when unset,
    /// and absent when the capsule does not set `lifecycle.keep_working`.
    #[tokio::test]
    async fn task_end_carries_keep_working_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for (task_id, keep_working) in [
            (
                "tsk_1",
                Some(KeepWorkingSummary {
                    used: 2,
                    limit: 2,
                    stopped_by: Some("limit"),
                }),
            ),
            (
                "tsk_2",
                Some(KeepWorkingSummary {
                    used: 1,
                    limit: 5,
                    stopped_by: None,
                }),
            ),
            ("tsk_3", None),
        ] {
            w.write_task_start(task_id, "ctx_1", "a2a", event_provenance(), 3, &[], &[])
                .await
                .unwrap();
            w.write_task_end(task_id, "ok", 0, Vec::new(), keep_working, None)
                .await
                .unwrap();
        }
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let ends: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "task_end")
            .collect();
        assert_eq!(
            ends[0]["keep_working"],
            serde_json::json!({"used": 2, "limit": 2, "stopped_by": "limit"})
        );
        assert_eq!(
            ends[1]["keep_working"],
            serde_json::json!({"used": 1, "limit": 5, "stopped_by": null})
        );
        assert!(ends[2].get("keep_working").is_none(), "{}", ends[2]);
    }

    /// One `keep_working` record per continuation, parented on the task, and the session-status
    /// count follows it.
    #[tokio::test]
    async fn keep_working_record_carries_its_fields_and_feeds_session_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let status = std::sync::Arc::new(crate::session_status::SessionStatus::new(
            std::time::Instant::now(),
            false,
            None,
            None,
            0,
            Some(3),
            None,
        ));
        w.set_session_status(std::sync::Arc::clone(&status));
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_keep_working("tsk_1", 1, 3, Some(4), 66)
            .await
            .unwrap();
        w.write_keep_working("tsk_1", 2, 3, None, 66).await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let start = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        let records: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "keep_working")
            .collect();
        assert_eq!(records.len(), 2);
        for (record, (number, turns)) in records
            .iter()
            .zip([(1, serde_json::json!(4)), (2, serde_json::Value::Null)])
        {
            assert_eq!(record["task_id"], "tsk_1");
            assert_eq!(record["number"], number);
            assert_eq!(record["limit"], 3);
            assert_eq!(record["turns_remaining"], turns);
            assert_eq!(record["message_bytes"], 66);
            assert_eq!(record["parent_id"], start["event_id"]);
            assert!(record["event_id"].is_string());
        }
        let spend = crate::session_status::SpendReading {
            used: 0,
            ceiling: None,
            machine: None,
        };
        assert_eq!(
            serde_json::to_value(status.snapshot(spend)).unwrap()["keep_working"],
            serde_json::json!({"used": 2, "left": 1})
        );
    }

    /// `task_end.done_check` is the summary the runtime passes and is absent with the option off.
    #[tokio::test]
    async fn task_end_carries_done_check_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for (task_id, done_check) in [
            (
                "tsk_1",
                Some(DoneCheckSummary {
                    runs: 3,
                    reopens: 2,
                    limit: 2,
                    met: Some(false),
                    stopped_by: Some("limit"),
                }),
            ),
            (
                "tsk_2",
                Some(DoneCheckSummary {
                    runs: 0,
                    reopens: 0,
                    limit: 1,
                    met: None,
                    stopped_by: None,
                }),
            ),
            ("tsk_3", None),
        ] {
            w.write_task_start(task_id, "ctx_1", "a2a", event_provenance(), 3, &[], &[])
                .await
                .unwrap();
            w.write_task_end(task_id, "ok", 0, Vec::new(), None, done_check)
                .await
                .unwrap();
        }
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let ends: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "task_end")
            .collect();
        assert_eq!(
            ends[0]["done_check"],
            serde_json::json!({"runs": 3, "reopens": 2, "limit": 2, "met": false,
                               "stopped_by": "limit"})
        );
        assert_eq!(
            ends[1]["done_check"],
            serde_json::json!({"runs": 0, "reopens": 0, "limit": 1, "met": null,
                               "stopped_by": null})
        );
        assert!(ends[2].get("done_check").is_none(), "{}", ends[2]);
    }

    /// One `done_check` record per run, parented on the task, with its fields in the documented
    /// order; a reopening run feeds the session-status count and a non-reopening one does not.
    #[tokio::test]
    async fn done_check_record_carries_its_fields_and_feeds_session_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let status = std::sync::Arc::new(crate::session_status::SessionStatus::new(
            std::time::Instant::now(),
            false,
            None,
            None,
            0,
            None,
            Some(3),
        ));
        w.set_session_status(std::sync::Arc::clone(&status));
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        let mut run = DoneCheckRun {
            task_id: "tsk_1",
            number: 1,
            command: "bash -c 'exit 1'",
            outcome: "failed",
            exit_code: Some(1),
            duration_ms: 12,
            stdout_bytes: 5,
            stderr_bytes: 0,
            truncated: false,
            resource_limit: None,
            reopened: true,
            reopens_left: 2,
            turns_remaining: Some(4),
        };
        w.write_done_check(&run, 1).await.unwrap();
        run.number = 2;
        run.outcome = "timed_out";
        run.exit_code = None;
        run.reopened = false;
        run.turns_remaining = None;
        w.write_done_check(&run, 1).await.unwrap();
        w.flush().await.unwrap();

        let text = std::fs::read_to_string(dir.path().join("trace.jsonl")).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains("\"done_check\""))
            .unwrap();
        let order: Vec<usize> = [
            "event_type",
            "event_id",
            "parent_id",
            "session_id",
            "timestamp",
            "task_id",
            "number",
            "command",
            "outcome",
            "exit_code",
            "duration_ms",
            "stdout_bytes",
            "stderr_bytes",
            "truncated",
            "resource_limit",
            "reopened",
            "reopens_left",
            "turns_remaining",
        ]
        .iter()
        .map(|key| line.find(&format!("\"{key}\":")).unwrap())
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{line}");

        let events = read_events(dir.path());
        let start = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        let records: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "done_check")
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["parent_id"], start["event_id"]);
        assert_eq!(records[0]["command"], "bash -c 'exit 1'");
        assert_eq!(records[0]["exit_code"], 1);
        assert_eq!(records[0]["reopened"], true);
        assert_eq!(records[0]["turns_remaining"], 4);
        assert_eq!(records[1]["outcome"], "timed_out");
        assert_eq!(records[1]["exit_code"], serde_json::Value::Null);
        assert_eq!(records[1]["resource_limit"], serde_json::Value::Null);
        assert_eq!(records[1]["turns_remaining"], serde_json::Value::Null);
        let spend = crate::session_status::SpendReading {
            used: 0,
            ceiling: None,
            machine: None,
        };
        assert_eq!(
            serde_json::to_value(status.snapshot(spend)).unwrap()["done_check"],
            serde_json::json!({"used": 1, "left": 2})
        );
    }

    /// `task_end` names the background commands still running, and has no such field when there
    /// were none.
    #[tokio::test]
    async fn task_end_names_the_commands_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for (task_id, running) in [
            ("tsk_1", vec!["wrk_0a".to_string(), "wrk_0b".to_string()]),
            ("tsk_2", Vec::new()),
        ] {
            w.write_task_start(task_id, "ctx_1", "a2a", event_provenance(), 3, &[], &[])
                .await
                .unwrap();
            w.write_task_end(task_id, "ok", 0, running, None, None)
                .await
                .unwrap();
        }
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let ends: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "task_end")
            .collect();
        assert_eq!(
            ends[0]["running_commands"],
            serde_json::json!(["wrk_0a", "wrk_0b"])
        );
        assert!(ends[1].get("running_commands").is_none(), "{}", ends[1]);
    }

    /// The two writers of `shell_completed` build one record: a completion a `background-command`
    /// call delivered carries no `completion_task_id`, one queued as a task names it, and both
    /// carry when the command exited and which way it reached the agent.
    #[tokio::test]
    async fn shell_completed_names_its_delivery_and_exit_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        let appender = ResourceTraceAppender::open(
            &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
            "test-session-id".to_string(),
            w.session_event_id().to_string(),
        )
        .await
        .unwrap();
        let completion = crate::detached::DetachedCompletion {
            work_id: "wrk_0a".to_string(),
            binary: "/usr/bin/bash".to_string(),
            command: "sleep 3".to_string(),
            exit_code: 0,
            duration_ms: 3_000,
            exited_at_ms: 1_750,
            output_path: "/records/logs/wrk_0a.log".to_string(),
            output_bytes: 10,
            resource_limit: None,
            output_holders: crate::shell::OutputHolders::Closed,
            context_id: "ctx_1".to_string(),
            provenance: crate::origin::TaskProvenance::derive(
                crate::origin::TaskOrigin::Completion,
                None,
            ),
            error: None,
        };
        w.write_session_start(None, None, Vec::new()).await.unwrap();
        w.flush().await.unwrap();
        appender.write_shell_completed(&completion).await;
        w.write_shell_completed(&completion, "tsk_2").await.unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let completed: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "shell_completed")
            .collect();
        assert_eq!(completed.len(), 2);
        assert_eq!(completed[0]["delivered_via"], "background_command");
        assert!(
            completed[0].get("completion_task_id").is_none(),
            "{}",
            completed[0]
        );
        assert_eq!(completed[1]["delivered_via"], "task");
        assert_eq!(completed[1]["completion_task_id"], "tsk_2");
        for record in &completed {
            assert_eq!(record["exited_at_ms"], 1_750);
            assert_eq!(record["work_id"], "wrk_0a");
            assert_eq!(record["status"], "ok");
            assert_eq!(record["duration_ms"], 3_000);
            assert_eq!(record["session_id"], "test-session-id");
            assert_eq!(record["parent_id"].as_str(), Some(w.session_event_id()));
        }
    }

    #[tokio::test]
    async fn task_reopened_names_hook_reason_and_ordinal() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_task_reopened(
            "tsk_1",
            "gatekeeper",
            "tests still fail",
            1,
            REOPEN_CONTEXT_CONTINUED,
            Some(7),
        )
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
        assert_eq!(re["attempt_context"], "continued");
        assert_eq!(re["turns_remaining"], 7);

        w.write_task_reopened(
            "tsk_1",
            "gatekeeper",
            "again",
            2,
            REOPEN_CONTEXT_CONTINUED,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();
        let uncapped = read_events(dir.path())
            .into_iter()
            .filter(|e| e["event_type"] == "task_reopened")
            .nth(1)
            .unwrap();
        assert!(uncapped
            .get("turns_remaining")
            .is_some_and(serde_json::Value::is_null));
        assert!(re["timestamp"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn task_continued_names_the_round_under_the_task_node() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_task_continued(
            "tsk_1",
            1,
            &["mcl_b".to_string(), "mcl_a".to_string()],
            &["dlg_1".to_string()],
            1234,
            Some(8),
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let start = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        let continued = events
            .iter()
            .find(|e| e["event_type"] == "task_continued")
            .unwrap();
        assert_eq!(continued["parent_id"], start["event_id"]);
        assert_eq!(continued["task_id"], "tsk_1");
        assert_eq!(continued["continuation_number"], 1);
        assert_eq!(
            continued["member_calls"],
            serde_json::json!(["mcl_b", "mcl_a"])
        );
        assert_eq!(continued["delegations"], serde_json::json!(["dlg_1"]));
        assert_eq!(continued["waited_ms"], 1234);
        assert_eq!(continued["turns_remaining"], 8);
    }

    /// `task_turns()` reports the cumulative per-task turn count the reopen loop reads to
    /// compute the remaining `max_turns` budget: it advances with each `write_inference`
    /// and resets on `write_task_start`.
    #[tokio::test]
    async fn task_turns_accessor_tracks_and_resets() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_inference(
            1,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(w.task_turns(), 2);
        // A new task resets the per-task counter.
        w.write_task_start("tsk_2", "ctx_2", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
    }

    /// `task_agent_turns()` counts the agent loop's own `inference` lines and not a hook's
    /// `run-inference` record, which `task_turns()` does count; both task boundaries reset it.
    #[tokio::test]
    async fn task_agent_turns_counts_agent_loop_inference_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        assert_eq!(w.task_agent_turns(), 0);
        let hook = InferenceOrigin {
            source: "hook:x".into(),
            model: "m".into(),
        };
        for (turn, origin) in [(0, None), (0, Some(&hook)), (1, None)] {
            w.write_inference(
                turn,
                Some(10),
                Some(5),
                "end_turn".to_string(),
                Some("end_turn"),
                None,
                origin,
                None,
                Vec::new(),
                None,
                None,
            )
            .await
            .unwrap();
        }
        assert_eq!(w.task_agent_turns(), 2);
        assert_eq!(w.task_turns(), 3);
        assert_eq!(w.task_hook_inferences(), 1);
        w.write_task_end("tsk_1", "completed", 0, Vec::new(), None, None)
            .await
            .unwrap();
        assert_eq!(
            w.task_agent_turns(),
            0,
            "a run outside a task frame starts at 0"
        );
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_task_start("tsk_2", "ctx_2", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        assert_eq!(w.task_agent_turns(), 0);
    }

    // ── Event identity and parenting ──────────────────────────────────────────

    /// The full tree one task produces, walked the way a reader walks it: every line is
    /// identified, every non-null `parent_id` names a line already written, and the shape is
    /// session → task → turn → {tool_call, shell}.
    #[tokio::test]
    async fn every_event_carries_a_unique_id_and_parents_to_an_earlier_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, vec!["bash".to_string()])
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
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
        w.write_artifact_pulled(&pulled(
            runtime_summary("pulled-style", ArtifactRuntime::Skill),
            "global store",
            None,
        ))
        .await
        .unwrap();
        w.write_task_end("tsk_1", "ok", 0, Vec::new(), None, None)
            .await
            .unwrap();
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
        assert_eq!(by_type("artifact_pulled")["parent_id"], session["event_id"]);
    }

    fn runtime_summary(name: &str, runtime: ArtifactRuntime) -> InstalledArtifactSummary {
        InstalledArtifactSummary {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            runtime,
            implementation: None,
            origin: LockOrigin::Runtime {
                session: "ses_puller".to_string(),
            },
        }
    }

    fn operator_summary(name: &str, runtime: ArtifactRuntime) -> InstalledArtifactSummary {
        InstalledArtifactSummary {
            origin: LockOrigin::Operator,
            ..runtime_summary(name, runtime)
        }
    }

    /// A pull of `summary` from `fetched_from`, with `owner_id` when the place records one.
    fn pulled(
        summary: InstalledArtifactSummary,
        fetched_from: &str,
        owner_id: Option<u64>,
    ) -> crate::runtime::PulledArtifact {
        crate::runtime::PulledArtifact {
            summary,
            fetched_from: fetched_from.to_string(),
            owner_id,
            notes: Vec::new(),
        }
    }

    /// `instruction_files` is always written: `null` without the key, the records in manifest
    /// order with it, and `[]` for an empty list.
    #[tokio::test]
    async fn session_start_instruction_files_is_null_without_the_key() {
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("AGENTS.md"), "x").unwrap();
        let read = crate::instruction_files::InstructionFiles::read(
            work.path(),
            &["AGENTS.md".to_string(), "CLAUDE.md".to_string()],
        );
        let empty = crate::instruction_files::InstructionFiles::read(work.path(), &[]);
        for (files, expected) in [
            (None, serde_json::Value::Null),
            (Some(&empty), serde_json::json!([])),
            (
                Some(&read),
                serde_json::json!([
                    {
                        "path": "AGENTS.md",
                        "status": "read",
                        "resolved": "AGENTS.md",
                        "bytes": 1,
                        "included_bytes": 1,
                        "truncated": false,
                        "sha256": murmur_artifact::sha256_hex(b"x"),
                    },
                    {"path": "CLAUDE.md", "status": "missing"},
                ]),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            w.set_instruction_files(files);
            w.write_session_start(Some(10), None, Vec::new())
                .await
                .unwrap();
            w.flush().await.unwrap();
            let events = read_events(dir.path());
            assert!(events[0].get("instruction_files").is_some());
            assert_eq!(events[0]["instruction_files"], expected);
        }
    }

    fn harness_start_with_prompt(prompt: &str) -> HarnessStart {
        HarnessStart {
            driver: "local/fixture-process-driver".to_string(),
            driver_version: "0.1.0".to_string(),
            harness: "fixture".to_string(),
            binary: "/bin/true".to_string(),
            binary_source: "describe".to_string(),
            harness_version: None,
            version_tested: false,
            harness_session_id: "h".to_string(),
            session_mode: "new".to_string(),
            args_count: 0,
            env_names: Vec::new(),
            account_env_unset: Vec::new(),
            files: Vec::new(),
            bridge_tools: Vec::new(),
            stdin_bytes: 0,
            keep_stdin_open: false,
            system_prompt_sha256: murmur_artifact::sha256_hex(prompt.as_bytes()),
            system_prompt: prompt.to_string(),
        }
    }

    /// `harness_start` names the prompt handed to the driver by its hash, never carries the text,
    /// and under content capture stores the text as that blob.
    #[tokio::test]
    async fn harness_start_records_the_system_prompt_hash_and_blob() {
        let prompt = "facts\n\nMANIFEST\n\n[Instruction file: AGENTS.md]\nx";
        let sha = murmur_artifact::sha256_hex(prompt.as_bytes());
        for capture in [TraceCapture::Meta, TraceCapture::Content] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer_with_opts(dir.path(), capture).await;
            w.write_harness_start(harness_start_with_prompt(prompt))
                .await
                .unwrap();
            w.flush().await.unwrap();
            let event = &read_events(dir.path())[0];
            assert_eq!(event["event_type"], "harness_start");
            assert_eq!(event["system_prompt_sha256"], sha.as_str());
            assert!(event.get("system_prompt").is_none(), "{event}");
            let blob = dir.path().join(BLOB_DIR_NAME).join(&sha);
            if capture == TraceCapture::Content {
                assert_eq!(std::fs::read(&blob).unwrap(), prompt.as_bytes());
            } else {
                assert!(!blob.exists());
            }
        }
    }

    /// `runtime_artifacts` is always written: `[]` for a session whose every artifact the operator
    /// pinned, including one that never called the setter.
    #[tokio::test]
    async fn runtime_origin_session_start_lists_nothing_for_an_operator_session() {
        for set in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut w = make_writer(dir.path()).await;
            if set {
                w.set_runtime_artifacts(&[
                    operator_summary("house-style", ArtifactRuntime::Skill),
                    operator_summary("fetcher", ArtifactRuntime::Tool),
                ]);
            }
            w.write_session_start(Some(10), None, Vec::new())
                .await
                .unwrap();
            w.flush().await.unwrap();
            let events = read_events(dir.path());
            assert_eq!(events[0]["runtime_artifacts"], serde_json::json!([]));
        }
    }

    /// Only runtime-origin entries are listed, in the order staging recorded them, each naming
    /// its puller.
    #[tokio::test]
    async fn runtime_origin_session_start_lists_runtime_artifacts_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.set_runtime_artifacts(&[
            runtime_summary("zeta-tool", ArtifactRuntime::Tool),
            operator_summary("house-style", ArtifactRuntime::Skill),
            InstalledArtifactSummary {
                origin: LockOrigin::Runtime {
                    session: "ses_other".to_string(),
                },
                ..runtime_summary("alpha-style", ArtifactRuntime::Skill)
            },
        ]);
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(
            events[0]["runtime_artifacts"],
            serde_json::json!([
                {"name": "zeta-tool", "version": "1.0.0", "origin": "runtime",
                 "session": "ses_puller", "trust": "untrusted"},
                {"name": "alpha-style", "version": "1.0.0", "origin": "runtime",
                 "session": "ses_other", "trust": "untrusted"},
            ])
        );
    }

    #[tokio::test]
    async fn runtime_origin_skill_call_carries_origin_and_trust() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        for origin in [
            LockOrigin::Operator,
            LockOrigin::Runtime {
                session: "ses_puller".to_string(),
            },
        ] {
            w.write_skill_call(0, "a-skill".to_string(), &origin, 1, 1, "ok".to_string())
                .await
                .unwrap();
        }
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events[0]["origin"], "operator");
        assert_eq!(events[0]["trust"], "trusted");
        assert_eq!(events[1]["origin"], "runtime");
        assert_eq!(events[1]["trust"], "untrusted");
    }

    /// Every key is written, `session` included: `null` when the lock entry is the operator's.
    /// `owner_id` is written only when the place the payload came from records one.
    #[tokio::test]
    async fn runtime_origin_artifact_pulled_writes_every_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_artifact_pulled(&pulled(
            runtime_summary("pulled-style", ArtifactRuntime::Skill),
            "github:ana/artifacts",
            Some(4242),
        ))
        .await
        .unwrap();
        w.write_artifact_pulled(&pulled(
            operator_summary("fetcher", ArtifactRuntime::Tool),
            "project store",
            None,
        ))
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let runtime = &events[0];
        assert_eq!(runtime["event_type"], "artifact_pulled");
        assert_eq!(runtime["name"], "pulled-style");
        assert_eq!(runtime["version"], "1.0.0");
        assert_eq!(runtime["runtime"], "skill");
        assert_eq!(runtime["origin"], "runtime");
        assert_eq!(runtime["session"], "ses_puller");
        assert_eq!(runtime["trust"], "untrusted");
        assert_eq!(runtime["fetched_from"], "github:ana/artifacts");
        assert_eq!(runtime["owner_id"], 4242);
        assert!(
            runtime["parent_id"].is_null(),
            "no session_start was written"
        );

        let operator = &events[1];
        assert_eq!(operator["runtime"], "tool");
        assert_eq!(operator["origin"], "operator");
        assert!(operator.get("session").is_some_and(Value::is_null));
        assert_eq!(operator["trust"], "trusted");
        assert_eq!(operator["fetched_from"], "project store");
        assert!(operator.get("owner_id").is_none());
    }

    /// A hook's `run-inference` record is a product of the turn it ran inside, not a turn of
    /// its own: it parents to the agent loop's inference and never replaces it as the node the
    /// turn's other events hang off.
    #[tokio::test]
    async fn hook_origin_inference_hangs_off_the_turn_rather_than_becoming_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
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
            Some(1),
            Some(1),
            "end_turn".to_string(),
            // A hook's `run-inference` saw no provider stop reason, so it records none.
            None,
            None,
            Some(&origin),
            None,
            Vec::new(),
            None,
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

    /// A writer with no session frame behind it names no parent: naming one would dangle.
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
        w.write_session_start(Some(1), None, Vec::new())
            .await
            .unwrap();
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

    /// `task_rejected` carries every field and, with no task active, hangs off `session_start`.
    #[tokio::test]
    async fn task_rejected_names_the_task_source_cause_and_reason_under_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.write_task_start("tsk_ran", "ctx_ran", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_task_end("tsk_ran", "ok", 0, Vec::new(), None, None)
            .await
            .unwrap();
        w.write_task_rejected(
            "tsk_1",
            "ctx_1",
            "detached_shell",
            TASK_REJECTED_SESSION_ENDED,
            "task rejected: the session ended before this task started",
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let session = events
            .iter()
            .find(|e| e["event_type"] == "session_start")
            .unwrap();
        let rejected: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "task_rejected")
            .collect();
        assert_eq!(rejected.len(), 1);
        let rejected = rejected[0];
        assert_eq!(rejected["parent_id"], session["event_id"], "{rejected}");
        assert_eq!(rejected["session_id"], session["session_id"]);
        assert_eq!(rejected["task_id"], "tsk_1");
        assert_eq!(rejected["context_id"], "ctx_1");
        assert_eq!(rejected["source"], "detached_shell");
        assert_eq!(rejected["cause"], "session_ended");
        assert_eq!(
            rejected["reason"],
            "task rejected: the session ended before this task started"
        );
        assert!(rejected["event_id"].is_string());
        assert!(rejected["timestamp"].is_u64());
        assert_eq!(rejected.as_object().unwrap().len(), 10, "{rejected}");
    }

    /// `task_failed` hangs off the task it failed, and a reason past the cap is cut on a character
    /// boundary. Both optional fields are absent, not null, when there is nothing to say.
    #[tokio::test]
    async fn task_failed_names_the_task_turn_cause_and_capped_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        assert_eq!(w.last_task_failure(), None);
        w.write_task_failed(None, TASK_FAILED_RUNTIME_ERROR, "before any task")
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        let long = "é".repeat(MAX_TASK_FAILURE_REASON_BYTES);
        w.write_task_failed(Some(2), TASK_FAILED_DRIVER_ERROR, &long)
            .await
            .unwrap();
        w.write_task_end("tsk_1", "failed", 0, Vec::new(), None, None)
            .await
            .unwrap();
        w.flush().await.unwrap();

        assert_eq!(w.task_failures_written(), 2);
        let events = read_events(dir.path());
        let failed: Vec<_> = events
            .iter()
            .filter(|e| e["event_type"] == "task_failed")
            .collect();
        assert_eq!(failed.len(), 2);

        assert!(failed[0].get("task_id").is_none(), "{}", failed[0]);
        assert!(failed[0].get("turn").is_none(), "{}", failed[0]);
        assert_eq!(failed[0]["cause"], "runtime_error");
        assert_eq!(failed[0]["reason"], "before any task");

        let start = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        assert_eq!(failed[1]["task_id"], "tsk_1");
        assert_eq!(failed[1]["turn"], 2);
        assert_eq!(failed[1]["cause"], "driver_error");
        assert_eq!(failed[1]["parent_id"], start["event_id"]);
        assert_eq!(failed[1]["session_id"], start["session_id"]);
        assert!(failed[1]["event_id"].is_string());
        assert!(failed[1]["timestamp"].is_u64());
        let reason = failed[1]["reason"].as_str().unwrap();
        assert!(reason.len() <= MAX_TASK_FAILURE_REASON_BYTES);
        assert!(reason.len() > MAX_TASK_FAILURE_REASON_BYTES - 2);
        assert_eq!(w.last_task_failure(), Some(reason));
    }

    /// Turn-level events carry the enclosing task's id, and `null` once the task has ended —
    /// `task_id` is a record of scope, not a value the writer holds on to.
    #[tokio::test]
    async fn turn_level_events_carry_the_active_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_skill_call(
            0,
            "house-style".to_string(),
            &LockOrigin::Operator,
            12,
            1,
            "ok".to_string(),
        )
        .await
        .unwrap();
        w.write_compaction(0, 100, OccupancyBasis::Estimate, 40)
            .await
            .unwrap();
        w.write_task_end("tsk_1", "ok", 0, Vec::new(), None, None)
            .await
            .unwrap();
        w.write_inference(
            1,
            Some(1),
            Some(1),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            Vec::new(),
            None,
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

    /// `tool_input_refused` carries the tool, its call id (`null` when absent or empty), the
    /// missing names and the reason, and is written under every capture mode alike.
    #[tokio::test]
    async fn tool_input_refused_records_its_fields_in_every_capture_mode() {
        for capture in [
            TraceCapture::None,
            TraceCapture::Meta,
            TraceCapture::Content,
        ] {
            for (given, expected) in [
                (Some("toolu_1"), Some("toolu_1")),
                (Some(""), None),
                (None, None),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let mut w = make_writer_with_opts(dir.path(), capture).await;
                w.write_tool_input_refused(
                    2,
                    "murmur-tool-editor",
                    given,
                    &["operation".to_string(), "dest_path".to_string()],
                    "murmur-tool-editor: missing required fields",
                )
                .await
                .unwrap();
                w.flush().await.unwrap();
                let e = &read_events(dir.path())[0];
                assert_eq!(e["event_type"], "tool_input_refused", "{capture:?}");
                assert!(e["event_id"].is_string());
                assert!(e["session_id"].is_string());
                assert!(e["timestamp"].is_u64());
                assert_eq!(e["turn"], 2);
                assert_eq!(e["tool_name"], "murmur-tool-editor");
                match expected {
                    Some(id) => assert_eq!(e["tool_call_id"], id, "given={given:?}"),
                    None => assert!(e["tool_call_id"].is_null(), "given={given:?}"),
                }
                assert_eq!(e["missing"], serde_json::json!(["operation", "dest_path"]));
                assert_eq!(e["reason"], "murmur-tool-editor: missing required fields");
            }
        }
    }

    /// `tools_refreshed` carries the turn, the task, the trigger and three sorted name lists,
    /// hangs off the task node rather than the previous turn, and `session_start` records the
    /// declared trigger — `null` under `transport: process`.
    #[tokio::test]
    async fn tools_refreshed_event_records_trigger_names_and_task_parent() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.set_tool_refresh(Some("immediate"));
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_tools_refreshed(
            1,
            "immediate",
            &["aaa-late-skill".to_string()],
            &[],
            &[
                "aaa-late-skill".to_string(),
                "zzz-existing-tool".to_string(),
            ],
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        assert_eq!(events[0]["tool_refresh"], "immediate");
        let task = events
            .iter()
            .find(|e| e["event_type"] == "task_start")
            .unwrap();
        let refreshed: Vec<&Value> = events
            .iter()
            .filter(|e| e["event_type"] == "tools_refreshed")
            .collect();
        assert_eq!(refreshed.len(), 1);
        let r = refreshed[0];
        assert!(r["event_id"].as_str().unwrap().starts_with("evt_"));
        assert_eq!(r["parent_id"], task["event_id"]);
        assert_eq!(r["session_id"], "test-session-id");
        assert!(r["timestamp"].as_u64().is_some());
        assert_eq!(r["turn"], 1);
        assert_eq!(r["task_id"], "tsk_1");
        assert_eq!(r["trigger"], "immediate");
        assert_eq!(r["added"], serde_json::json!(["aaa-late-skill"]));
        assert_eq!(r["removed"], serde_json::json!([]));
        assert_eq!(
            r["tools"],
            serde_json::json!(["aaa-late-skill", "zzz-existing-tool"])
        );

        let process_dir = tempfile::tempdir().unwrap();
        let mut process = make_writer(process_dir.path()).await;
        process.set_tool_refresh(None);
        process
            .write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        process.flush().await.unwrap();
        let start = &read_events(process_dir.path())[0];
        assert!(start.as_object().unwrap().contains_key("tool_refresh"));
        assert!(start["tool_refresh"].is_null());
    }

    /// A declined compaction is a full record of the decline, not a bare marker: it names the
    /// turn, the task, the occupancy the session went on running over, and the reason.
    #[tokio::test]
    async fn compaction_declined_records_turn_task_tokens_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_session_start(Some(10), None, Vec::new())
            .await
            .unwrap();
        w.write_task_start("tsk_1", "ctx_1", "a2a", event_provenance(), 3, &[], &[])
            .await
            .unwrap();
        w.write_inference(
            2,
            Some(10),
            Some(5),
            "tool_call".to_string(),
            Some("tool_call"),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.write_compaction_declined(
            2,
            4321,
            OccupancyBasis::Actual,
            COMPACTION_DECLINED_UNRESOLVED_TOOL_CALL,
        )
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
        assert_eq!(d["tokens_source"], "actual");
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
                Some(1),
                Some(1),
                "end_turn".to_string(),
                Some("end_turn"),
                None,
                None,
                None,
                Vec::new(),
                None,
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
            Some(10),
            Some(5),
            "end_turn".to_string(),
            Some("end_turn"),
            None,
            None,
            None,
            vec!["msg_0000000000000000000000000000000a".to_string()],
            Some(&wire),
            None,
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

    /// A record written for a completion the runtime parsed no driver response for names no stop
    /// reason: the key is absent, never an empty string. Absence means "no driver response was
    /// parsed here", so a reader can tell it apart from a driver that reported nothing — which is
    /// recorded as `""`.
    #[tokio::test]
    async fn stop_reason_is_absent_when_no_driver_response_was_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_inference(
            0,
            Some(10),
            Some(5),
            "end_turn".to_string(),
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let event = read_events(dir.path()).remove(0);
        assert!(
            event.get("stop_reason").is_none(),
            "stop_reason must be absent, got {event}"
        );
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
            Some(1),
            Some(1),
            "end_turn".to_string(),
            // A hook's `run-inference` saw no provider stop reason, so it records none.
            None,
            None,
            Some(&origin),
            None,
            Vec::new(),
            None,
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

    /// A delegation record a plan's `capsule` step wrote and one `delegate-task` wrote are the
    /// same record.
    ///
    /// Identical key sets and identical values on everything but the two fields that are
    /// per-line by construction — the event id and the clock. That is what makes a trace
    /// unable to say which surface launched a child, and it holds because both appenders
    /// serialize the same struct rather than because this test watches them.
    #[tokio::test]
    async fn a_plan_written_delegation_record_matches_a_delegate_task_one() {
        let plan_dir = tempfile::tempdir().unwrap();
        let tool_dir = tempfile::tempdir().unwrap();
        let session = "ses_0000000000004000800000000000plan".to_string();
        let node = "evt_0000000000004000800000000000node".to_string();

        let plan = PlanTraceAppender::open(
            &crate::workdir_fs::WorkdirRoot::open(plan_dir.path()).unwrap(),
            session.clone(),
            node.clone(),
        )
        .unwrap();
        let tool = ResourceTraceAppender::open(
            &crate::workdir_fs::WorkdirRoot::open(tool_dir.path()).unwrap(),
            session.clone(),
            node.clone(),
        )
        .await
        .unwrap();

        let snapshot = crate::workdir_snapshot::WorkdirSnapshot {
            source: "/home/you/project".to_string(),
            files: 2,
            dirs: 0,
            symlinks: 0,
            repointed_symlinks: 0,
            bytes: 12,
            skipped_count: 0,
            skipped: Vec::new(),
        };
        plan.write_delegation_start(
            "dlg_abc123",
            "worker",
            "0.1.0",
            "ses_child",
            ".murmur/c",
            "/r/ses_child",
            Some(&snapshot),
        );
        tool.write_delegation_start(
            "dlg_abc123",
            "worker",
            "0.1.0",
            "ses_child",
            ".murmur/c",
            "/r/ses_child",
            Some(&snapshot),
        )
        .await;
        plan.write_delegation(
            "worker",
            "0.1.0",
            Some("dlg_abc123".to_string()),
            Some("ses_child".to_string()),
            42,
            "completed",
            None,
        );
        tool.write_delegation(
            "worker",
            "0.1.0",
            Some("dlg_abc123".to_string()),
            Some("ses_child".to_string()),
            42,
            "completed",
            None,
        )
        .await;

        let read = |dir: &Path| -> Vec<serde_json::Map<String, Value>> {
            std::fs::read_to_string(dir.join("trace.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Map<_, _>>(line).unwrap())
                .collect()
        };
        let from_plan = read(plan_dir.path());
        let from_tool = read(tool_dir.path());
        assert_eq!(from_plan.len(), 2);
        assert_eq!(from_tool.len(), 2);
        assert_eq!(
            from_plan[0]["workdir_snapshot"],
            serde_json::to_value(&snapshot).unwrap()
        );

        for (planned, called, event_type) in [
            (&from_plan[0], &from_tool[0], "delegation_start"),
            (&from_plan[1], &from_tool[1], "delegation"),
        ] {
            assert_eq!(planned["event_type"], event_type);
            assert_eq!(called["event_type"], event_type);
            assert_eq!(
                planned.keys().collect::<Vec<_>>(),
                called.keys().collect::<Vec<_>>(),
                "{planned:?} vs {called:?}"
            );
            assert_eq!(planned["parent_id"], node.as_str());
            assert_eq!(called["parent_id"], node.as_str());
            for (key, value) in planned {
                if key == "event_id" || key == "timestamp" {
                    continue;
                }
                assert_eq!(value, &called[key], "{key}: {planned:?} vs {called:?}");
            }
            for key in ["plan_id", "step_id", "surface", "kind"] {
                assert!(!planned.contains_key(key), "{key} is on {planned:?}");
                assert!(!called.contains_key(key), "{key} is on {called:?}");
            }
        }
    }

    /// `spawner_ended` carries the lineage `session_start` does, on the same terms: both keys
    /// present for a delegated child, both absent — not null — for a session launched with a
    /// lifeline but no spawner handle.
    #[tokio::test]
    async fn spawner_ended_names_the_lineage_or_omits_it() {
        let dir = tempfile::tempdir().unwrap();
        let session = "ses_0000000000004000800000000child".to_string();
        let node = "evt_0000000000004000800000000000node".to_string();
        let appender = ResourceTraceAppender::open(
            &crate::workdir_fs::WorkdirRoot::open(dir.path()).unwrap(),
            session.clone(),
            node.clone(),
        )
        .await
        .unwrap();

        appender
            .write_spawner_ended(Some("ses_parent"), Some("dlg_abc123"))
            .await;
        appender.write_spawner_ended(None, None).await;

        let lines: Vec<serde_json::Map<String, Value>> =
            std::fs::read_to_string(dir.path().join("trace.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(lines.len(), 2);
        let named = &lines[0];
        assert_eq!(named["event_type"], "spawner_ended");
        assert_eq!(named["parent_id"], node.as_str());
        assert_eq!(named["session_id"], session.as_str());
        assert_eq!(named["spawned_by"], "ses_parent");
        assert_eq!(named["delegation_id"], "dlg_abc123");
        assert_eq!(
            named.keys().collect::<Vec<_>>(),
            [
                "delegation_id",
                "event_id",
                "event_type",
                "parent_id",
                "session_id",
                "spawned_by",
                "timestamp"
            ]
        );
        let bare = &lines[1];
        assert_eq!(bare["event_type"], "spawner_ended");
        assert!(!bare.contains_key("spawned_by"), "{bare:?}");
        assert!(!bare.contains_key("delegation_id"), "{bare:?}");
    }
}
