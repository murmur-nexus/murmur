use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    artifact_config::ARTIFACT_CONFIG_ENV,
    detached::{DetachPolicy, DetachedDispatchInfo},
    types::CapabilityPolicy,
};

const MAX_SHELL_OUTPUT_BYTES: usize = 16 * 1024;

const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh"];
const DEFAULT_ENV_BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "TERM",
];

/// Directory name for the per-session synthetic home, created under the capsule's
/// session workdir. Planned /proc and subprocess key isolation work
/// depend on this exact name/location convention.
///
/// `pub(crate)` for one reader outside this file: `sandbox::ensure_sealed_identity_files`, which
/// writes the `pw_dir` field of a sealed capsule's synthetic `/etc/passwd` entry. That field and
/// `$HOME` must be the same string — a second hand-copied `".capsule-home"` there is exactly how
/// they would stop being one.
pub(crate) const SYNTHETIC_HOME_DIR_NAME: &str = ".capsule-home";

/// Credential-shaped env var patterns stripped from every subprocess, regardless of
/// whether they came from the host process env, `env_overrides`, or
/// `policy.shell_baseline_env`. Supports exact match, trailing `*` (prefix) and
/// leading `*` (suffix) via `env_name_matches_pattern`.
const CREDENTIAL_ENV_PATTERNS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "HUGGING_FACE_HUB_TOKEN",
    "NEXUS_API_KEY",
    "AWS_*",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "DOCKER_*",
    "KUBECONFIG",
    "NPM_TOKEN",
    "PYPI_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "*_API_KEY",
];

#[derive(Debug)]
pub(crate) struct ShellResult {
    /// The program that was actually invoked, resolved to its canonical absolute path
    /// when the host `PATH` names it, else the bare invoked name — see
    /// [`crate::sandbox::resolve_invoked_binary_path`]. Carried out of here because the
    /// invoked name is otherwise lost the moment dispatch returns, leaving every
    /// downstream observer (`shell-event.binary`, `trace.jsonl`) unable to say what ran.
    pub(crate) binary: String,
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) duration_ms: u64,
    pub(crate) truncated: bool,
    pub(crate) full_output_path: Option<String>,
    /// The `capabilities.resources` field this subprocess was killed for exceeding, when the
    /// evidence names exactly one — see [`classify_resource_limit`]. `None` covers both "ran
    /// normally" and "died for a reason no single limit can be pinned to", which are
    /// deliberately not distinguished here: inventing an attribution is worse than declining one.
    pub(crate) resource_limit_hit: Option<String>,
}

/// Why [`execute_shell`] produced no result.
///
/// Two kinds, and the distinction is the whole point of the type: an ordinary failure is the
/// capsule's business (it sees a failed tool call and can decide what to do next), while a
/// [`Self::SealedRootConstructionFailed`] is the *session's* business — the containment boundary
/// the session declared stopped being establishable, so there is nothing sensible for the capsule
/// to retry and the run has to end with that named as the cause. Before this type existed both
/// collapsed into a `String` and the second was indistinguishable from the first by the time it
/// reached a caller that could act on it.
#[derive(Debug)]
pub(crate) enum ShellExecError {
    /// Binary not in `capabilities.shell.allow`, env construction failure, workdir budget
    /// breach, enforcement setup failure, spawn failure — everything that leaves the rest of
    /// the session viable.
    Failed(String),
    /// A `sealed` session's composed root could not be built for this subprocess, *after* the
    /// pre-launch probe reported the mechanism available. `detail` is what the child wrote to
    /// the `pre_exec` diagnostic pipe: the exact step and its errno.
    SealedRootConstructionFailed { detail: String },
}

impl ShellExecError {
    /// The typed, session-fatal error this failure represents, or `None` when the failure is a
    /// tool-level one the capsule can be told about and carry on from.
    ///
    /// Callers that own the session (the agent turn loop) return this; callers that only own a
    /// single dispatch pass it upward alongside the tool result rather than deciding themselves.
    pub(crate) fn session_fatal(&self) -> Option<crate::errors::RuntimeError> {
        match self {
            Self::Failed(_) => None,
            Self::SealedRootConstructionFailed { detail } => {
                Some(crate::errors::RuntimeError::SealedRootConstructionFailed {
                    detail: detail.clone(),
                })
            }
        }
    }
}

