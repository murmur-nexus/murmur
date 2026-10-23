use std::{
    collections::BTreeMap,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::{
    artifact_config::ARTIFACT_CONFIG_ENV,
    detached::{DetachPolicy, DetachedDispatchInfo},
    types::CapabilityPolicy,
    workdir_fs::{WorkdirRoot, DEFAULT_FILE_MODE},
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

/// What a shell command, plan shell step or native tool is refused with when its enforcement holds
/// no home folder ([`crate::sandbox::ShellEnforcement::home_root`]).
pub(crate) const NO_HOME_FOLDER_REFUSAL: &str =
    "no home folder: this command's enforcement holds no session home folder (home/ in the \
     session's records directory) to be its $HOME, so it is not run";

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
    /// Whether anything the command started was still holding its output once it exited, and
    /// what became of it.
    pub(crate) output_holders: OutputHolders,
}

/// What held a shell command's stdout or stderr open after its own process exited.
///
/// A command's pipes close only when every process holding a copy has exited or closed it, so a
/// process the command started in the background without redirecting its output keeps the call's
/// output open for as long as it runs. [`finish_output`] waits for the pipes for a bounded time
/// and then decides which of these it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputHolders {
    /// Both pipes reached EOF; nothing was killed.
    Closed,
    /// Processes in the command's process group were still holding its output after
    /// [`PIPE_DRAIN_AFTER_EXIT`] and were killed with `SIGKILL`.
    Killed,
    /// Something outside the command's process group still held its output
    /// [`LEFTOVER_KILL_WAIT`] after the kill. The result has what was read before the call
    /// returned; the holder runs until the session ends and kills it.
    Escaped,
}

impl OutputHolders {
    /// The runtime note telling the model what happened to the holders, or `None` when there
    /// were none. One line of fixed text, so it never grows with the command's output.
    pub(crate) fn note(self) -> Option<String> {
        const REDIRECT: &str = "A process meant to keep running in the background needs its \
            output redirected away from the call, e.g. `server >server.log 2>&1 &`.";
        match self {
            Self::Closed => None,
            Self::Killed => Some(format!(
                "The command exited, but processes it started were still holding its output \
                 after the wait and were killed. {REDIRECT}"
            )),
            Self::Escaped => Some(format!(
                "The command exited, but processes it started outside its process group still \
                 hold its output; the result has what was read before the call returned, and \
                 those processes keep running until the session ends. {REDIRECT}"
            )),
        }
    }
}

/// How long a shell call waits, once the command's own process has exited, for its stdout and
/// stderr to close before it kills what is still holding them.
///
/// Background work that finishes and prints inside this window is returned as part of the
/// call's output.
const PIPE_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);

/// How long a shell call keeps killing its process group and waiting for its pipes to close
/// after [`PIPE_DRAIN_AFTER_EXIT`], before it returns with the pipes still held.
pub(crate) const LEFTOVER_KILL_WAIT: Duration = Duration::from_secs(2);

