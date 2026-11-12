//! Programs a WASM tool runs through `murmur:process/process@0.2.0`, under the rules a shell
//! command obeys.
//!
//! A tool's store lives for one call, so nothing a tool starts can be held in it. A session's
//! processes live in [`SessionProcesses`], created once per agent session and held on the capsule
//! store; each tool call's store carries a [`ProcessAccess`] naming the registry, the calling
//! tool, the call's deadline and the route to the session's decision point.
//!
//! Every program is admitted in one order, the shell's: `capabilities.shell.allow`, then the
//! decision point (the `capabilities.filesystem.read_only` check and every `on-shell` deny hook,
//! asked on the exact argv that will run), then [`crate::shell::spawn_contained`] — the workdir
//! budget, the sanitized environment, the session's containment tier, cgroup scope and network
//! namespace, and the spawn.

use std::collections::{HashMap, VecDeque};
use std::io::{Read as _, Write as _};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use wasmtime::component::{HasData, Resource, ResourceTable};

use crate::bindings::tool_host::murmur::process::process as wit;
use crate::hooks::ResolvedCall;
use crate::sandbox::{EnforcementTier, ShellEnforcement};
use crate::shell::{ContainedChild, ContainedSpawnError, ShellExecError};
use crate::trace::ResourceTraceAppender;
use crate::types::CapabilityPolicy;
use crate::workdir_fs::WorkdirRoot;

/// The instance name the host registers on every WASM tool's linker.
pub(crate) const PROCESS_IFACE: &str = "murmur:process/process@0.2.0";

/// Live session processes per session, all tools together. An exited process does not count.
pub(crate) const MAX_SESSION_PROCESSES: usize = 8;

/// The bound on each output stream: what a `run` returns of each, and what a session process keeps
/// of each between two `read`s.
pub(crate) const MAX_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;

/// The longest process name a tool may choose.
pub(crate) const MAX_PROCESS_NAME_CHARS: usize = 64;

/// What prefixes every session process handle.
pub(crate) const HANDLE_PREFIX: &str = "prc_";

/// How long the session's end waits for the reapers of the processes it killed to record them.
const SESSION_END_REAP_WAIT: Duration = Duration::from_secs(3);

/// How long a session process's reaper waits, after the leader exited and its group was killed,
/// for both output pumps to reach end-of-file before it reports the exit anyway.
///
/// Killing the group closes every write end its members held, so end-of-file normally arrives as
/// soon as the pump reads the last chunk. Only a program that left the process group (`setsid`)
/// and still holds stdout or stderr open keeps a pipe from ending; this bounds that case, and that
/// stream's `-ended` stays false until the holder closes it.
const OUTPUT_DRAIN_LIMIT: Duration = Duration::from_secs(2);

/// How long a finished `run` keeps reading output a killed group's pipes still hold.
const RUN_DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);

/// How long `connect` waits for the TCP handshake with a loopback listener.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The closed vocabulary of rules a refusal names, with [`crate::agent::PROTECTED_PATH_RULE`].
pub(crate) mod rule {
    pub(crate) const SHELL_ALLOW: &str = "capabilities.shell.allow";
    pub(crate) const ON_SHELL: &str = "on-shell";
    pub(crate) const WORKDIR_BUDGET: &str = "capabilities.resources.max_workdir_bytes";
    pub(crate) const CAP: &str = "session-process-cap";
    pub(crate) const NAME: &str = "name";
    pub(crate) const CONNECT: &str = "connect";
    pub(crate) const UNAVAILABLE: &str = "unavailable";
    pub(crate) const SESSION_ENDED: &str = "session-ended";
}

/// The reason given when a session that gates calls has no route to its decision point.
const NO_DECISION_ROUTE: &str = "this session puts every program to its decision point (a policy \
    hook bound to on-shell or a capabilities.filesystem.read_only declaration), and no decision \
    point is reachable from this dispatch";

/// The reason given when the decision point stopped answering.
const DECISION_UNREACHABLE: &str =
    "the session stopped answering policy checks, so the program was not started";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refused(rule: &str, reason: impl Into<String>) -> wit::ProcessError {
    wit::ProcessError::Refused(wit::Refusal {
        rule: rule.to_string(),
        reason: reason.into(),
    })
}

// ---------------------------------------------------------------- the decision point

/// A refusal at the session's decision point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessRefusal {
    pub(crate) rule: &'static str,
    pub(crate) reason: String,
}

/// One program put to the session's decision point, on the argv it will run.
pub(crate) struct ProcessGateRequest {
    pub(crate) resolved: ResolvedCall,
    /// `None` lets the program start.
    pub(crate) reply: oneshot::Sender<Option<ProcessRefusal>>,
}

/// Hands a request to whoever answers the session's decision point; `false` when it cannot.
pub(crate) type GateSender = Arc<dyn Fn(ProcessGateRequest) -> bool + Send + Sync>;

/// How a tool call reaches the session's decision point.
#[derive(Clone)]
pub(crate) enum GateRoute {
    /// The session has no policy hook and no `read_only` declaration: nothing can refuse.
    Ungated,
    /// The dispatch's own decision point answers on this.
    Route(GateSender),
    /// The session gates calls and this dispatch has no way to ask: every program is refused.
    Unreachable,
}

impl GateRoute {
    async fn ask(&self, resolved: ResolvedCall) -> Option<ProcessRefusal> {
        let send = match self {
            Self::Ungated => return None,
            Self::Unreachable => {
                return Some(ProcessRefusal {
                    rule: rule::ON_SHELL,
                    reason: NO_DECISION_ROUTE.to_string(),
                })
            }
            Self::Route(send) => send,
        };
        let unreachable = || ProcessRefusal {
            rule: rule::ON_SHELL,
            reason: DECISION_UNREACHABLE.to_string(),
        };
        let (reply, verdict) = oneshot::channel();
        if !send(ProcessGateRequest { resolved, reply }) {
            return Some(unreachable());
        }
        verdict.await.unwrap_or_else(|_| Some(unreachable()))
    }
}

/// What the decision point is shown for a program: the shell call it is, built from the argv
/// that will be spawned and nothing else.
pub(crate) fn resolve_process_call(
    program: &str,
    args: &[String],
    workdir: &WorkdirRoot,
) -> ResolvedCall {
    ResolvedCall::shell_from_argv(program, posix_command_line(args), args, workdir)
}

