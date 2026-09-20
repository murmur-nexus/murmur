//! Running a `transport: process` capsule's harness through its process driver.
//!
//! One generic runner, for any process driver. Everything harness-specific — the argument list,
//! the environment, the files, what a line of output means, what an exit code means — is the
//! driver's, and this module holds none of it: it resolves and spawns a binary, moves bytes, and
//! records what the driver says happened.
//!
//! # One run
//!
//! 1. **Binary** — `inference.command`, else the driver's `describe().binary`, resolved at
//!    staging by `runtime::resolve_harness_binary`.
//! 2. **Version probe** — [`probe_harness_version`] runs the binary with the driver's
//!    `version-args`; an untested or unreadable version is `W-RUN-002` and nothing more.
//! 3. **Bridge** — `claude_bridge::bind_bridge`, when the capsule declares tools. The driver is
//!    handed the URL, the token and the capsule's **bare** tool names.
//! 4. **System prompt** — [`build_process_system_prompt`], the same value on every launch.
//! 5. **Launch** — [`plan_session`] decides whether this turn starts a conversation or continues
//!    one, then [`build_launch_request`], then the driver's `launch`, which returns the plan.
//! 6. **Spawn** — an environment that starts empty and holds only what `capabilities.env.allow`
//!    delivers plus the plan's `env-set`, in the accessible workdir the capsule's tools see.
//! 7. **Read** — complete stdout lines, batched into `parse`, fed to [`ProcessEventSink`], whose
//!    A2A half writes the task's frames through [`A2aStream`].
//! 8. **End** — the harness is dead whenever this returns, on every path, and the attempt's one
//!    terminal `status` frame is written from [`run_process_inference_loop`] whatever ended it.
//!
//! A run is bounded by inactivity, not by a wall clock: an agent that is working — writing output
//! or calling a tool through the bridge — is never interrupted for taking a long time.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use murmur_artifact::{runtime_warning_link, ConversationMode, InferenceConfig, W_RUN_002};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
};

use uuid::Uuid;

use crate::{
    agent::{AgentLoopExit, PLAN_TOOL_NOTICE, UNTRUSTED_CONTENT_NOTICE},
    errors::RuntimeError,
    harness_session::{new_harness_session_id, HarnessSessionMap},
    hooks::HookRuntime,
    murmur_md::MURMUR_MD_TRUST_NOTICE,
    otel::OtelEmitter,
    process_driver::{
        Bridge, ExitStatus, FailureKind, LaunchPlan, LaunchRequest, ProcessDriver, Session,
        SessionMode,
    },
    runtime::CapsuleStoreState,
    shell,
    streaming::{SseBroadcast, SseEventBuffer},
    trace::{HarnessExit, HarnessStart, TraceWriter},
    types::{ResumeMode, StagedProcessDriver},
    CapabilityPolicy,
};

use super::process_a2a::A2aStream;
use super::process_events::SinkOutcome;
use super::{claude_bridge, inventory::build_tool_inventory, process_events::ProcessEventSink};

/// Loopback address the tool bridge binds on. Always host-local, independent of the capsule
/// server's bind address — the bridge is only ever reached by the harness subprocess.
const BRIDGE_BIND_ADDR: &str = "127.0.0.1";

/// How long a run may go with neither a line of harness output nor a request to the tool bridge
/// before the harness is killed. Not a limit on how long a run may take: a harness that is
/// working resets it every time it says anything.
const PROCESS_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the version probe may take before the version counts as unreadable.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a harness that has produced its terminal event is given to exit on its own after its
/// stdin is closed, before it is killed.
const TERMINAL_EXIT_GRACE: Duration = Duration::from_secs(3);

/// The most complete lines handed to one `parse` call. A burst larger than this is split across
/// calls, in order, so one flood of output cannot make a single driver call unboundedly large.
const PARSE_BATCH_MAX_LINES: usize = 256;

/// Max bytes of harness stderr retained. The harness's stderr is otherwise invisible; its tail is
/// what the driver classifies a run by when the output ended without a terminal event.
const STDERR_TAIL_CAP: usize = 4096;

/// Where a run's driver-written files go, under the internal session workdir.
pub(crate) const FILES_DIR_NAME: &str = ".process-driver";

/// The token the runtime replaces, in the plan's arguments and `env-set` values, with the
/// absolute path of the run's private files directory.
const FILES_DIR_TOKEN: &str = "{files_dir}";

/// Read `task.md` from the workdir, returning empty string if absent.
fn read_task_from_workdir(workdir: &Path) -> String {
    std::fs::read_to_string(workdir.join("task.md")).unwrap_or_default()
}

/// Builds the system prompt the driver is handed. Always carries `MURMUR_MD_TRUST_NOTICE` and
/// `UNTRUSTED_CONTENT_NOTICE`, even when no `inference.system_prompt` is configured, so the
/// harness never receives MURMUR.md-adjacent context or runs tools without the
/// not-instructions / untrusted-content notices.
///
/// It carries no `[Capsule]` block, unlike the http transport's
/// `agent::build_augmented_system_prompt`: on this transport murmur does not render the prompt at
/// all — it hands the driver one string and the harness frames everything around it, so the two
/// unconditional notices, plus `PLAN_TOOL_NOTICE` where `plan_tool_present` says the capsule was
/// granted the plan tool, are the only part that is murmur's to inject. The value must
/// nonetheless stay launch-invariant, for the same prompt caching reason as the http path: it
/// goes at the head of the prompt, and every provider matches its cache on an exact prefix, so a
/// per-launch value here would miss the cache on every request.
pub(super) fn build_process_system_prompt(
    system_prompt: Option<&str>,
    plan_tool_present: bool,
) -> String {
    // The separating newline belongs to the notice, not to the template below, so an ungranted
    // capsule's prompt carries no blank line where the guidance would go. A single stray byte here
    // is enough to miss the provider's cached prefix on every request.
    let plan = if plan_tool_present {
        format!("\n{PLAN_TOOL_NOTICE}")
    } else {
        String::new()
    };
    let notices = format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}{plan}");
    match system_prompt.filter(|sp| !sp.is_empty()) {
        Some(sp) => format!("{notices}\n\n{sp}"),
        None => notices,
    }
}

/// What one turn knows about the conversation it belongs to, before the harness is asked for
/// anything.
///
/// Built once per task, in `agent::run_agent_loop`, from the launch's map and the task's own
/// context id. `continue_conversation` is the same rule the `http` path's `load_recorded_history`
/// uses — `lifecycle.conversation: threaded`, or any launch under `mur run --resume` — so
/// `--resume` keeps its meaning as a one-launch override of the capsule's own policy.
pub(crate) struct HarnessSessionPolicy {
    pub(crate) map: Arc<HarnessSessionMap>,
    pub(crate) context_id: Option<String>,
    pub(crate) continue_conversation: bool,
}