impl std::fmt::Display for ShellExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(message) => f.write_str(message),
            // Delegate to the `RuntimeError` Display text so the message an operator reads cannot
            // drift from the one `E-RUN-014` renders — the same reason the CLI's error mapping
            // delegates rather than restating.
            Self::SealedRootConstructionFailed { detail } => write!(
                f,
                "{}",
                crate::errors::RuntimeError::SealedRootConstructionFailed {
                    detail: detail.clone(),
                }
            ),
        }
    }
}

/// Lets the many `Result<_, String>` helpers `execute_shell` calls keep flowing through `?`.
impl From<String> for ShellExecError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

pub(crate) fn is_shell_interpreter(binary: &str) -> bool {
    SHELL_INTERPRETERS.contains(&binary)
}

pub(crate) fn split_shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match (ch, quote) {
            ('"' | '\'', None) => quote = Some(ch),
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

pub(crate) fn shell_tool_manifest_yaml(binary: &str) -> String {
    let description = if is_shell_interpreter(binary) {
        format!("Run a shell command via {binary} in the capsule workdir. The `command` field is passed to {binary} via -c.")
    } else {
        format!("Run {binary} in the capsule workdir. The `command` field is the argument list — omit the binary name itself (pass -s http://example.com, not {binary} -s http://example.com).")
    };

    format!(
        "name: {binary}\nversion: 0.0.0\nruntime: tool\nimplementation: native\ndescription: \"{description}\"\ninput_schema: '{{\"type\":\"object\",\"properties\":{{\"command\":{{\"type\":\"string\"}}}},\"required\":[\"command\"]}}'\n"
    )
}

/// Whether a shell command finished inside its grace period or was demoted to the background.
#[derive(Debug)]
pub(crate) enum ShellOutcome {
    Finished(ShellResult),
    Detached(DetachedDispatchInfo),
}

/// How often the grace-period wait checks on a child it cannot block for.
///
/// `wait_with_output` has no pollable form, so a command that may be demoted is waited for by
/// polling. Short enough that a command finishing just inside the grace period is not held past
/// it by a noticeable margin, long enough that a one-minute grace costs a few thousand
/// non-blocking syscalls rather than a busy loop.
const DETACH_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run a shell command, demoting it to the background if it outruns `detach`'s grace period.
///
/// With `detach: None` the child is waited for with a plain blocking wait and nothing is polled —
/// the path every call took before demotion existed, and the path `plan.rs` and the
/// script-capsule store state still take, because neither runs a task loop and so neither has
/// anywhere to deliver a completion.
///
/// With `detach: Some`, the child is polled until it exits or the deadline passes. On the
/// deadline the child, its two pipe readers, its own egress-proxy supervisor and the metadata
/// needed to describe it all move to one plain OS thread — not a `spawn_blocking` task — so
/// runtime teardown never waits on it and a session can exit while it runs.
///
/// Everything before the spawn is identical on both paths, including the whole spawn-failure
/// arm: a spawn failure happens inside the grace window and stays a foreground error.
pub(crate) fn run_shell(
    binary: &str,
    args: &[&str],
    env_overrides: &[(String, String)],
    workdir: &Path,
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
    detach: Option<DetachPolicy>,
) -> Result<ShellOutcome, ShellExecError> {
    if !policy.shell_allow.iter().any(|allowed| allowed == binary) {
        return Err(ShellExecError::Failed(format!(
            "binary '{binary}' is not in capabilities.shell.allow"
        )));
    }

    // Refuse before spawning, not after: once the workdir ceiling is crossed, the cheapest way
    // to stop it growing further is to not start the next process that would write to it.
    enforcement.check_workdir_budget()?;

    let env = build_shell_env(policy, env_overrides, workdir)?;

    // Resolve before spawning: this is the identity of the binary this call is about to
    // run, and it is the only point where the invoked name is still in hand.
    let resolved_binary = crate::sandbox::resolve_invoked_binary_path(binary);

    let started = Instant::now();
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(workdir)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Fail-closed: if kernel enforcement setup fails unexpectedly, propagate the error and
    // never call `.spawn()` at all — no code path here lets a Linux host silently run this
    // subprocess with zero enforcement because setup failed. This also installs the hard
    // rlimits and (where a scope exists) cgroup membership for the child.
    let supervisor = crate::sandbox::prepare_enforcement(&mut command, enforcement, workdir)?;

    // Snapshotted before the child runs so attribution keys on this call's delta rather than on
    // a session-cumulative total an earlier call could have moved.
    let cgroup_counters_before = enforcement
        .cgroup_scope
        .as_ref()
        .map(|scope| scope.event_counters());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            // A failure inside the `pre_exec` enforcement setup reaches us here only as a bare
            // errno (std collapses a `pre_exec` `io::Error` to its `raw_os_error()`, defaulting to
            // EINVAL). Drop `command` first so the parent's captured copy of the diagnostic pipe's
            // write end closes (letting the read below see EOF), then fold any legible detail the
            // child wrote into the returned error instead of the undifferentiated
            // "Invalid argument (os error 22)".
            drop(command);
            return Err(match supervisor.read_diagnostic() {
                // A composed-root failure keeps its own variant all the way out of here rather
                // than being flattened into a message: it means a host that *did* clear the
                // pre-launch sealed probe then failed to build the root, which is a different
                // event from a Landlock or seccomp step failing, points somewhere else, and ends
                // the session instead of the tool call. See [`ShellExecError::session_fatal`].
                Some(detail) if detail.contains(crate::sealed::SEALED_ROOT_FAILURE_PREFIX) => {
                    ShellExecError::SealedRootConstructionFailed { detail }
                }
                Some(detail) if !detail.is_empty() => ShellExecError::Failed(format!(
                    "sandbox: shell enforcement setup failed before exec: {detail}"
                )),
                _ => ShellExecError::Failed(error.to_string()),
            });
        }
    };

    // `wait_with_output` reads both pipes and waits in one uninterruptible step, so it cannot be
    // given a deadline. Draining each pipe on its own thread separates reading from waiting,
    // which is what lets the wait below be either blocking or polled.
    let readers = OutputReaders::spawn(&mut child);

    let status = match &detach {
        None => child.wait().map_err(|error| error.to_string())?,
        Some(policy) => {
            let deadline = started + policy.grace;
            loop {
                match child.try_wait().map_err(|error| error.to_string())? {
                    Some(status) => break status,
                    None => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            // A zero grace demotes here, at the first poll after the spawn.
                            return Ok(ShellOutcome::Detached(demote(
                                child,
                                readers,
                                supervisor,
                                DemotionContext {
                                    policy: detach.expect("the detach policy was matched above"),
                                    workdir: workdir.to_path_buf(),
                                    invoked_binary: binary.to_string(),
                                    resolved_binary,
                                    args: args.iter().map(|arg| (*arg).to_string()).collect(),
                                    started,
                                    cgroup_scope: enforcement.cgroup_scope.clone(),
                                    cgroup_counters_before,
                                },
                            )));
                        }
                        std::thread::sleep(remaining.min(DETACH_POLL_INTERVAL));
                    }
                }
            }
        }
    };
    let (stdout_bytes, stderr_bytes) = readers.join();
    supervisor.join_best_effort();
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    let resource_limit_hit = classify_resource_limit(
        &status,
        enforcement.cgroup_scope.as_deref(),
        cgroup_counters_before,
    );
    let exit_code = exit_code_of(&status);

    let mut stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    let mut truncated = false;
    let mut full_output_path = None;

    if stdout_bytes.len() > MAX_SHELL_OUTPUT_BYTES || stderr_bytes.len() > MAX_SHELL_OUTPUT_BYTES {
        truncated = true;
        let stdout_cut = stdout_bytes.len().min(MAX_SHELL_OUTPUT_BYTES);
        let stderr_cut = stderr_bytes.len().min(MAX_SHELL_OUTPUT_BYTES);
        let full_stdout = std::mem::replace(
            &mut stdout,
            String::from_utf8_lossy(&stdout_bytes[..stdout_cut]).to_string(),
        );
        let full_stderr = std::mem::replace(
            &mut stderr,
            String::from_utf8_lossy(&stderr_bytes[..stderr_cut]).to_string(),
        );
        full_output_path = Some(write_shell_output_log(
            workdir,
            &format!("shell-{}", unix_millis()),
            binary,
            args,
            exit_code,
            &full_stdout,
            &full_stderr,
        )?);
    }

    Ok(ShellOutcome::Finished(ShellResult {
        binary: resolved_binary,
        exit_code,
        stdout,
        stderr,
        duration_ms,
        truncated,
        full_output_path,
        resource_limit_hit,
    }))
}