/// `args` as one POSIX shell line: each argument as it is when it needs no quoting, else
/// single-quoted.
pub(crate) fn posix_command_line(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            let plain = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c));
            if plain {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `name` is a process name a tool may choose.
pub(crate) fn valid_process_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_PROCESS_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

// ---------------------------------------------------------------- per-call access

/// What a tool call's store holds of the session's processes.
pub(crate) enum ProcessAccess {
    Available(ProcessCallCtx),
    /// Every function refuses with rule `unavailable` and this reason.
    Unavailable(&'static str),
    /// In a guest worker: every function is answered by the supervisor, which holds the call's
    /// own access.
    Remote(Arc<crate::guest_worker::RemoteLink>),
}

/// The reasons a store holds no process context.
pub(crate) mod unavailable {
    pub(crate) const DRIVER: &str = "murmur:process is served to an agent capsule's tool calls; \
        this is an inference driver's call";
    pub(crate) const NOT_AGENT_TOOL_CALL: &str = "murmur:process is served to an agent \
        capsule's tool calls; this call was not dispatched by an agent's tool loop";
    pub(crate) const NO_REGISTRY: &str = "murmur:process is served to an agent capsule's tool \
        calls; this session holds no process registry";
}

/// One tool call's view of the session's processes.
pub(crate) struct ProcessCallCtx {
    registry: Arc<SessionProcesses>,
    tool: String,
    task_id: Option<String>,
    /// When this call's wall-clock deadline ends; set as `run` starts.
    deadline: Option<Instant>,
    gate: GateRoute,
}

impl ProcessCallCtx {
    pub(crate) fn new(
        registry: Arc<SessionProcesses>,
        tool: &str,
        task_id: Option<String>,
        gate: GateRoute,
    ) -> Self {
        Self {
            registry,
            tool: tool.to_string(),
            task_id,
            deadline: None,
            gate,
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.map_or(
            Duration::from_secs(crate::limits::DEFAULT_DEADLINE_SECONDS),
            |deadline| deadline.saturating_duration_since(Instant::now()),
        )
    }
}

impl ProcessAccess {
    /// Starts this call's deadline clock: `deadline` from now, the call's
    /// `capabilities.limits.deadline_seconds`.
    pub(crate) fn begin_call(&mut self, deadline: Duration) {
        match self {
            Self::Available(ctx) => ctx.deadline = Some(Instant::now() + deadline),
            Self::Remote(link) => link.run_started(deadline),
            Self::Unavailable(_) => {}
        }
    }

    fn ctx(&self) -> Result<&ProcessCallCtx, wit::ProcessError> {
        match self {
            Self::Available(ctx) => Ok(ctx),
            Self::Unavailable(reason) => Err(refused(rule::UNAVAILABLE, *reason)),
            Self::Remote(_) => Err(wit::ProcessError::Failed(
                "murmur:process is answered by the supervisor".to_string(),
            )),
        }
    }

    fn remote(&self) -> Option<&Arc<crate::guest_worker::RemoteLink>> {
        match self {
            Self::Remote(link) => Some(link),
            _ => None,
        }
    }
}

/// The host-side binding of `murmur:process` over one tool store.
pub(crate) struct ProcessView<'a> {
    pub(crate) access: &'a ProcessAccess,
    pub(crate) table: &'a mut ResourceTable,
}

/// The [`HasData`] marker `add_to_linker` is instantiated with.
pub(crate) struct ProcessHost;

impl HasData for ProcessHost {
    type Data<'a> = ProcessView<'a>;
}

// ---------------------------------------------------------------- the session registry

/// Where a session process is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Reserved under its name; admission and the spawn are under way.
    Starting,
    Running,
    Exited(i32),
    /// Its start was refused or failed; the reservation is gone.
    Abandoned,
}

/// The newest [`MAX_PROCESS_OUTPUT_BYTES`] of one stream, what was pushed out since the last
/// read, and whether the stream's pipe has reached end-of-file.
#[derive(Default)]
struct OutputRing {
    bytes: VecDeque<u8>,
    dropped: u64,
    /// Set by the pump, under this ring's lock, after its last push.
    eof: bool,
}

impl OutputRing {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        let over = self.bytes.len().saturating_sub(MAX_PROCESS_OUTPUT_BYTES);
        if over > 0 {
            self.bytes.drain(..over);
            self.dropped += over as u64;
        }
    }

    fn take(&mut self, max: usize) -> (Vec<u8>, u64) {
        let count = max.min(self.bytes.len());
        let taken = self.bytes.drain(..count).collect();
        (taken, std::mem::take(&mut self.dropped))
    }

    fn mark_end_of_file(&mut self) {
        self.eof = true;
    }

    /// The pipe reached end-of-file and every byte it delivered has been taken: no later take
    /// returns anything.
    fn ended(&self) -> bool {
        self.eof && self.bytes.is_empty()
    }
}

/// One process a tool started for the session.
struct SessionProcess {
    handle: String,
    tool: String,
    name: String,
    task_id: Option<String>,
    binary: Mutex<String>,
    phase: watch::Sender<Phase>,
    /// The group leader's pid, which names its group; `0` until spawned.
    pid: AtomicI32,
    signals: GroupSignals,
    started: Mutex<Instant>,
    stdin: Arc<Mutex<Option<std::process::ChildStdin>>>,
    stdout: Arc<Mutex<OutputRing>>,
    stderr: Arc<Mutex<OutputRing>>,
    /// What the `process_exit` line will say: `exited` unless a `stop` or the session's end killed
    /// it.
    exit_reason: Mutex<&'static str>,
    exit_traced: AtomicBool,
}

impl SessionProcess {
    fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    fn live(&self) -> bool {
        matches!(self.phase(), Phase::Starting | Phase::Running)
    }

    fn state(&self) -> wit::State {
        match self.phase() {
            Phase::Exited(code) => wit::State::Exited(code),
            _ => wit::State::Running,
        }
    }

    fn pgid(&self) -> i32 {
        self.pid.load(Ordering::SeqCst)
    }

    fn signal(&self, signal: libc::c_int) {
        if self.phase() == Phase::Running {
            self.signals.signal(self.pgid(), signal);
        }
    }

    /// Waits until the process has exited and its exit is recorded, or `limit` passes.
    async fn wait_exit(&self, limit: Duration) -> bool {
        let mut phase = self.phase.subscribe();
        let exited = tokio::time::timeout(
            limit,
            phase.wait_for(|phase| matches!(phase, Phase::Exited(_) | Phase::Abandoned)),
        )
        .await
        .is_ok();
        exited
    }
}

/// What reserving a name under the registry lock found.
enum Reservation {
    Reserved(Arc<SessionProcess>),
    Running(String),
    Concurrent(Arc<SessionProcess>),
    Full,
    Ended,
}

#[derive(Default)]
struct RegistryState {
    ended: bool,
    /// The latest process each `(tool, name)` started.
    by_name: HashMap<(String, String), Arc<SessionProcess>>,
    /// Every process the session started, by handle.
    by_handle: HashMap<String, Arc<SessionProcess>>,
}

impl RegistryState {
    fn live_count(&self) -> usize {
        self.by_name
            .values()
            .filter(|process| process.live())
            .count()
    }
}

/// A session's processes, and everything a program they run is contained by.
pub(crate) struct SessionProcesses {
    enforcement: ShellEnforcement,
    policy: CapabilityPolicy,
    workdir: WorkdirRoot,
    env_overrides: Vec<(String, String)>,
    trace: Option<Arc<ResourceTraceAppender>>,
    /// Whether the session's hook runtime gates calls. Fail-closed until the agent loop says:
    /// a dispatch with no route to the decision point refuses every program while this is set.
    hooks_gate_calls: AtomicBool,
    state: Mutex<RegistryState>,
    /// A `sealed` composed root that failed to build for a program, for the dispatch to end the
    /// session with as the shell does.
    sealed_failure: Mutex<Option<String>>,
}

/// What a `run` or `start` refusal is traced as.
struct RefusalRecord<'a> {
    tool: &'a str,
    task_id: Option<String>,
    kind: &'static str,
    name: Option<&'a str>,
    binary: Option<String>,
    port: Option<u16>,
}

impl SessionProcesses {
    /// One session's registry. `env_overrides` is what every shell command of the session is given
    /// ([`crate::runtime::subprocess_guest_env`] of the session's environment); `trace` is the
    /// session's concurrent trace appender.
    pub(crate) fn new(
        enforcement: ShellEnforcement,
        policy: CapabilityPolicy,
        workdir: WorkdirRoot,
        env_overrides: Vec<(String, String)>,
        trace: Option<Arc<ResourceTraceAppender>>,
    ) -> Self {
        Self {
            enforcement,
            policy,
            workdir,
            env_overrides,
            trace,
            hooks_gate_calls: AtomicBool::new(true),
            state: Mutex::new(RegistryState::default()),
            sealed_failure: Mutex::new(None),
        }
    }

    /// Records whether the session's hook runtime gates calls (`HookRuntime::gates_calls`).
    pub(crate) fn set_hooks_gate_calls(&self, gates: bool) {
        self.hooks_gate_calls.store(gates, Ordering::SeqCst);
    }

    /// The route for a dispatch that holds no decision point of its own.
    pub(crate) fn route_without_gate(&self, has_protected_paths: bool) -> GateRoute {
        if has_protected_paths || self.hooks_gate_calls.load(Ordering::SeqCst) {
            GateRoute::Unreachable
        } else {
            GateRoute::Ungated
        }
    }

    /// The `E-RUN-014` detail of a sealed root that failed to build for a program since the last
    /// call, which ends the session.
    pub(crate) fn take_sealed_failure(&self) -> Option<String> {
        lock(&self.sealed_failure).take()
    }

    fn ended(&self) -> bool {
        lock(&self.state).ended
    }

    fn lookup(&self, tool: &str, handle: &str) -> Result<Arc<SessionProcess>, wit::ProcessError> {
        lock(&self.state)
            .by_handle
            .get(handle)
            .filter(|process| process.tool == tool && process.phase() != Phase::Abandoned)
            .cloned()
            .ok_or(wit::ProcessError::UnknownHandle)
    }

