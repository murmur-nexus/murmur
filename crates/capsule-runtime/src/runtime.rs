use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use murmur_artifact::{
    current_platform, parse_hook_config_from_yaml, parse_tool_implementation_from_yaml,
    read_lockfile, security_warning_link, verify_sha256, write_lockfile_atomic, AfterTask,
    ArtifactImplementation, ArtifactRuntime, ContextConfig, ConversationMode, HookBinding,
    InferenceConfig, InterpreterRuntimeGrant, LifecycleConfig, LockedArtifact, LockedSha256,
    LockfileError, MurmurLock, Registry, RegistryError, RuntimeType, TaskAcceptance, LOCK_VERSION,
    MANIFEST_FILENAME, PACKED_MANIFEST_ENTRY, W_SEC_003, W_SEC_006, W_SEC_007, W_SEC_008,
    W_SEC_009, W_SEC_011, W_SEC_013, W_SEC_014,
};
use serde_yaml::Value;
use wasmtime::{
    component::{Component, HasSelf, Linker, ResourceTable},
    Config, Engine, Store,
};
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    p2::{
        bindings::http::types::ErrorCode as WasiHttpErrorCode,
        body::HyperOutgoingBody,
        types::{HostFutureIncomingResponse, OutgoingRequestConfig},
        HttpResult, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
    },
    WasiHttpCtx,
};

use crate::{
    a2a::{IncomingTask, TaskRegistry, TaskState},
    agent,
    artifact::{extract_manifest_yaml, extract_native_binary, extract_root_wasm, extract_skill_md},
    bindings::host::murmur::{
        self, artifact_manager::manage, message::send, tool_registry::invoke,
    },
    cgroup,
    containment::{achieved_containment_class, check_containment_floor},
    errors::RuntimeError,
    hooks::{
        dispatch_stage, HookEnvVars, HookEvent, HookRuntime, SessionContextData, ShellDispatchInfo,
        TaskReopen,
    },
    identity::{self, CapsuleIdentity},
    inference_import::HookInferenceCtx,
    limits::{classify_guest_failure, EpochTicker, ExecutionLimiter, GuestFailure},
    murmur_md,
    network_policy::{
        effective_tool_network_rules, parse_network_allow_rules, resolve_scoped_dir,
        validate_filesystem_scope, HookCapabilityGrant, NetworkAllowRule, RequestTarget,
        ToolCapabilityGrant,
    },
    otel::OtelEmitter,
    outgoing, resources, sandbox,
    sealed::UsernsGrant,
    shell::{
        build_shell_env, build_wasi_env_allowlist, execute_shell, is_shell_interpreter,
        shell_tool_manifest_yaml, split_shell_words, ShellResult,
    },
    state_store::STATE_PREOPEN_NAME,
    streaming::{
        emit_chunk_sse, emit_sse, emit_thinking_chunk_sse, SseBroadcast, SseEventBuffer,
        StreamStatus, TaskStatusUpdateEvent,
    },
    trace::TraceWriter,
    types::{
        ArtifactRequest, CapabilityPolicy, DispatchOutcome, InstalledArtifactSummary, LaunchResult,
        ResolvedLockArtifact, StageRequest, StagedHookArtifact, StagedSession,
    },
};

/// Versioned instance export name a guest built against the semver'd
/// `murmur:capsule@0.1.0` WIT package carries. This is the only name the host
/// resolves — the legacy unversioned fallback was removed after the dual-accept
/// runtime shipped (see `wit/VERSIONING.md`).
const WIT_CAPSULE_IFACE_VERSIONED: &str = "murmur:capsule/run@0.1.0";
/// Versioned instance export name a guest built against `murmur:tool@0.1.0`
/// carries. Only name the host resolves; see `WIT_CAPSULE_IFACE_VERSIONED`.
const WIT_TOOL_IFACE_VERSIONED: &str = "murmur:tool/run@0.1.0";

/// The host provides its guest-facing *import* interfaces under the versioned
/// instance name only. The legacy unversioned provisions were dropped after the
/// dual-accept runtime shipped; a guest importing only the
/// unversioned name now fails to link. See `wit/VERSIONING.md`.
const WIT_TOOL_REGISTRY_IFACE: &str = "murmur:tool-registry/invoke@0.1.0";
const WIT_TEXT_CHUNKS_IFACE: &str = "murmur:text/chunks@0.1.0";
const WIT_TASK_IFACE: &str = "murmur:task/task@0.1.0";

/// Resolve a guest interface instance export by its versioned name. Returns
/// `None` when the versioned name is absent, so a component that exports only
/// the legacy unversioned name (or no recognizable name) surfaces as the
/// missing-export error at the call site. See `wit/VERSIONING.md`.
fn resolve_versioned_iface<T>(
    instance: &wasmtime::component::Instance,
    store: &mut Store<T>,
    versioned: &str,
) -> Option<wasmtime::component::ComponentExportIndex> {
    instance.get_export_index(&mut *store, None, versioned)
}

/// Run one task's agent loop, honoring `on-task-end` `reopen-task` control decisions.
///
/// Fires `on-task-end` after every attempt (via [`HookRuntime::dispatch_task_end`]). If
/// a blocking hook returns `reopen-task(reason)` and both budgets still allow it — fewer
/// than `max_task_reopens` (the manifest's `lifecycle.max_task_reopens`) reopens used AND
/// cumulative task turns still below `inference.max_turns` — the task's
/// `accessible_workdir/task.md` is rewritten as the original content plus every reopen's
/// feedback so far, a `task_reopened` trace record is written, and the loop runs again.
/// Reopening shares one cumulative turn budget with the original attempt: each attempt is
/// handed only `max_turns - task_turns()` turns, so the whole task can never exceed the
/// capsule's turn ceiling.
///
/// Writes the terminal `task_end` record (carrying the final `reopen_count`) itself, and
/// the terminal `on-task-end` dispatch is simply the loop's last one. Returns the task's
/// final result: the last attempt's own result when a hook was satisfied (or none was
/// bound), or `Err` when a hook still wanted to reopen but the reopen budget or turn
/// ceiling was reached — so every existing `.is_err()` branch at the call sites treats an
/// exhausted reopen as a failed task. In that case the terminal record's `exit_status` is
/// `"reopen_budget_exhausted"`; otherwise it is the last attempt's `"ok"`/`"failed"`.
///
/// `agent_task_id` is what [`agent::run_agent_loop`] receives (governs A2A SSE emission);
/// `trace_task_id` is the id used for the `task_start`/`task_reopened`/`task_end` records
/// and the `on-task-end` hook event. The two coincide on the A2A path and differ on the
/// backward-compat `task.md` paths, which pass `agent_task_id = None`.
#[allow(clippy::too_many_arguments)]
async fn run_task_with_reopens(
    state: &mut CapsuleStoreState,
    workdir: &Path,
    inference: &InferenceConfig,
    max_task_reopens: u32,
    system_prompt: Option<String>,
    run_config: agent::AgentRunConfig,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    agent_task_id: Option<String>,
    sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    accessible_workdir: &Path,
    capsule_name: &str,
    capsule_version: &str,
    mode: ConversationMode,
    context_id: Option<String>,
    trace_task_id: &str,
) -> Result<(), RuntimeError> {
    let task_md_path = accessible_workdir.join("task.md");
    // Original task content, captured once before any feedback is appended, so repeated
    // reopens re-inject a fresh copy of every feedback item rather than compounding.
    let original_task = tokio::fs::read_to_string(&task_md_path)
        .await
        .unwrap_or_default();
    // Every reopen's (hook_name, reason) so far — all re-injected on each reopen.
    let mut feedback: Vec<(String, String)> = Vec::new();
    let mut reopens_used: u32 = 0;
    // Exactly the bytes the next attempt's agent loop will be handed, tracked alongside the
    // `task.md` writes below so `murmur:task-io/read`'s `as-given` form is what the model saw
    // rather than a re-read of a file whose path is a convention.
    let mut as_given = original_task.clone();

    loop {
        // This function owns the task's scope: nothing else puts a task in scope, which is why
        // the "no A2A message arrived" bypass path — a direct `run_agent_loop` call that
        // dispatches no `on-task-end` — correctly reports `no-task` to a hook.
        hooks.begin_task_attempt(original_task.clone(), as_given.clone());

        // Reopening never grants turns past `max_turns`: hand this attempt only the turns
        // still unspent by prior attempts of the same task. On the first attempt
        // `task_turns()` is 0 (reset by the preceding `write_task_start`), so it gets the
        // full ceiling.
        let remaining_turns = inference.max_turns.saturating_sub(trace.task_turns());
        let mut attempt_inference = inference.clone();
        attempt_inference.max_turns = remaining_turns;

        let result = agent::run_agent_loop(
            state,
            workdir,
            &attempt_inference,
            system_prompt.clone(),
            run_config.clone(),
            hooks,
            trace,
            otel,
            agent_task_id.clone(),
            sse.clone(),
            accessible_workdir,
            capsule_name,
            capsule_version,
            mode.clone(),
            context_id.clone(),
        )
        .await;

        let exit_str = if result.is_ok() { "ok" } else { "failed" };

        // Let the `on-task-end` hooks inspect this attempt and decide whether to reopen.
        let reopen = hooks
            .dispatch_task_end(trace_task_id.to_string(), exit_str.to_string())
            .await;

        match reopen {
            Some(TaskReopen { hook_name, reason }) => {
                // A hook wants more. Honor it only if a reopen remains in the budget AND
                // turns remain under the ceiling; otherwise the request is exhausted and
                // ends the task non-silently.
                let budget_ok = reopens_used < max_task_reopens;
                let turns_ok = trace.task_turns() < inference.max_turns;
                if budget_ok && turns_ok {
                    reopens_used += 1;
                    feedback.push((hook_name.clone(), reason.clone()));
                    let _ = trace
                        .write_task_reopened(trace_task_id, &hook_name, &reason, reopens_used)
                        .await;
                    // Rewrite task.md as original + all feedback so far; the resumed
                    // attempt picks it up through its normal `read_task`, so neither
                    // transport's message-building code needs to change.
                    let rewritten = build_reopen_task_md(&original_task, &feedback);
                    if let Err(e) = tokio::fs::write(&task_md_path, rewritten.as_bytes()).await {
                        eprintln!(
                            "[capsule-runtime] failed to inject reopen feedback into task.md: {e}"
                        );
                    }
                    as_given = rewritten;
                    continue;
                }
                // Budget or turn ceiling reached while a hook still wanted to reopen: end
                // the task as a distinct, non-silent failure rather than an ordinary
                // completion. `Err` keeps every existing `.is_err()` downstream branch.
                let _ = trace
                    .write_task_end(trace_task_id, "reopen_budget_exhausted", reopens_used)
                    .await;
                hooks.end_task();
                return Err(RuntimeError::AgentLoopFailed(format!(
                    "task reopen budget exhausted after {reopens_used} reopen(s): hook \
                     '{hook_name}' still requested another reopen"
                )));
            }
            None => {
                // No hook asked to reopen — this attempt is terminal.
                let _ = trace
                    .write_task_end(trace_task_id, exit_str, reopens_used)
                    .await;
                hooks.end_task();
                return result;
            }
        }
    }
}

/// Compose the reopened task's `task.md`: the original task content followed by a clearly
/// delimited feedback section for every reopen so far, each naming the hook that produced
/// it. Used by [`run_task_with_reopens`].
fn build_reopen_task_md(original: &str, feedback: &[(String, String)]) -> String {
    let mut out = original.trim_end().to_string();
    out.push_str(
        "\n\n---\n\n# Reopen feedback\n\nThe previous attempt was not accepted. Address the \
         following feedback, then continue.\n",
    );
    for (i, (hook_name, reason)) in feedback.iter().enumerate() {
        out.push_str(&format!(
            "\n## Reopen {} — from hook `{}`\n\n{}\n",
            i + 1,
            hook_name,
            reason.trim()
        ));
    }
    out
}

/// Live-delivery buffer: only needs to cover the lag between fastest and slowest
/// currently-connected reader.
const SSE_BROADCAST_CAPACITY: usize = 128;

/// Replay buffer: covers observers joining mid-task. Sized to handle the longest
/// expected task turn without eviction. Can be tuned independently of broadcast capacity.
/// Candidate for `LifecycleConfig` if operator tuning is needed in a future slice.
const SSE_REPLAY_CAPACITY: usize = 512;

/// Whether any staged hook can receive the `on-compaction` lifecycle event. Mirrors the
/// binding match the runtime uses when it actually dispatches compaction, so MURMUR.md's
/// "compaction configured" status reflects the real dispatch path rather than an artifact name.
fn has_compaction_hook(hooks: &[StagedHookArtifact]) -> bool {
    hooks.iter().any(|h| {
        matches!(
            h.config.binding,
            HookBinding::OnCompaction | HookBinding::All
        )
    })
}

/// Resolves and verifies all artifacts, compiles components, and prepares session state.
///
/// This function is intentionally separate from `launch_session` so policy/capability
/// checks can be layered in the seam between stage and launch without mixing concerns.
pub fn stage_session(
    registry: Arc<dyn Registry>,
    request: StageRequest,
) -> Result<StagedSession, RuntimeError> {
    validate_capability_policy(&request.capability_policy)?;
    // Before any registry pull, component compile or workdir creation: if this host cannot
    // back the declared floor, refuse rather than launch something weaker than was asked for.
    // `achieved` comes from a live kernel probe only — the manifest never gets a vote in what
    // the host is reported to provide.
    // `achieved` is the host's live kernel probe, capped by the one manifest property that can
    // lower it: `capabilities.filesystem.workdir_exec`. The manifest still gets no vote in what the
    // host is *reported* to provide — the cap can only ever subtract (see
    // `containment::achieved_containment_class`).
    let workdir_exec = request.capability_policy.workdir_exec_allowed;
    // Both host probes happen exactly once per session, right here, and every later consumer —
    // the refusal below and the `ScopeReport` recorded in the trace — reads these two bindings.
    // A second probe could read differently from the first (an AppArmor profile loaded, a
    // container's capabilities changed), and the trace would then describe a session that never
    // ran under what it claims.
    let enforcement_tier = sandbox::detect_enforcement_tier();
    // Probed only so the refusal can name *which* part of the sealed mechanism is missing —
    // the AppArmor profile, `CAP_SYS_ADMIN` inside a container, or the kernel itself.
    let sealed_blocker = crate::containment::detect_sealed_blocker();
    let achieved_containment = achieved_containment_class(enforcement_tier, workdir_exec);
    check_containment_floor(
        request.declared_containment_floor,
        achieved_containment,
        sealed_blocker,
        workdir_exec,
    )?;
    // The complete grant set this session is about to run under, in exactly the shape
    // `mur run --explain-scope --json` prints — same builder, same policy, same declared floor,
    // and the same two probes taken above. Computed once here rather than at trace-open time so
    // the record cannot drift from the decision that let the session start.
    let exports_files = request
        .exports
        .as_ref()
        .and_then(|exports| exports.files.clone());
    let exports_peer_files = request
        .exports
        .as_ref()
        .and_then(|exports| exports.peer_files.clone());
    // Resolved before the report and before any per-artifact staging, so a malformed store name
    // refuses the launch on the same terms as an unmeetable containment floor: nothing pulled,
    // nothing created, nothing instantiated. Resolution only — `stage_artifact_grant` below is
    // what actually creates a directory, and only for an artifact whose entry declared one.
    let state_stores = crate::state_store::state_store_reports(
        request
            .artifacts
            .iter()
            .map(|artifact| (artifact.name.as_str(), artifact.capabilities.as_ref())),
        &request.capsule_name,
    )?;
    warn_on_inert_capsule_wide_state(request.capability_policy.state_declared);
    let scope_report = crate::containment::scope_report_for_tier(
        &request.capability_policy,
        request.declared_containment_floor,
        enforcement_tier,
        sealed_blocker,
        crate::containment::detect_userns_grant(),
        request.exports.as_ref(),
        state_stores,
    );
    // Asked here, beside the containment floors and before any registry pull or workdir creation:
    // an ephemeral capsule's teardown is what bounds every handle it minted, and `after_task:
    // sleep` withdraws that bound on purpose. Once withdrawn, the declared lifetime is the only
    // one there is, so it has to be declared and it has to be short.
    check_persistent_handle_ttl(
        exports_peer_files.as_ref(),
        &resolve_lifecycle(
            request.lifecycle.clone(),
            request.lifecycle_override.as_ref(),
        ),
    )?;
    // A second, independent floor question, deliberately asked right here next to the first: not
    // "can this host back what was declared?" but "did the capsule declare enough for what it
    // asks for?". A `staged_runtime` grant needs a composed root to be staged into, and one is
    // only built for a capsule that declared `sealed` — so this refuses on the declared floor
    // alone and never consults the host probe above.
    crate::staged_runtime::check_staged_runtime_floor(
        &request.capability_policy.shell_staged_runtime,
        request.declared_containment_floor,
    )?;
    // The same question as the line above, asked of the other half of the same gap. That one
    // catches a grant declared at too low a floor; this one catches a `sealed` capsule that
    // declared no grant at all for a `shell.allow` entry that provably needs one — a `#!` script,
    // whose ELF/DT_NEEDED closure is empty, so the staging that makes an ELF binary work stages
    // nothing at all of what the script imports. Same declared-floor-only gating, same
    // pre-registry-pull position, same "name every offender once" refusal shape.
    crate::reachability::check_interpreted_entrypoints_reachable(
        &request.capability_policy,
        request.declared_containment_floor,
    )?;
    // A third refusal, in the same pre-staging seam and for the same fail-closed reason, but
    // about a mechanism that sits outside the containment ladder entirely: a capsule that can
    // spawn a native subprocess needs a network namespace to put it in, because that namespace —
    // not a syscall filter — is now what makes `capabilities.network.allow` mean anything for
    // that subprocess. See `RuntimeError::EgressNamespaceUnavailable` for why this refuses even
    // when the allowlist is empty.
    //
    // Narrower than `cgroup::requires_process_bounding` on purpose, not by oversight: that check
    // also fires for a capsule with a *native-implementation artifact* and neither `shell.allow`
    // nor `spawn.allow`, but `has_native_artifact` comes from `staged.installed_artifacts`, which
    // only exists after staging resolves the registry — i.e. after this very check has to have
    // already run, per the "before any registry pull" rule above. A native-artifact-only capsule
    // therefore does not get this clean refusal on a host that cannot build the namespace; it
    // instead surfaces as a raw `io::Error` out of `create_capsule_netns`'s `pre_exec` failure
    // when that artifact is actually launched.
    crate::network_namespace::check_egress_namespace(
        !request.capability_policy.shell_allow.is_empty()
            || !request.capability_policy.spawn_allow.is_empty(),
        crate::network_namespace::detect_egress_namespace_blocker(),
    )?;
    // Capsule-ceiling-level, not per-artifact: `interpreter_runtime` lives on the capsule's own
    // top-level `capabilities.shell`, so warn here (before the per-artifact staging loop) rather
    // than in `stage_artifact_grant`.
    warn_on_interpreter_runtime_grants(&request.capability_policy.shell_interpreter_runtime);
    // Same seam, same reason: a capsule-wide declaration whose cost the operator should see stated
    // once, before anything else happens. Ordered after the refusals above so a manifest that is
    // going to be rejected outright is not first warned about.
    warn_on_workdir_exec(workdir_exec);
    // A host posture rather than a manifest declaration, but stated in the same place and for the
    // same reason: this session is about to record an achieved class that a weakened host and the
    // shipped profile can both produce, and the operator should be told which one they are on
    // before reading the result. Never a refusal — see `warn_on_userns_restriction_disabled_host_wide`.
    // Read off the report rather than re-probed, so the warning and the record cannot disagree.
    warn_on_userns_restriction_disabled_host_wide(scope_report.userns_grant);
    // The non-fatal half of the reachability check above. A compiler driver's helper binaries
    // (`cc1`, `as`, `ld`, `collect2`) are exec'd by the driver itself and sit outside its own
    // DT_NEEDED closure, inside the fixed sealed tree that is deliberately bound without the
    // Landlock Execute right — so `cc` starts and the first real compile does not finish. This
    // warns rather than refuses because the probe behind it is a heuristic about one driver
    // family; see `reachability::warn_on_unreachable_toolchain_helpers`, which prints each
    // `W-SEC-012` line itself so `mur doctor` and this call site cannot state it differently.
    crate::reachability::warn_on_unreachable_toolchain_helpers(
        &request.capability_policy,
        request.declared_containment_floor,
    );

    let engine = build_engine()?;
    // Start ticking before the first guest runs: `dispatch_stage` below invokes on-stage
    // hooks, which are already subject to the epoch deadline.
    let epoch_ticker = EpochTicker::spawn(&engine);
    let lock_expectations = request.lock_expectations.map(|entries| {
        entries
            .into_iter()
            .map(|entry| (entry.name.clone(), entry))
            .collect::<HashMap<_, _>>()
    });

    let mut resolved_lock_artifacts = Vec::with_capacity(request.artifacts.len());
    let mut installed_artifacts = Vec::with_capacity(request.artifacts.len());
    let mut installed_manifests = Vec::with_capacity(request.artifacts.len());
    let mut tool_components = HashMap::with_capacity(request.artifacts.len());
    // Only tools/drivers that declare a `capabilities:` block get an entry; everything else
    // stays absent and therefore runs on the unclamped ceiling.
    let mut artifact_grants: HashMap<String, ToolCapabilityGrant> = HashMap::new();
    let mut hook_components = Vec::new();
    // The ceiling every per-artifact network grant is clamped against. Re-parsed here rather
    // than at launch because narrowing is lowered at staging time; `validate_capability_policy`
    // above already proved these entries parse.
    let ceiling_network_allow_rules =
        parse_network_allow_rules(&request.capability_policy.network_allow)?;
    // The capsule operator's own name for this capsule, borrowed for the length of the staging
    // loop below (which borrows `request.artifacts`). It is what an artifact entry's
    // `capabilities.state` defaults its store name to.
    let capsule_name = request.capsule_name.clone();
    // (name, binary_bytes) for native tool artifacts — installed after workdir creation
    let mut native_binaries: Vec<(String, Vec<u8>)> = Vec::new();
    // (name, skill_md_bytes) for skill artifacts — installed after workdir creation
    let mut skill_files: Vec<(String, Vec<u8>)> = Vec::new();

    for artifact in &request.artifacts {
        // Local-source skill: resolve skill.md directly from the filesystem and skip the
        // registry/lock pipeline entirely. Validated as skill-only at manifest parse time.
        if let Some(source) = &artifact.source {
            let skill_md = load_local_skill_md(&request.manifest_dir, source)?;
            skill_files.push((artifact.name.clone(), skill_md));
            installed_artifacts.push(InstalledArtifactSummary {
                name: artifact.name.clone(),
                version: artifact.version.clone(),
                runtime: artifact.runtime.clone(),
                implementation: None,
            });
            continue;
        }

        let (resolved_version, expected_hash) = match &lock_expectations {
            Some(expected_by_name) => {
                let expected = expected_by_name.get(&artifact.name).ok_or_else(|| {
                    RuntimeError::LockMissingEntry {
                        name: artifact.name.clone(),
                    }
                })?;

                if artifact.version != expected.resolved_version {
                    return Err(RuntimeError::LockVersionMismatch {
                        name: artifact.name.clone(),
                        requested: artifact.version.clone(),
                        pinned: expected.resolved_version.clone(),
                    });
                }

                (
                    expected.resolved_version.clone(),
                    Some(expected.sha256_wasm.clone()),
                )
            }
            None => (artifact.version.clone(), None),
        };

        // Always pass current_platform(). LocalRegistry and RemoteRegistry both implement a
        // fallback: if no platform-specific file exists they return the generic file. This
        // means WASM artifacts (which have no platform file) resolve transparently, while
        // native artifacts get their correct platform variant. Nexus must preserve this
        // fallback behaviour — if it ever becomes strict (error on no platform match), WASM
        // resolution will break.
        let resolved = registry
            .resolve_with_platform(&artifact.name, &resolved_version, Some(current_platform()))
            .map_err(|err| map_registry_error(&artifact.name, &resolved_version, err))?;

        verify_sha256(
            &artifact.name,
            &resolved_version,
            &resolved.bytes,
            &resolved.sha256,
        )
        .map_err(|_| RuntimeError::artifact_integrity_failed(&artifact.name, &resolved_version))?;

        if let Some(expected) = expected_hash {
            if resolved.sha256 != expected {
                return Err(RuntimeError::artifact_integrity_failed(
                    &artifact.name,
                    &resolved_version,
                ));
            }
        }

        let manifest_yaml =
            extract_manifest_yaml(&artifact.name, &resolved_version, &resolved.bytes)?;

        installed_manifests.push((artifact.name.clone(), manifest_yaml.clone()));

        let mut artifact_implementation: Option<ArtifactImplementation> = None;

        match artifact.runtime {
            ArtifactRuntime::Tool => {
                let implementation = parse_tool_implementation_from_yaml(&manifest_yaml);
                artifact_implementation = Some(implementation.clone());
                match implementation {
                    ArtifactImplementation::Native => {
                        // A native tool is a host subprocess, not a WASI guest: it never
                        // reaches `invoke_tool_component`, so nothing would apply a
                        // per-artifact grant to it. Say so rather than let the block read
                        // as enforced.
                        warn_on_unenforceable_native_capabilities(
                            &artifact.name,
                            artifact.capabilities.as_ref(),
                        );
                        let binary = extract_native_binary(
                            &artifact.name,
                            &resolved_version,
                            &resolved.bytes,
                        )?;
                        native_binaries.push((artifact.name.clone(), binary));
                    }
                    ArtifactImplementation::Wasm => {
                        stage_artifact_grant(
                            artifact,
                            &ceiling_network_allow_rules,
                            &capsule_name,
                            &mut artifact_grants,
                        )?;
                        let tool_wasm =
                            extract_root_wasm(&artifact.name, &resolved_version, &resolved.bytes)?;
                        let tool_component = Component::new(&engine, tool_wasm).map_err(|err| {
                            RuntimeError::ToolComponentCompile {
                                name: artifact.name.clone(),
                                version: resolved_version.clone(),
                                message: err.to_string(),
                            }
                        })?;
                        tool_components.insert(artifact.name.clone(), tool_component);
                    }
                }
            }
            ArtifactRuntime::Driver => {
                // Same call as the WASM-tool arm above, and deliberately so: a driver is
                // staged into `tool_components` and dispatched through
                // `invoke_tool_component` like any tool, so narrowing needs no
                // driver-specific enforcement anywhere downstream.
                stage_artifact_grant(
                    artifact,
                    &ceiling_network_allow_rules,
                    &capsule_name,
                    &mut artifact_grants,
                )?;
                let tool_wasm =
                    extract_root_wasm(&artifact.name, &resolved_version, &resolved.bytes)?;
                let tool_component = Component::new(&engine, tool_wasm).map_err(|err| {
                    RuntimeError::ToolComponentCompile {
                        name: artifact.name.clone(),
                        version: resolved_version.clone(),
                        message: err.to_string(),
                    }
                })?;
                tool_components.insert(artifact.name.clone(), tool_component);
            }
            ArtifactRuntime::Hook => {
                let hook_config = parse_hook_config_from_yaml(&manifest_yaml).map_err(|e| {
                    RuntimeError::Runtime(format!(
                        "hook {}@{} invalid config: {e}",
                        artifact.name, resolved_version
                    ))
                })?;
                let hook_wasm =
                    extract_root_wasm(&artifact.name, &resolved_version, &resolved.bytes)?;
                let hook_component = Component::new(&engine, hook_wasm).map_err(|err| {
                    RuntimeError::ToolComponentCompile {
                        name: artifact.name.clone(),
                        version: resolved_version.clone(),
                        message: err.to_string(),
                    }
                })?;
                // The grant comes from `artifact` — the operator's own manifest entry for
                // this hook — and never from `manifest_yaml`, the hook's bundled manifest
                // parsed just above for its behavioral contract. A hook pulled from a
                // registry therefore cannot widen what the host lets it do. Deriving here
                // (rather than at instantiation) means a malformed grant fails staging,
                // before any hook component runs.
                let mut grant =
                    HookCapabilityGrant::derive(artifact.capabilities.as_ref(), &capsule_name)?;
                // Same division as `stage_artifact_grant`: `derive` validated the name and stays
                // pure, and the directory is created here, on the staging path, once.
                if let Some(store) = grant.state_store.as_deref() {
                    grant.state_dir = Some(crate::state_store::ensure_state_store(store)?);
                }
                warn_on_inert_hook_capabilities(&artifact.name, artifact.capabilities.as_ref());
                hook_components.push(StagedHookArtifact {
                    name: artifact.name.clone(),
                    version: resolved_version.clone(),
                    component: hook_component,
                    config: hook_config,
                    grant,
                    // Operator-sourced like `grant`, for the same reason: how much of the
                    // agent's telemetry may be dropped to keep the loop moving is the
                    // operator's call, not the hook author's.
                    on_overflow: artifact.on_overflow,
                });
            }
            ArtifactRuntime::Skill => {
                let skill_md =
                    extract_skill_md(&artifact.name, &resolved_version, &resolved.bytes)?;
                skill_files.push((artifact.name.clone(), skill_md));
            }
        }

        resolved_lock_artifacts.push(ResolvedLockArtifact {
            name: artifact.name.clone(),
            resolved_version: resolved_version.clone(),
            sha256_wasm: resolved.sha256,
        });
        installed_artifacts.push(InstalledArtifactSummary {
            name: artifact.name.clone(),
            version: resolved_version,
            runtime: artifact.runtime.clone(),
            implementation: artifact_implementation,
        });
    }

    // For agent capsules (inference configured, empty WASM bytes) skip component compilation.
    // For script capsules, compile the WASM component.
    let capsule_component =
        if request.inference.is_some() && request.capsule_component_bytes.is_empty() {
            None
        } else if request.capsule_component_bytes.is_empty() {
            return Err(RuntimeError::CapsuleCompile(
                "capsule component bytes are required for non-agent capsules".to_string(),
            ));
        } else {
            Some(
                Component::new(&engine, &request.capsule_component_bytes)
                    .map_err(|err| RuntimeError::CapsuleCompile(err.to_string()))?,
            )
        };

    let session_id = generate_session_id();
    let (workdir, accessible_workdir) = if let Some(ref user_dir) = request.workdir {
        let user_dir = if user_dir.is_absolute() {
            user_dir.clone()
        } else {
            std::env::current_dir()
                .map_err(|e| RuntimeError::Runtime(format!("failed to get cwd: {e}")))?
                .join(user_dir)
        };
        if !user_dir.exists() || !user_dir.is_dir() {
            return Err(RuntimeError::Runtime(format!(
                "workdir '{}' does not exist or is not a directory",
                user_dir.display()
            )));
        }
        let session_dir = user_dir.join(".murmur").join(&session_id);
        (session_dir, user_dir)
    } else {
        let dir = request.manifest_dir.join("workdir").join(&session_id);
        let accessible = dir.clone();
        (dir, accessible)
    };
    // Before the workdir is created and before anything is staged into it: a declared export
    // whose root already resolves outside the accessible workdir must refuse the launch, not be
    // discovered one served file at a time.
    if let Some(ref export) = exports_files {
        crate::resource_plane::check_export_root(&accessible_workdir, export)?;
    }
    if let Some(ref export) = exports_peer_files {
        crate::resource_plane::check_peer_files_root(&accessible_workdir, export)?;
    }

    fs::create_dir_all(workdir.join("tools")).map_err(|source| RuntimeError::CreateWorkdir {
        path: workdir.display().to_string(),
        source,
    })?;

    for (name, manifest_yaml) in installed_manifests {
        write_tool_manifest(&workdir, &name, &manifest_yaml)?;
    }

    install_native_binaries(&workdir, native_binaries)?;
    install_skill_files(&workdir, skill_files)?;

    // Write generic manifests for any shell binary not already covered by a custom manifest.
    write_shell_tool_manifests(&workdir, &request.capability_policy.shell_allow)?;

    // The two peer-handoff tools, written on exactly the terms the shell manifests above are:
    // a synthetic `tools/<name>/murmur.yaml` that `build_tool_inventory` picks up unchanged,
    // paired with a dispatch branch in `dispatch_agent_tool_async`. Each is written **only** when
    // its grant is declared, so an undeclared capsule's model never sees the tool exists.
    write_peer_handoff_tool_manifests(
        &workdir,
        exports_peer_files.is_some(),
        !request.capability_policy.peer_fetch_allow.is_empty(),
    )?;

    // Dispatch on-stage hooks synchronously now that manifests are in place.
    let stage_env = HookEnvVars::default();
    dispatch_stage(
        &engine,
        &workdir,
        &hook_components,
        request.capability_policy.shell_allow.clone(),
        &stage_env,
        request.capability_policy.hook_limits(),
    )?;

    // Write MURMUR.md now for agent sessions so that tooling that inspects the workdir after
    // staging (e.g. tests, debuggers) can read it. launch_session overwrites this file after
    // port binding with the complete identity including capsule_url.
    if let Some(ref inference) = request.inference {
        let partial_identity = CapsuleIdentity {
            capsule_name: request.capsule_name.clone(),
            capsule_version: request.capsule_version.clone(),
            session_id: session_id.clone(),
            capsule_url: String::new(),
        };
        murmur_md::write_murmur_md(
            &workdir,
            Some(inference),
            request.context.as_ref(),
            has_compaction_hook(&hook_components),
            &request.capability_policy,
            &partial_identity,
        );
    }

    // When --workdir is used, copy the project manifest to the accessible workdir so the agent
    // can reference it by relative path from ".".
    if request.workdir.is_some() {
        let src = request.manifest_dir.join(MANIFEST_FILENAME);
        let dst = accessible_workdir.join(MANIFEST_FILENAME);
        if src.exists() && !dst.exists() {
            if let Err(e) = fs::copy(&src, &dst) {
                eprintln!(
                    "[capsule-runtime] warning: failed to copy {MANIFEST_FILENAME} to workdir: {e}"
                );
            }
        }
    }

    // Resolve lifecycle: manifest config + any override
    let lifecycle = resolve_lifecycle(request.lifecycle, request.lifecycle_override.as_ref());

    Ok(StagedSession {
        session_id,
        workdir,
        accessible_workdir,
        manifest_dir: request.manifest_dir,
        capsule_name: request.capsule_name,
        capsule_version: request.capsule_version,
        capsule_url: String::new(), // set by launch_session after port binding
        resolved_lock_artifacts,
        installed_artifacts,
        inference: request.inference,
        system_prompt_overridden: request.system_prompt_overridden,
        context: request.context,
        engine,
        capsule_component,
        tool_components,
        artifact_grants,
        hook_components,
        allowlisted_tools: request.allowlisted_tools,
        // The combined floor (manifest + workspace config + `--containment`) replaces the
        // manifest-only value the policy was built with, so every later reader — including
        // `ShellEnforcement::resolve`, which decides whether this session installs a composed
        // root — sees the class that was actually asked for.
        capability_policy: CapabilityPolicy {
            containment_floor: request.declared_containment_floor,
            ..request.capability_policy
        },
        otel_endpoint: request.otel_endpoint,
        eval_config_json: request.eval_config_json,
        case_id: request.case_id,
        dataset_id: request.dataset_id,
        lifecycle,
        trace_include_tool_output: request
            .trace
            .as_ref()
            .map(|t| t.include_tool_output)
            .unwrap_or(false),
        bind_addr: request.bind_addr,
        internal_port: request.internal_port,
        declared_containment_floor: request.declared_containment_floor,
        scope_report,
        exports_files,
        exports_peer_files,
        registry,
        _epoch_ticker: epoch_ticker,
    })
}