/// [`run_shell`] with no demotion: the command runs to completion in the foreground.
///
/// Kept at its original signature and return type because most callers cannot take a completion
/// and every one of them should keep reading exactly as it did.
pub(crate) fn execute_shell(
    binary: &str,
    args: &[&str],
    env_overrides: &[(String, String)],
    workdir: &Path,
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
) -> Result<ShellResult, ShellExecError> {
    match run_shell(
        binary,
        args,
        env_overrides,
        workdir,
        policy,
        enforcement,
        None,
    )? {
        ShellOutcome::Finished(result) => Ok(result),
        // Unreachable: demotion is reached only from the `Some(policy)` wait arm above. Reported
        // rather than panicked, so a future edit that broke that invariant would fail one tool
        // call instead of taking the session down.
        ShellOutcome::Detached(info) => Err(ShellExecError::Failed(format!(
            "shell command was demoted to the background as {} on a call site that cannot receive a completion",
            info.work_id
        ))),
    }
}

/// The two pipe-draining threads for one child.
///
/// Both must be running before the child is waited for: a child writing more than a pipe buffer
/// holds while nobody reads deadlocks, which is the reason `wait_with_output` reads and waits
/// together in the first place.
struct OutputReaders {
    stdout: Option<std::thread::JoinHandle<Vec<u8>>>,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
}