    fn find(&self, tool: &str, name: &str) -> Option<String> {
        lock(&self.state)
            .by_name
            .get(&(tool.to_string(), name.to_string()))
            .filter(|process| matches!(process.phase(), Phase::Running | Phase::Exited(_)))
            .map(|process| process.handle.clone())
    }

    async fn trace_refusal(&self, record: RefusalRecord<'_>, refusal: &ProcessRefusal) {
        if let Some(trace) = &self.trace {
            trace
                .write_process_refused(crate::trace::ProcessRefusedLine {
                    task_id: record.task_id,
                    tool: record.tool,
                    kind: record.kind,
                    name: record.name,
                    binary: record.binary.as_deref(),
                    port: record.port,
                    rule: refusal.rule,
                    reason: &refusal.reason,
                })
                .await;
        }
    }

    async fn refuse(
        &self,
        record: RefusalRecord<'_>,
        rule: &'static str,
        reason: String,
    ) -> wit::ProcessError {
        let refusal = ProcessRefusal { rule, reason };
        self.trace_refusal(record, &refusal).await;
        refused(refusal.rule, refusal.reason)
    }

    /// The shared admission of `run` and `start`: `capabilities.shell.allow`, then the decision
    /// point. Returns the refusal, traced, or nothing.
    async fn admit(
        &self,
        ctx: &ProcessCallCtx,
        kind: &'static str,
        name: Option<&str>,
        program: &str,
        args: &[String],
    ) -> Option<wit::ProcessError> {
        let record = || RefusalRecord {
            tool: &ctx.tool,
            task_id: ctx.task_id.clone(),
            kind,
            name,
            binary: Some(crate::sandbox::resolve_invoked_binary_path(program)),
            port: None,
        };
        if let Err(reason) = crate::shell::check_shell_allow(&self.policy, program) {
            return Some(self.refuse(record(), rule::SHELL_ALLOW, reason).await);
        }
        let resolved = resolve_process_call(program, args, &self.workdir);
        if let Some(refusal) = ctx.gate.ask(resolved).await {
            self.trace_refusal(record(), &refusal).await;
            return Some(refused(refusal.rule, refusal.reason));
        }
        None
    }

