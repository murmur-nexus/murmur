use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, configured_artifact_names, explain_scope,
    launch_session, stage_session, state_store_reports, AfterTask, ArtifactRequest,
    LifecycleOverride, LockExpectation, ResumeMode, ResumeRequest, RuntimeError, StageRequest,
    TaskAcceptance,
};
use murmur_artifact::{
    current_platform, effective_containment_floor, load_dotenv_non_override, load_runtime_manifest,
    read_lockfile, write_lockfile_atomic, ArtifactRuntime, ContainmentClass, InferenceConfig,
    LocalRegistry, LockedArtifact, LockedSha256, LockfileError, MurmurLock, Registry,
    ResolvedArtifact, LOCK_VERSION,
};

use crate::{
    commands::trace::{first_task_context_id, resolve_session_dir},
    config::load_effective_mur_config_if_any_exists,
    error::{
        CliError, E_IO_003, E_RUN_003, E_RUN_004, E_RUN_006, E_RUN_008, E_RUN_015, E_RUN_016,
        E_RUN_019,
    },
    registry_client::FallbackRegistry,
};

use super::{
    fail_run, lockfile_error_to_cli, print_run_output, runtime_manifest_error_to_cli, RunStatus,
};

/// In --json mode errors must not produce any stdout output — return the error as-is.
/// In human mode, delegate to fail_run which prints the status line.
fn fail(session_id: &str, workdir: &std::path::Path, error: CliError, json: bool) -> CliError {
    if json {
        error
    } else {
        fail_run(session_id, workdir, error)
    }
}

/// Where this launch's sessions live, and therefore where `--resume` looks for the one it names.
///
/// The two layouts are not interchangeable, and `--resume` has to match whichever this launch
/// will use: `--workdir <dir>` puts sessions at `<dir>/.murmur/<ses_id>`, and without it they go
/// to `<manifest_dir>/workdir/<ses_id>`. Never `$CWD/workdir` — a `--manifest` pointing
/// elsewhere would then resolve `@1` against a directory this capsule never wrote to.
fn session_root(project_dir: &Path, workdir_arg: Option<&Path>) -> PathBuf {
    match workdir_arg {
        Some(dir) => absolutise(dir).join(".murmur"),
        None => project_dir.join("workdir"),
    }
}

/// `dir` made absolute against the current directory, unchanged when it already is.
fn absolutise(dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(dir)
    }
}

