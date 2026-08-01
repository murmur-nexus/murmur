use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use wasmtime::{
    component::{Component, Linker},
    Store,
};
use wasmtime_wasi::{
    DirPerms, FilePerms, ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};
use wasmtime_wasi_http::{
    p2::{WasiHttpCtxView, WasiHttpView},
    WasiHttpCtx,
};

use murmur_artifact::{HookBinding, HookConfig, HookExecutionMode, PACKED_MANIFEST_ENTRY};

use crate::{
    bindings::hook::exports::murmur::hook::lifecycle::{
        CompactionEvent, HookOutput, InferenceEvent, Message, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    },
    errors::RuntimeError,
    inference_import::{add_inference_to_linker, HookInferenceCtx, HookInferenceRecord},
    limits::{classify_guest_failure, ExecutionLimiter, ExecutionLimits},
    network_policy::{resolve_scoped_dir, HookCapabilityGrant},
    runtime::NetworkPolicyHooks,
    types::StagedHookArtifact,
};

/// The one lifecycle instance export name the host accepts, matching the
/// `murmur:hook` version declared in `wit/`. The host keeps no compatibility
/// fallback: a hook compiled against any other version does not resolve, so a
/// WIT bump requires every hook artifact to be rebuilt (see `wit/VERSIONING.md`).
const LIFECYCLE_IFACE: &str = "murmur:hook/lifecycle@0.5.0";

/// Resolve the lifecycle instance export. `None` means the component does not
/// export [`LIFECYCLE_IFACE`], which surfaces as a missing-export error at the
/// call site.
fn resolve_lifecycle_iface(
    instance: &wasmtime::component::Instance,
    store: &mut Store<HookStoreState>,
) -> Option<wasmtime::component::ComponentExportIndex> {
    instance.get_export_index(&mut *store, None, LIFECYCLE_IFACE)
}

/// Diagnostic naming the accepted lifecycle export name, used wherever
/// resolution fails.
fn missing_lifecycle_msg(subject: &str) -> String {
    format!(
        "hook {subject} does not export {LIFECYCLE_IFACE}; rebuild the hook against the current WIT (run `mur install` for a default artifact, or rebuild from source otherwise)"
    )
}

pub(crate) struct HookRuntime {
    engine: wasmtime::Engine,
    /// Execution limits applied to every hook store — retained because async hooks are
    /// instantiated lazily, one fresh store per event, long after `new` returns.
    limits: ExecutionLimits,
    /// Session directory — used for error log paths and mounted as "." in hook WASI contexts.
    workdir: PathBuf,
    /// User-visible project directory — mounted as "/project" in hook WASI contexts
    /// so hooks can read project files while still writing output to the session dir.
    accessible_workdir: PathBuf,
    blocking_hooks: Vec<HookInstance>,
    async_hooks: Vec<AsyncHookSpec>,
    context: SessionContextData,
    /// Backing for the `murmur:runtime/inference` host import. `None` when the
    /// capsule has no usable inference driver — `run-inference` then returns a
    /// clear `err` instead of the import failing to link.
    inference: Option<Arc<HookInferenceCtx>>,
    /// Unsupported-arm faults produced by blocking hooks since the last drain, in
    /// dispatch order. Drained by the agent loop via [`Self::drain_dispatch_faults`]
    /// and written to `trace.jsonl` as `hook_dispatch_error` events before each
    /// `session_end` write. Mirrors the drain idiom used for `run-inference`
    /// records (see [`Self::drain_inference_records`]).
    dispatch_faults: Vec<DispatchFault>,
    started: Instant,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_tool_calls: u32,
    total_shell_calls: u32,
    total_turns: u32,
}

/// The six `murmur:hook/lifecycle` functions every compiled hook component must export;
/// a missing one is a hard, session-fatal error at instantiation time.
const REQUIRED_HOOK_FNS: [&str; 6] = [
    "on-session-start",
    "on-inference",
    "on-tool-call",
    "on-shell",
    "on-compaction",
    "on-session-end",
];

/// Lifecycle functions introduced after the original six. A hook component compiled
/// before these existed may omit them; it is simply not dispatched for those events.
const OPTIONAL_HOOK_FNS: [&str; 2] = ["on-task-start", "on-task-end"];

/// Single source of truth for which `hook-output` arm each of the nine lifecycle
/// events honors. Keyed by the WIT function name; the value is the one non-`none`
/// arm the runtime commits for that event, or `None` when the event honors nothing
/// beyond the always-silent `none`.
///
/// Both dispatch paths consult this table — the shared [`call_hook`] path (for the
/// eight events routed through [`HookRuntime::dispatch`]) and the separate
/// [`dispatch_stage`]/[`call_stage_once`] path (for `on-stage`) — so the
/// honored-arm decision lives in exactly one place. Any non-`none` arm a hook
/// returns that is *not* this event's honored arm is a loud, non-fatal
/// unsupported-arm fault (logged, and for the shared path also traced); see
/// [`classify_output`].
const HONORED_OUTPUT_ARM: &[(&str, Option<&str>)] = &[
    ("on-stage", Some("write-manifests")),
    ("on-session-start", None),
    ("on-task-start", None),
    ("on-inference", Some("artifact")),
    ("on-tool-call", None),
    ("on-shell", None),
    ("on-compaction", Some("replace-context")),
    ("on-task-end", Some("reopen-task")),
    ("on-session-end", None),
];

/// The `hook-output` arm `event` (a WIT lifecycle function name) honors beyond the
/// always-silent `none`, or `None` if it honors nothing else. Looks the event up in
/// [`HONORED_OUTPUT_ARM`]; an unknown name (which cannot occur for the nine declared
/// events) also yields `None`.
fn honored_arm(event: &str) -> Option<&'static str> {
    HONORED_OUTPUT_ARM
        .iter()
        .find(|(name, _)| *name == event)
        .and_then(|(_, arm)| *arm)
}

/// The `hook-output` variant name a hook returned, or `None` for the `none` arm.
/// The returned `&'static str` values are the same arm spellings used in
/// [`HONORED_OUTPUT_ARM`] and in the WIT `variant hook-output`.
fn output_arm_name(output: &HookOutput) -> Option<&'static str> {
    match output {
        HookOutput::None => None,
        HookOutput::ReplaceContext(_) => Some("replace-context"),
        HookOutput::WriteManifests(_) => Some("write-manifests"),
        HookOutput::Artifact(_) => Some("artifact"),
        HookOutput::ReopenTask(_) => Some("reopen-task"),
    }
}

