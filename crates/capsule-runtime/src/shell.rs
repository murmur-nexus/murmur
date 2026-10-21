use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::{
    artifact_config::ARTIFACT_CONFIG_ENV,
    detached::{DetachPolicy, DetachedDispatchInfo},
    types::CapabilityPolicy,
    workdir_fs::{WorkdirRoot, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE},
};

pub(crate) const MAX_SHELL_OUTPUT_BYTES: usize = 16 * 1024;

const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh"];
const DEFAULT_ENV_BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "TERM",
];

/// The two variables that name the account a process runs as, sorted. Neither is a credential:
/// each is a login name. A harness may look up its stored login by it (the macOS login keychain
/// is keyed by user name), so a harness started without them can fail to find a login it has.
pub(crate) const ACCOUNT_ENV: [&str; 2] = ["LOGNAME", "USER"];

/// Serializes the unit tests that set or remove an [`ACCOUNT_ENV`] name in this test binary's
/// process environment, which every test thread shares.
#[cfg(test)]
pub(crate) static ACCOUNT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The host's values of [`ACCOUNT_ENV`], read once.
///
/// Each name carries the host's value or is absent: the runtime never makes one up, never copies
/// one from the other, and [`HostAccountEnv::apply`] lets nothing an allowlist or a launch plan
/// put in an environment stand in for the host's. A value that is not valid UTF-8 counts as
/// unset, since no environment the runtime builds can carry it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostAccountEnv {
    set: Vec<(String, String)>,
    unset: Vec<String>,
}

impl HostAccountEnv {
    /// The account names this process holds.
    pub(crate) fn from_host() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The account names as `lookup` answers them, for a caller that is not reading this process.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let mut set = Vec::new();
        let mut unset = Vec::new();
        for name in ACCOUNT_ENV {
            match lookup(name) {
                Some(value) => set.push((name.to_string(), value)),
                None => unset.push(name.to_string()),
            }
        }
        Self { set, unset }
    }

    /// An account holding exactly `pairs`, for a test standing in for the host.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self::from_lookup(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    /// The names the host did not set, sorted. Empty when it set both.
    pub(crate) fn unset(&self) -> &[String] {
        &self.unset
    }

    /// The names the host set, with its values, sorted by name.
    pub(crate) fn set(&self) -> &[(String, String)] {
        &self.set
    }

    /// Make `env`'s account names the host's: both are removed, then each the host set is
    /// inserted with its value. Whatever `env` held under either name is gone afterwards.
    pub(crate) fn apply(&self, env: &mut BTreeMap<String, String>) {
        for name in ACCOUNT_ENV {
            env.remove(name);
        }
        env.extend(self.set.iter().cloned());
    }
}

/// Directory name for the per-session synthetic home, created under the capsule's
/// session workdir — `<accessible workdir>/.murmur/<session id>/` under `--workdir`, so it is
/// under the one prefix a consumer diffing the accessible workdir excludes. Planned /proc and
/// subprocess key isolation work depend on this exact name/location convention.
///
/// Nothing outside the runtime names this path: the capsule reaches it only through `$HOME`.
/// Declared as a runtime write by [`crate::workdir_writes`].
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
    /// The absolute path of the log holding the full output when [`Self::truncated`] is set:
    /// [`crate::detached::output_path_for`]'s path for a `shell-<ms>` stem in the session's
    /// records directory. `None` for output that fit, and for a truncated command run with no
    /// records directory, which writes no log.
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