/// How often [`OutputReaders`] is checked for EOF while a call waits on its pipes.
const PIPE_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
        format!("Run {binary} in the capsule workdir. The `command` field holds {binary}'s arguments and not the binary name. It is split into arguments at whitespace; single or double quotes group words, and a backslash escapes the next character. No shell runs it, so nothing in it is expanded.")
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
/// The command starts as the leader of a new process group, with `/dev/null` as its stdin. Once
/// its own process has exited, the call waits at most [`PIPE_DRAIN_AFTER_EXIT`] — never past the
/// grace deadline — for its stdout and stderr to close, then kills every process still in that
/// group and waits at most [`LEFTOVER_KILL_WAIT`] more; see [`finish_output`]. A background
/// process whose output is redirected away from the call holds neither pipe, so it never triggers
/// the kill, but the kill takes the whole group, it included, when anything else in it does.
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
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
    detach: Option<DetachPolicy>,
) -> Result<ShellOutcome, ShellExecError> {
    use std::os::unix::process::CommandExt as _;

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
        enforcement.home_dir_path(),
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
        // A command in its own process group that read `mur`'s terminal would be stopped by
        // `SIGTTIN`, and under process transport `mur`'s stdin is a protocol pipe.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so what it leaves holding its pipes can be killed without touching
        // anything an earlier call started. std calls `setpgid` before any `pre_exec` closure.
        .process_group(0);

    // Fail-closed: if kernel enforcement setup fails unexpectedly, propagate the error and
    // never call `.spawn()` at all — no code path here lets a Linux host silently run this
    // subprocess with zero enforcement because setup failed. This also installs the hard
    // rlimits and (where a scope exists) cgroup membership for the child.
    let supervisor =
        crate::sandbox::prepare_enforcement(&mut command, enforcement, accessible_workdir.path())?;

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
    // The child is its group's leader, so its pid names the group.
    let pgid = child.id() as i32;

    // `wait_with_output` reads both pipes and waits in one uninterruptible step, so it cannot be
    // given a deadline. Draining each pipe on its own thread separates reading from waiting,
    // which is what lets the wait below be either blocking or polled.
    let readers = OutputReaders::spawn(&mut child);

    // Demotion keeps the output in the records directory and points the agent at it. With no
    // records directory there is nowhere to keep it, so the command runs in the foreground.
    let records_root = enforcement.records_root();
    let detach = detach.filter(|_| records_root.is_some());

    let (status, drain_deadline) = match &detach {
        None => {
            let status = child.wait().map_err(|error| error.to_string())?;
            (status, Instant::now() + PIPE_DRAIN_AFTER_EXIT)
        }
        Some(policy) => {
            let deadline = started + policy.grace;
            loop {
                match child.try_wait().map_err(|error| error.to_string())? {
                    Some(status) => {
                        break (
                            status,
                            (Instant::now() + PIPE_DRAIN_AFTER_EXIT).min(deadline),
                        )
                    }
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
                                    pgid,
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
    let (stdout_bytes, stderr_bytes, output_holders) = finish_output(readers, pgid, drain_deadline);
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    // After `finish_output`, so forks refused while the holders were still running are in the
    // delta.
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
        output_holders,
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
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
) -> Result<ShellResult, ShellExecError> {
    match run_shell(
        binary,
        args,
        env_overrides,
        accessible_workdir,
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
///
/// Each thread appends what it reads to a buffer the call can take at any time, so the call can
/// stop waiting for EOF without joining: a process the command left behind can hold a pipe open
/// for as long as it runs. A demoted command's threads append to its log instead; see
/// [`OutputReaders::go_live`].
struct OutputReaders {
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
}

/// Where one pipe's reading thread puts what it reads.
enum PipeSink {
    /// Kept in memory until the call takes it.
    Buffer(Vec<u8>),
    /// Appended to a demoted command's log as it is read.
    Live(std::sync::Arc<std::sync::Mutex<crate::detached::LiveOutput>>),
    /// Read and dropped: the call has answered. A holder that writes forever neither costs memory
    /// nor gets `SIGPIPE`.
    Discard,
}

/// One pipe's reading thread and where it puts what it reads.
struct PipeReader {
    thread: std::thread::JoinHandle<()>,
    sink: std::sync::Arc<std::sync::Mutex<PipeSink>>,
}

fn lock_sink(sink: &std::sync::Mutex<PipeSink>) -> std::sync::MutexGuard<'_, PipeSink> {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PipeReader {
    fn spawn(mut pipe: impl std::io::Read + Send + 'static) -> Self {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(PipeSink::Buffer(Vec::new())));
        let thread = {
            let sink = std::sync::Arc::clone(&sink);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => match &mut *lock_sink(&sink) {
                            PipeSink::Buffer(buffer) => buffer.extend_from_slice(&chunk[..read]),
                            PipeSink::Live(output) => output
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .append(&chunk[..read]),
                            PipeSink::Discard => {}
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        // A read error loses whatever was not yet read and keeps what was; there
                        // is no better outcome available, and failing the whole call over it
                        // would throw away the command's exit status too.
                        Err(_) => break,
                    }
                }
            })
        };
        Self { thread, sink }
    }

    /// The bytes buffered so far, leaving the thread to discard the rest. Empty for a reader
    /// writing to a log, and for a thread that panicked.
    fn take(self) -> Vec<u8> {
        let taken = match std::mem::replace(&mut *lock_sink(&self.sink), PipeSink::Discard) {
            PipeSink::Buffer(buffer) => buffer,
            PipeSink::Live(_) | PipeSink::Discard => Vec::new(),
        };
        if self.thread.is_finished() && self.thread.join().is_err() {
            return Vec::new();
        }
        taken
    }
}

impl OutputReaders {
    fn spawn(child: &mut std::process::Child) -> Self {
        Self {
            stdout: child.stdout.take().map(PipeReader::spawn),
            stderr: child.stderr.take().map(PipeReader::spawn),
        }
    }

    /// Moves both readers onto `output`: what each has buffered is appended, stdout's then
    /// stderr's, and from then on each appends every chunk as it reads it.
    ///
    /// Both sinks are held across the switch, so no chunk read meanwhile lands ahead of the
    /// buffered bytes.
    fn go_live(&self, output: &std::sync::Arc<std::sync::Mutex<crate::detached::LiveOutput>>) {
        let mut sinks: Vec<_> = [&self.stdout, &self.stderr]
            .into_iter()
            .flatten()
            .map(|reader| lock_sink(&reader.sink))
            .collect();
        let mut live = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for sink in &mut sinks {
            if let PipeSink::Buffer(buffer) =
                std::mem::replace(&mut **sink, PipeSink::Live(std::sync::Arc::clone(output)))
            {
                live.append(&buffer);
            }
        }
    }

    /// Whether both pipes have reached EOF (or failed to read).
    fn at_eof(&self) -> bool {
        [&self.stdout, &self.stderr]
            .into_iter()
            .flatten()
            .all(|reader| reader.thread.is_finished())
    }

    /// Wait until both pipes reach EOF or `deadline` passes, whichever is first. `true` when they
    /// reached EOF.
    fn wait_for_eof(&self, deadline: Instant) -> bool {
        loop {
            if self.at_eof() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            std::thread::sleep(remaining.min(PIPE_POLL_INTERVAL));
        }
    }

    /// `(stdout, stderr)` as buffered so far, without waiting. A thread still reading keeps
    /// reading to EOF and stores and writes nothing more.
    fn take(self) -> (Vec<u8>, Vec<u8>) {
        let take = |reader: Option<PipeReader>| reader.map(PipeReader::take).unwrap_or_default();
        (take(self.stdout), take(self.stderr))
    }
}

/// `(stdout, stderr)` of a command whose own process has exited, and what was left holding them.
///
/// Waits for both pipes until `drain_deadline`. If they are still open then, signals the
/// command's process group `pgid` with `SIGKILL` until the group is gone, and waits for the pipes
/// at most [`LEFTOVER_KILL_WAIT`] more. Signalling stops at the first `kill` that fails: the
/// group's number stays reserved only while the group has members, so once it is gone the number
/// may name an unrelated group. A holder that left the group (`setsid`, `setpgid`) is beyond this
/// kill; its pipe is given up on and the session's end kills it.
fn finish_output(
    readers: OutputReaders,
    pgid: i32,
    drain_deadline: Instant,
) -> (Vec<u8>, Vec<u8>, OutputHolders) {
    if readers.wait_for_eof(drain_deadline) {
        let (stdout, stderr) = readers.take();
        return (stdout, stderr, OutputHolders::Closed);
    }

    let give_up = Instant::now() + LEFTOVER_KILL_WAIT;
    let mut group_alive = true;
    let mut killed = false;
    let closed = loop {
        if group_alive {
            // Refuses a `pgid` of 0 or 1, which would name `mur`'s own group or every process.
            if crate::formation_launch::signal_group(i64::from(pgid), libc::SIGKILL) {
                killed = true;
            } else {
                group_alive = false;
            }
        }
        if readers.at_eof() {
            break true;
        }
        let remaining = give_up.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        std::thread::sleep(remaining.min(PIPE_POLL_INTERVAL));
    };

    let (stdout, stderr) = readers.take();
    let holders = match (closed, killed) {
        (true, true) => OutputHolders::Killed,
        // The pipes closed on their own just after a zero-length window.
        (true, false) => OutputHolders::Closed,
        (false, _) => OutputHolders::Escaped,
    };
    (stdout, stderr, holders)
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
    /// The command's process group, which [`finish_output`] kills if it is still holding the
    /// command's output after it exits.
    pgid: i32,
    cgroup_scope: Option<std::sync::Arc<crate::cgroup::CgroupScope>>,
    cgroup_counters_before: Option<crate::cgroup::CgroupEventCounters>,
    /// The command's stay in the scope, held on the background thread until it exits.
    cgroup_occupancy: Option<crate::cgroup::ScopeOccupancy>,
}

/// Hand a still-running command to a thread of its own and return the handle for it.
///
/// The command's log is opened in the records directory and its readers switched onto it before
/// this returns, so the output read so far and everything after are on disk while it runs. The
/// work is registered before this returns too, so the session-end sweep can never miss work that
/// finished being registered after it looked.
///
/// The thread observes the command's own process exiting with `waitid(WNOWAIT)`, tells the
/// registry, and only then reaps it: a group is signalled only while its leader is unreaped. It
/// then finishes the output the way a foreground call does: [`finish_output`] waits at most
/// [`PIPE_DRAIN_AFTER_EXIT`] for its pipes and then kills what is still holding them, so the
/// completion arrives even when the command left background processes writing to its output.
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

    let arg_refs: Vec<&str> = context.args.iter().map(String::as_str).collect();
    let log = crate::detached::output_path_for(&context.records_root, &work_id);
    let output = std::sync::Arc::new(std::sync::Mutex::new(crate::detached::LiveOutput::open(
        &context.records_root,
        &log.relative,
        &log_command_line(&context.invoked_binary, &arg_refs),
    )));
    readers.go_live(&output);

    context.policy.registry.register(
        crate::detached::DetachedWork {
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
        },
        context.started,
        context.pgid,
        log.path.clone(),
        std::sync::Arc::clone(&output),
    );

    let provenance = context.policy.completion_provenance();
    let DemotionContext {
        policy,
        records_root,
        resolved_binary,
        started,
        pgid,
        cgroup_scope,
        cgroup_counters_before,
        cgroup_occupancy,
        ..
    } = context;

    std::thread::spawn(move || {
        let observed = wait_for_exit_unreaped(pgid);
        policy.registry.leader_exited(&work_id);
        let waited = child.wait();
        let (_, _, output_holders) =
            finish_output(readers, pgid, Instant::now() + PIPE_DRAIN_AFTER_EXIT);
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
        if let (Err(failure), None) = (observed, &error) {
            error = Some(format!("observing the command's exit failed: {failure}"));
        }

        let write_error = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .finish(exit_code);
        if let Some(failure) = write_error {
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
                output_holders,
                context_id: policy.context_id.clone(),
                provenance,
                error,
            });
    });

    info
}

