use std::{
    path::Path,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use murmur_artifact::ContainmentClass;
use serde::Serialize;
use serde_json::Value;
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
};

use crate::containment::ScopeReport;

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
    include_tool_output: bool,
    /// Where the effective system prompt came from: `"manifest"`, `"cli"` or `"none"`. Derived
    /// once in [`TraceWriter::open`] from the resolved prompt and the override flag, then repeated
    /// on every `session_start` the same way `model` is — the prompt cannot change between the
    /// tasks of one session.
    system_prompt_source: &'static str,
    /// SHA-256 (lowercase hex) of the resolved system prompt, or `None` when no prompt is in
    /// effect. Always written to `session_start` (as `null` when absent) so a trace records *that*
    /// a prompt was in effect and which one, even when the text itself is withheld.
    system_prompt_sha256: Option<String>,
    /// The resolved system prompt verbatim — populated only when the manifest opted in via
    /// `trace.include_tool_output`, on the same terms as tool output text. `None` otherwise, which
    /// omits the field from `session_start` entirely.
    system_prompt: Option<String>,
    session_start_time: Instant,
    session_started: bool,
    session_ended: bool,
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

// ── Event structs (Serialize → JSONL lines) ──────────────────────────────────

#[derive(Serialize)]
struct SessionStartEvent {
    event_type: &'static str,
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
    system_prompt_sha256: Option<String>,
    /// The resolved prompt verbatim. Written only when the manifest set
    /// `trace.include_tool_output: true`; omitted otherwise, since a system prompt is capsule
    /// content on the same footing as tool output and is not captured by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
}

#[derive(Serialize)]
struct InferenceEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    input_tokens: u64,
    output_tokens: u64,
    decision: String,
    tool_name: Option<String>,
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
}

#[derive(Serialize)]
struct ToolCallEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    tool_name: String,
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
    session_id: String,
    timestamp: u64,
    turn: u32,
    skill_name: String,
    output_bytes: u64,
    duration_ms: u64,
    status: String,
}

#[derive(Serialize)]
struct ShellEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
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

#[derive(Serialize)]
struct CompactionEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    turn: u32,
    tokens_before: u64,
    tokens_after: u64,
}

#[derive(Serialize)]
struct A2aTaskReceivedEvent {
    event_type: &'static str,
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
    session_id: String,
    timestamp: u64,
    peer_url: String,
    message_id: String,
    task_id: String,
    context_id: String,
    traceparent: Option<String>,
}

#[derive(Serialize)]
struct SessionEndEvent {
    event_type: &'static str,
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
    session_id: String,
    timestamp: u64,
    task_id: String,
    context_id: String,
    source: String,
    message_parts_bytes: u64,
}