    /// Spawns `program` contained exactly as a shell command is, on the blocking pool.
    #[allow(clippy::too_many_arguments)]
    async fn spawn(
        &self,
        ctx: &ProcessCallCtx,
        kind: &'static str,
        name: Option<&str>,
        program: &str,
        args: &[String],
        env: &[(String, String)],
        stdin: bool,
    ) -> Result<ContainedChild, wit::ProcessError> {
        let mut overrides = self.env_overrides.clone();
        overrides.extend(env.iter().cloned());
        let program_owned = program.to_string();
        let args_owned = args.to_vec();
        let workdir = self.workdir.clone();
        let policy = self.policy.clone();
        let enforcement = self.enforcement.clone();
        let spawned = tokio::task::spawn_blocking(move || {
            let args: Vec<&str> = args_owned.iter().map(String::as_str).collect();
            crate::shell::spawn_contained(
                &program_owned,
                &args,
                &overrides,
                &workdir,
                &policy,
                &enforcement,
                if stdin { Stdio::piped() } else { Stdio::null() },
                "murmur:process",
            )
        })
        .await
        .map_err(|error| wit::ProcessError::Failed(format!("the spawn panicked: {error}")))?;
        match spawned {
            Ok(child) => Ok(child),
            Err(ContainedSpawnError::WorkdirBudget(reason)) => Err(self
                .refuse(
                    RefusalRecord {
                        tool: &ctx.tool,
                        task_id: ctx.task_id.clone(),
                        kind,
                        name,
                        binary: Some(crate::sandbox::resolve_invoked_binary_path(program)),
                        port: None,
                    },
                    rule::WORKDIR_BUDGET,
                    reason,
                )
                .await),
            Err(ContainedSpawnError::Exec(ShellExecError::Failed(message))) => {
                Err(wit::ProcessError::Failed(message))
            }
            Err(ContainedSpawnError::Exec(
                error @ ShellExecError::SealedRootConstructionFailed { .. },
            )) => {
                let text = error
                    .session_fatal()
                    .map(|fatal| fatal.to_string())
                    .unwrap_or_else(|| error.to_string());
                if let ShellExecError::SealedRootConstructionFailed { detail } = error {
                    *lock(&self.sealed_failure) = Some(detail);
                }
                Err(wit::ProcessError::Failed(text))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn trace_start(
        &self,
        ctx: &ProcessCallCtx,
        process_id: &str,
        kind: &'static str,
        name: Option<&str>,
        binary: &str,
        argv: &[String],
        pid: i32,
    ) {
        if let Some(trace) = &self.trace {
            trace
                .write_process_start(crate::trace::ProcessStartLine {
                    task_id: ctx.task_id.clone(),
                    tool: &ctx.tool,
                    process_id,
                    kind,
                    name,
                    binary,
                    argv,
                    pid,
                })
                .await;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn trace_exit(
        trace: Option<&Arc<ResourceTraceAppender>>,
        task_id: Option<String>,
        tool: &str,
        process_id: &str,
        kind: &'static str,
        name: Option<&str>,
        binary: &str,
        exit_code: i32,
        reason: &'static str,
        duration_ms: u64,
    ) {
        if let Some(trace) = trace {
            trace
                .write_process_exit(crate::trace::ProcessExitLine {
                    task_id,
                    tool,
                    process_id,
                    kind,
                    name,
                    binary,
                    exit_code,
                    reason,
                    duration_ms,
                })
                .await;
        }
    }

    // ------------------------------------------------------------ run

    async fn run(
        &self,
        ctx: &ProcessCallCtx,
        request: wit::RunRequest,
    ) -> Result<wit::RunOutput, wit::ProcessError> {
        if self.ended() {
            return Err(refused(rule::SESSION_ENDED, "the session has ended"));
        }
        if let Some(refusal) = self
            .admit(ctx, "run", None, &request.program, &request.args)
            .await
        {
            return Err(refusal);
        }
        let limit = request.timeout_ms.map_or(ctx.remaining(), |ms| {
            Duration::from_millis(ms).min(ctx.remaining())
        });
        let ContainedChild {
            mut child,
            pgid,
            resolved_binary,
            started,
            cgroup_occupancy,
            ..
        } = self
            .spawn(
                ctx,
                "run",
                None,
                &request.program,
                &request.args,
                &request.env,
                request.stdin.is_some(),
            )
            .await?;
        // No `run` outlives its call: dropping this future (the call cut by its deadline) kills
        // the group.
        let signals = Arc::new(GroupSignals::default());
        let guard = KillGroupOnDrop(pgid, Arc::clone(&signals));
        let process_id = format!("{HANDLE_PREFIX}{}", uuid::Uuid::now_v7().simple());
        self.trace_start(
            ctx,
            &process_id,
            "run",
            None,
            &resolved_binary,
            &request.args,
            pgid,
        )
        .await;

        if let (Some(bytes), Some(mut stdin)) = (request.stdin, child.stdin.take()) {
            std::thread::spawn(move || {
                // A program that exits without reading its stdin closes the pipe; that is its
                // business, not a failure of the call.
                let _ = stdin.write_all(&bytes);
            });
        }
        let stdout = CappedCapture::spawn(child.stdout.take());
        let stderr = CappedCapture::spawn(child.stderr.take());

        let reaper_signals = Arc::clone(&signals);
        let waiter = tokio::task::spawn_blocking(move || {
            wait_and_kill_group(&mut child, pgid, &reaper_signals)
        });
        tokio::pin!(waiter);
        let (status, timed_out) = match tokio::time::timeout(limit, &mut waiter).await {
            Ok(joined) => (joined, false),
            Err(_) => {
                signals.signal(pgid, libc::SIGKILL);
                ((&mut waiter).await, true)
            }
        };
        drop(guard);
        drop(cgroup_occupancy);
        let exit_code = match status {
            Ok(Ok(status)) => crate::shell::exit_code_of(&status),
            Ok(Err(error)) => return Err(wit::ProcessError::Failed(error.to_string())),
            Err(error) => return Err(wit::ProcessError::Failed(error.to_string())),
        };
        let drain_until = Instant::now() + RUN_DRAIN_AFTER_EXIT;
        let (stdout, stdout_truncated) = stdout.finish(drain_until).await;
        let (stderr, stderr_truncated) = stderr.finish(drain_until).await;
        Self::trace_exit(
            self.trace.as_ref(),
            ctx.task_id.clone(),
            &ctx.tool,
            &process_id,
            "run",
            None,
            &resolved_binary,
            exit_code,
            if timed_out { "timed_out" } else { "exited" },
            started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        )
        .await;
        Ok(wit::RunOutput {
            exit_code,
            timed_out,
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
        })
    }

    // ------------------------------------------------------------ start

    async fn start(
        self: &Arc<Self>,
        ctx: &ProcessCallCtx,
        request: wit::StartRequest,
    ) -> Result<wit::Started, wit::ProcessError> {
        let name = request.name.as_str();
        let record = |rule_binary: bool| RefusalRecord {
            tool: &ctx.tool,
            task_id: ctx.task_id.clone(),
            kind: "session",
            name: Some(name),
            binary: rule_binary
                .then(|| crate::sandbox::resolve_invoked_binary_path(&request.program)),
            port: None,
        };
        if !valid_process_name(name) {
            return Err(self
                .refuse(
                    record(true),
                    rule::NAME,
                    format!(
                        "a process name is 1 to {MAX_PROCESS_NAME_CHARS} characters from \
                         A-Z a-z 0-9 . _ -; '{name}' is not"
                    ),
                )
                .await);
        }
        let key = (ctx.tool.clone(), name.to_string());

        // A name already running answers before anything is asked; one being started is waited
        // for, then looked at again.
        loop {
            let pending = {
                let state = lock(&self.state);
                if state.ended {
                    return Err(refused(rule::SESSION_ENDED, "the session has ended"));
                }
                match state.by_name.get(&key) {
                    Some(process) if process.phase() == Phase::Running => {
                        return Ok(wit::Started {
                            handle: process.handle.clone(),
                            already_running: true,
                        })
                    }
                    Some(process) if process.phase() == Phase::Starting => {
                        Some(Arc::clone(process))
                    }
                    _ => {
                        if state.live_count() >= MAX_SESSION_PROCESSES {
                            None
                        } else {
                            break;
                        }
                    }
                }
            };
            match pending {
                Some(process) => {
                    let mut phase = process.phase.subscribe();
                    let _ = phase.wait_for(|phase| *phase != Phase::Starting).await;
                }
                None => return Err(self.refuse_cap(record(true)).await),
            }
        }

        if let Some(refusal) = self
            .admit(ctx, "session", Some(name), &request.program, &request.args)
            .await
        {
            return Err(refusal);
        }

        // Reserve the name, checking the cap again under the lock.
        let reserved = {
            let mut state = lock(&self.state);
            match state.by_name.get(&key) {
                _ if state.ended => Reservation::Ended,
                Some(process) if process.phase() == Phase::Running => {
                    Reservation::Running(process.handle.clone())
                }
                Some(process) if process.phase() == Phase::Starting => {
                    Reservation::Concurrent(Arc::clone(process))
                }
                _ if state.live_count() >= MAX_SESSION_PROCESSES => Reservation::Full,
                _ => {
                    let process = Arc::new(SessionProcess {
                        handle: format!("{HANDLE_PREFIX}{}", uuid::Uuid::now_v7().simple()),
                        tool: ctx.tool.clone(),
                        name: name.to_string(),
                        task_id: ctx.task_id.clone(),
                        binary: Mutex::new(crate::sandbox::resolve_invoked_binary_path(
                            &request.program,
                        )),
                        phase: watch::Sender::new(Phase::Starting),
                        pid: AtomicI32::new(0),
                        signals: GroupSignals::default(),
                        started: Mutex::new(Instant::now()),
                        stdin: Arc::new(Mutex::new(None)),
                        stdout: Arc::new(Mutex::new(OutputRing::default())),
                        stderr: Arc::new(Mutex::new(OutputRing::default())),
                        exit_reason: Mutex::new("exited"),
                        exit_traced: AtomicBool::new(false),
                    });
                    state.by_name.insert(key.clone(), Arc::clone(&process));
                    state
                        .by_handle
                        .insert(process.handle.clone(), Arc::clone(&process));
                    Reservation::Reserved(process)
                }
            }
        };
        let process = match reserved {
            Reservation::Reserved(process) => process,
            Reservation::Ended => {
                return Err(refused(rule::SESSION_ENDED, "the session has ended"))
            }
            Reservation::Running(handle) => {
                return Ok(wit::Started {
                    handle,
                    already_running: true,
                })
            }
            Reservation::Concurrent(process) => {
                return Self::await_concurrent_start(&process).await
            }
            Reservation::Full => return Err(self.refuse_cap(record(true)).await),
        };

        let spawned = self
            .spawn(
                ctx,
                "session",
                Some(name),
                &request.program,
                &request.args,
                &request.env,
                true,
            )
            .await;
        let ContainedChild {
            mut child,
            pgid,
            resolved_binary,
            started,
            cgroup_occupancy,
            ..
        } = match spawned {
            Ok(child) => child,
            Err(error) => {
                let mut state = lock(&self.state);
                if state
                    .by_name
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &process))
                {
                    state.by_name.remove(&key);
                }
                state.by_handle.remove(&process.handle);
                process.phase.send_replace(Phase::Abandoned);
                return Err(error);
            }
        };

        *lock(&process.binary) = resolved_binary.clone();
        *lock(&process.started) = started;
        *lock(&process.stdin) = child.stdin.take();
        process.pid.store(pgid, Ordering::SeqCst);
        let stdout_ended = pump_into(child.stdout.take(), Arc::clone(&process.stdout));
        let stderr_ended = pump_into(child.stderr.take(), Arc::clone(&process.stderr));
        self.trace_start(
            ctx,
            &process.handle,
            "session",
            Some(name),
            &resolved_binary,
            &request.args,
            pgid,
        )
        .await;
        let ended_meanwhile = {
            let state = lock(&self.state);
            process.phase.send_replace(Phase::Running);
            state.ended
        };

        // The reaper: records the exit when it happens, not at the next call, and only once both
        // output pumps have buffered everything the group wrote, so whoever sees `Exited` can read
        // all of it. Each pump marks its ring ended under the ring's lock before it signals, and
        // the phase moves to `Exited` after this wait, so a `read` that follows an observed
        // `Exited` takes the ring lock after the last push.
        let registry = Arc::clone(self);
        let reaped = Arc::clone(&process);
        tokio::spawn(async move {
            let waited = Arc::clone(&reaped);
            let status = tokio::task::spawn_blocking(move || {
                wait_and_kill_group(&mut child, pgid, &waited.signals)
            })
            .await;
            let exited_at = Instant::now();
            drop(cgroup_occupancy);
            let exit_code = match status {
                Ok(Ok(status)) => crate::shell::exit_code_of(&status),
                _ => -1,
            };
            let drained = async {
                let _ = stdout_ended.await;
                let _ = stderr_ended.await;
            };
            let _ = tokio::time::timeout(OUTPUT_DRAIN_LIMIT, drained).await;
            registry.record_exit(&reaped, exit_code, exited_at).await;
        });

        if ended_meanwhile {
            // The session ended while this one was being spawned: it goes the way the others went.
            *lock(&process.exit_reason) = "session_end";
            process.signal(libc::SIGKILL);
        }
        Ok(wit::Started {
            handle: process.handle.clone(),
            already_running: false,
        })
    }

    /// A concurrent `start` of the same name reserved it first: answer with its outcome.
    async fn await_concurrent_start(
        process: &SessionProcess,
    ) -> Result<wit::Started, wit::ProcessError> {
        let mut phase = process.phase.subscribe();
        let _ = phase.wait_for(|phase| *phase != Phase::Starting).await;
        if matches!(process.phase(), Phase::Running | Phase::Exited(_)) {
            return Ok(wit::Started {
                handle: process.handle.clone(),
                already_running: true,
            });
        }
        Err(wit::ProcessError::Failed(
            "a concurrent start of this name was not started".to_string(),
        ))
    }

    async fn refuse_cap(&self, record: RefusalRecord<'_>) -> wit::ProcessError {
        self.refuse(
            record,
            rule::CAP,
            format!(
                "this session already holds {MAX_SESSION_PROCESSES} running processes, the most \
                 it may; stop one first"
            ),
        )
        .await
    }

    /// Records `process`'s exit once: its `process_exit` line, whose `duration_ms` runs from the
    /// spawn to `exited_at`, then its state.
    async fn record_exit(&self, process: &SessionProcess, exit_code: i32, exited_at: Instant) {
        if !process.exit_traced.swap(true, Ordering::SeqCst) {
            let reason = *lock(&process.exit_reason);
            let duration_ms = exited_at
                .saturating_duration_since(*lock(&process.started))
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let binary = lock(&process.binary).clone();
            Self::trace_exit(
                self.trace.as_ref(),
                process.task_id.clone(),
                &process.tool,
                &process.handle,
                "session",
                Some(&process.name),
                &binary,
                exit_code,
                reason,
                duration_ms,
            )
            .await;
        }
        process.phase.send_replace(Phase::Exited(exit_code));
    }

    // ------------------------------------------------------------ the rest of a session process

    async fn write(
        &self,
        ctx: &ProcessCallCtx,
        handle: &str,
        bytes: Vec<u8>,
    ) -> Result<(), wit::ProcessError> {
        let process = self.lookup(&ctx.tool, handle)?;
        if process.phase() != Phase::Running {
            return Err(wit::ProcessError::NotRunning);
        }
        let stdin = Arc::clone(&process.stdin);
        let writing = tokio::task::spawn_blocking(move || match lock(&stdin).as_mut() {
            Some(pipe) => pipe.write_all(&bytes).and_then(|()| pipe.flush()),
            None => Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        });
        match tokio::time::timeout(ctx.remaining(), writing).await {
            Err(_) => Err(wit::ProcessError::TimedOut),
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                Err(wit::ProcessError::NotRunning)
            }
            Ok(Ok(Err(error))) => Err(wit::ProcessError::Failed(error.to_string())),
            Ok(Err(error)) => Err(wit::ProcessError::Failed(error.to_string())),
        }
    }

    fn read(
        &self,
        ctx: &ProcessCallCtx,
        handle: &str,
        max_bytes: u32,
    ) -> Result<wit::Output, wit::ProcessError> {
        let process = self.lookup(&ctx.tool, handle)?;
        let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        // A stream's `-ended` is read under the same lock as its take, so a pump that pushes and
        // marks end-of-file between the two cannot make a read report ended with bytes left.
        let take = |ring: &Mutex<OutputRing>| {
            let mut ring = lock(ring);
            let (bytes, dropped) = ring.take(max);
            (bytes, dropped, ring.ended())
        };
        let (stdout, stdout_dropped, stdout_ended) = take(&process.stdout);
        let (stderr, stderr_dropped, stderr_ended) = take(&process.stderr);
        Ok(wit::Output {
            stdout,
            stderr,
            stdout_dropped,
            stderr_dropped,
            stdout_ended,
            stderr_ended,
        })
    }

    async fn stop(
        &self,
        ctx: &ProcessCallCtx,
        handle: &str,
    ) -> Result<wit::State, wit::ProcessError> {
        let process = self.lookup(&ctx.tool, handle)?;
        if process.phase() != Phase::Running {
            return Ok(process.state());
        }
        *lock(&process.exit_reason) = "stopped";
        process.signal(libc::SIGTERM);
        if !process
            .wait_exit(crate::background_command::STOP_TERM_GRACE)
            .await
        {
            process.signal(libc::SIGKILL);
            process
                .wait_exit(crate::background_command::STOP_KILL_WAIT)
                .await;
        }
        Ok(process.state())
    }

    // ------------------------------------------------------------ connect

    async fn connect(
        &self,
        ctx: &ProcessCallCtx,
        handle: &str,
        port: u16,
    ) -> Result<std::net::TcpStream, wit::ProcessError> {
        let process = self.lookup(&ctx.tool, handle)?;
        if process.phase() != Phase::Running {
            return Err(wit::ProcessError::NotRunning);
        }
        let pgid = process.pgid();
        let owned =
            tokio::task::spawn_blocking(move || owned_listener(&ProcFs::host(), pgid, port))
                .await
                .map_err(|error| wit::ProcessError::Failed(error.to_string()))?;
        let Some(family) = owned else {
            return Err(self
                .refuse(
                    RefusalRecord {
                        tool: &ctx.tool,
                        task_id: ctx.task_id.clone(),
                        kind: "connect",
                        name: Some(&process.name),
                        binary: None,
                        port: Some(port),
                    },
                    rule::CONNECT,
                    format!(
                        "no process in the group of '{}' listens on port {port} at a loopback or \
                         wildcard address",
                        process.name
                    ),
                )
                .await);
        };
        let address = match family {
            ListenerFamily::V4 => std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            ListenerFamily::V6 => std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        };
        let enforcement = self.enforcement.clone();
        tokio::task::spawn_blocking(move || connect_in_session(&enforcement, address))
            .await
            .map_err(|error| wit::ProcessError::Failed(error.to_string()))?
            .map_err(wit::ProcessError::Failed)
    }

    // ------------------------------------------------------------ the session's end

    /// Kills every process the session still runs, with its process group, waits boundedly for
    /// each exit to be recorded (`process_exit`, `reason: session_end`), and refuses every later
    /// `run` and `start`. Called by the session's teardown before its `session_end` line.
    pub(crate) async fn end_session(&self) {
        let live: Vec<Arc<SessionProcess>> = {
            let mut state = lock(&self.state);
            state.ended = true;
            state
                .by_name
                .values()
                .filter(|process| process.live())
                .cloned()
                .collect()
        };
        for process in &live {
            *lock(&process.exit_reason) = "session_end";
            process.signal(libc::SIGKILL);
        }
        for process in &live {
            if !process.wait_exit(SESSION_END_REAP_WAIT).await && process.phase() == Phase::Running
            {
                // The reaper did not get there in time: the line is written here instead, once.
                let exit_code = 128 + libc::SIGKILL;
                self.record_exit(process, exit_code, Instant::now()).await;
            }
        }
    }

    /// The pids of the session processes still running, for a test.
    #[cfg(test)]
    fn running_pids(&self) -> Vec<i32> {
        lock(&self.state)
            .by_name
            .values()
            .filter(|process| process.phase() == Phase::Running)
            .map(|process| process.pgid())
            .collect()
    }
}

impl Drop for SessionProcesses {
    /// The backstop for a session that never reached [`Self::end_session`]: kill only.
    fn drop(&mut self) {
        let state = lock(&self.state);
        for process in state.by_name.values() {
            process.signal(libc::SIGKILL);
        }
    }
}

/// Signals the process group a spawned child leads, only while that child is unreaped.
///
/// Never signal a group whose leader has been reaped: the group id is then free, and the kernel
/// may hand it to an unrelated process's group. The reaper marks the leader reaped under the same
/// lock a signal takes, before it reaps.
#[derive(Default)]
struct GroupSignals {
    leader_reaped: Mutex<bool>,
}

impl GroupSignals {
    fn signal(&self, pgid: i32, signal: libc::c_int) {
        let leader_reaped = lock(&self.leader_reaped);
        if pgid > 1 && !*leader_reaped {
            crate::formation_launch::signal_group(i64::from(pgid), signal);
        }
    }
}

/// Waits for a group leader to exit, kills whatever it left in its process group, then reaps it.
///
/// The kill is sent while the leader is still an unreaped zombie, which pins its group id: a
/// process a program forked dies when the program exits, so a session process that has exited
/// holds nothing running.
fn wait_and_kill_group(
    child: &mut std::process::Child,
    pgid: i32,
    signals: &GroupSignals,
) -> std::io::Result<std::process::ExitStatus> {
    if crate::shell::wait_for_exit_unreaped(pgid).is_ok() {
        signals.signal(pgid, libc::SIGKILL);
    }
    *lock(&signals.leader_reaped) = true;
    child.wait()
}

/// Kills a `run`'s process group if its call's future is dropped before the run finished.
struct KillGroupOnDrop(i32, Arc<GroupSignals>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        self.1.signal(self.0, libc::SIGKILL);
    }
}

/// Reads one pipe on its own thread into `ring` until end-of-file or a read error, then marks the
/// ring at end-of-file. The receiver completes once the mark is set (or the thread is gone); with
/// no pipe the ring is marked at once.
fn pump_into(
    pipe: Option<impl std::io::Read + Send + 'static>,
    ring: Arc<Mutex<OutputRing>>,
) -> oneshot::Receiver<()> {
    let (ended, on_end) = oneshot::channel();
    let Some(mut pipe) = pipe else {
        lock(&ring).mark_end_of_file();
        let _ = ended.send(());
        return on_end;
    };
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => lock(&ring).push(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        lock(&ring).mark_end_of_file();
        let _ = ended.send(());
    });
    on_end
}