/// Blocks until the child `pid` has exited, leaving it unreaped.
///
/// Until it is reaped the pid stays the child's, so its process group, which it leads, cannot be
/// taken by an unrelated process while anything signals it. `NOWAIT` leaves the child waitable,
/// so `Child::wait` still reaps it afterwards.
fn wait_for_exit_unreaped(pid: i32) -> std::io::Result<()> {
    use rustix::process::{waitid, Pid, WaitId, WaitIdOptions};

    let pid = (pid > 0)
        .then(|| Pid::from_raw(pid))
        .flatten()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
        ) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
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
/// and then the runtime-owned names no entry before them can set — `HOME` and `USERPROFILE` from
/// `home_dir`, `MURMUR_RUNTIME_DIR` and `MURMUR_RECORDS_DIR` from `runtime_dir` and
/// `records_dir`, and `TMPDIR`, `TMP` and `TEMP`, all three `tmpdir` or all three absent.
///
/// `home_dir` is the session's home folder, `home/` in its records directory. `None` refuses the
/// command: no command runs without it, and none is handed a home anywhere else.
pub(crate) fn build_shell_env(
    policy: &CapabilityPolicy,
    env_overrides: &[(String, String)],
    home_dir: Option<&std::path::Path>,
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
    // HOME/USERPROFILE, and so no strip_env pattern can remove the session's.
    let home = home_dir.ok_or_else(|| NO_HOME_FOLDER_REFUSAL.to_string())?;
    let home = home.to_string_lossy().into_owned();
    env.insert("HOME".to_string(), home.clone());
    env.insert("USERPROFILE".to_string(), home);
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

/// The command as a log's `Command:` line names it: the invoked binary and its arguments.
fn log_command_line(binary: &str, args: &[&str]) -> String {
    if args.is_empty() {
        binary.to_string()
    } else {
        format!("{binary} {}", args.join(" "))
    }
}

/// Write a foreground command's full stdout and stderr to its `shell-<ms>` log in the session's
/// records directory, returning the log's absolute path. [`crate::detached::output_path_for`]
/// places it.
///
/// The file holds a `Command:` and an `Exit code:` line, then `Stdout:` and `Stderr:` sections. A
/// demoted command's log has its own layout, written as it runs; see
/// [`crate::detached::LiveOutput`].
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
    let command = log_command_line(binary, args);

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

    use super::test_support::{process_dead_within, read_pid, KillPidFileOnDrop};
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
        let mut enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        enforcement.resource_limits = policy.resources;

        let result = execute_shell(
            "bash",
            &["-c", "ulimit -Hn"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must run");

        assert_eq!(result.stdout.trim(), "64", "stderr was: {}", result.stderr);
        assert_eq!(result.resource_limit_hit, None);
    }

    /// Runs `bash -c script` through `execute_shell` with `limits` both declared and applied.
    #[cfg(target_os = "linux")]
    fn run_bash_with_limits(
        limits: crate::resources::HostResourceLimits,
        script: &str,
    ) -> ShellResult {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            resources: limits,
            ..CapabilityPolicy::default()
        };
        let mut enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        enforcement.resource_limits = limits;

        execute_shell(
            "bash",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must run")
    }

    /// The bound behind `cpu_seconds` is the hard slot: a subshell that ignores `SIGXCPU` keeps
    /// running past `cpu_seconds` and is killed with `SIGKILL` one second later. That kill names
    /// no limit, and the subshell's CPU time, read from bash's `times` (whose second line is the
    /// reaped children's user and system time), stays within `cpu_seconds` plus one second, with
    /// slack for accounting granularity.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_command_that_ignores_sigxcpu_is_killed_one_second_later() {
        let result = run_bash_with_limits(
            crate::resources::HostResourceLimits {
                cpu_seconds: 1,
                ..crate::resources::HostResourceLimits::default()
            },
            "( trap '' XCPU; while :; do :; done ); echo \"status=$?\"; times",
        );

        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        assert!(
            result.stdout.contains("status=137"),
            "the subshell must die of SIGKILL; stdout was: {}",
            result.stdout
        );
        assert_eq!(result.resource_limit_hit, None);

        let children = result
            .stdout
            .lines()
            .last()
            .unwrap_or_else(|| panic!("no children line from `times`: {}", result.stdout));
        let cpu: f64 = children
            .split_whitespace()
            .map(|field| {
                let (minutes, seconds) = field.trim_end_matches('s').split_once('m').unwrap();
                minutes.parse::<f64>().unwrap() * 60.0 + seconds.parse::<f64>().unwrap()
            })
            .sum();
        assert!(
            cpu <= 2.5,
            "the subshell ran {cpu} CPU-seconds, past cpu_seconds + 1"
        );
    }

    /// `RLIMIT_CPU` reaches the child with its soft slot at `cpu_seconds` and its hard slot one
    /// second above, while every other rlimit keeps soft = hard, and an unprivileged child cannot
    /// raise the hard slot.
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(unsafe_code)]
    fn the_cpu_limit_warns_one_second_before_its_hard_ceiling() {
        let result = run_bash_with_limits(
            crate::resources::HostResourceLimits {
                cpu_seconds: 5,
                max_open_files: 64,
                ..crate::resources::HostResourceLimits::default()
            },
            r#"echo "$(ulimit -St) $(ulimit -Ht) $(ulimit -Sn) $(ulimit -Hn)"; ulimit -Ht 10 2>/dev/null || echo "raise refused""#,
        );

        let mut lines = result.stdout.lines();
        assert_eq!(
            lines.next(),
            Some("5 6 64 64"),
            "stderr was: {}",
            result.stderr
        );
        // SAFETY: `geteuid` takes no arguments, touches no memory of the caller and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(lines.next(), Some("raise refused"));
        }
    }

    /// The same ceiling through a kernel tier's `pre_exec` setup while this process holds more
    /// descriptors than the limit: the child inherits them until `execve`, so every descriptor the
    /// setup creates is numbered above 16, and the rlimit must not be in force until that setup
    /// is done.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_kernel_tier_command_starts_under_a_limit_below_the_held_descriptors() {
        const TEST: &str = "a_kernel_tier_command_starts_under_a_limit_below_the_held_descriptors";
        if crate::network_namespace::skip_without_egress_namespace(TEST) {
            return;
        }
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            resources: crate::resources::HostResourceLimits {
                max_open_files: 16,
                ..crate::resources::HostResourceLimits::default()
            },
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement::resolve(
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            crate::sandbox::HostProbe::probe(),
        )
        .unwrap()
        .with_runtime_dir(crate::sandbox::test_runtime_dir(temp.path()), None);
        if enforcement.tier != crate::sandbox::EnforcementTier::KernelFull {
            crate::runtime_err!(
                "[SKIP-HOST] {TEST}: this host resolves {:?}, the test needs KernelFull",
                enforcement.tier
            );
            return;
        }
        let held: Vec<fs::File> = (0..128)
            .map(|_| fs::File::open("/dev/null").unwrap())
            .collect();

        let result = execute_shell(
            "bash",
            &["-c", "ulimit -Hn"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must start under the limit");
        drop(held);

        assert_eq!(result.stdout.trim(), "16", "stderr was: {}", result.stderr);
        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
    }

    /// What MURMUR.md's `## Running Shell Commands` states on a kernel tier: a program outside
    /// `shell.allow` has no Landlock `Execute` rule, so a shell that tries to start it gets
    /// `EACCES` and reports exit status 126. Listing the program is what lets it start.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_program_outside_the_allowlist_is_refused_with_exit_126() {
        const TEST: &str = "a_program_outside_the_allowlist_is_refused_with_exit_126";
        if crate::network_namespace::skip_without_egress_namespace(TEST) {
            return;
        }
        let run = |allow: &[&str]| {
            let temp = tempdir().unwrap();
            let policy = CapabilityPolicy {
                shell_allow: allow.iter().map(|b| b.to_string()).collect(),
                ..CapabilityPolicy::default()
            };
            let enforcement = ShellEnforcement::resolve(
                &policy,
                murmur_artifact::ContainmentClass::Scoped,
                crate::sandbox::HostProbe::probe(),
            )
            .unwrap()
            .with_runtime_dir(crate::sandbox::test_runtime_dir(temp.path()), None);
            if enforcement.tier != crate::sandbox::EnforcementTier::KernelFull {
                crate::runtime_err!(
                    "[SKIP-HOST] {TEST}: this host resolves {:?}, the test needs KernelFull",
                    enforcement.tier
                );
                return None;
            }
            let result = execute_shell(
                "bash",
                &["-c", "cat /dev/null"],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
                &policy,
                &enforcement,
            )
            .expect("bash must start");
            Some(result)
        };

        let Some(refused) = run(&["bash"]) else {
            return;
        };
        assert_eq!(refused.exit_code, 126, "stderr was: {}", refused.stderr);
        assert!(
            refused.stderr.contains("Permission denied"),
            "stderr was: {}",
            refused.stderr
        );

        let allowed = run(&["bash", "cat"]).expect("the tier did not change between runs");
        assert_eq!(allowed.exit_code, 0, "stderr was: {}", allowed.stderr);
        crate::runtime_err!("{TEST}: ran on KernelFull; [bash] exited 126, [bash, cat] exited 0");
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
        let mut enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
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

    /// `$HOME` is the session's home folder, `home/` in its records directory, and the command
    /// runs in the work folder, which gains nothing.
    ///
    /// Asserted through a real subprocess rather than through `build_shell_env` alone, because
    /// `$HOME` naming a directory that does not exist is exactly the failure a wrong root would
    /// produce and a pure env check would not see.
    #[test]
    fn home_folder_is_home_and_the_work_folder_gains_nothing() {
        let accessible = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let enforcement = ShellEnforcement::environment_only()
            .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None);
        let home = enforcement.home_dir_path().unwrap().to_path_buf();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &[
                "-c",
                "echo marker > \"$HOME/probe\" && cat \"$HOME/probe\" && echo \"$HOME\" && pwd",
            ],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(accessible.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must run");

        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        let mut lines = result.stdout.lines();
        assert_eq!(lines.next(), Some("marker"));
        assert_eq!(lines.next(), Some(home.to_str().unwrap()));
        assert_eq!(
            std::fs::canonicalize(lines.next().unwrap()).unwrap(),
            std::fs::canonicalize(accessible.path()).unwrap(),
            "the subprocess cwd is the work folder"
        );
        assert_eq!(fs::read_to_string(home.join("probe")).unwrap(), "marker\n");
        assert_eq!(
            fs::read_dir(accessible.path()).unwrap().count(),
            0,
            "the work folder gains nothing"
        );
    }

    /// A command printing `seq 1 <last>`, past [`MAX_SHELL_OUTPUT_BYTES`] for any `last` over
    /// 4,000, run in `workdir`.
    fn print_seq(workdir: &Path, enforcement: &ShellEnforcement, last: u32) -> ShellResult {
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

        let result = print_seq(workdir.path(), &enforcement, 20_000);

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
    }

    /// Roots built without a records directory, holding only a home folder, give a truncated
    /// command no log to write: it writes none in the work folder and names no path.
    #[test]
    fn with_no_records_directory_a_truncated_command_writes_no_log() {
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
            )
            .with_home(open(crate::records_dir::HOME_SUBDIR)),
            None,
        );

        let enforcement = &roots_without_records;
        let workdir = tempdir().unwrap();
        let result = print_seq(workdir.path(), enforcement, 20_000);
        assert!(result.truncated);
        assert_eq!(result.stdout.len(), MAX_SHELL_OUTPUT_BYTES);
        assert_eq!(result.full_output_path, None);
        assert_eq!(
            fs::read_dir(workdir.path()).unwrap().count(),
            0,
            "nothing is written into the workdir"
        );
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
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
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
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
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
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
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
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
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
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
        )
        .unwrap();

        assert_eq!(result.exit_code, 7, "the subprocess must still have run");
        assert_eq!(
            result.binary, "./local-tool",
            "an unresolvable name degrades to the bare invoked name, never an error"
        );
    }

    #[test]
    fn build_shell_env_sets_home_to_the_home_folder() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        let env = build_shell_env(&policy, &[], Some(temp.path()), None, None, None).unwrap();

        let expected_home = temp.path().to_string_lossy().into_owned();
        assert_eq!(env.get("HOME"), Some(&expected_home));
        assert_eq!(env.get("USERPROFILE"), Some(&expected_home));
    }

    #[test]
    fn no_home_folder_build_shell_env_refuses() {
        let error =
            build_shell_env(&CapabilityPolicy::default(), &[], None, None, None, None).unwrap_err();
        assert_eq!(error, NO_HOME_FOLDER_REFUSAL);
        assert!(error.contains("home folder"), "{error}");
    }

    /// A command whose enforcement holds no home folder — no runtime directory, or roots without
    /// one — is refused before anything is spawned, and is handed no home anywhere else: nothing
    /// is created in the work folder or beside it.
    #[test]
    fn no_home_folder_execute_shell_refuses_and_creates_nothing() {
        let parent = tempdir().unwrap();
        let workdir = parent.path().join("work");
        fs::create_dir(&workdir).unwrap();
        let runtime = tempdir().unwrap();
        let roots = crate::sandbox::test_runtime_dir(runtime.path());
        let without_home = crate::runtime_dir::RuntimeDirRoots {
            home: None,
            ..roots
        };
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        for enforcement in [
            ShellEnforcement::environment_only(),
            ShellEnforcement::environment_only().with_runtime_dir(without_home.clone(), None),
        ] {
            let error = execute_shell(
                "bash",
                &["-c", "touch ran"],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
                &policy,
                &enforcement,
            )
            .unwrap_err();
            assert!(error.to_string().contains("no home folder"), "{error}");
        }
        assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1);
    }

    #[test]
    fn execute_shell_reports_the_home_folder_not_real_host_home() {
        let temp = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let enforcement = ShellEnforcement::environment_only()
            .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None);
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "echo -n $HOME"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .unwrap();

        let expected_home = runtime.path().join("records/home");
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

        let env =
            build_shell_env(&policy, &overrides, Some(temp.path()), None, None, None).unwrap();

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
        let shell = build_shell_env(&policy, &[], Some(temp.path()), None, None, None).unwrap();
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
        let shell = build_shell_env(&policy, &[], Some(temp.path()), None, None, None).unwrap();
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

        let env =
            build_shell_env(&policy, &overrides, Some(temp.path()), None, None, None).unwrap();

        assert_eq!(
            env.get("HOME"),
            Some(&temp.path().to_string_lossy().into_owned())
        );
    }

    #[test]
    fn build_shell_env_keeps_safe_baseline_vars() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        std::env::set_var("CARGO_HOME", "/fake/cargo/home");
        let env = build_shell_env(&policy, &[], Some(temp.path()), None, None, None).unwrap();
        std::env::remove_var("CARGO_HOME");

        assert_eq!(env.get("CARGO_HOME"), Some(&"/fake/cargo/home".to_string()));
    }

    /// A command names the account it runs as with the host's own pair, beside the session's
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

        let env = build_shell_env(
            &CapabilityPolicy::default(),
            &[],
            Some(root.path()),
            None,
            None,
            None,
        )
        .unwrap();
        let stripped = build_shell_env(
            &CapabilityPolicy {
                shell_strip_env: vec!["LOGNAME".to_string()],
                ..CapabilityPolicy::default()
            },
            &[],
            Some(root.path()),
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
        assert_eq!(
            env.get("HOME"),
            Some(&temp.path().to_string_lossy().into_owned())
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

        let env =
            build_shell_env(&policy, &overrides, Some(temp.path()), None, None, None).unwrap();

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

    /// A direct binary's description is the rule alone, a pure function of the name: it sits in
    /// the tool array of every request, so it carries no example and no per-launch input.
    #[test]
    fn a_direct_binary_description_states_the_argument_rule_and_nothing_else() {
        for binary in ["curl", "git", "jq", "python3"] {
            let yaml = shell_tool_manifest_yaml(binary);
            let value: serde_norway::Value = serde_norway::from_str(&yaml)
                .unwrap_or_else(|e| panic!("{binary} manifest must be valid YAML: {e}"));
            let description = value
                .get("description")
                .and_then(|v| v.as_str())
                .expect("the manifest has a description");
            assert_eq!(
                description,
                format!(
                    "Run {binary} in the capsule workdir. The `command` field holds {binary}'s \
                     arguments and not the binary name. It is split into arguments at whitespace; \
                     single or double quotes group words, and a backslash escapes the next \
                     character. No shell runs it, so nothing in it is expanded."
                )
            );
            assert!(!description.contains("example.com"), "{description}");
            assert!(!description.contains(" -s "), "{description}");
            assert_eq!(yaml, shell_tool_manifest_yaml(binary));
        }
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

    /// `run` on a thread of its own, failing the test after 30 s instead of hanging it.
    fn returns_within_30_s<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(run());
        });
        receiver
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("the shell call did not return within 30 s"))
    }

    /// A command that exits while a process it started in the background still holds its stdout
    /// returns well inside its grace period, and the holder is killed.
    #[test]
    fn a_background_holder_does_not_hold_a_graced_call() {
        use std::sync::Arc;

        use crate::{
            detached::{DetachPolicy, DetachedRegistry},
            origin::{TaskOrigin, TaskProvenance},
        };

        let temp = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let _sleeper = KillPidFileOnDrop(temp.path().join("sleeper.pid"));
        let workdir = temp.path().to_path_buf();
        let runtime_base = runtime.path().to_path_buf();
        let script = "sleep 300 & echo $! > sleeper.pid; echo started";

        let started = Instant::now();
        let outcome = returns_within_30_s(move || {
            let (registry, _reports) = DetachedRegistry::new();
            run_shell(
                "bash",
                &["-c", script],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
                &CapabilityPolicy {
                    shell_allow: vec!["bash".to_string()],
                    ..CapabilityPolicy::default()
                },
                &ShellEnforcement::environment_only()
                    .with_runtime_dir(crate::sandbox::test_runtime_dir(&runtime_base), None),
                Some(DetachPolicy {
                    grace: Duration::from_secs(5),
                    delivered: true,
                    registry: Arc::clone(&registry),
                    command: script.to_string(),
                    context_id: "ctx_holder_test".to_string(),
                    provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
                    abandoned: None,
                }),
            )
            .expect("a declared binary runs")
        });
        let elapsed = started.elapsed();

        let ShellOutcome::Finished(result) = outcome else {
            panic!("a command that exits at once must finish, got {outcome:?}");
        };
        assert!(
            elapsed < Duration::from_secs(5),
            "the call took {elapsed:?}, past its 5 s grace"
        );
        assert!(result.stdout.contains("started"), "{result:?}");
        assert_eq!(result.exit_code, 0, "{result:?}");
        let sleeper = read_pid(&temp.path().join("sleeper.pid"));
        assert!(
            process_dead_within(sleeper, Duration::from_secs(2)),
            "the background sleeper {sleeper} is still running"
        );
    }

    /// The same holder on the path with no grace period, which plan shell steps and the
    /// script-capsule store state take.
    #[test]
    fn a_background_holder_does_not_hold_an_ungraced_call() {
        let temp = tempdir().unwrap();
        let _sleeper = KillPidFileOnDrop(temp.path().join("sleeper.pid"));
        let workdir = temp.path().to_path_buf();

        let started = Instant::now();
        let result = returns_within_30_s(move || {
            execute_shell(
                "bash",
                &["-c", "sleep 300 & echo $! > sleeper.pid; echo started"],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
                &CapabilityPolicy {
                    shell_allow: vec!["bash".to_string()],
                    ..CapabilityPolicy::default()
                },
                &ShellEnforcement::environment_only().with_test_session_dirs(),
            )
            .expect("a declared binary runs")
        });
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "the call took {elapsed:?}"
        );
        assert!(result.stdout.contains("started"), "{result:?}");
        assert_eq!(result.exit_code, 0, "{result:?}");
        let sleeper = read_pid(&temp.path().join("sleeper.pid"));
        assert!(
            process_dead_within(sleeper, Duration::from_secs(2)),
            "the background sleeper {sleeper} is still running"
        );
        assert_eq!(result.output_holders, OutputHolders::Killed);
    }

    /// Background work that prints and exits inside the drain window is part of the output, and
    /// nothing is killed or reported.
    #[test]
    fn output_written_just_after_exit_is_kept() {
        use std::sync::Arc;

        use crate::{
            detached::{DetachPolicy, DetachedRegistry},
            origin::{TaskOrigin, TaskProvenance},
        };

        let temp = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let (registry, _reports) = DetachedRegistry::new();
        let script = "(sleep 0.3; echo late) & echo early";

        let outcome = run_shell(
            "bash",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &CapabilityPolicy {
                shell_allow: vec!["bash".to_string()],
                ..CapabilityPolicy::default()
            },
            &ShellEnforcement::environment_only()
                .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None),
            Some(DetachPolicy {
                grace: Duration::from_secs(5),
                delivered: true,
                registry: Arc::clone(&registry),
                command: script.to_string(),
                context_id: "ctx_late_output_test".to_string(),
                provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
                abandoned: None,
            }),
        )
        .expect("a declared binary runs");

        let ShellOutcome::Finished(result) = outcome else {
            panic!("a command that exits at once must finish, got {outcome:?}");
        };
        assert!(result.stdout.contains("early"), "{result:?}");
        assert!(result.stdout.contains("late"), "{result:?}");
        assert_eq!(result.output_holders, OutputHolders::Closed);
        assert!(OutputHolders::Closed.note().is_none());
    }

    /// A holder that left the command's process group cannot be killed by the call. The call
    /// returns anyway, with what it read, and says the holder is still running.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_holder_outside_the_process_group_is_reported_not_waited_for() {
        use std::sync::Arc;

        use crate::{
            detached::{DetachPolicy, DetachedRegistry},
            origin::{TaskOrigin, TaskProvenance},
        };

        let temp = tempdir().unwrap();
        let runtime = tempdir().unwrap();
        let pid_file = temp.path().join("escaped.pid");
        let escaped_guard = KillPidFileOnDrop(pid_file.clone());
        let workdir = temp.path().to_path_buf();
        let runtime_base = runtime.path().to_path_buf();
        let script = "setsid sleep 300 & echo $! > escaped.pid; echo started";

        let started = Instant::now();
        let outcome = returns_within_30_s(move || {
            let (registry, _reports) = DetachedRegistry::new();
            run_shell(
                "bash",
                &["-c", script],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(&workdir).unwrap(),
                &CapabilityPolicy {
                    shell_allow: vec!["bash".to_string()],
                    ..CapabilityPolicy::default()
                },
                &ShellEnforcement::environment_only()
                    .with_runtime_dir(crate::sandbox::test_runtime_dir(&runtime_base), None),
                Some(DetachPolicy {
                    grace: Duration::from_secs(5),
                    delivered: true,
                    registry: Arc::clone(&registry),
                    command: script.to_string(),
                    context_id: "ctx_escaped_test".to_string(),
                    provenance: Some(TaskProvenance::derive(TaskOrigin::User, None)),
                    abandoned: None,
                }),
            )
            .expect("a declared binary runs")
        });
        let elapsed = started.elapsed();

        let ShellOutcome::Finished(result) = outcome else {
            panic!("a command that exits at once must finish, got {outcome:?}");
        };
        let bound = Duration::from_secs(5) + LEFTOVER_KILL_WAIT + Duration::from_secs(2);
        assert!(elapsed < bound, "the call took {elapsed:?}, past {bound:?}");
        assert_eq!(result.stdout.trim(), "started", "{result:?}");
        assert_eq!(result.output_holders, OutputHolders::Escaped);
        let escaped = read_pid(&pid_file);
        assert!(
            !process_dead_within(escaped, Duration::ZERO),
            "a holder outside the process group is not the call's to kill"
        );
        drop(escaped_guard);
        assert!(process_dead_within(escaped, Duration::from_secs(2)));
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
            Some(root.path()),
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

        let without =
            build_shell_env(&policy, &overrides, Some(root.path()), None, None, None).unwrap();
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
            Some(root.path()),
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

        let without =
            build_shell_env(&policy, &overrides, Some(root.path()), None, None, None).unwrap();
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
                Some(root.path()),
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