#[derive(Serialize)]
struct TaskEndEvent {
    event_type: &'static str,
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

#[derive(Serialize)]
struct HookDispatchErrorEvent {
    event_type: &'static str,
    session_id: String,
    timestamp: u64,
    hook_name: String,
    event: String,
    arm: String,
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
        include_tool_output: bool,
        system_prompt: Option<String>,
        system_prompt_overridden: bool,
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
        Ok(Self {
            writer: BufWriter::new(file),
            session_id,
            capsule_name,
            capsule_version,
            model,
            capabilities,
            effective_grants,
            include_tool_output,
            system_prompt_source,
            system_prompt_sha256,
            system_prompt: include_tool_output.then_some(system_prompt).flatten(),
            session_start_time: Instant::now(),
            session_started: false,
            session_ended: false,
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

    pub(crate) async fn write_session_start(
        &mut self,
        max_turns: u32,
        tools_declared: Vec<String>,
    ) -> std::io::Result<()> {
        let event = SessionStartEvent {
            event_type: "session_start",
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
            system_prompt: self.system_prompt.clone(),
        };
        self.write_event(&event).await?;
        self.session_started = true;
        Ok(())
    }

    pub(crate) async fn write_inference(
        &mut self,
        turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        decision: String,
        tool_name: Option<String>,
        origin: Option<&InferenceOrigin>,
    ) -> std::io::Result<()> {
        let event = InferenceEvent {
            event_type: "inference",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            input_tokens,
            output_tokens,
            decision,
            tool_name,
            origin: origin.map(|o| o.source.clone()),
            model: origin.map(|o| o.model.clone()),
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn write_tool_call(
        &mut self,
        turn: u32,
        tool_name: String,
        input: Value,
        input_bytes: u64,
        output: &str,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
        state_effect: Option<String>,
        resource_id: Option<String>,
    ) -> std::io::Result<()> {
        let event = ToolCallEvent {
            event_type: "tool_call",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            tool_name,
            input,
            input_bytes,
            output: self.include_tool_output.then(|| output.to_string()),
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
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
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
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
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

    pub(crate) async fn write_a2a_task_received(
        &mut self,
        task_id: &str,
        context_id: &str,
        message_id: &str,
        traceparent_from_caller: Option<&str>,
    ) -> std::io::Result<()> {
        let event = A2aTaskReceivedEvent {
            event_type: "a2a_task_received",
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
    ) -> std::io::Result<()> {
        let event = A2aSendEvent {
            event_type: "a2a_send",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            peer_url: peer_url.to_string(),
            message_id: message_id.to_string(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            traceparent: traceparent.map(str::to_string),
        };
        self.write_event(&event).await
    }

    pub(crate) async fn write_task_start(
        &mut self,
        task_id: &str,
        context_id: &str,
        source: &str,
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

        let event = TaskStartEvent {
            event_type: "task_start",
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            task_id: task_id.to_string(),
            context_id: context_id.to_string(),
            source: source.to_string(),
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
            session_id: self.session_id.clone(),
            timestamp: timestamp_ms(),
            turn,
            tokens_before,
            tokens_after,
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

    async fn write_event(&mut self, event: &impl Serialize) -> std::io::Result<()> {
        let line = serde_json::to_string(event).map_err(std::io::Error::other)?;
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.write_all(b"\n").await
    }
}

fn timestamp_ms() -> u64 {
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
        types::CapabilityPolicy,
    };
    use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant};
    use serde_json::Value;

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
        )
    }

    async fn make_writer(dir: &std::path::Path) -> TraceWriter {
        make_writer_with_opts(dir, false).await
    }

    async fn make_writer_with_opts(
        dir: &std::path::Path,
        include_tool_output: bool,
    ) -> TraceWriter {
        make_writer_with_prompt(dir, include_tool_output, None, false).await
    }

    async fn make_writer_with_prompt(
        dir: &std::path::Path,
        include_tool_output: bool,
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
            include_tool_output,
            system_prompt.map(str::to_string),
            system_prompt_overridden,
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
            let mut w = make_writer_with_prompt(dir.path(), false, prompt, overridden).await;
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

    /// The prompt text itself rides on `trace.include_tool_output`, the same opt-in that governs
    /// tool output: withheld by default, verbatim when asked for. The source and hash are written
    /// either way, so a default trace still records that a prompt was in effect.
    #[tokio::test]
    async fn session_start_writes_verbatim_system_prompt_only_when_tool_output_is_included() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(dir.path(), false, Some("Be terse."), false).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        let e = &read_events(dir.path())[0];
        assert!(
            e.get("system_prompt").is_none(),
            "prompt text must be omitted without the opt-in, got {e}"
        );
        assert_eq!(
            e["system_prompt_sha256"],
            murmur_artifact::sha256_hex(b"Be terse.")
        );

        let opted_in = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(opted_in.path(), true, Some("Be terse."), false).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        assert_eq!(
            read_events(opted_in.path())[0]["system_prompt"],
            "Be terse."
        );
    }

    /// Opting in cannot conjure a prompt that was never in effect.
    #[tokio::test]
    async fn session_start_omits_verbatim_system_prompt_when_there_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(dir.path(), true, None, false).await;
        w.write_session_start(1, Vec::new()).await.unwrap();
        w.flush().await.unwrap();

        assert!(read_events(dir.path())[0].get("system_prompt").is_none());
    }

    /// The prompt is a session constant, like `model` — every task's `session_start` repeats it.
    #[tokio::test]
    async fn every_task_session_start_repeats_the_same_system_prompt_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer_with_prompt(dir.path(), false, Some("Be terse."), true).await;
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
            false,
            None,
            false,
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
            false,
            None,
            false,
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
                false,
                None,
                false,
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
            false,
            None,
            false,
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
        )
        .await
        .unwrap();
        w.flush().await.unwrap();

        let events = read_events(dir.path());
        let e = &events[0];
        assert_eq!(e["event_type"], "inference");
        assert_eq!(e["turn"], 0);
        assert_eq!(e["input_tokens"], 100);
        assert_eq!(e["output_tokens"], 50);
        assert_eq!(e["decision"], "tool_call");
        assert_eq!(e["tool_name"], "bash");
        // Confirm snake_case (not camelCase)
        assert!(e.get("inputTokens").is_none(), "must use snake_case");
        assert!(e.get("toolName").is_none(), "must use snake_case");
    }

    #[tokio::test]
    async fn tool_call_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_tool_call(
            1,
            "bash".to_string(),
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
        w.write_inference(0, 100, 50, "tool_call".to_string(), None, None)
            .await
            .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
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
        w.write_inference(0, 10, 5, "end_turn".to_string(), None, None)
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
        )
        .await
        .unwrap();
        w.write_tool_call(
            0,
            "bash".to_string(),
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
        w.write_inference(1, 60, 30, "end_turn".to_string(), None, None)
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
        let mut w = make_writer(dir.path()).await; // include_tool_output = false
        w.write_tool_call(
            0,
            "bash".to_string(),
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
        let mut w = make_writer(dir.path()).await; // include_tool_output = false
        w.write_tool_call(
            0,
            "bash".to_string(),
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
            "output must be absent when include_tool_output is false"
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
        let mut w = make_writer_with_opts(dir.path(), true).await;
        w.write_tool_call(
            0,
            "bash".to_string(),
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
            "output must be present when include_tool_output is true"
        );
        assert_eq!(e["output_bytes"], 60);
    }

    #[tokio::test]
    async fn task_end_carries_reopen_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = make_writer(dir.path()).await;
        w.write_task_start("tsk_1", "ctx_1", "a2a", 3)
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
        w.write_task_start("tsk_1", "ctx_1", "a2a", 3)
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
        w.write_task_start("tsk_1", "ctx_1", "a2a", 3)
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
        w.write_inference(0, 10, 5, "end_turn".to_string(), None, None)
            .await
            .unwrap();
        w.write_inference(1, 10, 5, "end_turn".to_string(), None, None)
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 2);
        // A new task resets the per-task counter.
        w.write_task_start("tsk_2", "ctx_2", "a2a", 3)
            .await
            .unwrap();
        assert_eq!(w.task_turns(), 0);
    }
}