/// One `run` stream: the first [`MAX_PROCESS_OUTPUT_BYTES`] kept, the rest read and discarded.
struct CappedCapture {
    shared: Arc<Mutex<(Vec<u8>, bool)>>,
    done: Arc<AtomicBool>,
}

impl CappedCapture {
    fn spawn(pipe: Option<impl std::io::Read + Send + 'static>) -> Self {
        let shared = Arc::new(Mutex::new((Vec::new(), false)));
        let done = Arc::new(AtomicBool::new(pipe.is_none()));
        if let Some(mut pipe) = pipe {
            let shared = Arc::clone(&shared);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut captured = lock(&shared);
                            let room = MAX_PROCESS_OUTPUT_BYTES.saturating_sub(captured.0.len());
                            let kept = room.min(read);
                            captured.0.extend_from_slice(&chunk[..kept]);
                            if kept < read {
                                captured.1 = true;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                done.store(true, Ordering::SeqCst);
            });
        }
        Self { shared, done }
    }

    /// What was captured, once the pipe closed or `until` passed.
    async fn finish(self, until: Instant) -> (Vec<u8>, bool) {
        while !self.done.load(Ordering::SeqCst) && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut captured = lock(&self.shared);
        (std::mem::take(&mut captured.0), captured.1)
    }
}