/// What [`call_hook`] / [`dispatch_stage`] should do with one hook's returned
/// `hook-output` for a given event, decided solely against [`HONORED_OUTPUT_ARM`].
enum OutputDisposition {
    /// The `none` arm — always silent and free, from every event. Nothing committed,
    /// nothing logged, nothing traced.
    Ignore,
    /// The single non-`none` arm this event honors. The caller commits its payload
    /// exactly as before this table existed.
    Honored,
    /// A non-`none` arm this event does not honor — a loud, non-fatal fault. Carries
    /// the arm name for the log line and trace record.
    Fault(&'static str),
}

/// Classify `output` for `event` against the single honored-arm table.
fn classify_output(event: &str, output: &HookOutput) -> OutputDisposition {
    match output_arm_name(output) {
        None => OutputDisposition::Ignore,
        Some(arm) if Some(arm) == honored_arm(event) => OutputDisposition::Honored,
        Some(arm) => OutputDisposition::Fault(arm),
    }
}

/// The one-line diagnostic written to `logs/hook-<name>.log` (and mirrored in the
/// `hook_dispatch_error` trace record's fields) when a hook returns a non-`none`
/// arm the event does not honor. Names the hook, the event's WIT function name, and
/// the discarded arm so the fault is diagnosable without reading any Rust source.
fn format_dispatch_fault(hook_name: &str, event: &str, arm: &str) -> String {
    format!(
        "hook '{hook_name}' returned unsupported hook-output arm '{arm}' from '{event}'; \
         this event does not honor that arm, so the value was discarded"
    )
}

/// A blocking hook returned a non-`none` `hook-output` arm the event does not honor.
/// Buffered by [`HookRuntime::dispatch`] and drained via
/// [`HookRuntime::drain_dispatch_faults`] so the agent loop can write it to
/// `trace.jsonl` as a `hook_dispatch_error` event. Faults from `on-stage` (which
/// runs before the trace writer exists) and from async hooks (fire-and-forget) are
/// logged but never buffered here — matching how those two paths already handle a
/// genuine hook `Err`.
#[derive(Debug, Clone)]
pub(crate) struct DispatchFault {
    /// Manifest name of the hook that returned the unsupported arm.
    pub hook_name: String,
    /// WIT lifecycle function name the arm was returned from (e.g. `on-tool-call`).
    pub event: String,
    /// The unsupported `hook-output` arm name (e.g. `write-manifests`).
    pub arm: String,
}

struct HookInstance {
    name: String,
    config: HookConfig,
    store: Store<HookStoreState>,
    /// Name-based dispatch table keyed by the WIT function name
    /// (e.g. `"on-session-start"`). Always holds every entry in [`REQUIRED_HOOK_FNS`];
    /// entries from [`OPTIONAL_HOOK_FNS`] are present only if the component exports them.
    funcs: HashMap<String, wasmtime::component::Func>,
}

/// Async hooks are not instantiated eagerly — a fresh instance is spawned per event.
struct AsyncHookSpec {
    name: String,
    config: HookConfig,
    component: Component,
    /// Retained from staging so each per-event instantiation applies the same grant a
    /// blocking hook would get — there is no execution mode that escapes the capability model.
    grant: HookCapabilityGrant,
}

struct HookStoreState {
    /// Resource limiter for this store, registered via `Store::limiter`, and the record of
    /// any growth request it denied — read back by `classify_guest_failure` in
    /// [`call_typed`] so a limit trap reads distinguishably in `logs/hook-<name>.log`.
    limits: ExecutionLimiter,
    table: ResourceTable,
    wasi: WasiCtx,
    /// wasi-http context. A hook's *only* route to the network: [`build_wasi_ctx`] grants no
    /// raw WASI socket capability, so `wasi:http/outgoing-handler` — filtered by
    /// `http_hooks` below — is the whole outbound surface, exactly as for capsules and tools.
    http: WasiHttpCtx,
    /// Per-hook allow-list enforcement, built from this hook's `HookCapabilityGrant`. An
    /// empty rule set (the default, for a hook the operator granted no `capabilities.network`)
    /// denies every request.
    http_hooks: NetworkPolicyHooks,
}

#[derive(Debug, Clone, Copy)]
struct HookTotals {
    input_tokens: u64,
    output_tokens: u64,
    tool_calls: u32,
    shell_calls: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionContextData {
    pub capsule_name: String,
    pub capsule_version: String,
    pub session_id: String,
    pub model: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone)]
pub(crate) enum HookEvent {
    SessionStart,
    /// Fires once per task, immediately before that task's agent loop runs.
    /// For `task_acceptance: none`/`single` this coincides with `SessionStart`;
    /// for `task_acceptance: queue` it fires once per queued task.
    TaskStart {
        task_id: String,
        context_id: String,
        source: String,
        input_bytes: u64,
    },
    Inference {
        turn: u32,
        input_tokens: u64,
        output_tokens: u64,
        decision: String,
        tool_name: Option<String>,
        prompt: Option<String>,
        output: Option<String>,
        tools: Option<String>,
    },
    ToolCall {
        turn: u32,
        tool_name: String,
        input_bytes: u64,
        output_bytes: u64,
        duration_ms: u64,
        status: String,
    },
    Shell {
        turn: u32,
        /// The program that ran, canonicalized against the host `PATH` where possible
        /// (else the bare invoked name).
        binary: String,
        command: String,
        exit_code: i32,
        stdout: String,
        stderr: String,
        stdout_bytes: u64,
        stderr_bytes: u64,
        duration_ms: u64,
    },
    /// Fires when the session token threshold is reached. Routed through the same
    /// shared dispatch path as every other event; a blocking hook may return
    /// `replace-context` to swap out the conversation history.
    Compaction {
        messages: Vec<Message>,
        session_tokens: u64,
        threshold: f64,
        /// Manifest-configured model for the hook's own summarization call, from
        /// `inference.compaction.model`. `None` when the manifest leaves it unset —
        /// resolving that to a concrete model is the receiving hook's job, not ours.
        model: Option<String>,
        /// Manifest-configured system-prompt override for that call, from
        /// `inference.compaction.system_prompt`. `None` when the manifest leaves it
        /// unset — picking a default prompt is the receiving hook's job, not ours.
        system_prompt: Option<String>,
    },
    /// Fires once per task, immediately after that task's agent loop returns.
    TaskEnd {
        task_id: String,
        exit_status: String,
    },
    SessionEnd {
        total_turns: u32,
        exit_status: String,
    },
}

/// Outcome of dispatching one event to one blocking hook. The shared dispatch loop
/// funnels every event through [`call_hook`], which returns one of these regardless
/// of event type; the caller (`dispatch`) collects the relevant variant per event.
enum HookCallResult {
    /// No committable output (hook returned `none`/`write-manifests`, produced an
    /// artifact for an event that doesn't forward artifacts, or the optional
    /// function is absent from this component).
    None,
    /// A structured artifact payload (only forwarded for `on-inference`).
    Artifact(HookArtifact),
    /// A replacement conversation history (only meaningful for `on-compaction`).
    ReplaceContext(Vec<Message>),
    /// A control decision to reopen the task (only honored for `on-task-end`).
    /// Carries the requesting hook's manifest name and its feedback `reason`; the
    /// agent loop re-runs with that feedback injected into the task content, up to
    /// the capsule's reopen/turn budget. See [`TaskReopen`].
    Reopen { hook_name: String, reason: String },
    /// The hook returned a non-`none` arm the event does not honor. Non-fatal: the
    /// caller logs it (and, for the shared dispatch path, buffers it for the trace)
    /// and continues as if the hook had returned `none`. Carries the event's WIT
    /// function name and the discarded arm name.
    UnsupportedArm { event: String, arm: String },
}

/// A blocking `on-task-end` hook returned `reopen-task(reason)`: a control
/// decision that the task's agent loop should re-run with `reason` injected as
/// feedback rather than being finalized. Surfaced by [`HookRuntime::dispatch_task_end`]
/// and acted on by the runtime's per-task reopen loop, subject to the capsule's
/// `inference.max_task_reopens` budget and `inference.max_turns` ceiling.
#[derive(Debug, Clone)]
pub(crate) struct TaskReopen {
    /// Manifest name of the hook that requested the reopen.
    pub hook_name: String,
    /// Feedback text the hook wants injected into the reopened task's content.
    pub reason: String,
}

/// An artifact emitted by a blocking hook via `HookOutput::Artifact`.
/// Carries the hook's manifest name so callers can identify the source without
/// any hardcoded strings.
#[derive(Debug, Clone)]
pub(crate) struct HookArtifact {
    /// Manifest name of the hook that emitted this artifact.
    pub hook_name: String,
    /// JSON payload string returned by the hook.
    pub payload: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ShellDispatchInfo {
    /// The program that was actually invoked — a canonical absolute path when the host
    /// `PATH` resolves the invoked name, else the bare name. Distinct from `command`,
    /// which carries the argument list alone and therefore never names the binary.
    pub binary: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub duration_ms: u64,
}

/// Env vars injected into every hook's WASI context. Group avoids a >7-arg constructor.
#[derive(Default)]
pub(crate) struct HookEnvVars<'a> {
    pub otel_endpoint: Option<&'a str>,
    pub eval_config_json: Option<&'a str>,
    pub case_id: Option<&'a str>,
    pub dataset_id: Option<&'a str>,
}

/// Dispatch `on-stage` for all blocking hooks with a matching binding.
///
/// Called from `stage_session` which may be in a sync context; this function is sync
/// and uses a temporary current-thread runtime internally.
pub(crate) fn dispatch_stage(
    engine: &wasmtime::Engine,
    workdir: &Path,
    staged_hooks: &[StagedHookArtifact],
    shell_allow: Vec<String>,
    env_vars: &HookEnvVars<'_>,
    limits: ExecutionLimits,
) -> Result<(), RuntimeError> {
    let matching: Vec<&StagedHookArtifact> = staged_hooks
        .iter()
        .filter(|h| {
            matches!(h.config.execution_mode, HookExecutionMode::Blocking)
                && matches!(h.config.binding, HookBinding::OnStage | HookBinding::All)
        })
        .collect();

    if matching.is_empty() {
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| RuntimeError::Runtime(format!("failed to build on-stage runtime: {e}")))?;

    rt.block_on(async {
        let evt = StageEvent { shell_allow };
        for staged in &matching {
            match call_stage_once(engine, workdir, staged, &evt, env_vars, limits).await {
                // `on-stage` runs during staging, before `trace.jsonl` exists, so an
                // unsupported-arm fault here is logged but never traced — unlike the
                // shared `dispatch` path. The honored arm is decided by the same
                // `HONORED_OUTPUT_ARM` table via `classify_output`.
                Ok(output) => match classify_output("on-stage", &output) {
                    OutputDisposition::Honored => {
                        // `on-stage` honors only `write-manifests`.
                        if let HookOutput::WriteManifests(manifests) = output {
                            for m in manifests {
                                let dir = workdir.join("tools").join(&m.binary_name);
                                if let Err(e) = std::fs::create_dir_all(&dir) {
                                    log_hook_error(
                                        workdir,
                                        &staged.name,
                                        &format!(
                                            "failed to create tool dir for {}: {e}",
                                            m.binary_name
                                        ),
                                    )
                                    .await;
                                    continue;
                                }
                                let manifest_path = dir.join(PACKED_MANIFEST_ENTRY);
                                if let Err(e) = std::fs::write(&manifest_path, &m.content) {
                                    log_hook_error(
                                        workdir,
                                        &staged.name,
                                        &format!(
                                            "failed to write manifest for {}: {e}",
                                            m.binary_name
                                        ),
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    OutputDisposition::Fault(arm) => {
                        log_hook_error(
                            workdir,
                            &staged.name,
                            &format_dispatch_fault(&staged.name, "on-stage", arm),
                        )
                        .await;
                    }
                    OutputDisposition::Ignore => {}
                },
                Err(err) => log_hook_error(workdir, &staged.name, &err).await,
            }
        }
        Ok(())
    })
}

async fn call_stage_once(
    engine: &wasmtime::Engine,
    workdir: &Path,
    staged: &StagedHookArtifact,
    evt: &StageEvent,
    env_vars: &HookEnvVars<'_>,
    limits: ExecutionLimits,
) -> Result<HookOutput, String> {
    let mut linker: Linker<HookStoreState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    // `on-stage` runs during staging, long before an inference driver exists —
    // the import is defined so an inference-importing hook still links, and
    // always errors.
    add_inference_to_linker(&mut linker, format!("hook:{}", staged.name), None)?;

    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(workdir, env_vars, &staged.grant).map_err(|e| e.to_string())?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: staged.grant.network_allow_rules.clone(),
        },
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(limits.deadline_ticks());

    let instance = linker
        .instantiate_async(&mut store, &staged.component)
        .await
        .map_err(|e| {
            format!(
                "failed to instantiate hook {}@{}: {e}",
                staged.name, staged.version
            )
        })?;

    let obs_idx = resolve_lifecycle_iface(&instance, &mut store)
        .ok_or_else(|| missing_lifecycle_msg(&format!("{}@{}", staged.name, staged.version)))?;

    let func = instance
        .get_export_index(&mut store, Some(&obs_idx), "on-stage")
        .and_then(|idx| instance.get_func(&mut store, idx))
        .ok_or_else(|| format!("hook {}@{} missing on-stage", staged.name, staged.version))?;

    // `on-stage` runs on its own throwaway store, so this cannot reuse the
    // blocking-hook path.
    call_stage_lift::<HookOutput>(&mut store, &func, evt, &staged.name, limits).await
}

/// Call `on-stage` on a throwaway store, lifting its `result<O, string>` where `O` is
/// the version-appropriate `hook-output` type. Returns the hook's own returned error
/// as the `Err` string, exactly as the pre-versioning inline call did.
async fn call_stage_lift<O>(
    store: &mut Store<HookStoreState>,
    func: &wasmtime::component::Func,
    evt: &StageEvent,
    hook_name: &str,
    limits: ExecutionLimits,
) -> Result<O, String>
where
    O: wasmtime::component::ComponentType + wasmtime::component::Lift + 'static,
{
    let f = func
        .typed::<(StageEvent,), (Result<O, String>,)>(&*store)
        .map_err(|e| e.to_string())?;
    // Fresh budget for the call itself, so instantiation cost cannot eat into it.
    store.set_epoch_deadline(limits.deadline_ticks());
    let called = f.call_async(&mut *store, (evt.clone(),)).await;
    let (result,) = match called {
        Ok(result) => result,
        Err(err) => {
            let failure = classify_guest_failure(&err, &store.data().limits);
            return Err(failure.message(&format!("hook '{hook_name}' on-stage"), &err));
        }
    };
    f.post_return_async(&mut *store)
        .await
        .map_err(|e| e.to_string())?;
    result
}

impl HookRuntime {
    /// Instantiate all blocking hook components; register async specs without instantiating.
    ///
    /// `instantiate_async` is required because the engine has `async_support(true)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn new(
        engine: &wasmtime::Engine,
        workdir: &Path,
        accessible_workdir: &Path,
        staged_hooks: Vec<StagedHookArtifact>,
        context: SessionContextData,
        env_vars: HookEnvVars<'_>,
        limits: ExecutionLimits,
        inference: Option<Arc<HookInferenceCtx>>,
    ) -> Result<Self, RuntimeError> {
        let mut blocking_hooks = Vec::new();
        let mut async_hooks = Vec::new();

        for staged in staged_hooks {
            match staged.config.execution_mode {
                HookExecutionMode::Async => {
                    async_hooks.push(AsyncHookSpec {
                        name: staged.name,
                        config: staged.config,
                        component: staged.component,
                        grant: staged.grant,
                    });
                }
                HookExecutionMode::Blocking => {
                    let instance =
                        instantiate_blocking_hook(
                            engine,
                            workdir,
                            accessible_workdir,
                            &staged,
                            &env_vars,
                            limits,
                            inference.clone(),
                        )
                        .await?;
                    blocking_hooks.push(instance);
                }
            }
        }

        Ok(Self {
            engine: engine.clone(),
            limits,
            workdir: workdir.to_path_buf(),
            accessible_workdir: accessible_workdir.to_path_buf(),
            blocking_hooks,
            async_hooks,
            context,
            inference,
            dispatch_faults: Vec::new(),
            started: Instant::now(),
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tool_calls: 0,
            total_shell_calls: 0,
            total_turns: 0,
        })
    }

    /// Whole-launch turn count, accumulated across every task processed by this
    /// runtime (one increment per `Inference` event). Read by `runtime.rs` when it
    /// fires the once-per-launch `SessionEnd` event so `total-turns` is a launch
    /// aggregate rather than a single task's count.
    pub(crate) fn total_turns(&self) -> u32 {
        self.total_turns
    }

    /// Take every `run-inference` trace record buffered since the last drain.
    /// The agent loop calls this after hook dispatch and writes each one through
    /// the session's `TraceWriter`/`OtelEmitter` — one record per call, success
    /// or failure, tagged `hook:<name>`.
    pub(crate) fn drain_inference_records(&self) -> Vec<HookInferenceRecord> {
        self.inference
            .as_ref()
            .map(|ctx| ctx.drain_records())
            .unwrap_or_default()
    }

    /// Take every unsupported-arm fault a blocking hook produced since the last
    /// drain. The agent loop calls this immediately before each `session_end` write
    /// and records each one through the session's `TraceWriter` as a
    /// `hook_dispatch_error` event, so no fault is lost regardless of which exit path
    /// the session takes. `on-stage` and async faults are never buffered here (they
    /// are logged only), so this never carries them.
    pub(crate) fn drain_dispatch_faults(&mut self) -> Vec<DispatchFault> {
        std::mem::take(&mut self.dispatch_faults)
    }

    /// Dispatch `event` to all bound hooks. Returns every artifact emitted by
    /// any blocking hook via `HookOutput::Artifact`, in hook-registration order.
    /// An empty `Vec` means no hook produced an artifact for this event.
    pub(crate) async fn emit(&mut self, workdir: &Path, event: HookEvent) -> Vec<HookArtifact> {
        self.dispatch(workdir, event).await.0
    }

    /// Shared dispatch path used by every Lifecycle Event. Iterates the blocking
    /// hooks (binding-filtered), then spawns each matching async hook fire-and-forget.
    /// Returns `(artifacts, replacement, first_error, reopen)`: `artifacts` collects every
    /// `on-inference` `HookOutput::Artifact`; `replacement` is the first `on-compaction`
    /// `HookOutput::ReplaceContext`; `first_error` is the message of the first bound
    /// hook that returned `Err` for this event; `reopen` is the first `on-task-end`
    /// `HookOutput::ReopenTask`. Callers read only the half they need — `emit()` takes
    /// `artifacts`, `dispatch_compaction` `replacement`/`first_error`, `dispatch_task_end`
    /// `reopen` — and discard the rest; the per-hook error is still logged and the loop
    /// still continues, so `emit()`'s observable behaviour is unchanged. Event-keyed side
    /// effects (the running token/tool-call/shell-call totals) run here so all events
    /// funnel through one place.
    async fn dispatch(
        &mut self,
        workdir: &Path,
        event: HookEvent,
    ) -> (
        Vec<HookArtifact>,
        Option<Vec<Message>>,
        Option<String>,
        Option<TaskReopen>,
    ) {
        if let HookEvent::Inference {
            input_tokens,
            output_tokens,
            ..
        } = &event
        {
            self.total_input_tokens = self.total_input_tokens.saturating_add(*input_tokens);
            self.total_output_tokens = self.total_output_tokens.saturating_add(*output_tokens);
            self.total_turns = self.total_turns.saturating_add(1);
        }
        if matches!(event, HookEvent::ToolCall { .. }) {
            self.total_tool_calls = self.total_tool_calls.saturating_add(1);
        }
        if matches!(event, HookEvent::Shell { .. }) {
            self.total_shell_calls = self.total_shell_calls.saturating_add(1);
        }

        let totals = HookTotals {
            input_tokens: self.total_input_tokens,
            output_tokens: self.total_output_tokens,
            tool_calls: self.total_tool_calls,
            shell_calls: self.total_shell_calls,
        };

        let mut artifacts: Vec<HookArtifact> = Vec::new();
        let mut replacement: Option<Vec<Message>> = None;
        let mut reopen: Option<TaskReopen> = None;
        let mut first_error: Option<String> = None;
        // Faults are collected here and appended to `self.dispatch_faults` after the
        // loop: the loop holds `&mut self.blocking_hooks`, so `self` cannot be touched
        // otherwise while it runs.
        let mut faults: Vec<DispatchFault> = Vec::new();
        for hook in &mut self.blocking_hooks {
            if !binding_matches_event(&hook.config.binding, &event) {
                continue;
            }
            match call_hook(hook, &self.context, &event, self.started.elapsed(), totals).await {
                Ok(HookCallResult::Artifact(a)) => {
                    artifacts.push(a);
                }
                Ok(HookCallResult::ReplaceContext(msgs)) => {
                    if replacement.is_none() {
                        replacement = Some(msgs);
                    }
                }
                Ok(HookCallResult::Reopen { hook_name, reason }) => {
                    // First reopen-requesting hook wins, mirroring `replacement`.
                    if reopen.is_none() {
                        reopen = Some(TaskReopen { hook_name, reason });
                    }
                }
                Ok(HookCallResult::None) => {}
                Ok(HookCallResult::UnsupportedArm { event: ev, arm }) => {
                    // Non-fatal: log it, buffer it for the trace, and continue as if the
                    // hook had returned `none`. It is not an `Err`, so it never becomes
                    // `first_error` — compaction must not fail the session over it.
                    log_hook_error(
                        workdir,
                        &hook.name,
                        &format_dispatch_fault(&hook.name, &ev, &arm),
                    )
                    .await;
                    faults.push(DispatchFault {
                        hook_name: hook.name.clone(),
                        event: ev,
                        arm,
                    });
                }
                Err(error) => {
                    log_hook_error(workdir, &hook.name, &error).await;
                    if first_error.is_none() {
                        first_error = Some(format!("{}: {error}", hook.name));
                    }
                }
            }
        }
        self.dispatch_faults.append(&mut faults);

        for spec in &self.async_hooks {
            if !binding_matches_event(&spec.config.binding, &event) {
                continue;
            }
            let engine = self.engine.clone();
            let session_workdir = self.workdir.clone();
            let accessible_workdir = self.accessible_workdir.clone();
            let component = spec.component.clone();
            let name = spec.name.clone();
            let context = self.context.clone();
            let event = event.clone();
            let elapsed = self.started.elapsed();
            let limits = self.limits;
            let inference = self.inference.clone();
            let grant = spec.grant.clone();

            tokio::task::spawn_local(async move {
                if let Err(err) = call_async_hook(
                    &engine,
                    &accessible_workdir,
                    &component,
                    &name,
                    &context,
                    &event,
                    elapsed,
                    totals,
                    limits,
                    inference,
                    &grant,
                )
                .await
                {
                    log_hook_error(&session_workdir, &name, &err).await;
                }
            });
        }

        (artifacts, replacement, first_error, reopen)
    }

    /// Fire `on-compaction` on all hooks with a matching binding.
    ///
    /// The three outcomes a caller must be able to tell apart:
    /// - `Ok(Some(messages))` — a blocking hook returned `replace-context`.
    /// - `Ok(None)` — no compaction-bound hook was invoked at all (or every one of
    ///   them returned `none`): the caller continues, uncompacted.
    /// - `Err(message)` — a compaction-bound hook *ran* and returned `Err`. There is
    ///   no safety net behind a declared compaction hook, so the caller must fail the
    ///   session rather than limp on with an over-budget context.
    ///
    /// A thin wrapper over the shared [`Self::dispatch`] path: it builds a
    /// `HookEvent::Compaction` and reads both the `replacement` and `first_error`
    /// halves of the result. An error wins over a replacement produced by some other
    /// hook in the same dispatch — a partially-failed compaction is still a failure.
    /// Async hooks fire-and-forget; their output is always discarded. Checkpoint
    /// signing after a successful replacement happens inside `dispatch`.
    pub(crate) async fn dispatch_compaction(
        &mut self,
        messages: Vec<Message>,
        session_tokens: u64,
        threshold: f64,
        model: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<Option<Vec<Message>>, String> {
        let workdir = self.workdir.clone();
        let event = HookEvent::Compaction {
            messages,
            session_tokens,
            threshold,
            model,
            system_prompt,
        };
        let (_, replacement, first_error, _) = self.dispatch(&workdir, event).await;
        match first_error {
            Some(error) => Err(error),
            None => Ok(replacement),
        }
    }

    /// Fire `on-task-end` on all hooks with a matching binding and surface the first
    /// [`TaskReopen`] any blocking hook requested via `reopen-task`, or `None` if none
    /// did. A thin wrapper over the shared [`Self::dispatch`] path, mirroring
    /// [`Self::dispatch_compaction`]: it builds a `HookEvent::TaskEnd` and reads only
    /// the `reopen` half of the four-tuple. `on-task-end` honors no artifact or
    /// replacement arm, so those halves are discarded; async hooks still fire
    /// fire-and-forget. A hook-returned `Err` is logged per-hook inside `dispatch`
    /// and does not abort the task, matching `emit`.
    pub(crate) async fn dispatch_task_end(
        &mut self,
        task_id: String,
        exit_status: String,
    ) -> Option<TaskReopen> {
        let workdir = self.workdir.clone();
        let event = HookEvent::TaskEnd {
            task_id,
            exit_status,
        };
        let (_, _, _, reopen) = self.dispatch(&workdir, event).await;
        reopen
    }
}

fn binding_matches_event(binding: &HookBinding, event: &HookEvent) -> bool {
    match binding {
        // `on-stage` is the only event never routed through `dispatch`, so `All`
        // (which excludes on-stage) matches every event that can reach here.
        HookBinding::All => true,
        HookBinding::OnStage => false, // on-stage fired separately during staging
        HookBinding::OnSessionStart => matches!(event, HookEvent::SessionStart),
        HookBinding::OnTaskStart => matches!(event, HookEvent::TaskStart { .. }),
        HookBinding::OnInference => matches!(event, HookEvent::Inference { .. }),
        HookBinding::OnToolCall => matches!(event, HookEvent::ToolCall { .. }),
        HookBinding::OnShell => matches!(event, HookEvent::Shell { .. }),
        HookBinding::OnCompaction => matches!(event, HookEvent::Compaction { .. }),
        HookBinding::OnTaskEnd => matches!(event, HookEvent::TaskEnd { .. }),
        HookBinding::OnSessionEnd => matches!(event, HookEvent::SessionEnd { .. }),
    }
}

async fn instantiate_blocking_hook(
    engine: &wasmtime::Engine,
    _workdir: &Path,
    project_dir: &Path,
    staged: &StagedHookArtifact,
    env_vars: &HookEnvVars<'_>,
    limits: ExecutionLimits,
    inference: Option<Arc<HookInferenceCtx>>,
) -> Result<HookInstance, RuntimeError> {
    let mut linker: Linker<HookStoreState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    add_inference_to_linker(&mut linker, format!("hook:{}", staged.name), inference)
        .map_err(RuntimeError::Runtime)?;

    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(project_dir, env_vars, &staged.grant)?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: staged.grant.network_allow_rules.clone(),
        },
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(limits.deadline_ticks());

    let instance = linker
        .instantiate_async(&mut store, &staged.component)
        .await
        .map_err(|err| {
            RuntimeError::Runtime(format!(
                "failed to instantiate hook {}@{}: {err}",
                staged.name, staged.version
            ))
        })?;

    let obs_idx = resolve_lifecycle_iface(&instance, &mut store).ok_or_else(|| {
        RuntimeError::Runtime(missing_lifecycle_msg(&format!(
            "{}@{}",
            staged.name, staged.version
        )))
    })?;

    let funcs = resolve_hook_fns(&instance, &mut store, &obs_idx, |fn_name| {
        RuntimeError::Runtime(format!(
            "hook {}@{} missing function {LIFECYCLE_IFACE}#{fn_name}",
            staged.name, staged.version
        ))
    })?;

    Ok(HookInstance {
        funcs,
        name: staged.name.clone(),
        config: staged.config.clone(),
        store,
    })
}

/// Build the name-based dispatch table for a hook component: every function in
/// [`REQUIRED_HOOK_FNS`] must resolve (a miss returns `Err(missing(fn_name))`),
/// while [`OPTIONAL_HOOK_FNS`] are inserted only when the component exports them —
/// this is what lets a pre-`on-task-start`/`on-task-end` component instantiate
/// cleanly and simply never receive those two events.
fn resolve_hook_fns<E>(
    instance: &wasmtime::component::Instance,
    store: &mut Store<HookStoreState>,
    obs_idx: &wasmtime::component::ComponentExportIndex,
    missing: impl Fn(&str) -> E,
) -> Result<HashMap<String, wasmtime::component::Func>, E> {
    let mut funcs = HashMap::new();
    for fn_name in REQUIRED_HOOK_FNS {
        let func = instance
            .get_export_index(&mut *store, Some(obs_idx), fn_name)
            .and_then(|idx| instance.get_func(&mut *store, idx))
            .ok_or_else(|| missing(fn_name))?;
        funcs.insert(fn_name.to_string(), func);
    }
    for fn_name in OPTIONAL_HOOK_FNS {
        if let Some(func) = instance
            .get_export_index(&mut *store, Some(obs_idx), fn_name)
            .and_then(|idx| instance.get_func(&mut *store, idx))
        {
            funcs.insert(fn_name.to_string(), func);
        }
    }
    Ok(funcs)
}

impl WasiView for HookStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Routes every hook outbound HTTP request through the same `NetworkPolicyHooks` gate that
/// `CapsuleStoreState`/`ToolStoreState` use — one enforcement implementation, three guest
/// kinds. The rules come from the hook's own grant, so a hook is never widened by the
/// capsule-wide `capabilities.network.allow`.
impl WasiHttpView for HookStoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

/// Invoke one named `murmur:hook/lifecycle` function on a hook instance with a single
/// record argument, awaiting its `result<hook-output, string>`.
///
/// Returns `Ok(None)` when the function is absent from this component's dispatch table —
/// only ever true for an [`OPTIONAL_HOOK_FNS`] entry, which is how a hook that predates
/// `on-task-start`/`on-task-end` is silently skipped for those events. A hook-returned
/// `Err` is propagated as the call error, matching the pre-refactor behavior.
async fn call_typed<T>(
    hook: &mut HookInstance,
    fn_name: &str,
    arg: T,
) -> Result<Option<HookOutput>, String>
where
    T: wasmtime::component::ComponentType + wasmtime::component::Lower,
{
    let func = match hook.funcs.get(fn_name) {
        Some(f) => *f,
        None => return Ok(None),
    };
    let out = invoke_typed::<T, HookOutput>(hook, &func, fn_name, arg).await?;
    Ok(Some(out))
}

/// Invoke one component `func` with a single record arg and lift its
/// `result<hook-output, string>`. The `Err` variant carries either an infra
/// failure or the hook's own returned error string.
async fn invoke_typed<T, O>(
    hook: &mut HookInstance,
    func: &wasmtime::component::Func,
    fn_name: &str,
    arg: T,
) -> Result<O, String>
where
    T: wasmtime::component::ComponentType + wasmtime::component::Lower,
    O: wasmtime::component::ComponentType + wasmtime::component::Lift + 'static,
{
    let f = func
        .typed::<(T,), (Result<O, String>,)>(&hook.store)
        .map_err(|e| e.to_string())?;

    // A blocking hook's store outlives every event it handles, so each lifecycle call is
    // given its own fresh budget rather than sharing one across the whole session.
    let ticks = hook.store.data().limits.limits().deadline_ticks();
    hook.store.set_epoch_deadline(ticks);

    let called = f.call_async(&mut hook.store, (arg,)).await;
    let (result,) = match called {
        Ok(result) => result,
        Err(err) => {
            // Still a plain `String` on the same `Err` path an ordinary hook error takes —
            // `dispatch` isolates it via `log_hook_error` and the session continues.
            // Classification only makes the message name the limit that fired.
            let failure = classify_guest_failure(&err, &hook.store.data().limits);
            let subject = format!("hook '{}' {fn_name}", hook.name);
            return Err(failure.message(&subject, &err));
        }
    };
    f.post_return_async(&mut hook.store)
        .await
        .map_err(|e| e.to_string())?;
    result
}

/// Call one hook instance for a runtime event via its name-based dispatch table.
///
/// Maps each `HookEvent` to its WIT function name, builds the matching record, and
/// invokes it through [`call_typed`]. Only `on-inference` forwards a `HookOutput::Artifact`
/// and only `on-compaction` forwards a `HookOutput::ReplaceContext`; every other event
/// discards the output. A missing optional function yields `HookCallResult::None`.
async fn call_hook(
    hook: &mut HookInstance,
    context: &SessionContextData,
    event: &HookEvent,
    elapsed: Duration,
    totals: HookTotals,
) -> Result<HookCallResult, String> {
    let output = match event {
        HookEvent::SessionStart => {
            let ctx = SessionContext {
                capsule_name: context.capsule_name.clone(),
                capsule_version: context.capsule_version.clone(),
                session_id: context.session_id.clone(),
                model: context.model.clone(),
                capabilities: context.capabilities.clone(),
            };
            call_typed(hook, "on-session-start", ctx).await?
        }
        HookEvent::TaskStart {
            task_id,
            context_id,
            source,
            input_bytes,
        } => {
            let evt = TaskStartEvent {
                task_id: task_id.clone(),
                context_id: context_id.clone(),
                source: source.clone(),
                input_bytes: *input_bytes,
            };
            call_typed(hook, "on-task-start", evt).await?
        }
        HookEvent::Inference {
            turn,
            input_tokens,
            output_tokens,
            decision,
            tool_name,
            prompt,
            output,
            tools,
        } => {
            let evt = InferenceEvent {
                turn: *turn,
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
                decision: decision.clone(),
                tool_name: tool_name.clone(),
                prompt: prompt.clone(),
                output: output.clone(),
                tools: tools.clone(),
            };
            call_typed(hook, "on-inference", evt).await?
        }
        HookEvent::ToolCall {
            turn,
            tool_name,
            input_bytes,
            output_bytes,
            duration_ms,
            status,
        } => {
            let evt = ToolEvent {
                turn: *turn,
                tool_name: tool_name.clone(),
                input_bytes: *input_bytes,
                output_bytes: *output_bytes,
                duration_ms: *duration_ms,
                status: status.clone(),
            };
            call_typed(hook, "on-tool-call", evt).await?
        }
        HookEvent::Shell {
            turn,
            binary,
            command,
            exit_code,
            stdout,
            stderr,
            stdout_bytes,
            stderr_bytes,
            duration_ms,
        } => {
            // `binary` is not truncated the way `command` is — a clipped path names a
            // different file, or none.
            let evt = ShellEvent {
                turn: *turn,
                binary: binary.clone(),
                command: command.chars().take(200).collect(),
                exit_code: *exit_code,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
                stdout_bytes: *stdout_bytes,
                stderr_bytes: *stderr_bytes,
                duration_ms: *duration_ms,
            };
            call_typed(hook, "on-shell", evt).await?
        }
        HookEvent::Compaction {
            messages,
            session_tokens,
            threshold,
            model,
            system_prompt,
        } => {
            let evt = CompactionEvent {
                messages: messages.clone(),
                session_tokens: *session_tokens,
                threshold: *threshold,
                model: model.clone(),
                system_prompt: system_prompt.clone(),
            };
            call_typed(hook, "on-compaction", evt).await?
        }
        HookEvent::TaskEnd {
            task_id,
            exit_status,
        } => {
            let evt = TaskEndEvent {
                task_id: task_id.clone(),
                exit_status: exit_status.clone(),
            };
            call_typed(hook, "on-task-end", evt).await?
        }
        HookEvent::SessionEnd {
            total_turns,
            exit_status,
        } => {
            let evt = SessionEndEvent {
                total_turns: *total_turns,
                total_input_tokens: totals.input_tokens,
                total_output_tokens: totals.output_tokens,
                total_tool_calls: totals.tool_calls,
                total_shell_calls: totals.shell_calls,
                duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
                exit_status: exit_status.clone(),
            };
            call_typed(hook, "on-session-end", evt).await?
        }
    };

    // A single decision against `HONORED_OUTPUT_ARM`: commit the one arm this event
    // honors (only `on-inference`/`artifact` and `on-compaction`/`replace-context`
    // among the eight events reaching here), stay silent for `none` and for an
    // absent optional function, and turn any other non-`none` arm into an
    // unsupported-arm fault the caller logs and traces.
    let event_fn = event_fn_name(event);
    Ok(match output {
        // Optional function absent from this component — never a fault; the hook
        // simply is not dispatched for this event.
        None => HookCallResult::None,
        Some(out) => match classify_output(event_fn, &out) {
            OutputDisposition::Ignore => HookCallResult::None,
            OutputDisposition::Honored => match out {
                HookOutput::Artifact(payload) => HookCallResult::Artifact(HookArtifact {
                    hook_name: hook.name.clone(),
                    payload,
                }),
                HookOutput::ReplaceContext(msgs) => HookCallResult::ReplaceContext(msgs),
                // `reopen-task` is honored only by `on-task-end`; the runtime's
                // reopen loop reads this off `dispatch_task_end`.
                HookOutput::ReopenTask(reason) => HookCallResult::Reopen {
                    hook_name: hook.name.clone(),
                    reason,
                },
                // `write-manifests` is honored only by `on-stage`, which is never
                // dispatched through `call_hook`; no other arm is honored by any of
                // the eight events reaching here.
                _ => HookCallResult::None,
            },
            OutputDisposition::Fault(arm) => HookCallResult::UnsupportedArm {
                event: event_fn.to_string(),
                arm: arm.to_string(),
            },
        },
    })
}

/// The WIT lifecycle function name a [`HookEvent`] dispatches to. Used to key
/// [`HONORED_OUTPUT_ARM`] and to name the event in an unsupported-arm fault.
fn event_fn_name(event: &HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "on-session-start",
        HookEvent::TaskStart { .. } => "on-task-start",
        HookEvent::Inference { .. } => "on-inference",
        HookEvent::ToolCall { .. } => "on-tool-call",
        HookEvent::Shell { .. } => "on-shell",
        HookEvent::Compaction { .. } => "on-compaction",
        HookEvent::TaskEnd { .. } => "on-task-end",
        HookEvent::SessionEnd { .. } => "on-session-end",
    }
}

/// Fresh-instantiation call for async hooks (output discarded).
#[allow(clippy::too_many_arguments)]
async fn call_async_hook(
    engine: &wasmtime::Engine,
    root_dir: &Path,
    component: &Component,
    name: &str,
    context: &SessionContextData,
    event: &HookEvent,
    elapsed: Duration,
    totals: HookTotals,
    limits: ExecutionLimits,
    inference: Option<Arc<HookInferenceCtx>>,
    grant: &HookCapabilityGrant,
) -> Result<(), String> {
    let mut linker: Linker<HookStoreState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker).map_err(|e| e.to_string())?;
    add_inference_to_linker(&mut linker, format!("hook:{name}"), inference)?;

    let env = HookEnvVars::default();
    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(root_dir, &env, grant).map_err(|e| e.to_string())?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: grant.network_allow_rules.clone(),
        },
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(limits.deadline_ticks());

    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(|e| e.to_string())?;

    let obs_idx =
        resolve_lifecycle_iface(&instance, &mut store).ok_or_else(|| missing_lifecycle_msg(name))?;

    let funcs = resolve_hook_fns(&instance, &mut store, &obs_idx, |fn_name| {
        format!("hook {name} missing {LIFECYCLE_IFACE}#{fn_name}")
    })?;

    let mut tmp = HookInstance {
        name: name.to_string(),
        config: HookConfig::default(),
        funcs,
        store,
    };
    // Async hooks fire-and-forget; any committable output is intentionally discarded.
    // An unsupported-arm result is routed to the same `Err` channel a genuine hook
    // error takes, so the caller's existing `log_hook_error` records it once — logged
    // but never traced, matching how async errors are already handled.
    match call_hook(&mut tmp, context, event, elapsed, totals).await? {
        HookCallResult::UnsupportedArm { event: ev, arm } => {
            Err(format_dispatch_fault(name, &ev, &arm))
        }
        _ => Ok(()),
    }
}

/// Build a WASI context for a hook instance, governed by `grant` — the capability block the
/// capsule operator declared on this hook's entry in their own manifest.
///
/// Default-deny on both axes, and the default is what an ungranted hook gets:
///
/// - **Network.** Nothing here grants network capability. `inherit_network()` and
///   `allow_ip_name_lookup(true)` are deliberately absent, so a hook has no raw WASI sockets
///   under any grant. Its only outbound route is `wasi:http/outgoing-handler`, linked at each
///   instantiation site and filtered by `NetworkPolicyHooks` against
///   `grant.network_allow_rules` — the same gated path capsules and tools use. An empty rule
///   list denies every request.
/// - **Filesystem.** No preopened directory unless `grant.filesystem_scope` names one, in
///   which case exactly one directory is preopened: `root_dir/<scope>`, mounted as the hook's
///   current directory `"."`. Nothing above or beside that subtree is reachable, because a
///   guest can only name paths under a preopen. The scope was already validated (relative,
///   non-escaping) by `HookCapabilityGrant::derive` at staging time; it is created here if it
///   does not exist, and a creation failure fails the instantiation rather than silently
///   downgrading to no access.
///
/// `root_dir` is the hook's working directory: the session dir for `on-stage`, the project
/// directory for blocking and async hooks.
fn build_wasi_ctx(
    root_dir: &Path,
    env: &HookEnvVars<'_>,
    grant: &HookCapabilityGrant,
) -> Result<WasiCtx, RuntimeError> {
    // No host env inheritance: the explicit `MURMUR_*` injections below are the entire
    // environment a hook component ever sees. Hooks have no manifest-declared allowlist
    // because no hook artifact reads an arbitrary host var.
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio();

    if let Some(endpoint) = env.otel_endpoint {
        builder.env("MURMUR_OTEL_ENDPOINT", endpoint);
    }
    if let Ok(formation_id) = std::env::var("MURMUR_FORMATION_ID") {
        builder.env("MURMUR_FORMATION_ID", &formation_id);
    }
    if let Some(config_json) = env.eval_config_json {
        builder.env("MURMUR_EVAL_CONFIG", config_json);
    }
    if let Some(id) = env.case_id {
        builder.env("MURMUR_CASE_ID", id);
    }
    if let Some(id) = env.dataset_id {
        builder.env("MURMUR_DATASET_ID", id);
    }

    if let Some(scope) = grant.filesystem_scope.as_deref() {
        let scoped_dir = resolve_scoped_dir(root_dir, scope)?;
        builder
            .preopened_dir(&scoped_dir, ".", DirPerms::all(), FilePerms::all())
            .map_err(|err| RuntimeError::wasi(scoped_dir, err.to_string()))?;
    }

    Ok(builder.build())
}

async fn log_hook_error(workdir: &Path, hook_name: &str, error: &str) {
    use tokio::io::AsyncWriteExt;
    let log_dir = workdir.join("logs");
    let _ = tokio::fs::create_dir_all(&log_dir).await;
    let path: PathBuf = log_dir.join(format!("hook-{hook_name}.log"));
    let line = format!("{error}\n");
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        let _ = file.write_all(line.as_bytes()).await;
        // `tokio::fs::File` buffers, and dropping it does NOT flush — without this the
        // line reaches disk only if the runtime happens to get around to it, which made
        // "the hook error was logged" a coin flip for anything reading the file back.
        let _ = file.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_import::INFERENCE_IFACE_VERSIONED;
    use tempfile::TempDir;

    /// A retired lifecycle instance name. The host accepts [`LIFECYCLE_IFACE`] and
    /// nothing else, so a double exporting this must fail to resolve — that is what
    /// the no-fallback tests below assert.
    const RETIRED_IFACE: &str = "murmur:hook/lifecycle@0.4.0";

    /// Engine configured like the production one (component model + async + epoch
    /// interruption) so a hand-authored WAT component double can be compiled and
    /// instantiated. Epoch interruption only arms the mechanism — a deadline fires solely
    /// when an [`EpochTicker`] is also running, which only the deadline test spawns.
    fn hook_test_engine() -> wasmtime::Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        wasmtime::Engine::new(&config).expect("engine builds")
    }