impl OutputReaders {
    fn spawn(child: &mut std::process::Child) -> Self {
        fn drain(
            pipe: Option<impl std::io::Read + Send + 'static>,
        ) -> Option<std::thread::JoinHandle<Vec<u8>>> {
            let mut pipe = pipe?;
            Some(std::thread::spawn(move || {
                let mut buffer = Vec::new();
                // A read error loses whatever was not yet read and keeps what was; there is no
                // better outcome available, and failing the whole call over it would throw away
                // the command's exit status too.
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            }))
        }

        Self {
            stdout: drain(child.stdout.take()),
            stderr: drain(child.stderr.take()),
        }
    }

    /// `(stdout, stderr)`, waiting for both pipes to reach EOF. A reader thread that panicked
    /// yields empty bytes rather than propagating.
    fn join(self) -> (Vec<u8>, Vec<u8>) {
        let take = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
            handle
                .map(|handle| handle.join().unwrap_or_default())
                .unwrap_or_default()
        };
        (take(self.stdout), take(self.stderr))
    }
}

/// Everything the background thread needs to describe the command it is finishing.
struct DemotionContext {
    policy: DetachPolicy,
    workdir: PathBuf,
    /// The name as invoked, which is what the log header records.
    invoked_binary: String,
    /// The same name resolved to a path where the host `PATH` named one — what the trace and the
    /// completion report, matching [`ShellResult::binary`].
    resolved_binary: String,
    args: Vec<String>,
    started: Instant,
    cgroup_scope: Option<std::sync::Arc<crate::cgroup::CgroupScope>>,
    cgroup_counters_before: Option<crate::cgroup::CgroupEventCounters>,
}

/// Hand a still-running command to a thread of its own and return the handle for it.
///
/// The work is registered before this returns, so the session-end sweep can never miss work that
/// finished being registered after it looked.
fn demote(
    mut child: std::process::Child,
    readers: OutputReaders,
    supervisor: crate::sandbox::SupervisorHandle,
    context: DemotionContext,
) -> DetachedDispatchInfo {
    let work_id = crate::detached::new_work_id();
    let info = DetachedDispatchInfo {
        work_id: work_id.clone(),
        binary: context.resolved_binary.clone(),
        command: context.policy.command.clone(),
        grace_ms: context
            .policy
            .grace
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
    };

    context
        .policy
        .registry
        .register(crate::detached::DetachedWork {
            work_id: work_id.clone(),
            binary: context.resolved_binary.clone(),
            command: context.policy.command.clone(),
            started_at_ms: unix_millis(),
        });

    let provenance = context.policy.completion_provenance();
    let DemotionContext {
        policy,
        workdir,
        invoked_binary,
        resolved_binary,
        args,
        started,
        cgroup_scope,
        cgroup_counters_before,
    } = context;

    std::thread::spawn(move || {
        let waited = child.wait();
        let (stdout_bytes, stderr_bytes) = readers.join();
        // This command's own egress proxy, torn down with the command it served. Each
        // `prepare_enforcement` call starts its own, so a foreground command finishing in the
        // meantime cannot have taken this one down.
        supervisor.join_best_effort();
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

        let (exit_code, resource_limit, mut error) = match waited {
            Ok(status) => (
                exit_code_of(&status),
                classify_resource_limit(&status, cgroup_scope.as_deref(), cgroup_counters_before),
                None,
            ),
            Err(failure) => (
                -1,
                None,
                Some(format!("waiting for the command failed: {failure}")),
            ),
        };

        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let output_path = crate::detached::output_path_for(&work_id);
        if let Err(failure) = write_shell_output_log(
            &workdir,
            &work_id,
            &invoked_binary,
            &arg_refs,
            exit_code,
            &stdout,
            &stderr,
        ) {
            error = Some(match error {
                Some(existing) => format!("{existing}; {failure}"),
                None => failure,
            });
        }
        let output_bytes = std::fs::metadata(workdir.join(&output_path))
            .map(|metadata| metadata.len())
            .unwrap_or(0);

        policy
            .registry
            .complete(crate::detached::DetachedCompletion {
                work_id,
                binary: resolved_binary,
                command: policy.command.clone(),
                exit_code,
                duration_ms,
                output_path,
                output_bytes,
                resource_limit,
                context_id: policy.context_id.clone(),
                provenance,
                error,
            });
    });

    info
}