// ---------------------------------------------------------------- connect's ownership check

/// The address family of a listener `connect` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerFamily {
    V4,
    V6,
}

/// Where `/proc` is read from: the host's, or a constructed tree in a test.
pub(crate) struct ProcFs {
    root: std::path::PathBuf,
}

impl ProcFs {
    pub(crate) fn host() -> Self {
        Self {
            root: std::path::PathBuf::from("/proc"),
        }
    }

    #[cfg(test)]
    fn at(root: &std::path::Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// The pids whose process group is `pgid`.
    fn group_members(&self, pgid: i32) -> Vec<i32> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(self.root.join(pid.to_string()).join("stat"))
                    .ok()
                    .and_then(|stat| stat_pgrp(&stat))
                    == Some(pgid)
            })
            .collect()
    }

    /// The socket inodes `pid` holds open.
    fn socket_inodes(&self, pid: i32) -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir(self.root.join(pid.to_string()).join("fd")) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|target| {
                target
                    .to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse()
                    .ok()
            })
            .collect()
    }

    /// `pid`'s view of its network namespace's `tcp` or `tcp6` table.
    fn tcp_table(&self, pid: i32, table: &str) -> String {
        std::fs::read_to_string(self.root.join(pid.to_string()).join("net").join(table))
            .unwrap_or_default()
    }
}

/// The process group field of a `/proc/<pid>/stat` line: the fifth, counted after the command
/// name's closing parenthesis (which may itself contain spaces and parentheses).
fn stat_pgrp(stat: &str) -> Option<i32> {
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(2)?.parse().ok()
}

/// The inodes of the sockets in a `/proc/net/tcp`-format `table` listening on `port` at a
/// loopback or wildcard address.
fn listening_inodes(table: &str, port: u16, v6: bool) -> Vec<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (address, local_port) = fields.get(1)?.split_once(':')?;
            if u16::from_str_radix(local_port, 16).ok()? != port || *fields.get(3)? != "0A" {
                return None;
            }
            let reachable = if v6 {
                v6_loopback_or_wildcard(address)
            } else {
                v4_loopback_or_wildcard(address)
            };
            reachable.then(|| fields.get(9)?.parse().ok()).flatten()
        })
        .collect()
}

/// `address` as `/proc/net/tcp` prints it (the address's four bytes as one native-endian hex
/// word) is `0.0.0.0` or in `127.0.0.0/8`.
fn v4_loopback_or_wildcard(address: &str) -> bool {
    let Ok(word) = u32::from_str_radix(address, 16) else {
        return false;
    };
    let octets = word.to_ne_bytes();
    word == 0 || octets[0] == 127
}

/// `address` as `/proc/net/tcp6` prints it (four native-endian hex words) is `::`, `::1` or an
/// IPv4-mapped loopback address.
fn v6_loopback_or_wildcard(address: &str) -> bool {
    if address.len() != 32 {
        return false;
    }
    let mut bytes = [0u8; 16];
    for (index, chunk) in bytes.chunks_mut(4).enumerate() {
        let Ok(word) = u32::from_str_radix(&address[index * 8..index * 8 + 8], 16) else {
            return false;
        };
        chunk.copy_from_slice(&word.to_ne_bytes());
    }
    let ip = std::net::Ipv6Addr::from(bytes);
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
}

/// Whether a process in group `pgid` holds a socket listening on `port` at a loopback or wildcard
/// address, read from the group leader's own view of its network namespace; the family to reach
/// it on when one does.
pub(crate) fn owned_listener(proc_fs: &ProcFs, pgid: i32, port: u16) -> Option<ListenerFamily> {
    let v4 = listening_inodes(&proc_fs.tcp_table(pgid, "tcp"), port, false);
    let v6 = listening_inodes(&proc_fs.tcp_table(pgid, "tcp6"), port, true);
    if v4.is_empty() && v6.is_empty() {
        return None;
    }
    let held: std::collections::HashSet<u64> = proc_fs
        .group_members(pgid)
        .into_iter()
        .flat_map(|member| proc_fs.socket_inodes(member))
        .collect();
    if v4.iter().any(|inode| held.contains(inode)) {
        Some(ListenerFamily::V4)
    } else if v6.iter().any(|inode| held.contains(inode)) {
        Some(ListenerFamily::V6)
    } else {
        None
    }
}