/// `--resume <session>` turned into the context id the named session ran under, plus the request
/// the runtime stages under.
///
/// Resolved here, before `stage_session`, and therefore before this launch's own session
/// directory exists — which is what makes `@1` name the previous run rather than the one being
/// started. `--resume` is sugar over `--context`: the id this returns goes into the same
/// `StageRequest.context_id` field `--context` fills, and there is no second load path.
fn resolve_resume(
    resume_arg: &str,
    resume_mode: ResumeMode,
    project_dir: &Path,
    workdir_arg: Option<&Path>,
) -> Result<(ResumeRequest, String), CliError> {
    let root = session_root(project_dir, workdir_arg);
    let session_dir = resolve_session_dir(resume_arg, &root, "--resume")?;
    let session = session_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| resume_arg.to_string());
    let context_id = first_task_context_id(&session_dir)?.ok_or_else(|| {
        CliError::with_hint(
            E_RUN_016,
            format!(
                "cannot resume session {session}: its trace.jsonl records no task_start carrying \
                 a context id"
            ),
            "only a session that actually ran a task has a conversation to continue. Run \
             `mur trace show <session>` to see what it did, and resume one that reached a task \
             — see docs/content/reference/cli.md",
        )
    })?;
    Ok((
        ResumeRequest {
            from_session: session,
            mode: resume_mode,
        },
        context_id,
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_run(
    manifest_arg: &Path,
    capsule_arg: Option<&str>,
    capsule_version_arg: Option<&str>,
    spawn_grant_stdin: bool,
    task_arg: Option<&str>,
    system_prompt_arg: Option<&str>,
    context_id_arg: Option<&str>,
    resume_arg: Option<&str>,
    resume_mode: capsule_runtime::ResumeMode,
    lifecycle_task_acceptance: Option<&str>,
    lifecycle_after_task: Option<&str>,
    workdir_arg: Option<PathBuf>,
    json: bool,
    verbose: bool,
    bind_addr: &str,
    no_env_file: bool,
    containment_arg: Option<&str>,
    explain_scope_only: bool,
) -> Result<(), CliError> {
    let mut session_id = "n/a".to_string();
    let mut workdir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("/"))
        .join("workdir");

    // `--capsule NAME --capsule-version VERSION` runs an installed artifact rather than a project
    // directory: nothing is read from disk beside the artifact itself, and the only directory
    // involved is the workdir. It is the path a delegated child is launched on.
    let installed_capsule = capsule_arg.zip(capsule_version_arg);

    let (manifest_path, project_dir) = match installed_capsule {
        Some(_) => {
            let dir = match workdir_arg.as_deref() {
                Some(dir) => absolutise(dir),
                None => std::env::current_dir().map_err(|source| {
                    fail(
                        &session_id,
                        &workdir,
                        CliError::new(
                            E_IO_003,
                            format!("failed to determine current working directory: {source}"),
                        ),
                        json,
                    )
                })?,
            };
            // Never read; the manifest comes out of the artifact. Kept so the two modes share one
            // `manifest_dir`, which is what a manifest-relative system prompt resolves against.
            (dir.join("murmur.yaml"), dir)
        }
        None => {
            let manifest_path = if manifest_arg.is_absolute() {
                manifest_arg.to_path_buf()
            } else {
                let cwd = std::env::current_dir().map_err(|source| {
                    fail(
                        &session_id,
                        &workdir,
                        CliError::new(
                            E_IO_003,
                            format!("failed to determine current working directory: {source}"),
                        ),
                        json,
                    )
                })?;
                cwd.join(manifest_arg)
            };
            let project_dir = manifest_path
                .parent()
                .map(|p| p.to_path_buf())
                .ok_or_else(|| {
                    fail(
                        &session_id,
                        &workdir,
                        CliError::new(E_IO_003, "failed to determine manifest directory"),
                        json,
                    )
                })?;
            (manifest_path, project_dir)
        }
    };
    workdir = project_dir.join("workdir");

    // Read before anything else this launch does, and read exactly once: the parent's runtime
    // writes one line and closes the pipe, so a launch that deferred the read past a refusal
    // would leave the parent blocked on a reader that never arrives.
    let spawn_grant = if spawn_grant_stdin {
        Some(read_spawn_grant().map_err(|error| fail(&session_id, &workdir, error, json))?)
    } else {
        None
    };

    // Read here, ahead of staging, for the same reason the grant is: a child that cannot read
    // the handle its parent injected cannot report its outcome to anybody, and running it would
    // produce work whose result reaches nobody. Refusing before `stage_session` leaves no session
    // directory and no trace behind. The value itself is read a second time by `launch_session`,
    // which holds the reporting guard.
    if let Err(error) = capsule_runtime::SpawnerHandle::from_env() {
        return Err(fail(&session_id, &workdir, CliError::from(error), json));
    }

    // Resolved here, ahead of everything staging does, for two reasons: `stage_session` is what
    // creates this launch's `ses_*` directory, so `@1` must be read while the most recent session
    // is still the *previous* run; and a refusal must leave nothing behind to clean up.
    if resume_arg.is_some() && context_id_arg.is_some() {
        return Err(fail(
            &session_id,
            &workdir,
            CliError::with_hint(
                E_RUN_015,
                "--resume and --context name the same thing two ways",
                "--resume <session> resolves that session's context id for you; --context <id> \
                 names one directly. Pass whichever you have, not both — see \
                 docs/content/reference/cli.md",
            ),
            json,
        ));
    }
    let resume = match resume_arg {
        Some(arg) => Some(
            resolve_resume(arg, resume_mode, &project_dir, workdir_arg.as_deref())
                .map_err(|error| fail(&session_id, &workdir, error, json))?,
        ),
        None => None,
    };
    // `--resume` is sugar over `--context`: one field, one record, one set of bookkeeping.
    let context_id = resume
        .as_ref()
        .map(|(_, context_id)| context_id.clone())
        .or_else(|| context_id_arg.map(str::to_string));

    let workspace_root = find_workspace_root(&project_dir);
    if !no_env_file {
        load_dotenv_non_override(&workspace_root).map_err(|source| {
            fail(
                &session_id,
                &workdir,
                CliError::new(E_IO_003, source.to_string()),
                json,
            )
        })?;
    }

    // In `--capsule` mode the artifact's own bytes are the manifest *and* the component, read in
    // memory: a capsule launched by name is staged from exactly what the registry holds rather
    // than from a directory somebody laid out.
    let (runtime_manifest, installed_bytes) = match installed_capsule {
        Some((name, version)) => {
            let resolved = resolve_installed_capsule(&project_dir, name, version)
                .map_err(|error| fail(&session_id, &workdir, error, json))?;
            let manifest_yaml =
                capsule_runtime::artifact::extract_manifest_yaml(name, version, &resolved.bytes)
                    .map_err(|error| fail(&session_id, &workdir, CliError::from(error), json))?;
            let manifest = murmur_artifact::RuntimeManifest::from_yaml_str(&manifest_yaml)
                .map_err(|err| {
                    fail(
                        &session_id,
                        &workdir,
                        runtime_manifest_error_to_cli(err),
                        json,
                    )
                })?;
            (manifest, Some(resolved.bytes))
        }
        None => {
            let manifest = load_runtime_manifest(&manifest_path).map_err(|err| {
                fail(
                    &session_id,
                    &workdir,
                    runtime_manifest_error_to_cli(err),
                    json,
                )
            })?;
            (manifest, None)
        }
    };

    // Warn if the manifest pins a different mur version than is currently running.
    // Do not abort — local development routinely runs ahead of a pinned version.
    if let Some(required) = &runtime_manifest.mur_version {
        let running = env!("CARGO_PKG_VERSION");
        if required != running {
            eprintln!(
                "warning: manifest requires mur {required} but you are running mur {running}"
            );
        }
    }

    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);

    // The containment floor is the strongest class any of the three sources asked for. This is
    // the only reason `mur run` reads a MurConfig at all — no other run behavior is configurable
    // from the workspace files.
    let cli_containment = parse_containment_flag(containment_arg, &session_id, &workdir, json)?;
    let workspace_containment = load_effective_mur_config_if_any_exists()
        .map_err(|error| fail(&session_id, &workdir, error, json))?
        .and_then(|config| config.containment);
    let declared_containment_floor = effective_containment_floor(
        workspace_containment,
        runtime_manifest
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.containment),
        cli_containment,
    );

    // A diagnostic, not an enforcement gate: it reports even when the floor is unmet and exits
    // 0. Placed ahead of every side effect — no PATH pre-flight, no registry, no workdir, no
    // staging — so it stays fast and read-only.
    if explain_scope_only {
        // Resolved from the manifest's own artifact entries, with `runtime_manifest.name` as the
        // store default — the same inputs, and the same resolver, `stage_session` uses, so the
        // report describes the launch rather than offering a second opinion about it. Resolution
        // creates nothing, so `--explain-scope` stays read-only; a malformed store name still
        // refuses here, exactly as it would refuse a real run.
        let state_stores = state_store_reports(
            runtime_manifest
                .artifacts
                .iter()
                .map(|artifact| (artifact.name.as_str(), artifact.capabilities.as_ref())),
            &runtime_manifest.name,
        )
        .map_err(|error| fail(&session_id, &workdir, CliError::from(error), json))?;
        // Same inputs and the same resolver `stage_session` uses, so a malformed `config:` block
        // refuses here exactly as it would refuse a real run: a diagnostic that describes a launch
        // must not tell an operator their manifest is fine and then refuse it.
        let configured_artifacts = configured_artifact_names(
            runtime_manifest
                .artifacts
                .iter()
                .map(|artifact| (artifact.name.as_str(), artifact.config.as_ref())),
        )
        .map_err(|error| fail(&session_id, &workdir, CliError::from(error), json))?;
        let report = explain_scope(
            &capability_policy,
            declared_containment_floor,
            runtime_manifest.exports.as_ref(),
            state_stores,
            configured_artifacts,
        );
        if json {
            let line = serde_json::to_string(&report).map_err(|source| {
                CliError::new(
                    E_IO_003,
                    format!("failed to serialize scope report: {source}"),
                )
            })?;
            println!("{line}");
        } else {
            print!("{}", report.render());
        }
        return Ok(());
    }

    // `--system-prompt` is applied here, to the `InferenceConfig` clone that becomes
    // `StageRequest.inference`, rather than inside the runtime's own prompt resolution: the tool
    // inventory and MURMUR.md's skill listing read `system_prompt_artifact` straight off this
    // same struct, so clearing the manifest's declaration here is what keeps every reader of it
    // agreeing with the override. Placed after the `--explain-scope` return above, which reports
    // capability grants and is unaffected by a system prompt.
    let (staged_inference, system_prompt_overridden) = apply_system_prompt_override(
        runtime_manifest.inference.clone(),
        system_prompt_arg,
        &session_id,
        &workdir,
        json,
    )?;

    // Pre-flight: for process transport, verify the CLI binary is on PATH before staging.
    if let Some(ref inference) = runtime_manifest.inference {
        if inference.transport == "process" {
            let command = inference.command.as_deref().unwrap_or("claude");
            if !is_on_path(command) {
                return Err(fail(
                    &session_id,
                    &workdir,
                    CliError::with_hint(
                        E_RUN_006,
                        format!("inference.command '{command}' not found on PATH"),
                        "install the claude CLI from https://claude.ai/download",
                    ),
                    json,
                ));
            }
        }
    }

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::with_capacity(runtime_manifest.artifacts.len());

    for artifact in &runtime_manifest.artifacts {
        match artifact.runtime {
            ArtifactRuntime::Tool => {
                allowlisted_tools.insert(artifact.name.clone());
            }
            ArtifactRuntime::Driver | ArtifactRuntime::Hook | ArtifactRuntime::Skill => {}
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let lock_path = project_dir.join("murmur.lock");
    // An installed capsule carries no project directory, so there is no lockfile to honour and
    // none to write: its artifact set is whatever the published manifest declares.
    let (staged_artifacts, lock_expectations, write_lock_after_stage) =
        if installed_capsule.is_some() {
            (requested_artifacts, None, false)
        } else {
            match read_lockfile(&lock_path) {
                Ok(lock) => {
                    let mut pinned_artifacts = Vec::with_capacity(requested_artifacts.len());
                    let mut expectations = Vec::with_capacity(requested_artifacts.len());
                    for artifact in &requested_artifacts {
                        // Local-source skills are not registry-resolved and not locked — pass
                        // them through untouched without requiring a lockfile entry.
                        if artifact.source.is_some() {
                            pinned_artifacts.push(artifact.clone());
                            continue;
                        }

                        let entry = lock.artifact_for(&artifact.name).ok_or_else(|| {
                            fail(
                                &session_id,
                                &workdir,
                                CliError::new(
                                    E_RUN_003,
                                    format!(
                                        "murmur.lock missing artifact entry for '{}'",
                                        artifact.name
                                    ),
                                ),
                                json,
                            )
                        })?;

                        pinned_artifacts.push(ArtifactRequest {
                            name: artifact.name.clone(),
                            version: entry.resolved_version.clone(),
                            runtime: artifact.runtime.clone(),
                            source: None,
                            on_overflow: artifact.on_overflow,
                            config: artifact.config.clone(),
                            capabilities: artifact.capabilities.clone(),
                        });
                        expectations.push(LockExpectation {
                            name: artifact.name.clone(),
                            resolved_version: entry.resolved_version.clone(),
                            sha256_wasm: entry.sha256.wasm.clone(),
                        });
                    }
                    (pinned_artifacts, Some(expectations), false)
                }
                Err(LockfileError::NotFound(_)) => (requested_artifacts, None, true),
                Err(error) => {
                    return Err(fail(
                        &session_id,
                        &workdir,
                        lockfile_error_to_cli(error),
                        json,
                    ));
                }
            }
        };

    let project_store_path = project_dir.join(".murmur").join("artifacts");
    let local_registry = LocalRegistry::new(&project_store_path);
    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    check_artifacts_installed(&local_registry, &global_registry, &staged_artifacts)
        .map_err(|error| fail(&session_id, &workdir, error, json))?;

    // Agent capsules are manifest-only — no WASM component to discover.
    // Script capsules require exactly one root *.wasm file in the project directory, or, in
    // `--capsule` mode, the root component of the artifact archive.
    let capsule_component_bytes = if runtime_manifest.inference.is_some() {
        Vec::new()
    } else if let Some((name, version)) = installed_capsule {
        let bytes = installed_bytes
            .as_deref()
            .expect("installed capsule mode always resolves artifact bytes");
        capsule_runtime::artifact::extract_root_wasm(name, version, bytes)
            .map_err(|error| fail(&session_id, &workdir, CliError::from(error), json))?
    } else {
        let capsule_path = discover_capsule_component(&project_dir)
            .map_err(|error| fail(&session_id, &workdir, error, json))?;

        fs::read(&capsule_path).map_err(|source| {
            fail(
                &session_id,
                &workdir,
                CliError::new(
                    E_IO_003,
                    format!(
                        "failed to read capsule component {}: {source}",
                        capsule_path.display()
                    ),
                ),
                json,
            )
        })?
    };

    // Parse lifecycle override from CLI flags
    let lifecycle_override = parse_lifecycle_override(
        lifecycle_task_acceptance,
        lifecycle_after_task,
        &session_id,
        &workdir,
        json,
    )?;

    let stage_request = StageRequest {
        manifest_dir: project_dir.clone(),
        capsule_name: runtime_manifest.name.clone(),
        capsule_version: runtime_manifest.version.clone(),
        capsule_component_bytes,
        artifacts: staged_artifacts,
        allowlisted_tools,
        lock_expectations,
        capability_policy,
        inference: staged_inference,
        system_prompt_overridden,
        context: runtime_manifest.context.clone(),
        context_id,
        resume: resume.map(|(request, _)| request),
        otel_endpoint: runtime_manifest
            .observability
            .as_ref()
            .and_then(|o| o.otel_endpoint.clone()),
        eval_config_json: runtime_manifest
            .observability
            .as_ref()
            .and_then(|o| o.eval.as_ref())
            .and_then(|e| serde_json::to_string(e).ok()),
        case_id: None,
        dataset_id: None,
        lifecycle: runtime_manifest.lifecycle.clone(),
        lifecycle_override,
        trace: runtime_manifest.trace,
        workdir: workdir_arg.as_deref().map(absolutise),
        bind_addr: bind_addr.to_string(),
        internal_port: runtime_manifest
            .network
            .as_ref()
            .and_then(|n| n.internal_port),
        declared_containment_floor,
        exports: runtime_manifest.exports.clone(),
        spawn_grant,
    };

    // Stage against project-then-global, the same order `check_artifacts_installed` just
    // pre-flighted. Handing staging only the project store made the two disagree: an artifact
    // published to the global store passed the check and then failed to stage, surfacing as
    // `E-REG-001 not found in registry` for something `mur list` could see.
    let staged = stage_session(
        Arc::new(FallbackRegistry {
            primary: local_registry,
            secondary: global_registry,
        }),
        stage_request,
    )
    .map_err(|error| fail(&session_id, &workdir, CliError::from(error), json))?;

    session_id = staged.session_id.clone();
    workdir = staged.workdir.clone();

    if let Some(task_value) = task_arg {
        // `staged.workdir` is the internal `.murmur/<session_id>` bookkeeping directory —
        // the agent's tools are preopened at `staged.accessible_workdir` (see
        // capsule-runtime's build_wasi_ctx calls), so task.md must land there or the
        // agent's own `read_file("task.md")` 404s even though the task was delivered.
        let dst = staged.accessible_workdir.join("task.md");
        write_input_to_workdir(task_value, &dst).map_err(|source| {
            fail(
                &session_id,
                &workdir,
                CliError::new(
                    E_IO_003,
                    format!("failed to write --task to workdir: {source}"),
                ),
                json,
            )
        })?;
    }

    if write_lock_after_stage {
        let lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: staged
                .resolved_lock_artifacts
                .iter()
                .map(|entry| LockedArtifact {
                    name: entry.name.clone(),
                    resolved_version: entry.resolved_version.clone(),
                    sha256: LockedSha256 {
                        wasm: entry.sha256_wasm.clone(),
                    },
                })
                .collect(),
        };

        write_lockfile_atomic(&lock_path, &lock)
            .map_err(|error| fail(&session_id, &workdir, lockfile_error_to_cli(error), json))?;
    }

    if json {
        let session_id_for_closure = session_id.clone();
        let capsule_name = runtime_manifest.name.clone();
        let capsule_version = runtime_manifest.version.clone();
        let pid = std::process::id();
        // Compute accessible_workdir for JSON output (mirrors stage_session logic).
        let accessible_workdir_for_json = match workdir_arg.as_deref() {
            Some(wd) => absolutise(wd),
            None => staged.workdir.clone(),
        };
        launch_session(staged, move |url| {
            println!(
                "{}",
                serde_json::json!({
                    "url": url,
                    "pid": pid,
                    "session_id": session_id_for_closure,
                    "name": capsule_name,
                    "version": capsule_version,
                    "workdir": accessible_workdir_for_json.to_string_lossy(),
                })
            );
        })
        .map(|_| ())
        .map_err(CliError::from)
    } else {
        // Capture fields for the startup closure before `staged` is consumed by launch_session.
        let session_id_for_startup = session_id.clone();
        let workdir_for_startup = workdir.clone();
        let skill_count = runtime_manifest
            .artifacts
            .iter()
            .filter(|a| a.runtime == ArtifactRuntime::Skill)
            .count();
        let manifest_name = runtime_manifest.name.clone();
        let manifest_version = runtime_manifest.version.clone();
        let driver_line = runtime_manifest.inference.as_ref().map(|inf| {
            if inf.transport == "process" {
                let cmd = inf.command.as_deref().unwrap_or("claude");
                format!("{cmd} process ({})", inf.model)
            } else {
                let artifact = inf
                    .driver
                    .as_ref()
                    .map(|d| d.artifact.as_str())
                    .unwrap_or("<driver>");
                format!("{artifact} ({})", inf.model)
            }
        });

        match launch_session(staged, move |url| {
            if !url.is_empty() {
                println!("murmur: url {url}");
            }
            println!("session: {session_id_for_startup}");
            if verbose {
                println!("workdir: {}", workdir_for_startup.display());
                println!("manifest: {manifest_name} v{manifest_version}");
                if let Some(ref d) = driver_line {
                    println!("driver: {d}");
                }
                if skill_count > 0 {
                    println!("skills: {skill_count} installed");
                }
                // TODO(formation): print formation_id here once formation IDs are assigned at mur run time
            }
        }) {
            Ok(launched) => {
                print_run_output(&launched.session_id, &launched.workdir, RunStatus::Success);
                Ok(())
            }
            Err(RuntimeError::CapsuleTrap(message)) => {
                let error = CliError::from(RuntimeError::CapsuleTrap(message));
                print_run_output(&session_id, &workdir, RunStatus::Trapped);
                Err(error)
            }
            Err(error) => Err(fail_run(&session_id, &workdir, CliError::from(error))),
        }
    }
}