/// Which session this turn launches with, and what it may remember afterwards.
pub(super) struct SessionPlan {
    pub(super) session: Session,
    /// The context to store this turn's session id under, or `None` when nothing is remembered:
    /// a turn that starts every conversation from nothing, and a turn whose context id is
    /// unresolved.
    pub(super) context_key: Option<String>,
}

/// Whether this turn continues the conversation its context already has, or starts a new one.
///
/// `lifecycle.conversation: threaded` is the capsule's own policy. `mur run --resume` overrides it
/// for one launch, which is what keeps `--resume` meaning the same thing on both transports: the
/// `http` path's `load_recorded_history` reloads a message list on exactly this condition.
pub(crate) fn continues_conversation(mode: ConversationMode, resume: Option<ResumeMode>) -> bool {
    matches!(mode, ConversationMode::Threaded) || resume.is_some()
}

/// Decide what to hand the harness for this turn.
///
/// | `continue_conversation` | The map holds an id for this context | Launched with |
/// |---|---|---|
/// | no | not read | `mode: new`, a fresh id |
/// | yes | no | `mode: new`, a fresh id |
/// | yes | yes | `mode: resume`, the stored id |
///
/// A turn with no context id resolved launches `mode: new` and remembers nothing: there is
/// nothing to key a conversation on.
pub(super) fn plan_session(policy: &HarnessSessionPolicy) -> SessionPlan {
    let new = |context_key: Option<String>| SessionPlan {
        session: Session {
            id: new_harness_session_id(),
            mode: SessionMode::New,
        },
        context_key,
    };
    let Some(context_id) = policy.context_id.as_deref().filter(|id| !id.is_empty()) else {
        return new(None);
    };
    if !policy.continue_conversation {
        return new(None);
    }
    match policy.map.get(context_id) {
        Some(id) => SessionPlan {
            session: Session {
                id,
                mode: SessionMode::Resume,
            },
            context_key: Some(context_id.to_string()),
        },
        None => new(Some(context_id.to_string())),
    }
}

/// How a session mode is spelled in the trace, and in the args a driver's `launch` builds from it.
fn session_mode_name(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::New => "new",
        SessionMode::Resume => "resume",
    }
}

/// The session one run is driving, and everything needed to remember it or to say it is gone.
///
/// Held by [`ProcessEventSink`], which is where both answers arrive: a `session-started` event
/// naming the conversation, and a `turn-failed` that never came with one.
pub(super) struct RunSession {
    map: Arc<HarnessSessionMap>,
    /// The context this run's session id is stored under, or `None` when nothing is remembered.
    context_key: Option<String>,
    /// What the context id is called in a diagnostic, including when it resolved to nothing.
    context_id: String,
    /// The id handed to the driver for this run.
    id: String,
    mode: SessionMode,
    harness: String,
    driver: String,
}

impl RunSession {
    pub(super) fn new(
        plan: &SessionPlan,
        policy: &HarnessSessionPolicy,
        harness: &str,
        driver: &str,
    ) -> Self {
        Self {
            map: Arc::clone(&policy.map),
            context_key: plan.context_key.clone(),
            context_id: policy
                .context_id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| crate::conversation::UNRESOLVED_CONTEXT.to_string()),
            id: plan.session.id.clone(),
            mode: plan.session.mode,
            harness: harness.to_string(),
            driver: driver.to_string(),
        }
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn mode(&self) -> SessionMode {
        self.mode
    }

    /// Store `id` as this context's conversation.
    ///
    /// Whatever the harness reported wins over whatever the runtime asked for: a harness that
    /// mints its own session ids says so with `session-started`, and that is the id it will answer
    /// to next time.
    pub(super) fn remember(&self, id: &str) {
        let Some(context_key) = self.context_key.as_deref() else {
            return;
        };
        self.map.put(context_key, id, &self.harness, &self.driver);
    }

    /// The failure a resumed turn produces when the harness never reported the session it was
    /// handed.
    ///
    /// `None` for every other ending. A turn launched `mode: new` established nothing to lose; a
    /// turn that reported a `session-started` was continuing something; and `auth`, `quota`,
    /// `max-turns` and `canceled` each name a cause of their own — an expired login is not a
    /// missing conversation.
    pub(super) fn session_gone(
        &self,
        kind: FailureKind,
        reported_session: bool,
        message: &str,
    ) -> Option<RuntimeError> {
        if self.mode != SessionMode::Resume
            || reported_session
            || !matches!(kind, FailureKind::HarnessError | FailureKind::Other)
        {
            return None;
        }
        Some(RuntimeError::HarnessSessionGone {
            context_id: self.context_id.clone(),
            session_id: self.id.clone(),
            harness: self.harness.clone(),
            path: self
                .context_key
                .as_deref()
                .and_then(|context| self.map.entry_path(context)),
            detail: message.to_string(),
        })
    }
}

/// Everything the driver needs to plan one run.
///
/// `model` is `none` when the manifest set none, so a driver never has to decide what an empty
/// model means. The bridge's tool names go across bare: naming them the way a harness needs is
/// the driver's job, and stripping that naming again in `parse` is too.
pub(super) fn build_launch_request(
    inference: &InferenceConfig,
    system_prompt: Option<&str>,
    plan_tool_present: bool,
    bridge: Option<&claude_bridge::BridgeHandle>,
    session: Session,
    harness_version: Option<String>,
    task: String,
) -> LaunchRequest {
    LaunchRequest {
        model: Some(inference.model.trim().to_string()).filter(|m| !m.is_empty()),
        system_prompt: build_process_system_prompt(system_prompt, plan_tool_present),
        config: inference
            .driver
            .as_ref()
            .and_then(|driver| driver.config.clone()),
        bridge: bridge.map(|b| Bridge {
            server_name: claude_bridge::BRIDGE_SERVER_NAME.to_string(),
            url: b.url.clone(),
            bearer_token: b.token.clone(),
            tool_names: b.tool_names.clone(),
        }),
        session,
        harness_version,
        task,
    }
}

/// Replace [`FILES_DIR_TOKEN`] wherever it appears.
fn substitute_files_dir(value: &str, files_dir: &str) -> String {
    value.replace(FILES_DIR_TOKEN, files_dir)
}