/// A TCP connection to `address` from inside the session's network namespace on the tiers that
/// have one, and from the runtime's own namespace on the tiers whose commands share it.
fn connect_in_session(
    enforcement: &ShellEnforcement,
    address: std::net::SocketAddr,
) -> Result<std::net::TcpStream, String> {
    let shares_host_namespace = matches!(
        enforcement.tier,
        EnforcementTier::Unconfined | EnforcementTier::EnvironmentOnly
    );
    if shares_host_namespace {
        return std::net::TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|error| format!("connecting to {address} failed: {error}"));
    }
    #[cfg(target_os = "linux")]
    {
        let join = enforcement.session_netns.establish(
            &enforcement.egress_tcp_ports,
            crate::egress_proxy::EgressPolicy::new(
                enforcement.network_allow_rules.clone(),
                enforcement.network_allow_ips.clone(),
            ),
            enforcement.observed_held_ports(),
        )?;
        let (user_fd, net_fd) = join.raw_fds();
        linux_connect::connect_in_namespace(user_fd, net_fd, address)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("this platform has no session network namespace to connect inside".to_string())
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux_connect {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    /// Forks a single-threaded child that joins the session's user and network namespaces,
    /// connects a TCP socket to `address` there, and hands the socket back over `SCM_RIGHTS`.
    ///
    /// The runtime is multithreaded and cannot `setns` into a user namespace itself; a socket
    /// belongs to the namespace it was created in, so the child's socket reaches the session's
    /// loopback and keeps doing so in the runtime's hands.
    pub(super) fn connect_in_namespace(
        user_fd: RawFd,
        net_fd: RawFd,
        address: std::net::SocketAddr,
    ) -> Result<std::net::TcpStream, String> {
        let mut pair = [0i32; 2];
        // SAFETY: `pair` is a 2-element stack array, exactly what `socketpair` writes.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(format!(
                "socketpair() for the connection hand-off failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: both fds were just returned by `socketpair` and nothing else owns them.
        let (parent_end, child_end) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        let child_fd = child_end.as_raw_fd();

        // Built before the fork, so the child allocates nothing.
        let (storage, length) = socket_address(&address);
        let domain = if address.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        // A blocking `connect` gives up after the socket's send timeout, so a listener whose
        // accept queue is full holds the child no longer than `CONNECT_TIMEOUT`.
        let handshake_limit = libc::timeval {
            tv_sec: libc::time_t::try_from(super::CONNECT_TIMEOUT.as_secs()).unwrap_or(5),
            tv_usec: 0,
        };
        let no_limit = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let timeval_length = std::mem::size_of::<libc::timeval>() as libc::socklen_t;

        // SAFETY: the child runs only `setns`, `socket`, `setsockopt`, `connect`, `sendmsg`,
        // `send` and `_exit` — async-signal-safe syscalls over stack locals and buffers built
        // before the fork — with no allocation, no locks and no stdio.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!(
                "fork() for the connection failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            // SAFETY: forked-child context; every branch ends in `_exit`.
            unsafe {
                if crate::network_namespace::join_session_netns(user_fd, net_fd).is_err() {
                    fail(child_fd);
                }
                let socket = libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                if socket < 0 {
                    fail(child_fd);
                }
                if libc::setsockopt(
                    socket,
                    libc::SOL_SOCKET,
                    libc::SO_SNDTIMEO,
                    (&raw const handshake_limit).cast(),
                    timeval_length,
                ) != 0
                {
                    fail(child_fd);
                }
                if libc::connect(
                    socket,
                    (&raw const storage).cast::<libc::sockaddr>(),
                    length,
                ) != 0
                {
                    fail(child_fd);
                }
                // The tool's writes on the connection are bounded by its own call, not by the
                // handshake's limit.
                if libc::setsockopt(
                    socket,
                    libc::SOL_SOCKET,
                    libc::SO_SNDTIMEO,
                    (&raw const no_limit).cast(),
                    timeval_length,
                ) != 0
                {
                    fail(child_fd);
                }
                if crate::network_namespace::send_fds(child_fd, &[socket]).is_err() {
                    fail(child_fd);
                }
                libc::_exit(0);
            }
        }
        drop(child_end);
        let status = crate::network_namespace::reap_child(pid)
            .map_err(|error| format!("waiting for the connecting child failed: {error}"))?;
        receive(parent_end.as_raw_fd()).map_err(|detail| format!("{detail} ({status})"))
    }

    /// Reports the current `errno` to the parent and exits.
    ///
    /// # Safety
    /// Forked-child context only: `send` and `_exit` over a stack local.
    unsafe fn fail(fd: RawFd) -> ! {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        let bytes = errno.to_ne_bytes();
        // SAFETY: `bytes` is a live local; `_exit` never returns.
        unsafe {
            libc::send(fd, bytes.as_ptr().cast(), bytes.len(), 0);
            libc::_exit(1)
        }
    }

    /// The child's one message: the connected socket, or the `errno` of the step that failed.
    fn receive(fd: RawFd) -> Result<std::net::TcpStream, String> {
        #[repr(C, align(8))]
        struct Control([u8; 64]);
        // SAFETY: every buffer is a live local outliving the one non-blocking `recvmsg`; the
        // control buffer is 8-byte aligned as the CMSG macros require, and a received descriptor
        // is taken into an `OwnedFd` exactly once.
        unsafe {
            let mut payload = [0u8; 8];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut control = Control([0u8; 64]);
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr().cast();
            msg.msg_controllen = control.0.len() as _;
            let read = libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC);
            if read < 0 {
                return Err("the connecting child handed nothing over".to_string());
            }
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if !cmsg.is_null()
                && (*cmsg).cmsg_level == libc::SOL_SOCKET
                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
            {
                let mut raw: RawFd = -1;
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    (&raw mut raw).cast::<u8>(),
                    std::mem::size_of::<RawFd>(),
                );
                if raw >= 0 {
                    return Ok(std::net::TcpStream::from(OwnedFd::from_raw_fd(raw)));
                }
            }
            if read as usize >= 4 {
                let errno = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
                return Err(format!(
                    "connecting inside the session's network namespace failed: {}",
                    std::io::Error::from_raw_os_error(errno)
                ));
            }
            Err("the connecting child handed nothing over".to_string())
        }
    }

    /// `address` as a `sockaddr_storage` and its length.
    fn socket_address(address: &std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
        // SAFETY: an all-zero `sockaddr_storage` is a valid value, and each arm writes a
        // `sockaddr_in`/`sockaddr_in6`, both smaller than and alignment-compatible with it.
        unsafe {
            let mut storage: libc::sockaddr_storage = std::mem::zeroed();
            match address {
                std::net::SocketAddr::V4(v4) => {
                    let sin = (&raw mut storage).cast::<libc::sockaddr_in>();
                    (*sin).sin_family = libc::AF_INET as libc::sa_family_t;
                    (*sin).sin_port = v4.port().to_be();
                    (*sin).sin_addr = libc::in_addr {
                        s_addr: u32::from_ne_bytes(v4.ip().octets()),
                    };
                    (
                        storage,
                        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                    )
                }
                std::net::SocketAddr::V6(v6) => {
                    let sin6 = (&raw mut storage).cast::<libc::sockaddr_in6>();
                    (*sin6).sin6_family = libc::AF_INET6 as libc::sa_family_t;
                    (*sin6).sin6_port = v6.port().to_be();
                    (*sin6).sin6_addr = libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    };
                    (
                        storage,
                        std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------- the connection resource

/// A `connection` resource: a TCP stream opened for one tool call, closed with its store.
pub struct Connection {
    stream: Option<Arc<std::net::TcpStream>>,
    /// In a guest worker, the supervisor's handle for the connection it holds.
    remote: Option<u32>,
}

// ---------------------------------------------------------------- the bindings

impl wit::Host for ProcessView<'_> {
    async fn run(&mut self, request: wit::RunRequest) -> Result<wit::RunOutput, wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Run(request)).await.run();
        }
        let ctx = self.access.ctx()?;
        ctx.registry.run(ctx, request).await
    }

    async fn start(
        &mut self,
        request: wit::StartRequest,
    ) -> Result<wit::Started, wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Start(request)).await.started();
        }
        let ctx = self.access.ctx()?;
        ctx.registry.start(ctx, request).await
    }

    async fn find(&mut self, name: String) -> Option<wit::Handle> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Find(name)).await.found();
        }
        let ctx = self.access.ctx().ok()?;
        ctx.registry.find(&ctx.tool, &name)
    }

    async fn status(&mut self, handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Status(handle)).await.state();
        }
        let ctx = self.access.ctx()?;
        Ok(ctx.registry.lookup(&ctx.tool, &handle)?.state())
    }

    async fn write(
        &mut self,
        handle: wit::Handle,
        bytes: Vec<u8>,
    ) -> Result<(), wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Write(handle, bytes)).await.unit();
        }
        let ctx = self.access.ctx()?;
        ctx.registry.write(ctx, &handle, bytes).await
    }

    async fn read(
        &mut self,
        handle: wit::Handle,
        max_bytes: u32,
    ) -> Result<wit::Output, wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link
                .process(ProcessCall::Read(handle, max_bytes))
                .await
                .output();
        }
        let ctx = self.access.ctx()?;
        ctx.registry.read(ctx, &handle, max_bytes)
    }

    async fn stop(&mut self, handle: wit::Handle) -> Result<wit::State, wit::ProcessError> {
        if let Some(link) = self.access.remote() {
            return link.process(ProcessCall::Stop(handle)).await.state();
        }
        let ctx = self.access.ctx()?;
        ctx.registry.stop(ctx, &handle).await
    }

    async fn connect(
        &mut self,
        handle: wit::Handle,
        port: u16,
    ) -> Result<Resource<Connection>, wit::ProcessError> {
        let connection = if let Some(link) = self.access.remote() {
            let remote = link
                .process(ProcessCall::Connect(handle, port))
                .await
                .connected()?;
            Connection {
                stream: None,
                remote: Some(remote),
            }
        } else {
            let ctx = self.access.ctx()?;
            let stream = ctx.registry.connect(ctx, &handle, port).await?;
            Connection {
                stream: Some(Arc::new(stream)),
                remote: None,
            }
        };
        self.table
            .push(connection)
            .map_err(|error| wit::ProcessError::Failed(error.to_string()))
    }
}

