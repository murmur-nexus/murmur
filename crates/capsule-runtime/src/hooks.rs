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

use murmur_artifact::{HookBinding, HookConfig, HookExecutionMode, PACKED_MANIFEST_ENTRY};

use crate::{
    agent::append_bootstrap_log,
    bindings::hook::exports::murmur::hook::lifecycle::{
        CompactionEvent, HookOutput, InferenceEvent, Message, SessionContext, SessionEndEvent,
        ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
    },
    checkpoint_sign::{sign_existing_checkpoints, verify_and_quarantine_checkpoints},
    errors::RuntimeError,
    inference_import::{add_inference_to_linker, HookInferenceCtx, HookInferenceRecord},
    limits::{classify_guest_failure, ExecutionLimiter, ExecutionLimits},
    types::StagedHookArtifact,
};

/// Current versioned instance export name: what a hook compiled against
/// `murmur:hook@0.3.0` (5-field `compaction-event`) carries in its
/// component-type section.
const OBS_IFACE_V0_3: &str = "murmur:hook/lifecycle@0.3.0";

/// Previous versioned instance export name, still accepted.
///
/// `murmur:hook` went `0.2.0 → 0.3.0` because `compaction-event` gained two
/// fields, which the canonical ABI cannot absorb additively. Every hook other
/// than `murmur-hook-compact` is unaffected by those fields, so the host keeps
/// loading `@0.2.0`-compiled hooks rather than forcing a fleet-wide rebuild —
/// it just sends them the old 3-field `compaction-event` (see
/// [`CompactionEventV02`]).
///
/// This is a **transitional, package-scoped** exception covering exactly two
/// versions of one package. It is *not* a reinstatement of the general
/// unversioned-name fallback, which was removed permanently; a hook exporting
/// the bare `murmur:hook/lifecycle` name still fails hard. See
/// `wit/VERSIONING.md`.
const OBS_IFACE_V0_2: &str = "murmur:hook/lifecycle@0.2.0";

/// Which `murmur:hook/lifecycle` version a given component resolved at. The
/// only dispatch decision it drives is the `on-compaction` record shape — every
/// other lifecycle record is byte-identical between the two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleVersion {
    V0_3,
    V0_2,
}

/// Resolve the lifecycle instance export, trying `@0.3.0` first and falling back
/// to `@0.2.0`. Returns the export index together with the version that matched.
/// `None` means the component exports neither name, which surfaces as a
/// missing-export error at the call site.
fn resolve_lifecycle_iface(
    instance: &wasmtime::component::Instance,
    store: &mut Store<HookStoreState>,
) -> Option<(wasmtime::component::ComponentExportIndex, LifecycleVersion)> {
    if let Some(idx) = instance.get_export_index(&mut *store, None, OBS_IFACE_V0_3) {
        return Some((idx, LifecycleVersion::V0_3));
    }
    instance
        .get_export_index(&mut *store, None, OBS_IFACE_V0_2)
        .map(|idx| (idx, LifecycleVersion::V0_2))
}

/// Diagnostic naming both accepted lifecycle export names, used wherever
/// resolution fails.
fn missing_lifecycle_msg(subject: &str) -> String {
    format!(
        "hook {subject} exports neither {OBS_IFACE_V0_3} nor {OBS_IFACE_V0_2}; rebuild the hook against the versioned WIT (run `mur install` for a default artifact, or rebuild from source otherwise)"
    )
}