/// Instantiates the staged capsule and executes it.
///
/// Agent capsules (inference configured) run the built-in native Rust loop.
/// Script capsules (WASM component present) instantiate and call `murmur:capsule/run#run()`.
pub fn launch_session(
    mut staged: StagedSession,
    on_url: impl FnOnce(&str),
) -> Result<LaunchResult, RuntimeError> {
    let network_allow_rules = parse_network_allow_rules(&staged.capability_policy.network_allow)?;

    // --- Host-process bounding, before any WASM is instantiated ------------------------------
    //
    // A capsule that can reach a native subprocess by any route (`shell.allow`, `spawn.allow`,
    // or a native-implementation artifact) needs a cgroup scope around that process tree. On
    // Linux, failing to get one is fatal here — refusing the launch is strictly better than
    // running the tree with no aggregate memory/pids/cpu ceiling, and it must happen before
    // instantiation so no subprocess is ever spawned unbounded. Off Linux there is no cgroup to
    // get, so `prepare_scope` returns `None` and the gap is reported as `W-SEC-010` instead.
    let has_native_artifact = staged.installed_artifacts.iter().any(|artifact| {
        matches!(
            artifact.implementation,
            Some(murmur_artifact::ArtifactImplementation::Native)
        )
    });
    let requires_process_bounding =
        cgroup::requires_process_bounding(&staged.capability_policy, has_native_artifact);
    let cgroup_scope = cgroup::prepare_scope(
        requires_process_bounding,
        &staged.capability_policy.resources,
        &staged.session_id,
        &staged.workdir,
    )
    .map_err(|reason| RuntimeError::CgroupDelegationUnavailable { reason })?;
    let workdir_guard = Some(resources::WorkdirGuard::spawn(
        &staged.workdir,
        staged.capability_policy.resources.workdir_max_bytes,
    ));

    let shell_enforcement = sandbox::ShellEnforcement::resolve(
        &staged.capability_policy,
        staged.declared_containment_floor,
    )
    .map_err(RuntimeError::Runtime)?
    .with_host_bounding(cgroup_scope, workdir_guard);
    let inference_env = staged
        .inference
        .as_ref()
        .map(inference_env_pairs)
        .unwrap_or_default();

    if let Some(ref inference) = staged.inference {
        let workdir = staged.workdir.clone();
        let system_prompt = resolve_system_prompt(&staged.manifest_dir, &workdir, inference)?;
        let compaction_system_prompt =
            resolve_compaction_system_prompt(&staged.manifest_dir, inference.compaction.as_ref())?;
        let session_id = staged.session_id.clone();
        let accessible_workdir = staged.accessible_workdir.clone();
        let context_window = resolve_context_window(staged.context.as_ref());

        let run_config = agent::AgentRunConfig {
            context_window,
            compaction_threshold: inference
                .compaction
                .as_ref()
                .and_then(|c| c.threshold)
                .unwrap_or(0.98),
            compaction_model: inference.compaction.as_ref().and_then(|c| c.model.clone()),
            compaction_system_prompt,
            compaction_dump_summaries: inference
                .compaction
                .as_ref()
                .and_then(|c| c.dump_summaries)
                .unwrap_or(false),
            max_output_tokens: inference
                .max_tokens
                .unwrap_or(agent::DEFAULT_MAX_OUTPUT_TOKENS),
        };

        // --- Identity and HTTP server setup ---

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::Runtime(format!("failed to create tokio runtime: {e}")))?;

        let (tcp_listener, external_port) = rt.block_on(identity::bind_local_port(
            &staged.bind_addr,
            staged.internal_port,
        ))?;

        let capsule_url = format!("localhost:{external_port}");
        staged.capsule_url = capsule_url.clone();
        on_url(&capsule_url);

        let capsule_identity = CapsuleIdentity {
            capsule_name: staged.capsule_name.clone(),
            capsule_version: staged.capsule_version.clone(),
            session_id: session_id.clone(),
            capsule_url: capsule_url.clone(),
        };

        // Inject identity env vars into all WASI contexts for this session.
        let mut all_env = inference_env.clone();
        all_env.push((
            "MURMUR_CAPSULE_NAME".to_string(),
            staged.capsule_name.clone(),
        ));
        all_env.push((
            "MURMUR_CAPSULE_VERSION".to_string(),
            staged.capsule_version.clone(),
        ));
        all_env.push(("MURMUR_SESSION_ID".to_string(), session_id.clone()));
        all_env.push(("MURMUR_CAPSULE_URL".to_string(), capsule_url.clone()));

        murmur_md::write_murmur_md(
            &workdir,
            Some(inference),
            staged.context.as_ref(),
            has_compaction_hook(&staged.hook_components),
            &staged.capability_policy,
            &capsule_identity,
        );

        sandbox::warn_for_enforcement_tier(
            shell_enforcement.tier,
            &workdir,
            &staged.capability_policy,
        );
        sandbox::warn_for_missing_aggregate_bounding(
            &workdir,
            requires_process_bounding,
            shell_enforcement.cgroup_scope.is_some(),
        );

        let agent_card = identity::build_agent_card(
            &capsule_identity,
            &staged.installed_artifacts,
            &staged.capability_policy,
        );
        let agent_card_json = agent_card.to_string();

        // --- Lifecycle config ---
        let effective_lifecycle = staged.lifecycle.clone();
        let conversation_mode = effective_lifecycle.conversation_mode.clone();
        let queue_capacity = match effective_lifecycle.task_acceptance {
            TaskAcceptance::Queue => effective_lifecycle.queue_depth,
            _ => 1,
        };

        // --- A2A task registry and incoming channel ---
        let task_registry: Arc<Mutex<TaskRegistry>> = Arc::new(Mutex::new(TaskRegistry::new(
            effective_lifecycle.queue_depth,
            effective_lifecycle.task_acceptance.clone(),
        )));
        let (task_tx, mut task_rx) = tokio::sync::mpsc::channel::<IncomingTask>(queue_capacity);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // SSE broadcast channel and replay buffer for SSE clients
        let (sse_tx, _) =
            tokio::sync::broadcast::channel::<std::sync::Arc<String>>(SSE_BROADCAST_CAPACITY);
        let sse_buffer = std::sync::Arc::new(Mutex::new(SseEventBuffer::new(SSE_REPLAY_CAPACITY)));

        let capsule_name = staged.capsule_name.clone();
        let capabilities = capability_names(&staged.capability_policy);
        // Computed once at stage time, not re-derived here: `session_start` records what this
        // session ran with, and the staged report is the single place that value already lives.
        // It also carries the declared/achieved classes and `workdir_exec` the event's own
        // top-level fields are written from.
        let effective_grants = staged.scope_report.clone();
        // The resource plane is built from a host path, a declared export, an achieved class, a
        // counter and a trace handle — nothing the agent loop owns and nothing a completed task
        // leaves behind. That is what a later reader-only launch mode over an existing workdir
        // would need, and no more.
        let exports_files = staged.exports_files.clone();
        let resource_containment = staged.scope_report.achieved_containment;
        let resource_accessible_workdir = accessible_workdir.clone();

        // The peer plane's minting key: 32 random bytes, generated here and only when
        // `exports.peer_files` is declared, held in memory for this session and destroyed with it.
        // Never written to disk and never placed in an environment variable — teardown is the
        // revocation mechanism, so there must be nothing left to reload.
        let exports_peer_files = staged.exports_peer_files.clone();
        let peer_mint_key = match exports_peer_files {
            Some(_) => Some(std::sync::Arc::new(
                crate::peer_handoff::PeerMintKey::generate().map_err(RuntimeError::Runtime)?,
            )),
            None => None,
        };
        // Both sides derive the audience from the fetching capsule's own advertised identity, so
        // what this capsule asserts on a redeem it issues is the same string a peer would read
        // off the card it publishes.
        let own_peer_audience = crate::peer_handoff::own_audience(&capsule_identity);
        let peer_fetch_rules =
            parse_network_allow_rules(&staged.capability_policy.peer_fetch_allow)?;

        // Capture staged fields that move into the async block
        let hook_components = staged.hook_components;
        let tool_components = staged.tool_components;
        let artifact_grants = staged.artifact_grants;
        let allowlisted_tools = staged.allowlisted_tools.clone();
        let installed_artifacts = staged.installed_artifacts;
        let engine = staged.engine.clone();
        let capability_policy = staged.capability_policy.clone();
        let shell_enforcement_for_state = shell_enforcement.clone();
        let otel_endpoint = staged.otel_endpoint;
        let eval_config_json = staged.eval_config_json;
        let case_id = staged.case_id;
        let dataset_id = staged.dataset_id;
        let capsule_version = staged.capsule_version.clone();
        let inference_model = inference.model.clone();
        // Non-empty only for the WASM-driver transport; a `transport: process`
        // capsule has no driver artifact, so a hook's `run-inference` correctly
        // reports "no driver configured" rather than silently doing nothing.
        let inference_driver_name = inference
            .driver
            .as_ref()
            .map(|d| d.artifact.clone())
            .filter(|a| !a.is_empty());
        let trace_include_tool_output = staged.trace_include_tool_output;
        // The trace records the *resolved* prompt — what `resolve_system_prompt` returned, before
        // `build_augmented_system_prompt` prepends the `[Capsule]` block — so a reader compares
        // what the manifest (or `--system-prompt`) actually said, not the runtime's framing of it.
        let trace_system_prompt = system_prompt.clone();
        let system_prompt_overridden = staged.system_prompt_overridden;
        let registry_for_pull = Arc::clone(&staged.registry);
        let lock_path_for_pull = staged.manifest_dir.join("murmur.lock");

        // task.md must live where the agent's own tools are preopened (accessible_workdir),
        // not the internal `.murmur/<session_id>` bookkeeping dir (workdir) — otherwise this
        // pre-seed check misses a `--task`-written file and the agent's own read of task.md
        // 404s even after a task was delivered.
        let workdir_task_md = accessible_workdir.join("task.md");

        // Extra copies retained for the LaunchResult returned after block_on consumes the others.
        let session_id_ret = session_id.clone();
        let workdir_ret = workdir.clone();

        // --- Agent loop inside a LocalSet (handles !Send WasiCtx) ---
        let loop_result: Result<(), RuntimeError> = rt.block_on(async move {
            let mut trace = TraceWriter::open(
                &workdir,
                session_id.clone(),
                capsule_name.clone(),
                capsule_version.clone(),
                inference_model.clone(),
                capabilities.clone(),
                effective_grants,
                trace_include_tool_output,
                trace_system_prompt,
                system_prompt_overridden,
            )
            .await
            .map_err(|e| RuntimeError::AgentLoopFailed(format!("failed to open trace.jsonl: {e}")))?;

            // A second handle to the same trace.jsonl, not a borrow of the writer above: the
            // motivating read happens after `session_end`, when the agent loop's writer is gone.
            // A trace that cannot be opened must not make the plane unserveable — the read is
            // still refused or served correctly, it is only unrecorded.
            let resource_trace = crate::trace::ResourceTraceAppender::open(
                &workdir,
                session_id.clone(),
            )
            .await
            .ok()
            .map(std::sync::Arc::new);
            let resource_plane = std::sync::Arc::new(crate::resource_plane::ResourcePlane::new(
                &resource_accessible_workdir,
                exports_files.as_ref(),
                resource_containment,
                task_registry.lock().unwrap().resource_generation(),
                resource_trace.clone(),
            ));
            // Always built, declared or not: an undeclared capsule still has to record the redeem
            // it refused, and its declared half is `None` only because there is nothing to serve.
            // The key exists exactly when the export does — both come from the same declaration.
            let peer_plane = std::sync::Arc::new(crate::peer_handoff::PeerPlane::new(
                &resource_accessible_workdir,
                exports_peer_files
                    .as_ref()
                    .zip(peer_mint_key.as_ref())
                    .map(|(export, key)| (export, std::sync::Arc::clone(key))),
                session_id.clone(),
                resource_containment,
                task_registry.lock().unwrap().resource_generation(),
                resource_trace.clone(),
            ));

            let mut otel = OtelEmitter::new(
                otel_endpoint.clone(),
                &workdir,
                capsule_name.clone(),
                capsule_version.clone(),
            );

            let local = tokio::task::LocalSet::new();
            local
                .run_until(async move {
                    // Spawn HTTP server as a local task (uses spawn_local for !Send compat)
                    let server_handle =
                        tokio::task::spawn_local(identity::serve_http(
                            tcp_listener,
                            shutdown_rx,
                            agent_card_json,
                            Arc::clone(&task_registry),
                            task_tx,
                            effective_lifecycle.task_acceptance.clone(),
                            sse_tx.clone(),
                            Arc::clone(&sse_buffer),
                            conversation_mode.clone(),
                            std::sync::Arc::clone(&resource_plane),
                            std::sync::Arc::clone(&peer_plane),
                        ));

                    // Read before `capability_policy` moves into the store state below. Hooks
                    // get the same resource caps as every other guest but their own, lower
                    // deadline default — see `CapabilityPolicy::hook_limits`.
                    let hook_limits = capability_policy.hook_limits();

                    // Build CapsuleStoreState ONCE — reused across all task iterations
                    let mut state = CapsuleStoreState {
                        // Agent capsules have no WASM component of their own, so this state
                        // never backs a `Store` and this limiter is never registered. It is
                        // the per-tool limiters built in `dispatch_tool_async` (from
                        // `capability_policy.limits`) that bound this path's guests.
                        limits: capability_policy.limits.limiter(),
                        table: ResourceTable::new(),
                        // The capsule's own store is the ceiling itself, never narrowed:
                        // per-artifact grants apply to staged tools/drivers, not to the
                        // agent loop's own context.
                        wasi: build_wasi_ctx(
                            &accessible_workdir,
                            None,
                            // Nor does a state store: it too is granted per artifact, and the
                            // capsule holds no artifact grant.
                            None,
                            &all_env,
                            &capability_policy,
                        )?,
                        http: WasiHttpCtx::new(),
                        http_hooks: NetworkPolicyHooks {
                            network_allow_rules: network_allow_rules.clone(),
                        },
                        network_allow_rules,
                        peer_fetch_rules,
                        peer_plane: Some(std::sync::Arc::clone(&peer_plane)),
                        peer_own_audience: own_peer_audience,
                        peer_trace: resource_trace,
                        inference_env: all_env,
                        engine: engine.clone(),
                        workdir: workdir.clone(),
                        accessible_workdir: accessible_workdir.clone(),
                        tool_components,
                        artifact_grants,
                        allowlisted_tools,
                        installed_artifacts,
                        session_id: session_id.clone(),
                        pending_a2a_events: Vec::new(),
                        capability_policy,
                        shell_enforcement: shell_enforcement_for_state,
                        current_traceparent: None,
                        a2a_task_registry: Some(Arc::clone(&task_registry)),
                        a2a_sse: Some((sse_tx.clone(), Arc::clone(&sse_buffer))),
                        a2a_task_id: None,
                        input_timeout_secs: effective_lifecycle.input_timeout_secs,
                        a2a_chunk_event_id: Arc::new(AtomicU64::new(u64::MAX / 4)),
                        a2a_chunks_emitted: Arc::new(AtomicBool::new(false)),
                        registry: registry_for_pull,
                        lock_path: lock_path_for_pull,
                        driver_continuation_id: None,
                        driver_continuation_context_id: None,
                        driver_continuation_acked_len: 0,
                    };

                    // Backing for the hooks' `murmur:runtime/inference` import.
                    // Sourced from `state` (built directly above) so a hook's
                    // `run-inference` runs the *same* driver component, under the
                    // same capability policy and network allowlist, as the agent
                    // loop's own turns. `None` when no driver artifact is staged.
                    let hook_inference = inference_driver_name
                        .as_ref()
                        .and_then(|driver_name| {
                            state
                                .tool_components
                                .get(driver_name)
                                .map(|component| (driver_name.clone(), component.clone()))
                        })
                        .map(|(driver_name, driver_component)| {
                            // Same grant `dispatch_tool_async` would apply to this driver, so
                            // a hook's `run-inference` cannot route around its narrowing.
                            let driver_grant = state.artifact_grants.get(&driver_name).cloned();
                            Arc::new(HookInferenceCtx {
                                driver_name,
                                driver_component,
                                model: inference_model.clone(),
                                engine: state.engine.clone(),
                                accessible_workdir: state.accessible_workdir.clone(),
                                inference_env: state.inference_env.clone(),
                                capability_policy: state.capability_policy.clone(),
                                network_allow_rules: state.network_allow_rules.clone(),
                                driver_grant,
                                records: std::sync::Mutex::new(Vec::new()),
                            })
                        });

                    // Create hooks ONCE — session_start fires once per capsule lifetime
                    let mut hooks = HookRuntime::new(
                        &engine,
                        &workdir,
                        &accessible_workdir,
                        hook_components,
                        SessionContextData {
                            capsule_name: capsule_name.clone(),
                            capsule_version: capsule_version.clone(),
                            session_id: session_id.clone(),
                            model: inference_model.clone(),
                            capabilities: capabilities.clone(),
                        },
                        HookEnvVars {
                            otel_endpoint: otel_endpoint.as_deref(),
                            eval_config_json: eval_config_json.as_deref(),
                            case_id: case_id.as_deref(),
                            dataset_id: dataset_id.as_deref(),
                        },
                        hook_limits,
                        hook_inference,
                    )
                    .await?;

                    // on-session-start fires ONCE per launch, before the task loop —
                    // regardless of task_acceptance. For queue capsules this is the single
                    // session boundary that the per-task on-task-start events nest inside.
                    hooks.emit(&workdir, HookEvent::SessionStart).await;

                    let final_loop_result: Result<(), RuntimeError>;

                    // ── LOOP BODY STARTS HERE ──────────────────────────────
                    // Each iteration processes one task. Single/none modes break after
                    // the first iteration; queue+sleep iterates until channel closes.
                    'task_loop: loop {
                        // ── WAIT FOR NEXT TASK ──
                        let incoming: IncomingTask = match effective_lifecycle.task_acceptance {
                            TaskAcceptance::None => {
                                // Does not accept incoming tasks; run from task.md if present
                                if workdir_task_md.exists() {
                                    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
                                    let context_id = format!("ctx_{}", uuid::Uuid::now_v7().simple());
                                    let bytes = tokio::fs::metadata(&workdir_task_md)
                                        .await
                                        .map(|m| m.len())
                                        .unwrap_or(0);
                                    let _ = trace
                                        .write_task_start(
                                            &task_id, &context_id, "task_md", bytes,
                                        )
                                        .await;
                                    hooks
                                        .emit(
                                            &workdir,
                                            HookEvent::TaskStart {
                                                task_id: task_id.clone(),
                                                context_id: context_id.clone(),
                                                source: "task_md".to_string(),
                                                input_bytes: bytes,
                                            },
                                        )
                                        .await;
                                    otel.begin_session(None);
                                    state.current_traceparent = otel.outgoing_traceparent();
                                    // run_task_with_reopens fires on-task-end, honors any
                                    // reopen-task within budget, and writes the terminal
                                    // task_end (with reopen_count) itself.
                                    let result = run_task_with_reopens(
                                        &mut state,
                                        &workdir,
                                        inference,
                                        effective_lifecycle.max_task_reopens,
                                        system_prompt,
                                        run_config,
                                        &mut hooks,
                                        &mut trace,
                                        &mut otel,
                                        None,
                                        None,
                                        &accessible_workdir,
                                        &capsule_name,
                                        &capsule_version,
                                        conversation_mode.clone(),
                                        Some(context_id.clone()),
                                        &task_id,
                                    )
                                    .await;
                                    final_loop_result = result;
                                    break 'task_loop;
                                } else {
                                    final_loop_result = Ok(());
                                    break 'task_loop;
                                }
                            }
                            TaskAcceptance::Single | TaskAcceptance::Queue => {
                                if workdir_task_md.exists() {
                                    // Backward compat: existing task.md → single run, no A2A
                                    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
                                    let context_id = format!("ctx_{}", uuid::Uuid::now_v7().simple());
                                    let bytes = tokio::fs::metadata(&workdir_task_md)
                                        .await
                                        .map(|m| m.len())
                                        .unwrap_or(0);
                                    let _ = trace
                                        .write_task_start(
                                            &task_id, &context_id, "task_md", bytes,
                                        )
                                        .await;
                                    hooks
                                        .emit(
                                            &workdir,
                                            HookEvent::TaskStart {
                                                task_id: task_id.clone(),
                                                context_id: context_id.clone(),
                                                source: "task_md".to_string(),
                                                input_bytes: bytes,
                                            },
                                        )
                                        .await;
                                    otel.begin_session(None);
                                    state.current_traceparent = otel.outgoing_traceparent();
                                    let result = run_task_with_reopens(
                                        &mut state,
                                        &workdir,
                                        inference,
                                        effective_lifecycle.max_task_reopens,
                                        system_prompt.clone(),
                                        run_config.clone(),
                                        &mut hooks,
                                        &mut trace,
                                        &mut otel,
                                        None,
                                        None,
                                        &accessible_workdir,
                                        &capsule_name,
                                        &capsule_version,
                                        conversation_mode.clone(),
                                        Some(context_id.clone()),
                                        &task_id,
                                    )
                                    .await;
                                    let _ = trace.flush().await;
                                    if matches!(effective_lifecycle.task_acceptance, TaskAcceptance::Single) || result.is_err() {
                                        final_loop_result = result;
                                        break 'task_loop;
                                    }
                                    // Queue mode: remove task.md so the next iteration
                                    // falls through to task_rx.recv() for queued subtasks.
                                    let _ = tokio::fs::remove_file(&workdir_task_md).await;
                                    continue 'task_loop;
                                }
                                // Wait for the next task from the mpsc channel.
                                // queue+sleep mode waits indefinitely — no self-terminating
                                // timeout. The host (mur-roost) is responsible for shutdown.
                                // All other modes apply MURMUR_A2A_TIMEOUT_SECS (default 30 s).
                                let is_queue_sleep =
                                    matches!(effective_lifecycle.task_acceptance, TaskAcceptance::Queue)
                                    && matches!(effective_lifecycle.after_task, AfterTask::Sleep);

                                if is_queue_sleep {
                                    match task_rx.recv().await {
                                        Some(task) => task,
                                        None => {
                                            final_loop_result = Ok(());
                                            break 'task_loop;
                                        }
                                    }
                                } else {
                                    let idle_timeout_secs: u64 =
                                        std::env::var("MURMUR_A2A_TIMEOUT_SECS")
                                            .ok()
                                            .and_then(|v| v.parse().ok())
                                            .unwrap_or(30);
                                    match tokio::time::timeout(
                                        std::time::Duration::from_secs(idle_timeout_secs),
                                        task_rx.recv(),
                                    )
                                    .await
                                    {
                                        Ok(Some(task)) => task,
                                        Ok(None) => {
                                            final_loop_result = Ok(());
                                            break 'task_loop;
                                        }
                                        Err(_elapsed) => {
                                            eprintln!("[capsule-runtime] no A2A message received within timeout; running with empty task");
                                            otel.begin_session(None);
                                            state.current_traceparent = otel.outgoing_traceparent();
                                            final_loop_result = agent::run_agent_loop(
                                                &mut state,
                                                &workdir,
                                                inference,
                                                system_prompt,
                                                run_config,
                                                &mut hooks,
                                                &mut trace,
                                                &mut otel,
                                                None,
                                                None,
                                                &accessible_workdir,
                                                &capsule_name,
                                                &capsule_version,
                                                conversation_mode.clone(),
                                                None,
                                            )
                                            .await;
                                            break 'task_loop;
                                        }
                                    }
                                }
                            }
                        };

                        // ── ACTIVATE TASK ──
                        {
                            let mut reg = task_registry.lock().unwrap();
                            reg.start_task(incoming.task_id.clone(), incoming.context_id.clone());
                        }
                        if let Err(e) =
                            tokio::fs::write(&workdir_task_md, &incoming.message_text).await
                        {
                            eprintln!(
                                "[capsule-runtime] failed to write A2A message to task.md: {e}"
                            );
                        }
                        let _ = trace
                            .write_a2a_task_received(
                                &incoming.task_id,
                                &incoming.context_id,
                                &incoming.message_id,
                                incoming.traceparent.as_deref(),
                            )
                            .await;
                        let _ = trace
                            .write_task_start(
                                &incoming.task_id,
                                &incoming.context_id,
                                "a2a",
                                incoming.message_text.len() as u64,
                            )
                            .await;
                        hooks
                            .emit(
                                &workdir,
                                HookEvent::TaskStart {
                                    task_id: incoming.task_id.clone(),
                                    context_id: incoming.context_id.clone(),
                                    source: "a2a".to_string(),
                                    input_bytes: incoming.message_text.len() as u64,
                                },
                            )
                            .await;

                        // ── RUN AGENT LOOP ──
                        otel.begin_session(incoming.traceparent.as_deref());
                        state.current_traceparent = otel.outgoing_traceparent();
                        state.a2a_task_id = Some(incoming.task_id.clone());
                        let loop_result = run_task_with_reopens(
                            &mut state,
                            &workdir,
                            inference,
                            effective_lifecycle.max_task_reopens,
                            system_prompt.clone(),
                            run_config.clone(),
                            &mut hooks,
                            &mut trace,
                            &mut otel,
                            Some(incoming.task_id.clone()),
                            Some((sse_tx.clone(), Arc::clone(&sse_buffer))),
                            &accessible_workdir,
                            &capsule_name,
                            &capsule_version,
                            conversation_mode.clone(),
                            Some(incoming.context_id.clone()),
                            &incoming.task_id,
                        )
                        .await;

                        // ── POST-LOOP SLOT UPDATE ──
                        // task_end (with reopen_count) and the terminal on-task-end dispatch
                        // already happened inside run_task_with_reopens; an exhausted reopen
                        // budget surfaces here as loop_result.is_err(), i.e. a failed task.
                        let exit_state = if loop_result.is_ok() {
                            TaskState::Completed
                        } else {
                            TaskState::Failed
                        };
                        let _ = trace.flush().await;
                        {
                            let mut reg = task_registry.lock().unwrap();
                            reg.finish_task(exit_state);
                            // Immediately after the terminal state, so a resource-plane read that
                            // lands next reports the turn these bytes belong to.
                            reg.advance_resource_generation();
                        }

                        // ── DECIDE WHETHER TO CONTINUE ──
                        match effective_lifecycle.after_task {
                            AfterTask::Exit => {
                                final_loop_result = loop_result;
                                break 'task_loop;
                            }
                            AfterTask::Sleep => {
                                if matches!(
                                    effective_lifecycle.task_acceptance,
                                    TaskAcceptance::Single
                                ) {
                                    // single mode always exits after one task
                                    final_loop_result = loop_result;
                                    break 'task_loop;
                                }
                                // Queue+sleep: clear task.md and wait for next task
                                let _ = tokio::fs::remove_file(&workdir_task_md).await;
                                continue 'task_loop;
                            }
                        }
                    }
                    // ── LOOP BODY ENDS HERE ────────────────────────────────

                    // on-session-end fires ONCE per launch, after the task loop exits.
                    // total_turns is the whole-launch aggregate accumulated by HookRuntime
                    // (one per Inference event across every task), and exit_status reflects
                    // the loop's final result.
                    let session_exit_status =
                        if final_loop_result.is_ok() { "ok" } else { "failed" };
                    let session_total_turns = hooks.total_turns();
                    hooks
                        .emit(
                            &workdir,
                            HookEvent::SessionEnd {
                                total_turns: session_total_turns,
                                exit_status: session_exit_status.to_string(),
                            },
                        )
                        .await;

                    // Every async hook's queue is drained — and its worker awaited — while the
                    // `LocalSet` is still alive. Without this, `run_until` returning would drop
                    // the workers mid-call and take the session-end export with them. Bounded,
                    // so a wedged hook delays the exit by at most the drain budget; whatever it
                    // did not finish is reported through the same fault path as any other hook
                    // fault, which is why this runs *before* the flush below.
                    hooks.drain_async_hooks().await;
                    agent::flush_hook_dispatch_faults(&mut hooks, &mut trace).await;
                    let _ = trace.write_session_end_if_not_ended("failed").await;
                    otel.emit_session_end_if_not_ended("failed").await;
                    trace.flush().await.map_err(|e| {
                        RuntimeError::AgentLoopFailed(format!("failed to flush trace: {e}"))
                    })?;

                    // Signal HTTP server to shut down, then wait for it
                    let _ = shutdown_tx.send(());
                    let _ = server_handle.await;

                    final_loop_result
                })
                .await
        });

        loop_result?;

        return Ok(LaunchResult {
            session_id: session_id_ret,
            workdir: workdir_ret,
        });
    }

    // Script capsule path — requires a compiled WASM component.
    let capsule_component = staged
        .capsule_component
        .ok_or(RuntimeError::CapsuleCompile(
            "non-agent capsule has no compiled component".to_string(),
        ))?;

    let mut linker = Linker::new(&staged.engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;

    // `murmur:tool-registry/invoke@0.1.0` is the one host-provided interface a
    // capsule guest imports. Register it by hand against the same `invoke::Host`
    // impl on `CapsuleStoreState` (the generated `invoke::add_to_linker` would do
    // the same, but this keeps the registration alongside the tool-side ones).
    {
        let iface = WIT_TOOL_REGISTRY_IFACE;
        linker
            .instance(iface)
            .map_err(|err| RuntimeError::Runtime(err.to_string()))?
            .func_wrap(
                "invoke",
                |mut store: wasmtime::StoreContextMut<'_, CapsuleStoreState>,
                 (tool_name, input): (String, murmur::tool::run::ToolInput)|
                 -> wasmtime::Result<(Result<murmur::tool::run::ToolResult, String>,)> {
                    Ok((invoke::Host::invoke(store.data_mut(), tool_name, input),))
                },
            )
            .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    }
    manage::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;
    send::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
        .map_err(|err| RuntimeError::Runtime(err.to_string()))?;

    // Read before `staged.capability_policy` moves into the store state below.
    let capsule_limits = staged.capability_policy.limits;

    let state = CapsuleStoreState {
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(
            &staged.accessible_workdir,
            // The capsule component runs on the ceiling, not on any artifact's grant — so it gets
            // neither a narrowed workdir preopen nor a state preopen. A capsule cannot reach a
            // tool's store, by construction and not by convention: it holds no descriptor that
            // names one.
            None,
            None,
            &inference_env,
            &staged.capability_policy,
        )?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: network_allow_rules.clone(),
        },
        network_allow_rules,
        // A script capsule has no peer-handoff surface: `share-file` and `fetch-peer-file` are
        // agent-loop tools, and no WIT import exposes either to a wasm component. These are the
        // deny values rather than an omission — a future `murmur:peer-file` interface would fill
        // them here, from `staged.exports_peer_files` and `staged.capability_policy`.
        peer_fetch_rules: Vec::new(),
        peer_plane: None,
        peer_own_audience: String::new(),
        peer_trace: None,
        inference_env,
        engine: staged.engine.clone(),
        workdir: staged.workdir.clone(),
        accessible_workdir: staged.accessible_workdir.clone(),
        tool_components: staged.tool_components,
        artifact_grants: staged.artifact_grants,
        allowlisted_tools: staged.allowlisted_tools,
        installed_artifacts: staged.installed_artifacts,
        session_id: staged.session_id.clone(),
        pending_a2a_events: Vec::new(),
        capability_policy: staged.capability_policy,
        shell_enforcement: shell_enforcement.clone(),
        current_traceparent: None,
        a2a_task_registry: None,
        a2a_sse: None,
        a2a_task_id: None,
        input_timeout_secs: None,
        a2a_chunk_event_id: Arc::new(AtomicU64::new(u64::MAX / 4)),
        a2a_chunks_emitted: Arc::new(AtomicBool::new(false)),
        registry: Arc::clone(&staged.registry),
        lock_path: staged.manifest_dir.join("murmur.lock"),
        driver_continuation_id: None,
        driver_continuation_context_id: None,
        driver_continuation_acked_len: 0,
        limits: capsule_limits.limiter(),
    };

    let mut store = Store::new(&staged.engine, state);
    // Must precede instantiation: `Store::limiter` latches the instance/table/memory counts
    // the store enforces, and instantiation itself allocates against them.
    store.limiter(|state| &mut state.limits);

    // Script capsule uses a multi-thread runtime so block_in_place works inside send::Host::send
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| RuntimeError::Runtime(format!("failed to create tokio runtime: {e}")))?;

    rt.block_on(async {
        // Instantiation runs the guest's own start/init code, so it gets a deadline too.
        store.set_epoch_deadline(capsule_limits.deadline_ticks());
        let instantiated = linker
            .instantiate_async(&mut store, &capsule_component)
            .await;
        let instance = match instantiated {
            Ok(instance) => instance,
            Err(err) => return Err(capsule_guest_error(&err, &store.data().limits)),
        };

        let capsule_iface =
            resolve_versioned_iface(&instance, &mut store, WIT_CAPSULE_IFACE_VERSIONED)
                .ok_or(RuntimeError::CapsuleExportMissing)?;
        let capsule_run = instance
            .get_export_index(&mut store, Some(&capsule_iface), "run")
            .and_then(|idx| instance.get_func(&mut store, idx))
            .ok_or(RuntimeError::CapsuleExportMissing)?;

        let run = capsule_run
            .typed::<(), ()>(&store)
            .map_err(|err| RuntimeError::Runtime(err.to_string()))?;

        // Fresh budget for `run` itself, so instantiation cost cannot eat into it.
        store.set_epoch_deadline(capsule_limits.deadline_ticks());
        let called = run.call_async(&mut store, ()).await;
        if let Err(err) = called {
            return Err(capsule_guest_error(&err, &store.data().limits));
        }

        Ok(())
    })?;

    // Drain any buffered a2a_send trace events from the capsule run.
    let pending = std::mem::take(&mut store.data_mut().pending_a2a_events);
    if !pending.is_empty() {
        rt.block_on(async {
            if let Ok(mut trace) = TraceWriter::open(
                &staged.workdir,
                staged.session_id.clone(),
                staged.capsule_name.clone(),
                staged.capsule_version.clone(),
                String::new(),
                Vec::new(),
                staged.scope_report.clone(),
                false,
                None,
                false,
            )
            .await
            {
                for (peer_url, message_id, task_id, context_id, traceparent) in pending {
                    let _ = trace
                        .write_a2a_send(
                            &peer_url,
                            &message_id,
                            &task_id,
                            &context_id,
                            traceparent.as_deref(),
                        )
                        .await;
                }
                let _ = trace.flush().await;
            }
        });
    }

    // Notify the caller that the capsule has started (no URL for script capsules).
    on_url("");

    Ok(LaunchResult {
        session_id: staged.session_id,
        workdir: staged.workdir,
    })
}

