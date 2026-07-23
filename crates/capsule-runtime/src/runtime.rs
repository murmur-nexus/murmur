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
    ArtifactImplementation, ArtifactRuntime, ContextConfig, HookBinding, LifecycleConfig, LockedArtifact,
    LockedSha256, LockfileError, MurmurLock, Registry, RegistryError, RuntimeType,
    TaskAcceptance, LOCK_VERSION, MANIFEST_FILENAME, PACKED_MANIFEST_ENTRY, W_SEC_003,
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
    errors::RuntimeError,
    hooks::{
        dispatch_stage, HookEnvVars, HookEvent, HookRuntime, SessionContextData, ShellDispatchInfo,
    },
    identity::{self, CapsuleIdentity},
    inference_import::HookInferenceCtx,
    limits::{classify_guest_failure, EpochTicker, ExecutionLimiter, GuestFailure},
    murmur_md,
    network_policy::{
        parse_network_allow_rules, validate_filesystem_scope, NetworkAllowRule, RequestTarget,
    },
    otel::OtelEmitter,
    outgoing, sandbox,
    shell::{
        build_shell_env, build_wasi_env_allowlist, execute_shell, is_shell_interpreter,
        shell_tool_manifest_yaml, split_shell_words, ShellResult,
    },
    streaming::{
        emit_chunk_sse, emit_sse, emit_thinking_chunk_sse, SseBroadcast, SseEventBuffer,
        StreamStatus, TaskStatusUpdateEvent,
    },
    trace::TraceWriter,
    types::{
        CapabilityPolicy, DispatchOutcome, InstalledArtifactSummary, LaunchResult,
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
    hooks
        .iter()
        .any(|h| matches!(h.config.binding, HookBinding::OnCompaction | HookBinding::All))
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
    let mut hook_components = Vec::new();
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
                        let binary = extract_native_binary(
                            &artifact.name,
                            &resolved_version,
                            &resolved.bytes,
                        )?;
                        native_binaries.push((artifact.name.clone(), binary));
                    }
                    ArtifactImplementation::Wasm => {
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
                hook_components.push(StagedHookArtifact {
                    name: artifact.name.clone(),
                    version: resolved_version.clone(),
                    component: hook_component,
                    config: hook_config,
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

    // Dispatch on-stage hooks synchronously now that manifests are in place.
    let stage_env = HookEnvVars::default();
    dispatch_stage(
        &engine,
        &workdir,
        &hook_components,
        request.capability_policy.shell_allow.clone(),
        &stage_env,
        request.capability_policy.limits,
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
        context: request.context,
        engine,
        capsule_component,
        tool_components,
        hook_components,
        allowlisted_tools: request.allowlisted_tools,
        capability_policy: request.capability_policy,
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
        job_id: request.job_id,
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
    let shell_enforcement = sandbox::ShellEnforcement::resolve(&staged.capability_policy)
        .map_err(RuntimeError::Runtime)?;
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
            max_output_tokens: inference
                .max_tokens
                .unwrap_or(agent::DEFAULT_MAX_OUTPUT_TOKENS),
        };

        // --- Identity and HTTP server setup ---

        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| RuntimeError::Runtime(format!("failed to create tokio runtime: {e}")))?;

        let (tcp_listener, external_port) = rt
            .block_on(identity::bind_local_port(&staged.bind_addr, staged.internal_port))?;

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
        if let Some(ref job_id) = staged.job_id {
            all_env.push(("MURMUR_JOB_ID".to_string(), job_id.clone()));
        }

        murmur_md::write_murmur_md(
            &workdir,
            Some(inference),
            staged.context.as_ref(),
            has_compaction_hook(&staged.hook_components),
            &staged.capability_policy,
            &capsule_identity,
        );

        sandbox::warn_for_enforcement_tier(shell_enforcement.tier, &workdir, &staged.capability_policy);

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
        let (sse_tx, _) = tokio::sync::broadcast::channel::<std::sync::Arc<String>>(SSE_BROADCAST_CAPACITY);
        let sse_buffer = std::sync::Arc::new(Mutex::new(SseEventBuffer::new(SSE_REPLAY_CAPACITY)));

        let capsule_name = staged.capsule_name.clone();
        let capabilities = capability_names(&staged.capability_policy);

        // Capture staged fields that move into the async block
        let hook_components = staged.hook_components;
        let tool_components = staged.tool_components;
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
                trace_include_tool_output,
            )
            .await
            .map_err(|e| RuntimeError::AgentLoopFailed(format!("failed to open trace.jsonl: {e}")))?;

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
                        ));

                    // Read before `capability_policy` moves into the store state below.
                    let hook_limits = capability_policy.limits;

                    // Build CapsuleStoreState ONCE — reused across all task iterations
                    let mut state = CapsuleStoreState {
                        // Agent capsules have no WASM component of their own, so this state
                        // never backs a `Store` and this limiter is never registered. It is
                        // the per-tool limiters built in `dispatch_tool_async` (from
                        // `capability_policy.limits`) that bound this path's guests.
                        limits: capability_policy.limits.limiter(),
                        table: ResourceTable::new(),
                        wasi: build_wasi_ctx(&accessible_workdir, &all_env, &capability_policy)?,
                        http: WasiHttpCtx::new(),
                        http_hooks: NetworkPolicyHooks {
                            network_allow_rules: network_allow_rules.clone(),
                        },
                        network_allow_rules,
                        inference_env: all_env,
                        engine: engine.clone(),
                        workdir: workdir.clone(),
                        accessible_workdir: accessible_workdir.clone(),
                        tool_components,
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
                            Arc::new(HookInferenceCtx {
                                driver_name,
                                driver_component,
                                model: inference_model.clone(),
                                engine: state.engine.clone(),
                                accessible_workdir: state.accessible_workdir.clone(),
                                inference_env: state.inference_env.clone(),
                                capability_policy: state.capability_policy.clone(),
                                network_allow_rules: state.network_allow_rules.clone(),
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
                                    let result = agent::run_agent_loop(
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
                                        Some(context_id.clone()),
                                    )
                                    .await;
                                    let exit_str =
                                        if result.is_ok() { "ok" } else { "failed" };
                                    let _ = trace.write_task_end(&task_id, exit_str).await;
                                    hooks
                                        .emit(
                                            &workdir,
                                            HookEvent::TaskEnd {
                                                task_id: task_id.clone(),
                                                exit_status: exit_str.to_string(),
                                            },
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
                                    let result = agent::run_agent_loop(
                                        &mut state,
                                        &workdir,
                                        inference,
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
                                    )
                                    .await;
                                    let exit_str =
                                        if result.is_ok() { "ok" } else { "failed" };
                                    let _ = trace.write_task_end(&task_id, exit_str).await;
                                    hooks
                                        .emit(
                                            &workdir,
                                            HookEvent::TaskEnd {
                                                task_id: task_id.clone(),
                                                exit_status: exit_str.to_string(),
                                            },
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
                        let loop_result = agent::run_agent_loop(
                            &mut state,
                            &workdir,
                            inference,
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
                        )
                        .await;

                        // ── POST-LOOP SLOT UPDATE ──
                        let exit_state = if loop_result.is_ok() {
                            TaskState::Completed
                        } else {
                            TaskState::Failed
                        };
                        let exit_str = if loop_result.is_ok() { "ok" } else { "failed" };
                        let _ = trace.write_task_end(&incoming.task_id, exit_str).await;
                        hooks
                            .emit(
                                &workdir,
                                HookEvent::TaskEnd {
                                    task_id: incoming.task_id.clone(),
                                    exit_status: exit_str.to_string(),
                                },
                            )
                            .await;
                        let _ = trace.flush().await;
                        {
                            let mut reg = task_registry.lock().unwrap();
                            reg.finish_task(exit_state);
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
            &inference_env,
            &staged.capability_policy,
        )?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: network_allow_rules.clone(),
        },
        network_allow_rules,
        inference_env,
        engine: staged.engine.clone(),
        workdir: staged.workdir.clone(),
        accessible_workdir: staged.accessible_workdir.clone(),
        tool_components: staged.tool_components,
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

        let returned = run.post_return_async(&mut store).await;
        match returned {
            Ok(()) => Ok(()),
            Err(err) => Err(capsule_guest_error(&err, &store.data().limits)),
        }
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

fn resolve_system_prompt(
    manifest_dir: &Path,
    workdir: &Path,
    inference: &murmur_artifact::InferenceConfig,
) -> Result<Option<String>, RuntimeError> {
    if let Some(prompt) = inference.system_prompt.as_ref() {
        return Ok(Some(prompt.clone()));
    }

    if let Some(path) = inference.system_prompt_file.as_ref() {
        let prompt_path = manifest_dir.join(path);
        return fs::read_to_string(&prompt_path)
            .map(Some)
            .map_err(|source| RuntimeError::SystemPromptFileRead {
                path: prompt_path.display().to_string(),
                source,
            });
    }

    if let Some(art_name) = inference.system_prompt_artifact.as_ref() {
        let skill_path = workdir.join("tools").join(art_name).join("skill.md");
        return fs::read_to_string(&skill_path)
            .map(Some)
            .map_err(|source| RuntimeError::SystemPromptArtifactRead {
                name: art_name.clone(),
                source,
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
        let prompt_path = manifest_dir.join(path);
        return fs::read_to_string(&prompt_path).map(Some).map_err(|source| {
            RuntimeError::CompactionSystemPromptFileRead {
                path: prompt_path.display().to_string(),
                source,
            }
        });
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
            &format!("[capability-policy] warning[{W_SEC_003}]: {BASH_NETWORK_BYPASS_WARNING} ({link})"),
        );
    }
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
    config.async_support(true);
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
fn build_wasi_ctx(
    workdir: &Path,
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
    builder
        .preopened_dir(workdir, ".", DirPerms::all(), FilePerms::all())
        .map_err(|err| RuntimeError::wasi(workdir.to_path_buf(), err.to_string()))?;

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
            inference.driver.as_ref().map(|d| d.artifact.clone()).unwrap_or_default(),
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
            status: StreamStatus { state: "input-required".into(), message: prompt.clone(), response: None },
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
                    status: StreamStatus { state: "working".into(), message: "resumed".into(), response: None },
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
    pub(crate) inference_env: Vec<(String, String)>,
    pub(crate) engine: Engine,
    pub(crate) workdir: PathBuf,
    pub(crate) accessible_workdir: PathBuf,
    pub(crate) tool_components: HashMap<String, Component>,
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
        // Enforce network policy
        let for_parse = if peer_url.contains("://") {
            peer_url.clone()
        } else {
            format!("http://{peer_url}")
        };
        let uri: http::Uri = for_parse
            .parse()
            .map_err(|e| format!("invalid peer URL '{peer_url}': {e}"))?;
        let target = RequestTarget::from_request(&uri, false)
            .ok_or_else(|| format!("invalid peer URL '{peer_url}'"))?;
        if !self
            .network_allow_rules
            .iter()
            .any(|rule| rule.matches(&target))
        {
            return Err(format!(
                "network policy: '{peer_url}' not in capabilities.network.allow"
            ));
        }

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
        } else if self.capability_policy.shell_allow.iter().any(|b| b == &name) {
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
        let manifest_yaml =
            extract_manifest_yaml(&name, &version, &resolved.bytes).map_err(|err| err.to_string())?;
        write_tool_manifest(&self.workdir, &name, &manifest_yaml).map_err(|err| err.to_string())?;

        let (artifact_runtime, implementation, wasm_component) = match resolved.meta.runtime {
            RuntimeType::Wasm => {
                let wasm_bytes =
                    extract_root_wasm(&name, &version, &resolved.bytes).map_err(|err| err.to_string())?;
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
                (ArtifactRuntime::Tool, Some(ArtifactImplementation::Native), None)
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
            capabilities: "artifact-manager/search and remove are not implemented"
                .to_string(),
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
    pub(crate) network_allow_rules: &'a [NetworkAllowRule],
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
            move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>,
                  (chunk,): (String,)| {
                chunks_emitted_flag.store(true, Ordering::Relaxed);
                if let (Some((ref tx, ref buf)), Some(ref tid)) =
                    (&sse_for_chunk, &task_id_for_chunk)
                {
                    emit_chunk_sse(tx, buf, &chunk_event_id, tid, &chunk);
                }
                Ok(())
            },
        )
        .map_err(|err| {
            format!("failed to register emit-chunk for tool '{name}': {err}")
        })?;

        inst.func_wrap(
            "emit-thinking-chunk",
            move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>,
                  (chunk,): (String,)| {
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
            .map_err(|err| {
                format!("failed to define {task_iface} instance for '{name}': {err}")
            })?
            .func_wrap_async(
                "request-input",
                move |_store: wasmtime::StoreContextMut<'_, ToolStoreState>,
                      (prompt,): (String,)| {
                    let reg = ri_task_registry.clone();
                    let sse = ri_sse.clone();
                    let tid = ri_task_id.clone();
                    let fut: std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = wasmtime::Result<(String,)>>
                                + Send,
                        >,
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
            .map_err(|err| {
                format!("failed to register request-input for tool '{name}': {err}")
            })?;
    }

    let tool_limits = capability_policy.limits;
    let state = ToolStoreState {
        limits: tool_limits.limiter(),
        table: ResourceTable::new(),
        wasi: build_wasi_ctx(
            accessible_workdir,
            inference_env,
            capability_policy,
        )
        .map_err(|err| format!("failed to build WASI context for tool '{name}': {err}"))?,
        http: WasiHttpCtx::new(),
        http_hooks: NetworkPolicyHooks {
            network_allow_rules: network_allow_rules.to_vec(),
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
    // also the driver path (`agent.rs` dispatches the inference driver through here),
    // which before this slice had no deadline of any kind.
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
    run.post_return_async(&mut store)
        .await
        .map_err(|err| format!("tool '{name}' post-return failed: {err}"))?;
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
                dispatch_shell_tool(&name, input, &workdir, &env_overrides, &policy, &enforcement)
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
fn write_tool_manifest(workdir: &Path, name: &str, manifest_yaml: &str) -> Result<(), RuntimeError> {
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

    let env = build_shell_env(policy, &[], workdir)?;

    let mut child = Command::new(binary_path)
        .current_dir(workdir)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
                command: command.clone(),
                exit_code: result.exit_code,
                stdout: result.stdout.clone(),
                stderr: result.stderr.clone(),
                stdout_bytes: result.stdout.len() as u64,
                stderr_bytes: result.stderr.len() as u64,
                duration_ms: result.duration_ms,
            };
            DispatchOutcome {
                result: shell_result_to_tool_result(&command, result),
                shell: Some(shell),
                is_skill: false,
            }
        }
        Err(error) => DispatchOutcome::tool(murmur::tool::run::ToolResult {
            status: murmur::tool::run::Status::Error,
            summary: Some("shell execution failed".to_string()),
            data: Some(error),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }),
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

fn shell_result_to_tool_result(command: &str, result: ShellResult) -> murmur::tool::run::ToolResult {
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
    use murmur_artifact::{ArtifactMeta, ArtifactRuntime, Registry, ResolvedArtifact, RuntimeType};
    use tempfile::TempDir;

    use super::*;

    fn bootstrap_log_contents(workdir: &Path) -> String {
        fs::read_to_string(workdir.join("logs").join("bootstrap.log")).unwrap_or_default()
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
        assert!(log.contains(W_SEC_003), "log should carry its warning code: {log}");
        assert!(
            log.contains(&security_warning_link(W_SEC_003)),
            "log should link to the security-warnings doc page: {log}"
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
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: None,
            capability_policy: CapabilityPolicy::default(),
            inference: None,
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
            job_id: None,
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
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "echo-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "different".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
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
            job_id: None,
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
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "different-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "abc".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
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
            job_id: None,
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
            }],
            allowlisted_tools: HashSet::from(["echo-tool".to_string()]),
            lock_expectations: Some(vec![crate::types::LockExpectation {
                name: "echo-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256_wasm: "abc".to_string(),
            }]),
            capability_policy: CapabilityPolicy::default(),
            inference: None,
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
            job_id: None,
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
        let bytes =
            load_local_skill_md(manifest_dir.path(), &skill.to_string_lossy()).unwrap();
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
            }],
            allowlisted_tools: HashSet::new(),
            lock_expectations: None,
            capability_policy: CapabilityPolicy::default(),
            inference: Some(inference),
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
            job_id: None,
        };

        let staged = stage_session(Arc::new(PanicRegistry), request).unwrap();
        let installed = staged.workdir.join("tools").join("my-skill").join("skill.md");
        assert!(installed.exists(), "skill.md not installed at {}", installed.display());
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
        let wasi = build_wasi_ctx(&workdir, &[], &CapabilityPolicy::default()).unwrap();
        CapsuleStoreState {
            limits: crate::limits::ExecutionLimits::default().limiter(),
            table: ResourceTable::new(),
            wasi,
            http: WasiHttpCtx::new(),
            http_hooks: NetworkPolicyHooks {
                network_allow_rules: Vec::new(),
            },
            network_allow_rules: Vec::new(),
            inference_env: Vec::new(),
            engine,
            workdir: workdir.clone(),
            accessible_workdir: workdir,
            tool_components: HashMap::new(),
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
        assert_eq!(state.active_continuation(Some("ctx-a")), Some(("cont-1", 2)));

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
            (PACKED_MANIFEST_ENTRY, b"name: my-skill\nversion: 1.0.0\nruntime: skill\n"),
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
        let entry = lock.artifact_for("my-skill").expect("lock entry for my-skill");
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

        let mut state = build_test_state(Arc::new(TamperedRegistry), workdir.clone(), lock_path.clone());

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
            (PACKED_MANIFEST_ENTRY, b"name: my-skill\nversion: 2.0.0\nruntime: skill\n"),
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
        assert!(err.contains("murmur.lock conflict"), "unexpected error message: {err}");

        assert!(!workdir.join("tools").join("my-skill").join("skill.md").exists());
        assert!(state.installed_artifacts.is_empty());

        // Lock must be left exactly as it was.
        let lock = read_lockfile(&lock_path).unwrap();
        let entry = lock.artifact_for("my-skill").unwrap();
        assert_eq!(entry.resolved_version, "1.0.0");
        assert_eq!(entry.sha256.wasm, "pinned-hash-from-earlier-pull");
    }

    /// H-3 evidence-of-done: two independent `stage_session` calls against the same
    /// `--workdir` must share the same `accessible_workdir` (the checkpoint signing root),
    /// even though `workdir` (internal staging dir) is fresh each time. A checkpoint file
    /// signed by the first "session" (via a `SessionEnd` dispatch) must verify cleanly on
    /// the second, independent "session"'s `SessionStart` dispatch; tampering it between the
    /// two must cause the second session to quarantine it.
    #[test]
    fn checkpoint_signing_survives_resume_across_independent_stage_sessions() {
        use crate::checkpoint_sign::test_support::{with_home, HOME_LOCK};
        use crate::hooks::HookEvent;
        use murmur_artifact::{InferenceConfig, InferenceDriver};

        struct EmptyRegistry;
        impl Registry for EmptyRegistry {
            fn resolve(&self, _: &str, _: &str) -> Result<ResolvedArtifact, RegistryError> {
                unreachable!("no artifacts declared in this test")
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

        fn minimal_inference() -> InferenceConfig {
            InferenceConfig {
                transport: "http".into(),
                endpoint: Some("http://localhost".into()),
                model: "test-model".into(),
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
            }
        }

        fn stage(user_dir: &Path, manifest_dir: &Path) -> StagedSession {
            let request = StageRequest {
                manifest_dir: manifest_dir.to_path_buf(),
                capsule_name: "test".to_string(),
                capsule_version: "0.0.1".to_string(),
                capsule_component_bytes: Vec::new(),
                artifacts: Vec::new(),
                allowlisted_tools: HashSet::new(),
                lock_expectations: None,
                capability_policy: CapabilityPolicy::default(),
                inference: Some(minimal_inference()),
                context: None,
                otel_endpoint: None,
                eval_config_json: None,
                case_id: None,
                dataset_id: None,
                lifecycle: None,
                lifecycle_override: None,
                trace: None,
                workdir: Some(user_dir.to_path_buf()),
                bind_addr: "127.0.0.1".to_string(),
                internal_port: None,
                job_id: None,
            };
            stage_session(Arc::new(EmptyRegistry), request).unwrap()
        }

        async fn hook_runtime_for(staged: &StagedSession) -> HookRuntime {
            HookRuntime::new(
                &staged.engine,
                &staged.workdir,
                &staged.accessible_workdir,
                Vec::new(),
                SessionContextData {
                    capsule_name: staged.capsule_name.clone(),
                    capsule_version: staged.capsule_version.clone(),
                    session_id: staged.session_id.clone(),
                    model: "test-model".to_string(),
                    capabilities: Vec::new(),
                },
                HookEnvVars::default(),
                crate::limits::ExecutionLimits::default(),
                None,
            )
            .await
            .unwrap()
        }

        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let manifest_dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();

        let (first, second) = with_home(home.path(), || {
            let first = stage(user_dir.path(), manifest_dir.path());
            let second = stage(user_dir.path(), manifest_dir.path());
            (first, second)
        });

        // The design doc's central empirical question: does `accessible_workdir` stay stable
        // (and thus usable as the checkpoint signing root) across two independent launches
        // against the same `--workdir`, even though the internal `workdir` is regenerated?
        assert_eq!(first.accessible_workdir, user_dir.path());
        assert_eq!(second.accessible_workdir, user_dir.path());
        assert_ne!(
            first.workdir, second.workdir,
            "internal staging workdir must be fresh per session_id"
        );
        assert_ne!(first.session_id, second.session_id);

        let checkpoints = first.accessible_workdir.join("checkpoints");
        fs::create_dir_all(&checkpoints).unwrap();
        fs::write(checkpoints.join("summary.md"), "goals: ship it").unwrap();

        with_home(home.path(), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // "Session one" ends, signing whatever checkpoint state exists.
                let mut hooks_one = hook_runtime_for(&first).await;
                hooks_one
                    .emit(
                        &first.workdir,
                        HookEvent::SessionEnd {
                            total_turns: 1,
                            exit_status: "ok".to_string(),
                        },
                    )
                    .await;

                // "Session two" is an independently staged/launched resume against the same
                // --workdir. Its SessionStart must verify the checkpoint session one signed.
                let mut hooks_two = hook_runtime_for(&second).await;
                hooks_two
                    .emit(&second.workdir, HookEvent::SessionStart)
                    .await;
            });
        });

        assert!(
            checkpoints.join("summary.md").exists(),
            "validly-signed checkpoint must survive an independent resume"
        );
        assert!(!checkpoints.join("summary.md.rejected").exists());

        // Simulate tampering (e.g. a compromised tool) between the two sessions.
        fs::write(checkpoints.join("summary.md"), "tampered by a compromised tool").unwrap();

        with_home(home.path(), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut hooks_three = hook_runtime_for(&second).await;
                hooks_three
                    .emit(&second.workdir, HookEvent::SessionStart)
                    .await;
            });
        });

        assert!(
            !checkpoints.join("summary.md").exists(),
            "tampered checkpoint must be renamed away on the next resume"
        );
        assert!(checkpoints.join("summary.md.rejected").exists());
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
        )
        .unwrap();
        std::env::remove_var("MYCOMPANY_SECRET");

        let data = result.data.unwrap_or_default();
        assert!(
            !data.contains("leaked-secret"),
            "policy.shell_strip_env pattern must compose for native tool subprocess, got: {data}"
        );
    }
}