    /// A minimal `murmur:hook/lifecycle` component double exporting exactly `fn_names`,
    /// each backed by a trivial `func() -> ()` core function. Resolution in
    /// `resolve_hook_fns` only looks exports up by name — it never inspects signatures
    /// or calls them — so these stubs are sufficient to exercise the required-vs-optional
    /// instantiation logic (invariants 5 and 6).
    fn hook_double(engine: &wasmtime::Engine, fn_names: &[&str]) -> Component {
        // Default double exports the current versioned instance name, so the
        // required/optional suite exercises real resolution against the version the
        // host probes first.
        hook_double_iface(engine, LIFECYCLE_IFACE, fn_names)
    }

    /// Like [`hook_double`] but the exported lifecycle instance carries the given
    /// instance name, so tests can build a component that exports the versioned
    /// (`murmur:hook/lifecycle@0.2.0`), the legacy unversioned, or a
    /// deliberately-unmatched name to exercise `resolve_lifecycle_iface` and its
    /// hard-error path.
    fn hook_double_iface(engine: &wasmtime::Engine, iface: &str, fn_names: &[&str]) -> Component {
        let exports = fn_names
            .iter()
            .map(|n| format!("    (export \"{n}\" (func $f))"))
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!(
            "(component\n\
             (core module $m (func (export \"f\")))\n\
             (core instance $i (instantiate $m))\n\
             (func $f (canon lift (core func $i \"f\")))\n\
             (instance $lc\n{exports}\n)\n\
             (export \"{iface}\" (instance $lc))\n\
             )"
        );
        let bytes = wat::parse_str(&wat).expect("component WAT parses");
        Component::new(engine, &bytes).expect("component double compiles")
    }