/// Return true if `name` resolves to an executable on the current PATH.
fn is_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path_val| {
            std::env::split_paths(&path_val).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

/// Whether one declared artifact is available to a session on the current platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArtifactPresence {
    /// Declared with a local `source:` path — resolved from the filesystem at stage
    /// time, never from a registry, so there is nothing to check.
    LocalSource,
    /// Resolved from the project store or the global store. Carries the artifact that
    /// was resolved, so a caller that wants to inspect the bytes (e.g. `mur doctor`
    /// hashing them against `murmur.lock`) reads exactly the copy reported here rather
    /// than re-deriving the store order for itself.
    Installed(Box<ResolvedArtifact>),
    /// Resolved from neither store.
    Missing,
}

/// Resolve one artifact the way a session will: project store first, then global store,
/// both for the current platform.
pub(crate) fn artifact_presence(
    project_registry: &LocalRegistry,
    global_registry: &LocalRegistry,
    artifact: &ArtifactRequest,
    platform: &str,
) -> ArtifactPresence {
    if artifact.source.is_some() {
        return ArtifactPresence::LocalSource;
    }
    match project_registry
        .resolve_with_platform(&artifact.name, &artifact.version, Some(platform))
        .or_else(|_| {
            global_registry.resolve_with_platform(&artifact.name, &artifact.version, Some(platform))
        }) {
        Ok(resolved) => ArtifactPresence::Installed(Box::new(resolved)),
        Err(_) => ArtifactPresence::Missing,
    }
}

