use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::Subcommand;
use serde::Deserialize;

use crate::error::{CliError, E_IO_001, E_IO_003};

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

#[derive(Debug, Deserialize)]
struct SessionStartEvent {
    session_id: String,
    capsule_name: String,
    capsule_version: String,
    model: String,
    max_turns: u32,
}

#[derive(Debug, Deserialize)]
struct InferenceEvent {
    turn: u32,
    decision: String,
    #[serde(default)]
    tool_name: Option<String>,
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

#[derive(Debug, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
enum TraceEvent {
    SessionStart(SessionStartEvent),
    Inference(InferenceEvent),
    ToolCall(ToolCallEvent),
    SkillCall(SkillCallEvent),
    Shell(ShellEvent),
    Compaction(CompactionEvent),
    CompactionDeclined(CompactionDeclinedEvent),
    SessionEnd(SessionEndEvent),
    TaskStart(TaskStartEvent),
    TaskEnd(TaskEndEvent),
    TaskReopened(TaskReopenedEvent),
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
    inference_turns: Vec<(u32, String)>,
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
}

/// One `task_reopened` trace record, surfaced in `mur trace show`.
struct ReopenRecord {
    reopen_number: u32,
    hook_name: String,
    reason: String,
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

fn parse_trace_file(path: &Path) -> Result<Vec<TraceEvent>, CliError> {
    let content = fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CliError::new(
            E_IO_001,
            format!("trace file not found: {}", path.display()),
        ),
        _ => CliError::new(E_IO_003, format!("failed to read {}: {e}", path.display())),
    })?;

    let mut events = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<TraceEvent>(line) {
            Ok(ev) => events.push(ev),
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
    let mut inference_turns: Vec<(u32, String)> = Vec::new();
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

    for event in events {
        match event {
            TraceEvent::SessionStart(e) => ss = Some(e),
            TraceEvent::Inference(e) => {
                inference_turns.push((e.turn, e.decision));
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
            inference_turns,
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

fn ses_entries(workdir: &Path) -> Result<Vec<String>, CliError> {
    if !workdir.exists() || !workdir.is_dir() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry_res in fs::read_dir(workdir).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to read {}: {e}", workdir.display()),
        )
    })? {
        let entry = entry_res.map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to read entry in {}: {e}", workdir.display()),
            )
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ses_") && entry.path().is_dir() {
            entries.push(name);
        }
    }
    Ok(entries)
}

pub(crate) fn resolve_session(
    session: Option<String>,
    workdir: &Path,
) -> Result<PathBuf, CliError> {
    match session {
        None => {
            let mut entries = ses_entries(workdir)?;
            if entries.is_empty() {
                return Err(CliError::new(
                    E_TRC_002,
                    format!("no sessions found in workdir at {}", workdir.display()),
                ));
            }
            entries.sort();
            let latest = entries.into_iter().last().unwrap();
            Ok(workdir.join(latest).join("trace.jsonl"))
        }
        Some(s) => {
            // Backward compatibility: treat as a literal path if it looks like one.
            if s.contains('/') || s.ends_with(".jsonl") {
                return Ok(PathBuf::from(&s));
            }
            // Full session ID: "ses_" prefix + 32-char hex = 36 chars total.
            if s.starts_with("ses_") && s.len() == 36 {
                let path = workdir.join(&s).join("trace.jsonl");
                if !path.exists() {
                    return Err(CliError::new(
                        E_TRC_002,
                        format!("session {} not found in {}", s, workdir.display()),
                    ));
                }
                return Ok(path);
            }
            // Suffix matching (case-insensitive).
            let suffix_lower = s.to_lowercase();
            let entries = ses_entries(workdir)?;
            let mut matches: Vec<String> = entries
                .into_iter()
                .filter(|e| e.to_lowercase().ends_with(&suffix_lower))
                .collect();
            match matches.len() {
                0 => Err(CliError::new(
                    E_TRC_002,
                    format!(
                        "no session found matching suffix '{}' in {}",
                        s,
                        workdir.display()
                    ),
                )),
                1 => Ok(workdir.join(&matches[0]).join("trace.jsonl")),
                n => {
                    matches.sort();
                    Err(CliError::new(
                        E_TRC_002,
                        format!(
                            "ambiguous: '{}' matches {} sessions — provide more characters\n{}",
                            s,
                            n,
                            matches
                                .iter()
                                .map(|m| format!("  {m}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ),
                    ))
                }
            }
        }
    }
}

fn resolve_diff_arg(arg: &str, workdir: &Path, label: &str) -> Result<PathBuf, CliError> {
    if let Some(n_str) = arg.strip_prefix('@') {
        let n: usize = n_str.parse().map_err(|_| {
            CliError::new(
                E_TRC_002,
                format!("{label}: invalid ordinal '{arg}' — expected @1, @2, ..."),
            )
        })?;
        if n == 0 {
            return Err(CliError::new(
                E_TRC_002,
                format!("{label}: ordinal must be @1 or higher"),
            ));
        }
        let mut entries = ses_entries(workdir)?;
        if entries.is_empty() {
            return Err(CliError::new(
                E_TRC_002,
                format!("no sessions found in workdir at {}", workdir.display()),
            ));
        }
        entries.sort();
        entries.reverse(); // descending: most recent first
        if n > entries.len() {
            return Err(CliError::new(
                E_TRC_002,
                format!(
                    "{label}: @{n} is out of range — workdir has {} session{}",
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        return Ok(workdir.join(&entries[n - 1]).join("trace.jsonl"));
    }
    resolve_session(Some(arg.to_string()), workdir)
        .map_err(|e| CliError::new(e.code, format!("{label}: {}", e.message)))
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

fn session_id_timestamp_ms(ses_name: &str) -> Option<u64> {
    let hex = ses_name.strip_prefix("ses_")?;
    if hex.len() < 12 {
        return None;
    }
    u64::from_str_radix(&hex[..12], 16).ok()
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
    println!("capsule:    {} v{}", m.capsule_name, m.capsule_version);
    println!("model:      {}", m.model);
    println!("status:     {}", m.exit_status);
    println!("duration:   {}", fmt_dur(m.duration_ms));
    println!();

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
    println!();

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
            .inference_turns
            .iter()
            .map(|(t, d)| (*t, d.as_str()))
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
}

pub(crate) fn run_trace_show(
    session: Option<String>,
    workdir_arg: Option<PathBuf>,
) -> Result<(), CliError> {
    let workdir = workdir_arg.unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("workdir")
    });
    let path = resolve_session(session, &workdir)?;
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
    let events = parse_trace_file(&path)?;
    if events.is_empty() {
        return Err(CliError::new(
            E_TRC_001,
            format!(
                "{}: trace file is empty (incomplete or zero-event session)",
                path.display()
            ),
        ));
    }

    let mut session_id = String::new();
    let mut inferences: Vec<(u32, String, Option<String>)> = Vec::new();
    let mut tool_durations: HashMap<u32, u64> = HashMap::new();
    let mut tool_inputs: HashMap<u32, String> = HashMap::new();

    for event in &events {
        match event {
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
    Ok(())
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
                session_id_timestamp_ms(e)
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
}