/// What a failed `spawn()` of a `command` that went through
/// [`crate::sandbox::prepare_enforcement`] means, for every subprocess that does: shell commands,
/// plan shell steps and native tools.
///
/// A failure inside the `pre_exec` enforcement setup reaches the caller only as a bare errno (std
/// collapses a `pre_exec` `io::Error` to its `raw_os_error()`, defaulting to EINVAL). `command` is
/// dropped first so the parent's captured copy of the diagnostic pipe's write end closes, letting
/// the read see EOF; then the detail the child wrote replaces the undifferentiated
/// "Invalid argument (os error 22)". `subject` names the spawn in that message (`shell`,
/// `native tool`), and `spawn_error` is the error returned when the child wrote nothing.
pub(crate) fn enforced_spawn_failure(
    command: Command,
    supervisor: &crate::sandbox::SupervisorHandle,
    subject: &str,
    spawn_error: String,
) -> ShellExecError {
    drop(command);
    match supervisor.read_diagnostic() {
        // A composed-root failure keeps its own variant rather than being flattened into a
        // message: it means a host that *did* clear the pre-launch sealed probe then failed to
        // build the root, which is a different event from a Landlock or seccomp step failing,
        // points somewhere else, and ends the session instead of the tool call. See
        // [`ShellExecError::session_fatal`].
        Some(detail) if detail.contains(crate::sealed::SEALED_ROOT_FAILURE_PREFIX) => {
            ShellExecError::SealedRootConstructionFailed { detail }
        }
        Some(detail) if !detail.is_empty() => ShellExecError::Failed(format!(
            "sandbox: {subject} enforcement setup failed before exec: {detail}"
        )),
        _ => ShellExecError::Failed(spawn_error),
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
/// the path `plan.rs` and the script-capsule store state take, because neither runs a task loop
/// and so neither has anywhere to deliver a completion.
///
/// With `detach: Some`, the child is polled until it exits or the deadline passes. On the
/// deadline the child, its two pipe readers and the metadata needed to describe it all move to
/// one plain OS thread — not a `spawn_blocking` task — so runtime teardown never waits on it. The
/// session's end kills it with every other process the session started.
///
/// A detach policy is honoured only when `enforcement` holds a records directory, where a demoted
/// command's log goes. Without one the command runs to completion in the foreground.
///
/// Every command runs in the session's one network namespace, so a server one call starts — in
/// the background, or demoted — is reachable from the next call of the same session.
///
/// Everything before the spawn is identical on both paths, including the whole spawn-failure
/// arm: a spawn failure happens inside the grace window and stays a foreground error.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_shell(
    binary: &str,
    args: &[&str],
    env_overrides: &[(String, String)],
    accessible_workdir: &WorkdirRoot,
    session_workdir: &WorkdirRoot,
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

    let env = build_shell_env(
        policy,
        env_overrides,
        session_workdir,
        enforcement.runtime_dir_path(),
        enforcement.records_dir_path(),
        enforcement.tmpdir_value().as_deref(),
    )?;

    // Resolve before spawning: this is the identity of the binary this call is about to
    // run, and it is the only point where the invoked name is still in hand.
    let resolved_binary = crate::sandbox::resolve_invoked_binary_path(binary);

    let started = Instant::now();
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(accessible_workdir.path())
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Fail-closed: if kernel enforcement setup fails unexpectedly, propagate the error and
    // never call `.spawn()` at all — no code path here lets a Linux host silently run this
    // subprocess with zero enforcement because setup failed. This also installs the hard
    // rlimits and (where a scope exists) cgroup membership for the child.
    let supervisor = crate::sandbox::prepare_enforcement(
        &mut command,
        enforcement,
        accessible_workdir.path(),
        session_workdir,
    )?;

    // Entered before the snapshot and held until the command exits — carried to the demoted
    // thread if it gets that far — so a sibling's spawn anywhere inside this window is visible to
    // attribution.
    let cgroup_occupancy = enforcement.cgroup_scope.as_ref().map(|scope| scope.enter());
    // Snapshotted before the child runs so attribution keys on this call's delta rather than on
    // a session-cumulative total an earlier call could have moved.
    let cgroup_counters_before = enforcement
        .cgroup_scope
        .as_ref()
        .map(|scope| scope.event_counters());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Err(enforced_spawn_failure(
                command,
                &supervisor,
                "shell",
                error.to_string(),
            ))
        }
    };

    // `wait_with_output` reads both pipes and waits in one uninterruptible step, so it cannot be
    // given a deadline. Draining each pipe on its own thread separates reading from waiting,
    // which is what lets the wait below be either blocking or polled.
    let readers = OutputReaders::spawn(&mut child);

    // Demotion keeps the output in the records directory and points the agent at it. With no
    // records directory there is nowhere to keep it, so the command runs in the foreground.
    let records_root = enforcement.records_root();
    let detach = detach.filter(|_| records_root.is_some());

    let status = match &detach {
        None => child.wait().map_err(|error| error.to_string())?,
        Some(policy) => {
            let deadline = started + policy.grace;
            loop {
                match child.try_wait().map_err(|error| error.to_string())? {
                    Some(status) => break status,
                    None => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        // A caller that stopped waiting gets the same demotion the deadline
                        // gives, at the next poll: nobody is left to read a foreground result.
                        let abandoned = policy
                            .abandoned
                            .as_ref()
                            .is_some_and(crate::detached::AbandonSignal::is_raised);
                        if remaining.is_zero() || abandoned {
                            // A zero grace demotes here, at the first poll after the spawn.
                            return Ok(ShellOutcome::Detached(demote(
                                child,
                                readers,
                                DemotionContext {
                                    policy: detach.expect("the detach policy was matched above"),
                                    records_root: records_root
                                        .expect("a detach policy is kept only with a records root")
                                        .clone(),
                                    invoked_binary: binary.to_string(),
                                    resolved_binary,
                                    args: args.iter().map(|arg| (*arg).to_string()).collect(),
                                    started,
                                    cgroup_scope: enforcement.cgroup_scope.clone(),
                                    cgroup_counters_before,
                                    cgroup_occupancy,
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
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    let resource_limit_hit = classify_resource_limit(
        &status,
        enforcement.cgroup_scope.as_deref(),
        cgroup_counters_before,
        cgroup_occupancy.as_ref(),
    );
    drop(cgroup_occupancy);
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
        if let Some(records_root) = records_root {
            full_output_path = Some(write_shell_output_log(
                records_root,
                &format!("shell-{}", crate::trace::timestamp_ms()),
                binary,
                args,
                exit_code,
                &full_stdout,
                &full_stderr,
            )?);
        }
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
    accessible_workdir: &WorkdirRoot,
    session_workdir: &WorkdirRoot,
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
) -> Result<ShellResult, ShellExecError> {
    match run_shell(
        binary,
        args,
        env_overrides,
        accessible_workdir,
        session_workdir,
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
    /// The session's records directory, where the command's log goes and which the completion
    /// names it in.
    records_root: WorkdirRoot,
    /// The name as invoked, which is what the log header records.
    invoked_binary: String,
    /// The same name resolved to a path where the host `PATH` named one — what the trace and the
    /// completion report, matching [`ShellResult::binary`].
    resolved_binary: String,
    args: Vec<String>,
    started: Instant,
    cgroup_scope: Option<std::sync::Arc<crate::cgroup::CgroupScope>>,
    cgroup_counters_before: Option<crate::cgroup::CgroupEventCounters>,
    /// The command's stay in the scope, held on the background thread until it exits.
    cgroup_occupancy: Option<crate::cgroup::ScopeOccupancy>,
}

/// Hand a still-running command to a thread of its own and return the handle for it.
///
/// The work is registered before this returns, so the session-end sweep can never miss work that
/// finished being registered after it looked.
fn demote(
    mut child: std::process::Child,
    readers: OutputReaders,
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
            // The spawn instant, not this one. Demotion happens a whole grace period after the
            // command started, and every reader of this field — the cancel residue, the
            // session-end sweep's `running_ms` — reports it as how long the command has been
            // running, which must include the foreground portion to match the `duration_ms` a
            // completion carries.
            started_at_ms: crate::trace::timestamp_ms().saturating_sub(
                context
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
            ),
        });

    let provenance = context.policy.completion_provenance();
    let DemotionContext {
        policy,
        records_root,
        invoked_binary,
        resolved_binary,
        args,
        started,
        cgroup_scope,
        cgroup_counters_before,
        cgroup_occupancy,
    } = context;

    std::thread::spawn(move || {
        let waited = child.wait();
        let (stdout_bytes, stderr_bytes) = readers.join();
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

        let (exit_code, resource_limit, mut error) = match waited {
            Ok(status) => (
                exit_code_of(&status),
                classify_resource_limit(
                    &status,
                    cgroup_scope.as_deref(),
                    cgroup_counters_before,
                    cgroup_occupancy.as_ref(),
                ),
                None,
            ),
            Err(failure) => (
                -1,
                None,
                Some(format!("waiting for the command failed: {failure}")),
            ),
        };
        drop(cgroup_occupancy);

        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let log = crate::detached::output_path_for(&records_root, &work_id);
        if let Err(failure) = write_shell_output_log(
            &records_root,
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
        let output_bytes = records_root
            .file_len(&log.relative)
            .ok()
            .flatten()
            .unwrap_or(0);

        policy
            .registry
            .complete(crate::detached::DetachedCompletion {
                work_id,
                binary: resolved_binary,
                command: policy.command.clone(),
                exit_code,
                duration_ms,
                output_path: log.path,
                output_bytes,
                resource_limit,
                context_id: policy.context_id.clone(),
                provenance,
                error,
            });
    });

    info
}

/// Exit code for a finished subprocess, keeping the signal that killed it legible.
///
/// `ExitStatus::code()` returns `None` for every signal-killed process on Unix, so a bare
/// `.unwrap_or(-1)` collapses a `SIGXCPU` from `RLIMIT_CPU`, a cgroup OOM `SIGKILL` and an
/// unrelated crash into one indistinguishable `-1`. `128 + signal` is the long-standing shell
/// convention for exactly this case and keeps the cause readable in the trace even where
/// [`classify_resource_limit`] declines to name a limit.
pub(crate) fn exit_code_of(status: &std::process::ExitStatus) -> i32 {
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
/// which is the more specific claim. The counter delta is about the whole scope, so it names this
/// call's limit only when the call had the scope to itself — see [`attribute_resource_limit`].
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
    occupancy: Option<&crate::cgroup::ScopeOccupancy>,
) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;

    let signal_limit = status
        .signal()
        .and_then(crate::resources::limit_from_signal);
    let counter_delta = match (cgroup_scope, counters_before) {
        (Some(scope), Some(before)) => scope.event_counters().attribution_since(before),
        _ => None,
    };
    let exclusive = occupancy.is_some_and(crate::cgroup::ScopeOccupancy::exclusive);
    attribute_resource_limit(signal_limit, counter_delta, exclusive)
}

/// The limit a call is reported as having hit, from the two kinds of evidence there are.
///
/// The signal names the process that died, so it is this call's whenever there is one. The
/// cgroup counter delta is about the scope, and every process in the scope moves it: it is this
/// call's only when the call was `exclusive` — alone in the scope from entry to exit. Otherwise
/// the delta could be a sibling's, and nothing is attributed.
pub(crate) fn attribute_resource_limit(
    signal_limit: Option<&'static str>,
    counter_delta: Option<&'static str>,
    exclusive: bool,
) -> Option<String> {
    signal_limit
        .or(counter_delta.filter(|_| exclusive))
        .map(str::to_string)
}

/// The environment of one shell command, plan shell step or native tool: [`DEFAULT_ENV_BASELINE`]
/// from the host, `env_overrides`, `capabilities.shell.baseline_env`, the credential backstop,
/// and then the runtime-owned names no entry before them can set — the synthetic `HOME`,
/// `MURMUR_RUNTIME_DIR` and `MURMUR_RECORDS_DIR` from `runtime_dir` and `records_dir`, and
/// `TMPDIR`, `TMP` and `TEMP`, all three `tmpdir` or all three absent.
pub(crate) fn build_shell_env(
    policy: &CapabilityPolicy,
    env_overrides: &[(String, String)],
    session_workdir: &WorkdirRoot,
    runtime_dir: Option<&std::path::Path>,
    records_dir: Option<&std::path::Path>,
    tmpdir: Option<&str>,
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
        // Formation names and the spawner lifeline are never a native process's: a baseline
        // entry naming one supplies nothing from the host.
        if crate::formation::is_member_grant_env(key)
            || key == crate::lifeline::SPAWNER_LIFELINE_ENV
        {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    // Inserted last so neither a guest-supplied `env_overrides` entry nor a
    // manifest-declared `shell_baseline_env` entry can resurrect the real host
    // HOME/USERPROFILE, and so no strip_env pattern can remove the synthetic one.
    let synthetic_home = synthetic_home_dir(session_workdir)?;
    let synthetic_home = synthetic_home.to_string_lossy().into_owned();
    env.insert("HOME".to_string(), synthetic_home.clone());
    env.insert("USERPROFILE".to_string(), synthetic_home);
    // Runtime-owned on the same terms: whatever an override, a baseline entry or the host said,
    // a command reads the session's own runtime directory here, or no such variable at all.
    env.remove(crate::runtime_dir::RUNTIME_DIR_ENV);
    if let Some(runtime_dir) = runtime_dir {
        env.insert(
            crate::runtime_dir::RUNTIME_DIR_ENV.to_string(),
            runtime_dir.to_string_lossy().into_owned(),
        );
    }
    // And the session's records directory, on the same terms.
    env.remove(crate::records_dir::RECORDS_DIR_ENV);
    if let Some(records_dir) = records_dir {
        env.insert(
            crate::records_dir::RECORDS_DIR_ENV.to_string(),
            records_dir.to_string_lossy().into_owned(),
        );
    }
    // And the session's temporary folder, under all three names programs look for it by.
    for name in crate::temp_dir::TEMP_DIR_ENV {
        env.remove(name);
        if let Some(tmpdir) = tmpdir {
            env.insert(name.to_string(), tmpdir.to_string());
        }
    }

    Ok(env)
}

/// Drop every entry whose name matches [`CREDENTIAL_ENV_PATTERNS`] or one of
/// `extra_patterns` (a policy's `shell_strip_env`).
///
/// This is the single credential backstop for both environments the runtime builds: the
/// native subprocess env ([`build_shell_env`]) and the WASI guest env
/// ([`build_declared_env`]). It runs *after* whatever allowlist populated `env`, so a
/// declared name never bypasses the filter.
pub(crate) fn strip_credential_shaped_vars(
    env: &mut BTreeMap<String, String>,
    extra_patterns: &[String],
) {
    env.retain(|key, _| !credential_backstop_drops(key, extra_patterns));
}

/// Which list a credential backstop pattern came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackstopPatternSource {
    /// [`CREDENTIAL_ENV_PATTERNS`], the list no manifest can change.
    Builtin,
    /// The manifest's own `capabilities.shell.strip_env`.
    StripEnv,
}

/// The pattern that makes the credential backstop drop a name, verbatim, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackstopMatch {
    pub pattern: String,
    pub source: BackstopPatternSource,
}

/// The first credential backstop pattern that matches `name`, or `None` when the backstop keeps
/// it — the one decision procedure behind [`strip_credential_shaped_vars`],
/// [`credential_backstop_drops`], `W-SEC-024` and the `E-CAP-016` staging refusal.
///
/// [`CREDENTIAL_ENV_PATTERNS`] is tried before `extra_patterns`, so a name both lists cover is
/// attributed to the built-in pattern: that is the one no manifest edit can remove.
/// `extra_patterns` is a policy's `shell_strip_env`. Judges a name alone and never reads the host
/// environment.
pub fn credential_backstop_match(name: &str, extra_patterns: &[String]) -> Option<BackstopMatch> {
    CREDENTIAL_ENV_PATTERNS
        .iter()
        .map(|pattern| (*pattern, BackstopPatternSource::Builtin))
        .chain(
            extra_patterns
                .iter()
                .map(|pattern| (pattern.as_str(), BackstopPatternSource::StripEnv)),
        )
        .find(|(pattern, _)| env_name_matches_pattern(pattern, name))
        .map(|(pattern, source)| BackstopMatch {
            pattern: pattern.to_string(),
            source,
        })
}

/// Whether the credential backstop would drop a variable of this name — the same question
/// [`strip_credential_shaped_vars`] answers per entry, asked of a name alone. See
/// [`credential_backstop_match`] for which pattern decided it.
pub fn credential_backstop_drops(name: &str, extra_patterns: &[String]) -> bool {
    credential_backstop_match(name, extra_patterns).is_some()
}

/// Resolve the host variables the manifest declared: only names in `capabilities.env.allow` that
/// the host actually has set, minus anything credential-shaped. A declared name absent from the
/// host is simply omitted, not an error.
///
/// This is what every WASM guest observes. A `transport: process` harness starts from it plus the
/// host's [`HostAccountEnv`], before its driver's `env-set` is applied. Native shell subprocesses
/// are the exception: they run on [`DEFAULT_ENV_BASELINE`] plus `capabilities.shell.baseline_env`,
/// built by [`build_shell_env`].
///
/// [`ARTIFACT_CONFIG_ENV`], a formation member's names
/// ([`crate::formation::is_member_grant_env`]) and [`crate::lifeline::SPAWNER_LIFELINE_ENV`] are
/// reserved and never resolved from the host, whatever a manifest allowlists: the names are
/// runtime-owned, and their values come from the declaring artifact's own `config:` block and from
/// the session, or from nowhere. Skipped here rather than relied on being overwritten later, so a
/// host value cannot reach a guest whose entry declared no config at all.
pub(crate) fn build_declared_env(policy: &CapabilityPolicy) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    for key in &policy.env_allow {
        if key == ARTIFACT_CONFIG_ENV
            || crate::formation::is_member_grant_env(key)
            || key == crate::lifeline::SPAWNER_LIFELINE_ENV
            || key == crate::runtime_dir::RUNTIME_DIR_ENV
            || key == crate::records_dir::RECORDS_DIR_ENV
        {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    env
}

fn synthetic_home_dir(session_workdir: &WorkdirRoot) -> Result<std::path::PathBuf, String> {
    session_workdir
        .create_dir_all(SYNTHETIC_HOME_DIR_NAME, DEFAULT_DIR_MODE)
        .map_err(|error| format!("failed to create synthetic home directory: {error}"))?;
    Ok(session_workdir.host_path(SYNTHETIC_HOME_DIR_NAME))
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

/// Write one command's full stdout and stderr to its log in the session's records directory,
/// returning the log's absolute path. [`crate::detached::output_path_for`] places it.
///
/// Two callers, one content format: a foreground command whose output exceeded
/// [`MAX_SHELL_OUTPUT_BYTES`] passes a `shell-<ms>` stem, and a demoted command passes its work
/// id. A reader that finds either file reads the same header, `Stdout:` and `Stderr:` sections.
fn write_shell_output_log(
    records_root: &WorkdirRoot,
    file_stem: &str,
    binary: &str,
    args: &[&str],
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<String, String> {
    let log = crate::detached::output_path_for(records_root, file_stem);

    let command = if args.is_empty() {
        binary.to_string()
    } else {
        format!("{binary} {}", args.join(" "))
    };

    let content = format!(
        "Command: {command}\nExit code: {exit_code}\n\nStdout:\n{stdout}\n\nStderr:\n{stderr}\n"
    );

    records_root
        .write_file(&log.relative, content.as_bytes(), DEFAULT_FILE_MODE)
        .map_err(|error| format!("failed to write shell log: {error}"))?;

    Ok(log.path)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must run");

        assert_eq!(result.stdout.trim(), "64", "stderr was: {}", result.stderr);
        assert_eq!(result.resource_limit_hit, None);
    }

    /// With `RLIMIT_NPROC` left alone, `pids.max` is the only process bound on a scoped capsule,
    /// and it has to stop a fork bomb by itself and be named as what did. The children sleep
    /// rather than exit so every one of them holds a task in the scope until the loop gives up.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_fork_bomb_in_a_scoped_capsule_is_stopped_by_pids_max() {
        const TEST: &str = "a_fork_bomb_in_a_scoped_capsule_is_stopped_by_pids_max";
        if !crate::cgroup::cgroup_delegation_available() {
            crate::runtime_err!("[SKIP-HOST] {TEST}: this host cannot delegate a cgroup v2 scope");
            return;
        }

        let temp = tempdir().unwrap();
        let limits = crate::resources::HostResourceLimits {
            cgroup_pids_max: 16,
            ..crate::resources::HostResourceLimits::default()
        };
        let scope = crate::cgroup::prepare_scope(true, &limits, "ses_fork_bomb", temp.path())
            .expect("this host delegates a cgroup scope")
            .scope;
        let policy = CapabilityPolicy {
            shell_allow: vec!["python3".to_string()],
            resources: limits,
            ..CapabilityPolicy::default()
        };
        let mut enforcement = ShellEnforcement::environment_only();
        enforcement.resource_limits = limits;
        let enforcement = enforcement.with_host_bounding(scope, None);

        let script = "import os, resource, signal, time\n\
            children = []\n\
            for _ in range(4096):\n\
            \x20   try:\n\
            \x20       pid = os.fork()\n\
            \x20   except OSError:\n\
            \x20       break\n\
            \x20   if pid == 0:\n\
            \x20       time.sleep(30)\n\
            \x20       os._exit(0)\n\
            \x20   children.append(pid)\n\
            for pid in children:\n\
            \x20   os.kill(pid, signal.SIGKILL)\n\
            for pid in children:\n\
            \x20   os.waitpid(pid, 0)\n\
            soft = resource.getrlimit(resource.RLIMIT_NPROC)[0]\n\
            print(len(children))\n\
            print('unlimited' if soft == resource.RLIM_INFINITY else soft)\n";
        let result = execute_shell(
            "python3",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("python3 must run");

        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        let mut lines = result.stdout.lines();
        let reached: u64 = lines.next().unwrap().parse().unwrap();
        let soft = lines.next().unwrap();
        assert!(
            reached < 16,
            "pids.max is 16 and the python process holds one task, so at most 15 children can \
             be live; reached {reached}"
        );
        assert_eq!(
            result.resource_limit_hit,
            Some("cgroup_pids_max".to_string())
        );
        let (own_soft, _) = crate::resources::test_support::own_rlimit_nproc();
        assert_eq!(
            soft,
            crate::resources::test_support::rendered_rlimit(own_soft),
            "a scoped spawn inherits RLIMIT_NPROC unchanged"
        );
    }

    /// Under `--workdir` the two workdirs are different directories, and each of the two things
    /// `execute_shell` does with one has to pick the right one: the subprocess runs in the
    /// accessible workdir and `$HOME` resolves into the session workdir.
    ///
    /// Asserted through a real subprocess rather than through `build_shell_env` alone, because
    /// `$HOME` naming a directory that was never created is exactly the failure this split can
    /// produce and a pure env check would not see.
    #[test]
    fn a_shell_command_runs_in_the_accessible_workdir_with_home_inside_the_session_one() {
        let accessible = tempdir().unwrap();
        let session = accessible.path().join(".murmur").join("ses_shell_test");
        fs::create_dir_all(&session).unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &[
                "-c",
                "echo marker > \"$HOME/probe\" && cat \"$HOME/probe\" && pwd",
            ],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(accessible.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(&session).unwrap(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .expect("bash must run");

        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        let mut lines = result.stdout.lines();
        assert_eq!(lines.next(), Some("marker"));
        assert_eq!(
            std::fs::canonicalize(lines.next().unwrap()).unwrap(),
            std::fs::canonicalize(accessible.path()).unwrap(),
            "the subprocess cwd is the directory the capsule was asked to produce"
        );

        assert_eq!(
            fs::read_to_string(session.join(SYNTHETIC_HOME_DIR_NAME).join("probe")).unwrap(),
            "marker\n"
        );
        assert!(
            !accessible.path().join(SYNTHETIC_HOME_DIR_NAME).exists(),
            "the synthetic home must add nothing to the top level of the accessible workdir"
        );
    }

    /// A command printing `seq 1 <last>`, past [`MAX_SHELL_OUTPUT_BYTES`] for any `last` over
    /// 4,000, run in `workdir` with `session` as its session workdir. The two are different
    /// directories, so `$HOME` adds nothing to `workdir`.
    fn print_seq(
        workdir: &Path,
        session: &Path,
        enforcement: &ShellEnforcement,
        last: u32,
    ) -> ShellResult {
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let command = format!("seq 1 {last}");
        execute_shell(
            "bash",
            &["-c", &command],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(session).unwrap(),
            &policy,
            enforcement,
        )
        .expect("bash must run")
    }

    /// A truncated command's full output goes to `logs/shell-<ms>.log` in the session's records
    /// directory, and the result names it by its absolute path. The work folder gains nothing.
    #[test]
    fn a_truncated_output_log_lands_in_the_records_directory() {
        let workdir = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let enforcement = ShellEnforcement::environment_only()
            .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None);
        let records_root = enforcement.records_root().unwrap().clone();
        let session = tempdir().unwrap();

        let result = print_seq(workdir.path(), session.path(), &enforcement, 20_000);

        assert!(result.truncated);
        assert_eq!(result.stdout.len(), MAX_SHELL_OUTPUT_BYTES);
        let path = result
            .full_output_path
            .expect("a truncated command names its log");
        let file_name = Path::new(&path).file_name().unwrap().to_str().unwrap();
        let ms = file_name
            .strip_prefix("shell-")
            .and_then(|rest| rest.strip_suffix(".log"))
            .unwrap_or_else(|| panic!("{path} is not logs/shell-<ms>.log"));
        assert!(ms.chars().all(|c| c.is_ascii_digit()), "{path}");
        assert_eq!(
            Path::new(&path),
            records_root.host_path("logs").join(file_name),
        );
        assert!(Path::new(&path).is_absolute(), "{path}");

        let log = fs::read_to_string(&path).unwrap();
        let expected: String = (1..=20_000).map(|n| format!("{n}\n")).collect();
        let stdout = log
            .split_once("\nStdout:\n")
            .and_then(|(_, rest)| rest.split_once("\n\nStderr:\n"))
            .map(|(stdout, _)| stdout)
            .expect("the log keeps its Stdout: and Stderr: sections");
        assert_eq!(stdout, expected, "the log holds the full stdout");
        assert!(log.starts_with("Command: bash -c seq 1 20000\nExit code: 0\n"));

        assert!(
            !workdir.path().join("logs").exists(),
            "nothing is written into the accessible workdir"
        );
        assert_eq!(fs::read_dir(workdir.path()).unwrap().count(), 0);
        assert!(!session.path().join("logs").exists());
    }

    /// Outside a session there is no records directory, so a truncated command writes no log in
    /// either workdir and names no path. Roots built without records are the same case as no
    /// roots at all.
    #[test]
    fn with_no_records_directory_a_truncated_command_writes_no_log() {
        let without_roots = ShellEnforcement::environment_only();
        let runtime = tempdir().unwrap();
        let open = |sub: &str| {
            let path = runtime.path().join(sub);
            fs::create_dir_all(&path).unwrap();
            crate::workdir_fs::WorkdirRoot::open(&path).unwrap()
        };
        let roots_without_records = ShellEnforcement::environment_only().with_runtime_dir(
            crate::runtime_dir::RuntimeDirRoots::new(
                open(crate::runtime_dir::RUNTIME_SUBDIR),
                open(crate::runtime_dir::SEALED_ETC_SUBDIR),
                open(crate::runtime_dir::HARNESS_SUBDIR),
            ),
            None,
        );

        for enforcement in [&without_roots, &roots_without_records] {
            let workdir = tempdir().unwrap();
            let session = tempdir().unwrap();
            let result = print_seq(workdir.path(), session.path(), enforcement, 20_000);
            assert!(result.truncated);
            assert_eq!(result.stdout.len(), MAX_SHELL_OUTPUT_BYTES);
            assert_eq!(result.full_output_path, None);
            assert_eq!(
                fs::read_dir(workdir.path()).unwrap().count(),
                0,
                "nothing is written into the workdir"
            );
            assert!(
                !session.path().join("logs").exists(),
                "nothing is written into the session workdir"
            );
        }
        assert!(!runtime.path().join("logs").exists());
        assert!(!runtime
            .path()
            .join(crate::runtime_dir::RUNTIME_SUBDIR)
            .join("logs")
            .exists());
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(Path::new(".")).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(Path::new(".")).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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

        let env = build_shell_env(
            &policy,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();

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
        // A file where the synthetic home belongs, so it cannot be created.
        std::fs::write(
            temp.path().join(SYNTHETIC_HOME_DIR_NAME),
            b"not a directory",
        )
        .unwrap();
        let policy = CapabilityPolicy::default();

        let error = build_shell_env(
            &policy,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap_err();

        assert!(error.contains(SYNTHETIC_HOME_DIR_NAME), "{error}");
    }

    #[test]
    fn execute_shell_does_not_spawn_when_synthetic_home_creation_fails() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join(SYNTHETIC_HOME_DIR_NAME),
            b"not a directory",
        )
        .unwrap();
        let workdir = temp.path().to_path_buf();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let error = execute_shell(
            "bash",
            &["-c", "echo should-not-run"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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

        let env = build_shell_env(
            &policy,
            &overrides,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();

        assert!(!env.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(!env.contains_key("DOCKER_AUTH_CONFIG"));
        assert!(!env.contains_key("STRIPE_API_KEY"));
        assert!(!env.contains_key("GITHUB_TOKEN"));
        assert_eq!(env.get("SAFE_VAR"), Some(&"kept".to_string()));
    }

    #[test]
    fn declared_env_is_empty_without_declarations() {
        std::env::set_var("MURMUR_TEST_WASI_UNDECLARED", "host-value");
        let policy = CapabilityPolicy::default();

        let env = build_declared_env(&policy);

        assert!(env.is_empty());
    }

    /// The lifeline variable names a descriptor of the member's own process: neither an
    /// `env.allow` nor a `shell.baseline_env` declaration resolves it from the host.
    #[test]
    fn declared_and_shell_env_never_carry_the_formation_lifeline() {
        use crate::lifeline::FORMATION_LIFELINE_ENV;
        let _guard = crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempdir().unwrap();
        std::env::set_var(FORMATION_LIFELINE_ENV, "7");
        let policy = CapabilityPolicy {
            env_allow: vec![FORMATION_LIFELINE_ENV.to_string()],
            shell_baseline_env: vec![FORMATION_LIFELINE_ENV.to_string()],
            ..CapabilityPolicy::default()
        };
        let declared = build_declared_env(&policy);
        let shell = build_shell_env(
            &policy,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        std::env::remove_var(FORMATION_LIFELINE_ENV);
        assert!(
            !declared.contains_key(FORMATION_LIFELINE_ENV),
            "{declared:?}"
        );
        assert!(!shell.contains_key(FORMATION_LIFELINE_ENV), "{shell:?}");
    }

    /// The spawner lifeline names a descriptor of the child's own process, on the same terms.
    #[test]
    fn declared_and_shell_env_never_carry_the_spawner_lifeline() {
        use crate::lifeline::SPAWNER_LIFELINE_ENV;
        let _guard = crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempdir().unwrap();
        std::env::set_var(SPAWNER_LIFELINE_ENV, "7");
        let policy = CapabilityPolicy {
            env_allow: vec![SPAWNER_LIFELINE_ENV.to_string()],
            shell_baseline_env: vec![SPAWNER_LIFELINE_ENV.to_string()],
            ..CapabilityPolicy::default()
        };
        let declared = build_declared_env(&policy);
        let shell = build_shell_env(
            &policy,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        std::env::remove_var(SPAWNER_LIFELINE_ENV);
        assert!(!declared.contains_key(SPAWNER_LIFELINE_ENV), "{declared:?}");
        assert!(!shell.contains_key(SPAWNER_LIFELINE_ENV), "{shell:?}");
    }

    #[test]
    fn declared_env_passes_through_declared_host_var() {
        std::env::set_var("MURMUR_TEST_WASI_ALLOWED", "host-value");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "MURMUR_TEST_WASI_ALLOWED".to_string(),
                // Declared but unset on the host: omitted, not an error.
                "MURMUR_TEST_WASI_NEVER_SET".to_string(),
            ],
            ..CapabilityPolicy::default()
        };

        let env = build_declared_env(&policy);

        assert_eq!(
            env.get("MURMUR_TEST_WASI_ALLOWED"),
            Some(&"host-value".to_string())
        );
        assert!(!env.contains_key("MURMUR_TEST_WASI_NEVER_SET"));
    }

    #[test]
    fn declared_env_strips_credential_shaped_declarations() {
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

        let env = build_declared_env(&policy);

        // Declaring a credential-shaped name does not bypass the backstop.
        assert!(env.is_empty(), "expected all names stripped, got {env:?}");
    }

    /// A credential-shaped name outside every backstop pattern is delivered like any other
    /// declared name: `W-SEC-024` reports that grant, and nothing withholds it.
    #[test]
    fn declared_env_delivers_a_credential_shaped_name_the_backstop_keeps() {
        std::env::set_var("PRIVATE_KEY", "host-private-key");
        std::env::set_var("MURMUR_TEST_WASI_PLAIN", "host-plain");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "PRIVATE_KEY".to_string(),
                "MURMUR_TEST_WASI_PLAIN".to_string(),
            ],
            ..CapabilityPolicy::default()
        };

        let env = build_declared_env(&policy);

        assert_eq!(
            env.get("PRIVATE_KEY"),
            Some(&"host-private-key".to_string())
        );
        assert_eq!(
            env.get("MURMUR_TEST_WASI_PLAIN"),
            Some(&"host-plain".to_string())
        );
    }

    #[test]
    fn build_shell_env_home_override_cannot_survive() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_baseline_env: vec!["HOME".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("HOME".to_string(), "/tmp/guest-controlled".to_string())];

        let env = build_shell_env(
            &policy,
            &overrides,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();

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
        let env = build_shell_env(
            &policy,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        std::env::remove_var("CARGO_HOME");

        assert_eq!(env.get("CARGO_HOME"), Some(&"/fake/cargo/home".to_string()));
    }

    /// A command names the account it runs as with the host's own pair, beside a synthetic
    /// `HOME`, and `strip_env` governs the account names like any other baseline entry.
    #[test]
    fn build_shell_env_carries_the_hosts_account_names() {
        let temp = tempdir().unwrap();
        let root = crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap();
        let _guard = ACCOUNT_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::env::set_var("USER", "murmur-test-user");
        std::env::set_var("LOGNAME", "murmur-test-login");

        let env =
            build_shell_env(&CapabilityPolicy::default(), &[], &root, None, None, None).unwrap();
        let stripped = build_shell_env(
            &CapabilityPolicy {
                shell_strip_env: vec!["LOGNAME".to_string()],
                ..CapabilityPolicy::default()
            },
            &[],
            &root,
            None,
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            env.get("USER").map(String::as_str),
            Some("murmur-test-user")
        );
        assert_eq!(
            env.get("LOGNAME").map(String::as_str),
            Some("murmur-test-login")
        );
        let expected_home = temp.path().join(SYNTHETIC_HOME_DIR_NAME);
        assert_eq!(
            env.get("HOME"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
        assert!(!stripped.contains_key("LOGNAME"), "{stripped:?}");
        assert_eq!(
            stripped.get("USER").map(String::as_str),
            Some("murmur-test-user")
        );
        let guest = build_declared_env(&CapabilityPolicy::default());
        assert!(
            !guest.contains_key("USER") && !guest.contains_key("LOGNAME"),
            "a WASM guest is handed only what env.allow declares: {guest:?}"
        );
    }

    #[test]
    fn host_account_with_both_names_set_reports_none_unset() {
        let account = HostAccountEnv::from_pairs(&[("USER", "alice"), ("LOGNAME", "alice-login")]);
        assert!(account.unset().is_empty());
        let mut env = BTreeMap::new();
        account.apply(&mut env);
        assert_eq!(env.get("USER").map(String::as_str), Some("alice"));
        assert_eq!(env.get("LOGNAME").map(String::as_str), Some("alice-login"));
    }

    /// One name set never supplies the other.
    #[test]
    fn host_account_with_one_name_set_never_copies_it_to_the_other() {
        let account = HostAccountEnv::from_pairs(&[("USER", "alice")]);
        assert_eq!(account.unset(), ["LOGNAME".to_string()]);
        let mut env = BTreeMap::new();
        account.apply(&mut env);
        assert_eq!(env.get("USER").map(String::as_str), Some("alice"));
        assert!(!env.contains_key("LOGNAME"), "{env:?}");
    }

    #[test]
    fn host_account_with_neither_name_set_reports_both_sorted() {
        let account = HostAccountEnv::from_pairs(&[]);
        assert_eq!(account.unset(), ["LOGNAME".to_string(), "USER".to_string()]);
        let mut env = BTreeMap::new();
        account.apply(&mut env);
        assert!(env.is_empty(), "{env:?}");
    }

    /// Whatever an environment held under an account name is replaced by the host's value, or
    /// removed when the host has none.
    #[test]
    fn host_account_apply_replaces_a_stale_value() {
        let account = HostAccountEnv::from_pairs(&[("USER", "alice")]);
        let mut env = BTreeMap::from([
            ("USER".to_string(), "someone-else".to_string()),
            ("LOGNAME".to_string(), "someone-else".to_string()),
            ("PATH".to_string(), "/bin".to_string()),
        ]);
        account.apply(&mut env);
        assert_eq!(env.get("USER").map(String::as_str), Some("alice"));
        assert!(!env.contains_key("LOGNAME"), "{env:?}");
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
    }

    #[test]
    fn build_shell_env_developer_declared_strip_env_still_composes() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_strip_env: vec!["MYCOMPANY_*".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("MYCOMPANY_SECRET".to_string(), "leaked".to_string())];

        let env = build_shell_env(
            &policy,
            &overrides,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            None,
            None,
            None,
        )
        .unwrap();

        assert!(!env.contains_key("MYCOMPANY_SECRET"));
    }

    /// The prediction and the strip are one decision procedure, asserted over names that exercise
    /// every arm of the pattern grammar plus a manifest-supplied pattern. A disagreement here is a
    /// `W-SEC-024` line that tells an operator the opposite of what the guest env will hold.
    #[test]
    fn credential_backstop_drops_agrees_with_the_strip_it_predicts() {
        let extra = vec!["*_SERVICE_SECRET".to_string()];
        let names = [
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "GITHUB_TOKEN_2",
            "AWS_ACCESS_KEY_ID",
            "MY_AWS_KEY",
            "STRIPE_API_KEY",
            "KUBECONFIG",
            "MY_SERVICE_SECRET",
            "DATABASE_PASSWORD",
            "SLACK_BOT_TOKEN",
            "HOME",
            "TZ",
        ];

        let mut env: BTreeMap<String, String> = names
            .iter()
            .map(|name| ((*name).to_string(), "value".to_string()))
            .collect();
        strip_credential_shaped_vars(&mut env, &extra);

        for name in names {
            assert_eq!(
                credential_backstop_drops(name, &extra),
                !env.contains_key(name),
                "prediction and strip disagree about {name}"
            );
            assert_eq!(
                credential_backstop_match(name, &extra).is_some(),
                !env.contains_key(name),
                "match and strip disagree about {name}"
            );
        }
    }

    #[test]
    fn credential_backstop_match_attributes_a_builtin_pattern() {
        assert_eq!(
            credential_backstop_match("AWS_REGION", &[]),
            Some(BackstopMatch {
                pattern: "AWS_*".to_string(),
                source: BackstopPatternSource::Builtin,
            })
        );
        assert_eq!(
            credential_backstop_match("GITHUB_TOKEN", &[]),
            Some(BackstopMatch {
                pattern: "GITHUB_TOKEN".to_string(),
                source: BackstopPatternSource::Builtin,
            })
        );
    }

    #[test]
    fn credential_backstop_match_attributes_a_strip_env_pattern() {
        assert_eq!(
            credential_backstop_match("MY_SERVICE_SECRET", &["*_SERVICE_SECRET".to_string()]),
            Some(BackstopMatch {
                pattern: "*_SERVICE_SECRET".to_string(),
                source: BackstopPatternSource::StripEnv,
            })
        );
    }

    /// A name both lists cover is attributed to the built-in pattern, the one a manifest edit
    /// cannot remove.
    #[test]
    fn credential_backstop_match_prefers_the_builtin_pattern_when_both_match() {
        assert_eq!(
            credential_backstop_match("STRIPE_API_KEY", &["STRIPE_*".to_string()]),
            Some(BackstopMatch {
                pattern: "*_API_KEY".to_string(),
                source: BackstopPatternSource::Builtin,
            })
        );
    }

    #[test]
    fn credential_backstop_match_is_none_for_a_name_the_backstop_keeps() {
        assert_eq!(credential_backstop_match("TZ", &[]), None);
        assert_eq!(
            credential_backstop_match("PRIVATE_KEY", &["*_SERVICE_SECRET".to_string()]),
            None
        );
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
        serde_norway::from_str::<serde_norway::Value>(&yaml)
            .expect("bash manifest must be valid YAML");
    }

    #[test]
    fn shell_tool_manifest_yaml_is_valid_yaml_for_non_interpreter() {
        let yaml = shell_tool_manifest_yaml("curl");
        serde_norway::from_str::<serde_norway::Value>(&yaml)
            .expect("curl manifest must be valid YAML — embedded quotes break YAML parsing");
    }

    #[test]
    fn shell_tool_manifest_yaml_non_interpreter_has_tool_runtime() {
        let yaml = shell_tool_manifest_yaml("curl");
        let value: serde_norway::Value = serde_norway::from_str(&yaml).unwrap();
        assert_eq!(
            value.get("runtime").and_then(|v| v.as_str()),
            Some("tool"),
            "non-interpreter manifest must have runtime: tool so inventory picks it up"
        );
    }

    /// A caller that stops waiting demotes its command at once, through the same path the grace
    /// period takes, rather than holding a blocking thread for the rest of the grace window.
    #[test]
    fn an_abandoned_command_demotes_before_its_grace() {
        use std::sync::Arc;

        use crate::{
            detached::{AbandonSignal, DetachPolicy, DetachedRegistry},
            origin::{TaskOrigin, TaskProvenance},
        };

        let temp = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let (registry, _reports) = DetachedRegistry::new();
        let abandon = AbandonSignal::new();
        let raiser = {
            let abandon = abandon.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                abandon.raise();
            })
        };

        let started = Instant::now();
        let outcome = run_shell(
            "bash",
            // Outlives the abandonment by seconds and then ends on its own, so nothing has to
            // kill it; without the demotion the call would return only when it exited.
            &["-c", "sleep 5"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &ShellEnforcement::environment_only()
                .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None),
            Some(DetachPolicy {
                grace: Duration::from_secs(60),
                delivered: true,
                registry: Arc::clone(&registry),
                command: "sleep 5".to_string(),
                context_id: "ctx_abandon_test".to_string(),
                provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
                abandoned: Some(abandon),
            }),
        )
        .expect("a declared binary runs");
        let elapsed = started.elapsed();
        raiser.join().unwrap();

        let ShellOutcome::Detached(info) = outcome else {
            panic!("an abandoned command must demote, got {outcome:?}");
        };
        assert!(
            elapsed < Duration::from_secs(2),
            "demotion waited {elapsed:?}, as if the 60 s grace still applied"
        );
        assert!(
            registry
                .outstanding()
                .iter()
                .any(|work| work.work_id == info.work_id),
            "the demoted command is registered like one that outran its grace"
        );
    }

    #[test]
    fn a_shared_window_attributes_no_cgroup_delta() {
        assert_eq!(
            attribute_resource_limit(None, Some("cgroup_memory_bytes"), true).as_deref(),
            Some("cgroup_memory_bytes"),
            "a call alone in the scope owns its delta"
        );
        assert_eq!(
            attribute_resource_limit(None, Some("cgroup_memory_bytes"), false),
            None,
            "a delta from a shared window could be a sibling's"
        );
        assert_eq!(
            attribute_resource_limit(Some("cpu_seconds"), Some("cgroup_memory_bytes"), false)
                .as_deref(),
            Some("cpu_seconds"),
            "the signal is per-process evidence and needs no exclusive window"
        );
        assert_eq!(attribute_resource_limit(None, None, true), None);
    }

    /// `MURMUR_RUNTIME_DIR` is runtime-owned like `HOME`: an override, a declared baseline name
    /// and a host value all lose to the session's own runtime directory, and with none there is
    /// no such variable.
    #[test]
    fn build_shell_env_sets_the_runtime_dir_last_whatever_else_says() {
        let workdir = tempfile::tempdir().unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        std::env::set_var(crate::runtime_dir::RUNTIME_DIR_ENV, "/host/value");
        let policy = CapabilityPolicy {
            shell_baseline_env: vec![crate::runtime_dir::RUNTIME_DIR_ENV.to_string()],
            env_allow: vec![crate::runtime_dir::RUNTIME_DIR_ENV.to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = [(
            crate::runtime_dir::RUNTIME_DIR_ENV.to_string(),
            "/override".to_string(),
        )];

        let env = build_shell_env(
            &policy,
            &overrides,
            &root,
            Some(std::path::Path::new("/runtime/dir")),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            env.get(crate::runtime_dir::RUNTIME_DIR_ENV)
                .map(String::as_str),
            Some("/runtime/dir")
        );

        let without = build_shell_env(&policy, &overrides, &root, None, None, None).unwrap();
        assert!(!without.contains_key(crate::runtime_dir::RUNTIME_DIR_ENV));
        assert!(!build_declared_env(&policy).contains_key(crate::runtime_dir::RUNTIME_DIR_ENV));
    }

    /// `MURMUR_RECORDS_DIR` is runtime-owned on `MURMUR_RUNTIME_DIR`'s terms: an override, a
    /// baseline entry, `env.allow` and a host value all lose to the session's own records
    /// directory, and with none there is no such variable.
    #[test]
    fn build_shell_env_sets_the_records_dir_last_whatever_else_says() {
        let workdir = tempfile::tempdir().unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        std::env::set_var(crate::records_dir::RECORDS_DIR_ENV, "/host/records");
        let policy = CapabilityPolicy {
            shell_baseline_env: vec![crate::records_dir::RECORDS_DIR_ENV.to_string()],
            env_allow: vec![crate::records_dir::RECORDS_DIR_ENV.to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = [(
            crate::records_dir::RECORDS_DIR_ENV.to_string(),
            "/override".to_string(),
        )];

        let env = build_shell_env(
            &policy,
            &overrides,
            &root,
            None,
            Some(std::path::Path::new("/records/dir")),
            None,
        )
        .unwrap();
        assert_eq!(
            env.get(crate::records_dir::RECORDS_DIR_ENV)
                .map(String::as_str),
            Some("/records/dir")
        );

        let without = build_shell_env(&policy, &overrides, &root, None, None, None).unwrap();
        assert!(!without.contains_key(crate::records_dir::RECORDS_DIR_ENV));
        assert!(!build_declared_env(&policy).contains_key(crate::records_dir::RECORDS_DIR_ENV));
    }

    /// `TMPDIR`, `TMP` and `TEMP` are runtime-owned: `/tmp` when the session maps it, the
    /// folder's absolute path when not, and absent when the enforcement holds no folder — whatever
    /// the host, an override or `shell.baseline_env` said.
    #[test]
    fn build_shell_env_owns_tmpdir_tmp_and_temp() {
        use crate::temp_dir::{NotMappedReason, TempMapping, TEMP_DIR_ENV};

        let workdir = tempfile::tempdir().unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        let base = tempfile::tempdir().unwrap();
        let roots = crate::sandbox::test_runtime_dir(base.path());
        let folder = roots.temp.as_ref().unwrap().path().to_path_buf();
        for name in TEMP_DIR_ENV {
            std::env::set_var(name, "/host/tmp");
        }
        let policy = CapabilityPolicy {
            shell_baseline_env: TEMP_DIR_ENV.iter().map(|name| name.to_string()).collect(),
            ..CapabilityPolicy::default()
        };
        let overrides: Vec<(String, String)> = TEMP_DIR_ENV
            .iter()
            .map(|name| (name.to_string(), "/override".to_string()))
            .collect();

        let held = |tier, mapping| {
            crate::sandbox::ShellEnforcement {
                tier,
                ..crate::sandbox::ShellEnforcement::environment_only()
            }
            .with_runtime_dir(roots.clone(), None)
            .with_temp_mapping(mapping)
        };
        let folder_text = folder.to_string_lossy().into_owned();
        let cases = [
            (
                held(
                    crate::sandbox::EnforcementTier::KernelFull,
                    TempMapping::MountNamespace,
                ),
                Some("/tmp".to_string()),
            ),
            (
                held(
                    crate::sandbox::EnforcementTier::KernelSealed,
                    TempMapping::ComposedRoot,
                ),
                Some("/tmp".to_string()),
            ),
            (
                held(
                    crate::sandbox::EnforcementTier::KernelFull,
                    TempMapping::NotMapped(NotMappedReason::NoMountNamespace),
                ),
                Some(folder_text.clone()),
            ),
            (
                held(
                    crate::sandbox::EnforcementTier::KernelSeccompOnly,
                    TempMapping::NotMapped(NotMappedReason::NoMountNamespaceNoLandlock),
                ),
                Some(folder_text.clone()),
            ),
            (
                held(
                    crate::sandbox::EnforcementTier::KernelFull,
                    TempMapping::NotMapped(NotMappedReason::WorkdirIsTmp),
                ),
                Some(folder_text.clone()),
            ),
            (
                held(
                    crate::sandbox::EnforcementTier::EnvironmentOnly,
                    TempMapping::NotMapped(NotMappedReason::NoKernelContainment),
                ),
                Some(folder_text.clone()),
            ),
            (crate::sandbox::ShellEnforcement::environment_only(), None),
        ];
        for (enforcement, expected) in cases {
            assert_eq!(
                enforcement.tmpdir_value(),
                expected,
                "{:?}",
                enforcement.tier
            );
            let env = build_shell_env(
                &policy,
                &overrides,
                &root,
                None,
                None,
                enforcement.tmpdir_value().as_deref(),
            )
            .unwrap();
            for name in TEMP_DIR_ENV {
                assert_eq!(
                    env.get(name),
                    expected.as_ref(),
                    "{name} on {:?}",
                    enforcement.tier
                );
            }
        }
    }

    /// The baseline passes no temporary-folder name through from the host.
    #[test]
    fn the_env_baseline_names_no_temp_variable() {
        for name in crate::temp_dir::TEMP_DIR_ENV {
            assert!(!DEFAULT_ENV_BASELINE.contains(&name), "{name}");
        }
    }
}