fn check_artifacts_installed(
    project_registry: &LocalRegistry,
    global_registry: &LocalRegistry,
    artifacts: &[ArtifactRequest],
) -> Result<(), CliError> {
    let platform = current_platform();
    let mut missing = Vec::new();

    for artifact in artifacts {
        if artifact_presence(project_registry, global_registry, artifact, platform)
            == ArtifactPresence::Missing
        {
            missing.push(format!("{}@{}", artifact.name, artifact.version));
        }
    }

    if missing.is_empty() {
        return Ok(());
    }

    Err(CliError::with_hint(
        E_RUN_008,
        format!("missing artifacts: {}", missing.join(", ")),
        "run `mur install` to install all manifest dependencies",
    ))
}

/// Validates `--containment` against the three class names, with the same shape as
/// `parse_lifecycle_override` below: a bad value is an `E-IO-003` naming the accepted set and
/// the offending input, not a bespoke error code for this one flag. `None` in means the flag was
/// not passed, which contributes nothing to the floor.
fn parse_containment_flag(
    value: Option<&str>,
    session_id: &str,
    workdir: &std::path::Path,
    json: bool,
) -> Result<Option<ContainmentClass>, CliError> {
    let Some(value) = value else {
        return Ok(None);
    };

    value
        .parse::<ContainmentClass>()
        .map(Some)
        .map_err(|error| {
            fail(
                session_id,
                workdir,
                CliError::new(E_IO_003, format!("--containment {error}")),
                json,
            )
        })
}