    /// Default-deny staging, matching a manifest hook entry with no `capabilities:` block.
    fn staged_double(component: Component) -> StagedHookArtifact {
        staged_double_granted(component, HookCapabilityGrant::default())
    }

    /// [`staged_double`] with an explicit grant, for the capability suite.
    fn staged_double_granted(
        component: Component,
        grant: HookCapabilityGrant,
    ) -> StagedHookArtifact {
        StagedHookArtifact {
            name: "test-hook".to_string(),
            version: "0.0.1".to_string(),
            component,
            config: HookConfig::default(),
            grant,
        }
    }

    async fn new_with_hooks(
        engine: &wasmtime::Engine,
        workdir: &Path,
        accessible: &Path,
        staged: Vec<StagedHookArtifact>,
    ) -> Result<HookRuntime, RuntimeError> {
        new_with_hooks_limited(
            engine,
            workdir,
            accessible,
            staged,
            ExecutionLimits::default(),
        )
        .await
    }

    /// [`new_with_hooks`] with an explicit limits block, so the deadline suite can ask for
    /// a budget short enough to observe inside a test.
    async fn new_with_hooks_limited(
        engine: &wasmtime::Engine,
        workdir: &Path,
        accessible: &Path,
        staged: Vec<StagedHookArtifact>,
        limits: ExecutionLimits,
    ) -> Result<HookRuntime, RuntimeError> {
        HookRuntime::new(
            engine,
            workdir,
            accessible,
            staged,
            SessionContextData {
                capsule_name: "test-capsule".to_string(),
                capsule_version: "0.1.0".to_string(),
                session_id: "sess-test".to_string(),
                model: "test-model".to_string(),
                capabilities: Vec::new(),
            },
            HookEnvVars::default(),
            limits,
            None,
        )
        .await
    }

    /// Invariant 5: a pre-`on-task-start`/`on-task-end` component (all six original
    /// exports, neither task export) instantiates cleanly and is simply never dispatched
    /// for the two task events — no crash, no error logged.
    #[test]
    fn blocking_hook_missing_task_fns_instantiates_and_skips_them() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_double(&engine, &REQUIRED_HOOK_FNS);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks =
                new_with_hooks(&engine, session.path(), accessible.path(), vec![staged_double(component)])
                    .await
                    .expect("a hook lacking on-task-start/on-task-end must still instantiate");

            assert_eq!(hooks.blocking_hooks.len(), 1);
            let funcs = &hooks.blocking_hooks[0].funcs;
            for name in REQUIRED_HOOK_FNS {
                assert!(funcs.contains_key(name), "required fn {name} must resolve");
            }
            assert!(!funcs.contains_key("on-task-start"));
            assert!(!funcs.contains_key("on-task-end"));