/// Reads a manifest-relative prompt file, returning the resolved path alongside any I/O
/// error so the caller can map it to its own `RuntimeError` variant.
fn read_prompt_file(manifest_dir: &Path, path: &str) -> Result<String, (PathBuf, std::io::Error)> {
    let prompt_path = manifest_dir.join(path);
    fs::read_to_string(&prompt_path).map_err(|source| (prompt_path, source))
}

fn resolve_system_prompt(
    manifest_dir: &Path,
    workdir: &Path,
    inference: &murmur_artifact::InferenceConfig,
) -> Result<Option<String>, RuntimeError> {
    if let Some(prompt) = inference.system_prompt.as_ref() {
        return Ok(Some(prompt.clone()));
    }

    if let Some(path) = inference.system_prompt_file.as_ref() {
        return read_prompt_file(manifest_dir, path)
            .map(Some)
            .map_err(|(prompt_path, source)| RuntimeError::SystemPromptFileRead {
                path: prompt_path.display().to_string(),
                source,
            });
    }

    if let Some(art_name) = inference.system_prompt_artifact.as_ref() {
        let skill_path = workdir.join("tools").join(art_name).join("skill.md");
        return fs::read_to_string(&skill_path).map(Some).map_err(|source| {
            RuntimeError::SystemPromptArtifactRead {
                name: art_name.clone(),
                source,
            }
        });
    }

    Ok(None)
}

/// Resolves the compaction system prompt from the two mutually exclusive manifest
/// sources — the inline `inference.compaction.system_prompt` string, or the contents of
/// the file named by `inference.compaction.system_prompt_file`, read relative to the
/// manifest directory. Absence stays absence: no default prompt is substituted, and the
/// hook receives `option::none`.
///
/// Mutual exclusion is enforced at manifest parse time, so an inline prompt winning here
/// is unreachable for a manifest that loaded successfully.
fn resolve_compaction_system_prompt(
    manifest_dir: &Path,
    compaction: Option<&murmur_artifact::CompactionConfig>,
) -> Result<Option<String>, RuntimeError> {
    let Some(compaction) = compaction else {
        return Ok(None);
    };

    if let Some(prompt) = compaction.system_prompt.as_ref() {
        return Ok(Some(prompt.clone()));
    }

    if let Some(path) = compaction.system_prompt_file.as_ref() {
        return read_prompt_file(manifest_dir, path)
            .map(Some)
            .map_err(
                |(prompt_path, source)| RuntimeError::CompactionSystemPromptFileRead {
                    path: prompt_path.display().to_string(),
                    source,
                },
            );
    }

    Ok(None)
}

fn capability_names(policy: &CapabilityPolicy) -> Vec<String> {
    let mut names = Vec::new();
    if !policy.network_allow.is_empty() {
        names.push("network".to_string());
    }
    if policy.filesystem_scope.is_some() {
        names.push("filesystem".to_string());
    }
    if !policy.shell_allow.is_empty() {
        names.push("shell".to_string());
    }
    names
}

/// Core message for the bash+network bypass warning (finding C-7 in
/// `murmur-security-assessment.md`) — shared verbatim between the stderr line and the
/// `logs/bootstrap.log` line so the two never drift apart.
const BASH_NETWORK_BYPASS_WARNING: &str = "capabilities.shell.allow includes \"bash\" and \
capabilities.network.allow is non-empty, but network.allow does not constrain bash's own \
outbound connections on this platform.";

/// Warns (non-fatal — matches the warn-and-continue convention of `murmur_md::write_murmur_md`
/// and the murmur.yaml-copy warning above) when `policy.shell_allow` contains the exact binary
/// name `"bash"` and `policy.network_allow` is non-empty. The check is an exact match on the
/// literal `"bash"`, not `shell::is_shell_interpreter` (which also matches `sh`/`zsh`/`fish`/
/// `dash`/`ksh`) — a deliberate scope limit matching C-7's own scoping, not an oversight.
pub(crate) fn warn_if_bash_network_bypass(workdir: &Path, policy: &CapabilityPolicy) {
    let has_bash = policy.shell_allow.iter().any(|binary| binary == "bash");
    if has_bash && !policy.network_allow.is_empty() {
        let link = security_warning_link(W_SEC_003);
        eprintln!("[capsule-runtime] warning[{W_SEC_003}]: {BASH_NETWORK_BYPASS_WARNING} ({link})");
        agent::append_bootstrap_log(
            workdir,
            &format!(
                "[capability-policy] warning[{W_SEC_003}]: {BASH_NETWORK_BYPASS_WARNING} ({link})"
            ),
        );
    }
}

/// Warns (non-fatal, once per declared grant) for every `capabilities.shell.interpreter_runtime`
/// entry. Declaring one narrows an allowlisted binary's Landlock scope to specific host
/// directories so a path-based interpreter can reach its stdlib — but it couples the capsule to a
/// specific host distro/interpreter-version layout (e.g. `/usr/lib/python3.11` stops resolving the
/// moment the host ships Python 3.12), which the operator should see plainly. The durable fix is
/// the still-unbuilt staged runtime bind-mount; this grant only bridges until then.
///
/// Shared verbatim between `mur run` (from [`stage_session`]) and `mur doctor`, so both surface
/// the same code, wording, and doc link. Like [`warn_on_inert_hook_capabilities`], it fires before
/// any session workdir exists (the capsule-ceiling check runs at the top of `stage_session`, and
/// `doctor` never launches a session at all), so it goes to stderr only, not `logs/bootstrap.log`.
pub fn warn_on_interpreter_runtime_grants(grants: &[InterpreterRuntimeGrant]) {
    for grant in grants {
        let link = security_warning_link(W_SEC_009);
        let dirs = grant
            .dirs
            .iter()
            .map(|dir| {
                let list = if dir.list_dir { "list_dir" } else { "no-list" };
                format!("{} ({list})", dir.path)
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[capsule-runtime] warning[{W_SEC_009}]: capabilities.shell.interpreter_runtime grants \
             '{}' host directories outside the workdir [{dirs}] — this couples the capsule to a \
             specific host distro/interpreter-version layout (e.g. /usr/lib/python3.11 breaks the \
             moment the host ships Python 3.12); the durable fix is the staged runtime bind-mount, \
             which this grant only bridges until ({link})",
            grant.binary
        );
    }
}

/// Warns (non-fatal, once per session) when a capsule declares
/// `capabilities.filesystem.workdir_exec: true`.
///
/// This is the one grant that trades away an enforcement property rather than widening a scope:
/// with the workdir's Landlock `Execute` right granted, `capabilities.shell.allow` stops being
/// something the kernel can hold the capsule to — a binary it compiles, downloads or renames inside
/// its own workdir runs regardless. The declaration is legitimate (compile-and-run workflows need
/// it) and the class report already says `advisory`, but a class in a JSON field is easy to miss
/// and the reason for it is not self-evident, so it is also stated in words, once, at staging.
///
/// Shared shape with [`warn_on_interpreter_runtime_grants`]: fires before any session workdir
/// exists, so it goes to stderr only, not `logs/bootstrap.log`.
pub fn warn_on_workdir_exec(workdir_exec: bool) {
    if !workdir_exec {
        return;
    }
    let link = security_warning_link(W_SEC_011);
    eprintln!(
        "[capsule-runtime] warning[{W_SEC_011}]: capabilities.filesystem.workdir_exec is true — \
         the session workdir keeps its Landlock Execute right, so anything the capsule writes \
         there can run regardless of capabilities.shell.allow; this capsule reports containment \
         class 'advisory' on every host, including a Landlock-capable one ({link})"
    );
}

/// Warns (non-fatal, once per session) when this host's unprivileged user namespaces are
/// unrestricted because `kernel.apparmor_restrict_unprivileged_userns` is off, rather than because
/// the shipped `mur-sealed` AppArmor profile is confining this binary.
///
/// The two hosts back `sealed` and the capsule network namespace equally well, and neither is
/// refused. They differ in blast radius: the profile grants one binary permission to create a user
/// namespace, while the sysctl grants it to everything on the machine, which is the hardening
/// Ubuntu 23.10+ ships on precisely because unprivileged user namespaces are a recurring local
/// privilege-escalation surface. Both reach the same achieved class, so without this warning a
/// `sealed` result on a weakened host reads in the record exactly like one obtained through the
/// mechanism murmur ships.
///
/// Takes the probed grant rather than probing, so `mur run`'s staging path and `mur doctor` state
/// one warning in one wording, and so the decision is testable without a host that has AppArmor.
/// Every other grant, including [`UsernsGrant::Withheld`], is silent here — `Withheld` is already
/// carried by `E-CAP-003`/`E-CAP-005` where it actually blocks something.
pub fn warn_on_userns_restriction_disabled_host_wide(grant: Option<UsernsGrant>) {
    if grant != Some(UsernsGrant::RestrictionDisabledHostWide) {
        return;
    }
    let link = security_warning_link(W_SEC_013);
    eprintln!(
        "[capsule-runtime] warning[{W_SEC_013}]: kernel.apparmor_restrict_unprivileged_userns is \
         off on this host, so unprivileged user namespaces are unrestricted for every binary on \
         the machine, not just for mur — this is what makes sealed containment and the capsule \
         network namespace work here, and it is not the configuration murmur ships. To get the \
         narrow, mur-only grant instead: restore the knob to 1 (removing any \
         /etc/sysctl.d/*-userns.conf drop-in that sets it to 0), then install and load the shipped \
         profile with `sudo install -m 644 packaging/apparmor/{profile} {path} && sudo \
         apparmor_parser -r {path}`. Nothing is refused because of this ({link})",
        profile = crate::sealed::SEALED_APPARMOR_PROFILE_NAME,
        path = crate::sealed::SEALED_APPARMOR_PROFILE_PATH,
    );
}

/// Warns (non-fatal, once per session) when the capsule's own top-level `capabilities.state` is
/// declared, because that declaration reaches nothing.
///
/// A durable state store is granted per artifact — it is the tool, driver or hook entry that gets
/// the second preopen, and the capsule's own guest is built with no artifact grant at all. So a
/// capsule-wide block creates no directory and opens no `state/` path for anybody. Structurally
/// valid, hence warned rather than refused, on the same terms as `W-SEC-006` and `W-SEC-008`; but
/// stated plainly, because the alternative signal an operator gets is an empty directory that
/// never appears.
///
/// Same seam as [`warn_on_workdir_exec`]: fires at staging, before any session workdir exists, so
/// it goes to stderr only and not to `logs/bootstrap.log`.
fn warn_on_inert_capsule_wide_state(state_declared: bool) {
    if !state_declared {
        return;
    }
    let link = security_warning_link(W_SEC_014);
    eprintln!(
        "[capsule-runtime] warning[{W_SEC_014}]: capsule-wide capabilities.state is declared, but \
         a durable state store is granted per artifact — nothing reads a top-level declaration, \
         so no store was created and no 'state' preopen exists. Move the block onto the tool, \
         driver or hook entry that needs it ({link})"
    );
}

/// A per-hook `capabilities:` block reuses the whole [`murmur_artifact::Capabilities`]
/// vocabulary, but only `network`, `filesystem`, `state` and `task_io` govern a hook — the rest are
/// capsule-wide concerns nothing reads per-artifact. Warn rather than reject (the block is
/// structurally valid) so an operator who declared, say, `shell.allow` on a hook entry learns
/// it is inert instead of assuming it was applied. Infallible and non-fatal, like
/// [`warn_if_bash_network_bypass`], and carries the same `W-SEC-*` registry code + doc link
/// convention as every other non-fatal capability warning (see `security_warnings.rs`).
///
/// Not written to `logs/bootstrap.log`, unlike [`warn_if_bash_network_bypass`]: this fires
/// during artifact staging in [`stage_session`], before the session workdir
/// `warn_if_bash_network_bypass`/`sandbox::warn_for_enforcement_tier` write into even exists.
fn warn_on_inert_hook_capabilities(
    hook_name: &str,
    capabilities: Option<&murmur_artifact::Capabilities>,
) {
    let inert = inert_capability_sub_blocks(capabilities);
    if !inert.is_empty() {
        let link = security_warning_link(W_SEC_006);
        eprintln!(
            "[capsule-runtime] warning[{W_SEC_006}]: hook '{hook_name}' declares capabilities.{} \
             which the runtime does not apply per-hook — only capabilities.network, \
             capabilities.filesystem, capabilities.state and capabilities.task_io govern a hook \
             ({link})",
            inert.join(", capabilities.")
        );
    }
}

/// The sub-blocks a per-artifact `capabilities:` grant never reads, whichever role declared
/// it. Shared by the hook (`W-SEC-006`) and tool/driver (`W-SEC-008`) warnings, which differ
/// only in code and wording — the hazard, and the set of inert keys, is identical.
fn inert_capability_sub_blocks(
    capabilities: Option<&murmur_artifact::Capabilities>,
) -> Vec<&'static str> {
    let Some(capabilities) = capabilities else {
        return Vec::new();
    };

    [
        ("shell", capabilities.shell.is_some()),
        ("spawn", capabilities.spawn.is_some()),
        ("env", capabilities.env.is_some()),
        ("limits", capabilities.limits.is_some()),
        // Same hazard as its siblings: host-process bounds are session-scoped (one cgroup scope,
        // one workdir guard, one set of rlimits per spawned process), so a per-artifact or
        // per-hook `resources:` block is structurally accepted and silently inert.
        ("resources", capabilities.resources.is_some()),
        // The containment floor is capsule-wide, resolved before staging — a per-artifact
        // declaration of it is read by nothing.
        ("containment", capabilities.containment.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// Lower one tool's or driver's per-artifact grant and record it, warning about anything the
/// operator declared that narrowing will not honor.
///
/// Called from the WASM-tool and driver staging arms only. Inserting nothing when the entry
/// declares no `capabilities:` block is what makes the absent case a strict no-op: dispatch
/// looks the artifact up by name and falls back to the session's own policy on a miss.
fn stage_artifact_grant(
    artifact: &ArtifactRequest,
    ceiling_network_allow_rules: &[NetworkAllowRule],
    capsule_name: &str,
    artifact_grants: &mut HashMap<String, ToolCapabilityGrant>,
) -> Result<(), RuntimeError> {
    let Some(capabilities) = artifact.capabilities.as_ref() else {
        return Ok(());
    };

    // Derived from `artifact` — the operator's own manifest entry — and never from the
    // artifact's bundled `murmur.yaml`, so a tool pulled from a registry cannot scope itself
    // up. `capsule_name` is operator-sourced for the same reason: it is what an undeclared
    // `capabilities.state.store` defaults to, and a registry-pulled tool must not be able to
    // name the store it lands in. Deriving at staging (not at dispatch) means a malformed grant
    // fails the run before any guest starts.
    let mut grant = ToolCapabilityGrant::derive(
        Some(capabilities),
        ceiling_network_allow_rules,
        capsule_name,
    )?;
    // The one side effect on this path, and it happens only for an entry that declared a store:
    // `derive` validated the name and left the directory to be made here, so lowering stays pure
    // and a run that never reaches staging creates nothing on disk.
    if let Some(store) = grant.state_store.as_deref() {
        grant.state_dir = Some(crate::state_store::ensure_state_store(store)?);
    }
    warn_on_out_of_ceiling_network_entries(&artifact.name, &grant.dropped_network_entries);
    warn_on_inert_tool_capabilities(&artifact.name, Some(capabilities));
    artifact_grants.insert(artifact.name.clone(), grant);
    Ok(())
}

/// A per-artifact `network.allow` entry the capsule-wide ceiling does not itself allow was
/// dropped rather than granted — narrowing only ever subtracts. Non-fatal on purpose: the
/// resulting posture is strictly *tighter* than the operator asked for, so failing staging
/// would punish a safe mistake, but a silent drop would leave them believing a host is
/// reachable when it is not.
fn warn_on_out_of_ceiling_network_entries(artifact_name: &str, dropped: &[String]) {
    if dropped.is_empty() {
        return;
    }

    let link = security_warning_link(W_SEC_007);
    eprintln!(
        "[capsule-runtime] warning[{W_SEC_007}]: artifact '{artifact_name}' declares \
         capabilities.network.allow entries the capsule-wide ceiling does not allow ({}) — \
         they are dropped, not granted, because per-artifact capabilities can only narrow \
         ({link})",
        dropped.join(", ")
    );
}

/// The tool/driver counterpart of [`warn_on_inert_hook_capabilities`]: a per-artifact grant reads
/// only `network`, `filesystem` and `state`, so any other sub-block is structurally valid and
/// silently inert. Warn rather than reject, matching how every other capability-posture issue
/// is reported.
fn warn_on_inert_tool_capabilities(
    artifact_name: &str,
    capabilities: Option<&murmur_artifact::Capabilities>,
) {
    let inert = inert_capability_sub_blocks(capabilities);
    if !inert.is_empty() {
        let link = security_warning_link(W_SEC_008);
        eprintln!(
            "[capsule-runtime] warning[{W_SEC_008}]: artifact '{artifact_name}' declares \
             capabilities.{} which per-artifact narrowing does not apply — only \
             capabilities.network, capabilities.filesystem and capabilities.state apply to a tool \
             or driver ({link})",
            inert.join(", capabilities.")
        );
    }
}

/// A `runtime: tool` artifact with a native (non-WASM) implementation runs as a host
/// subprocess under the capsule-wide shell/sandbox machinery, never through the WASI tool
/// path per-artifact grants are applied on. Declaring one is therefore wholly inert, which is
/// a sharper hazard than an inert sub-block and gets the same `W-SEC-008` treatment.
fn warn_on_unenforceable_native_capabilities(
    artifact_name: &str,
    capabilities: Option<&murmur_artifact::Capabilities>,
) {
    if capabilities.is_none() {
        return;
    }

    let link = security_warning_link(W_SEC_008);
    eprintln!(
        "[capsule-runtime] warning[{W_SEC_008}]: artifact '{artifact_name}' declares \
         per-artifact 'capabilities:' but ships a native implementation — narrowing applies \
         only to WASM tools and drivers, so this grant is not enforced ({link})"
    );
}

/// Builds the single `Engine` a session runs every guest on.
///
/// `epoch_interruption` compiles a deadline check into wasm loop back-edges and function
/// entries. It only arms the mechanism: a deadline fires solely when some store has called
/// `set_epoch_deadline` *and* an [`EpochTicker`] is advancing this engine's epoch, both of
/// which `stage_session` sets up.
fn build_engine() -> Result<Engine, RuntimeError> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.epoch_interruption(true);
    Engine::new(&config).map_err(|err| RuntimeError::Runtime(err.to_string()))
}

/// Map a failed capsule-guest invocation onto the most specific `RuntimeError` available,
/// so a deadline or resource-limit trap is distinguishable from a guest panic rather than
/// collapsing into the generic `CapsuleTrap` bucket.
fn capsule_guest_error(error: &wasmtime::Error, limiter: &ExecutionLimiter) -> RuntimeError {
    match classify_guest_failure(error, limiter) {
        GuestFailure::DeadlineExceeded { seconds } => {
            RuntimeError::CapsuleDeadlineExceeded { seconds }
        }
        GuestFailure::ResourceLimit { message } => RuntimeError::CapsuleResourceLimit { message },
        GuestFailure::Other => RuntimeError::CapsuleTrap(error.to_string()),
    }
}

fn map_registry_error(name: &str, version: &str, error: RegistryError) -> RuntimeError {
    match error {
        RegistryError::NotFound { .. } => RuntimeError::artifact_not_found(name, version),
        RegistryError::IntegrityMismatch { .. } => {
            RuntimeError::artifact_integrity_failed(name, version)
        }
        other => RuntimeError::Runtime(other.to_string()),
    }
}

/// Build a guest's WASI context with a scoped environment: no host inheritance, only what
/// `policy.env_allow` declares (credential-filtered) plus the runtime's own `extra_env`.
///
/// `extra_env` is applied last so a manifest cannot shadow a runtime-owned `MURMUR_*` value
/// by allowlisting its name.
///
/// `filesystem_scope` is a per-artifact narrowing of what gets preopened as `"."`: `None`
/// preopens `workdir` itself, which is what every caller without a per-artifact grant passes
/// and is the pre-existing behavior. `Some(scope)` preopens `workdir/scope` instead, created
/// if missing — already validated as relative and non-escaping by
/// [`ToolCapabilityGrant::derive`] at staging time.
///
/// `state_dir` is the artifact's durable state store, already created at `0700` by the staging
/// path. `Some(dir)` adds a *second* preopen at the guest path [`STATE_PREOPEN_NAME`], so the
/// guest reaches it as `state/<file>`; it is a host path outside every workdir, and the only one
/// a guest can name. `None` — every caller without a `capabilities.state` grant — adds nothing,
/// leaving the workdir preopen as the guest's only filesystem reach.
fn build_wasi_ctx(
    workdir: &Path,
    filesystem_scope: Option<&str>,
    state_dir: Option<&Path>,
    extra_env: &[(String, String)],
    policy: &CapabilityPolicy,
) -> Result<WasiCtx, RuntimeError> {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdio();
    for (key, value) in build_wasi_env_allowlist(policy) {
        builder.env(key, value);
    }
    for (key, value) in extra_env {
        builder.env(key, value);
    }

    // Hard error rather than a silent fall back to the unscoped workdir (which would widen
    // the grant) or to no preopen at all (which would look like a guest bug).
    let preopen_root = match filesystem_scope {
        None => workdir.to_path_buf(),
        Some(scope) => resolve_scoped_dir(workdir, scope)?,
    };

    builder
        .preopened_dir(&preopen_root, ".", DirPerms::all(), FilePerms::all())
        .map_err(|err| RuntimeError::wasi(preopen_root, err.to_string()))?;

    if let Some(state_dir) = state_dir {
        builder
            .preopened_dir(
                state_dir,
                STATE_PREOPEN_NAME,
                DirPerms::all(),
                FilePerms::all(),
            )
            .map_err(|err| RuntimeError::wasi(state_dir.to_path_buf(), err.to_string()))?;
    }

    Ok(builder.build())
}

fn inference_env_pairs(inference: &murmur_artifact::InferenceConfig) -> Vec<(String, String)> {
    let mut pairs = vec![
        (
            "MURMUR_INFERENCE_TRANSPORT".to_string(),
            inference.transport.clone(),
        ),
        (
            "MURMUR_INFERENCE_ENDPOINT".to_string(),
            inference.endpoint.clone().unwrap_or_default(),
        ),
        (
            "MURMUR_INFERENCE_MODEL".to_string(),
            inference.model.clone(),
        ),
        (
            "MURMUR_INFERENCE_DRIVER".to_string(),
            inference
                .driver
                .as_ref()
                .map(|d| d.artifact.clone())
                .unwrap_or_default(),
        ),
    ];

    if let Some(api_key) = inference.api_key.as_ref() {
        pairs.push(("MURMUR_INFERENCE_API_KEY".to_string(), api_key.clone()));
    }

    if let Some(config) = inference.driver.as_ref().and_then(|d| d.config.as_ref()) {
        pairs.push(("MURMUR_INFERENCE_DRIVER_CONFIG".to_string(), config.clone()));
    }

    pairs
}

/// Suspend an A2A task in the InputRequired state, await external input, and resume.
///
/// Called from the `murmur:task/task#request-input` host function registered in the tool linker.
/// Emits SSE events for the state transitions. Returns `Err` on timeout (traps the WASM guest).
pub(crate) async fn request_input_impl(
    task_id: String,
    prompt: String,
    task_registry: Arc<Mutex<TaskRegistry>>,
    sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    input_timeout_secs: Option<u64>,
) -> wasmtime::Result<String> {
    use std::time::Duration;
    use tokio::sync::oneshot;

    let (tx, rx) = oneshot::channel::<String>();
    {
        let mut reg = task_registry.lock().unwrap();
        reg.set_input_required(&task_id, prompt.clone(), tx)
            .map_err(|e| wasmtime::Error::msg(format!("request-input: {e}")))?;
    }

    // Use high IDs to avoid overlapping with agent-loop SSE event IDs (which start at 0).
    let mut sse_event_id: u64 = u64::MAX / 2;
    emit_sse(
        &sse,
        &mut sse_event_id,
        "status",
        &TaskStatusUpdateEvent {
            id: task_id.clone(),
            context_id: None,
            status: StreamStatus {
                state: "input-required".into(),
                message: prompt.clone(),
                response: None,
            },
            r#final: false,
        },
    )
    .await;

    let result = match input_timeout_secs {
        Some(secs) => tokio::time::timeout(Duration::from_secs(secs), rx)
            .await
            .map_err(|_| ())
            .and_then(|r| r.map_err(|_| ())),
        None => rx.await.map_err(|_| ()),
    };

    match result {
        Ok(text) => {
            emit_sse(
                &sse,
                &mut sse_event_id,
                "status",
                &TaskStatusUpdateEvent {
                    id: task_id.clone(),
                    context_id: None,
                    status: StreamStatus {
                        state: "working".into(),
                        message: "resumed".into(),
                        response: None,
                    },
                    r#final: false,
                },
            )
            .await;
            Ok(text)
        }
        Err(()) => {
            {
                let mut reg = task_registry.lock().unwrap();
                reg.finish_task(TaskState::Failed);
                reg.advance_resource_generation();
            }
            emit_sse(
                &sse,
                &mut sse_event_id,
                "status",
                &TaskStatusUpdateEvent {
                    id: task_id.clone(),
                    context_id: None,
                    status: StreamStatus {
                        state: "failed".into(),
                        message: "input-timeout".into(),
                        response: None,
                    },
                    r#final: true,
                },
            )
            .await;
            Err(wasmtime::Error::msg("input-timeout"))
        }
    }
}

pub(crate) struct NetworkPolicyHooks {
    pub(crate) network_allow_rules: Vec<NetworkAllowRule>,
}

impl WasiHttpHooks for NetworkPolicyHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<HostFutureIncomingResponse> {
        let target =
            RequestTarget::from_request(request.uri(), config.use_tls).ok_or_else(|| {
                wasmtime_wasi_http::p2::HttpError::from(WasiHttpErrorCode::HttpRequestDenied)
            })?;

        if !self
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&target))
        {
            return Err(wasmtime_wasi_http::p2::HttpError::from(
                WasiHttpErrorCode::HttpRequestDenied,
            ));
        }

        Ok(wasmtime_wasi_http::p2::default_send_request(
            request, config,
        ))
    }
}