/// The environment the harness starts with: nothing, then the host values of the names
/// `capabilities.env.allow` declares, then the plan's `env-set`, which wins.
///
/// The same mechanism every WASM guest gets, credential backstop included — a credential-shaped
/// name in `env.allow` is refused at staging, so the harness cannot be handed a key that way
/// either. Native shell subprocesses use a different, baseline-derived environment and are
/// deliberately not the model here: a harness is a guest, not a shell command.
pub(super) fn build_harness_env(
    policy: &CapabilityPolicy,
    env_set: &[(String, String)],
    files_dir: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = shell::build_declared_env(policy);
    for (name, value) in env_set {
        if !crate::process_driver::is_usable_env_name(name) {
            return Err(format!(
                "the launch plan's env-set names {name:?}, which is not a usable variable name"
            ));
        }
        env.insert(name.clone(), substitute_files_dir(value, files_dir));
    }
    Ok(env)
}

/// A driver file name must name a file inside the run's private directory and nothing else.
fn check_driver_file_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(format!(
            "the launch plan's files name {name:?}, which is not a plain file name"
        ));
    }
    Ok(())
}

/// Whether a probed version line names a version the driver was tested against.
///
/// The line is whatever the harness printed, which is usually its own name and then a version, so
/// every whitespace-separated token is a candidate. One leading `v` is dropped, because a harness
/// that prints `v1.2.3` and a driver that lists `1.2.3` mean the same release.
fn version_is_tested(line: &str, tested_versions: &[String]) -> bool {
    line.split_whitespace()
        .map(|token| token.strip_prefix('v').unwrap_or(token))
        .any(|token| tested_versions.iter().any(|tested| tested == token))
}

/// What the version probe found.
enum VersionProbe {
    /// The first non-empty line the harness printed, trimmed.
    Read(String),
    /// No version could be read, and why.
    Unavailable(String),
}