/// Applies `--system-prompt` to the manifest's inference config, returning the config to stage
/// and whether the override was in effect — the latter being the only thing left afterwards that
/// distinguishes an overridden prompt from a manifest that declared the same text inline, which
/// is what `session_start.system_prompt_source` reports.
///
/// The override replaces all three manifest declaration forms at once: `system_prompt_file` and
/// `system_prompt_artifact` are cleared rather than left to lose a precedence contest, so a file
/// that does not exist or an artifact that was never installed cannot fail the launch that was
/// explicitly told not to use it.
fn apply_system_prompt_override(
    inference: Option<InferenceConfig>,
    value: Option<&str>,
    session_id: &str,
    workdir: &std::path::Path,
    json: bool,
) -> Result<(Option<InferenceConfig>, bool), CliError> {
    let Some(value) = value else {
        return Ok((inference, false));
    };

    let Some(mut inference) = inference else {
        return Err(fail(
            session_id,
            workdir,
            CliError::with_hint(
                E_IO_003,
                "--system-prompt requires an agent capsule: this manifest has no inference: block",
                "add an inference: block to murmur.yaml, or drop --system-prompt",
            ),
            json,
        ));
    };

    // Trimmed on the same terms `optional_trimmed_string` applies to `inference.system_prompt` at
    // manifest parse time, so `--system-prompt "$(cat prompt.md)"` and the inline manifest form
    // reach the model as the same bytes. An all-whitespace value therefore clears the prompt —
    // still an override, just one that resolves to nothing.
    let trimmed = value.trim();
    inference.system_prompt = (!trimmed.is_empty()).then(|| trimmed.to_string());
    inference.system_prompt_file = None;
    inference.system_prompt_artifact = None;

    Ok((Some(inference), true))
}