pub(crate) struct CapsuleStoreState {
    /// Resource limiter for this store, registered via `Store::limiter`. Also the record of
    /// any growth request it denied, which `classify_guest_failure` reads to tell a
    /// limit trap apart from a guest panic.
    pub(crate) limits: ExecutionLimiter,
    pub(crate) table: ResourceTable,
    pub(crate) wasi: WasiCtx,
    pub(crate) http: WasiHttpCtx,
    pub(crate) http_hooks: NetworkPolicyHooks,
    pub(crate) network_allow_rules: Vec<NetworkAllowRule>,
    /// `capabilities.peer_fetch.allow`, parsed. Checked **before** `fetch-peer-file` opens any
    /// connection, and never merged with `network_allow_rules`.
    pub(crate) peer_fetch_rules: Vec<NetworkAllowRule>,
    /// The minting side. `None` — no `exports.peer_files` — means no `share-file` tool manifest
    /// was written, so this is the belt to that braces: the dispatch branch refuses rather than
    /// assuming the tool could not have been called.
    pub(crate) peer_plane: Option<Arc<crate::peer_handoff::PeerPlane>>,
    /// This capsule's own audience, asserted on every redeem it issues.
    pub(crate) peer_own_audience: String,
    /// Where the peer-handoff tools write their records. The concurrent `O_APPEND` sink rather
    /// than the loop's `TraceWriter`, which is `&mut`-owned by the loop and out of reach here —
    /// so a mint and a fetch land at the moment of the event rather than at the next task
    /// boundary.
    pub(crate) peer_trace: Option<Arc<crate::trace::ResourceTraceAppender>>,
    pub(crate) inference_env: Vec<(String, String)>,
    pub(crate) engine: Engine,
    pub(crate) workdir: PathBuf,
    pub(crate) accessible_workdir: PathBuf,
    pub(crate) tool_components: HashMap<String, Component>,
    /// Per-artifact narrowing keyed by artifact name, moved over from
    /// [`StagedSession::artifact_grants`]. A name absent here dispatches on the full ceiling.
    pub(crate) artifact_grants: HashMap<String, ToolCapabilityGrant>,
    pub(crate) allowlisted_tools: HashSet<String>,
    pub(crate) installed_artifacts: Vec<InstalledArtifactSummary>,
    pub(crate) session_id: String,
    /// Buffered outgoing A2A send events — drained into trace.jsonl after the capsule run.
    /// (peer_url, message_id, task_id, context_id, traceparent)
    pub(crate) pending_a2a_events: Vec<(String, String, String, String, Option<String>)>,
    pub(crate) capability_policy: CapabilityPolicy,
    /// Host-detected kernel enforcement tier + resolved network allowlist IPs for this
    /// session's shell subprocesses. Kept separate from `CapabilityPolicy` (which stays
    /// purely manifest-derived) since this is host-probed data, not manifest data.
    pub(crate) shell_enforcement: sandbox::ShellEnforcement,
    /// W3C traceparent for outgoing murmur:message/send calls — set by the runtime loop
    /// after each begin_session so the active session span propagates to peer capsules.
    pub(crate) current_traceparent: Option<String>,
    // ── A2A request-input support ────────────────────────────────────────────────
    /// Shared task registry — Some in A2A mode, None for script capsules.
    pub(crate) a2a_task_registry: Option<Arc<Mutex<TaskRegistry>>>,
    /// SSE broadcast channel — Some in A2A mode, None for script capsules.
    pub(crate) a2a_sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    /// Active A2A task ID — set per-task before run_agent_loop, None outside A2A context.
    pub(crate) a2a_task_id: Option<String>,
    /// Optional input timeout from lifecycle config.
    pub(crate) input_timeout_secs: Option<u64>,
    // ── A2A streaming text chunk support ─────────────────────────────────────────
    /// Monotonically increasing event ID counter for text chunk SSE events.
    /// Starts at u64::MAX/4 to avoid overlap with agent-loop status/artifact IDs (from 0).
    pub(crate) a2a_chunk_event_id: Arc<AtomicU64>,
    /// Set to true when any emit-chunk call is made during the current driver dispatch.
    /// Reset to false before each driver dispatch in run_agent_loop.
    pub(crate) a2a_chunks_emitted: Arc<AtomicBool>,
    /// Registry used to resolve additional artifacts requested at runtime via `manage.pull()`.
    pub(crate) registry: Arc<dyn Registry>,
    /// Path to this session's `murmur.lock`, consulted and updated by `manage.pull()`.
    pub(crate) lock_path: PathBuf,
    // ── Stateful-driver continuation ───────────────────────────────────
    /// Continuation id most recently returned by the inference driver via
    /// `tool-result.metadata["continuation_id"]`, held for the current session loop.
    /// `None` means "no continuation held" — every Turn resends the full `messages`
    /// array (the behavior of every driver shipped today, which never sets the key).
    pub(crate) driver_continuation_id: Option<String>,
    /// The `context_id` under which `driver_continuation_id` was established. Used as a
    /// cross-context safety guard: a held continuation is only reused when the current
    /// Turn's `context_id` matches this, so a driver-side continuation from one
    /// conversation is never carried into an unrelated Task within the same session loop.
    pub(crate) driver_continuation_context_id: Option<String>,
    /// Number of leading entries of the logical `messages` array the driver has already
    /// acknowledged. On an incremental Turn the host transmits only `messages[acked_len..]`.
    pub(crate) driver_continuation_acked_len: usize,
}

impl CapsuleStoreState {
    /// Returns the held continuation `(id, acked_len)` iff a continuation is currently held
    /// **and** was established under `context_id`. The context-id guard is required for
    /// correctness: without it, an incremental send against a driver-side continuation from
    /// an unrelated conversation would silently corrupt the driver's context.
    pub(crate) fn active_continuation(&self, context_id: Option<&str>) -> Option<(&str, usize)> {
        let id = self.driver_continuation_id.as_deref()?;
        if self.driver_continuation_context_id.as_deref() != context_id {
            return None;
        }
        Some((id, self.driver_continuation_acked_len))
    }

    /// Persist the continuation id a driver returned on this Turn, scoped to `context_id`,
    /// recording that the driver now knows the first `acked_len` entries of `messages`.
    pub(crate) fn record_continuation(
        &mut self,
        id: String,
        context_id: Option<String>,
        acked_len: usize,
    ) {
        self.driver_continuation_id = Some(id);
        self.driver_continuation_context_id = context_id;
        self.driver_continuation_acked_len = acked_len;
    }

    /// Drop any held continuation id and its bookkeeping. Called when a driver stops
    /// returning the key ("not continuing") and at every `replace-context` commit.
    pub(crate) fn clear_continuation(&mut self) {
        self.driver_continuation_id = None;
        self.driver_continuation_context_id = None;
        self.driver_continuation_acked_len = 0;
    }

    /// Advance the acked-message count for a currently-held, same-context continuation.
    /// Used when a Task's final `end_turn` persists an extra assistant message into the
    /// context history that the driver already knows (it generated it), so the next
    /// same-context Task sends only its new user message rather than re-including it.
    pub(crate) fn advance_continuation_acked_len(
        &mut self,
        context_id: Option<&str>,
        acked_len: usize,
    ) {
        if self.driver_continuation_id.is_some()
            && self.driver_continuation_context_id.as_deref() == context_id
        {
            self.driver_continuation_acked_len = acked_len;
        }
    }
}

impl WasiView for CapsuleStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for CapsuleStoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

impl invoke::Host for CapsuleStoreState {
    fn invoke(
        &mut self,
        name: String,
        input: murmur::tool::run::ToolInput,
    ) -> Result<murmur::tool::run::ToolResult, String> {
        if !self.allowlisted_tools.contains(&name) {
            return Err(format!(
                "tool '{name}' is not declared in manifest allowlist"
            ));
        }
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.dispatch_tool_async(&name, input))
        })
    }
}

impl send::Host for CapsuleStoreState {
    fn send(
        &mut self,
        peer_url: String,
        message: send::Message,
    ) -> Result<send::TaskResult, String> {
        check_destination_allowed(
            &self.network_allow_rules,
            &peer_url,
            "capabilities.network.allow",
        )?;

        let message_id = message.message_id.clone();
        let outgoing_msg = outgoing::OutgoingMessage {
            message_id: message.message_id,
            context_id: message.context_id,
            text: message.text,
        };

        // send_a2a_message is async; use block_in_place so we can call it from this sync
        // host function while inside a multi-thread Tokio runtime (script capsule path).
        let traceparent = self.current_traceparent.clone();
        let task = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(outgoing::send_a2a_message(
                &peer_url,
                outgoing_msg,
                traceparent.clone(),
            ))
        })?;

        self.pending_a2a_events.push((
            peer_url.clone(),
            message_id,
            task.id.clone(),
            task.context_id.clone(),
            traceparent,
        ));
        Ok(send::TaskResult {
            task_id: task.id,
            context_id: task.context_id,
            state: match task.status.state {
                crate::a2a::TaskState::Submitted => "submitted".to_string(),
                crate::a2a::TaskState::Working => "working".to_string(),
                crate::a2a::TaskState::InputRequired => "input-required".to_string(),
                crate::a2a::TaskState::Completed => "completed".to_string(),
                crate::a2a::TaskState::Failed => "failed".to_string(),
                crate::a2a::TaskState::Rejected => "rejected".to_string(),
            },
        })
    }
}

impl manage::Host for CapsuleStoreState {
    fn list(&mut self) -> Vec<manage::ArtifactSummary> {
        let mut result: Vec<manage::ArtifactSummary> = self
            .installed_artifacts
            .iter()
            .filter(|a| a.runtime.is_llm_visible())
            .map(|artifact| manage::ArtifactSummary {
                name: artifact.name.clone(),
                version: artifact.version.clone(),
                runtime: runtime_type_to_wit(&artifact.runtime, artifact.implementation.as_ref()),
            })
            .collect();

        let existing: std::collections::HashSet<String> =
            result.iter().map(|a| a.name.clone()).collect();
        for binary in &self.capability_policy.shell_allow {
            if !existing.contains(binary) {
                result.push(manage::ArtifactSummary {
                    name: binary.clone(),
                    version: "0.0.0".to_string(),
                    runtime: manage::RuntimeType::Native,
                });
            }
        }

        result
    }

    fn describe(&mut self, name: String) -> Result<manage::ArtifactInfo, String> {
        // Check installed artifacts first, then fall back to shell tool manifests on disk.
        let fallback_summary;
        let summary = if let Some(s) = self
            .installed_artifacts
            .iter()
            .find(|artifact| artifact.name == name)
        {
            s
        } else if self
            .capability_policy
            .shell_allow
            .iter()
            .any(|b| b == &name)
        {
            fallback_summary = InstalledArtifactSummary {
                name: name.clone(),
                version: "0.0.0".to_string(),
                runtime: ArtifactRuntime::Tool,
                implementation: Some(ArtifactImplementation::Native),
            };
            &fallback_summary
        } else {
            return Err(format!("artifact '{name}' is not installed"));
        };

        let manifest_path = self
            .workdir
            .join("tools")
            .join(&name)
            .join(PACKED_MANIFEST_ENTRY);

        read_artifact_info_from_manifest(&manifest_path, summary)
    }

    fn search(&mut self, query: String) -> Result<Vec<manage::ArtifactSummary>, String> {
        Err(format!("not implemented (query: {query})"))
    }

    fn pull(&mut self, name: String, version: String) -> Result<manage::ArtifactSummary, String> {
        // 1. Resolve + verify against the registry's own self-reported hash.
        let resolved = self
            .registry
            .resolve_with_platform(&name, &version, Some(current_platform()))
            .map_err(|err| format!("failed to resolve {name}@{version}: {err}"))?;

        verify_sha256(&name, &version, &resolved.bytes, &resolved.sha256).map_err(|_| {
            format!("artifact integrity check failed for {name}@{version}: registry-reported hash does not match downloaded bytes")
        })?;

        // 2. Cross-check against any existing murmur.lock pin — a runtime pull must never
        // silently override what's already pinned for this artifact.
        let mut lock = match read_lockfile(&self.lock_path) {
            Ok(lock) => lock,
            Err(LockfileError::NotFound(_)) => MurmurLock {
                lock_version: LOCK_VERSION,
                artifacts: Vec::new(),
            },
            Err(err) => return Err(format!("failed to read murmur.lock: {err}")),
        };

        if let Some(existing) = lock.artifact_for(&name) {
            if existing.resolved_version != version || existing.sha256.wasm != resolved.sha256 {
                return Err(format!(
                    "murmur.lock conflict for '{name}': pinned {}@{} (sha256 {}), but the \
                     registry now resolves {name}@{version} (sha256 {}) — refusing to override \
                     a pinned artifact at runtime",
                    existing.name, existing.resolved_version, existing.sha256.wasm, resolved.sha256
                ));
            }
        }

        // 3. Extract murmur.yaml, dispatch extraction by runtime type, and write files under
        // <workdir>/tools/<name>/ — no disk writes happen before steps 1-2 succeed.
        let manifest_yaml = extract_manifest_yaml(&name, &version, &resolved.bytes)
            .map_err(|err| err.to_string())?;
        write_tool_manifest(&self.workdir, &name, &manifest_yaml).map_err(|err| err.to_string())?;

        let (artifact_runtime, implementation, wasm_component) = match resolved.meta.runtime {
            RuntimeType::Wasm => {
                let wasm_bytes = extract_root_wasm(&name, &version, &resolved.bytes)
                    .map_err(|err| err.to_string())?;
                let component = Component::new(&self.engine, &wasm_bytes)
                    .map_err(|err| format!("failed to compile pulled component '{name}': {err}"))?;
                (
                    ArtifactRuntime::Tool,
                    Some(ArtifactImplementation::Wasm),
                    Some(component),
                )
            }
            RuntimeType::Native => {
                let binary = extract_native_binary(&name, &version, &resolved.bytes)
                    .map_err(|err| err.to_string())?;
                install_native_binaries(&self.workdir, vec![(name.clone(), binary)])
                    .map_err(|err| err.to_string())?;
                (
                    ArtifactRuntime::Tool,
                    Some(ArtifactImplementation::Native),
                    None,
                )
            }
            RuntimeType::Static => {
                let skill_md = extract_skill_md(&name, &version, &resolved.bytes)
                    .map_err(|err| err.to_string())?;
                install_skill_files(&self.workdir, vec![(name.clone(), skill_md)])
                    .map_err(|err| err.to_string())?;
                (ArtifactRuntime::Skill, None, None)
            }
        };

        // 4. Files are on disk — now, and only now, update murmur.lock.
        if let Some(entry) = lock.artifacts.iter_mut().find(|entry| entry.name == name) {
            entry.resolved_version = version.clone();
            entry.sha256 = LockedSha256 {
                wasm: resolved.sha256.clone(),
            };
        } else {
            lock.artifacts.push(LockedArtifact {
                name: name.clone(),
                resolved_version: version.clone(),
                sha256: LockedSha256 {
                    wasm: resolved.sha256.clone(),
                },
            });
        }
        write_lockfile_atomic(&self.lock_path, &lock)
            .map_err(|err| format!("failed to write murmur.lock: {err}"))?;

        // 5. Reflect the pulled artifact in in-memory session state so list()/describe() (and,
        // for WASM tools, invoke()) see it immediately.
        if let Some(component) = wasm_component {
            self.tool_components.insert(name.clone(), component);
        }

        let summary = InstalledArtifactSummary {
            name: name.clone(),
            version: version.clone(),
            runtime: artifact_runtime,
            implementation,
        };
        if let Some(existing) = self
            .installed_artifacts
            .iter_mut()
            .find(|artifact| artifact.name == name)
        {
            *existing = summary.clone();
        } else {
            self.installed_artifacts.push(summary.clone());
        }

        Ok(manage::ArtifactSummary {
            name,
            version,
            runtime: runtime_type_to_wit(&summary.runtime, summary.implementation.as_ref()),
        })
    }

    fn remove(&mut self, name: String) -> Result<bool, String> {
        Err(format!("not implemented (name: {name})"))
    }

    fn diagnostics(&mut self) -> Result<manage::RuntimeState, String> {
        Ok(manage::RuntimeState {
            capsule_id: self.session_id.clone(),
            installed: self.list(),
            capabilities: "artifact-manager/search and remove are not implemented".to_string(),
        })
    }
}

/// Borrowed half of a WASM tool invocation environment: everything
/// [`invoke_tool_component`] needs that is neither the component nor the A2A
/// wiring. Grouped into a struct so the hook runtime can assemble one from its
/// own owned copies without a >7-argument function.
pub(crate) struct ToolInvokeEnv<'a> {
    pub(crate) engine: &'a Engine,
    pub(crate) accessible_workdir: &'a Path,
    pub(crate) inference_env: &'a [(String, String)],
    pub(crate) capability_policy: &'a CapabilityPolicy,
    /// The capsule-wide ceiling. What actually gets enforced is this, clamped by
    /// `artifact_grant` when the operator declared one.
    pub(crate) network_allow_rules: &'a [NetworkAllowRule],
    /// This artifact's optional narrowing, from the operator's own manifest entry. `None` —
    /// no `capabilities:` block on the entry — means the ceiling applies untouched and the
    /// whole `accessible_workdir` is preopened, exactly as before narrowing existed.
    pub(crate) artifact_grant: Option<&'a ToolCapabilityGrant>,
}

/// Per-session A2A wiring registered on a tool linker.
///
/// The two host interfaces it backs (`murmur:text/chunks`, `murmur:task/task`)
/// are always *defined* — a streaming driver imports them and would fail to
/// instantiate otherwise — but each function is a no-op when its channel is
/// absent. [`ToolA2aWiring::silent`] is that all-absent form, used for a
/// dispatch that is not part of an A2A task turn (a hook's `run-inference`
/// call, which must not stream chunks into the user's SSE stream or ask the
/// user for input).
pub(crate) struct ToolA2aWiring {
    sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    task_id: Option<String>,
    chunk_event_id: Arc<AtomicU64>,
    chunks_emitted: Arc<AtomicBool>,
    task_registry: Option<Arc<Mutex<TaskRegistry>>>,
    input_timeout_secs: Option<u64>,
}

impl ToolA2aWiring {
    pub(crate) fn silent() -> Self {
        Self {
            sse: None,
            task_id: None,
            chunk_event_id: Arc::new(AtomicU64::new(0)),
            chunks_emitted: Arc::new(AtomicBool::new(false)),
            task_registry: None,
            input_timeout_secs: None,
        }
    }
}

/// Instantiate `component` in a fresh `Linker`/`Store` and call its
/// `murmur:tool/run@0.1.0#run` export.
///
/// This is the single WASM-tool (and therefore inference-driver) invocation
/// body in the runtime. [`CapsuleStoreState::dispatch_tool_async`] is a thin
/// wrapper that fills `env`/`a2a` from the capsule store; a hook's
/// `run-inference` host import fills them from its own owned copies. Neither
/// duplicates any part of the instantiate/type-check/call sequence below.
pub(crate) async fn invoke_tool_component(
    env: ToolInvokeEnv<'_>,
    a2a: ToolA2aWiring,
    name: &str,
    component: &Component,
    input: murmur::tool::run::ToolInput,
) -> Result<murmur::tool::run::ToolResult, String> {
    let ToolInvokeEnv {
        engine,
        accessible_workdir,
        inference_env,
        capability_policy,
        network_allow_rules,
        artifact_grant,
    } = env;
    let ToolA2aWiring {
        sse: a2a_sse,
        task_id: a2a_task_id,
        chunk_event_id: a2a_chunk_event_id,
        chunks_emitted: a2a_chunks_emitted,
        task_registry: a2a_task_registry,
        input_timeout_secs,
    } = a2a;

    let mut linker = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)
        .map_err(|err| format!("failed to add WASI linker for tool '{name}': {err}"))?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker)
        .map_err(|err| format!("failed to add HTTP linker for tool '{name}': {err}"))?;

    // Register murmur:text/chunks host functions (synchronous).
    // Components that do not import this interface ignore the registrations.
    // Both functions must be defined in a single .instance() call — Wasmtime
    // rejects a second .instance() for the same interface name. Registered
    // under the versioned name only (see WIT_TEXT_CHUNKS_IFACE / wit/VERSIONING.md).
    {
        let chunks_iface = WIT_TEXT_CHUNKS_IFACE;
        let sse_for_chunk = a2a_sse.clone();
        let task_id_for_chunk = a2a_task_id.clone();
        let chunk_event_id = Arc::clone(&a2a_chunk_event_id);
        let chunks_emitted_flag = Arc::clone(&a2a_chunks_emitted);
        let sse_for_thinking = a2a_sse.clone();
        let task_id_for_thinking = a2a_task_id.clone();
        let thinking_event_id = Arc::clone(&a2a_chunk_event_id);

        let mut inst = linker.instance(chunks_iface).map_err(|err| {
            format!("failed to define {chunks_iface} instance for '{name}': {err}")
        })?;

        inst.func_wrap(
            "emit-chunk",
            move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>, (chunk,): (String,)| {
                chunks_emitted_flag.store(true, Ordering::Relaxed);
                if let (Some((ref tx, ref buf)), Some(ref tid)) =
                    (&sse_for_chunk, &task_id_for_chunk)
                {
                    emit_chunk_sse(tx, buf, &chunk_event_id, tid, &chunk);
                }
                Ok(())
            },
        )
        .map_err(|err| format!("failed to register emit-chunk for tool '{name}': {err}"))?;

        inst.func_wrap(
            "emit-thinking-chunk",
            move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>, (chunk,): (String,)| {
                if let (Some((ref tx, ref buf)), Some(ref tid)) =
                    (&sse_for_thinking, &task_id_for_thinking)
                {
                    emit_thinking_chunk_sse(tx, buf, &thinking_event_id, tid, &chunk);
                }
                Ok(())
            },
        )
        .map_err(|err| {
            format!("failed to register emit-thinking-chunk for tool '{name}': {err}")
        })?;
    }

    // Register the murmur:task/task#request-input host function under the
    // versioned name only (see WIT_TASK_IFACE).
    // Components that do not import this interface ignore the registration.
    {
        let task_iface = WIT_TASK_IFACE;
        let ri_task_registry = a2a_task_registry.clone();
        let ri_sse = a2a_sse.clone();
        let ri_task_id = a2a_task_id.clone();
        let ri_timeout = input_timeout_secs;
        linker
            .instance(task_iface)
            .map_err(|err| format!("failed to define {task_iface} instance for '{name}': {err}"))?
            .func_wrap_async(
                "request-input",
                move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>,
                      (prompt,): (String,)| {
                    let reg = ri_task_registry.clone();
                    let sse = ri_sse.clone();
                    let tid = ri_task_id.clone();
                    let fut: std::pin::Pin<
                        Box<dyn std::future::Future<Output = wasmtime::Result<(String,)>> + Send>,
                    > = Box::pin(async move {
                        let result = match (reg, tid) {
                            (Some(reg), Some(tid)) => {
                                request_input_impl(tid, prompt, reg, sse, ri_timeout).await
                            }
                            _ => Err(wasmtime::Error::msg(
                                "request-input is not available outside an A2A task context",
                            )),
                        };
                        result.map(|s| (s,))
                    });
                    Box::new(fut)
                        as Box<
                            dyn std::future::Future<Output = wasmtime::Result<(String,)>>
                                + Send
                                + '_,
                        >
                },
            )
            .map_err(|err| format!("failed to register request-input for tool '{name}': {err}"))?;
    }

    let tool_limits = capability_policy.limits;
    // Both halves of the grant are resolved here, on the one path every WASM tool and the
    // inference driver share, so a driver needs no enforcement code of its own.
    let effective_network_rules = effective_tool_network_rules(artifact_grant, network_allow_rules);
    let filesystem_scope = artifact_grant.and_then(|grant| grant.filesystem_scope.as_deref());
    // Absent for every artifact that declared no `capabilities.state`, so an undeclared tool is
    // built with the workdir preopen and nothing else.
    let state_dir = artifact_grant.and_then(|grant| grant.state_dir.as_deref());
    let state = ToolStoreState {
        limits: tool_limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(
            accessible_workdir,
            filesystem_scope,
            state_dir,
            inference_env,
            capability_policy,
        )
        .map_err(|err| format!("failed to build WASI context for tool '{name}': {err}"))?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: effective_network_rules.to_vec(),
        },
    };

    let mut store = Store::new(engine, state);
    // Registered before instantiation — see the capsule store for why.
    store.limiter(|state| &mut state.limits);

    store.set_epoch_deadline(tool_limits.deadline_ticks());
    let instance = linker
        .instantiate_async(&mut store, component)
        .await
        .map_err(|err| format!("failed to instantiate tool '{name}': {err}"))?;

    let tool_iface = resolve_versioned_iface(&instance, &mut store, WIT_TOOL_IFACE_VERSIONED)
        .ok_or_else(|| {
            RuntimeError::ToolExportMissing {
                name: name.to_string(),
            }
            .to_string()
        })?;
    let tool_run = instance
        .get_export_index(&mut store, Some(&tool_iface), "run")
        .and_then(|idx| instance.get_func(&mut store, idx))
        .ok_or_else(|| {
            RuntimeError::ToolExportMissing {
                name: name.to_string(),
            }
            .to_string()
        })?;

    let run = tool_run
        .typed::<(murmur::tool::run::ToolInput,), (murmur::tool::run::ToolResult,)>(&store)
        .map_err(|err| format!("failed to type-check tool '{name}' run export: {err}"))?;

    // Fresh budget for `run` itself, so instantiation cost cannot eat into it. This is
    // also the driver path (`agent.rs` dispatches the inference driver through here).
    store.set_epoch_deadline(tool_limits.deadline_ticks());
    let called = run.call_async(&mut store, (input,)).await;
    let (result,) = match called {
        Ok(result) => result,
        Err(err) => {
            // Classified rather than folded into the generic "trapped" string, so a
            // deadline or limit trap is distinguishable on a path that reports failures
            // as plain text. `Other` reproduces the pre-slice wording verbatim.
            let failure = classify_guest_failure(&err, &store.data().limits);
            return Err(failure.message(&format!("tool '{name}'"), &err));
        }
    };
    Ok(result)
}