/// Run the harness with the driver's `version-args` and read back what it printed.
///
/// Empty stdin, the same base environment and working directory as the run itself, and a short
/// bound of its own: a harness that hangs on `--version` must not hold up the run.
async fn probe_harness_version(
    binary: &Path,
    version_args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> VersionProbe {
    let mut command = Command::new(binary);
    command
        .args(version_args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return VersionProbe::Unavailable(format!("it could not be run: {error}")),
    };
    let output = match tokio::time::timeout(VERSION_PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return VersionProbe::Unavailable(format!("it could not be run: {error}"))
        }
        Err(_) => {
            return VersionProbe::Unavailable(format!(
                "it did not answer within {}s",
                VERSION_PROBE_TIMEOUT.as_secs()
            ))
        }
    };
    if !output.status.success() {
        return VersionProbe::Unavailable("it exited non-zero".to_string());
    }
    match first_non_empty_line(&output.stdout).or_else(|| first_non_empty_line(&output.stderr)) {
        Some(line) => VersionProbe::Read(line),
        None => VersionProbe::Unavailable("it printed nothing".to_string()),
    }
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// The `W-RUN-002` body: what was read, or why nothing was, and what the driver was tested against.
fn untested_version_message(probe: &VersionProbe, tested_versions: &[String]) -> String {
    let tested = if tested_versions.is_empty() {
        "the driver names no tested versions".to_string()
    } else {
        format!("tested against {}", tested_versions.join(", "))
    };
    match probe {
        VersionProbe::Read(line) => format!(
            "the harness reports version '{line}', which this process driver was not tested \
             against ({tested})"
        ),
        VersionProbe::Unavailable(reason) => format!(
            "the harness version could not be read: {reason}; this process driver is {tested}"
        ),
    }
}

/// The run's private directory for the files the driver asked for, removed when the run ends.
struct FilesDir(PathBuf);

impl FilesDir {
    /// Create `<session workdir>/.process-driver/<run id>/`, readable only by this user.
    fn create(workdir: &Path) -> Result<Self, String> {
        let dir = workdir
            .join(FILES_DIR_NAME)
            .join(Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("failed to create the harness files directory: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("failed to restrict the harness files directory: {e}"))?;
        }
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for FilesDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write the plan's files into the run's private directory, each readable only by this user.
fn write_driver_files(
    dir: &Path,
    files: &[crate::process_driver::DriverFile],
) -> Result<Vec<String>, String> {
    let mut names = Vec::with_capacity(files.len());
    for file in files {
        check_driver_file_name(&file.name)?;
        let path = dir.join(&file.name);
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut handle = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("failed to write the harness file {:?}: {e}", file.name))?;
            handle
                .write_all(file.contents.as_bytes())
                .map_err(|e| format!("failed to write the harness file {:?}: {e}", file.name))?;
        }
        #[cfg(not(unix))]
        std::fs::write(&path, file.contents.as_bytes())
            .map_err(|e| format!("failed to write the harness file {:?}: {e}", file.name))?;
        names.push(file.name.clone());
    }
    Ok(names)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_process_inference_loop(
    // `store_state` (shared &) lets the tool bridge execute declared tool artifacts through the
    // same dispatch the HTTP path uses, and carries the process driver this session staged.
    store_state: &CapsuleStoreState,
    workdir: &Path,
    inference: &InferenceConfig,
    system_prompt: Option<String>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    task_id: Option<String>,
    sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    context_id: Option<String>,
    accessible_workdir: &Path,
    _name: &str,
    _version: &str,
    session_policy: HarnessSessionPolicy,
) -> Result<AgentLoopExit, RuntimeError> {
    // Every path below leaves through the one `finish` call at the end, which is what makes the
    // attempt's terminal `status` frame arrive exactly once — including on the paths that return
    // an error, where a client would otherwise wait on a frame no one was going to write.
    let mut a2a = A2aStream::new(
        sse,
        task_id,
        context_id,
        Arc::clone(&store_state.a2a_chunks_emitted),
    );
    let outcome = run_attempt(
        store_state,
        workdir,
        inference,
        system_prompt,
        hooks,
        trace,
        otel,
        accessible_workdir,
        session_policy,
        &mut a2a,
    )
    .await;
    a2a.finish(&outcome).await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_attempt(
    store_state: &CapsuleStoreState,
    workdir: &Path,
    inference: &InferenceConfig,
    system_prompt: Option<String>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    accessible_workdir: &Path,
    session_policy: HarnessSessionPolicy,
    a2a: &mut A2aStream,
) -> Result<AgentLoopExit, RuntimeError> {
    let Some(staged) = store_state.process_driver.clone() else {
        return Err(RuntimeError::DriverNotConfigured);
    };

    // The trace's `session_start`/`session_end` frame and the `on-session-start`/`on-session-end`
    // hook dispatch both fire once per launch, from runtime.rs around the task loop. This function
    // runs one attempt of one task and writes neither.
    otel.begin_session(None);

    let outcome = run_harness(
        store_state,
        &staged,
        workdir,
        inference,
        system_prompt.as_deref(),
        hooks,
        trace,
        otel,
        accessible_workdir,
        session_policy,
        a2a,
    )
    .await;

    otel.emit_session_end(if outcome.is_ok() { "ok" } else { "failed" })
        .await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn run_harness(
    store_state: &CapsuleStoreState,
    staged: &StagedProcessDriver,
    workdir: &Path,
    inference: &InferenceConfig,
    system_prompt: Option<&str>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    accessible_workdir: &Path,
    session_policy: HarnessSessionPolicy,
    a2a: &mut A2aStream,
) -> Result<AgentLoopExit, RuntimeError> {
    // One driver instance for the whole run, so `parse` may answer from what `launch` was given.
    let mut driver = ProcessDriver::instantiate(
        &store_state.engine,
        &staged.component,
        &staged.name,
        &staged.version,
    )
    .await?;

    let description = &staged.description;
    let base_env = shell::build_declared_env(&store_state.capability_policy);
    let probe = probe_harness_version(
        &staged.binary,
        &description.version_args,
        &base_env,
        accessible_workdir,
    )
    .await;
    let (harness_version, version_tested) = match &probe {
        VersionProbe::Read(line) => (
            Some(line.clone()),
            version_is_tested(line, &description.tested_versions),
        ),
        VersionProbe::Unavailable(_) => (None, false),
    };
    if !version_tested {
        let message = untested_version_message(&probe, &description.tested_versions);
        crate::runtime_err!(
            "[capsule-runtime] warning[{W_RUN_002}]: {message} ({})",
            runtime_warning_link(W_RUN_002)
        );
        let _ = trace.write_harness_warning(W_RUN_002, &message).await;
    }

    let inventory = build_tool_inventory(workdir, inference.system_prompt_artifact.as_deref());
    let bridge = claude_bridge::bind_bridge(BRIDGE_BIND_ADDR, &inventory).await;

    // task.md lives in accessible_workdir (where the agent's own tools are preopened), not the
    // internal session workdir. Fenced on the same condition as the http path, from the same
    // function, so the transport a capsule runs on does not decide whether an untrusted payload is
    // marked. The fence source is discarded: this transport keeps no conversation record.
    let (task, _fence_source) = super::fence_task_payload(
        store_state.current_task_provenance,
        read_task_from_workdir(accessible_workdir),
    );

    let session_plan = plan_session(&session_policy);
    let session = RunSession::new(
        &session_plan,
        &session_policy,
        &description.harness,
        &staged.name,
    );
    let request = build_launch_request(
        inference,
        system_prompt,
        store_state.capability_policy.plan_submit,
        bridge.as_ref(),
        session_plan.session.clone(),
        harness_version.clone(),
        task,
    );
    let launch_error = |message: String| RuntimeError::ProcessDriverCallFailed {
        name: staged.name.clone(),
        version: staged.version.clone(),
        call: "launch".to_string(),
        message,
    };
    let plan: LaunchPlan = driver.launch(request).await?.map_err(launch_error)?;

    let files_dir = FilesDir::create(workdir).map_err(launch_error)?;
    let files_dir_path = files_dir.path().to_string_lossy().into_owned();
    let files = write_driver_files(files_dir.path(), &plan.files).map_err(launch_error)?;
    let args: Vec<String> = plan
        .args
        .iter()
        .map(|arg| substitute_files_dir(arg, &files_dir_path))
        .collect();
    let env = build_harness_env(
        &store_state.capability_policy,
        &plan.env_set,
        &files_dir_path,
    )
    .map_err(launch_error)?;

    let stdin_bytes = plan.stdin.as_ref().map(Vec::len).unwrap_or(0);
    let _ = trace
        .write_harness_start(HarnessStart {
            driver: staged.name.clone(),
            driver_version: staged.version.clone(),
            harness: description.harness.clone(),
            binary: staged.binary.to_string_lossy().into_owned(),
            binary_source: staged.binary_source.clone(),
            harness_version,
            version_tested,
            harness_session_id: session.id().to_string(),
            session_mode: session_mode_name(session.mode()).to_string(),
            args_count: args.len(),
            env_names: env.keys().cloned().collect(),
            files,
            bridge_tools: bridge
                .as_ref()
                .map(|b| b.tool_names.clone())
                .unwrap_or_default(),
            stdin_bytes,
            keep_stdin_open: plan.keep_stdin_open,
        })
        .await;

    let mut sink = ProcessEventSink::new(workdir, inference.max_turns, session, a2a);
    let outcome = drive_harness(
        store_state,
        staged,
        bridge.as_ref(),
        &args,
        &env,
        accessible_workdir,
        &plan,
        &mut driver,
        &mut sink,
        hooks,
        trace,
        otel,
    )
    .await;
    sink.finish(trace).await;
    outcome
}

/// What ended the reading loop, before the harness has been reaped.
enum RunEnd {
    /// The driver reported a terminal event.
    Terminal(Result<(), RuntimeError>),
    /// A turn opened past the attempt's budget.
    TurnBudget(RuntimeError),
    /// The harness closed stdout without a terminal event.
    Eof,
    /// Neither stdout nor the bridge said anything for the whole window.
    Inactive,
    /// A call into the driver failed, or the bridge stopped serving.
    Failed(RuntimeError),
}

#[allow(clippy::too_many_arguments)]
async fn drive_harness(
    store_state: &CapsuleStoreState,
    staged: &StagedProcessDriver,
    bridge: Option<&claude_bridge::BridgeHandle>,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
    plan: &LaunchPlan,
    driver: &mut ProcessDriver,
    sink: &mut ProcessEventSink<'_>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
) -> Result<AgentLoopExit, RuntimeError> {
    let spawned_at = Instant::now();
    let mut child = Command::new(&staged.binary)
        .args(args)
        // The environment is exactly what was built: no host variable reaches the harness unless
        // `capabilities.env.allow` declared it or the driver's plan set it.
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The backstop to every explicit kill below: a panic or an early return still reaps it.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            RuntimeError::AgentLoopFailed(format!(
                "failed to spawn the harness {}: {e}",
                staged.binary.display()
            ))
        })?;

    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_drain = child.stderr.take().map(|stderr| {
        let buf = Arc::clone(&stderr_tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut tail) = buf.lock() {
                    tail.push_str(&line);
                    tail.push('\n');
                    // Trim whole leading lines to stay under the cap without splitting a UTF-8
                    // char (newlines are ASCII, so slicing after one is always safe).
                    while tail.len() > STDERR_TAIL_CAP {
                        match tail.find('\n') {
                            Some(nl) => *tail = tail[nl + 1..].to_string(),
                            None => break,
                        }
                    }
                }
            }
        })
    });

    // Reading stdout on its own task is what makes the loop below cancel-safe: `recv` can be
    // dropped by the inactivity timer or the bridge without losing a byte of a half-read line.
    let (line_tx, mut line_rx) = mpsc::channel::<String>(1024);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(read_lines(stdout, line_tx));
    }

    let mut stdin = child.stdin.take();
    if let Some(bytes) = plan.stdin.as_ref() {
        if let Some(handle) = stdin.as_mut() {
            if let Err(error) = handle.write_all(bytes).await {
                let exit = finish_child(&mut child, stdin.take(), None).await;
                write_harness_exit(trace, &exit, "driver_error", spawned_at).await;
                return Err(RuntimeError::AgentLoopFailed(format!(
                    "failed to write to the harness's stdin: {error}"
                )));
            }
            let _ = handle.flush().await;
        }
    }
    if !plan.keep_stdin_open {
        stdin = None;
    }

    let inactivity = inactivity_timeout();
    let last_activity = Arc::new(Mutex::new(tokio::time::Instant::now()));
    let bump = {
        let clock = Arc::clone(&last_activity);
        move || {
            if let Ok(mut at) = clock.lock() {
                *at = tokio::time::Instant::now();
            }
        }
    };
    // One long-lived future: re-creating it per iteration would drop a connection mid tool call.
    let bridge_future = async {
        match bridge {
            Some(handle) => handle.serve(store_state, &bump).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(bridge_future);

    let end = loop {
        let deadline = match last_activity.lock() {
            Ok(at) => *at + inactivity,
            Err(_) => tokio::time::Instant::now() + inactivity,
        };
        let first = tokio::select! {
            biased;
            line = line_rx.recv() => line,
            () = &mut bridge_future => break RunEnd::Failed(RuntimeError::AgentLoopFailed(
                "the tool bridge stopped accepting connections".to_string(),
            )),
            () = tokio::time::sleep_until(deadline) => {
                // A bridge request may have moved the clock while this timer was armed.
                let idle = last_activity
                    .lock()
                    .map(|at| at.elapsed())
                    .unwrap_or(inactivity);
                if idle >= inactivity {
                    break RunEnd::Inactive;
                }
                continue;
            }
        };
        let Some(first) = first else {
            break RunEnd::Eof;
        };
        if let Ok(mut at) = last_activity.lock() {
            *at = tokio::time::Instant::now();
        }
        let mut lines = vec![first];
        while lines.len() < PARSE_BATCH_MAX_LINES {
            match line_rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(_) => break,
            }
        }
        let events = match driver.parse(lines).await {
            Ok(events) => events,
            Err(error) => break RunEnd::Failed(error),
        };
        match sink.consume(events, hooks, trace, otel).await {
            SinkOutcome::Continue => {}
            SinkOutcome::Ended => break RunEnd::Terminal(Ok(())),
            SinkOutcome::Failed(error) => break RunEnd::Terminal(Err(error)),
            SinkOutcome::TurnBudgetExceeded(error) => break RunEnd::TurnBudget(error),
        }
    };

    // Whatever ended the loop, the harness does not outlive this function. A run that produced
    // its terminal event is given the grace period to exit on its own; one that is still mid-turn
    // is killed outright, because waiting on it is waiting on spend.
    let grace = match &end {
        RunEnd::Terminal(_) | RunEnd::Eof => Some(TERMINAL_EXIT_GRACE),
        RunEnd::TurnBudget(_) | RunEnd::Inactive | RunEnd::Failed(_) => None,
    };
    let exit = finish_child(&mut child, stdin, grace).await;
    if let Some(handle) = stderr_drain {
        let _ = tokio::time::timeout(Duration::from_millis(200), handle).await;
    }

    match end {
        RunEnd::Terminal(result) => {
            write_harness_exit(trace, &exit, "terminal", spawned_at).await;
            result.map(|()| AgentLoopExit::Ok)
        }
        RunEnd::TurnBudget(error) => {
            write_harness_exit(trace, &exit, "max_turns", spawned_at).await;
            Err(error)
        }
        RunEnd::Inactive => {
            write_harness_exit(trace, &exit, "inactivity", spawned_at).await;
            Err(RuntimeError::ProcessHarnessInactive {
                seconds: inactivity.as_secs(),
            })
        }
        RunEnd::Failed(error) => {
            write_harness_exit(trace, &exit, "driver_error", spawned_at).await;
            Err(error)
        }
        RunEnd::Eof => {
            write_harness_exit(trace, &exit, "eof", spawned_at).await;
            let status = ExitStatus {
                code: exit.code,
                signal: exit.signal,
                stderr_tail: stderr_tail.lock().map(|t| t.clone()).unwrap_or_default(),
                // Nothing here interrupts a turn on purpose, so a classified exit is never a
                // cancellation.
                interrupted: false,
                saw_terminal: false,
            };
            let event = driver.classify_exit(status).await?;
            match sink.consume(vec![event], hooks, trace, otel).await {
                SinkOutcome::Ended => Ok(AgentLoopExit::Ok),
                SinkOutcome::Failed(error) | SinkOutcome::TurnBudgetExceeded(error) => Err(error),
                SinkOutcome::Continue => Err(RuntimeError::ProcessDriverCallFailed {
                    name: staged.name.clone(),
                    version: staged.version.clone(),
                    call: "classify-exit".to_string(),
                    message: "returned an event that is neither turn-end nor turn-failed"
                        .to_string(),
                }),
            }
        }
    }
}