fn parse_lifecycle_override(
    task_acceptance: Option<&str>,
    after_task: Option<&str>,
    session_id: &str,
    workdir: &std::path::Path,
    json: bool,
) -> Result<Option<LifecycleOverride>, CliError> {
    if task_acceptance.is_none() && after_task.is_none() {
        return Ok(None);
    }

    let parsed_ta = match task_acceptance {
        None => None,
        Some("none") => Some(TaskAcceptance::None),
        Some("single") => Some(TaskAcceptance::Single),
        Some("queue") => Some(TaskAcceptance::Queue),
        Some(other) => {
            return Err(fail(
                session_id,
                workdir,
                CliError::new(
                    E_IO_003,
                    format!(
                        "--lifecycle-task-acceptance must be one of: none, single, queue; got '{other}'"
                    ),
                ),
                json,
            ));
        }
    };

    let parsed_at = match after_task {
        None => None,
        Some("exit") => Some(AfterTask::Exit),
        Some("sleep") => Some(AfterTask::Sleep),
        Some(other) => {
            return Err(fail(
                session_id,
                workdir,
                CliError::new(
                    E_IO_003,
                    format!("--lifecycle-after-task must be one of: exit, sleep; got '{other}'"),
                ),
                json,
            ));
        }
    };

    Ok(Some(LifecycleOverride {
        task_acceptance: parsed_ta,
        after_task: parsed_at,
    }))
}