impl CapsuleStoreState {
    /// Async WASM tool dispatch — used by the agent loop for drivers and WASM tools.
    pub(crate) async fn dispatch_tool_async(
        &self,
        name: &str,
        input: murmur::tool::run::ToolInput,
    ) -> Result<murmur::tool::run::ToolResult, String> {
        let Some(component) = self.tool_components.get(name) else {
            return Err(format!("tool '{name}' is not available in this session"));
        };
        invoke_tool_component(
            ToolInvokeEnv {
                engine: &self.engine,
                accessible_workdir: &self.accessible_workdir,
                inference_env: &self.inference_env,
                capability_policy: &self.capability_policy,
                network_allow_rules: &self.network_allow_rules,
                // Absent for every artifact that declared no `capabilities:` block, and for
                // anything pulled in at runtime via `manage.pull()` (which has no operator
                // manifest entry to narrow from) — both keep the full ceiling.
                artifact_grant: self.artifact_grants.get(name),
            },
            ToolA2aWiring {
                sse: self.a2a_sse.clone(),
                task_id: self.a2a_task_id.clone(),
                chunk_event_id: Arc::clone(&self.a2a_chunk_event_id),
                chunks_emitted: Arc::clone(&self.a2a_chunks_emitted),
                task_registry: self.a2a_task_registry.clone(),
                input_timeout_secs: self.input_timeout_secs,
            },
            name,
            component,
            input,
        )
        .await
    }

    /// Dispatch a tool call from the agent loop: native binary, shell, or WASM.
    pub(crate) async fn dispatch_agent_tool_async(
        &self,
        name: &str,
        input: murmur::tool::run::ToolInput,
    ) -> Result<DispatchOutcome, String> {
        // The two runtime-provided peer-handoff tools, intercepted ahead of every other path.
        // They have no artifact, no binary and no component: the manifests under
        // `workdir/tools/` exist so `build_tool_inventory` shows them to the model, and this is
        // where the call actually lands.
        if name == SHARE_FILE_TOOL {
            return self
                .dispatch_share_file(input)
                .await
                .map(DispatchOutcome::tool);
        }
        if name == FETCH_PEER_FILE_TOOL {
            return self
                .dispatch_fetch_peer_file(input)
                .await
                .map(DispatchOutcome::tool);
        }

        // Native artifact: packaged binary in workdir/tools/<name>/<name>
        let native_bin = self.workdir.join("tools").join(name).join(name);
        if native_bin.exists() && !self.tool_components.contains_key(name) {
            return enforce_allowlist(&self.allowlisted_tools, name, || {
                dispatch_native_tool(
                    name,
                    input,
                    &native_bin,
                    &self.accessible_workdir,
                    &self.capability_policy,
                    &self.shell_enforcement,
                )
            })
            .map(DispatchOutcome::tool);
        }

        // Shell tool — run on a blocking thread so the LocalSet stays free to handle
        // incoming HTTP requests (e.g. curl POSTing back to the same capsule's server).
        if self
            .capability_policy
            .shell_allow
            .iter()
            .any(|allowed| allowed == name)
        {
            let name = name.to_string();
            let workdir = self.accessible_workdir.clone();
            let env_overrides = self.inference_env.clone();
            let policy = self.capability_policy.clone();
            let enforcement = self.shell_enforcement.clone();
            return tokio::task::spawn_blocking(move || {
                dispatch_shell_tool(
                    &name,
                    input,
                    &workdir,
                    &env_overrides,
                    &policy,
                    &enforcement,
                )
            })
            .await
            .map_err(|e| format!("shell tool panicked: {e}"));
        }

        // Skill artifact: return skill.md content as the tool result (no WASM dispatch).
        // Works in capsules with no shell/file capabilities — the runtime reads the file.
        let skill_md_path = self.workdir.join("tools").join(name).join("skill.md");
        if skill_md_path.exists() {
            let content = fs::read_to_string(&skill_md_path)
                .map_err(|e| format!("failed to read skill '{name}': {e}"))?;
            return Ok(DispatchOutcome::skill(murmur::tool::run::ToolResult {
                status: murmur::tool::run::Status::Passed,
                summary: Some(format!("Skill {name} guidance")),
                data: Some(content),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            }));
        }

        // WASM tool
        if !self.allowlisted_tools.contains(name) {
            return Err(format!(
                "tool '{name}' is not declared in manifest allowlist"
            ));
        }
        self.dispatch_tool_async(name, input)
            .await
            .map(DispatchOutcome::tool)
    }

    /// `share-file`: mint one handle for one file, for one named peer.
    ///
    /// The audience is derived from the peer's own agent card, fetched here. That fetch is an
    /// ordinary outbound request and is enforced against `capabilities.network.allow` — the same
    /// rule `send::Host::send` applies — so **minting grants no new outbound authority**.
    ///
    /// Returns no filesystem path in any field. The agent asked for the path; echoing it back as
    /// a fact of the handle would make the handle look like an address, which is the one thing it
    /// is not.
    async fn dispatch_share_file(
        &self,
        input: murmur::tool::run::ToolInput,
    ) -> Result<murmur::tool::run::ToolResult, String> {
        let args = parse_tool_json_input(SHARE_FILE_TOOL, &input)?;
        let path = required_string_arg(SHARE_FILE_TOOL, &args, "path")?;
        let peer = required_string_arg(SHARE_FILE_TOOL, &args, "peer")?;
        let ttl = args.get("ttl").and_then(serde_json::Value::as_str);

        let Some(plane) = self.peer_plane.as_ref().filter(|plane| plane.is_declared()) else {
            return Err(format!(
                "'{SHARE_FILE_TOOL}' needs an exports.peer_files block in murmur.yaml; \
                 this capsule declares none"
            ));
        };

        let ttl_secs =
            match ttl {
                None => None,
                Some(text) => Some(murmur_artifact::parse_duration_secs(text).map_err(
                    |message| format!("'{SHARE_FILE_TOOL}' was given an unusable ttl: {message}"),
                )?),
            };

        let audience = match self.peer_audience_for(&peer).await {
            Ok(audience) => audience,
            Err(reason) => {
                self.trace_mint(
                    None,
                    &path,
                    "",
                    None,
                    "peer_unreachable",
                    Some(reason.clone()),
                )
                .await;
                return Err(reason);
            }
        };

        match plane.mint_handle(&path, &audience, ttl_secs) {
            Ok(minted) => {
                self.trace_mint(
                    Some(minted.handle_id.clone()),
                    &minted.path,
                    &minted.audience,
                    Some(minted.expires_at_ms),
                    "ok",
                    None,
                )
                .await;
                Ok(json_tool_result(
                    format!("Minted a handle for '{}' addressed to {}", path, audience),
                    serde_json::json!({
                        "handle": minted.handle,
                        "handle_id": minted.handle_id,
                        "expires_at_ms": minted.expires_at_ms,
                        "audience": minted.audience,
                    }),
                ))
            }
            Err(error) => {
                let reason = error.message();
                self.trace_mint(
                    None,
                    &path,
                    &audience,
                    None,
                    error.code(),
                    Some(reason.clone()),
                )
                .await;
                // Names the authoriser and nothing else: not the host path it resolved to, not
                // what it found there. A refused mint must not become a probe.
                Err(format!(
                    "'{SHARE_FILE_TOOL}' refused '{path}': {reason}. Only files under the \
                     declared exports.peer_files.root may be shared."
                ))
            }
        }
    }

    /// The audience a handle for `peer` must be minted for, read off that peer's own agent card.
    async fn peer_audience_for(&self, peer: &str) -> Result<String, String> {
        check_destination_allowed(
            &self.network_allow_rules,
            peer,
            "capabilities.network.allow",
        )?;
        let card = crate::outgoing::fetch_agent_card(peer)
            .await
            .map_err(|error| format!("peer_unreachable: {error}"))?;
        crate::peer_handoff::audience_from_card(&card)
            .map_err(|error| format!("peer_unreachable: {error}"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn trace_mint(
        &self,
        handle_id: Option<String>,
        path: &str,
        audience: &str,
        expires_at_ms: Option<u64>,
        outcome: &str,
        reason: Option<String>,
    ) {
        if let Some(trace) = &self.peer_trace {
            trace
                .write_peer_handle_mint(handle_id, path, audience, expires_at_ms, outcome, reason)
                .await;
        }
    }

    /// `fetch-peer-file`: redeem a handle a peer sent and land the bytes as a file.
    ///
    /// The bytes are never placed in the result and never returned as text. Ingestion is a file
    /// the agent must decide to read, not context it is handed — a peer that can put arbitrary
    /// content in front of this model would otherwise have a prompt-injection channel that costs
    /// it nothing.
    async fn dispatch_fetch_peer_file(
        &self,
        input: murmur::tool::run::ToolInput,
    ) -> Result<murmur::tool::run::ToolResult, String> {
        let args = parse_tool_json_input(FETCH_PEER_FILE_TOOL, &input)?;
        let peer = required_string_arg(FETCH_PEER_FILE_TOOL, &args, "peer")?;
        let handle = required_string_arg(FETCH_PEER_FILE_TOOL, &args, "handle")?;
        let handle_id = crate::peer_handoff::handle_id(&handle);

        // Before any connection is opened, so a refused destination is never contacted at all.
        if let Err(reason) = check_destination_allowed(
            &self.peer_fetch_rules,
            &peer,
            "capabilities.peer_fetch.allow",
        ) {
            self.trace_fetch(
                &peer,
                &handle_id,
                None,
                "peer_not_allowed",
                Some(reason.clone()),
            )
            .await;
            return Err(reason);
        }

        let response = match crate::outgoing::redeem_peer_handle(
            &peer,
            &handle,
            &self.peer_own_audience,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                self.trace_fetch(
                    &peer,
                    &handle_id,
                    None,
                    "peer_unreachable",
                    Some(error.clone()),
                )
                .await;
                return Err(format!(
                    "'{FETCH_PEER_FILE_TOOL}' could not reach {peer}: {error}"
                ));
            }
        };

        if response.status != 200 {
            // The peer's own refusal code, restated rather than flattened: `handle_expired` and
            // `handle_not_valid` mean different things to whoever reads the trace.
            let code = serde_json::from_slice::<serde_json::Value>(&response.body)
                .ok()
                .and_then(|body| {
                    body.get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("http_{}", response.status));
            let reason = format!("{peer} refused the handle: {code}");
            self.trace_fetch(&peer, &handle_id, None, &code, Some(reason.clone()))
                .await;
            return Err(format!("'{FETCH_PEER_FILE_TOOL}' failed: {reason}"));
        }

        let sha256 = murmur_artifact::sha256_hex(&response.body);
        // The validator the peer served describes the body it accompanies, so disagreeing with it
        // means the bytes were not the ones that were hashed. Refuse rather than store.
        if let Some(etag) = response.header("etag") {
            let expected = format!("\"sha256:{sha256}\"");
            if etag != expected {
                let reason = format!("{peer} served an etag that does not describe its own body");
                self.trace_fetch(
                    &peer,
                    &handle_id,
                    None,
                    "etag_mismatch",
                    Some(reason.clone()),
                )
                .await;
                return Err(format!("'{FETCH_PEER_FILE_TOOL}' failed: {reason}"));
            }
        }
        let generation = response
            .header("x-murmur-generation")
            .and_then(|value| value.parse::<u64>().ok());

        // Runtime-chosen, never peer-chosen: the peer discloses no path, and the basename is only
        // a hint read out of the token's own unverified payload.
        let basename = crate::peer_handoff::decode_payload_unverified(&handle).map(|p| p.p);
        let stored_path = crate::peer_handoff::stored_path_for(&handle_id, basename.as_deref());
        let absolute = self.accessible_workdir.join(&stored_path);
        if let Some(parent) = absolute.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                let reason = format!("failed to create {}: {error}", parent.display());
                self.trace_fetch(&peer, &handle_id, None, "io_error", Some(reason.clone()))
                    .await;
                return Err(format!("'{FETCH_PEER_FILE_TOOL}' failed: {reason}"));
            }
        }
        if let Err(error) = fs::write(&absolute, &response.body) {
            let reason = format!("failed to write {stored_path}: {error}");
            self.trace_fetch(&peer, &handle_id, None, "io_error", Some(reason.clone()))
                .await;
            return Err(format!("'{FETCH_PEER_FILE_TOOL}' failed: {reason}"));
        }

        if let Some(trace) = &self.peer_trace {
            trace
                .write_peer_file_fetch(
                    &peer,
                    &handle_id,
                    Some(stored_path.clone()),
                    Some(response.body.len() as u64),
                    Some(sha256.clone()),
                    "ok",
                    None,
                )
                .await;
        }

        let mut data = serde_json::json!({
            "path": stored_path,
            "bytes": response.body.len(),
            "sha256": sha256,
            "peer": peer,
        });
        if let Some(generation) = generation {
            data["generation"] = serde_json::json!(generation);
        }
        Ok(json_tool_result(
            format!(
                "Stored {} bytes from {peer} at {stored_path}",
                response.body.len()
            ),
            data,
        ))
    }

    async fn trace_fetch(
        &self,
        peer: &str,
        handle_id: &str,
        stored_path: Option<String>,
        outcome: &str,
        reason: Option<String>,
    ) {
        if let Some(trace) = &self.peer_trace {
            trace
                .write_peer_file_fetch(peer, handle_id, stored_path, None, None, outcome, reason)
                .await;
        }
    }
}

/// Refuses a destination that no rule in `rules` covers, naming the manifest key that would have
/// to allow it.
///
/// The same `RequestTarget`/`NetworkAllowRule` pair `send::Host::send` uses, applied to a second,
/// separate list — so `capabilities.peer_fetch.allow` and `capabilities.network.allow` are
/// enforced by one matcher and can never drift into two dialects.
fn check_destination_allowed(
    rules: &[NetworkAllowRule],
    peer_url: &str,
    field: &str,
) -> Result<(), String> {
    let for_parse = if peer_url.contains("://") {
        peer_url.to_string()
    } else {
        format!("http://{peer_url}")
    };
    let uri: http::Uri = for_parse
        .parse()
        .map_err(|e| format!("invalid peer URL '{peer_url}': {e}"))?;
    let target = RequestTarget::from_request(&uri, false)
        .ok_or_else(|| format!("invalid peer URL '{peer_url}'"))?;
    if rules.iter().any(|rule| rule.matches(&target)) {
        return Ok(());
    }
    Err(format!("network policy: '{peer_url}' not in {field}"))
}

/// One tool call's `data` field, parsed as a JSON object.
fn parse_tool_json_input(
    tool: &str,
    input: &murmur::tool::run::ToolInput,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let raw = input.data.as_deref().unwrap_or("{}");
    let raw = if raw.trim().is_empty() { "{}" } else { raw };
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        _ => Err(format!("'{tool}' expects a JSON object as its input")),
    }
}

fn required_string_arg(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("'{tool}' requires a non-empty '{key}'"))
}

/// A passing tool result whose `data` is a JSON object. The agent loop sends `data || summary` to
/// the model, so the object is what the model reads back.
fn json_tool_result(summary: String, data: serde_json::Value) -> murmur::tool::run::ToolResult {
    murmur::tool::run::ToolResult {
        status: murmur::tool::run::Status::Passed,
        summary: Some(summary),
        data: Some(data.to_string()),
        data_path: None,
        truncated: false,
        metadata: Vec::new(),
    }
}

fn read_artifact_info_from_manifest(
    manifest_path: &Path,
    fallback: &InstalledArtifactSummary,
) -> Result<manage::ArtifactInfo, String> {
    let manifest_content = fs::read_to_string(manifest_path)
        .map_err(|err| format!("failed to read {}: {err}", manifest_path.display()))?;

    let value: Value = serde_yaml::from_str(&manifest_content)
        .map_err(|err| format!("failed to parse {}: {err}", manifest_path.display()))?;

    let root = value.as_mapping();

    let description = root
        .and_then(|mapping| mapping.get(Value::String("description".to_string())))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let tags = root
        .and_then(|mapping| mapping.get(Value::String("tags".to_string())))
        .and_then(Value::as_sequence)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let input_schema = root
        .and_then(|mapping| mapping.get(Value::String("input_schema".to_string())))
        .map(yaml_to_json_string)
        .transpose()?;
    let output_schema = root
        .and_then(|mapping| mapping.get(Value::String("output_schema".to_string())))
        .map(yaml_to_json_string)
        .transpose()?;

    Ok(manage::ArtifactInfo {
        name: fallback.name.clone(),
        version: fallback.version.clone(),
        description,
        tags,
        runtime: runtime_type_to_wit(&fallback.runtime, fallback.implementation.as_ref()),
        input_schema,
        output_schema,
    })
}

fn yaml_to_json_string(value: &Value) -> Result<String, String> {
    if let Value::String(s) = value {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
            return serde_json::to_string(&parsed)
                .map_err(|err| format!("failed to convert schema to JSON: {err}"));
        }
    }
    serde_json::to_string(value).map_err(|err| format!("failed to convert schema to JSON: {err}"))
}

fn runtime_type_to_wit(
    runtime: &ArtifactRuntime,
    implementation: Option<&ArtifactImplementation>,
) -> manage::RuntimeType {
    match (runtime, implementation) {
        (ArtifactRuntime::Tool, Some(ArtifactImplementation::Native)) => {
            manage::RuntimeType::Native
        }
        _ => manage::RuntimeType::Wasm,
    }
}

struct ToolStoreState {
    /// Resource limiter for this store — see [`CapsuleStoreState::limits`].
    limits: ExecutionLimiter,
    table: ResourceTable,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    http_hooks: NetworkPolicyHooks,
}

impl WasiView for ToolStoreState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for ToolStoreState {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.http_hooks,
        }
    }
}

/// Write a single artifact's `murmur.yaml` under `<workdir>/tools/<name>/`.
///
/// Used both by `stage_session` (for every artifact declared in the manifest) and by
/// `manage.pull()` (for the single artifact it just fetched at runtime).
fn write_tool_manifest(
    workdir: &Path,
    name: &str,
    manifest_yaml: &str,
) -> Result<(), RuntimeError> {
    let manifest_path = workdir.join("tools").join(name).join(PACKED_MANIFEST_ENTRY);
    let Some(parent) = manifest_path.parent() else {
        return Err(RuntimeError::Runtime(format!(
            "failed to derive parent for tool manifest path {}",
            manifest_path.display()
        )));
    };

    fs::create_dir_all(parent).map_err(|source| RuntimeError::WriteToolManifest {
        path: manifest_path.display().to_string(),
        source,
    })?;
    fs::write(&manifest_path, manifest_yaml).map_err(|source| RuntimeError::WriteToolManifest {
        path: manifest_path.display().to_string(),
        source,
    })
}