            // Dispatching a task event must be a silent no-op for this hook: because the
            // function is absent it is skipped before any wasm call, so no error is logged.
            hooks
                .emit(
                    session.path(),
                    HookEvent::TaskStart {
                        task_id: "tsk_1".to_string(),
                        context_id: "ctx_1".to_string(),
                        source: "a2a".to_string(),
                        input_bytes: 3,
                    },
                )
                .await;
            hooks
                .emit(
                    session.path(),
                    HookEvent::TaskEnd {
                        task_id: "tsk_1".to_string(),
                        exit_status: "ok".to_string(),
                    },
                )
                .await;
        });

        assert!(
            !session.path().join("logs").join("hook-test-hook.log").exists(),
            "a hook that lacks the task exports must not log an error when task events fire"
        );
    }

    /// A component that *does* export the two optional task functions registers them,
    /// so they can be dispatched.
    #[test]
    fn blocking_hook_with_task_fns_registers_them() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let mut names: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        names.extend_from_slice(&OPTIONAL_HOOK_FNS);
        let component = hook_double(&engine, &names);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let hooks =
                new_with_hooks(&engine, session.path(), accessible.path(), vec![staged_double(component)])
                    .await
                    .expect("component exporting task fns instantiates");
            let funcs = &hooks.blocking_hooks[0].funcs;
            assert!(funcs.contains_key("on-task-start"));
            assert!(funcs.contains_key("on-task-end"));
        });
    }

    fn staged_double_named(
        name: &str,
        binding: HookBinding,
        component: Component,
    ) -> StagedHookArtifact {
        StagedHookArtifact {
            name: name.to_string(),
            version: "0.0.1".to_string(),
            component,
            config: HookConfig {
                binding,
                ..HookConfig::default()
            },
            grant: HookCapabilityGrant::default(),
        }
    }

    fn hook_log_lines(session: &Path, hook_name: &str) -> usize {
        let path = session.join("logs").join(format!("hook-{hook_name}.log"));
        std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    /// Simulates the `runtime.rs` dispatch sequence for a `task_acceptance: queue` launch
    /// that processes three tasks: `on-session-start` once, then a
    /// `on-task-start`/inference/`on-task-end` trio per task, then `on-session-end` once.
    ///
    /// Each hook double is bound to a single lifecycle event and its exported functions
    /// have a deliberately mismatched (no-arg) signature, so every *attempted* dispatch
    /// fails `.typed()` and appends exactly one line to that hook's error log. Counting
    /// log lines therefore counts invocations: the session-bound hooks must be hit once
    /// each while the task-bound hooks are hit once per task. Also asserts `total_turns`
    /// is the whole-launch aggregate (one per inference), which is what the once-per-launch
    /// `SessionEnd` carries.
    #[test]
    fn queue_mode_fires_session_events_once_and_task_events_per_task() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        // Every double exports all eight lifecycle functions; the per-hook binding (not the
        // exports) decides which events reach it.
        let mut all_fns: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        all_fns.extend_from_slice(&OPTIONAL_HOOK_FNS);
        let component = hook_double(&engine, &all_fns);

        let staged = vec![
            staged_double_named("sess-start", HookBinding::OnSessionStart, component.clone()),
            staged_double_named("task-start", HookBinding::OnTaskStart, component.clone()),
            staged_double_named("task-end", HookBinding::OnTaskEnd, component.clone()),
            staged_double_named("sess-end", HookBinding::OnSessionEnd, component.clone()),
        ];

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(&engine, session.path(), accessible.path(), staged)
                .await
                .expect("all doubles export the six required fns and instantiate");

            // once before the loop
            hooks.emit(session.path(), HookEvent::SessionStart).await;
            // three tasks, each a start / one inference / end trio
            for t in 0..3u32 {
                hooks
                    .emit(
                        session.path(),
                        HookEvent::TaskStart {
                            task_id: format!("tsk_{t}"),
                            context_id: format!("ctx_{t}"),
                            source: "a2a".to_string(),
                            input_bytes: 1,
                        },
                    )
                    .await;
                hooks
                    .emit(
                        session.path(),
                        HookEvent::Inference {
                            turn: 0,
                            input_tokens: 0,
                            output_tokens: 0,
                            decision: "end_turn".to_string(),
                            tool_name: None,
                            prompt: None,
                            output: None,
                            tools: None,
                        },
                    )
                    .await;
                hooks
                    .emit(
                        session.path(),
                        HookEvent::TaskEnd {
                            task_id: format!("tsk_{t}"),
                            exit_status: "ok".to_string(),
                        },
                    )
                    .await;
            }
            // once after the loop, carrying the whole-launch turn aggregate
            let total_turns = hooks.total_turns();
            assert_eq!(total_turns, 3, "one turn accumulated per inference across tasks");
            hooks
                .emit(
                    session.path(),
                    HookEvent::SessionEnd {
                        total_turns,
                        exit_status: "ok".to_string(),
                    },
                )
                .await;
        });

        assert_eq!(hook_log_lines(session.path(), "sess-start"), 1, "on-session-start fires once");
        assert_eq!(hook_log_lines(session.path(), "sess-end"), 1, "on-session-end fires once");
        assert_eq!(hook_log_lines(session.path(), "task-start"), 3, "on-task-start fires per task");
        assert_eq!(hook_log_lines(session.path(), "task-end"), 3, "on-task-end fires per task");
    }

    /// Invariant 6: a component missing one of the six original functions still fails
    /// `HookRuntime::new` with an error naming the missing function — the fail-fast
    /// diagnostic is not relaxed to a silent skip for the required six.
    #[test]
    fn blocking_hook_missing_required_fn_fails_new() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // Every original export except on-session-start.
        let names: Vec<&str> = REQUIRED_HOOK_FNS
            .iter()
            .copied()
            .filter(|n| *n != "on-session-start")
            .collect();
        let component = hook_double(&engine, &names);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let msg = match new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double(component)],
            )
            .await
            {
                Ok(_) => panic!("missing a required lifecycle fn must fail instantiation"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains("on-session-start"),
                "error must name the missing function, got: {msg}"
            );
        });
    }

    /// A hook component built against the *current* versioned
    /// `murmur:hook/lifecycle@0.5.0` interface (the name a freshly-compiled hook
    /// carries) instantiates and registers every required and optional function.
    /// The current versioned name is the one `resolve_lifecycle_iface` probes first.
    #[test]
    fn versioned_hook_double_instantiates_and_registers_fns() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let mut names: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        names.extend_from_slice(&OPTIONAL_HOOK_FNS);
        let component = hook_double_iface(&engine, LIFECYCLE_IFACE, &names);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double(component)],
            )
            .await
            .expect("a hook exporting the versioned lifecycle name must instantiate");
            assert_eq!(hooks.blocking_hooks.len(), 1);
            let funcs = &hooks.blocking_hooks[0].funcs;
            for name in REQUIRED_HOOK_FNS {
                assert!(funcs.contains_key(name), "required fn {name} must resolve");
            }
            assert!(funcs.contains_key("on-task-start"));
            assert!(funcs.contains_key("on-task-end"));
        });
    }

    /// A component exporting only the *legacy unversioned* `murmur:hook/lifecycle`
    /// instance — as hooks published before the versioned WIT did — no longer
    /// instantiates. The fallback probe was removed, so instantiation fails hard
    /// with a missing-export error that names the versioned interface the host
    /// expected and points the author at rebuilding.
    #[test]
    fn unversioned_only_hook_double_fails_hard() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_double_iface(&engine, "murmur:hook/lifecycle", &REQUIRED_HOOK_FNS);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let msg = match new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double(component)],
            )
            .await
            {
                Ok(_) => panic!("a hook exporting only the unversioned lifecycle name must fail"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains(LIFECYCLE_IFACE),
                "error must name the versioned lifecycle export, got: {msg}"
            );
            assert!(
                msg.contains("rebuild"),
                "error must hint at rebuilding the hook, got: {msg}"
            );
        });
    }

    /// When the versioned lifecycle instance name does not resolve (here the
    /// component exports an incompatible future version the host does not
    /// recognise), instantiation fails with the missing-export diagnostic naming
    /// the versioned interface — the probe must not swallow a genuinely absent
    /// interface.
    #[test]
    fn hook_missing_lifecycle_export_fails() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component =
            hook_double_iface(&engine, "murmur:hook/lifecycle@9.9.9", &REQUIRED_HOOK_FNS);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let msg = match new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double(component)],
            )
            .await
            {
                Ok(_) => panic!("a component exporting neither probed lifecycle name must fail"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains(LIFECYCLE_IFACE),
                "error must name the missing lifecycle export, got: {msg}"
            );
        });
    }

    /// A `murmur:hook/lifecycle` double whose every export is one core function that never
    /// returns, so an epoch deadline is the only thing that can end a call to it.
    ///
    /// Unlike [`hook_double`] — whose stubs exist only to be *resolved* and would fail
    /// `TypedFunc`'s signature check if called — this one declares the real
    /// `func(session-context) -> result<hook-output, string>` type, which is what lets
    /// `call_typed` get as far as `call_async`. Only `on-session-start` is ever dispatched
    /// against it here; the other five names reuse the same function purely to satisfy
    /// `resolve_hook_fns`, exactly as `hook_double` does.
    ///
    /// The core function is lifted with 10 flat i32 params (four strings plus one
    /// `list<string>`, two i32 each) and returns an i32 pointer to the result area, since
    /// `result<hook-output, string>` exceeds one flat result. It spins before touching any
    /// of that, so the return area is never written.
    ///
    /// The exported instance must re-export every *named* type its signature references
    /// (`message`, `tool-manifest`, `hook-output`, `session-context`) — a component-model
    /// validity rule, not decoration. Omitting them fails `Component::new` with the
    /// distinctly unhelpful "instance not valid to be used as export". Structural types
    /// like `string` need no such export, which is why [`hook_double`] gets away without
    /// any of this.
    fn hook_spin_double(engine: &wasmtime::Engine) -> Component {
        hook_spin_double_iface(engine, LIFECYCLE_IFACE)
    }

    /// [`hook_spin_double`] under an explicit lifecycle instance name, so the
    /// same real-signature `on-session-start` can be presented as either
    /// lifecycle version.
    fn hook_spin_double_iface(engine: &wasmtime::Engine, iface: &str) -> Component {
        let exports = REQUIRED_HOOK_FNS
            .iter()
            .map(|n| format!("    (export \"{n}\" (func $f))"))
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 8)
    (func (export "spin")
      (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)
      (loop $l (br $l))
      unreachable)
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $session-context (record
    (field "capsule-name" string)
    (field "capsule-version" string)
    (field "session-id" string)
    (field "model" string)
    (field "capabilities" (list string))))
  (type $ft (func (param "ctx" $session-context) (result (result $hook-output (error string)))))

  (func $f (type $ft)
    (canon lift (core func $i "spin")
      (memory $mem) (realloc $realloc) string-encoding=utf8))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "session-context" (type $session-context))
{exports}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("spin component WAT parses");
        Component::new(engine, &bytes).expect("spin component double compiles")
    }

    /// Wrap a hand-written core module in the current lifecycle component shell.
    ///
    /// `core_body` must export `memory`, `realloc`, `oncompact` (lifted as
    /// `on-compaction`) and `noop` (every other required export). Splitting this out
    /// keeps the compaction doubles below down to the wasm that actually differs
    /// between them; the surrounding type/instance section is a component-model
    /// validity requirement, not test-specific detail (see
    /// [`hook_spin_double_iface`]).
    ///
    /// `oncompact`'s flat signature is fixed by the canonical ABI lowering of the
    /// 5-field `compaction-event`: `(list<message> → i32 i32) (u64) (f64)
    /// (option<string> → i32 i32 i32) (option<string> → i32 i32 i32)`.
    fn compaction_component_from_core(engine: &wasmtime::Engine, core_body: &str) -> Component {
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .filter(|n| **n != "on-compaction")
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let iface = LIFECYCLE_IFACE;
        let wat = format!(
            r#"(component
  (core module $m
{core_body}
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $compaction-event (record
    (field "messages" (list $message))
    (field "session-tokens" u64)
    (field "threshold" f64)
    (field "model" (option string))
    (field "system-prompt" (option string))))
  (type $ft (func (param "event" $compaction-event) (result (result $hook-output (error string)))))

  (func $oc (type $ft)
    (canon lift (core func $i "oncompact") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "compaction-event" (type $compaction-event))
    (export "on-compaction" (func $oc))
{stubs}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("compaction component WAT parses");
        Component::new(engine, &bytes).expect("compaction component double compiles")
    }

    /// Sentinel content an echo double reports when the option field it was asked to
    /// echo arrived as `none`. Deliberately not a plausible model name or prompt, so
    /// "the host sent none" can never be mistaken for "the host sent the string
    /// `none`".
    const MODEL_ABSENT_SENTINEL: &str = "<<absent>>";

    /// A compaction double that reports back what it saw in `compaction-event.model`:
    /// it returns `ok(replace-context([{role: "model", content: <the model string>}]))`,
    /// substituting [`MODEL_ABSENT_SENTINEL`] when the option arrived as `none`.
    fn hook_compaction_echo_model_double(engine: &wasmtime::Engine) -> Component {
        hook_compaction_echo_double(engine, "model", "model")
    }

    /// The `system-prompt` twin of [`hook_compaction_echo_model_double`] — echoes
    /// `compaction-event.system-prompt` back as the returned message's content, under
    /// role `"system-prompt"`.
    fn hook_compaction_echo_system_prompt_double(engine: &wasmtime::Engine) -> Component {
        hook_compaction_echo_double(engine, "sp", "system-prompt")
    }

    /// Shared body of the two echo doubles above: echo the `option<string>` carried by
    /// the `$<field>-some/-ptr/-len` flattened params back out as the single returned
    /// message's content, tagged with `role`.
    ///
    /// Echoing the *received* pointer/length straight back into the returned message is
    /// what makes this a real assertion on the wire value rather than on host-side Rust
    /// state — the string the test reads is the one the guest was actually handed.
    ///
    /// Return area is laid out by hand at offset 128:
    /// `result` discriminant `0` (ok); `hook-output` discriminant `1`
    /// (`replace-context`) at 132 with its `list<message>` (ptr 160, len 1) at 136/140;
    /// the one `message` at 160 as `{role ptr/len, content ptr/len}`. The role string
    /// lives at 200 and the absent-sentinel at 224.
    /// Unlike the other doubles this one needs a genuine bump `realloc` — a fixed
    /// address would lower the messages list, the model string and the system-prompt
    /// string all on top of each other, and the echoed bytes would be garbage.
    fn hook_compaction_echo_double(
        engine: &wasmtime::Engine,
        field: &str,
        role: &str,
    ) -> Component {
        let sentinel_len = MODEL_ABSENT_SENTINEL.len();
        let role_len = role.len();
        let core = format!(
            r#"    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 1024))
    (data (i32.const 200) "{role}")
    (data (i32.const 224) "{MODEL_ABSENT_SENTINEL}")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $r i32)
      (global.set $bump
        (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (local.set $r (global.get $bump))
      (global.set $bump (i32.add (global.get $bump) (local.get 3)))
      (local.get $r))
    (func (export "oncompact")
      (param $msgs-ptr i32) (param $msgs-len i32)
      (param $tokens i64) (param $threshold f64)
      (param $model-some i32) (param $model-ptr i32) (param $model-len i32)
      (param $sp-some i32) (param $sp-ptr i32) (param $sp-len i32)
      (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 1))
      (i32.store (i32.const 136) (i32.const 160))
      (i32.store (i32.const 140) (i32.const 1))
      (i32.store (i32.const 160) (i32.const 200))
      (i32.store (i32.const 164) (i32.const {role_len}))
      (i32.store (i32.const 168)
        (select (local.get ${field}-ptr) (i32.const 224) (local.get ${field}-some)))
      (i32.store (i32.const 172)
        (select (local.get ${field}-len) (i32.const {sentinel_len}) (local.get ${field}-some)))
      (i32.const 128))
    (func (export "noop"))"#
        );
        compaction_component_from_core(engine, &core)
    }

    /// A compaction double whose `on-compaction` unconditionally returns
    /// `err("boom")` — the "declared compaction hook, no safety net behind it" case.
    /// Return area at 128: `result` discriminant `1` (err), payload string
    /// (ptr 300, len 4) at 132/136.
    fn hook_compaction_err_double(engine: &wasmtime::Engine) -> Component {
        let core = r#"    (memory (export "memory") 1)
    (data (i32.const 300) "boom")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "oncompact")
      (param i32 i32 i64 f64 i32 i32 i32 i32 i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 1))
      (i32.store (i32.const 132) (i32.const 300))
      (i32.store (i32.const 136) (i32.const 4))
      (i32.const 128))
    (func (export "noop"))"#;
        compaction_component_from_core(engine, core)
    }

    /// Drive one `dispatch_compaction` against a single named double and hand the
    /// caller the raw result to assert on.
    async fn dispatch_compaction_with(
        session: &Path,
        accessible: &Path,
        engine: &wasmtime::Engine,
        double: Component,
        model: Option<String>,
        system_prompt: Option<String>,
    ) -> Result<Option<Vec<Message>>, String> {
        let mut hooks = new_with_hooks(
            engine,
            session,
            accessible,
            vec![staged_double_named(
                "compactor",
                HookBinding::OnCompaction,
                double,
            )],
        )
        .await
        .expect("double instantiates");
        hooks
            .dispatch_compaction(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                1234,
                0.98,
                model,
                system_prompt,
            )
            .await
    }

    /// Scenario 1: `inference.compaction.model` set in the manifest reaches the hook
    /// verbatim as `compaction-event.model`.
    #[test]
    fn compaction_hook_receives_manifest_model() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(dispatch_compaction_with(
            session.path(),
            accessible.path(),
            &engine,
            hook_compaction_echo_model_double(&engine),
            Some("claude-haiku-4-5".to_string()),
            None,
        ));

        let messages = result.expect("hook succeeded").expect("replace-context");
        assert_eq!(messages[0].role, "model");
        assert_eq!(messages[0].content, "claude-haiku-4-5");
    }

    /// `inference.compaction.system_prompt` set in the manifest reaches the hook
    /// verbatim as `compaction-event.system-prompt` — and does so independently of
    /// `model`, which is left unset here.
    #[test]
    fn compaction_hook_receives_manifest_system_prompt() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let prompt = "task = X, currently editing Y, already tried Z.";
        let result = rt.block_on(dispatch_compaction_with(
            session.path(),
            accessible.path(),
            &engine,
            hook_compaction_echo_system_prompt_double(&engine),
            None,
            Some(prompt.to_string()),
        ));

        let messages = result.expect("hook succeeded").expect("replace-context");
        assert_eq!(messages[0].role, "system-prompt");
        assert_eq!(messages[0].content, prompt);
    }

    /// No `system_prompt:` in the manifest arrives as `option::none` — nothing on this
    /// path substitutes a default prompt. `model` is set here to prove the two fields
    /// resolve independently.
    #[test]
    fn compaction_hook_receives_none_when_system_prompt_unset() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(dispatch_compaction_with(
            session.path(),
            accessible.path(),
            &engine,
            hook_compaction_echo_system_prompt_double(&engine),
            Some("claude-haiku-4-5".to_string()),
            None,
        ));

        let messages = result.expect("hook succeeded").expect("replace-context");
        assert_eq!(messages[0].content, MODEL_ABSENT_SENTINEL);
    }

    /// Scenario 2: no `model:` in the manifest arrives as `option::none` — nothing on
    /// this path substitutes the primary `inference.model` first.
    #[test]
    fn compaction_hook_receives_none_when_model_unset() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(dispatch_compaction_with(
            session.path(),
            accessible.path(),
            &engine,
            hook_compaction_echo_model_double(&engine),
            None,
            None,
        ));

        let messages = result.expect("hook succeeded").expect("replace-context");
        assert_eq!(messages[0].content, MODEL_ABSENT_SENTINEL);
    }

    /// Scenario 4: a bound compaction hook that returns `Err` surfaces as `Err` to the
    /// caller — distinct from the `Ok(None)` that "no hook was bound" produces — while
    /// still logging to the hook's own log exactly as before.
    #[test]
    fn compaction_hook_error_surfaces_to_caller() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(dispatch_compaction_with(
            session.path(),
            accessible.path(),
            &engine,
            hook_compaction_err_double(&engine),
            None,
            None,
        ));

        let error = result.expect_err("a failing compaction hook must not read as Ok(None)");
        assert!(
            error.contains("compactor") && error.contains("boom"),
            "error must name the hook and carry its message, got {error:?}"
        );
        assert_eq!(
            hook_log_lines(session.path(), "compactor"),
            1,
            "per-hook error logging is unchanged"
        );
    }

    /// Scenario 3: no compaction-bound hook at all stays `Ok(None)` — the caller
    /// continues without compaction rather than failing the session.
    #[test]
    fn compaction_with_no_bound_hook_is_ok_none() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(async {
            let mut hooks = new_with_hooks(&engine, session.path(), accessible.path(), vec![])
                .await
                .expect("empty hook set");
            hooks
                .dispatch_compaction(Vec::new(), 1234, 0.98, None, None)
                .await
        });

        assert!(matches!(result, Ok(None)));
    }

    async fn dispatch_compaction_against(
        session: &Path,
        accessible: &Path,
        engine: &wasmtime::Engine,
    ) {
        let core = r#"    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "oncompact") (param i32 i32 i64 f64 i32 i32 i32 i32 i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 0))
      (i32.const 128))
    (func (export "noop"))"#;
        let mut hooks = new_with_hooks(
            engine,
            session,
            accessible,
            vec![staged_double_named(
                "compactor",
                HookBinding::OnCompaction,
                compaction_component_from_core(engine, core),
            )],
        )
        .await
        .expect("a current-version hook must instantiate");

        let replacement = hooks
            .dispatch_compaction(
                vec![Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                }],
                1234,
                0.98,
                None,
                None,
            )
            .await;
        assert!(
            matches!(replacement, Ok(None)),
            "the double returns hook-output::none, so no context replacement and no error"
        );
    }

    /// A hook built against the current lifecycle version receives `on-compaction`
    /// with the full 5-field record. Nothing is logged, so the dispatch really
    /// completed rather than failing `.typed()` and being isolated.
    #[test]
    fn current_hook_receives_five_field_compaction_event() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(dispatch_compaction_against(
            session.path(),
            accessible.path(),
            &engine,
        ));

        assert_eq!(
            hook_log_lines(session.path(), "compactor"),
            0,
            "on-compaction must be received cleanly, not as an ABI mismatch"
        );
    }

    /// The host keeps no compatibility fallback. A hook still compiled against a
    /// retired lifecycle version does not resolve at all: it fails at instantiation
    /// with the missing-export diagnostic, naming the one accepted interface and
    /// pointing the author at a rebuild, rather than being quietly accepted and
    /// then mis-dispatched.
    #[test]
    fn retired_version_hook_fails_to_instantiate() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let msg = match new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "legacy",
                    HookBinding::All,
                    hook_spin_double_iface(&engine, RETIRED_IFACE),
                )],
            )
            .await
            {
                Ok(_) => panic!("a hook exporting a retired lifecycle version must not resolve"),
                Err(e) => e.to_string(),
            };
            assert!(
                msg.contains(LIFECYCLE_IFACE),
                "error must name the accepted lifecycle export, got: {msg}"
            );
            assert!(
                msg.contains("rebuild"),
                "error must hint at rebuilding the hook, got: {msg}"
            );
        });
    }

    // ---- shell-event: `binary` (the `@0.4.0 → @0.5.0` bump) ----

    /// A double whose `on-shell` declares the **current** 9-field `shell-event`, and which
    /// verifies what it was handed rather than merely accepting it: the core function
    /// byte-compares the incoming `binary` string against `expect_binary` and traps on any
    /// difference. So "zero lines in the hook log" here means the guest received exactly
    /// the value the host claims it sent — a weaker double could pass while `binary`
    /// carried garbage.
    ///
    /// `on-shell`'s flat params are fixed by the canonical ABI lowering of the 9-field
    /// record: `u32` + three `string`s (2 each, in field order `binary`, `command`,
    /// `stdout`, `stderr` — four strings, 8 i32) + `s32` + three `u64`. Unlike the
    /// constant-pointer `realloc` the other doubles use, this one bump-allocates: four
    /// strings lowered to the same address would clobber each other, and `binary` is
    /// lowered first.
    ///
    /// The expected bytes live at offset 0 and the `result<hook-output, string>` return
    /// area at 128, so `expect_binary` must stay well under 128 bytes.
    fn hook_shell_double(engine: &wasmtime::Engine, expect_binary: &str) -> Component {
        assert!(
            expect_binary.len() < 128,
            "expected-binary data segment would overlap the return area at 128"
        );
        let expect_len = expect_binary.len();
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .filter(|n| **n != "on-shell")
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let iface = LIFECYCLE_IFACE;
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (data (i32.const 0) "{expect_binary}")
    (global $next (mut i32) (i32.const 1024))
    (func (export "realloc") (param i32 i32 i32) (param $size i32) (result i32)
      (local $at i32)
      (local.set $at (global.get $next))
      (global.set $next (i32.add (local.get $at) (local.get $size)))
      (local.get $at))
    (func (export "onshell")
      (param $turn i32) (param $binp i32) (param $binlen i32)
      (param i32) (param i32) (param i32) (param i32) (param i32) (param i32) (param i32)
      (param i64) (param i64) (param i64) (result i32)
      (local $i i32)
      (if (i32.ne (local.get $binlen) (i32.const {expect_len})) (then unreachable))
      (block $done
        (loop $l
          (br_if $done (i32.ge_u (local.get $i) (i32.const {expect_len})))
          (if (i32.ne
                (i32.load8_u (i32.add (local.get $binp) (local.get $i)))
                (i32.load8_u (local.get $i)))
            (then unreachable))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $l)))
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 0))
      (i32.const 128))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $shell-event (record
    (field "turn" u32)
    (field "binary" string)
    (field "command" string)
    (field "exit-code" s32)
    (field "stdout" string)
    (field "stderr" string)
    (field "stdout-bytes" u64)
    (field "stderr-bytes" u64)
    (field "duration-ms" u64)))
  (type $ft (func (param "event" $shell-event) (result (result $hook-output (error string)))))

  (func $sh (type $ft)
    (canon lift (core func $i "onshell") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "shell-event" (type $shell-event))
    (export "on-shell" (func $sh))
{stubs}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("@0.5.0 shell component WAT parses");
        Component::new(engine, &bytes).expect("@0.5.0 shell component double compiles")
    }

    /// Stage `component` as the sole `on-shell` hook and dispatch one shell event
    /// carrying `binary`.
    async fn dispatch_shell_against(
        session: &Path,
        accessible: &Path,
        engine: &wasmtime::Engine,
        component: Component,
        binary: &str,
    ) {
        let mut hooks = new_with_hooks(
            engine,
            session,
            accessible,
            vec![staged_double_named(
                "sheller",
                HookBinding::OnShell,
                component,
            )],
        )
        .await
        .expect("a current-version hook must instantiate");

        hooks
            .emit(
                session,
                HookEvent::Shell {
                    turn: 1,
                    binary: binary.to_string(),
                    command: "-q tests/".to_string(),
                    exit_code: 0,
                    stdout: "ok".to_string(),
                    stderr: String::new(),
                    stdout_bytes: 2,
                    stderr_bytes: 0,
                    duration_ms: 12,
                },
            )
            .await;
    }

    /// A hook rebuilt against `@0.5.0` receives the new 9-field `shell-event`, and the
    /// `binary` it observes is byte-for-byte what the host sent — the whole point of the
    /// bump. The double traps on any mismatch, so an empty log is the assertion.
    #[test]
    fn current_hook_receives_the_invoked_binary_in_shell_event() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(dispatch_shell_against(
            session.path(),
            accessible.path(),
            &engine,
            hook_shell_double(&engine, "/usr/bin/pytest"),
            "/usr/bin/pytest",
        ));

        assert_eq!(
            hook_log_lines(session.path(), "sheller"),
            0,
            "a @0.5.0 hook must receive on-shell cleanly with the exact binary the host sent"
        );
    }

    /// Guards the test above: the double really does inspect `binary`, so a wrong value
    /// traps rather than passing silently. Without this, `v0_5_hook_receives_...` would
    /// still pass if the host sent an empty or stale `binary`.
    #[test]
    fn shell_double_rejects_a_binary_it_did_not_expect() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(dispatch_shell_against(
            session.path(),
            accessible.path(),
            &engine,
            hook_shell_double(&engine, "/usr/bin/pytest"),
            "/usr/bin/cargo",
        ));

        let log = std::fs::read_to_string(session.path().join("logs").join("hook-sheller.log"))
            .expect("the mismatch must be logged");
        assert!(
            log.contains("on-shell trapped"),
            "the double must trap *inside the guest* on an unexpected binary — a `.typed()` \
             mismatch (which reports differently) would prove nothing about the field's \
             value; got: {log}"
        );
    }

    /// End-to-end through the real host import: a hook component that imports
    /// `murmur:runtime/inference@0.2.0` and calls `run-inference` gets back the
    /// driver double's completion text, which it returns as an `on-inference`
    /// artifact so the test can read it.
    #[test]
    fn hook_importing_run_inference_receives_the_driver_completion() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let driver = crate::inference_import::test_support::driver_double(
            &engine,
            0,
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"hello from driver"}]}"#,
        );
        let ctx = Arc::new(HookInferenceCtx {
            driver_name: "mock-driver".to_string(),
            driver_component: driver,
            model: "manifest-model".to_string(),
            engine: engine.clone(),
            accessible_workdir: accessible.path().to_path_buf(),
            inference_env: Vec::new(),
            capability_policy: crate::types::CapabilityPolicy::default(),
            network_allow_rules: Vec::new(),
            driver_grant: None,
            records: std::sync::Mutex::new(Vec::new()),
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let artifacts = rt.block_on(async {
            let mut hooks = HookRuntime::new(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "caller",
                    HookBinding::OnInference,
                    hook_inference_caller_double(&engine),
                )],
                SessionContextData {
                    capsule_name: "test-capsule".to_string(),
                    capsule_version: "0.1.0".to_string(),
                    session_id: "sess-test".to_string(),
                    model: "manifest-model".to_string(),
                    capabilities: Vec::new(),
                },
                HookEnvVars::default(),
                ExecutionLimits::default(),
                Some(Arc::clone(&ctx)),
            )
            .await
            .expect("a hook importing murmur:runtime/inference must link");
            hooks.emit(session.path(), inference_event()).await
        });

        assert_eq!(artifacts.len(), 1, "the hook returns run-inference's text");
        assert_eq!(artifacts[0].payload, "hello from driver");
        let records = ctx.drain_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].origin.source, "hook:caller");
        assert_eq!(records[0].origin.model, "manifest-model");
    }

    /// Same component, but the capsule has no inference driver staged. The
    /// import still links (so the hook runs at all) and `run-inference` returns
    /// a clear `err` naming the manifest key to add — never a panic or an
    /// instantiation failure.
    #[test]
    fn hook_run_inference_without_driver_gets_clear_error() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let artifacts = rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "caller",
                    HookBinding::OnInference,
                    hook_inference_caller_double(&engine),
                )],
            )
            .await
            .expect("the import is defined even with no driver, so the hook links");
            hooks.emit(session.path(), inference_event()).await
        });

        assert_eq!(artifacts.len(), 1);
        assert!(
            artifacts[0].payload.contains("inference driver is not configured")
                && artifacts[0].payload.contains("inference.driver.artifact"),
            "got: {}",
            artifacts[0].payload
        );
    }

    fn inference_event() -> HookEvent {
        HookEvent::Inference {
            turn: 0,
            input_tokens: 0,
            output_tokens: 0,
            decision: "end_turn".to_string(),
            tool_name: None,
            prompt: None,
            output: None,
            tools: None,
        }
    }

    /// A `@0.3.0` lifecycle double that *imports* `murmur:runtime/inference@0.2.0`
    /// and, on `on-inference`, calls `run-inference` with an empty message list
    /// and `model: none`, then returns whichever string the call produced —
    /// the completion text on success, the error message on failure — as
    /// `hook-output::artifact`. `on-inference` is the one event whose artifact
    /// `dispatch` forwards, which is what makes the result observable.
    ///
    /// Memory and `realloc` live in a separate core module so the lowered import
    /// can reference them without a cyclic instantiation. `result<inference-response,
    /// string>` lays `ok`/`err` payloads at the same offset (record align 8 →
    /// discriminant at 0, payload at 8), so one code path copies either string's
    /// `(ptr, len)` into the artifact.
    fn hook_inference_caller_double(engine: &wasmtime::Engine) -> Component {
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .filter(|n| **n != "on-inference")
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!(
            r#"(component
  (import "{INFERENCE_IFACE_VERSIONED}" (instance $inf
    (type (record (field "role" string) (field "content" string)))
    (export "message" (type (eq 0)))
    (type (list 1))
    (type (option string))
    (type (record
      (field "messages" 2)
      (field "system-prompt" 3)
      (field "model" 3)))
    (export "inference-request" (type (eq 4)))
    (type (record
      (field "text" string)
      (field "model-used" string)
      (field "input-tokens" u64)
      (field "output-tokens" u64)))
    (export "inference-response" (type (eq 6)))
    (type (result 7 (error string)))
    (export "run-inference" (func (param "request" 5) (result 8)))
  ))
  (alias export $inf "run-inference" (func $runi))

  (core module $libc
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 1024))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $p i32)
      (local.set $p (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (global.set $bump (i32.add (local.get $p) (i32.add (local.get 3) (i32.const 8))))
      (local.get $p))
  )
  (core instance $li (instantiate $libc))
  (alias core export $li "memory" (core memory $mem))
  (alias core export $li "realloc" (core func $realloc))
  (core func $run_lowered
    (canon lower (func $runi) (memory $mem) (realloc $realloc) string-encoding=utf8))

  (core module $m
    (import "libc" "memory" (memory 1))
    (import "inf" "run" (func $run (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))
    (func (export "oninf") (param i32) (result i32)
      (call $run
        (i32.const 0) (i32.const 0)
        (i32.const 0) (i32.const 0) (i32.const 0)
        (i32.const 0) (i32.const 0) (i32.const 0)
        (i32.const 256))
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const 3))
      (i32.store (i32.const 136) (i32.load (i32.const 264)))
      (i32.store (i32.const 140) (i32.load (i32.const 268)))
      (i32.const 128))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m
    (with "libc" (instance $li))
    (with "inf" (instance (export "run" (func $run_lowered))))))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $inference-event (record
    (field "turn" u32)
    (field "input-tokens" u64)
    (field "output-tokens" u64)
    (field "decision" string)
    (field "tool-name" (option string))
    (field "prompt" (option string))
    (field "output" (option string))
    (field "tools" (option string))))
  (type $ft (func (param "event" $inference-event) (result (result $hook-output (error string)))))

  (func $oninf (type $ft)
    (canon lift (core func $i "oninf") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "inference-event" (type $inference-event))
    (export "on-inference" (func $oninf))
{stubs}
  )
  (export "{LIFECYCLE_IFACE}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("inference-caller component WAT parses");
        Component::new(engine, &bytes).expect("inference-caller component double compiles")
    }

    /// A hook that never returns from `on-session-start` is cut off at its epoch deadline
    /// and flows through the *existing* per-hook isolation: the failure is logged to
    /// `logs/hook-<name>.log` by `log_hook_error` and `dispatch` carries on to the next
    /// hook rather than aborting the session. This is the hook half of the slice's
    /// "before: unkillable, after: bounded" proof.
    #[test]
    fn blocking_hook_that_spins_is_interrupted_and_does_not_abort_dispatch() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // Without a running ticker the epoch never advances and the deadline cannot fire.
        let _ticker = crate::limits::EpochTicker::spawn(&engine);

        let limits = ExecutionLimits {
            deadline_seconds: 1,
            ..ExecutionLimits::default()
        };

        // The spinner is registered first, so a second hook receiving the same event at all
        // proves dispatch resumed after the trap instead of unwinding.
        let staged = vec![
            staged_double_named("spinner", HookBinding::All, hook_spin_double(&engine)),
            staged_double_named(
                "bystander",
                HookBinding::All,
                hook_double(&engine, &REQUIRED_HOOK_FNS),
            ),
        ];

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks =
                new_with_hooks_limited(&engine, session.path(), accessible.path(), staged, limits)
                    .await
                    .expect("both doubles export every required fn, so both instantiate");

            // Returns at all == the spinning guest was interrupted. Before this slice the
            // await below would never complete.
            hooks.emit(session.path(), HookEvent::SessionStart).await;
        });

        let spinner_log =
            std::fs::read_to_string(session.path().join("logs").join("hook-spinner.log"))
                .expect("the interrupted hook must have logged via log_hook_error");
        assert!(
            spinner_log.contains("exceeded its 1s execution deadline"),
            "the log must name the deadline that fired rather than read as a generic \
             trap; got: {spinner_log}"
        );

        // The bystander's own call fails TypedFunc's signature check (its stub is a bare
        // `func() -> ()`), which is itself an ordinary hook error — the point is only that
        // dispatch reached it at all after the spinner trapped.
        assert!(
            session
                .path()
                .join("logs")
                .join("hook-bystander.log")
                .exists(),
            "dispatch must continue to the next hook after a deadline trap"
        );
    }

    // ── Per-hook capability grants (default-deny network + filesystem) ────────

    use crate::network_policy::RequestTarget;
    use murmur_artifact::{Capabilities, FilesystemCapabilities, NetworkCapabilities};

    /// Build the grant a hook entry declaring exactly these two sub-blocks would get,
    /// going through the same `HookCapabilityGrant::derive` the staging path uses.
    fn grant_of(network: Option<&str>, scope: Option<&str>) -> HookCapabilityGrant {
        let caps = Capabilities {
            network: network.map(|entry| NetworkCapabilities {
                allow: vec![entry.to_string()],
                unix_sockets: false,
            }),
            filesystem: scope.map(|scope| FilesystemCapabilities {
                scope: Some(scope.to_string()),
            }),
            shell: None,
            spawn: None,
            env: None,
            limits: None,
            containment: None,
        };
        HookCapabilityGrant::derive(Some(&caps)).expect("grant is valid")
    }

    /// A hook store built exactly as the three instantiation sites build one, so the
    /// network suite exercises the real `WasiHttpView` wiring rather than a stand-in.
    fn hook_store_state(root: &Path, grant: &HookCapabilityGrant) -> HookStoreState {
        HookStoreState {
            limits: ExecutionLimits::default().limiter(),
            table: ResourceTable::new(),
            wasi: build_wasi_ctx(root, &HookEnvVars::default(), grant).expect("wasi ctx builds"),
            http: WasiHttpCtx::new(),
            http_hooks: NetworkPolicyHooks {
                network_allow_rules: grant.network_allow_rules.clone(),
            },
        }
    }

    /// Ask a hook store's own HTTP gate to send a request, the way
    /// `wasi:http/outgoing-handler` does. `Err` is a policy denial; `Ok` means the policy
    /// admitted the request (the returned future is dropped without being driven, so no
    /// connection is ever completed).
    fn send_through_hook_store(state: &mut HookStoreState, uri: &str, use_tls: bool) -> bool {
        use http_body_util::{BodyExt, Empty};

        let body = Empty::<bytes::Bytes>::new()
            .map_err(|err| match err {})
            .boxed_unsync();
        let request = hyper::Request::builder()
            .uri(uri)
            .body(body)
            .expect("request builds");
        let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
            use_tls,
            connect_timeout: Duration::from_millis(1),
            first_byte_timeout: Duration::from_millis(1),
            between_bytes_timeout: Duration::from_millis(1),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { state.http().hooks.send_request(request, config).is_ok() })
    }

    /// Default-deny, network half: a hook whose operator entry declared no `capabilities:`
    /// has an empty allow-list, and its store's own HTTP gate denies every request.
    #[test]
    fn ungranted_hook_store_denies_every_outbound_request() {
        let root = TempDir::new().unwrap();
        let grant = HookCapabilityGrant::default();
        let mut state = hook_store_state(root.path(), &grant);

        assert!(!send_through_hook_store(
            &mut state,
            "https://telemetry.example.com/ingest",
            true
        ));
        assert!(!send_through_hook_store(
            &mut state,
            "http://127.0.0.1:1/local",
            false
        ));
    }

    /// Granted network: exactly the declared host is admitted; every other host is denied
    /// by the same `NetworkPolicyHooks` capsules and tools use.
    #[test]
    fn granted_hook_store_admits_only_the_declared_host() {
        let root = TempDir::new().unwrap();
        // A loopback port nothing listens on: the policy decision is observable without
        // the test ever completing a connection.
        let grant = grant_of(Some("http://127.0.0.1:1"), None);
        let mut state = hook_store_state(root.path(), &grant);

        assert!(
            send_through_hook_store(&mut state, "http://127.0.0.1:1/ingest", false),
            "the granted host must pass the allow-list"
        );
        assert!(
            !send_through_hook_store(&mut state, "http://127.0.0.1:2/ingest", false),
            "a different port on the granted host must still be denied"
        );
        assert!(
            !send_through_hook_store(&mut state, "https://evil.example.com/x", true),
            "an undeclared host must be denied"
        );
    }

    /// Default-deny, filesystem half: nothing is preopened. Observable because
    /// `preopened_dir` requires the directory to exist — building a context rooted at a
    /// path that does not exist can only succeed if no preopen was attempted at all.
    #[test]
    fn ungranted_hook_gets_no_preopened_directory() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("does-not-exist");

        build_wasi_ctx(
            &missing,
            &HookEnvVars::default(),
            &HookCapabilityGrant::default(),
        )
        .expect("an ungranted hook preopens nothing, so a missing root is not an error");

        assert!(
            !missing.exists(),
            "default-deny must not create the working directory either"
        );
    }

    /// Granted filesystem: exactly one directory — `<root>/<scope>` — is preopened, and it
    /// is created if absent. Sibling paths under the same root are never preopened, so a
    /// guest has no descriptor with which to name them.
    #[test]
    fn granted_hook_preopens_only_the_scoped_subtree() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("secret.txt"), b"capsule state").unwrap();
        let sibling = root.path().join("other-artifact");
        std::fs::create_dir_all(&sibling).unwrap();

        let grant = grant_of(None, Some("hook-state"));
        build_wasi_ctx(root.path(), &HookEnvVars::default(), &grant)
            .expect("a granted scope is created and preopened");

        let scoped = root.path().join("hook-state");
        assert!(scoped.is_dir(), "the granted scope is created if missing");
        // Writes inside the scope land on the real host filesystem at that path.
        std::fs::write(scoped.join("cursor.json"), b"{}").unwrap();
        assert!(scoped.join("cursor.json").exists());
        // Nothing else under the root was touched or mounted.
        assert!(sibling.is_dir());
        assert!(root.path().join("secret.txt").exists());
    }

    /// An existing scope directory is reused as-is: granting a scope must not clobber
    /// state a previous run left there.
    #[test]
    fn granted_hook_scope_reuses_an_existing_directory() {
        let root = TempDir::new().unwrap();
        let scoped = root.path().join("hook-state");
        std::fs::create_dir_all(&scoped).unwrap();
        std::fs::write(scoped.join("cursor.json"), b"{\"seen\":7}").unwrap();

        build_wasi_ctx(
            root.path(),
            &HookEnvVars::default(),
            &grant_of(None, Some("hook-state")),
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(scoped.join("cursor.json")).unwrap(),
            "{\"seen\":7}"
        );
    }

    /// A scope that cannot be created fails instantiation rather than silently degrading
    /// to "no filesystem access" — an operator who declared a scope must not have it
    /// quietly dropped.
    #[test]
    fn unusable_scope_fails_instantiation() {
        let root = TempDir::new().unwrap();
        // A regular file where the scope directory would go: create_dir_all cannot succeed.
        std::fs::write(root.path().join("hook-state"), b"not a directory").unwrap();

        let err = match build_wasi_ctx(
            root.path(),
            &HookEnvVars::default(),
            &grant_of(None, Some("hook-state")),
        ) {
            Ok(_) => panic!("an uncreatable scope must be a hard error"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("hook-state"),
            "the error must name the scope: {err}"
        );
    }

    /// No exempt instantiation path — blocking hooks. `HookRuntime::new` applies the grant
    /// to the *project* directory, and only the granted subtree there.
    #[test]
    fn blocking_hook_instantiation_applies_the_grant() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_double(&engine, &REQUIRED_HOOK_FNS);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_granted(
                    component,
                    grant_of(None, Some("hook-state")),
                )],
            )
            .await
            .expect("a granted blocking hook instantiates");
        });

        assert!(
            accessible.path().join("hook-state").is_dir(),
            "the blocking path preopens the scope under the project dir"
        );
        assert!(
            !session.path().join("hook-state").exists(),
            "the session dir is not the blocking hook's root and must stay untouched"
        );
    }

    /// No exempt instantiation path — blocking hooks, default-deny. Instantiation succeeds
    /// against a project directory that does not exist, which is only possible because no
    /// preopen is attempted.
    #[test]
    fn ungranted_blocking_hook_instantiates_without_any_preopen() {
        let session = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_double(&engine, &REQUIRED_HOOK_FNS);
        let missing_project_dir = session.path().join("no-such-project");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            new_with_hooks(
                &engine,
                session.path(),
                &missing_project_dir,
                vec![staged_double(component)],
            )
            .await
            .expect("an ungranted blocking hook preopens nothing");
        });

        assert!(!missing_project_dir.exists());
    }

    /// No exempt instantiation path — `on-stage`. `call_stage_once` builds its context from
    /// the same grant; the dispatch itself fails on the stub's signature, which is after
    /// the WASI context has already been built.
    #[test]
    fn on_stage_instantiation_applies_the_grant() {
        let workdir = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let mut names: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        names.push("on-stage");
        let staged = staged_double_granted(
            hook_double(&engine, &names),
            grant_of(None, Some("hook-state")),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = call_stage_once(
                &engine,
                workdir.path(),
                &staged,
                &StageEvent {
                    shell_allow: Vec::new(),
                },
                &HookEnvVars::default(),
                ExecutionLimits::default(),
            )
            .await;
        });

        assert!(
            workdir.path().join("hook-state").is_dir(),
            "the on-stage path preopens the scope under the session workdir"
        );
    }

    /// No exempt instantiation path — async hooks. Each per-event instantiation applies the
    /// grant carried on its `AsyncHookSpec`.
    #[test]
    fn async_hook_instantiation_applies_the_grant() {
        let root = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_double(&engine, &REQUIRED_HOOK_FNS);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let _ = call_async_hook(
                &engine,
                root.path(),
                &component,
                "async-hook",
                &SessionContextData {
                    capsule_name: "test-capsule".to_string(),
                    capsule_version: "0.1.0".to_string(),
                    session_id: "sess-test".to_string(),
                    model: "test-model".to_string(),
                    capabilities: Vec::new(),
                },
                &HookEvent::SessionStart,
                Duration::from_secs(0),
                HookTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    tool_calls: 0,
                    shell_calls: 0,
                },
                ExecutionLimits::default(),
                None,
                &grant_of(None, Some("hook-state")),
            )
            .await;
        });

        assert!(
            root.path().join("hook-state").is_dir(),
            "the async path preopens the scope like the other two"
        );
    }

    /// No self-escalation: the grant is derived from the *operator's* manifest entry, so a
    /// hook artifact whose own bundled murmur.yaml declares broad capabilities is still
    /// fully denied when the operator granted it nothing.
    #[test]
    fn hook_self_declared_capabilities_do_not_grant_anything() {
        // The hook artifact's own bundled manifest, self-declaring broad access.
        let hook_own_manifest = r#"
name: greedy-hook
version: 1.0.0
binding: on-inference
capabilities:
  network:
    allow:
      - https://evil.example.com
  filesystem:
    scope: .
"#;
        let config = murmur_artifact::parse_hook_config_from_yaml(hook_own_manifest)
            .expect("the hook's own manifest still parses for its behavioral contract");
        assert_eq!(config.binding, HookBinding::OnInference);

        // The operator's own manifest, granting this hook nothing.
        let operator_manifest = murmur_artifact::RuntimeManifest::from_yaml_str(
            r#"
name: cap
version: 0.0.1
artifacts:
  - name: greedy-hook
    version: 1.0.0
    runtime: hook
"#,
        )
        .unwrap();
        let grant =
            HookCapabilityGrant::derive(operator_manifest.artifacts[0].capabilities.as_ref())
                .unwrap();

        assert_eq!(
            grant,
            HookCapabilityGrant::default(),
            "only the operator's entry may grant; the hook's own manifest is never read"
        );

        let root = TempDir::new().unwrap();
        let mut state = hook_store_state(root.path(), &grant);
        assert!(!send_through_hook_store(
            &mut state,
            "https://evil.example.com/exfil",
            true
        ));
        let target = RequestTarget {
            scheme: "https".to_string(),
            host: "evil.example.com".to_string(),
            port: Some(443),
        };
        assert!(!grant
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&target)));
    }

    // ── Uniform hook-output dispatch: unsupported-arm faults ──────────────────

    /// An `on-tool-call` double returning `ok(<arm>)`, where `arm_disc` selects the
    /// `hook-output` variant: `0` = `none`, `1` = `replace-context([])`, `2` =
    /// `write-manifests([])`. `on-tool-call` honors no arm (see
    /// [`HONORED_OUTPUT_ARM`]), so `1` and `2` are unsupported-arm faults while `0`
    /// is silent. The list arms return an empty list — `(ptr,len)` left at `(0,0)` —
    /// so no guest heap layout is needed.
    ///
    /// Return area at 128: `result` discriminant `0` (ok); `hook-output`
    /// discriminant at 132; list `(ptr,len)` at 136/140.
    fn hook_tool_call_arm_double(engine: &wasmtime::Engine, arm_disc: i32) -> Component {
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .filter(|n| **n != "on-tool-call")
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let iface = LIFECYCLE_IFACE;
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "ontool")
      (param i32 i32 i32 i64 i64 i64 i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const {arm_disc}))
      (i32.store (i32.const 136) (i32.const 0))
      (i32.store (i32.const 140) (i32.const 0))
      (i32.const 128))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $tool-event (record
    (field "turn" u32)
    (field "tool-name" string)
    (field "input-bytes" u64)
    (field "output-bytes" u64)
    (field "duration-ms" u64)
    (field "status" string)))
  (type $ft (func (param "event" $tool-event) (result (result $hook-output (error string)))))

  (func $ot (type $ft)
    (canon lift (core func $i "ontool") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "tool-event" (type $tool-event))
    (export "on-tool-call" (func $ot))
{stubs}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("tool-call component WAT parses");
        Component::new(engine, &bytes).expect("tool-call component double compiles")
    }

    /// A *current-version* (`@0.5.0`, 5-case `hook-output`) `on-task-end` double that
    /// returns `ok(<arm>)`. `arm_disc` selects the variant: `0` = `none`, `4` =
    /// `reopen-task(reason)`. The `reopen-task` payload is a static string at guest
    /// offset 300 so the host lifts the real bytes the guest declared, exercising the
    /// typed `reopen-task` wire path end-to-end rather than host-side Rust state.
    ///
    /// Return area at 128: `result` discriminant `0` (ok); `hook-output` discriminant
    /// at 132; the `reopen-task` string `(ptr,len)` at 136/140. `on-task-end`'s flat
    /// params are the two strings of `task-end-event` (4 i32).
    fn hook_task_end_arm_double(
        engine: &wasmtime::Engine,
        arm_disc: i32,
        reason: &str,
    ) -> Component {
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let reason_len = reason.len();
        let iface = LIFECYCLE_IFACE;
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (data (i32.const 300) "{reason}")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "ontaskend") (param i32 i32 i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const {arm_disc}))
      (i32.store (i32.const 136) (i32.const 300))
      (i32.store (i32.const 140) (i32.const {reason_len}))
      (i32.const 128))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $task-end-event (record
    (field "task-id" string)
    (field "exit-status" string)))
  (type $ft (func (param "event" $task-end-event) (result (result $hook-output (error string)))))

  (func $te (type $ft)
    (canon lift (core func $i "ontaskend") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "task-end-event" (type $task-end-event))
    (export "on-task-end" (func $te))
{stubs}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("task-end component WAT parses");
        Component::new(engine, &bytes).expect("task-end component double compiles")
    }

    /// Scenario: a blocking `on-task-end` hook returning `reopen-task(reason)` is the one
    /// honored arm for that event; `dispatch_task_end` surfaces it as a
    /// [`TaskReopen`] naming the hook and carrying the exact feedback string the guest
    /// returned, with no dispatch fault logged.
    #[test]
    fn on_task_end_reopen_task_is_surfaced_by_dispatch_task_end() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 4 = reopen-task.
        let component = hook_task_end_arm_double(&engine, 4, "tests still fail");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named("gatekeeper", HookBinding::OnTaskEnd, component)],
            )
            .await
            .expect("task-end double instantiates");

            let reopen = hooks
                .dispatch_task_end("tsk_1".to_string(), "ok".to_string())
                .await
                .expect("on-task-end returned reopen-task, so a TaskReopen must surface");
            assert_eq!(reopen.hook_name, "gatekeeper");
            assert_eq!(reopen.reason, "tests still fail");
            assert!(
                hooks.drain_dispatch_faults().is_empty(),
                "reopen-task is honored at on-task-end, not a fault"
            );
        });

        assert_eq!(
            hook_log_lines(session.path(), "gatekeeper"),
            0,
            "an honored reopen-task must not log a fault"
        );
    }

    /// An `on-task-end` hook returning `none` (arm 0) drives no reopen:
    /// `dispatch_task_end` returns `None`.
    #[test]
    fn on_task_end_none_surfaces_no_reopen() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_task_end_arm_double(&engine, 0, "unused");

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named("observer", HookBinding::OnTaskEnd, component)],
            )
            .await
            .expect("task-end double instantiates");

            let reopen = hooks
                .dispatch_task_end("tsk_1".to_string(), "ok".to_string())
                .await;
            assert!(reopen.is_none(), "none from on-task-end must not request a reopen");
            assert!(hooks.drain_dispatch_faults().is_empty());
        });
    }

    /// Invariant: `reopen-task` returned from any event other than `on-task-end` is a
    /// dispatch fault, not a silent grant. Bind the same 5-case double to `on-tool-call`
    /// (its `on-tool-call` export is a bare stub, so dispatching it fails `.typed()` —
    /// but the point tested here is the honored-arm *table*: `on-tool-call` honors no
    /// arm, so even if it returned `reopen-task` it would be classified `Fault`, exactly
    /// as `classify_output` decides generically).
    #[test]
    fn reopen_task_is_only_honored_at_on_task_end() {
        // Pure table check: reopen-task is honored ONLY by on-task-end.
        for (event, arm) in HONORED_OUTPUT_ARM {
            let honors_reopen = *arm == Some("reopen-task");
            assert_eq!(
                honors_reopen,
                *event == "on-task-end",
                "{event} honoring reopen-task must be exactly on-task-end"
            );
        }
    }

    /// An `on-stage` double returning `ok(<arm>)`, `arm_disc` as in
    /// [`hook_tool_call_arm_double`]. `on-stage` honors only `write-manifests`
    /// (`2`), so `1` (`replace-context`) is an unsupported-arm fault and `0`
    /// (`none`) is silent — exercised through `dispatch_stage`.
    fn hook_stage_arm_double(engine: &wasmtime::Engine, arm_disc: i32) -> Component {
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let iface = LIFECYCLE_IFACE;
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "onstage") (param i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (i32.store (i32.const 132) (i32.const {arm_disc}))
      (i32.store (i32.const 136) (i32.const 0))
      (i32.store (i32.const 140) (i32.const 0))
      (i32.const 128))
    (func (export "noop"))
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $message (record (field "role" string) (field "content" string)))
  (type $tool-manifest (record (field "binary-name" string) (field "content" string)))
  (type $hook-output (variant
    (case "none")
    (case "replace-context" (list $message))
    (case "write-manifests" (list $tool-manifest))
    (case "artifact" string)
    (case "reopen-task" string)))
  (type $stage-event (record (field "shell-allow" (list string))))
  (type $ft (func (param "event" $stage-event) (result (result $hook-output (error string)))))

  (func $os (type $ft)
    (canon lift (core func $i "onstage") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (func $noop (canon lift (core func $i "noop")))

  (instance $lc
    (export "message" (type $message))
    (export "tool-manifest" (type $tool-manifest))
    (export "hook-output" (type $hook-output))
    (export "stage-event" (type $stage-event))
    (export "on-stage" (func $os))
{stubs}
  )
  (export "{iface}" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("stage component WAT parses");
        Component::new(engine, &bytes).expect("stage component double compiles")
    }

    fn tool_call_event() -> HookEvent {
        HookEvent::ToolCall {
            turn: 0,
            tool_name: "bash".to_string(),
            input_bytes: 0,
            output_bytes: 0,
            duration_ms: 0,
            status: "ok".to_string(),
        }
    }

    /// Scenario (a): a blocking hook that returns a non-`none` arm the event does not
    /// honor (`on-tool-call` → `write-manifests`) gets exactly one line in
    /// `logs/hook-<name>.log` naming the hook/event/arm, and exactly one entry from
    /// the new drain accessor carrying the same structured fields. The session
    /// continues — `emit` returns normally with no artifact.
    #[test]
    fn unsupported_arm_from_blocking_hook_is_logged_and_buffered() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 2 = write-manifests, unsupported for on-tool-call.
        let component = hook_tool_call_arm_double(&engine, 2);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "toolhook",
                    HookBinding::OnToolCall,
                    component,
                )],
            )
            .await
            .expect("double instantiates");

            let artifacts = hooks.emit(session.path(), tool_call_event()).await;
            assert!(
                artifacts.is_empty(),
                "an unsupported arm commits nothing, exactly as `none` would"
            );

            let faults = hooks.drain_dispatch_faults();
            assert_eq!(faults.len(), 1, "exactly one buffered fault");
            assert_eq!(faults[0].hook_name, "toolhook");
            assert_eq!(faults[0].event, "on-tool-call");
            assert_eq!(faults[0].arm, "write-manifests");

            // Draining is destructive: a second drain yields nothing.
            assert!(hooks.drain_dispatch_faults().is_empty());
        });

        assert_eq!(
            hook_log_lines(session.path(), "toolhook"),
            1,
            "exactly one fault line is written to the per-hook log"
        );
        let log = std::fs::read_to_string(
            session.path().join("logs").join("hook-toolhook.log"),
        )
        .unwrap();
        assert!(log.contains("on-tool-call"), "log names the event: {log}");
        assert!(log.contains("write-manifests"), "log names the arm: {log}");
        assert!(log.contains("toolhook"), "log names the hook: {log}");
    }

    /// Scenario (c): `none` from an event that honors no arm produces no fault —
    /// no log line, no buffered fault.
    #[test]
    fn none_from_blocking_hook_produces_no_fault() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 0 = none.
        let component = hook_tool_call_arm_double(&engine, 0);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "quiet",
                    HookBinding::OnToolCall,
                    component,
                )],
            )
            .await
            .expect("double instantiates");

            hooks.emit(session.path(), tool_call_event()).await;
            assert!(
                hooks.drain_dispatch_faults().is_empty(),
                "`none` must never buffer a fault"
            );
        });

        assert_eq!(
            hook_log_lines(session.path(), "quiet"),
            0,
            "`none` must never write a log line"
        );
    }

    /// Scenario (b), on-inference/artifact half: the honored arm is committed and
    /// produces no fault. Uses the inference-caller double (returns `artifact`).
    #[test]
    fn honored_inference_artifact_produces_no_fault() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_inference_caller_double(&engine);

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "inf",
                    HookBinding::OnInference,
                    component,
                )],
            )
            .await
            .expect("double instantiates");

            let artifacts = hooks.emit(session.path(), inference_event()).await;
            assert_eq!(artifacts.len(), 1, "the honored artifact arm is committed");
            assert!(
                hooks.drain_dispatch_faults().is_empty(),
                "the honored arm must not be a fault"
            );
        });

        assert_eq!(
            hook_log_lines(session.path(), "inf"),
            0,
            "the honored arm must not write a fault line"
        );
    }

    /// Scenario (b), on-compaction/replace-context half: the honored arm still
    /// produces `Ok(Some(messages))` and buffers no fault.
    #[test]
    fn honored_compaction_replace_context_produces_no_fault() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let rt = tokio::runtime::Runtime::new().unwrap();

        rt.block_on(async {
            let mut hooks = new_with_hooks(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "compactor",
                    HookBinding::OnCompaction,
                    hook_compaction_echo_model_double(&engine),
                )],
            )
            .await
            .expect("double instantiates");

            let result = hooks
                .dispatch_compaction(
                    vec![Message {
                        role: "user".to_string(),
                        content: "hello".to_string(),
                    }],
                    1234,
                    0.98,
                    Some("claude-haiku-4-5".to_string()),
                    None,
                )
                .await;
            let messages = result.expect("hook succeeded").expect("replace-context");
            assert_eq!(messages[0].content, "claude-haiku-4-5");
            assert!(
                hooks.drain_dispatch_faults().is_empty(),
                "the honored replace-context arm must not be a fault"
            );
        });

        assert_eq!(
            hook_log_lines(session.path(), "compactor"),
            0,
            "the honored arm must not write a fault line"
        );
    }

    /// Scenario (d): an `on-stage` hook returning an unsupported arm
    /// (`replace-context`) is logged via `dispatch_stage`'s path, and no
    /// `trace.jsonl` is written — `dispatch_stage` runs before the trace writer
    /// exists.
    #[test]
    fn on_stage_unsupported_arm_is_logged_not_traced() {
        let workdir = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 1 = replace-context, unsupported for on-stage.
        let staged = staged_double_named("stager", HookBinding::OnStage, hook_stage_arm_double(&engine, 1));

        dispatch_stage(
            &engine,
            workdir.path(),
            std::slice::from_ref(&staged),
            Vec::new(),
            &HookEnvVars::default(),
            ExecutionLimits::default(),
        )
        .expect("dispatch_stage returns Ok even when a hook returns an unsupported arm");

        assert_eq!(
            hook_log_lines(workdir.path(), "stager"),
            1,
            "the on-stage fault is logged once"
        );
        let log =
            std::fs::read_to_string(workdir.path().join("logs").join("hook-stager.log")).unwrap();
        assert!(log.contains("on-stage"), "log names the event: {log}");
        assert!(log.contains("replace-context"), "log names the arm: {log}");
        assert!(
            !workdir.path().join("trace.jsonl").exists(),
            "on-stage runs before the trace writer exists, so no trace is attempted"
        );
    }

    /// Scenario (d) control: `write-manifests` from `on-stage` is the honored arm —
    /// it writes the manifest and logs no fault.
    #[test]
    fn on_stage_write_manifests_remains_honored() {
        let workdir = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 2 = write-manifests (honored) — but with an empty list, so nothing is
        // written; the point is that no fault is logged for the honored arm.
        let staged = staged_double_named("stager", HookBinding::OnStage, hook_stage_arm_double(&engine, 2));

        dispatch_stage(
            &engine,
            workdir.path(),
            std::slice::from_ref(&staged),
            Vec::new(),
            &HookEnvVars::default(),
            ExecutionLimits::default(),
        )
        .expect("dispatch_stage succeeds");

        assert_eq!(
            hook_log_lines(workdir.path(), "stager"),
            0,
            "the honored write-manifests arm must not log a fault"
        );
    }

    /// Scenario (e): an async hook returning an unsupported arm is routed to the
    /// same `Err` channel a genuine async error takes — `call_async_hook` returns
    /// `Err` naming the event and arm, which the `dispatch` spawn site logs once via
    /// the unchanged `log_hook_error` path (never traced).
    #[test]
    fn async_hook_unsupported_arm_routes_to_error_channel() {
        let root = TempDir::new().unwrap();
        let engine = hook_test_engine();
        // arm 2 = write-manifests, unsupported for on-tool-call.
        let component = hook_tool_call_arm_double(&engine, 2);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt.block_on(async {
            call_async_hook(
                &engine,
                root.path(),
                &component,
                "async-hook",
                &SessionContextData {
                    capsule_name: "test-capsule".to_string(),
                    capsule_version: "0.1.0".to_string(),
                    session_id: "sess-test".to_string(),
                    model: "test-model".to_string(),
                    capabilities: Vec::new(),
                },
                &tool_call_event(),
                Duration::from_secs(0),
                HookTotals {
                    input_tokens: 0,
                    output_tokens: 0,
                    tool_calls: 0,
                    shell_calls: 0,
                },
                ExecutionLimits::default(),
                None,
                &HookCapabilityGrant::default(),
            )
            .await
            .expect_err("an unsupported arm must surface as an Err on the async path")
        });

        assert!(err.contains("on-tool-call"), "async fault names the event: {err}");
        assert!(err.contains("write-manifests"), "async fault names the arm: {err}");
    }

    /// Scenario (e), full path: an async hook (`execution_mode: async`) returning an
    /// unsupported arm is logged exactly once to `logs/hook-<name>.log` via the
    /// fire-and-forget `spawn_local` site in `dispatch`, and buffers no fault (async
    /// faults are never traced).
    #[test]
    fn async_hook_unsupported_arm_logged_once_and_not_buffered() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let component = hook_tool_call_arm_double(&engine, 2);

        let staged = StagedHookArtifact {
            name: "async-tool".to_string(),
            version: "0.0.1".to_string(),
            component,
            config: HookConfig {
                binding: HookBinding::OnToolCall,
                execution_mode: HookExecutionMode::Async,
                ..HookConfig::default()
            },
            grant: HookCapabilityGrant::default(),
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let mut hooks = new_with_hooks(&engine, session.path(), accessible.path(), vec![staged])
                .await
                .expect("async double instantiates");
            hooks.emit(session.path(), tool_call_event()).await;
            // The async hook is fire-and-forget via spawn_local; yield until it has
            // run to completion and logged (bounded so a genuine failure still ends).
            for _ in 0..2000 {
                if hook_log_lines(session.path(), "async-tool") >= 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                hooks.drain_dispatch_faults().is_empty(),
                "async faults are logged but never buffered for the trace"
            );
        });

        assert_eq!(
            hook_log_lines(session.path(), "async-tool"),
            1,
            "the async unsupported-arm fault is logged exactly once"
        );
    }

    /// The honored-arm table has exactly one entry per WIT lifecycle function, and
    /// every entry's arm (when set) is a real `hook-output` variant name. Guards the
    /// single-source-of-truth invariant against a typo or a missing/extra row.
    #[test]
    fn honored_output_arm_table_is_complete_and_consistent() {
        let mut names: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        names.extend_from_slice(&OPTIONAL_HOOK_FNS);
        names.push("on-stage");
        assert_eq!(
            HONORED_OUTPUT_ARM.len(),
            names.len(),
            "one honored-arm entry per lifecycle function"
        );
        for name in &names {
            assert!(
                HONORED_OUTPUT_ARM.iter().any(|(n, _)| n == name),
                "{name} must have a honored-arm entry"
            );
        }
        for (_, arm) in HONORED_OUTPUT_ARM {
            if let Some(arm) = arm {
                assert!(
                    matches!(
                        *arm,
                        "replace-context" | "write-manifests" | "artifact" | "reopen-task"
                    ),
                    "honored arm {arm} must be a real hook-output variant"
                );
            }
        }
    }
}