/// Helpers for tests whose command leaves a process behind and writes its pid to a file.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The pid the test's command wrote to `pid_file`.
    pub(crate) fn read_pid(pid_file: &Path) -> u32 {
        std::fs::read_to_string(pid_file)
            .expect("the command wrote its pid file")
            .trim()
            .parse()
            .expect("the pid file holds a pid")
    }

    /// Whether `pid` has exited within `timeout`: it is gone, or a zombie whichever process it
    /// was orphaned to has not reaped yet.
    pub(crate) fn process_dead_within(pid: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !crate::running::pid_is_alive(pid) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the process whose pid the test's command wrote to the file, if it is still running
    /// when the guard drops, so a failing test leaves nothing behind. It reads the file at drop,
    /// so it also covers a test that panicked before reading it.
    pub(crate) struct KillPidFileOnDrop(pub(crate) PathBuf);

    impl Drop for KillPidFileOnDrop {
        fn drop(&mut self) {
            let Some(pid) = std::fs::read_to_string(&self.0)
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
                .filter(|pid| *pid > 0)
            else {
                return;
            };
            if !process_dead_within(pid.unsigned_abs(), Duration::ZERO) {
                // SAFETY: `kill` takes two integers and dereferences nothing. `pid` is positive,
                // so it names one process, never a group or every process.
                #[allow(unsafe_code)]
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}