/// Milliseconds since the Unix epoch, for the two ids built from wall-clock time: a truncation
/// log's `shell-<ms>` stem and a demoted command's `started_at_ms`.
fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Exit code for a finished subprocess, keeping the signal that killed it legible.
///
/// `ExitStatus::code()` returns `None` for every signal-killed process on Unix, so a bare
/// `.unwrap_or(-1)` collapses a `SIGXCPU` from `RLIMIT_CPU`, a cgroup OOM `SIGKILL` and an
/// unrelated crash into one indistinguishable `-1`. `128 + signal` is the long-standing shell
/// convention for exactly this case and keeps the cause readable in the trace even where
/// [`classify_resource_limit`] declines to name a limit.
fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(-1)
}

/// Name the `capabilities.resources` field a subprocess was killed for exceeding, or `None`.
///
/// Two independent sources of evidence, both kernel-maintained facts rather than inference:
///
///   * the kill signal, for the two rlimits that raise a signal unique to themselves
///     (`SIGXCPU` → `cpu_seconds`, `SIGXFSZ` → `max_file_size_bytes`);
///   * this call's delta on the cgroup scope's `memory.events`/`pids.events` counters
///     (`oom_kill` → `cgroup_memory_bytes`, `max` → `cgroup_pids_max`).
///
/// The signal is checked first: where both fire, it identifies the individual process that died,
/// which is the more specific claim.
///
/// The `pids.max` case is reported even when the process exited normally, because that is how it
/// presents — `pids.max` refuses a `fork()` rather than killing anything, so a fork bomb held by
/// the cgroup usually shows up as a shell exiting non-zero with `EAGAIN` on stderr. Without the
/// counter there would be nothing distinguishing it from any other failure.
///
/// Nothing else is attributed. See [`crate::resources::limit_from_signal`] for why
/// `RLIMIT_AS`/`RLIMIT_DATA`, `RLIMIT_NPROC` and `RLIMIT_NOFILE` are deliberately left
/// unattributed rather than guessed at.
fn classify_resource_limit(
    status: &std::process::ExitStatus,
    cgroup_scope: Option<&crate::cgroup::CgroupScope>,
    counters_before: Option<crate::cgroup::CgroupEventCounters>,
) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    if let Some(limit) = status
        .signal()
        .and_then(crate::resources::limit_from_signal)
    {
        return Some(limit.to_string());
    }

    let (scope, before) = (cgroup_scope?, counters_before?);
    scope
        .event_counters()
        .attribution_since(before)
        .map(str::to_string)
}