fn install_native_binaries(
    workdir: &Path,
    native_binaries: Vec<(String, Vec<u8>)>,
) -> Result<(), RuntimeError> {
    for (name, bytes) in native_binaries {
        let binary_path = workdir.join("tools").join(&name).join(&name);
        let Some(parent) = binary_path.parent() else {
            return Err(RuntimeError::Runtime(format!(
                "failed to derive parent for native binary path {}",
                binary_path.display()
            )));
        };
        fs::create_dir_all(parent).map_err(|source| RuntimeError::WriteToolManifest {
            path: binary_path.display().to_string(),
            source,
        })?;
        fs::write(&binary_path, &bytes).map_err(|source| RuntimeError::WriteToolManifest {
            path: binary_path.display().to_string(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&binary_path)
                .map_err(|source| RuntimeError::WriteToolManifest {
                    path: binary_path.display().to_string(),
                    source,
                })?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&binary_path, perms).map_err(|source| {
                RuntimeError::WriteToolManifest {
                    path: binary_path.display().to_string(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

/// Resolve and read a local-source skill's `skill.md` bytes.
///
/// `source` may be a relative or absolute path. Relative paths resolve against `manifest_dir`
/// (the directory containing `murmur.yaml`). The resolved path may be:
///   - a file (assumed to be `skill.md` itself), read directly, or
///   - a directory, in which case `skill.md` is located case-insensitively within it.
///
/// Errors before the workdir is created so that failures exit non-zero with no side effects.
fn load_local_skill_md(manifest_dir: &Path, source: &str) -> Result<Vec<u8>, RuntimeError> {
    let raw = Path::new(source);
    let path = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        manifest_dir.join(raw)
    };

    if !path.exists() {
        return Err(RuntimeError::SkillSourceNotFound {
            path: path.display().to_string(),
        });
    }

    let skill_md_path = if path.is_dir() {
        find_skill_md_in_dir(&path)?.ok_or_else(|| RuntimeError::SkillSourceMissingSkillMd {
            path: path.display().to_string(),
        })?
    } else {
        path.clone()
    };

    fs::read(&skill_md_path).map_err(|source| RuntimeError::SkillSourceRead {
        path: skill_md_path.display().to_string(),
        source,
    })
}

/// Locate `skill.md` in a directory, matching the filename case-insensitively. First match wins.
fn find_skill_md_in_dir(dir: &Path) -> Result<Option<std::path::PathBuf>, RuntimeError> {
    let entries = fs::read_dir(dir).map_err(|source| RuntimeError::SkillSourceRead {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| RuntimeError::SkillSourceRead {
            path: dir.display().to_string(),
            source,
        })?;
        if entry.file_name().to_string_lossy().to_lowercase() == "skill.md" {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn install_skill_files(
    workdir: &Path,
    skill_files: Vec<(String, Vec<u8>)>,
) -> Result<(), RuntimeError> {
    for (name, bytes) in skill_files {
        let skill_path = workdir.join("tools").join(&name).join("skill.md");
        let Some(parent) = skill_path.parent() else {
            return Err(RuntimeError::Runtime(format!(
                "failed to derive parent for skill.md path {}",
                skill_path.display()
            )));
        };
        fs::create_dir_all(parent).map_err(|source| RuntimeError::WriteToolManifest {
            path: skill_path.display().to_string(),
            source,
        })?;
        fs::write(&skill_path, &bytes).map_err(|source| RuntimeError::WriteToolManifest {
            path: skill_path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Refuses a persistent capsule that declares `exports.peer_files` without a short enough
/// `max_ttl`.
///
/// The rule keys on `lifecycle.after_task`, which is the manifest's ephemerality axis. `exit` —
/// the default — is ephemeral: the capsule dies with the task and teardown destroys the minting
/// key, so every outstanding handle stops verifying at once and the declared lifetime can never
/// be the real bound. `sleep` is the operator's opt-out: the capsule stays alive, its instance
/// key stays alive, and a handle sitting in persisted A2A message history stays redeemable — so
/// there the declared lifetime *is* the bound, and it must be declared and capped.
fn check_persistent_handle_ttl(
    peer_files: Option<&murmur_artifact::PeerFilesExport>,
    lifecycle: &LifecycleConfig,
) -> Result<(), RuntimeError> {
    let Some(export) = peer_files else {
        return Ok(());
    };
    if lifecycle.after_task != murmur_artifact::AfterTask::Sleep {
        return Ok(());
    }
    let ceiling = murmur_artifact::PERSISTENT_PEER_HANDLE_TTL_CEILING_SECS;
    match export.max_ttl_secs {
        Some(declared) if declared <= ceiling => Ok(()),
        declared => Err(RuntimeError::PersistentCapsuleNeedsHandleTtl {
            declared_secs: declared,
            ceiling_secs: ceiling,
        }),
    }
}

/// Writes the synthetic tool manifests for the two peer-handoff tools, each only when its own
/// grant is declared.
fn write_peer_handoff_tool_manifests(
    workdir: &Path,
    can_mint: bool,
    can_fetch: bool,
) -> Result<(), RuntimeError> {
    if can_mint {
        write_tool_manifest(workdir, SHARE_FILE_TOOL, SHARE_FILE_TOOL_MANIFEST)?;
    }
    if can_fetch {
        write_tool_manifest(workdir, FETCH_PEER_FILE_TOOL, FETCH_PEER_FILE_TOOL_MANIFEST)?;
    }
    Ok(())
}

/// Tool the minting side gains from `exports.peer_files`.
pub(crate) const SHARE_FILE_TOOL: &str = "share-file";

/// Tool the ingesting side gains from `capabilities.peer_fetch`.
pub(crate) const FETCH_PEER_FILE_TOOL: &str = "fetch-peer-file";

/// `share-file`'s manifest. The description tells the model the two things it cannot work out
/// from the schema: that the returned handle is what goes into the message, and that the path is
/// relative to the declared export root rather than to the workdir.
const SHARE_FILE_TOOL_MANIFEST: &str = concat!(
    "name: share-file\n",
    "version: 0.0.0\n",
    "runtime: tool\n",
    "implementation: native\n",
    "description: \"Mint an opaque handle a named peer can use to fetch one file from this ",
    "capsule's declared peer export. `path` is relative to exports.peer_files.root; `peer` is ",
    "the peer's address. Returns a handle to put in a message to that peer — it names no ",
    "filesystem path and is redeemable only by that peer.\"\n",
    "input_schema: '",
    r#"{"type":"object","properties":{"path":{"type":"string"},"peer":{"type":"string"},"ttl":{"type":"string"}},"required":["path","peer"]}"#,
    "'\n",
);

/// `fetch-peer-file`'s manifest. It states plainly that the bytes arrive as a file, because a
/// model that expects them inline will otherwise ask for them again.
const FETCH_PEER_FILE_TOOL_MANIFEST: &str = concat!(
    "name: fetch-peer-file\n",
    "version: 0.0.0\n",
    "runtime: tool\n",
    "implementation: native\n",
    "description: \"Redeem a handle a peer sent and store the file it names in this capsule's ",
    "workdir. Returns the stored path, size and SHA-256 — never the file's contents. Read the ",
    "stored path if you need what is in it.\"\n",
    "input_schema: '",
    r#"{"type":"object","properties":{"peer":{"type":"string"},"handle":{"type":"string"}},"required":["peer","handle"]}"#,
    "'\n",
);

fn write_shell_tool_manifests(workdir: &Path, shell_allow: &[String]) -> Result<(), RuntimeError> {
    for binary in shell_allow {
        let manifest_path = workdir
            .join("tools")
            .join(binary)
            .join(PACKED_MANIFEST_ENTRY);

        if manifest_path.exists() {
            continue;
        }

        let Some(parent) = manifest_path.parent() else {
            return Err(RuntimeError::Runtime(format!(
                "failed to derive parent for shell manifest path {}",
                manifest_path.display()
            )));
        };

        fs::create_dir_all(parent).map_err(|source| RuntimeError::WriteToolManifest {
            path: manifest_path.display().to_string(),
            source,
        })?;

        fs::write(&manifest_path, shell_tool_manifest_yaml(binary)).map_err(|source| {
            RuntimeError::WriteToolManifest {
                path: manifest_path.display().to_string(),
                source,
            }
        })?;
    }

    Ok(())
}

/// Execute a native artifact binary.
///
/// The binary receives the serialized ToolInput JSON on stdin and must write a valid
/// ToolResult JSON object to stdout. The binary's working directory is the capsule workdir.
fn dispatch_native_tool(
    name: &str,
    input: murmur::tool::run::ToolInput,
    binary_path: &Path,
    workdir: &Path,
    policy: &CapabilityPolicy,
    enforcement: &sandbox::ShellEnforcement,
) -> Result<murmur::tool::run::ToolResult, String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    let input_json = serde_json::to_string(&serde_json::json!({
        "data": input.data,
        "log_path": input.log_path,
    }))
    .map_err(|e| format!("failed to serialize input for native tool '{name}': {e}"))?;

    enforcement.check_workdir_budget()?;

    let env = build_shell_env(policy, &[], workdir)?;

    // Bound to a local before spawning (rather than chained straight into `.spawn()`) so a
    // `pre_exec` step can be attached to it, mirroring `execute_shell`'s shape.
    let mut command = Command::new(binary_path);
    command
        .current_dir(workdir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // This path carries the same hard rlimits and the same cgroup membership as `execute_shell`,
    // but installs no seccomp filter and no Landlock scope. That asymmetry is deliberate and
    // open: a native-implementation artifact is bounded but not confined.
    sandbox::attach_process_limits(&mut command, enforcement);
    // Mark every fd >= 3 close-on-exec in the forked child, so this subprocess inherits only the
    // stdio pipes configured just above and nothing that merely happened to be open in the
    // runtime process. Fds 0/1/2 are deliberately excluded: this function writes the tool's
    // `ToolInput` JSON to the child's stdin and parses its `ToolResult` JSON back off stdout, so
    // stdio inheritance across `execve` is the point, not a leak — see
    // `sandbox::FD_HYGIENE_FIRST_FD` for the full recorded decision.
    //
    // This is the shared helper `execute_shell`'s path also runs (as the first step of
    // `sandbox::linux_enforce::child_install_enforcement`), so the two spawn paths cannot drift
    // apart on this dimension. It takes no policy input — fd hygiene is unconditional — and it
    // deliberately does not bring seccomp/Landlock enforcement with it; kernel sandboxing of
    // native tool subprocesses remains the documented gap it was.
    crate::sandbox::apply_fd_hygiene(&mut command);

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn native tool '{name}': {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input_json.as_bytes());
        // stdin closes when dropped, signalling EOF to the child
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("native tool '{name}' failed to complete: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if stdout.trim().is_empty() {
        return Ok(murmur::tool::run::ToolResult {
            status: if output.status.success() {
                murmur::tool::run::Status::Passed
            } else {
                murmur::tool::run::Status::Error
            },
            summary: Some(format!(
                "native tool '{}' exited with {}",
                name, output.status
            )),
            data: if stderr.is_empty() {
                None
            } else {
                Some(stderr.to_string())
            },
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        });
    }

    match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
        Ok(json) => {
            let status = match json.get("status").and_then(|s| s.as_str()) {
                Some("passed") => murmur::tool::run::Status::Passed,
                Some("failed") => murmur::tool::run::Status::Failed,
                _ => murmur::tool::run::Status::Error,
            };
            let summary = json
                .get("summary")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string());
            let data = json
                .get("data")
                .and_then(|d| d.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    json.get("data")
                        .filter(|d| !d.is_null())
                        .map(|d| d.to_string())
                });
            Ok(murmur::tool::run::ToolResult {
                status,
                summary,
                data,
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            })
        }
        Err(_) => Ok(murmur::tool::run::ToolResult {
            status: if output.status.success() {
                murmur::tool::run::Status::Passed
            } else {
                murmur::tool::run::Status::Error
            },
            summary: Some(format!("native tool '{}' completed", name)),
            data: Some(stdout.to_string()),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }),
    }
}

fn dispatch_shell_tool(
    name: &str,
    input: murmur::tool::run::ToolInput,
    workdir: &Path,
    env_overrides: &[(String, String)],
    policy: &CapabilityPolicy,
    enforcement: &sandbox::ShellEnforcement,
) -> DispatchOutcome {
    let command = match extract_shell_command(&input) {
        Ok(command) => command,
        Err(error) => {
            return DispatchOutcome::tool(murmur::tool::run::ToolResult {
                status: murmur::tool::run::Status::Error,
                summary: Some("shell command parsing failed".to_string()),
                data: Some(error),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            });
        }
    };

    let split_args: Vec<String>;
    let args: Vec<&str> = if is_shell_interpreter(name) {
        vec!["-c", command.as_str()]
    } else {
        split_args = split_shell_words(&command);
        split_args.iter().map(String::as_str).collect()
    };

    match execute_shell(name, &args, env_overrides, workdir, policy, enforcement) {
        Ok(result) => {
            let shell = ShellDispatchInfo {
                // `name` is the invoked binary (each `capabilities.shell.allow` entry is
                // exposed as its own tool), resolved to a path by `execute_shell`.
                binary: result.binary.clone(),
                command: command.clone(),
                exit_code: result.exit_code,
                stdout: result.stdout.clone(),
                stderr: result.stderr.clone(),
                stdout_bytes: result.stdout.len() as u64,
                stderr_bytes: result.stderr.len() as u64,
                duration_ms: result.duration_ms,
                resource_limit: result.resource_limit_hit.clone(),
            };
            DispatchOutcome {
                result: shell_result_to_tool_result(&command, result),
                shell: Some(shell),
                is_skill: false,
                fatal: None,
            }
        }
        Err(error) => DispatchOutcome {
            // The tool result is filled in either way, so the trace records the call that was
            // attempted; `fatal` is what tells the agent turn loop that this particular failure
            // is not one the capsule gets another turn to react to.
            result: murmur::tool::run::ToolResult {
                status: murmur::tool::run::Status::Error,
                summary: Some("shell execution failed".to_string()),
                data: Some(error.to_string()),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
            },
            shell: None,
            is_skill: false,
            fatal: error.session_fatal(),
        },
    }
}

fn extract_shell_command(input: &murmur::tool::run::ToolInput) -> Result<String, String> {
    let data = input
        .data
        .as_deref()
        .ok_or_else(|| "shell tool input.data is required".to_string())?;

    let json: serde_json::Value = serde_json::from_str(data)
        .map_err(|error| format!("shell tool input must be valid JSON: {error}"))?;

    let command = json
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "shell tool input must include a non-empty 'command' string".to_string())?;

    Ok(command.to_string())
}

fn shell_result_to_tool_result(
    command: &str,
    result: ShellResult,
) -> murmur::tool::run::ToolResult {
    let mut data = format!(
        "$ {}\nExit code: {}\nStdout:\n{}\nStderr:\n{}",
        command, result.exit_code, result.stdout, result.stderr
    );

    let mut metadata = Vec::new();
    if result.truncated {
        if let Some(path) = result.full_output_path.as_ref() {
            data.push_str(&format!(
                "\n\nOutput truncated. Full output written to {path}"
            ));
            metadata.push(("full_output_path".to_string(), path.clone()));
        } else {
            data.push_str("\n\nOutput truncated.");
        }
    }

    murmur::tool::run::ToolResult {
        status: murmur::tool::run::Status::Passed,
        summary: Some(format!(
            "Shell command exited with code {}",
            result.exit_code
        )),
        data: Some(data),
        data_path: None,
        truncated: result.truncated,
        metadata,
    }
}

fn resolve_context_window(context: Option<&ContextConfig>) -> u32 {
    context.and_then(|c| c.max_tokens).unwrap_or(0)
}

fn validate_capability_policy(policy: &CapabilityPolicy) -> Result<(), RuntimeError> {
    parse_network_allow_rules(&policy.network_allow)?;
    if let Some(scope) = policy.filesystem_scope.as_deref() {
        validate_filesystem_scope(scope)?;
    }

    Ok(())
}

fn enforce_allowlist<T, F>(
    allowlisted_tools: &HashSet<String>,
    name: &str,
    dispatch: F,
) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    if !allowlisted_tools.contains(name) {
        return Err(format!(
            "tool '{name}' is not declared in manifest allowlist"
        ));
    }

    dispatch()
}

fn generate_session_id() -> String {
    format!("ses_{}", uuid::Uuid::now_v7().simple())
}

fn resolve_lifecycle(
    base: Option<LifecycleConfig>,
    override_: Option<&murmur_artifact::LifecycleOverride>,
) -> LifecycleConfig {
    let mut config = base.unwrap_or_default();
    if let Some(ov) = override_ {
        if let Some(ta) = &ov.task_acceptance {
            config.task_acceptance = ta.clone();
        }
        if let Some(at) = &ov.after_task {
            config.after_task = at.clone();
        }
    }
    config
}

#[cfg(test)]
mod tests {

    // ── The persistent-capsule handle-TTL rule ───────────────────────────────

    fn peer_export(max_ttl_secs: Option<u64>) -> murmur_artifact::PeerFilesExport {
        murmur_artifact::PeerFilesExport {
            root: "out/".to_string(),
            max_ttl_secs,
            max_bytes: 10 * 1024 * 1024,
        }
    }

    fn lifecycle_with(after_task: murmur_artifact::AfterTask) -> LifecycleConfig {
        LifecycleConfig {
            after_task,
            ..Default::default()
        }
    }

    /// An ephemeral capsule needs no ceiling at all: teardown destroys the key, so the declared
    /// lifetime can never be the real bound however long it is.
    #[test]
    fn an_ephemeral_capsule_may_declare_any_handle_ttl_or_none() {
        for declared in [None, Some(1), Some(900), Some(86_400), Some(u64::MAX)] {
            assert!(
                check_persistent_handle_ttl(
                    Some(&peer_export(declared)),
                    &lifecycle_with(murmur_artifact::AfterTask::Exit),
                )
                .is_ok(),
                "exit + max_ttl {declared:?} must launch"
            );
        }
    }

    #[test]
    fn a_persistent_capsule_must_declare_a_handle_ttl_at_or_under_the_ceiling() {
        for declared in [Some(1), Some(600), Some(900)] {
            assert!(
                check_persistent_handle_ttl(
                    Some(&peer_export(declared)),
                    &lifecycle_with(murmur_artifact::AfterTask::Sleep),
                )
                .is_ok(),
                "sleep + max_ttl {declared:?} must launch"
            );
        }
        for declared in [None, Some(901), Some(1800), Some(3600)] {
            let error = check_persistent_handle_ttl(
                Some(&peer_export(declared)),
                &lifecycle_with(murmur_artifact::AfterTask::Sleep),
            )
            .expect_err("sleep + max_ttl {declared:?} must refuse");
            assert!(matches!(
                error,
                RuntimeError::PersistentCapsuleNeedsHandleTtl { .. }
            ));
            let rendered = error.to_string();
            assert!(
                rendered.contains("exports.peer_files.max_ttl"),
                "{rendered}"
            );
            assert!(
                rendered.contains("lifecycle.after_task: sleep"),
                "{rendered}"
            );
            assert!(rendered.contains("900s"), "{rendered}");
            assert!(rendered.contains("durability"), "{rendered}");
        }
    }

    /// The rule is about handles, so a capsule that declares no peer export is never asked.
    #[test]
    fn a_capsule_without_peer_files_is_unaffected_by_the_ttl_rule() {
        for after_task in [
            murmur_artifact::AfterTask::Exit,
            murmur_artifact::AfterTask::Sleep,
        ] {
            assert!(check_persistent_handle_ttl(None, &lifecycle_with(after_task)).is_ok());
        }
    }
    use murmur_artifact::{ArtifactMeta, ArtifactRuntime, Registry, ResolvedArtifact, RuntimeType};
    use tempfile::TempDir;

    use super::*;

    fn bootstrap_log_contents(workdir: &Path) -> String {
        fs::read_to_string(workdir.join("logs").join("bootstrap.log")).unwrap_or_default()
    }

    /// An `InferenceConfig` with every system-prompt field empty, for the prompt-resolution
    /// tests below to fill in one at a time.
    fn inference_without_prompt() -> murmur_artifact::InferenceConfig {
        murmur_artifact::InferenceConfig {
            transport: "http".to_string(),
            endpoint: Some("http://127.0.0.1:1".to_string()),
            model: "test-model".to_string(),
            api_key: None,
            driver: None,
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
        }
    }

    #[test]
    fn resolve_system_prompt_returns_none_when_nothing_is_declared() {
        let tmp = TempDir::new().unwrap();
        let resolved =
            resolve_system_prompt(tmp.path(), tmp.path(), &inference_without_prompt()).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn resolve_system_prompt_returns_the_inline_prompt_verbatim() {
        let tmp = TempDir::new().unwrap();
        let inference = murmur_artifact::InferenceConfig {
            system_prompt: Some("Be terse.".to_string()),
            ..inference_without_prompt()
        };
        let resolved = resolve_system_prompt(tmp.path(), tmp.path(), &inference).unwrap();
        assert_eq!(resolved.as_deref(), Some("Be terse."));
    }

    /// File contents are used exactly as they sit on disk — trailing newline included. The
    /// trimming that applies to the inline form happens at manifest parse time and has no
    /// counterpart here.
    #[test]
    fn resolve_system_prompt_reads_the_prompt_file_verbatim_relative_to_the_manifest_dir() {
        let manifest_dir = TempDir::new().unwrap();
        let workdir = TempDir::new().unwrap();
        fs::write(manifest_dir.path().join("conventions.md"), "  Be terse.\n").unwrap();

        let inference = murmur_artifact::InferenceConfig {
            system_prompt_file: Some("conventions.md".to_string()),
            ..inference_without_prompt()
        };
        let resolved =
            resolve_system_prompt(manifest_dir.path(), workdir.path(), &inference).unwrap();
        assert_eq!(resolved.as_deref(), Some("  Be terse.\n"));
    }

    #[test]
    fn resolve_system_prompt_errors_when_the_prompt_file_is_missing() {
        let tmp = TempDir::new().unwrap();
        let inference = murmur_artifact::InferenceConfig {
            system_prompt_file: Some("missing-conventions.md".to_string()),
            ..inference_without_prompt()
        };
        match resolve_system_prompt(tmp.path(), tmp.path(), &inference) {
            Err(RuntimeError::SystemPromptFileRead { path, .. }) => {
                assert!(path.ends_with("missing-conventions.md"), "got {path}");
            }
            other => panic!("expected SystemPromptFileRead, got {other:?}"),
        }
    }

    /// The artifact branch reads the staged skill's `skill.md` out of the *workdir*, not the
    /// manifest directory — it is a resolved artifact, not a file the author wrote beside
    /// murmur.yaml.
    #[test]
    fn resolve_system_prompt_reads_skill_md_from_the_staged_artifact() {
        let manifest_dir = TempDir::new().unwrap();
        let workdir = TempDir::new().unwrap();
        let skill_dir = workdir.path().join("tools").join("house-style");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("skill.md"), "# House style\nBe terse.").unwrap();

        let inference = murmur_artifact::InferenceConfig {
            system_prompt_artifact: Some("house-style".to_string()),
            ..inference_without_prompt()
        };
        let resolved =
            resolve_system_prompt(manifest_dir.path(), workdir.path(), &inference).unwrap();
        assert_eq!(resolved.as_deref(), Some("# House style\nBe terse."));
    }

    #[test]
    fn resolve_system_prompt_errors_when_the_prompt_artifact_has_no_skill_md() {
        let tmp = TempDir::new().unwrap();
        let inference = murmur_artifact::InferenceConfig {
            system_prompt_artifact: Some("house-style".to_string()),
            ..inference_without_prompt()
        };
        match resolve_system_prompt(tmp.path(), tmp.path(), &inference) {
            Err(RuntimeError::SystemPromptArtifactRead { name, .. }) => {
                assert_eq!(name, "house-style");
            }
            other => panic!("expected SystemPromptArtifactRead, got {other:?}"),
        }
    }

    /// `mur run --system-prompt` overrides by clearing the other two fields and setting the
    /// inline one, so resolution never reaches a declaration the operator replaced — including
    /// a `system_prompt_file` pointing at a file that does not exist.
    #[test]
    fn resolve_system_prompt_ignores_cleared_declarations() {
        let tmp = TempDir::new().unwrap();
        let inference = murmur_artifact::InferenceConfig {
            system_prompt: Some("CLI prompt".to_string()),
            system_prompt_file: None,
            system_prompt_artifact: None,
            ..inference_without_prompt()
        };
        let resolved = resolve_system_prompt(tmp.path(), tmp.path(), &inference).unwrap();
        assert_eq!(resolved.as_deref(), Some("CLI prompt"));
    }

    #[test]
    fn bash_and_network_together_trigger_warning() {
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            network_allow: vec!["https://api.example.com".to_string()],
            ..Default::default()
        };
        warn_if_bash_network_bypass(tmp.path(), &policy);
        let log = bootstrap_log_contents(tmp.path());
        assert!(log.contains("bash"), "log should mention bash: {log}");
        assert!(log.contains("network"), "log should mention network: {log}");
        assert!(
            log.contains(W_SEC_003),
            "log should carry its warning code: {log}"
        );
        assert!(
            log.contains(&security_warning_link(W_SEC_003)),
            "log should link to the diagnostics doc page: {log}"
        );
    }

    fn compaction_config(
        system_prompt: Option<&str>,
        system_prompt_file: Option<&str>,
    ) -> murmur_artifact::CompactionConfig {
        murmur_artifact::CompactionConfig {
            threshold: None,
            model: Some("compaction-model".to_string()),
            system_prompt: system_prompt.map(str::to_string),
            system_prompt_file: system_prompt_file.map(str::to_string),
            dump_summaries: None,
        }
    }

    /// `system_prompt_file` is read relative to the manifest directory — not the process
    /// cwd — and its contents reach the hook verbatim, newlines and all.
    #[test]
    fn compaction_system_prompt_file_resolves_relative_to_manifest_dir() {
        let manifest_dir = TempDir::new().unwrap();
        let body = "Summarize aggressively.\nKeep file paths.\n";
        fs::write(manifest_dir.path().join("compaction-instructions.md"), body).unwrap();

        let resolved = resolve_compaction_system_prompt(
            manifest_dir.path(),
            Some(&compaction_config(None, Some("compaction-instructions.md"))),
        )
        .expect("file resolves");

        assert_eq!(resolved, Some(body.to_string()));
    }

    /// The inline field keeps its pre-existing behavior: returned as-is, no file touched.
    #[test]
    fn compaction_inline_system_prompt_resolves_without_reading_a_file() {
        let manifest_dir = TempDir::new().unwrap();

        let resolved = resolve_compaction_system_prompt(
            manifest_dir.path(),
            Some(&compaction_config(Some("inline prompt"), None)),
        )
        .expect("inline prompt resolves");

        assert_eq!(resolved, Some("inline prompt".to_string()));
    }

    /// Neither prompt source set — and no `compaction:` block at all — both stay `None`;
    /// nothing on this path substitutes a default prompt.
    #[test]
    fn compaction_system_prompt_absent_resolves_to_none() {
        let manifest_dir = TempDir::new().unwrap();

        assert_eq!(
            resolve_compaction_system_prompt(
                manifest_dir.path(),
                Some(&compaction_config(None, None))
            )
            .unwrap(),
            None
        );
        assert_eq!(
            resolve_compaction_system_prompt(manifest_dir.path(), None).unwrap(),
            None
        );
    }

    /// A missing file fails with the compaction-specific variant — distinguishable by
    /// variant, not just message text, from the primary prompt's `SystemPromptFileRead` —
    /// and names the resolved path.
    #[test]
    fn compaction_system_prompt_file_missing_reports_compaction_variant() {
        let manifest_dir = TempDir::new().unwrap();

        let err = resolve_compaction_system_prompt(
            manifest_dir.path(),
            Some(&compaction_config(None, Some("nope.md"))),
        )
        .expect_err("missing file must fail");

        match &err {
            RuntimeError::CompactionSystemPromptFileRead { path, .. } => {
                assert!(
                    path.ends_with("nope.md"),
                    "error should name the resolved path, got {path}"
                );
                assert!(
                    path.starts_with(&manifest_dir.path().display().to_string()),
                    "path should be manifest-dir relative, got {path}"
                );
            }
            other => panic!("expected CompactionSystemPromptFileRead, got {other:?}"),
        }
        assert!(err
            .to_string()
            .contains("inference.compaction.system_prompt_file"));
    }

    #[test]
    fn network_without_bash_does_not_warn() {
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["cargo".to_string(), "git".to_string()],
            network_allow: vec!["https://api.example.com".to_string()],
            ..Default::default()
        };
        warn_if_bash_network_bypass(tmp.path(), &policy);
        assert!(bootstrap_log_contents(tmp.path()).is_empty());
    }

    #[test]
    fn bash_without_network_does_not_warn() {
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            network_allow: Vec::new(),
            ..Default::default()
        };
        warn_if_bash_network_bypass(tmp.path(), &policy);
        assert!(bootstrap_log_contents(tmp.path()).is_empty());
    }

    #[test]
    fn neither_declared_does_not_warn() {
        let tmp = TempDir::new().unwrap();
        warn_if_bash_network_bypass(tmp.path(), &CapabilityPolicy::default());
        assert!(bootstrap_log_contents(tmp.path()).is_empty());
    }

    #[test]
    fn non_bash_shell_interpreter_does_not_warn() {
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["sh".to_string()],
            network_allow: vec!["https://api.example.com".to_string()],
            ..Default::default()
        };
        warn_if_bash_network_bypass(tmp.path(), &policy);
        assert!(
            bootstrap_log_contents(tmp.path()).is_empty(),
            "exact match on \"bash\" literal must not fire for other shell interpreters like sh"
        );
    }

    #[test]
    fn allowlist_blocks_unlisted_before_dispatch() {
        let allowlist = HashSet::from(["echo-tool".to_string()]);
        let mut dispatched = false;

        let result = enforce_allowlist(&allowlist, "missing-tool", || {
            dispatched = true;
            Ok::<_, String>(())
        });

        assert!(!dispatched);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("not declared in manifest allowlist"));
    }

    #[test]
    fn allowlist_dispatches_listed_tool() {
        let allowlist = HashSet::from(["echo-tool".to_string()]);
        let result = enforce_allowlist(&allowlist, "echo-tool", || Ok::<_, String>("ok")).unwrap();
        assert_eq!(result, "ok");
    }

    /// Builds a component that exports a single empty instance under `iface`.
    /// `resolve_versioned_iface` only looks up the export index by instance
    /// name — it never inspects the instance's contents — so an empty instance
    /// is sufficient to exercise its versioned-name probe.
    fn iface_double(engine: &wasmtime::Engine, iface: &str) -> wasmtime::component::Component {
        let wat = format!(
            "(component\n\
             (instance $i)\n\
             (export \"{iface}\" (instance $i))\n\
             )"
        );
        let bytes = wat::parse_str(&wat).expect("component WAT parses");
        wasmtime::component::Component::new(engine, bytes).expect("component compiles")
    }

    fn iface_test_engine() -> wasmtime::Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        wasmtime::Engine::new(&config).expect("engine builds")
    }

    fn instantiate_iface_double(
        engine: &wasmtime::Engine,
        iface: &str,
    ) -> (wasmtime::component::Instance, Store<()>) {
        let component = iface_double(engine, iface);
        let linker = wasmtime::component::Linker::new(engine);
        let mut store = Store::new(engine, ());
        let instance = linker
            .instantiate(&mut store, &component)
            .expect("component with no imports instantiates");
        (instance, store)
    }

    /// A component exporting the versioned instance name (the shape a guest
    /// built against the semver'd WIT carries) resolves via the versioned probe.
    #[test]
    fn resolve_versioned_iface_finds_versioned_name() {
        let engine = iface_test_engine();
        let (instance, mut store) = instantiate_iface_double(&engine, WIT_CAPSULE_IFACE_VERSIONED);
        let found = resolve_versioned_iface(&instance, &mut store, WIT_CAPSULE_IFACE_VERSIONED);
        assert!(
            found.is_some(),
            "a component exporting the versioned name must resolve"
        );
    }

    /// A component exporting only the legacy unversioned instance name no longer
    /// resolves — the fallback probe was removed, so the versioned-only lookup
    /// returns `None` and the call site surfaces a hard missing-export error.
    #[test]
    fn resolve_versioned_iface_rejects_unversioned_only_name() {
        let engine = iface_test_engine();
        let (instance, mut store) = instantiate_iface_double(&engine, "murmur:tool/run");
        let found = resolve_versioned_iface(&instance, &mut store, WIT_TOOL_IFACE_VERSIONED);
        assert!(
            found.is_none(),
            "a component exporting only the legacy unversioned name must no longer resolve"
        );
    }

    /// A component exporting neither the versioned name nor any recognizable
    /// name resolves to `None` — the probe must not silently swallow a genuinely
    /// absent interface.
    #[test]
    fn resolve_versioned_iface_returns_none_when_neither_name_matches() {
        let engine = iface_test_engine();
        let (instance, mut store) = instantiate_iface_double(&engine, "murmur:capsule/nonexistent");
        let found = resolve_versioned_iface(&instance, &mut store, WIT_CAPSULE_IFACE_VERSIONED);
        assert!(
            found.is_none(),
            "a component exporting neither probed name must not resolve"
        );
    }

    #[test]
    fn generate_session_id_is_unique() {
        let first = generate_session_id();
        let second = generate_session_id();

        assert_ne!(first, second);
        assert!(first.starts_with("ses_"), "session id must start with ses_");
        assert_eq!(
            first.len(),
            36,
            "session id must be 36 chars (ses_ + 32 hex)"
        );
        uuid::Uuid::parse_str(&first[4..]).expect("session id suffix should be a valid UUID");
    }

    #[test]
    fn sha_verification_happens_before_component_compile() {
        struct FakeRegistry;

        impl Registry for FakeRegistry {
            fn resolve(
                &self,
                name: &str,
                version: &str,
            ) -> Result<ResolvedArtifact, RegistryError> {
                Ok(ResolvedArtifact {
                    meta: ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Wasm,
                        artifact_runtime: "wasm".to_string(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    bytes: b"not-a-real-zip".to_vec().into(),
                    sha256: "definitely-wrong".to_string(),
                })
            }

            fn publish(
                &self,
                _meta: ArtifactMeta,
                _bytes: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }

            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let request = StageRequest {
            manifest_dir: tempdir.path().to_path_buf(),
            capsule_name: "test".to_string(),
            capsule_version: String::new(),
            capsule_component_bytes: b"not-a-component".to_vec(),
            artifacts: vec![crate::types::ArtifactRequest {
                name: "echo-tool".to_string(),
                version: "0.0.1".to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: None,
            capability_policy: CapabilityPolicy::default(),
            inference: None,
            system_prompt_overridden: false,
            context: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: None,
        };

        let err = match stage_session(Arc::new(FakeRegistry), request) {
            Ok(_) => panic!("expected stage_session to fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeError::ArtifactIntegrityFailed { name, version }
                if name == "echo-tool" && version == "0.0.1"
        ));
    }

    #[test]
    fn lock_expectation_mismatch_returns_integrity_failure() {
        struct FakeRegistry;

        impl Registry for FakeRegistry {
            fn resolve(
                &self,
                name: &str,
                version: &str,
            ) -> Result<ResolvedArtifact, RegistryError> {
                Ok(ResolvedArtifact {
                    meta: ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Wasm,
                        artifact_runtime: "wasm".to_string(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    bytes: b"not-a-real-zip".to_vec().into(),
                    sha256: murmur_artifact::sha256_hex(b"not-a-real-zip"),
                })
            }

            fn publish(
                &self,
                _meta: ArtifactMeta,
                _bytes: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }

            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let request = StageRequest {
            manifest_dir: tempdir.path().to_path_buf(),
            capsule_name: "test".to_string(),
            capsule_version: String::new(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![crate::types::ArtifactRequest {
                name: "echo-tool".to_string(),
                version: "0.0.1".to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "echo-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "different".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
            system_prompt_overridden: false,
            context: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: None,
        };

        let err = match stage_session(Arc::new(FakeRegistry), request) {
            Ok(_) => panic!("expected stage_session to fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeError::ArtifactIntegrityFailed { name, version }
                if name == "echo-tool" && version == "0.0.1"
        ));
    }

    #[test]
    fn lock_expectations_require_entry_for_each_manifest_artifact() {
        struct FakeRegistry;

        impl Registry for FakeRegistry {
            fn resolve(
                &self,
                _name: &str,
                _version: &str,
            ) -> Result<ResolvedArtifact, RegistryError> {
                unreachable!("lock validation should fail before any registry resolve call")
            }

            fn publish(
                &self,
                _meta: ArtifactMeta,
                _bytes: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }

            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let request = StageRequest {
            manifest_dir: tempdir.path().to_path_buf(),
            capsule_name: "test".to_string(),
            capsule_version: String::new(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![crate::types::ArtifactRequest {
                name: "echo-tool".to_string(),
                version: "0.0.1".to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "different-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "abc".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
            system_prompt_overridden: false,
            context: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: None,
        };

        let err = match stage_session(Arc::new(FakeRegistry), request) {
            Ok(_) => panic!("expected stage_session to fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeError::LockMissingEntry { name } if name == "echo-tool"
        ));
    }

    #[test]
    fn lock_expectation_version_mismatch_is_reported() {
        struct FakeRegistry;

        impl Registry for FakeRegistry {
            fn resolve(
                &self,
                _name: &str,
                _version: &str,
            ) -> Result<ResolvedArtifact, RegistryError> {
                unreachable!("version mismatch should fail before registry resolve")
            }

            fn publish(
                &self,
                _meta: ArtifactMeta,
                _bytes: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }

            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let tempdir = tempfile::tempdir().unwrap();
        let request = StageRequest {
            manifest_dir: tempdir.path().to_path_buf(),
            capsule_name: "test".to_string(),
            capsule_version: String::new(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![crate::types::ArtifactRequest {
                name: "echo-tool".to_string(),
                version: "0.0.9".to_string(),
                runtime: ArtifactRuntime::Tool,
                source: None,
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "echo-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "abc".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
            system_prompt_overridden: false,
            context: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: None,
        };

        let err = match stage_session(Arc::new(FakeRegistry), request) {
            Ok(_) => panic!("expected stage_session to fail"),
            Err(err) => err,
        };
        assert!(matches!(
            err,
            RuntimeError::LockVersionMismatch {
                name,
                requested,
                pinned,
            } if name == "echo-tool" && requested == "0.0.9" && pinned == "0.0.1"
        ));
    }

    // ── local-source skill resolution ──────────────────────────────────────

    #[test]
    fn load_local_skill_md_reads_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("skill.md");
        fs::write(&skill, b"# hi from file").unwrap();
        let bytes = load_local_skill_md(dir.path(), "skill.md").unwrap();
        assert_eq!(bytes, b"# hi from file");
    }

    #[test]
    fn load_local_skill_md_finds_skill_md_in_directory_case_insensitive() {
        let manifest_dir = tempfile::tempdir().unwrap();
        let skill_dir = manifest_dir.path().join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        // Uppercase filename — must still be found.
        fs::write(skill_dir.join("SKILL.MD"), b"# upper").unwrap();
        let bytes = load_local_skill_md(manifest_dir.path(), "skills/my-skill").unwrap();
        assert_eq!(bytes, b"# upper");
    }

    #[test]
    fn load_local_skill_md_missing_path_errors_with_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_local_skill_md(dir.path(), "does/not/exist").unwrap_err();
        match err {
            RuntimeError::SkillSourceNotFound { path } => {
                assert!(path.contains("does/not/exist"), "path was: {path}");
            }
            other => panic!("expected SkillSourceNotFound, got {other:?}"),
        }
    }

    #[test]
    fn load_local_skill_md_directory_without_skill_md_errors() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let err = load_local_skill_md(dir.path(), "empty").unwrap_err();
        match err {
            RuntimeError::SkillSourceMissingSkillMd { path } => {
                assert!(path.contains("empty"), "path was: {path}");
            }
            other => panic!("expected SkillSourceMissingSkillMd, got {other:?}"),
        }
    }

    #[test]
    fn load_local_skill_md_absolute_path_ignores_manifest_dir() {
        let manifest_dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let skill = elsewhere.path().join("skill.md");
        fs::write(&skill, b"# absolute").unwrap();
        let bytes = load_local_skill_md(manifest_dir.path(), &skill.to_string_lossy()).unwrap();
        assert_eq!(bytes, b"# absolute");
    }

    #[test]
    fn stage_session_installs_local_source_skill_without_registry() {
        use murmur_artifact::{InferenceConfig, InferenceDriver};

        // Registry must never be consulted for a local-source skill.
        struct PanicRegistry;
        impl Registry for PanicRegistry {
            fn resolve(&self, _: &str, _: &str) -> Result<ResolvedArtifact, RegistryError> {
                panic!("registry must not be called for a local-source skill");
            }
            fn publish(
                &self,
                _: ArtifactMeta,
                _: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }
            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let project = tempfile::tempdir().unwrap();
        let skill_dir = project.path().join("skills").join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("skill.md"), b"# my local skill").unwrap();

        let inference = InferenceConfig {
            transport: "http".into(),
            endpoint: Some("http://localhost".into()),
            model: "claude-3-haiku".into(),
            api_key: None,
            driver: Some(InferenceDriver {
                artifact: "test-driver".into(),
                config: None,
            }),
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
        };

        let request = StageRequest {
            manifest_dir: project.path().to_path_buf(),
            capsule_name: "test".to_string(),
            capsule_version: "0.0.1".to_string(),
            capsule_component_bytes: Vec::new(),
            artifacts: vec![crate::types::ArtifactRequest {
                name: "my-skill".to_string(),
                version: "local".to_string(),
                runtime: ArtifactRuntime::Skill,
                source: Some("skills/my-skill".to_string()),
                on_overflow: Default::default(),
                capabilities: None,
            }],
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: CapabilityPolicy::default(),
            inference: Some(inference),
            system_prompt_overridden: false,
            context: None,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: murmur_artifact::ContainmentClass::Advisory,
            exports: None,
        };

        let staged = stage_session(Arc::new(PanicRegistry), request).unwrap();
        let installed = staged
            .workdir
            .join("tools")
            .join("my-skill")
            .join("skill.md");
        assert!(
            installed.exists(),
            "skill.md not installed at {}",
            installed.display()
        );
        assert_eq!(fs::read(&installed).unwrap(), b"# my local skill");
        // No lock artifact recorded for a local-source skill.
        assert!(staged.resolved_lock_artifacts.is_empty());
        assert_eq!(staged.installed_artifacts.len(), 1);
        assert_eq!(staged.installed_artifacts[0].version, "local");
        // MURMUR.md (written during staging) lists the skill as callable.
        let murmur_md = fs::read_to_string(staged.workdir.join("MURMUR.md")).unwrap();
        assert!(
            murmur_md.contains("**my-skill**"),
            "MURMUR.md missing skill listing:\n{murmur_md}"
        );
        assert!(
            murmur_md.contains("call by name to load guidance"),
            "MURMUR.md missing callable skill hint:\n{murmur_md}"
        );
    }

    // ── manage.pull() ──────────────────────────────────────────────────────────

    fn zip_with_files(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            for (name, bytes) in files {
                zip.start_file(*name, options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn build_test_state(
        registry: Arc<dyn Registry>,
        workdir: PathBuf,
        lock_path: PathBuf,
    ) -> CapsuleStoreState {
        let engine = build_engine().unwrap();
        let wasi = build_wasi_ctx(&workdir, None, None, &[], &CapabilityPolicy::default()).unwrap();
        CapsuleStoreState {
            limits: crate::limits::ExecutionLimits::default().limiter(),
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            http_hooks: NetworkPolicyHooks {
                network_allow_rules: Vec::new(),
            },
            network_allow_rules: Vec::new(),
            peer_fetch_rules: Vec::new(),
            peer_plane: None,
            peer_own_audience: String::new(),
            peer_trace: None,
            inference_env: Vec::new(),
            engine,
            workdir: workdir.clone(),
            accessible_workdir: workdir,
            tool_components: HashMap::new(),
            artifact_grants: HashMap::new(),
            allowlisted_tools: HashSet::new(),
            installed_artifacts: Vec::new(),
            session_id: "ses_test".to_string(),
            pending_a2a_events: Vec::new(),
            capability_policy: CapabilityPolicy::default(),
            shell_enforcement: sandbox::ShellEnforcement::environment_only(),
            current_traceparent: None,
            a2a_task_registry: None,
            a2a_sse: None,
            a2a_task_id: None,
            input_timeout_secs: None,
            a2a_chunk_event_id: Arc::new(AtomicU64::new(0)),
            a2a_chunks_emitted: Arc::new(AtomicBool::new(false)),
            registry,
            lock_path,
            driver_continuation_id: None,
            driver_continuation_context_id: None,
            driver_continuation_acked_len: 0,
        }
    }

    // ── Driver continuation bookkeeping on CapsuleStoreState ─────────────

    fn continuation_test_state() -> CapsuleStoreState {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let lock_path = workdir.join("murmur.lock");
        // Registry/workdir are irrelevant to continuation bookkeeping; reuse the helper.
        build_test_state(
            Arc::new(FakeSkillRegistry::new(Vec::new())),
            workdir,
            lock_path,
        )
    }

    #[test]
    fn continuation_default_is_none_and_active_query_returns_none() {
        let state = continuation_test_state();
        assert!(state.driver_continuation_id.is_none());
        assert!(state.active_continuation(Some("ctx-a")).is_none());
        assert!(state.active_continuation(None).is_none());
    }

    #[test]
    fn continuation_record_then_active_requires_matching_context() {
        // Scenario 6: a held continuation is only reused under the same context_id.
        let mut state = continuation_test_state();
        state.record_continuation("cont-1".into(), Some("ctx-a".into()), 2);

        assert_eq!(
            state.active_continuation(Some("ctx-a")),
            Some(("cont-1", 2)),
            "same context → continuation is active"
        );
        assert!(
            state.active_continuation(Some("ctx-b")).is_none(),
            "different context → continuation must not be reused"
        );
        assert!(
            state.active_continuation(None).is_none(),
            "absent context → continuation must not be reused"
        );
    }

    #[test]
    fn continuation_clear_drops_all_bookkeeping() {
        // Scenarios 3 & 5: driver silence / replace-context commit drops the held id.
        let mut state = continuation_test_state();
        state.record_continuation("cont-1".into(), Some("ctx-a".into()), 5);
        state.clear_continuation();
        assert!(state.driver_continuation_id.is_none());
        assert_eq!(state.driver_continuation_acked_len, 0);
        assert!(state.active_continuation(Some("ctx-a")).is_none());
    }

    #[test]
    fn advance_acked_len_only_affects_same_context_held_continuation() {
        // Scenario 7: end_turn persists an assistant message the driver already knows; the
        // acked length advances so the next same-context Task wires only its new user message.
        let mut state = continuation_test_state();
        state.record_continuation("cont-1".into(), Some("ctx-a".into()), 1);

        state.advance_continuation_acked_len(Some("ctx-b"), 9);
        assert_eq!(
            state.driver_continuation_acked_len, 1,
            "advancing under a different context must be a no-op"
        );

        state.advance_continuation_acked_len(Some("ctx-a"), 2);
        assert_eq!(
            state.active_continuation(Some("ctx-a")),
            Some(("cont-1", 2))
        );

        state.clear_continuation();
        state.advance_continuation_acked_len(Some("ctx-a"), 7);
        assert_eq!(
            state.driver_continuation_acked_len, 0,
            "advancing with no held continuation must be a no-op"
        );
    }

    struct FakeSkillRegistry {
        bytes: Vec<u8>,
        sha256: String,
    }

    impl FakeSkillRegistry {
        fn new(bytes: Vec<u8>) -> Self {
            let sha256 = murmur_artifact::sha256_hex(&bytes);
            Self { bytes, sha256 }
        }
    }

    impl Registry for FakeSkillRegistry {
        fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError> {
            Ok(ResolvedArtifact {
                meta: ArtifactMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    runtime: RuntimeType::Static,
                    artifact_runtime: "skill".to_string(),
                    platforms: Vec::new(),
                    description: None,
                    tags: Vec::new(),
                },
                bytes: self.bytes.clone().into(),
                sha256: self.sha256.clone(),
            })
        }

        fn publish(
            &self,
            _meta: ArtifactMeta,
            _bytes: &[u8],
        ) -> Result<murmur_artifact::PublishResult, RegistryError> {
            unreachable!()
        }

        fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
            unreachable!()
        }
    }

    #[test]
    fn pull_happy_path_installs_artifact_and_updates_lock() {
        let artifact_bytes = zip_with_files(&[
            (
                PACKED_MANIFEST_ENTRY,
                b"name: my-skill\nversion: 1.0.0\nruntime: skill\n",
            ),
            ("skill.md", b"# guidance"),
        ]);
        let registry = Arc::new(FakeSkillRegistry::new(artifact_bytes));
        let expected_sha256 = registry.sha256.clone();

        let project = tempfile::tempdir().unwrap();
        let workdir = project.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let lock_path = project.path().join("murmur.lock");

        let mut state = build_test_state(registry, workdir.clone(), lock_path.clone());

        let summary = manage::Host::pull(&mut state, "my-skill".to_string(), "1.0.0".to_string())
            .expect("pull should succeed");
        assert_eq!(summary.name, "my-skill");
        assert_eq!(summary.version, "1.0.0");

        let installed_skill_md = workdir.join("tools").join("my-skill").join("skill.md");
        assert!(installed_skill_md.exists());
        assert_eq!(fs::read(&installed_skill_md).unwrap(), b"# guidance");

        assert!(state
            .installed_artifacts
            .iter()
            .any(|a| a.name == "my-skill" && a.version == "1.0.0"));

        let lock = read_lockfile(&lock_path).expect("murmur.lock should have been written");
        let entry = lock
            .artifact_for("my-skill")
            .expect("lock entry for my-skill");
        assert_eq!(entry.resolved_version, "1.0.0");
        assert_eq!(entry.sha256.wasm, expected_sha256);
    }

    #[test]
    fn pull_rejects_tampered_bytes_and_writes_nothing() {
        struct TamperedRegistry;
        impl Registry for TamperedRegistry {
            fn resolve(
                &self,
                name: &str,
                version: &str,
            ) -> Result<ResolvedArtifact, RegistryError> {
                Ok(ResolvedArtifact {
                    meta: ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Static,
                        artifact_runtime: "skill".to_string(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    bytes: b"tampered-bytes".to_vec().into(),
                    sha256: "not-the-real-hash".to_string(),
                })
            }

            fn publish(
                &self,
                _meta: ArtifactMeta,
                _bytes: &[u8],
            ) -> Result<murmur_artifact::PublishResult, RegistryError> {
                unreachable!()
            }

            fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
                unreachable!()
            }
        }

        let project = tempfile::tempdir().unwrap();
        let workdir = project.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let lock_path = project.path().join("murmur.lock");

        let mut state = build_test_state(
            Arc::new(TamperedRegistry),
            workdir.clone(),
            lock_path.clone(),
        );

        let err = manage::Host::pull(&mut state, "evil-tool".to_string(), "1.0.0".to_string())
            .expect_err("tampered bytes must be rejected");
        assert!(err.contains("integrity"), "unexpected error message: {err}");

        assert!(!workdir.join("tools").join("evil-tool").exists());
        assert!(!lock_path.exists());
        assert!(state.installed_artifacts.is_empty());
    }

    #[test]
    fn pull_rejects_lock_conflict_and_writes_nothing() {
        let artifact_bytes = zip_with_files(&[
            (
                PACKED_MANIFEST_ENTRY,
                b"name: my-skill\nversion: 2.0.0\nruntime: skill\n",
            ),
            ("skill.md", b"# guidance v2"),
        ]);
        let registry = Arc::new(FakeSkillRegistry::new(artifact_bytes));

        let project = tempfile::tempdir().unwrap();
        let workdir = project.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let lock_path = project.path().join("murmur.lock");

        // Pin a different version/hash for this artifact ahead of time.
        write_lockfile_atomic(
            &lock_path,
            &MurmurLock {
                lock_version: LOCK_VERSION,
                artifacts: vec![LockedArtifact {
                    name: "my-skill".to_string(),
                    resolved_version: "1.0.0".to_string(),
                    sha256: LockedSha256 {
                        wasm: "pinned-hash-from-earlier-pull".to_string(),
                    },
                }],
            },
        )
        .unwrap();

        let mut state = build_test_state(registry, workdir.clone(), lock_path.clone());

        let err = manage::Host::pull(&mut state, "my-skill".to_string(), "2.0.0".to_string())
            .expect_err("lock conflict must be rejected");
        assert!(
            err.contains("murmur.lock conflict"),
            "unexpected error message: {err}"
        );

        assert!(!workdir
            .join("tools")
            .join("my-skill")
            .join("skill.md")
            .exists());
        assert!(state.installed_artifacts.is_empty());

        // Lock must be left exactly as it was.
        let lock = read_lockfile(&lock_path).unwrap();
        let entry = lock.artifact_for("my-skill").unwrap();
        assert_eq!(entry.resolved_version, "1.0.0");
        assert_eq!(entry.sha256.wasm, "pinned-hash-from-earlier-pull");
    }

    /// Writes an executable shell script native-tool fixture that echoes the given env
    /// var names (space-separated `NAME=value`, empty string for an unset var) into the
    /// `data` field of a passing ToolResult JSON payload on stdout.
    fn write_env_echo_native_tool(dir: &Path, name: &str, env_vars: &[&str]) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script_path = dir.join(name);
        let echoes: String = env_vars
            .iter()
            .map(|var| format!(r#"echo -n "{var}=${var} ""#))
            .collect::<Vec<_>>()
            .join("\n");
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '{{\"status\":\"passed\",\"data\":\"'\n{echoes}\nprintf '\"}}'\n"
        );
        fs::write(&script_path, script).unwrap();
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        script_path
    }

    /// The wiring the shell-event fix depends on: `dispatch_shell_tool` carries the resolved
    /// binary out of `execute_shell` and into `ShellDispatchInfo`, which is the only channel
    /// by which `agent.rs` can put it on the hook event and the trace record. `command` keeps
    /// its pre-existing meaning — the argument list alone, never the binary name.
    #[test]
    fn dispatch_shell_tool_reports_the_invoked_binary_separately_from_the_command() {
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let outcome = dispatch_shell_tool(
            "bash",
            murmur::tool::run::ToolInput {
                data: Some(r#"{"command":"echo hi"}"#.to_string()),
                log_path: None,
            },
            tmp.path(),
            &[],
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        );

        let shell = outcome
            .shell
            .expect("a successful shell call reports itself");
        assert!(
            Path::new(&shell.binary).is_absolute() && shell.binary.ends_with("bash"),
            "binary must be the resolved path of what ran, got {:?}",
            shell.binary
        );
        assert_eq!(
            shell.command, "echo hi",
            "command must still carry only the argument list"
        );
        assert_eq!(shell.exit_code, 0);
    }

    /// The dispatch-layer half of the composed-root failure path: a sealed session whose root
    /// could not be built does not come back as an ordinary failed tool call the capsule gets
    /// another turn to react to. The tool result is still filled in (so the trace records the
    /// attempt), but `fatal` carries the typed `RuntimeError` the agent turn loop returns and
    /// the CLI renders as `E-RUN-014`.
    ///
    /// Linux-only because the forced-failure seam lives in the Linux `pre_exec` path; the
    /// mechanism it stands in for is Linux-only too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_sealed_composed_root_failure_ends_the_session_not_just_the_tool_call() {
        if crate::network_namespace::skip_without_egress_namespace(
            "a_sealed_composed_root_failure_ends_the_session_not_just_the_tool_call",
        ) {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let _guard = sandbox::ForceSealedRootFailureGuard::new();
        let outcome = dispatch_shell_tool(
            "bash",
            murmur::tool::run::ToolInput {
                data: Some(r#"{"command":"echo hi"}"#.to_string()),
                log_path: None,
            },
            tmp.path(),
            &[],
            &policy,
            &sandbox::sealed_test_enforcement(),
        );

        assert!(
            matches!(outcome.result.status, murmur::tool::run::Status::Error),
            "the failed call must still read as a failed call"
        );
        let data = outcome.result.data.as_deref().unwrap_or_default();
        assert!(
            data.contains("composed root"),
            "the tool result must still say what happened: {data}"
        );
        let fatal = outcome
            .fatal
            .expect("a composed-root failure must be carried out as session-fatal");
        assert!(
            matches!(fatal, RuntimeError::SealedRootConstructionFailed { .. }),
            "must be the variant murmur-cli maps to E-RUN-014: {fatal}"
        );
    }

    /// The contrast: an ordinary shell failure leaves `fatal` unset, so the capsule keeps its
    /// turn. Without this, "always fatal" would pass the test above.
    #[test]
    fn an_ordinary_shell_failure_leaves_the_session_running() {
        let tmp = TempDir::new().unwrap();
        let outcome = dispatch_shell_tool(
            "bash",
            murmur::tool::run::ToolInput {
                data: Some(r#"{"command":"echo hi"}"#.to_string()),
                log_path: None,
            },
            tmp.path(),
            &[],
            // Empty allowlist: `execute_shell` refuses before spawning anything.
            &CapabilityPolicy::default(),
            &sandbox::ShellEnforcement::environment_only(),
        );

        assert!(matches!(
            outcome.result.status,
            murmur::tool::run::Status::Error
        ));
        assert!(
            outcome.fatal.is_none(),
            "a disallowed binary is the capsule's problem, not the session's"
        );
    }

    /// The composed path, on real inputs and a real file: dispatch a real shell tool, then
    /// hand its `ShellDispatchInfo` to a real `TraceWriter` exactly as `agent.rs` does, and
    /// read the resulting `workdir/trace.jsonl` back. This is the automated stand-in for
    /// eyeballing a session's trace after `mur run` — the two lines it mirrors in `agent.rs`
    /// are a straight field pass-through, but the JSONL key and its value are pinned here.
    #[test]
    fn shell_dispatch_writes_the_resolved_binary_into_trace_jsonl() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().to_path_buf();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let events: Vec<serde_json::Value> = rt.block_on(async {
            let outcome = dispatch_shell_tool(
                "bash",
                murmur::tool::run::ToolInput {
                    data: Some(r#"{"command":"echo hi"}"#.to_string()),
                    log_path: None,
                },
                &workdir,
                &[],
                &policy,
                &sandbox::ShellEnforcement::environment_only(),
            );
            let shell = outcome.shell.expect("the shell call reports itself");

            let mut trace = TraceWriter::open(
                &workdir,
                "ses_test".to_string(),
                "cap".to_string(),
                "0.1.0".to_string(),
                "test-model".to_string(),
                Vec::new(),
                crate::containment::scope_report_for_tier(
                    &policy,
                    murmur_artifact::ContainmentClass::Advisory,
                    sandbox::EnforcementTier::EnvironmentOnly,
                    None,
                    None,
                    None,
                    Vec::new(),
                ),
                false,
                None,
                false,
            )
            .await
            .unwrap();
            trace
                .write_shell(
                    1,
                    shell.binary.clone(),
                    shell.command.clone(),
                    shell.exit_code,
                    shell.stdout_bytes,
                    shell.stderr_bytes,
                    shell.duration_ms,
                    shell.resource_limit.clone(),
                )
                .await
                .unwrap();
            trace.flush().await.unwrap();

            fs::read_to_string(workdir.join("trace.jsonl"))
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        });

        let shell_event = events
            .iter()
            .find(|e| e["event_type"] == "shell")
            .expect("trace.jsonl must carry a shell event");
        let binary = shell_event["binary"]
            .as_str()
            .expect("the shell event must carry a `binary` string");
        assert!(
            Path::new(binary).is_absolute() && binary.ends_with("bash"),
            "trace.jsonl's binary must be the resolved absolute path of what ran, got {binary:?}"
        );
        assert_eq!(shell_event["command"], "echo hi");
    }

    #[test]
    fn dispatch_native_tool_gets_synthetic_home_matching_execute_shell() {
        let tmp = TempDir::new().unwrap();
        let binary = write_env_echo_native_tool(tmp.path(), "echo-home", &["HOME"]);
        let policy = CapabilityPolicy::default();

        let result = dispatch_native_tool(
            "echo-home",
            murmur::tool::run::ToolInput {
                data: None,
                log_path: None,
            },
            &binary,
            tmp.path(),
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        )
        .unwrap();

        let expected_home = tmp.path().join(".capsule-home");
        let data = result.data.unwrap_or_default();
        assert!(
            data.contains(&format!("HOME={}", expected_home.display())),
            "expected synthetic HOME in native tool output, got: {data}"
        );
        assert!(expected_home.is_dir());
    }

    #[test]
    fn dispatch_native_tool_strips_credential_shaped_var() {
        let tmp = TempDir::new().unwrap();
        let binary = write_env_echo_native_tool(tmp.path(), "echo-token", &["GITHUB_TOKEN"]);
        let policy = CapabilityPolicy::default();

        std::env::set_var("GITHUB_TOKEN", "leaked-token");
        let result = dispatch_native_tool(
            "echo-token",
            murmur::tool::run::ToolInput {
                data: None,
                log_path: None,
            },
            &binary,
            tmp.path(),
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        )
        .unwrap();
        std::env::remove_var("GITHUB_TOKEN");

        let data = result.data.unwrap_or_default();
        assert!(
            !data.contains("leaked-token"),
            "GITHUB_TOKEN must not reach a native tool subprocess, got: {data}"
        );
    }

    #[test]
    fn dispatch_native_tool_strips_wildcard_credential_pattern() {
        let tmp = TempDir::new().unwrap();
        let binary = write_env_echo_native_tool(tmp.path(), "echo-stripe", &["STRIPE_API_KEY"]);
        let policy = CapabilityPolicy::default();

        std::env::set_var("STRIPE_API_KEY", "leaked-key");
        let result = dispatch_native_tool(
            "echo-stripe",
            murmur::tool::run::ToolInput {
                data: None,
                log_path: None,
            },
            &binary,
            tmp.path(),
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        )
        .unwrap();
        std::env::remove_var("STRIPE_API_KEY");

        let data = result.data.unwrap_or_default();
        assert!(
            !data.contains("leaked-key"),
            "*_API_KEY wildcard pattern must strip STRIPE_API_KEY from native tool subprocess, got: {data}"
        );
    }

    #[test]
    fn dispatch_native_tool_keeps_safe_baseline_var() {
        let tmp = TempDir::new().unwrap();
        let binary = write_env_echo_native_tool(tmp.path(), "echo-cargo-home", &["CARGO_HOME"]);
        let policy = CapabilityPolicy::default();

        std::env::set_var("CARGO_HOME", "/fake/cargo/home");
        let result = dispatch_native_tool(
            "echo-cargo-home",
            murmur::tool::run::ToolInput {
                data: None,
                log_path: None,
            },
            &binary,
            tmp.path(),
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        )
        .unwrap();
        std::env::remove_var("CARGO_HOME");

        let data = result.data.unwrap_or_default();
        assert!(
            data.contains("CARGO_HOME=/fake/cargo/home"),
            "safe baseline var CARGO_HOME must pass through to native tool subprocess, got: {data}"
        );
    }

    #[test]
    fn dispatch_native_tool_composes_policy_strip_env() {
        let tmp = TempDir::new().unwrap();
        let binary =
            write_env_echo_native_tool(tmp.path(), "echo-mycompany", &["MYCOMPANY_SECRET"]);
        let policy = CapabilityPolicy {
            shell_strip_env: vec!["MYCOMPANY_*".to_string()],
            ..CapabilityPolicy::default()
        };

        std::env::set_var("MYCOMPANY_SECRET", "leaked-secret");
        let result = dispatch_native_tool(
            "echo-mycompany",
            murmur::tool::run::ToolInput {
                data: None,
                log_path: None,
            },
            &binary,
            tmp.path(),
            &policy,
            &sandbox::ShellEnforcement::environment_only(),
        )
        .unwrap();
        std::env::remove_var("MYCOMPANY_SECRET");

        let data = result.data.unwrap_or_default();
        assert!(
            !data.contains("leaked-secret"),
            "policy.shell_strip_env pattern must compose for native tool subprocess, got: {data}"
        );
    }

    // ── Per-artifact grant narrowing for tools and drivers ───────────────────────────

    /// The capsule ceiling the narrowing tests clamp against. Loopback ports nothing listens
    /// on, so a policy decision is observable without any connection ever completing.
    fn narrowing_ceiling() -> Vec<NetworkAllowRule> {
        parse_network_allow_rules(&[
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:2".to_string(),
        ])
        .unwrap()
    }

    fn grant_of(network: Option<Vec<&str>>, scope: Option<&str>) -> ToolCapabilityGrant {
        let caps = murmur_artifact::Capabilities {
            peer_fetch: None,
            network: network.map(|allow| murmur_artifact::NetworkCapabilities {
                allow: allow.into_iter().map(str::to_string).collect(),
                unix_sockets: false,
            }),
            filesystem: scope.map(|scope| murmur_artifact::FilesystemCapabilities {
                scope: Some(scope.to_string()),
                workdir_exec: false,
            }),
            shell: None,
            spawn: None,
            env: None,
            limits: None,
            resources: None,
            state: None,
            task_io: None,
            containment: None,
        };
        ToolCapabilityGrant::derive(Some(&caps), &narrowing_ceiling(), "test-capsule")
            .expect("grant is valid")
    }

    /// Push a request through the very `NetworkPolicyHooks` a tool store is built with, so
    /// the assertion is on the real wasi-http gate rather than on the rule list.
    fn send_through_tool_hooks(rules: &[NetworkAllowRule], uri: &str, use_tls: bool) -> bool {
        use http_body_util::{BodyExt, Empty};

        let mut hooks = NetworkPolicyHooks {
            network_allow_rules: rules.to_vec(),
        };
        let body = Empty::<bytes::Bytes>::new()
            .map_err(|err| match err {})
            .boxed_unsync();
        let request = hyper::Request::builder()
            .uri(uri)
            .body(body)
            .expect("request builds");
        let config = wasmtime_wasi_http::p2::types::OutgoingRequestConfig {
            use_tls,
            connect_timeout: std::time::Duration::from_millis(1),
            first_byte_timeout: std::time::Duration::from_millis(1),
            between_bytes_timeout: std::time::Duration::from_millis(1),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { hooks.send_request(request, config).is_ok() })
    }

    /// The no-op invariant, network half: a tool with no per-artifact entry dispatches on the
    /// full ceiling, reaching everything the capsule may reach.
    #[test]
    fn ungranted_tool_keeps_the_whole_ceiling() {
        let ceiling = narrowing_ceiling();
        let rules = effective_tool_network_rules(None, &ceiling);

        assert!(send_through_tool_hooks(
            rules,
            "http://127.0.0.1:1/x",
            false
        ));
        assert!(send_through_tool_hooks(
            rules,
            "http://127.0.0.1:2/x",
            false
        ));
    }

    /// A narrowed tool reaches only its declared host; the ceiling's other host is gone for
    /// that artifact even though a sibling tool still reaches it.
    #[test]
    fn narrowed_tool_reaches_only_its_declared_host() {
        let ceiling = narrowing_ceiling();
        let grant = grant_of(Some(vec!["http://127.0.0.1:1"]), None);
        let narrowed = effective_tool_network_rules(Some(&grant), &ceiling);

        assert!(
            send_through_tool_hooks(narrowed, "http://127.0.0.1:1/x", false),
            "the declared host stays reachable"
        );
        assert!(
            !send_through_tool_hooks(narrowed, "http://127.0.0.1:2/x", false),
            "the rest of the ceiling is dropped for this artifact"
        );
        // The sibling with no entry is unaffected by its neighbour's narrowing.
        let sibling = effective_tool_network_rules(None, &ceiling);
        assert!(send_through_tool_hooks(
            sibling,
            "http://127.0.0.1:2/x",
            false
        ));
    }

    /// An entry outside the ceiling is dropped rather than granted, and reported so staging
    /// can raise `W-SEC-007`.
    #[test]
    fn out_of_ceiling_entry_is_dropped_and_reported() {
        let ceiling = narrowing_ceiling();
        let grant = grant_of(
            Some(vec!["http://127.0.0.1:1", "https://evil.example.com"]),
            None,
        );
        let narrowed = effective_tool_network_rules(Some(&grant), &ceiling);

        assert!(!send_through_tool_hooks(
            narrowed,
            "https://evil.example.com/x",
            true
        ));
        assert_eq!(
            grant.dropped_network_entries,
            vec!["https://evil.example.com".to_string()]
        );
    }

    /// Without a scope the preopened root is the workdir itself — observable because
    /// `preopened_dir` requires the directory to exist, so a missing workdir is an error.
    #[test]
    fn tool_without_filesystem_scope_preopens_the_workdir_itself() {
        let root = TempDir::new().unwrap();
        let missing = root.path().join("does-not-exist");

        assert!(
            build_wasi_ctx(&missing, None, None, &[], &CapabilityPolicy::default()).is_err(),
            "an unscoped tool preopens the workdir, which must exist"
        );
        build_wasi_ctx(root.path(), None, None, &[], &CapabilityPolicy::default())
            .expect("an existing workdir preopens as before");
        assert!(
            !missing.exists(),
            "the unscoped path must not create anything"
        );
    }

    /// With a scope the preopened root is `<workdir>/<scope>`, created if absent. Sibling
    /// paths under the workdir are never mounted, so a guest has no descriptor for them.
    #[test]
    fn tool_with_filesystem_scope_preopens_only_the_scoped_subtree() {
        let root = TempDir::new().unwrap();
        std::fs::write(root.path().join("secret.txt"), b"capsule state").unwrap();

        build_wasi_ctx(
            root.path(),
            Some("cache"),
            None,
            &[],
            &CapabilityPolicy::default(),
        )
        .expect("a granted scope is created and preopened");

        let scoped = root.path().join("cache");
        assert!(scoped.is_dir(), "the granted scope is created if missing");
        std::fs::write(scoped.join("entry.json"), b"{}").unwrap();
        assert!(scoped.join("entry.json").exists());
        assert!(
            root.path().join("secret.txt").exists(),
            "nothing outside the scope was touched"
        );
    }

    /// A scope that cannot be created is a hard error naming the scope, never a silent
    /// widening back to the whole workdir.
    #[test]
    fn unusable_filesystem_scope_is_a_hard_error() {
        let root = TempDir::new().unwrap();
        // A regular file where the scope directory would go: `create_dir_all` cannot proceed.
        std::fs::write(root.path().join("cache"), b"not a directory").unwrap();

        let policy = CapabilityPolicy::default();
        let err = match build_wasi_ctx(root.path(), Some("cache"), None, &[], &policy) {
            Ok(_) => panic!("an uncreatable scope must fail loudly"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("cache"),
            "the error must name the scope, got: {err}"
        );
    }

    /// End-to-end through the one dispatch body every WASM tool and the inference driver
    /// share: a real component is instantiated and called under a grant, and the scoped
    /// directory it was granted appears on the host filesystem.
    #[test]
    fn real_dispatch_applies_the_filesystem_scope() {
        let engine = build_engine().unwrap();
        let workdir = TempDir::new().unwrap();
        let component = crate::inference_import::test_support::driver_double(
            &engine,
            0,
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#,
        );
        let ceiling = narrowing_ceiling();
        let grant = grant_of(None, Some("tool-cache"));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async {
            invoke_tool_component(
                ToolInvokeEnv {
                    engine: &engine,
                    accessible_workdir: workdir.path(),
                    inference_env: &[],
                    capability_policy: &CapabilityPolicy::default(),
                    network_allow_rules: &ceiling,
                    artifact_grant: Some(&grant),
                },
                ToolA2aWiring::silent(),
                "scoped-tool",
                &component,
                murmur::tool::run::ToolInput {
                    data: Some("{}".to_string()),
                    log_path: None,
                },
            )
            .await
        });

        assert!(result.is_ok(), "the scoped tool still runs: {result:?}");
        assert!(
            workdir.path().join("tool-cache").is_dir(),
            "the real dispatch path preopened the granted scope"
        );
    }

    /// The same dispatch with no grant is byte-for-byte the pre-narrowing behavior: the whole
    /// accessible workdir is the preopen root and no subtree is created.
    #[test]
    fn real_dispatch_without_a_grant_is_unchanged() {
        let engine = build_engine().unwrap();
        let workdir = TempDir::new().unwrap();
        let component = crate::inference_import::test_support::driver_double(
            &engine,
            0,
            r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#,
        );
        let ceiling = narrowing_ceiling();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async {
            invoke_tool_component(
                ToolInvokeEnv {
                    engine: &engine,
                    accessible_workdir: workdir.path(),
                    inference_env: &[],
                    capability_policy: &CapabilityPolicy::default(),
                    network_allow_rules: &ceiling,
                    artifact_grant: None,
                },
                ToolA2aWiring::silent(),
                "plain-tool",
                &component,
                murmur::tool::run::ToolInput {
                    data: Some("{}".to_string()),
                    log_path: None,
                },
            )
            .await
        });

        assert!(
            result.is_ok(),
            "an ungranted tool runs as before: {result:?}"
        );
        assert_eq!(
            std::fs::read_dir(workdir.path()).unwrap().count(),
            0,
            "no per-artifact subtree is created when nothing was granted"
        );
    }

    /// Staging lowers a grant only for artifacts that declared one, and only from the
    /// operator's own entry — the map is the single source dispatch consults.
    #[test]
    fn staging_records_grants_only_for_declaring_artifacts() {
        let ceiling = narrowing_ceiling();
        let mut grants = HashMap::new();

        let declared = ArtifactRequest {
            name: "scoped-tool".to_string(),
            version: "1.0.0".to_string(),
            runtime: ArtifactRuntime::Tool,
            source: None,
            on_overflow: Default::default(),
            capabilities: Some(murmur_artifact::Capabilities {
                peer_fetch: None,
                network: Some(murmur_artifact::NetworkCapabilities {
                    allow: vec!["http://127.0.0.1:1".to_string()],
                    unix_sockets: false,
                }),
                filesystem: None,
                shell: None,
                spawn: None,
                env: None,
                limits: None,
                resources: None,
                state: None,
                task_io: None,
                containment: None,
            }),
        };
        let silent = ArtifactRequest {
            name: "plain-tool".to_string(),
            version: "1.0.0".to_string(),
            runtime: ArtifactRuntime::Tool,
            source: None,
            on_overflow: Default::default(),
            capabilities: None,
        };

        stage_artifact_grant(&declared, &ceiling, "test-capsule", &mut grants).unwrap();
        stage_artifact_grant(&silent, &ceiling, "test-capsule", &mut grants).unwrap();

        assert!(grants.contains_key("scoped-tool"));
        assert!(
            !grants.contains_key("plain-tool"),
            "an artifact with no capabilities block must stay absent so dispatch falls back \
             to the ceiling"
        );
    }

    /// A malformed grant fails staging rather than surfacing as a confusing denial once the
    /// tool is already running.
    #[test]
    fn staging_rejects_an_escaping_filesystem_scope() {
        let mut grants = HashMap::new();
        let artifact = ArtifactRequest {
            name: "escaping-tool".to_string(),
            version: "1.0.0".to_string(),
            runtime: ArtifactRuntime::Tool,
            source: None,
            on_overflow: Default::default(),
            capabilities: Some(murmur_artifact::Capabilities {
                peer_fetch: None,
                network: None,
                filesystem: Some(murmur_artifact::FilesystemCapabilities {
                    scope: Some("../escape".to_string()),
                    workdir_exec: false,
                }),
                shell: None,
                spawn: None,
                env: None,
                limits: None,
                resources: None,
                state: None,
                task_io: None,
                containment: None,
            }),
        };

        let err =
            stage_artifact_grant(&artifact, &narrowing_ceiling(), "test-capsule", &mut grants)
                .expect_err("an escaping scope must fail staging");
        assert!(matches!(err, RuntimeError::InvalidFilesystemScope { .. }));
        assert!(grants.is_empty());
    }

    /// The inert-sub-block set is shared with the hook warning: `network`/`filesystem` are
    /// consumed, everything else is reported.
    #[test]
    fn inert_sub_blocks_are_exactly_the_unconsumed_ones() {
        let caps = murmur_artifact::Capabilities {
            peer_fetch: None,
            network: Some(murmur_artifact::NetworkCapabilities {
                allow: Vec::new(),
                unix_sockets: false,
            }),
            filesystem: Some(murmur_artifact::FilesystemCapabilities {
                scope: None,
                workdir_exec: false,
            }),
            shell: Some(murmur_artifact::ShellCapabilities {
                allow: vec!["bash".to_string()],
                strip_env: None,
                baseline_env: None,
                interpreter_runtime: Vec::new(),
                staged_runtime: Vec::new(),
            }),
            spawn: None,
            env: Some(murmur_artifact::EnvCapabilities { allow: Vec::new() }),
            limits: None,
            resources: None,
            // Declared here precisely to assert its absence from the list below: `state` is a
            // sub-block per-artifact narrowing *does* read, so reporting it as inert would tell
            // an operator their durable store was ignored when it was granted.
            state: Some(murmur_artifact::StateCapabilities { store: None }),
            task_io: None,
            containment: Some(murmur_artifact::ContainmentClass::Sealed),
        };

        assert_eq!(
            inert_capability_sub_blocks(Some(&caps)),
            vec!["shell", "env", "containment"]
        );
        assert!(inert_capability_sub_blocks(None).is_empty());
    }

    // ── End-to-end reopen loop (real Wasmtime hook + real process transport) ──────

    /// Write an executable fake `claude`-dialect CLI that, on each spawn, consumes its
    /// stdin then emits exactly one assistant text turn and a success result — so every
    /// agent-loop attempt burns exactly one turn and returns `Ok`.
    fn write_fake_claude_cli(dir: &Path) -> PathBuf {
        let script = dir.join("claude");
        fs::write(
            &script,
            "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}'\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    /// A current-version (`@0.5.0`, 5-case `hook-output`) `on-task-end` hook double that
    /// returns `reopen-task(reason)` on its first `reopen_limit` invocations (tracked by a
    /// mutable core global that persists across a blocking hook's reused store) and `none`
    /// thereafter. `reopen_limit` large ⇒ "always reopen".
    fn on_task_end_reopen_double(
        engine: &wasmtime::Engine,
        reopen_limit: u32,
        reason: &str,
    ) -> wasmtime::component::Component {
        let reason_len = reason.len();
        let stubs = [
            "on-session-start",
            "on-inference",
            "on-tool-call",
            "on-shell",
            "on-compaction",
            "on-session-end",
        ]
        .iter()
        .map(|n| format!("    (export \"{n}\" (func $noop))"))
        .collect::<Vec<_>>()
        .join("\n");
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (global $count (mut i32) (i32.const 0))
    (data (i32.const 300) "{reason}")
    (func (export "realloc") (param i32 i32 i32 i32) (result i32) i32.const 512)
    (func (export "ontaskend") (param i32 i32 i32 i32) (result i32)
      (i32.store (i32.const 128) (i32.const 0))
      (if (result i32) (i32.lt_u (global.get $count) (i32.const {reopen_limit}))
        (then
          (global.set $count (i32.add (global.get $count) (i32.const 1)))
          (i32.store (i32.const 132) (i32.const 4))
          (i32.store (i32.const 136) (i32.const 300))
          (i32.store (i32.const 140) (i32.const {reason_len}))
          (i32.const 128))
        (else
          (i32.store (i32.const 132) (i32.const 0))
          (i32.const 128))))
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
  (export "murmur:hook/lifecycle@0.5.0" (instance $lc))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("on-task-end reopen double WAT parses");
        wasmtime::component::Component::new(engine, &bytes)
            .expect("on-task-end reopen double compiles")
    }

    /// Drive `run_task_with_reopens` once, with a real process-transport agent loop (fake
    /// `claude` CLI, one turn per attempt) and a real Wasmtime `on-task-end` hook that
    /// reopens `reopen_limit` times. Returns the task result and the parsed `trace.jsonl`.
    async fn run_reopen_scenario(
        reopen_limit: u32,
        max_task_reopens: u32,
        max_turns: u32,
    ) -> (Result<(), RuntimeError>, Vec<serde_json::Value>) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        fs::create_dir_all(workdir.join("tools")).unwrap();
        fs::write(workdir.join("task.md"), "Original task: build the thing.").unwrap();
        let cli = write_fake_claude_cli(dir.path());

        let inference = InferenceConfig {
            transport: "process".into(),
            endpoint: None,
            model: "test-model".into(),
            api_key: None,
            driver: None,
            command: Some(cli.to_string_lossy().to_string()),
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns,
            max_tokens: None,
        };

        let mut state = build_test_state(
            Arc::new(FakeSkillRegistry::new(Vec::new())),
            workdir.clone(),
            workdir.join("murmur.lock"),
        );

        let mut trace = TraceWriter::open(
            &workdir,
            "ses_test".to_string(),
            "cap".to_string(),
            "0.1.0".to_string(),
            "test-model".to_string(),
            Vec::new(),
            crate::containment::scope_report_for_tier(
                &CapabilityPolicy::default(),
                murmur_artifact::ContainmentClass::Advisory,
                sandbox::EnforcementTier::EnvironmentOnly,
                None,
                None,
                None,
                Vec::new(),
            ),
            false,
            None,
            false,
        )
        .await
        .unwrap();

        let mut otel = OtelEmitter::new(None, &workdir, "cap".to_string(), "0.1.0".to_string());

        let staged_hook = StagedHookArtifact {
            name: "gatekeeper".to_string(),
            version: "0.0.1".to_string(),
            component: on_task_end_reopen_double(&state.engine, reopen_limit, "tests still fail"),
            config: murmur_artifact::HookConfig {
                binding: HookBinding::OnTaskEnd,
                execution_mode: murmur_artifact::HookExecutionMode::Blocking,
                commit_policy: murmur_artifact::HookCommitPolicy::ReopenTask,
            },
            grant: HookCapabilityGrant::default(),
            on_overflow: Default::default(),
        };

        let mut hooks = HookRuntime::new(
            &state.engine,
            &workdir,
            &workdir,
            vec![staged_hook],
            SessionContextData {
                capsule_name: "cap".to_string(),
                capsule_version: "0.1.0".to_string(),
                session_id: "ses_test".to_string(),
                model: "test-model".to_string(),
                capabilities: Vec::new(),
            },
            HookEnvVars::default(),
            crate::limits::ExecutionLimits::default(),
            None,
        )
        .await
        .unwrap();

        let run_config = agent::AgentRunConfig {
            context_window: 0,
            compaction_threshold: 0.98,
            compaction_model: None,
            compaction_system_prompt: None,
            compaction_dump_summaries: false,
            max_output_tokens: 1024,
        };

        // Caller resets per-task counters via write_task_start before the reopen loop.
        trace
            .write_task_start("tsk_1", "ctx_1", "task_md", 8)
            .await
            .unwrap();

        let result = run_task_with_reopens(
            &mut state,
            &workdir,
            &inference,
            max_task_reopens,
            None,
            run_config,
            &mut hooks,
            &mut trace,
            &mut otel,
            None,
            None,
            &workdir,
            "cap",
            "0.1.0",
            ConversationMode::Stateless,
            Some("ctx_1".to_string()),
            "tsk_1",
        )
        .await;

        trace.flush().await.unwrap();
        let content = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
        let events: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (result, events)
    }

    // ── murmur:task-io/read end to end ────────────────────────────────────────

    use crate::task_io_import::test_support::{reader_double, REPORT_SEP};

    /// A `claude` CLI double that emits a different result sentinel on each invocation,
    /// counted through a file next to the script. A reopened task runs the agent loop more
    /// than once, and telling attempt 2's output from attempt 1's is the whole point of
    /// clearing the slot at attempt start.
    fn write_counting_fake_claude_cli(dir: &Path) -> PathBuf {
        let script = dir.join("claude-counting");
        let counter = dir.join("attempt-counter");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncat > /dev/null\nn=$(cat '{c}' 2>/dev/null || echo 0)\n\
                 n=$((n+1))\necho \"$n\" > '{c}'\n\
                 printf '%s\\n' \"{{\\\"type\\\":\\\"assistant\\\",\\\"message\\\":\
                 {{\\\"content\\\":[{{\\\"type\\\":\\\"text\\\",\\\"text\\\":\
                 \\\"RESULT-$n\\\"}}]}}}}\"\n\
                 printf '%s\\n' \"{{\\\"type\\\":\\\"result\\\",\\\"subtype\\\":\
                 \\\"success\\\",\\\"result\\\":\\\"RESULT-$n\\\"}}\"\n",
                c = counter.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }
        script
    }

    /// The task text every task-io scenario starts from — a sentinel a hook can only have
    /// obtained through the import, since the hook holds no filesystem scope.
    const TASK_SENTINEL: &str = "TASK-SENTINEL: build the thing.";

    /// Drive `run_task_with_reopens` end to end with the `murmur:task-io/read` reader double
    /// as the `on-task-end` hook, on either transport.
    ///
    /// The hook's grant is task-io only: no network rules, no filesystem scope. Everything it
    /// reports in its `reopen-task` reason therefore came through the import. Returns the
    /// task result and the parsed `trace.jsonl`.
    async fn run_task_io_scenario(
        transport: &str,
        task_io_read: bool,
        max_task_reopens: u32,
    ) -> (Result<(), RuntimeError>, Vec<serde_json::Value>) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        fs::create_dir_all(workdir.join("tools")).unwrap();
        fs::write(workdir.join("task.md"), TASK_SENTINEL).unwrap();

        let mut state = build_test_state(
            Arc::new(FakeSkillRegistry::new(Vec::new())),
            workdir.clone(),
            workdir.join("murmur.lock"),
        );

        let inference = if transport == "process" {
            let cli = write_counting_fake_claude_cli(dir.path());
            InferenceConfig {
                transport: "process".into(),
                command: Some(cli.to_string_lossy().to_string()),
                driver: None,
                ..task_io_inference_config()
            }
        } else {
            // The http transport dispatches a WASM driver component out of `tools/<name>`;
            // the directory's existence is what `run_agent_loop` checks before instantiating.
            fs::create_dir_all(workdir.join("tools").join("mock-driver")).unwrap();
            state.tool_components.insert(
                "mock-driver".to_string(),
                crate::inference_import::test_support::driver_double(
                    &state.engine,
                    0,
                    r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"RESULT-1"}]}"#,
                ),
            );
            InferenceConfig {
                transport: "http".into(),
                command: None,
                driver: Some(murmur_artifact::InferenceDriver {
                    artifact: "mock-driver".to_string(),
                    config: None,
                }),
                ..task_io_inference_config()
            }
        };

        let mut trace = TraceWriter::open(
            &workdir,
            "ses_test".to_string(),
            "cap".to_string(),
            "0.1.0".to_string(),
            "test-model".to_string(),
            Vec::new(),
            crate::containment::scope_report_for_tier(
                &CapabilityPolicy::default(),
                murmur_artifact::ContainmentClass::Advisory,
                sandbox::EnforcementTier::EnvironmentOnly,
                None,
                None,
                None,
                Vec::new(),
            ),
            false,
            None,
            false,
        )
        .await
        .unwrap();
        let mut otel = OtelEmitter::new(None, &workdir, "cap".to_string(), "0.1.0".to_string());

        let staged_hook = StagedHookArtifact {
            name: "gatekeeper".to_string(),
            version: "0.0.1".to_string(),
            component: reader_double(&state.engine),
            config: murmur_artifact::HookConfig {
                binding: HookBinding::OnTaskEnd,
                execution_mode: murmur_artifact::HookExecutionMode::Blocking,
                commit_policy: murmur_artifact::HookCommitPolicy::ReopenTask,
            },
            grant: crate::network_policy::HookCapabilityGrant {
                network_allow_rules: Vec::new(),
                filesystem_scope: None,
                task_io_read,
                state_store: None,
                state_dir: None,
            },
            on_overflow: Default::default(),
        };

        let mut hooks = HookRuntime::new(
            &state.engine,
            &workdir,
            &workdir,
            vec![staged_hook],
            SessionContextData {
                capsule_name: "cap".to_string(),
                capsule_version: "0.1.0".to_string(),
                session_id: "ses_test".to_string(),
                model: "test-model".to_string(),
                capabilities: Vec::new(),
            },
            HookEnvVars::default(),
            crate::limits::ExecutionLimits::default(),
            None,
        )
        .await
        .unwrap();

        let run_config = agent::AgentRunConfig {
            context_window: 0,
            compaction_threshold: 0.98,
            compaction_model: None,
            compaction_system_prompt: None,
            compaction_dump_summaries: false,
            max_output_tokens: 1024,
        };

        trace
            .write_task_start("tsk_1", "ctx_1", "task_md", 8)
            .await
            .unwrap();

        let result = run_task_with_reopens(
            &mut state,
            &workdir,
            &inference,
            max_task_reopens,
            None,
            run_config,
            &mut hooks,
            &mut trace,
            &mut otel,
            None,
            None,
            &workdir,
            "cap",
            "0.1.0",
            ConversationMode::Stateless,
            Some("ctx_1".to_string()),
            "tsk_1",
        )
        .await;

        trace.flush().await.unwrap();
        let content = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
        let events: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        (result, events)
    }

    /// The fields both transports share in [`run_task_io_scenario`].
    fn task_io_inference_config() -> InferenceConfig {
        InferenceConfig {
            transport: String::new(),
            endpoint: None,
            model: "test-model".into(),
            api_key: None,
            driver: None,
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
        }
    }

    /// Every `task_reopened` reason in `events`, in order, split into the reader double's
    /// five fields `[A, O, R, LI, LO]`.
    ///
    /// Split from the right: a reopened attempt's `as-given` is the task text plus the
    /// previous attempt's report injected as feedback, separators and all, so only the four
    /// trailing fields are positionally fixed. Everything before them is field `A`.
    fn reopen_reports(events: &[serde_json::Value]) -> Vec<Vec<String>> {
        events
            .iter()
            .filter(|e| e["event_type"] == "task_reopened")
            .map(|e| {
                let mut fields: Vec<String> = e["reason"]
                    .as_str()
                    .unwrap()
                    .rsplitn(5, REPORT_SEP)
                    .map(str::to_string)
                    .collect();
                fields.reverse();
                fields
            })
            .collect()
    }

    /// The shippable outcome: a hook with no filesystem grant and no network grant, granted
    /// only `task_io.read`, reads the task it was given and the result the agent produced at
    /// `on-task-end` and returns both in its `reopen-task` reason.
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_hook_reads_task_and_result_over_the_process_transport() {
        let (_, events) = run_task_io_scenario("process", true, 1).await;
        assert_eq!(
            reopen_reports(&events),
            vec![vec![
                format!("A={TASK_SENTINEL}"),
                format!("O={TASK_SENTINEL}"),
                "R=RESULT-1".to_string(),
                format!("LI={}", TASK_SENTINEL.len()),
                "LO=8".to_string(),
            ]],
            "the hook holds no filesystem scope, so neither sentinel could have come from disk"
        );
    }

    /// Transport parity: the same hook and the same scenario through the WASM-driver loop in
    /// `agent.rs` rather than the subprocess loop in `agent/process.rs`. Each transport funnels
    /// its result-text write through its own recorder, so each transport's own result is what
    /// the hook reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn granted_hook_reads_task_and_result_over_the_http_transport() {
        let (_, events) = run_task_io_scenario("http", true, 1).await;
        assert_eq!(
            reopen_reports(&events),
            vec![vec![
                format!("A={TASK_SENTINEL}"),
                format!("O={TASK_SENTINEL}"),
                "R=RESULT-1".to_string(),
                format!("LI={}", TASK_SENTINEL.len()),
                "LO=8".to_string(),
            ]]
        );
    }

    /// Default-deny end to end: the same double under a grant with `task_io_read: false` still
    /// instantiates, is still dispatched, and still drives a reopen — its reason just carries
    /// the `not-granted` marker instead of either sentinel. The session is not aborted.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_ungranted_hook_reads_nothing_end_to_end() {
        let (_, events) = run_task_io_scenario("process", false, 1).await;
        let reports = reopen_reports(&events);
        assert_eq!(
            reports,
            vec![vec!["A=!0", "O=!0", "R=!0", "LI=!0", "LO=!0"]],
            "every read is not-granted, and no length leaks either"
        );
    }

    /// Reopen semantics across three attempts. `original` is byte-identical on every attempt;
    /// `as-given` picks up the `# Reopen feedback` section `build_reopen_task_md` writes; and
    /// the output read on attempt N is attempt N's own result, never attempt N-1's.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopened_attempts_see_their_own_input_and_their_own_output() {
        let (result, events) = run_task_io_scenario("process", true, 2).await;
        assert!(
            result.is_err(),
            "the reader double always reopens, so the budget runs out"
        );
        let reports = reopen_reports(&events);
        assert_eq!(reports.len(), 2, "max_task_reopens: 2 ⇒ two reopens");

        for (index, report) in reports.iter().enumerate() {
            assert_eq!(
                report[1],
                format!("O={TASK_SENTINEL}"),
                "`original` never changes across attempts"
            );
            assert_eq!(
                report[2],
                format!("R=RESULT-{}", index + 1),
                "attempt {} must read its own result, never the previous attempt's",
                index + 1
            );
        }

        assert_eq!(
            reports[0][0],
            format!("A={TASK_SENTINEL}"),
            "attempt 1 was handed the pristine task"
        );
        let attempt_two_input = &reports[1][0];
        assert!(
            attempt_two_input.starts_with(&format!("A={TASK_SENTINEL}"))
                && attempt_two_input.contains("# Reopen feedback"),
            "attempt 2's `as-given` is the pristine task plus the injected feedback, got: \
             {attempt_two_input}"
        );
        assert_eq!(
            reports[1][3],
            format!("LI={}", attempt_two_input.len() - "A=".len()),
            "input-len reports the byte length of the very text read back"
        );
    }

    fn count_type(events: &[serde_json::Value], ty: &str) -> usize {
        events.iter().filter(|e| e["event_type"] == ty).count()
    }

    /// Happy path: one reopen, then the hook is satisfied. The agent loop runs twice, one
    /// `task_reopened` sits between the attempts naming the hook and its feedback, and the
    /// terminal `task_end` shows `reopen_count: 1` with the second attempt's real outcome.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopen_once_then_satisfied_end_to_end() {
        let (result, events) = run_reopen_scenario(1, 5, 10).await;
        assert!(
            result.is_ok(),
            "a satisfied hook ends the task Ok: {result:?}"
        );
        assert_eq!(
            count_type(&events, "inference"),
            2,
            "agent loop ran exactly twice"
        );
        assert_eq!(count_type(&events, "task_reopened"), 1);
        let re = events
            .iter()
            .find(|e| e["event_type"] == "task_reopened")
            .unwrap();
        assert_eq!(re["hook_name"], "gatekeeper");
        assert_eq!(re["reason"], "tests still fail");
        assert_eq!(re["reopen_number"], 1);
        let end = events
            .iter()
            .find(|e| e["event_type"] == "task_end")
            .unwrap();
        assert_eq!(end["reopen_count"], 1);
        assert_eq!(end["exit_status"], "ok");
    }

    /// Budget exhausted: a hook that always reopens with `lifecycle.max_task_reopens: 1` runs
    /// the loop exactly twice, then ends `reopen_budget_exhausted` (an `Err`, so downstream
    /// task-failure branches fire) with `reopen_count: 1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopen_budget_exhausted_end_to_end() {
        let (result, events) = run_reopen_scenario(99, 1, 10).await;
        assert!(
            result.is_err(),
            "an exhausted reopen budget is a task failure"
        );
        assert_eq!(count_type(&events, "inference"), 2, "1 original + 1 reopen");
        assert_eq!(count_type(&events, "task_reopened"), 1);
        let end = events
            .iter()
            .find(|e| e["event_type"] == "task_end")
            .unwrap();
        assert_eq!(end["reopen_count"], 1);
        assert_eq!(end["exit_status"], "reopen_budget_exhausted");
    }

    /// Turn ceiling respected: `inference.max_turns: 3`, `lifecycle.max_task_reopens: 5`, a
    /// hook that always reopens, one turn per attempt. Cumulative `inference` records never
    /// exceed 3, and the task ends `reopen_budget_exhausted` once turns run out even though
    /// reopens remain in the budget.
    #[tokio::test(flavor = "multi_thread")]
    async fn reopen_never_exceeds_max_turns_end_to_end() {
        let (result, events) = run_reopen_scenario(99, 5, 3).await;
        assert!(result.is_err());
        assert_eq!(
            count_type(&events, "inference"),
            3,
            "cumulative turns must never exceed max_turns"
        );
        assert_eq!(
            count_type(&events, "task_reopened"),
            2,
            "3 attempts ⇒ 2 reopens"
        );
        let end = events
            .iter()
            .find(|e| e["event_type"] == "task_end")
            .unwrap();
        assert_eq!(end["reopen_count"], 2);
        assert_eq!(end["exit_status"], "reopen_budget_exhausted");
    }
}