/// The `murmur:hook@0.2.0` shape of `compaction-event`, hand-derived because
/// bindgen only ever generates the *current* (`@0.3.0`, 5-field) record.
///
/// `TypedFunc::typed` checks a component function structurally — field order and
/// types, not names — so lowering the 5-field [`CompactionEvent`] into a
/// `@0.2.0`-compiled hook's `on-compaction` fails the type check outright rather
/// than truncating. Sending this 3-field twin instead is what lets an
/// un-rebuilt hook keep receiving compaction events unchanged. `Lower` only:
/// the host builds and sends one, it never lifts one back.
#[derive(wasmtime::component::ComponentType, wasmtime::component::Lower)]
#[component(record)]
struct CompactionEventV02 {
    messages: Vec<Message>,
    #[component(name = "session-tokens")]
    session_tokens: u64,
    threshold: f64,
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

struct HookInstance {
    name: String,
    config: HookConfig,
    store: Store<HookStoreState>,
    /// Lifecycle package version this component's exports resolved at; selects
    /// the `on-compaction` record shape.
    lifecycle_version: LifecycleVersion,
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
}

struct HookStoreState {
    /// Resource limiter for this store, registered via `Store::limiter`, and the record of
    /// any growth request it denied — read back by `classify_guest_failure` in
    /// [`call_typed`] so a limit trap reads distinguishably in `logs/hook-<name>.log`.
    limits: ExecutionLimiter,
    table: ResourceTable,
    wasi: WasiCtx,
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
        /// Manifest-configured model for the hook's own summarization call.
        /// Always `None` today — `compaction-model-from-manifest` populates it.
        model: Option<String>,
        /// Manifest-configured system-prompt override for that call. Always
        /// `None` today — `compaction-system-prompt-override` populates it.
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
                Ok(HookOutput::WriteManifests(manifests)) => {
                    for m in manifests {
                        let dir = workdir.join("tools").join(&m.binary_name);
                        if let Err(e) = std::fs::create_dir_all(&dir) {
                            log_hook_error(
                                workdir,
                                &staged.name,
                                &format!("failed to create tool dir for {}: {e}", m.binary_name),
                            )
                            .await;
                            continue;
                        }
                        let manifest_path = dir.join(PACKED_MANIFEST_ENTRY);
                        if let Err(e) = std::fs::write(&manifest_path, &m.content) {
                            log_hook_error(
                                workdir,
                                &staged.name,
                                &format!("failed to write manifest for {}: {e}", m.binary_name),
                            )
                            .await;
                        }
                    }
                }
                Ok(_) => {}
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
    // `on-stage` runs during staging, long before an inference driver exists —
    // the import is defined so an inference-importing hook still links, and
    // always errors.
    add_inference_to_linker(&mut linker, format!("hook:{}", staged.name), None)?;

    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(workdir, env_vars).map_err(|e| e.to_string())?,
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

    let (obs_idx, _version) = resolve_lifecycle_iface(&instance, &mut store)
        .ok_or_else(|| missing_lifecycle_msg(&format!("{}@{}", staged.name, staged.version)))?;

    let func = instance
        .get_export_index(&mut store, Some(&obs_idx), "on-stage")
        .and_then(|idx| instance.get_func(&mut store, idx))
        .ok_or_else(|| format!("hook {}@{} missing on-stage", staged.name, staged.version))?;

    let f = func
        .typed::<(StageEvent,), (Result<HookOutput, String>,)>(&store)
        .map_err(|e| e.to_string())?;
    // Fresh budget for the call itself, so instantiation cost cannot eat into it.
    store.set_epoch_deadline(limits.deadline_ticks());
    let called = f.call_async(&mut store, (evt.clone(),)).await;
    let (result,) = match called {
        Ok(result) => result,
        Err(err) => {
            let failure = classify_guest_failure(&err, &store.data().limits);
            return Err(failure.message(&format!("hook '{}' on-stage", staged.name), &err));
        }
    };
    f.post_return_async(&mut store)
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

    /// Dispatch `event` to all bound hooks. Returns every artifact emitted by
    /// any blocking hook via `HookOutput::Artifact`, in hook-registration order.
    /// An empty `Vec` means no hook produced an artifact for this event.
    pub(crate) async fn emit(&mut self, workdir: &Path, event: HookEvent) -> Vec<HookArtifact> {
        self.dispatch(workdir, event).await.0
    }