fn find_workspace_root(start: &Path) -> PathBuf {
    let mut current = start.to_path_buf();
    loop {
        if current.join("murmur.yaml").exists() {
            return current;
        }

        let Some(parent) = current.parent() else {
            return start.to_path_buf();
        };

        current = parent.to_path_buf();
    }
}

/// One line of standard input, as this launch's spawn approval.
///
/// The grant travels on a pipe rather than on the argument vector or in the environment: both of
/// those are readable through `/proc/<pid>` by any process running as the same user, which is
/// exactly what a sibling capsule's shell tool is.
fn read_spawn_grant() -> Result<capsule_runtime::SpawnApproval, CliError> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!("failed to read the spawn grant from standard input: {source}"),
        )
    })?;
    let line = line.trim();
    if line.is_empty() {
        return Err(CliError::with_hint(
            E_RUN_019,
            "--spawn-grant-stdin was given but standard input carried no grant",
            "the launching runtime writes one line and closes the pipe; run without \
             --spawn-grant-stdin to launch this capsule directly",
        ));
    }
    Ok(capsule_runtime::SpawnApproval::new(line.to_string()))
}

/// Resolve an installed capsule from the project store, then the global one.
///
/// The same order `check_artifacts_installed` and staging use for every other artifact, so a
/// capsule published to either store runs by name. Through [`FallbackRegistry`], so that only a
/// *missing* artifact falls through to the global store: an artifact the project store holds but
/// cannot read is that error, not a lookup somewhere else.
fn resolve_installed_capsule(
    project_dir: &Path,
    name: &str,
    version: &str,
) -> Result<ResolvedArtifact, CliError> {
    FallbackRegistry {
        primary: LocalRegistry::new(project_dir.join(".murmur").join("artifacts")),
        secondary: LocalRegistry::from_default_home().map_err(CliError::from)?,
    }
    .resolve_with_platform(name, version, Some(current_platform()))
    .map_err(|source| {
        CliError::with_hint(
            E_RUN_008,
            format!("capsule '{name}@{version}' is not installed: {source}"),
            "publish or install it first — `mur install <artifact>.mur.zip` — then run \
             `mur list` to confirm the coordinate",
        )
    })
}