/// How long a run may go silent. Overridable in debug builds so the inactivity tests do not have
/// to wait out the real window; release builds read nothing from the environment.
fn inactivity_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(ms) = std::env::var("MURMUR_DEBUG_PROCESS_INACTIVITY_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }
    PROCESS_INACTIVITY_TIMEOUT
}

/// How the harness process ended.
struct ExitReport {
    code: Option<i32>,
    signal: Option<i32>,
    killed: bool,
}

/// Close the harness's stdin, give it `grace` to exit on its own, then kill it and reap it.
///
/// `grace` of `None` kills immediately. Returns once the process is gone, on every path.
async fn finish_child(
    child: &mut Child,
    stdin: Option<ChildStdin>,
    grace: Option<Duration>,
) -> ExitReport {
    // Dropping the handle closes the pipe, which is how a harness reading stdin learns the run is
    // over. Done before the wait, so the grace period is time it can actually use.
    drop(stdin);
    let waited = match grace {
        Some(grace) => tokio::time::timeout(grace, child.wait()).await.ok(),
        None => None,
    };
    match waited {
        Some(Ok(status)) => ExitReport {
            code: status.code(),
            signal: exit_signal(&status),
            killed: false,
        },
        _ => {
            let _ = child.kill().await;
            let status = child.wait().await.ok();
            ExitReport {
                code: status.as_ref().and_then(std::process::ExitStatus::code),
                signal: status.as_ref().and_then(exit_signal),
                killed: true,
            }
        }
    }
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

async fn write_harness_exit(
    trace: &mut TraceWriter,
    exit: &ExitReport,
    cause: &str,
    spawned_at: Instant,
) {
    let _ = trace
        .write_harness_exit(HarnessExit {
            code: exit.code,
            signal: exit.signal,
            cause: cause.to_string(),
            killed: exit.killed,
            duration_ms: spawned_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        })
        .await;
}

/// Send each complete line of `stdout` down `tx`.
///
/// A line ends at `\n`; the terminator and one trailing `\r` are dropped, invalid UTF-8 is
/// replaced, and an empty line is passed through — what a blank line means is the driver's to
/// decide. Trailing bytes with no terminator at EOF are not a line and are dropped: a driver
/// reading complete lines would be handed a fragment.
async fn read_lines(stdout: tokio::process::ChildStdout, tx: mpsc::Sender<String>) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.last() != Some(&b'\n') {
            return;
        }
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if tx
            .send(String::from_utf8_lossy(&buf).into_owned())
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Launch-invariance: the system prompt is a pure function of its argument, with no path,
    /// session id or other per-launch value anywhere in it, so two launches of the same capsule
    /// hand the harness a byte-identical prefix. See the builder's own doc comment.
    #[test]
    fn process_system_prompt_is_launch_invariant() {
        for arg in [None, Some("You are a helpful assistant.")] {
            let first = build_process_system_prompt(arg, false);
            let second = build_process_system_prompt(arg, false);
            assert_eq!(first, second, "same input must yield the same string");
            assert!(first.contains(MURMUR_MD_TRUST_NOTICE));
            assert!(first.contains(UNTRUSTED_CONTENT_NOTICE));
            assert!(
                !first.contains("[Capsule]"),
                "the process transport injects no [Capsule] block; got:\n{first}"
            );
            assert!(
                !first.split_whitespace().any(|t| t.starts_with('/')),
                "no host path may appear in the prompt; got:\n{first}"
            );
            assert!(
                !first.contains("ses_"),
                "no session id may appear in the prompt; got:\n{first}"
            );
        }
    }

    #[test]
    fn process_system_prompt_carries_trust_notice_with_no_custom_prompt() {
        let prompt = build_process_system_prompt(None, false);
        assert_eq!(
            prompt,
            format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}")
        );
    }

    #[test]
    fn process_system_prompt_carries_trust_notice_alongside_custom_prompt() {
        let prompt = build_process_system_prompt(Some("You are a helpful assistant."), false);
        assert!(prompt.contains(MURMUR_MD_TRUST_NOTICE));
        assert!(prompt.contains(UNTRUSTED_CONTENT_NOTICE));
        assert!(prompt.contains("You are a helpful assistant."));
        let notice_pos = prompt.find(MURMUR_MD_TRUST_NOTICE).unwrap();
        let untrusted_pos = prompt.find(UNTRUSTED_CONTENT_NOTICE).unwrap();
        let custom_pos = prompt.find("You are a helpful assistant.").unwrap();
        assert!(notice_pos < custom_pos);
        assert!(untrusted_pos < custom_pos);
    }

    #[test]
    fn process_system_prompt_treats_empty_string_as_absent() {
        let prompt = build_process_system_prompt(Some(""), false);
        assert_eq!(
            prompt,
            format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}")
        );
    }

    fn process_inference(model: &str, config: Option<&str>) -> InferenceConfig {
        InferenceConfig {
            transport: "process".into(),
            model: model.into(),
            driver: Some(murmur_artifact::InferenceDriver {
                artifact: "a-process-driver".into(),
                config: config.map(str::to_string),
            }),
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
            max_session_tokens: None,
        }
    }

    /// A policy over a map that keeps nothing on disk, which is all `plan_session` reads.
    fn policy(context_id: Option<&str>, continue_conversation: bool) -> HarnessSessionPolicy {
        HarnessSessionPolicy {
            map: Arc::new(HarnessSessionMap::new(None, Path::new("/tmp"))),
            context_id: context_id.map(str::to_string),
            continue_conversation,
        }
    }

    fn run_session(plan: &SessionPlan, policy: &HarnessSessionPolicy) -> RunSession {
        RunSession::new(plan, policy, "fixture-harness", "fixture-process-driver")
    }

    /// `lifecycle.conversation: stateless`, the default: every task starts a conversation from
    /// nothing, and the map is neither read nor written.
    #[test]
    fn harness_session_stateless_always_launches_new() {
        let policy = policy(Some("ctx_1"), false);
        policy.map.put("ctx_1", "remembered", "h", "d");

        let plan = plan_session(&policy);
        assert_eq!(plan.session.mode, SessionMode::New);
        assert_ne!(plan.session.id, "remembered");
        assert_eq!(plan.context_key, None);

        // And the next one gets a different id again.
        assert_ne!(plan_session(&policy).session.id, plan.session.id);
    }

    #[test]
    fn harness_session_threaded_with_no_entry_launches_new() {
        let policy = policy(Some("ctx_1"), true);
        let plan = plan_session(&policy);
        assert_eq!(plan.session.mode, SessionMode::New);
        assert_eq!(plan.context_key.as_deref(), Some("ctx_1"));
    }

    #[test]
    fn harness_session_threaded_with_an_entry_launches_resume() {
        let policy = policy(Some("ctx_1"), true);
        policy.map.put("ctx_1", "remembered", "h", "d");

        let plan = plan_session(&policy);
        assert_eq!(plan.session.mode, SessionMode::Resume);
        assert_eq!(plan.session.id, "remembered");
        assert_eq!(plan.context_key.as_deref(), Some("ctx_1"));
    }

    /// Another context's entry is another conversation.
    #[test]
    fn harness_session_reads_only_its_own_context() {
        let policy = policy(Some("ctx_2"), true);
        policy.map.put("ctx_1", "remembered", "h", "d");
        assert_eq!(plan_session(&policy).session.mode, SessionMode::New);
    }

    #[test]
    fn harness_session_an_unresolved_context_launches_new_and_remembers_nothing() {
        for context_id in [None, Some("")] {
            let policy = policy(context_id, true);
            let plan = plan_session(&policy);
            assert_eq!(plan.session.mode, SessionMode::New);
            assert_eq!(plan.context_key, None);

            run_session(&plan, &policy).remember("anything");
            assert_eq!(policy.map.get(""), None);
        }
    }

    /// `mur run --resume` is a launch-scoped override of `lifecycle.conversation`: a `stateless`
    /// capsule continues its context's session for that launch only, exactly as an `http` capsule
    /// loads its record for that launch only.
    #[test]
    fn harness_session_resume_overrides_stateless() {
        assert!(!continues_conversation(ConversationMode::Stateless, None));
        assert!(continues_conversation(
            ConversationMode::Stateless,
            Some(ResumeMode::Full)
        ));
        assert!(continues_conversation(ConversationMode::Threaded, None));

        // And the override reaches the plan: the stored id is handed back, not a fresh one.
        let policy = policy(
            Some("ctx_1"),
            continues_conversation(ConversationMode::Stateless, Some(ResumeMode::Full)),
        );
        policy.map.put("ctx_1", "remembered", "h", "d");
        let plan = plan_session(&policy);
        assert_eq!(plan.session.mode, SessionMode::Resume);
        assert_eq!(plan.session.id, "remembered");
    }

    #[test]
    fn harness_session_remembers_what_the_harness_reported() {
        let policy = policy(Some("ctx_1"), true);
        let plan = plan_session(&policy);
        run_session(&plan, &policy).remember("harness-chose-this");
        assert_eq!(
            policy.map.get("ctx_1").as_deref(),
            Some("harness-chose-this")
        );
    }

    /// The one ending that means the harness does not hold the conversation this context names.
    #[test]
    fn harness_session_gone_only_for_a_resume_the_harness_never_acknowledged() {
        let policy = policy(Some("ctx_1"), true);
        policy.map.put("ctx_1", "remembered", "h", "d");
        let plan = plan_session(&policy);
        let session = run_session(&plan, &policy);

        for kind in [FailureKind::HarnessError, FailureKind::Other] {
            let gone = session
                .session_gone(kind, false, "no conversation found")
                .expect("a resume with no session-started is a missing conversation");
            let message = gone.to_string();
            assert!(message.contains("ctx_1"), "{message}");
            assert!(message.contains("remembered"), "{message}");
            assert!(message.contains("fixture-harness"), "{message}");
            assert!(message.contains("no conversation found"), "{message}");
        }

        // A harness that named its session was continuing something, whatever went wrong after.
        for kind in [FailureKind::HarnessError, FailureKind::Other] {
            assert!(session.session_gone(kind, true, "boom").is_none());
        }

        // Every other kind names its own cause.
        for kind in [
            FailureKind::Auth,
            FailureKind::Quota,
            FailureKind::MaxTurns,
            FailureKind::Canceled,
        ] {
            assert!(session.session_gone(kind, false, "boom").is_none());
        }
    }

    #[test]
    fn harness_session_gone_never_fires_on_a_new_session() {
        let policy = policy(Some("ctx_1"), true);
        let plan = plan_session(&policy);
        assert_eq!(plan.session.mode, SessionMode::New);
        let session = run_session(&plan, &policy);
        assert!(session
            .session_gone(FailureKind::HarnessError, false, "boom")
            .is_none());
    }

    #[test]
    fn session_mode_names_match_the_wit_spelling() {
        assert_eq!(session_mode_name(SessionMode::New), "new");
        assert_eq!(session_mode_name(SessionMode::Resume), "resume");
    }

    fn new_session(id: &str) -> Session {
        Session {
            id: id.to_string(),
            mode: SessionMode::New,
        }
    }

    #[test]
    fn launch_request_carries_the_prompt_config_and_a_new_session() {
        let inference = process_inference("a-model", Some(r#"{"profile":"work"}"#));
        let request = build_launch_request(
            &inference,
            Some("be brief"),
            true,
            None,
            new_session("sess-1"),
            Some("harness 1.0.0".to_string()),
            "do the thing".to_string(),
        );
        assert_eq!(request.model.as_deref(), Some("a-model"));
        assert_eq!(
            request.system_prompt,
            build_process_system_prompt(Some("be brief"), true),
            "the driver must be handed exactly what build_process_system_prompt produced"
        );
        assert_eq!(request.config.as_deref(), Some(r#"{"profile":"work"}"#));
        assert_eq!(request.session.id, "sess-1");
        assert!(matches!(request.session.mode, SessionMode::New));
        assert_eq!(request.harness_version.as_deref(), Some("harness 1.0.0"));
        assert_eq!(request.task, "do the thing");
        assert!(request.bridge.is_none());
    }

    #[test]
    fn launch_request_omits_an_empty_model() {
        for model in ["", "   "] {
            let request = build_launch_request(
                &process_inference(model, None),
                None,
                false,
                None,
                new_session("sess-1"),
                None,
                "t".to_string(),
            );
            assert!(
                request.model.is_none(),
                "an unset model leaves the choice to the harness"
            );
            assert!(request.config.is_none());
        }
    }

    /// The runtime hands the driver bare names and the bridge's own server name. Building a
    /// harness-shaped tool name out of them is the driver's job.
    #[tokio::test]
    async fn launch_request_names_bridge_tools_bare() {
        let inventory = vec![json!({"name": "echo-tool", "parameters": {"type": "object"}})];
        let handle = claude_bridge::bind_bridge(BRIDGE_BIND_ADDR, &inventory)
            .await
            .expect("the bridge binds when tools are declared");
        let request = build_launch_request(
            &process_inference("m", None),
            None,
            false,
            Some(&handle),
            new_session("sess-1"),
            None,
            "t".to_string(),
        );
        let bridge = request.bridge.expect("a bridge was bound");
        assert_eq!(bridge.tool_names, vec!["echo-tool".to_string()]);
        assert_eq!(bridge.server_name, claude_bridge::BRIDGE_SERVER_NAME);
        assert_eq!(bridge.url, handle.url);
        assert_eq!(bridge.bearer_token, handle.token);
    }

    fn policy_allowing(names: &[&str]) -> CapabilityPolicy {
        CapabilityPolicy {
            env_allow: names.iter().map(|n| n.to_string()).collect(),
            ..CapabilityPolicy::default()
        }
    }

    #[test]
    fn harness_env_starts_empty_and_holds_only_declared_names() {
        std::env::set_var("MURMUR_TEST_HARNESS_DECLARED", "yes");
        std::env::set_var("MURMUR_TEST_HARNESS_UNDECLARED", "no");
        let env = build_harness_env(
            &policy_allowing(&["MURMUR_TEST_HARNESS_DECLARED"]),
            &[],
            "/files",
        )
        .unwrap();
        assert_eq!(
            env.get("MURMUR_TEST_HARNESS_DECLARED").map(String::as_str),
            Some("yes")
        );
        assert!(!env.contains_key("MURMUR_TEST_HARNESS_UNDECLARED"));
        assert!(!env.contains_key("PATH"), "nothing is inherited: {env:?}");
    }

    #[test]
    fn harness_env_applies_env_set_last_and_substitutes_the_files_dir() {
        std::env::set_var("MURMUR_TEST_HARNESS_OVERRIDE", "host");
        let env = build_harness_env(
            &policy_allowing(&["MURMUR_TEST_HARNESS_OVERRIDE"]),
            &[
                (
                    "MURMUR_TEST_HARNESS_OVERRIDE".to_string(),
                    "plan".to_string(),
                ),
                ("FILES".to_string(), "{files_dir}/config.json".to_string()),
            ],
            "/tmp/run",
        )
        .unwrap();
        assert_eq!(
            env.get("MURMUR_TEST_HARNESS_OVERRIDE").map(String::as_str),
            Some("plan"),
            "the plan's env-set is applied after the allowlist and wins"
        );
        assert_eq!(
            env.get("FILES").map(String::as_str),
            Some("/tmp/run/config.json")
        );
    }

    /// The credential backstop is the same one every WASM guest gets: declaring a
    /// credential-shaped name delivers nothing, so the harness cannot be handed a host key that
    /// way. A value the driver's own `env-set` composes — the bridge's bearer token among them —
    /// is not a host credential and is delivered as written.
    #[test]
    fn harness_env_never_delivers_a_credential_shaped_host_variable() {
        std::env::set_var("MURMUR_TEST_HARNESS_API_KEY", "leaked");
        let env = build_harness_env(
            &policy_allowing(&["MURMUR_TEST_HARNESS_API_KEY"]),
            &[],
            "/files",
        )
        .unwrap();
        assert!(
            env.is_empty(),
            "a credential-shaped host name reached the harness: {env:?}"
        );
    }

    /// A run is bounded by silence, not by how long it takes: this window is the only limit a
    /// harness can trip, and it is what the runner uses unless a debug build overrides it.
    #[test]
    fn the_only_bound_on_a_run_is_the_inactivity_window() {
        assert_eq!(PROCESS_INACTIVITY_TIMEOUT, Duration::from_secs(600));
        if std::env::var_os("MURMUR_DEBUG_PROCESS_INACTIVITY_MS").is_none() {
            assert_eq!(inactivity_timeout(), PROCESS_INACTIVITY_TIMEOUT);
        }
    }

    #[test]
    fn harness_env_refuses_a_name_that_is_not_a_variable() {
        for bad in ["", "A=B", "A\0B"] {
            let error = build_harness_env(
                &CapabilityPolicy::default(),
                &[(bad.to_string(), "v".to_string())],
                "/files",
            )
            .unwrap_err();
            assert!(error.contains("env-set"), "{bad:?}: {error}");
        }
    }

    #[test]
    fn files_dir_is_substituted_wherever_the_token_appears() {
        assert_eq!(substitute_files_dir("--config", "/d"), "--config");
        assert_eq!(
            substitute_files_dir("{files_dir}/c.json", "/d"),
            "/d/c.json"
        );
        assert_eq!(
            substitute_files_dir("{files_dir}:{files_dir}", "/d"),
            "/d:/d"
        );
    }

    #[test]
    fn driver_file_names_must_be_plain_file_names() {
        assert!(check_driver_file_name("config.json").is_ok());
        assert!(check_driver_file_name(".murmurrc").is_ok());
        for bad in ["", ".", "..", "a/b", "/etc/passwd", "a\0b"] {
            assert!(
                check_driver_file_name(bad).is_err(),
                "{bad:?} names something other than a file in the run's directory"
            );
        }
    }

    #[test]
    fn a_version_is_tested_when_any_token_matches() {
        let tested = vec!["1.0.0".to_string(), "1.1.0".to_string()];
        assert!(version_is_tested("1.0.0", &tested));
        assert!(version_is_tested("some-harness 1.1.0", &tested));
        assert!(
            version_is_tested("v1.0.0", &tested),
            "one leading v is dropped"
        );
        assert!(version_is_tested("some-harness v1.0.0 (build 9)", &tested));
        assert!(!version_is_tested("0.9.0", &tested));
        assert!(!version_is_tested("some-harness 1.0.0-beta", &tested));
        assert!(
            !version_is_tested("vv1.0.0", &tested),
            "only one v is dropped"
        );
        assert!(!version_is_tested("1.0.0", &[]));
    }

    #[test]
    fn the_untested_version_warning_names_what_was_found_and_what_was_tested() {
        let tested = vec!["1.0.0".to_string()];
        let read = untested_version_message(&VersionProbe::Read("h 0.9.0".into()), &tested);
        assert!(read.contains("0.9.0"), "{read}");
        assert!(read.contains("1.0.0"), "{read}");
        let missing = untested_version_message(
            &VersionProbe::Unavailable("it exited non-zero".into()),
            &tested,
        );
        assert!(missing.contains("it exited non-zero"), "{missing}");
        assert!(missing.contains("1.0.0"), "{missing}");
    }

    #[test]
    fn the_files_directory_is_private_and_removed_when_the_run_ends() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let files = FilesDir::create(dir.path()).unwrap();
            let names = write_driver_files(
                files.path(),
                &[crate::process_driver::DriverFile {
                    name: "config.json".to_string(),
                    contents: "{}".to_string(),
                }],
            )
            .unwrap();
            assert_eq!(names, vec!["config.json".to_string()]);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode(files.path()), 0o700);
                assert_eq!(mode(&files.path().join("config.json")), 0o600);
            }
            assert!(files.path().starts_with(dir.path().join(FILES_DIR_NAME)));
            files.path().to_path_buf()
        };
        assert!(!path.exists(), "the run's files directory outlived the run");
    }
}