    /// Shared dispatch path used by every Lifecycle Event. Iterates the blocking
    /// hooks (binding-filtered), then spawns each matching async hook fire-and-forget.
    /// Returns `(artifacts, replacement)`: `artifacts` collects every `on-inference`
    /// `HookOutput::Artifact`; `replacement` is the first `on-compaction`
    /// `HookOutput::ReplaceContext`. Event-keyed side effects (checkpoint verify on
    /// `SessionStart`, checkpoint sign on `SessionEnd` and on a compaction that
    /// replaced context) run here so all events funnel through one place.
    async fn dispatch(
        &mut self,
        workdir: &Path,
        event: HookEvent,
    ) -> (Vec<HookArtifact>, Option<Vec<Message>>) {
        if matches!(event, HookEvent::SessionStart) {
            self.verify_checkpoints_on_start(workdir);
        }

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
                Ok(HookCallResult::None) => {}
                Err(error) => {
                    log_hook_error(workdir, &hook.name, &error).await;
                }
            }
        }

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
                )
                .await
                {
                    log_hook_error(&session_workdir, &name, &err).await;
                }
            });
        }

        if matches!(event, HookEvent::SessionEnd { .. }) {
            self.sign_checkpoints(workdir);
        }
        if matches!(event, HookEvent::Compaction { .. }) && replacement.is_some() {
            self.sign_checkpoints(workdir);
        }

        (artifacts, replacement)
    }

    /// Verifies checkpoint files under `accessible_workdir/checkpoints` against their `.sig`
    /// sidecars, quarantining anything missing/invalid before the agent gets control.
    /// `log_workdir` is where the rejection warning (if any) is logged, matching the
    /// `workdir` this event's `log_hook_error` calls already use.
    fn verify_checkpoints_on_start(&self, log_workdir: &Path) {
        match verify_and_quarantine_checkpoints(&self.accessible_workdir) {
            Ok(quarantined) if !quarantined.is_empty() => {
                append_bootstrap_log(
                    log_workdir,
                    &format!(
                        "[checkpoint] rejected unsigned/tampered checkpoint file(s): {}",
                        quarantined.join(", ")
                    ),
                );
            }
            Ok(_) => {}
            Err(err) => {
                append_bootstrap_log(
                    log_workdir,
                    &format!(
                        "[checkpoint] signing key unavailable, skipping checkpoint verification: {err}"
                    ),
                );
            }
        }
    }

    /// Signs whatever checkpoint files currently exist under `accessible_workdir/checkpoints`.
    fn sign_checkpoints(&self, log_workdir: &Path) {
        if let Err(err) = sign_existing_checkpoints(&self.accessible_workdir) {
            append_bootstrap_log(
                log_workdir,
                &format!("[checkpoint] signing key unavailable, skipping checkpoint signing: {err}"),
            );
        }
    }

    /// Fire `on-compaction` on all hooks with a matching binding, returning the first
    /// `replace-context` output any blocking hook produces (or `None`).
    ///
    /// Now a thin wrapper over the shared [`Self::dispatch`] path: it builds a
    /// `HookEvent::Compaction` and returns only the `replacement` half of the result.
    /// Async hooks fire-and-forget; their output is always discarded. Checkpoint
    /// signing after a successful replacement happens inside `dispatch`.
    #[must_use]
    pub(crate) async fn dispatch_compaction(
        &mut self,
        messages: Vec<Message>,
        session_tokens: u64,
        threshold: f64,
        model: Option<String>,
        system_prompt: Option<String>,
    ) -> Option<Vec<Message>> {
        let workdir = self.workdir.clone();
        let event = HookEvent::Compaction {
            messages,
            session_tokens,
            threshold,
            model,
            system_prompt,
        };
        self.dispatch(&workdir, event).await.1
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
    add_inference_to_linker(&mut linker, format!("hook:{}", staged.name), inference)
        .map_err(RuntimeError::Runtime)?;

    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(project_dir, env_vars)?,
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

    let (obs_idx, lifecycle_version) = resolve_lifecycle_iface(&instance, &mut store)
        .ok_or_else(|| {
            RuntimeError::Runtime(missing_lifecycle_msg(&format!(
                "{}@{}",
                staged.name, staged.version
            )))
        })?;

    let iface_name = match lifecycle_version {
        LifecycleVersion::V0_3 => OBS_IFACE_V0_3,
        LifecycleVersion::V0_2 => OBS_IFACE_V0_2,
    };
    let funcs = resolve_hook_fns(&instance, &mut store, &obs_idx, |fn_name| {
        RuntimeError::Runtime(format!(
            "hook {}@{} missing function {iface_name}#{fn_name}",
            staged.name, staged.version
        ))
    })?;

    Ok(HookInstance {
        funcs,
        name: staged.name.clone(),
        config: staged.config.clone(),
        store,
        lifecycle_version,
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
    let f = func
        .typed::<(T,), (Result<HookOutput, String>,)>(&hook.store)
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
    Ok(Some(result?))
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
            command,
            exit_code,
            stdout,
            stderr,
            stdout_bytes,
            stderr_bytes,
            duration_ms,
        } => {
            let evt = ShellEvent {
                turn: *turn,
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
            // The one record whose shape differs between `murmur:hook@0.2.0`
            // and `@0.3.0`. `TypedFunc::typed` is structural, so the 5-field
            // record simply does not type-check against a `@0.2.0`-compiled
            // hook's `on-compaction`; send it the 3-field twin instead.
            match hook.lifecycle_version {
                LifecycleVersion::V0_3 => {
                    let evt = CompactionEvent {
                        messages: messages.clone(),
                        session_tokens: *session_tokens,
                        threshold: *threshold,
                        model: model.clone(),
                        system_prompt: system_prompt.clone(),
                    };
                    call_typed(hook, "on-compaction", evt).await?
                }
                LifecycleVersion::V0_2 => {
                    let evt = CompactionEventV02 {
                        messages: messages.clone(),
                        session_tokens: *session_tokens,
                        threshold: *threshold,
                    };
                    call_typed(hook, "on-compaction", evt).await?
                }
            }
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

    // Only on-inference forwards artifacts; only on-compaction forwards a replacement.
    // Every other event (and every non-matching output) commits nothing.
    Ok(match event {
        HookEvent::Inference { .. } => match output {
            Some(HookOutput::Artifact(payload)) => HookCallResult::Artifact(HookArtifact {
                hook_name: hook.name.clone(),
                payload,
            }),
            _ => HookCallResult::None,
        },
        HookEvent::Compaction { .. } => match output {
            Some(HookOutput::ReplaceContext(msgs)) => HookCallResult::ReplaceContext(msgs),
            _ => HookCallResult::None,
        },
        _ => HookCallResult::None,
    })
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
) -> Result<(), String> {
    let mut linker: Linker<HookStoreState> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(|e| e.to_string())?;
    add_inference_to_linker(&mut linker, format!("hook:{name}"), inference)?;

    let env = HookEnvVars::default();
    let state = HookStoreState {
        limits: limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(root_dir, &env).map_err(|e| e.to_string())?,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_epoch_deadline(limits.deadline_ticks());

    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(|e| e.to_string())?;

    let (obs_idx, lifecycle_version) =
        resolve_lifecycle_iface(&instance, &mut store).ok_or_else(|| missing_lifecycle_msg(name))?;

    let iface_name = match lifecycle_version {
        LifecycleVersion::V0_3 => OBS_IFACE_V0_3,
        LifecycleVersion::V0_2 => OBS_IFACE_V0_2,
    };
    let funcs = resolve_hook_fns(&instance, &mut store, &obs_idx, |fn_name| {
        format!("hook {name} missing {iface_name}#{fn_name}")
    })?;

    let mut tmp = HookInstance {
        name: name.to_string(),
        config: HookConfig::default(),
        funcs,
        store,
        lifecycle_version,
    };
    // Async hooks fire-and-forget; any committable output is intentionally discarded.
    call_hook(&mut tmp, context, event, elapsed, totals).await.map(|_| ())
}

/// Build a WASI context for a hook instance.
///
/// `root_dir` is preopened as `"."` — the hook's current directory.
/// For blocking hooks this is the project directory (same as tools), so file
/// reads like `std::fs::read("fibonacci.py")` resolve correctly.
/// Hooks no longer write any output to the filesystem; structured data is
/// returned via `HookOutput::Artifact` and forwarded to the SSE stream.
fn build_wasi_ctx(root_dir: &Path, env: &HookEnvVars<'_>) -> Result<WasiCtx, RuntimeError> {
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

    builder.inherit_network();
    builder.allow_ip_name_lookup(true);

    builder
        .preopened_dir(root_dir, ".", DirPerms::all(), FilePerms::all())
        .map_err(|err| RuntimeError::wasi(root_dir.to_path_buf(), err.to_string()))?;

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        checkpoint_sign::test_support::{with_home, HOME_LOCK},
        inference_import::INFERENCE_IFACE_VERSIONED,
    };
    use tempfile::TempDir;

    /// Builds a `HookRuntime` with zero registered hooks against a fresh `TempDir` pair,
    /// mirroring the `(workdir, accessible_workdir)` split `stage_session` produces. No
    /// wasmtime component is ever instantiated since `staged_hooks` is empty, so a plain
    /// default `Engine` suffices.
    async fn test_hook_runtime(workdir: &Path, accessible_workdir: &Path) -> HookRuntime {
        HookRuntime::new(
            &wasmtime::Engine::default(),
            workdir,
            accessible_workdir,
            Vec::new(),
            SessionContextData {
                capsule_name: "test-capsule".to_string(),
                capsule_version: "0.1.0".to_string(),
                session_id: "sess-test".to_string(),
                model: "test-model".to_string(),
                capabilities: Vec::new(),
            },
            HookEnvVars::default(),
            ExecutionLimits::default(),
            None,
        )
        .await
        .expect("HookRuntime::new should succeed with zero hooks")
    }

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
        // Default double exports the versioned instance name — the only name the
        // host resolves now that the unversioned fallback is removed — so the
        // required/optional suite exercises real resolution.
        hook_double_iface(engine, OBS_IFACE_V0_3, fn_names)
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

    fn staged_double(component: Component) -> StagedHookArtifact {
        StagedHookArtifact {
            name: "test-hook".to_string(),
            version: "0.0.1".to_string(),
            component,
            config: HookConfig::default(),
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

    /// Transition-window invariant: a hook component built against the *versioned*
    /// `murmur:hook/lifecycle@0.2.0` interface (the name a freshly-compiled hook
    /// carries) instantiates and registers every required and optional function.
    /// The versioned name is the one `resolve_lifecycle_iface` probes first.
    #[test]
    fn versioned_hook_double_instantiates_and_registers_fns() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let mut names: Vec<&str> = REQUIRED_HOOK_FNS.to_vec();
        names.extend_from_slice(&OPTIONAL_HOOK_FNS);
        let component = hook_double_iface(&engine, OBS_IFACE_V0_3, &names);

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
                msg.contains(OBS_IFACE_V0_3),
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
                msg.contains(OBS_IFACE_V0_3),
                "error must name the missing lifecycle export, got: {msg}"
            );
        });
    }

    #[test]
    fn session_end_emit_signs_existing_checkpoints() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        std::fs::create_dir_all(&checkpoints).unwrap();
        std::fs::write(checkpoints.join("summary.md"), "goals: ship it").unwrap();

        with_home(home.path(), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut hooks = test_hook_runtime(session.path(), accessible.path()).await;
                hooks
                    .emit(
                        session.path(),
                        HookEvent::SessionEnd {
                            total_turns: 1,
                            exit_status: "ok".to_string(),
                        },
                    )
                    .await;
            });
        });

        assert!(
            checkpoints.join("summary.md.sig").exists(),
            "SessionEnd dispatch should sign existing checkpoint files"
        );
    }

    #[test]
    fn session_start_emit_quarantines_tampered_checkpoint() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        std::fs::create_dir_all(&checkpoints).unwrap();
        std::fs::write(checkpoints.join("plan.json"), r#"{"tasks":[]}"#).unwrap();

        with_home(home.path(), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // Sign it first (simulating a prior session-end), then tamper, then start again.
                let mut hooks = test_hook_runtime(session.path(), accessible.path()).await;
                hooks
                    .emit(
                        session.path(),
                        HookEvent::SessionEnd {
                            total_turns: 1,
                            exit_status: "ok".to_string(),
                        },
                    )
                    .await;
                std::fs::write(checkpoints.join("plan.json"), r#"{"tasks":["evil"]}"#).unwrap();

                let mut resumed = test_hook_runtime(session.path(), accessible.path()).await;
                resumed.emit(session.path(), HookEvent::SessionStart).await;
            });
        });

        assert!(
            !checkpoints.join("plan.json").exists(),
            "tampered checkpoint should have been renamed away"
        );
        assert!(
            checkpoints.join("plan.json.rejected").exists(),
            "SessionStart dispatch should quarantine a tampered checkpoint file"
        );
        let bootstrap_log =
            std::fs::read_to_string(session.path().join("logs").join("bootstrap.log"))
                .unwrap_or_default();
        assert!(
            bootstrap_log.contains("plan.json"),
            "bootstrap.log should name the rejected file, got: {bootstrap_log}"
        );
    }

    #[test]
    fn session_start_emit_leaves_validly_signed_checkpoint_untouched() {
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = TempDir::new().unwrap();
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let checkpoints = accessible.path().join("checkpoints");
        std::fs::create_dir_all(&checkpoints).unwrap();
        std::fs::write(checkpoints.join("decisions.json"), r#"{"decisions":[]}"#).unwrap();

        with_home(home.path(), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut hooks = test_hook_runtime(session.path(), accessible.path()).await;
                hooks
                    .emit(
                        session.path(),
                        HookEvent::SessionEnd {
                            total_turns: 1,
                            exit_status: "ok".to_string(),
                        },
                    )
                    .await;

                let mut resumed = test_hook_runtime(session.path(), accessible.path()).await;
                resumed.emit(session.path(), HookEvent::SessionStart).await;
            });
        });

        assert!(
            checkpoints.join("decisions.json").exists(),
            "validly-signed checkpoint must survive verification untouched"
        );
        assert!(!checkpoints.join("decisions.json.rejected").exists());
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
        hook_spin_double_iface(engine, OBS_IFACE_V0_3)
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
    (case "artifact" string)))
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

    /// A lifecycle double whose `on-compaction` genuinely declares the record
    /// shape of `version` and returns `ok(hook-output::none)`.
    ///
    /// `TypedFunc::typed` checks structurally, so this is the only way to prove
    /// the host sends the *right* shape: a 5-field lower against the `@0.2.0`
    /// double (or vice versa) fails the type check and lands in the hook's error
    /// log instead of completing silently.
    ///
    /// The core function ignores its params and returns a pointer to a
    /// hand-laid-out `result<hook-output, string>` — discriminant `0` (ok) at
    /// offset 0, the `hook-output` variant at offset 4 with discriminant `0`
    /// (`none`). The other five required exports are bare `func() -> ()` stubs;
    /// only `on-compaction` is ever dispatched here.
    fn hook_compaction_double(engine: &wasmtime::Engine, version: LifecycleVersion) -> Component {
        let (iface, extra_fields, extra_params) = match version {
            LifecycleVersion::V0_3 => (
                OBS_IFACE_V0_3,
                "\n    (field \"model\" (option string))\n    (field \"system-prompt\" (option string))",
                " i32 i32 i32 i32 i32 i32",
            ),
            LifecycleVersion::V0_2 => (OBS_IFACE_V0_2, "", ""),
        };
        let stubs = REQUIRED_HOOK_FNS
            .iter()
            .filter(|n| **n != "on-compaction")
            .map(|n| format!("    (export \"{n}\" (func $noop))"))
            .collect::<Vec<_>>()
            .join("\n");
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "oncompact") (param i32 i32 i64 f64{extra_params}) (result i32)
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
    (case "artifact" string)))
  (type $compaction-event (record
    (field "messages" (list $message))
    (field "session-tokens" u64)
    (field "threshold" f64){extra_fields}))
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

    async fn dispatch_compaction_against(
        session: &Path,
        accessible: &Path,
        engine: &wasmtime::Engine,
        version: LifecycleVersion,
    ) {
        let mut hooks = new_with_hooks(
            engine,
            session,
            accessible,
            vec![staged_double_named(
                "compactor",
                HookBinding::OnCompaction,
                hook_compaction_double(engine, version),
            )],
        )
        .await
        .expect("both lifecycle versions must instantiate");
        assert_eq!(hooks.blocking_hooks[0].lifecycle_version, version);

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
            replacement.is_none(),
            "the double returns hook-output::none, so no context replacement"
        );
    }

    /// A hook still compiled against `murmur:hook/lifecycle@0.2.0` — every hook
    /// except the future rebuilt `murmur-hook-compact` — instantiates via the
    /// version fallback and receives `on-compaction` with the old 3-field
    /// record. Nothing is logged, so the dispatch really completed rather than
    /// failing `.typed()` and being isolated.
    #[test]
    fn v0_2_hook_receives_three_field_compaction_event() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(dispatch_compaction_against(
            session.path(),
            accessible.path(),
            &engine,
            LifecycleVersion::V0_2,
        ));

        assert_eq!(
            hook_log_lines(session.path(), "compactor"),
            0,
            "a @0.2.0 hook must receive on-compaction cleanly, not an ABI mismatch"
        );
    }

    /// A hook rebuilt against `@0.3.0` receives the new 5-field record.
    #[test]
    fn v0_3_hook_receives_five_field_compaction_event() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(dispatch_compaction_against(
            session.path(),
            accessible.path(),
            &engine,
            LifecycleVersion::V0_3,
        ));

        assert_eq!(hook_log_lines(session.path(), "compactor"), 0);
    }

    /// A `@0.2.0`-compiled hook keeps receiving every *other* lifecycle event
    /// too: those records are shape-identical between the two versions, so the
    /// single bindgen-generated type dispatches to either. Proven by a spin
    /// double re-exported under the `@0.2.0` instance name — reaching the epoch
    /// deadline means `on-session-start` type-checked and actually entered the
    /// guest.
    #[test]
    fn v0_2_hook_still_receives_unchanged_lifecycle_records() {
        let session = TempDir::new().unwrap();
        let accessible = TempDir::new().unwrap();
        let engine = hook_test_engine();
        let _ticker = crate::limits::EpochTicker::spawn(&engine);
        let limits = ExecutionLimits {
            deadline_seconds: 1,
            ..ExecutionLimits::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut hooks = new_with_hooks_limited(
                &engine,
                session.path(),
                accessible.path(),
                vec![staged_double_named(
                    "legacy",
                    HookBinding::All,
                    hook_spin_double_iface(&engine, OBS_IFACE_V0_2),
                )],
                limits,
            )
            .await
            .expect("a @0.2.0 hook must instantiate through the version fallback");
            assert_eq!(
                hooks.blocking_hooks[0].lifecycle_version,
                LifecycleVersion::V0_2
            );
            hooks.emit(session.path(), HookEvent::SessionStart).await;
        });

        let log = std::fs::read_to_string(session.path().join("logs").join("hook-legacy.log"))
            .expect("the spinning @0.2.0 hook logs its deadline");
        assert!(
            log.contains("exceeded its 1s execution deadline"),
            "the call must have entered the guest (deadline), not failed a type check; got: {log}"
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
    (case "artifact" string)))
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
  (export "{OBS_IFACE_V0_3}" (instance $lc))
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
}