pub(crate) fn build_shell_env(
    policy: &CapabilityPolicy,
    env_overrides: &[(String, String)],
    workdir: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();

    for key in DEFAULT_ENV_BASELINE {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }

    for (key, value) in env_overrides {
        env.insert(key.clone(), value.clone());
    }

    for key in &policy.shell_baseline_env {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    // Inserted last so neither a guest-supplied `env_overrides` entry nor a
    // manifest-declared `shell_baseline_env` entry can resurrect the real host
    // HOME/USERPROFILE, and so no strip_env pattern can remove the synthetic one.
    let synthetic_home = synthetic_home_dir(workdir)?;
    let synthetic_home = synthetic_home.to_string_lossy().into_owned();
    env.insert("HOME".to_string(), synthetic_home.clone());
    env.insert("USERPROFILE".to_string(), synthetic_home);

    Ok(env)
}

/// Drop every entry whose name matches [`CREDENTIAL_ENV_PATTERNS`] or one of
/// `extra_patterns` (a policy's `shell_strip_env`).
///
/// This is the single credential backstop for both environments the runtime builds: the
/// native subprocess env ([`build_shell_env`]) and the WASI guest env
/// ([`build_wasi_env_allowlist`]). It runs *after* whatever allowlist populated `env`, so a
/// declared name never bypasses the filter.
pub(crate) fn strip_credential_shaped_vars(
    env: &mut BTreeMap<String, String>,
    extra_patterns: &[String],
) {
    env.retain(|key, _| {
        !CREDENTIAL_ENV_PATTERNS
            .iter()
            .copied()
            .chain(extra_patterns.iter().map(String::as_str))
            .any(|pattern| env_name_matches_pattern(pattern, key))
    });
}

/// Resolve the host variables a WASM guest may observe: only names the manifest declared in
/// `capabilities.env.allow` and that the host actually has set, minus anything
/// credential-shaped. A declared name absent from the host is simply omitted, not an error.
///
/// [`ARTIFACT_CONFIG_ENV`] is reserved and never resolved from the host, whatever a manifest
/// allowlists: the name is runtime-owned, and its value comes from the declaring artifact's own
/// `config:` block or from nowhere. Skipped here rather than relied on being overwritten later,
/// so a host value cannot reach a guest whose entry declared no config at all.
pub(crate) fn build_wasi_env_allowlist(policy: &CapabilityPolicy) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    for key in &policy.env_allow {
        if key == ARTIFACT_CONFIG_ENV {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    env
}

fn synthetic_home_dir(workdir: &Path) -> Result<std::path::PathBuf, String> {
    let home = workdir.join(SYNTHETIC_HOME_DIR_NAME);
    fs::create_dir_all(&home).map_err(|error| {
        format!(
            "failed to create synthetic home directory {}: {error}",
            home.display()
        )
    })?;
    Ok(home)
}

fn env_name_matches_pattern(pattern: &str, key: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix('*') {
        if let Some(middle) = rest.strip_suffix('*') {
            return key.contains(middle);
        }
        return key.ends_with(rest);
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        return key.starts_with(prefix);
    }

    pattern == key
}

/// Write one command's full stdout and stderr to `logs/<file_stem>.log`, returning the
/// workdir-relative path.
///
/// Two callers, one content format: a foreground command whose output exceeded
/// [`MAX_SHELL_OUTPUT_BYTES`] passes a `shell-<ms>` stem, and a demoted command passes its work
/// id. A reader that finds either file reads the same header, `Stdout:` and `Stderr:` sections.
fn write_shell_output_log(
    workdir: &Path,
    file_stem: &str,
    binary: &str,
    args: &[&str],
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<String, String> {
    let logs_dir = workdir.join("logs");
    fs::create_dir_all(&logs_dir)
        .map_err(|error| format!("failed to create shell logs directory: {error}"))?;

    let filename = format!("{file_stem}.log");
    let relative_path = format!("logs/{filename}");
    let path = logs_dir.join(&filename);

    let command = if args.is_empty() {
        binary.to_string()
    } else {
        format!("{binary} {}", args.join(" "))
    };

    let content = format!(
        "Command: {command}\nExit code: {exit_code}\n\nStdout:\n{stdout}\n\nStderr:\n{stderr}\n"
    );

    fs::write(&path, content)
        .map_err(|error| format!("failed to write shell log {}: {error}", path.display()))?;

    Ok(relative_path)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::sandbox::ShellEnforcement;

    /// The `pre_exec` rlimit plumbing, end to end on whatever platform runs the suite: a
    /// declared `capabilities.resources.max_open_files` must reach the child as its **hard**
    /// ceiling, which is what `ulimit -Hn` reports. This asserts the mechanism (the value was
    /// applied to `rlim_max`, not only `rlim_cur`), not a security outcome — the hostile-capsule
    /// scenarios require real Linux hardware.
    #[test]
    fn execute_shell_applies_the_declared_limit_as_a_hard_ceiling() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            resources: crate::resources::HostResourceLimits {
                max_open_files: 64,
                ..crate::resources::HostResourceLimits::default()
            },
            ..CapabilityPolicy::default()
        };
        let mut enforcement = ShellEnforcement::environment_only();
        enforcement.resource_limits = policy.resources;

        let result = execute_shell(
            "bash",
            &["-c", "ulimit -Hn"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("bash must run");

        assert_eq!(result.stdout.trim(), "64", "stderr was: {}", result.stderr);
        assert_eq!(result.resource_limit_hit, None);
    }

    /// A subprocess that exits normally must never be reported as limit-killed — the negative
    /// half of attribution, and the one that keeps `resource_limit_hit` meaningful in the trace.
    #[test]
    fn execute_shell_reports_no_resource_limit_for_an_ordinary_exit() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "exit 3"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .expect("bash must run");

        assert_eq!(result.exit_code, 3);
        assert_eq!(result.resource_limit_hit, None);
    }

    #[test]
    fn execute_shell_blocks_binary_not_in_allowlist() {
        let policy = CapabilityPolicy::default();
        let error = execute_shell(
            "bash",
            &["-c", "echo hi"],
            &[],
            Path::new("."),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("capabilities.shell.allow"));
    }

    #[test]
    fn execute_shell_returns_nonzero_exit_code_without_error() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "exit 42"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 42);
    }

    /// A real subprocess run through the real `execute_shell`: the returned `binary` is the
    /// canonical absolute path of the interpreter that ran, not the bare invoked name — the
    /// value that reaches `shell-event.binary` and `trace.jsonl`'s `binary` key.
    #[test]
    fn execute_shell_reports_the_resolved_path_of_the_invoked_binary() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "exit 0"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();

        assert!(
            Path::new(&result.binary).is_absolute(),
            "a binary found on PATH must be reported as an absolute path, got {:?}",
            result.binary
        );
        assert!(
            result.binary.ends_with("bash"),
            "the reported path must still name bash, got {:?}",
            result.binary
        );
    }

    /// The fallback, exercised end-to-end rather than only against the resolver: a workdir
    /// -relative program spawns fine (the child resolves it after `chdir`) but has no host
    /// `PATH` entry to canonicalize against, so `binary` degrades to the invoked name
    /// unchanged. Nothing else about the call changes — it still runs and still reports its
    /// exit code.
    #[test]
    fn execute_shell_falls_back_to_the_bare_name_when_nothing_resolves() {
        let temp = tempdir().unwrap();
        let tool = temp.path().join("local-tool");
        fs::write(&tool, "#!/bin/sh\nexit 7\n").unwrap();
        let mut perms = fs::metadata(&tool).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&tool, perms).unwrap();

        let policy = CapabilityPolicy {
            shell_allow: vec!["./local-tool".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "./local-tool",
            &[],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();

        assert_eq!(result.exit_code, 7, "the subprocess must still have run");
        assert_eq!(
            result.binary, "./local-tool",
            "an unresolvable name degrades to the bare invoked name, never an error"
        );
    }

    #[test]
    fn build_shell_env_sets_synthetic_home_under_workdir() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        let env = build_shell_env(&policy, &[], temp.path()).unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(
            env.get("HOME"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("USERPROFILE"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
        assert!(expected_home.is_dir());
    }

    #[test]
    fn build_shell_env_reports_synthetic_home_creation_failure() {
        let temp = tempdir().unwrap();
        let blocking_file = temp.path().join("blocked");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        let workdir = blocking_file.join("subdir");
        let policy = CapabilityPolicy::default();

        let error = build_shell_env(&policy, &[], &workdir).unwrap_err();

        let expected_home = workdir.join(".capsule-home");
        assert!(error.contains(&expected_home.to_string_lossy().into_owned()));
    }

    #[test]
    fn execute_shell_does_not_spawn_when_synthetic_home_creation_fails() {
        let temp = tempdir().unwrap();
        let blocking_file = temp.path().join("blocked");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        let workdir = blocking_file.join("subdir");
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let error = execute_shell(
            "bash",
            &["-c", "echo should-not-run"],
            &[],
            &workdir,
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap_err();

        assert!(error.to_string().contains(".capsule-home"));
    }

    #[test]
    fn execute_shell_reports_synthetic_home_not_real_host_home() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "echo -n $HOME"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(result.stdout, expected_home.to_string_lossy());
        if let Ok(real_home) = std::env::var("HOME") {
            assert_ne!(result.stdout, real_home);
        }
    }

    #[test]
    fn build_shell_env_strips_wildcard_credential_patterns_from_overrides() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();
        let overrides = vec![
            ("AWS_ACCESS_KEY_ID".to_string(), "leaked".to_string()),
            ("DOCKER_AUTH_CONFIG".to_string(), "leaked".to_string()),
            ("STRIPE_API_KEY".to_string(), "leaked".to_string()),
            ("GITHUB_TOKEN".to_string(), "leaked".to_string()),
            ("SAFE_VAR".to_string(), "kept".to_string()),
        ];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        assert!(!env.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(!env.contains_key("DOCKER_AUTH_CONFIG"));
        assert!(!env.contains_key("STRIPE_API_KEY"));
        assert!(!env.contains_key("GITHUB_TOKEN"));
        assert_eq!(env.get("SAFE_VAR"), Some(&"kept".to_string()));
    }

    #[test]
    fn build_wasi_env_allowlist_is_empty_without_declarations() {
        std::env::set_var("MURMUR_TEST_WASI_UNDECLARED", "host-value");
        let policy = CapabilityPolicy::default();

        let env = build_wasi_env_allowlist(&policy);

        assert!(env.is_empty());
    }

    #[test]
    fn build_wasi_env_allowlist_passes_through_declared_host_var() {
        std::env::set_var("MURMUR_TEST_WASI_ALLOWED", "host-value");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "MURMUR_TEST_WASI_ALLOWED".to_string(),
                // Declared but unset on the host: omitted, not an error.
                "MURMUR_TEST_WASI_NEVER_SET".to_string(),
            ],
            ..CapabilityPolicy::default()
        };

        let env = build_wasi_env_allowlist(&policy);

        assert_eq!(
            env.get("MURMUR_TEST_WASI_ALLOWED"),
            Some(&"host-value".to_string())
        );
        assert!(!env.contains_key("MURMUR_TEST_WASI_NEVER_SET"));
    }

    #[test]
    fn build_wasi_env_allowlist_strips_credential_shaped_declarations() {
        std::env::set_var("GITHUB_TOKEN", "leaked-token");
        std::env::set_var("STRIPE_API_KEY", "leaked-key");
        std::env::set_var("MURMUR_TEST_WASI_CUSTOM_SECRET", "leaked-secret");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "GITHUB_TOKEN".to_string(),
                "STRIPE_API_KEY".to_string(),
                "MURMUR_TEST_WASI_CUSTOM_SECRET".to_string(),
            ],
            shell_strip_env: vec!["*_CUSTOM_SECRET".to_string()],
            ..CapabilityPolicy::default()
        };

        let env = build_wasi_env_allowlist(&policy);

        // Declaring a credential-shaped name does not bypass the backstop.
        assert!(env.is_empty(), "expected all names stripped, got {env:?}");
    }

    #[test]
    fn build_shell_env_home_override_cannot_survive() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_baseline_env: vec!["HOME".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("HOME".to_string(), "/tmp/guest-controlled".to_string())];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(
            env.get("HOME"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn build_shell_env_keeps_safe_baseline_vars() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        std::env::set_var("CARGO_HOME", "/fake/cargo/home");
        let env = build_shell_env(&policy, &[], temp.path()).unwrap();
        std::env::remove_var("CARGO_HOME");

        assert_eq!(env.get("CARGO_HOME"), Some(&"/fake/cargo/home".to_string()));
    }

    #[test]
    fn build_shell_env_developer_declared_strip_env_still_composes() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_strip_env: vec!["MYCOMPANY_*".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("MYCOMPANY_SECRET".to_string(), "leaked".to_string())];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        assert!(!env.contains_key("MYCOMPANY_SECRET"));
    }

    #[test]
    fn env_name_matches_pattern_trailing_wildcard() {
        assert!(env_name_matches_pattern("AWS_*", "AWS_ACCESS_KEY_ID"));
        assert!(!env_name_matches_pattern("AWS_*", "MY_AWS_KEY"));
    }

    #[test]
    fn env_name_matches_pattern_leading_wildcard() {
        assert!(env_name_matches_pattern("*_API_KEY", "STRIPE_API_KEY"));
        assert!(!env_name_matches_pattern("*_API_KEY", "API_KEY_ID"));
    }

    #[test]
    fn env_name_matches_pattern_exact_match() {
        assert!(env_name_matches_pattern("GITHUB_TOKEN", "GITHUB_TOKEN"));
        assert!(!env_name_matches_pattern("GITHUB_TOKEN", "GITHUB_TOKEN_2"));
    }

    #[test]
    fn shell_tool_manifest_yaml_is_valid_yaml_for_interpreter() {
        let yaml = shell_tool_manifest_yaml("bash");
        serde_yaml::from_str::<serde_yaml::Value>(&yaml).expect("bash manifest must be valid YAML");
    }

    #[test]
    fn shell_tool_manifest_yaml_is_valid_yaml_for_non_interpreter() {
        let yaml = shell_tool_manifest_yaml("curl");
        serde_yaml::from_str::<serde_yaml::Value>(&yaml)
            .expect("curl manifest must be valid YAML — embedded quotes break serde_yaml parsing");
    }

    #[test]
    fn shell_tool_manifest_yaml_non_interpreter_has_tool_runtime() {
        let yaml = shell_tool_manifest_yaml("curl");
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            value.get("runtime").and_then(|v| v.as_str()),
            Some("tool"),
            "non-interpreter manifest must have runtime: tool so inventory picks it up"
        );
    }
}