fn discover_capsule_component(project_dir: &Path) -> Result<PathBuf, CliError> {
    let preferred = project_dir.join("capsule.wasm");
    if preferred.exists() {
        return Ok(preferred);
    }

    let mut candidates = Vec::new();
    for entry in fs::read_dir(project_dir).map_err(|source| {
        CliError::new(
            E_IO_003,
            format!(
                "failed to read project directory {}: {source}",
                project_dir.display()
            ),
        )
    })? {
        let entry = entry.map_err(CliError::from)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("wasm") {
            candidates.push(path);
        }
    }

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(CliError::new(
            E_RUN_004,
            format!(
                "no capsule component found in {} (expected capsule.wasm or exactly one root *.wasm)",
                project_dir.display()
            ),
        )),
        _ => {
            candidates.sort();
            let display = candidates
                .iter()
                .map(|path| {
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("<unknown>")
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(CliError::new(
                E_RUN_004,
                format!(
                    "multiple root *.wasm files found in {}: {} (set capsule.wasm explicitly)",
                    project_dir.display(),
                    display
                ),
            ))
        }
    }
}

/// Copy `value` as a file if it names an existing path, otherwise write it
/// as UTF-8 text.  This is the implementation of `--input` auto-detection.
fn write_input_to_workdir(value: &str, dst: &Path) -> std::io::Result<()> {
    if Path::new(value).exists() {
        fs::copy(value, dst).map(|_| ())
    } else {
        fs::write(dst, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_input_file_path_copies_contents() {
        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();

        let src = src_dir.path().join("task.md");
        fs::write(&src, "hello from file").unwrap();

        let dst = dst_dir.path().join("task.md");
        write_input_to_workdir(src.to_str().unwrap(), &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "hello from file");
    }

    #[test]
    fn write_input_inline_text_written_verbatim() {
        let dst_dir = tempdir().unwrap();
        let dst = dst_dir.path().join("task.md");

        write_input_to_workdir("Run: echo hello", &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "Run: echo hello");
    }

    #[test]
    fn write_input_empty_string_creates_empty_file() {
        let dst_dir = tempdir().unwrap();
        let dst = dst_dir.path().join("task.md");

        write_input_to_workdir("", &dst).unwrap();

        assert_eq!(fs::read_to_string(&dst).unwrap(), "");
    }
}