impl wit::HostConnection for ProcessView<'_> {
    async fn read(
        &mut self,
        connection: Resource<Connection>,
        max_bytes: u32,
        timeout_ms: u32,
    ) -> Result<Vec<u8>, wit::ProcessError> {
        if let Some((link, remote)) = self.remote_of(&connection) {
            return link
                .process(ProcessCall::ConnectionRead(remote, max_bytes, timeout_ms))
                .await
                .bytes();
        }
        let stream = self.stream_of(&connection)?;
        let limit = self
            .call_remaining()
            .min(Duration::from_millis(u64::from(timeout_ms)));
        let max = usize::try_from(max_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_PROCESS_OUTPUT_BYTES);
        tokio::task::spawn_blocking(move || {
            // A zero timeout would mean "block forever" to the socket.
            stream.set_read_timeout(Some(limit.max(Duration::from_millis(1))))?;
            let mut buffer = vec![0u8; max];
            let read = (&*stream).read(&mut buffer)?;
            buffer.truncate(read);
            Ok::<_, std::io::Error>(buffer)
        })
        .await
        .map_err(|error| wit::ProcessError::Failed(error.to_string()))?
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                wit::ProcessError::TimedOut
            }
            _ => wit::ProcessError::Failed(error.to_string()),
        })
    }

    async fn write(
        &mut self,
        connection: Resource<Connection>,
        bytes: Vec<u8>,
    ) -> Result<(), wit::ProcessError> {
        if let Some((link, remote)) = self.remote_of(&connection) {
            return link
                .process(ProcessCall::ConnectionWrite(remote, bytes))
                .await
                .unit();
        }
        let stream = self.stream_of(&connection)?;
        let limit = self.call_remaining().max(Duration::from_millis(1));
        tokio::task::spawn_blocking(move || {
            stream.set_write_timeout(Some(limit))?;
            (&*stream).write_all(&bytes)
        })
        .await
        .map_err(|error| wit::ProcessError::Failed(error.to_string()))?
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                wit::ProcessError::TimedOut
            }
            _ => wit::ProcessError::Failed(error.to_string()),
        })
    }

    async fn drop(&mut self, connection: Resource<Connection>) -> wasmtime::Result<()> {
        if let Some((link, remote)) = self.remote_of(&connection) {
            let _ = link.process(ProcessCall::ConnectionDrop(remote)).await;
        }
        self.table.delete(connection)?;
        Ok(())
    }
}

impl ProcessView<'_> {
    fn stream_of(
        &self,
        connection: &Resource<Connection>,
    ) -> Result<Arc<std::net::TcpStream>, wit::ProcessError> {
        self.table
            .get(connection)
            .map_err(|error| wit::ProcessError::Failed(error.to_string()))?
            .stream
            .clone()
            .ok_or_else(|| wit::ProcessError::Failed("connection is held elsewhere".to_string()))
    }

    /// The supervisor's handle for `connection`, when this view is a guest worker's.
    fn remote_of(
        &self,
        connection: &Resource<Connection>,
    ) -> Option<(Arc<crate::guest_worker::RemoteLink>, u32)> {
        let link = self.access.remote()?;
        let remote = self.table.get(connection).ok()?.remote?;
        Some((Arc::clone(link), remote))
    }

    fn call_remaining(&self) -> Duration {
        match self.access {
            ProcessAccess::Available(ctx) => ctx.remaining(),
            ProcessAccess::Unavailable(_) | ProcessAccess::Remote(_) => Duration::ZERO,
        }
    }
}

/// One `murmur:process` function call a guest worker forwards to the supervisor. A connection is
/// named by the supervisor's handle for it.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum ProcessCall {
    Run(wit::RunRequest),
    Start(wit::StartRequest),
    Find(String),
    Status(String),
    Write(String, Vec<u8>),
    Read(String, u32),
    Stop(String),
    Connect(String, u16),
    ConnectionRead(u32, u32, u32),
    ConnectionWrite(u32, Vec<u8>),
    ConnectionDrop(u32),
}

/// The supervisor's answer to a [`ProcessCall`].
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum ProcessReply {
    Run(Result<wit::RunOutput, wit::ProcessError>),
    Started(Result<wit::Started, wit::ProcessError>),
    Found(Option<String>),
    State(Result<wit::State, wit::ProcessError>),
    Unit(Result<(), wit::ProcessError>),
    Output(Result<wit::Output, wit::ProcessError>),
    Connected(Result<u32, wit::ProcessError>),
    Bytes(Result<Vec<u8>, wit::ProcessError>),
    /// The supervisor would not answer: the call was not in flight, or the worker is lost.
    Unanswered(String),
}

fn mismatched(reply: ProcessReply) -> wit::ProcessError {
    match reply {
        ProcessReply::Unanswered(reason) => wit::ProcessError::Failed(reason),
        other => wit::ProcessError::Failed(format!("unexpected reply {other:?}")),
    }
}

impl ProcessReply {
    fn run(self) -> Result<wit::RunOutput, wit::ProcessError> {
        match self {
            Self::Run(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn started(self) -> Result<wit::Started, wit::ProcessError> {
        match self {
            Self::Started(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn found(self) -> Option<String> {
        match self {
            Self::Found(found) => found,
            _ => None,
        }
    }
    fn state(self) -> Result<wit::State, wit::ProcessError> {
        match self {
            Self::State(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn unit(self) -> Result<(), wit::ProcessError> {
        match self {
            Self::Unit(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn output(self) -> Result<wit::Output, wit::ProcessError> {
        match self {
            Self::Output(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn connected(self) -> Result<u32, wit::ProcessError> {
        match self {
            Self::Connected(result) => result,
            other => Err(mismatched(other)),
        }
    }
    fn bytes(self) -> Result<Vec<u8>, wit::ProcessError> {
        match self {
            Self::Bytes(result) => result,
            other => Err(mismatched(other)),
        }
    }
}

/// The supervisor side of a guest worker's `murmur:process`: answers each [`ProcessCall`] with
/// the call's own access, exactly as the in-process store does, holding the call's connections.
pub(crate) struct RemoteProcessHost {
    access: ProcessAccess,
    table: ResourceTable,
}

impl RemoteProcessHost {
    pub(crate) fn new(access: ProcessAccess) -> Self {
        Self {
            access,
            table: ResourceTable::new(),
        }
    }

    pub(crate) fn begin_call(&mut self, deadline: Duration) {
        self.access.begin_call(deadline);
    }

    pub(crate) async fn answer(&mut self, call: ProcessCall) -> ProcessReply {
        use wit::Host as _;
        let mut view = ProcessView {
            access: &self.access,
            table: &mut self.table,
        };
        match call {
            ProcessCall::Run(request) => ProcessReply::Run(view.run(request).await),
            ProcessCall::Start(request) => ProcessReply::Started(view.start(request).await),
            ProcessCall::Find(name) => ProcessReply::Found(view.find(name).await),
            ProcessCall::Status(handle) => ProcessReply::State(view.status(handle).await),
            ProcessCall::Write(handle, bytes) => {
                ProcessReply::Unit(wit::Host::write(&mut view, handle, bytes).await)
            }
            ProcessCall::Read(handle, max) => {
                ProcessReply::Output(wit::Host::read(&mut view, handle, max).await)
            }
            ProcessCall::Stop(handle) => ProcessReply::State(view.stop(handle).await),
            ProcessCall::Connect(handle, port) => ProcessReply::Connected(
                view.connect(handle, port)
                    .await
                    .map(|resource| resource.rep()),
            ),
            ProcessCall::ConnectionRead(rep, max, timeout) => ProcessReply::Bytes(
                wit::HostConnection::read(&mut view, Resource::new_borrow(rep), max, timeout).await,
            ),
            ProcessCall::ConnectionWrite(rep, bytes) => ProcessReply::Unit(
                wit::HostConnection::write(&mut view, Resource::new_borrow(rep), bytes).await,
            ),
            ProcessCall::ConnectionDrop(rep) => {
                let dropped = wit::HostConnection::drop(&mut view, Resource::new_own(rep)).await;
                ProcessReply::Unit(
                    dropped.map_err(|error| wit::ProcessError::Failed(error.to_string())),
                )
            }
        }
    }
}

/// Registers `murmur:process/process@0.2.0` on a tool linker whose store state yields a
/// [`ProcessView`] through `view`.
pub(crate) fn add_to_linker<T: Send + 'static>(
    linker: &mut wasmtime::component::Linker<T>,
    view: fn(&mut T) -> ProcessView<'_>,
) -> wasmtime::Result<()> {
    wit::add_to_linker::<T, ProcessHost>(linker, view)
}

#[cfg(test)]
mod tests;
