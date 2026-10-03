//! Launching a delegated child as an operating-system process of the parent's own runtime.
//!
//! The daemon referees and hands back an approval; this is what turns that approval into a
//! running capsule. The parent's runtime composes a directory for the child beneath its own
//! accessible workdir, builds the child's environment from the child's declaration rather than
//! its own, and starts the `mur` binary as a subprocess. The child's runtime registers with the
//! daemon for itself (see [`crate::registration`]), so the parent never states what the child
//! holds — it only names which artifact is running.
//!
//! Three properties fall out of the child being a process rather than a thread:
//!
//! * A daemon crash takes no child with it. Nothing the child needs to keep running lives in the
//!   daemon's address space.
//! * Each child has its own process environment and working directory, so a native subprocess
//!   started by one child inherits nothing a sibling shares.
//! * A child that declares the `sealed` containment floor enters a mount namespace of its own,
//!   because the containment machinery installs per process and the child *is* a process.
//!
//! **A child joins its parent's formation; it never starts one.** A parent session that belongs to
//! a formation hands its id to every child as [`FORMATION_ID_ENV`], beside the spawner handle and
//! independent of it, so a launch that names no lineage still carries it. A parent in no
//! formation hands none. Inheriting the id is not a grant: the child gets no formation channel, no
//! formation token and no callee, so its door has no membership and refuses every formation token.
//!
//! **The approval travels on the child's standard input.** Not on the argument vector and not in
//! the environment: both are readable from `/proc/<pid>` by any process running as the same user,
//! which is exactly what a sibling capsule's shell tool is.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::delegation::{
    self, CompletionAddress, DelegationOutcome, DelegationStatus, Reporter, Spawner, SpawnerHandle,
    SPAWNER_ENV,
};
use crate::errors::RuntimeError;
use crate::formation::{FormationId, FORMATION_ID_ENV};
use crate::mac_token;
use crate::spawn_credential::SpawnApproval;

/// Environment variable naming the `mur` binary to launch children with.
///
/// Defaults to [`std::env::current_exe`], which is correct in production: the process doing the
/// launching *is* `mur`. A test harness that runs inside its own test binary sets this to the
/// built `mur` instead.
pub const MUR_BINARY_ENV: &str = "MURMUR_MUR_BINARY";

/// How long the parent waits for a child to print its `--json` launch line.
///
/// Generous, because it bounds more than a handshake: a script capsule prints the line when its
/// run has *finished*, and an agent capsule compiles its driver component before it binds a port,
/// which on a cold host is the slowest thing in a launch.
const CHILD_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// Owner-only, so a sibling capsule running as the same user cannot list or read a child's
/// directory even though both sit under one parent.
#[cfg(unix)]
const CHILD_DIR_MODE: u32 = 0o700;

/// How many of the child's last stderr lines are kept to explain a launch that never reported.
///
/// The child's diagnostics are the operator's, so every line is echoed to this process's stderr as
/// it arrives; this bound is only what a *failure* quotes back, so a child that logged for an hour
/// before dying does not turn into an unbounded error string.
const CHILD_STDERR_TAIL_LINES: usize = 20;

/// How long a post-mortem read of the tail waits for the drain to reach the end of the pipe.
///
/// Bounds a thread being scheduled, not the child: by the time anything asks for a dead child's
/// tail the exit has already closed the write end, so the drain is at most one wake-up away.
/// A pipe some surviving grandchild still holds open would never reach EOF at all, and the bound
/// is what stops that inheriting process from wedging the parent's launch or its watcher.
const CHILD_STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the completion watcher asks whether the child is still running.
///
/// It polls rather than blocking on `wait`, because the process handle is shared with
/// [`LaunchedChild::shutdown`] and [`Drop`]: a watcher blocked inside `wait` would hold the lock
/// those need, and a second `wait` on one child is not a thing two owners can both do.
const CHILD_WATCH_INTERVAL: Duration = Duration::from_millis(100);

/// What the parent's runtime needs in order to start one approved child.
///
/// Carries no manifest and no capability declaration: what the child holds is decided by the
/// daemon from the child's own registry manifest, both at the `POST /spawn` that granted `grant`
/// and again at the `POST /register` the child performs for itself.
#[derive(Debug)]
pub struct ChildLaunchRequest {
    /// The parent's own accessible workdir. The child's directory is composed beneath it, which
    /// is what keeps the child inside the single preopen the parent's WASI layer already has.
    pub parent_accessible_workdir: PathBuf,
    pub capsule_name: String,
    pub capsule_version: String,
    /// The approval `POST /spawn` returned, spent by the child's own registration.
    pub grant: SpawnApproval,
    /// The child's `capabilities.env.allow`. Names listed here are copied from the parent's
    /// process environment into the child's; every other host variable the parent holds is
    /// absent from the child, because the child's environment is built from a cleared one.
    pub child_env_allow: Vec<String>,
    /// Base URL of the daemon the child registers with.
    pub roost_url: String,
    /// The session this child belongs to, or `None` for a launch that is not a delegation.
    ///
    /// `Some` is what turns the launch into a delegation: the launcher mints a delegation id and
    /// injects a [`SpawnerHandle`] as [`SPAWNER_ENV`], which is where the child reads the lineage
    /// it records in its own `session_start`. The watcher that reports for a child which could
    /// not report for itself starts only when that spawner also names a
    /// [`CompletionAddress`] — a parent waiting on its own connection wants no completion.
    /// `None` injects nothing and starts nothing.
    pub spawner: Option<Spawner>,
    /// How long the completion watcher observes this child before ending it, or `None` to observe
    /// it for as long as it runs.
    ///
    /// Meaningful only alongside a [`CompletionAddress`], because the watcher that enforces it
    /// starts only for a spawner that names one. On expiry the watcher stops the process and
    /// posts a [`DelegationStatus::Terminated`] outcome naming the bound, which is the only thing
    /// that tells a parent about a child that never ends.
    pub completion_deadline: Option<Duration>,
    /// The parent session's formation, which the child joins: injected as [`FORMATION_ID_ENV`]
    /// when `Some`, whether or not [`Self::spawner`] is. Inherited from the parent and never
    /// minted here; `None` — a parent in no formation — injects nothing.
    pub formation_id: Option<FormationId>,
}

/// How a child's process ended, as the watcher sees it.
enum Ending {
    /// The process exited on its own. `None` when it was reaped by someone who did not keep the
    /// status.
    Exited(Option<ExitStatus>),
    /// The delegation was ended on purpose rather than by the child: by the parent, through
    /// [`LaunchedChild::shutdown`] or `Drop`, or by the completion watcher at its deadline.
    Deliberate,
}

/// The one owner of the child's process handle.
///
/// Shared behind a lock between [`LaunchedChild`] — which kills and reaps — and the completion
/// watcher, which only ever asks whether the process is still there. Single ownership of the
/// `Child` is what keeps `pid()`, `shutdown()` and `Drop` behaving as they did: nothing else
/// takes the handle, and nothing else waits on it.
struct ChildProcess {
    /// `None` once [`LaunchedChild::shutdown`] or `Drop` has reaped the process.
    child: Option<Child>,
    /// Set before the kill by whoever ended the delegation on purpose, so a crash is never read
    /// as a termination. It does not say *who*: the completion watcher sets it at its own
    /// deadline too, and tells the two apart by a flag of its own.
    deliberate: bool,
    /// The exit status, once anyone has observed it.
    status: Option<ExitStatus>,
}

impl ChildProcess {
    /// How the process ended, or `None` while it is still running.
    ///
    /// Reaps a process that exited on its own, which is what makes a later `wait` in `shutdown`
    /// return the cached status rather than block.
    fn poll(&mut self) -> Option<Ending> {
        if self.deliberate {
            return Some(Ending::Deliberate);
        }
        let Some(child) = self.child.as_mut() else {
            // Reaped by neither `shutdown` nor `Drop`, which both set `deliberate` first: this
            // is unreachable, and reporting the exit is the harmless reading of it.
            return Some(Ending::Exited(self.status));
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.status = Some(status);
                Some(Ending::Exited(Some(status)))
            }
            Ok(None) => None,
            // The handle is unusable; treating the child as gone is the only outcome that lets
            // the delegation be reported at all.
            Err(_) => Some(Ending::Exited(self.status)),
        }
    }

    /// Kill and reap. Idempotent.
    fn end(&mut self) -> Result<(), std::io::Error> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // `kill` on an already-exited process is not an error worth surfacing — the wait below is
        // what actually retires the entry in the process table.
        let _ = child.kill();
        let status = child.wait()?;
        self.status = Some(status);
        Ok(())
    }
}

/// A running child capsule and the handles to end it.
pub struct LaunchedChild {
    /// The directory the parent created for this child, and passed as its `--workdir`. The
    /// child's own session artifacts land at `<workdir>/.murmur/<session_id>/`.
    pub workdir: PathBuf,
    /// The session id the child's runtime minted for itself.
    pub session_id: String,
    /// The child's A2A endpoint, `http://host:port`. Empty for a script capsule, which binds no
    /// port and has already finished by the time it reports.
    pub capsule_url: String,
    /// The argument vector the parent built, in order, starting with the binary.
    ///
    /// Exposed so a caller — a test, notably — can assert what was passed without reading
    /// `/proc`. It carries no token: the approval goes in on standard input.
    pub argv: Vec<String>,
    /// The complete environment the parent built for this child, in insertion order. Also carries
    /// no token.
    pub env: Vec<(String, String)>,
    /// The id this launch's completion reports under, `None` when no spawner was supplied.
    ///
    /// Minted here, injected into the child as part of [`SPAWNER_ENV`], and echoed back on the
    /// completion — the one value that joins a delegation to the task it produces at the parent.
    pub delegation_id: Option<String>,
    /// The operator token of a child that declares `network.authentication`, read from
    /// `tokens.operator` on its readiness line. The parent presents it on every call to the
    /// child's door. Never printed by `Debug`.
    pub door_token: Option<crate::door_auth::DoorToken>,
    /// The formation the child reports it joined, read from `formation_id` on its readiness
    /// line. `None` when the line carries no such key, or a value that is not a formation id.
    pub formation_id: Option<FormationId>,
    process: Arc<Mutex<ChildProcess>>,
    /// The child's last [`CHILD_STDERR_TAIL_LINES`] lines, retained so a crash can say why.
    stderr_tail: Arc<StderrTail>,
    /// When the child process was started, for the completion's `duration_ms`.
    started: Instant,
    /// Set by [`LaunchedChild::release`]. The one thing that stops [`Drop`] signalling the child:
    /// a released process is no longer this handle's to end.
    released: bool,
}

impl std::fmt::Debug for LaunchedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchedChild")
            .field("workdir", &self.workdir)
            .field("session_id", &self.session_id)
            .field("capsule_url", &self.capsule_url)
            .field("delegation_id", &self.delegation_id)
            .field("formation_id", &self.formation_id)
            .field(
                "door_token",
                &self.door_token.as_ref().map(|_| "<redacted>"),
            )
            .field("pid", &self.pid())
            .finish()
    }
}

impl LaunchedChild {
    /// The child's operating-system process id. `0` once it has been reaped.
    pub fn pid(&self) -> u32 {
        lock(&self.process)
            .child
            .as_ref()
            .map(Child::id)
            .unwrap_or(0)
    }

    /// The child's last [`CHILD_STDERR_TAIL_LINES`] lines, oldest first.
    ///
    /// The same lines a crash completion quotes back, exposed so a caller can read what the child
    /// said without scraping the stderr they were already echoed to.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.stderr_tail.lines()
    }

    /// Terminate the child and reap it. Idempotent: a second call, or a call after `Drop` has
    /// already run, does nothing.
    ///
    /// Marks the ending as deliberate, so the watcher records the delegation as `terminated` and
    /// posts nothing: the only party that would be told is the party that did it.
    pub fn shutdown(&mut self) -> Result<(), RuntimeError> {
        let mut process = lock(&self.process);
        process.deliberate = true;
        process.end().map_err(|error| {
            RuntimeError::Runtime(format!("failed to reap child capsule: {error}"))
        })
    }

    /// Stop owning the child's lifetime without signalling it, and let this handle go.
    ///
    /// The one escape from [`Drop`]'s kill. After this the process keeps running and **only the
    /// completion watcher started by [`launch_child_capsule`] will ever observe or reap it** — a
    /// child released from a launch that named no [`CompletionAddress`] is a process nothing will
    /// ever wait on.
    ///
    /// This is how a delegation outlives the call that made it: `delegate-task` returns as soon as
    /// the child is running and holding its task, and what the child eventually did arrives at the
    /// parent as a completion the watcher either forwards or writes itself.
    pub fn release(mut self) {
        self.released = true;
    }
}

impl Drop for LaunchedChild {
    /// Terminates and reaps, so a parent that returns early — including by panicking — leaves no
    /// orphaned capsule process behind holding a port and a directory. Deliberate on the same
    /// terms as [`LaunchedChild::shutdown`].
    ///
    /// A released child is left alone: its lifetime stopped being this handle's the moment
    /// [`LaunchedChild::release`] was called.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut process = lock(&self.process);
        process.deliberate = true;
        let _ = process.end();
    }
}

/// `workdir` named from `root`, falling back to the absolute path when it sits outside it.
///
/// One rule, used by the delegating plane wherever it names a child's directory to the parent's
/// agent or to the parent's trace.
pub(crate) fn workdir_relative_to(workdir: &Path, root: &Path) -> String {
    workdir
        .strip_prefix(root)
        .unwrap_or(workdir)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A mutex this crate holds only for the length of one field access, so a poisoned lock is
/// recovered from rather than turned into a panic in a `Drop`.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where a child of this parent goes: `<parent accessible workdir>/.murmur/children/<name>-<16 hex>`.
///
/// The 16 hex characters are fresh per call, so delegating the same capsule name and version twice
/// yields two directories rather than one shared one. Beneath `.murmur/`, alongside the parent's
/// own session directories, so a child's tree is inside the parent's single preopen and is pruned
/// with it.
pub fn child_workdir_for(parent_accessible_workdir: &Path, capsule_name: &str) -> PathBuf {
    let suffix = mac_token::random_hex(8).unwrap_or_else(|_| {
        // The OS CSPRNG is not expected to fail; a nanosecond clock reading still distinguishes
        // two delegations rather than collapsing them onto one directory.
        format!(
            "{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0)
        )
    });
    parent_accessible_workdir
        .join(".murmur")
        .join("children")
        .join(format!("{capsule_name}-{suffix}"))
}

/// Create the child's directory, start `mur` on it, and wait for the child to report itself.
pub fn launch_child_capsule(request: ChildLaunchRequest) -> Result<LaunchedChild, RuntimeError> {
    let workdir = child_workdir_for(&request.parent_accessible_workdir, &request.capsule_name);
    create_child_dir(&workdir)?;

    let binary = mur_binary()?;
    let argv: Vec<String> = vec![
        binary.display().to_string(),
        "run".to_string(),
        "--capsule".to_string(),
        request.capsule_name.clone(),
        "--capsule-version".to_string(),
        request.capsule_version.clone(),
        "--workdir".to_string(),
        workdir.display().to_string(),
        "--json".to_string(),
        "--no-env-file".to_string(),
        "--spawn-grant-stdin".to_string(),
    ];
    // Minted here, once per launch: a caller holding one `Spawner` that delegates twice gets two
    // delegation ids, and neither child can report under the other's.
    let handle = request
        .spawner
        .as_ref()
        .map(|spawner| SpawnerHandle::for_delegation(spawner, delegation::new_delegation_id()));
    let env = child_environment(&request, handle.as_ref());
    let started = Instant::now();

    // The grant's one and only appearance outside the parent's memory: one line on a pipe that is
    // closed immediately afterwards. Failing to write it is fatal — the child would otherwise sit
    // waiting on a line that will never come.
    let StartedProcess {
        child,
        stderr_tail,
        stdin_written: write_result,
    } = start_process(&ProcessLaunch {
        binary: &binary,
        args: &argv[1..],
        cwd: Some(&workdir),
        env: ProcessEnv::Cleared(&env),
        stdin_line: Some(request.grant.expose()),
        stderr_prefix: None,
        inherit_output: false,
        own_process_group: false,
        inherit_fd: None,
        #[cfg(unix)]
        lifeline: None,
    })
    .map_err(|error| {
        RuntimeError::Runtime(format!(
            "failed to start the child capsule process '{}': {error}",
            binary.display()
        ))
    })?;
    let write_result =
        write_result.map_err(|error| format!("failed to hand the child its launch grant: {error}"));
    let mut launched = LaunchedChild {
        workdir,
        session_id: String::new(),
        capsule_url: String::new(),
        argv,
        env,
        delegation_id: handle.as_ref().map(|handle| handle.delegation_id.clone()),
        door_token: None,
        formation_id: None,
        process: Arc::new(Mutex::new(ChildProcess {
            child: Some(child),
            deliberate: false,
            status: None,
        })),
        stderr_tail: Arc::clone(&stderr_tail),
        started,
        released: false,
    };
    if let Err(reason) = write_result {
        return Err(RuntimeError::Runtime(reason));
    }

    let line = first_json_line(&mut launched, &stderr_tail)?;
    let report: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
        RuntimeError::Runtime(format!(
            "the child capsule's first --json line did not parse: {error}"
        ))
    })?;
    let session_id = report
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            RuntimeError::Runtime(
                "the child capsule's --json line carried no session_id".to_string(),
            )
        })?;
    let url = report
        .get("url")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    launched.session_id = session_id.to_string();
    launched.door_token = readiness_door_token(&report);
    launched.formation_id = report
        .get("formation_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|id| FormationId::parse(id).ok());
    // A script capsule binds no port and reports an empty url; promoting that to `http://` would
    // manufacture an address nothing answers on.
    launched.capsule_url = if url.is_empty() {
        String::new()
    } else {
        format!("http://{url}")
    };

    // Started only for a launch somebody wants told about. A lineage-only handle names no
    // address, so a child whose parent waits on the connection it already holds is launched with
    // no watcher behind it and posts nothing anywhere.
    if let Some(handle) = handle {
        if let Some(address) = handle.report_to.clone() {
            watch_for_completion(
                &launched,
                handle,
                address,
                &request.capsule_name,
                &request.capsule_version,
                request.completion_deadline,
            );
        }
    }
    Ok(launched)
}

/// The operator token a child's `--json` readiness line carries under `tokens.operator`, present
/// only when the child declares `network.authentication`.
pub(crate) fn readiness_door_token(
    report: &serde_json::Value,
) -> Option<crate::door_auth::DoorToken> {
    report
        .pointer("/tokens/operator")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(|token| crate::door_auth::DoorToken::new(token.to_string()))
}

/// Report for a child that ended without reporting for itself, and stop one that never ends.
///
/// Polls the shared process handle rather than blocking on `wait`, so it never contends with
/// [`LaunchedChild::shutdown`] or `Drop` for ownership of the `Child`. For a released child this
/// thread is the process's only remaining observer, and the only thing that will ever reap it.
///
/// `deadline` bounds how long the child is watched, from the instant this watcher starts — which
/// is the instant the child reported itself ready, not the instant its process was spawned.
/// Getting to that point is bounded separately by [`CHILD_READY_TIMEOUT`], and folding a cold
/// host's staging time into the delegation deadline would end a child for being slow to start
/// rather than slow to work. On expiry the watcher stops the child itself and posts a
/// [`DelegationStatus::Terminated`] outcome naming the bound — the deadline is the watcher's own,
/// held in a local flag rather than read back off [`ChildProcess::deliberate`], so an ending the
/// *parent* chose stays distinguishable from one this thread imposed. `None` watches for as long
/// as the child runs.
///
/// What it does when the process ends on its own is decided by what the child left behind:
///
/// * A completion the child already delivered is left alone — exactly one completion per
///   delegation reaches the parent.
/// * A completion the child recorded but could not deliver is retried once, and the file is
///   rewritten with the result. Nothing is retried after that.
/// * No completion at all means the child died without a word: the watcher builds one with
///   `status: crashed`, carrying the exit status and the child's bounded stderr tail, and posts
///   it in the child's place.
/// * An ending the parent chose is recorded as `terminated` and posted to nobody, because the
///   only party that would be told is the party that did it.
fn watch_for_completion(
    launched: &LaunchedChild,
    handle: SpawnerHandle,
    address: CompletionAddress,
    capsule_name: &str,
    capsule_version: &str,
    deadline: Option<Duration>,
) {
    let process = Arc::clone(&launched.process);
    let stderr_tail = Arc::clone(&launched.stderr_tail);
    let workdir = launched.workdir.clone();
    let session_id = launched.session_id.clone();
    let capsule_name = capsule_name.to_string();
    let capsule_version = capsule_version.to_string();
    let started = launched.started;

    let watching_from = Instant::now();
    std::thread::spawn(move || {
        let expires_at = deadline.map(|bound| watching_from + bound);
        // Set here and read nowhere else: `ChildProcess::deliberate` is about to be set too, by
        // this thread, so it can no longer say who chose the ending.
        let mut expired = false;
        let ending = loop {
            if expires_at.is_some_and(|at| Instant::now() >= at) {
                let mut process = lock(&process);
                process.deliberate = true;
                let _ = process.end();
                expired = true;
                break Ending::Deliberate;
            }
            if let Some(ending) = lock(&process).poll() {
                break ending;
            }
            std::thread::sleep(CHILD_WATCH_INTERVAL);
        };
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

        if let Some(recorded) = delegation::read_completion(&workdir) {
            if recorded.delivered {
                return;
            }
            let retried = delegation::report_completion(&handle, &address, recorded, &workdir);
            if !retried.delivered {
                // One retry, then the file and the line above it are the record.
                crate::runtime_err!(
                    "[capsule-runtime] delegation {}: the child's completion is undelivered after one retry",
                    handle.delegation_id
                );
            }
            return;
        }

        let outcome = |status: DelegationStatus, detail: String| DelegationOutcome {
            delegation_id: handle.delegation_id.clone(),
            capsule_name: capsule_name.clone(),
            capsule_version: capsule_version.clone(),
            session_id: session_id.clone(),
            status,
            // The launcher reports for a child that recorded nothing, so it makes no claim about
            // a result file it never saw named.
            result_path: None,
            workdir: workdir.display().to_string(),
            duration_ms,
            detail: Some(detail),
            reported_by: Reporter::Launcher,
            delivered: false,
            delivery_error: None,
        };

        match ending {
            // The deadline is the parent's runtime acting on the parent's behalf while the parent
            // is elsewhere, so unlike an ending the parent chose by hand this one is posted: the
            // delegating agent asked for an outcome and this is the outcome.
            Ending::Deliberate if expired => {
                let bound = deadline.map(|bound| bound.as_secs()).unwrap_or_default();
                delegation::report_completion(
                    &handle,
                    &address,
                    outcome(
                        DelegationStatus::Terminated,
                        format!(
                            "the capsule was still running after {bound}s and was ended at the \
                             delegation deadline"
                        ),
                    ),
                    &workdir,
                );
            }
            Ending::Deliberate => {
                delegation::record_terminated(
                    &workdir,
                    outcome(
                        DelegationStatus::Terminated,
                        "the parent ended this delegation".to_string(),
                    ),
                );
            }
            Ending::Exited(status) => {
                let status_text = match status {
                    Some(status) => status.to_string(),
                    None => "unknown exit status".to_string(),
                };
                let tail = stderr_tail.lines_at_end().join("\n");
                let detail = if tail.is_empty() {
                    format!(
                        "the child process ended without recording a completion ({status_text})"
                    )
                } else {
                    format!(
                        "the child process ended without recording a completion ({status_text}); \
                         its last stderr lines were:\n{tail}"
                    )
                };
                delegation::report_completion(
                    &handle,
                    &address,
                    outcome(DelegationStatus::Crashed, detail),
                    &workdir,
                );
            }
        }
    });
}

/// The child's complete environment, built from a cleared one.
///
/// `capabilities.env.allow` is the child's own, not the parent's: a sibling's declaration reaches
/// nothing here, and a variable the parent holds but the child did not declare is simply absent.
/// The runtime-owned names — `PATH`, `HOME`, `MURMUR_ROOST_URL`, [`SPAWNER_ENV`] on a delegated
/// launch and [`FORMATION_ID_ENV`] for a parent in a formation, in that order — are applied last,
/// so a child cannot displace the daemon URL it is required to register with, the handle it
/// reports its outcome to, or the formation it joins, by allowlisting the name.
/// A formation member's names ([`crate::formation::is_member_grant_env`]) are never handed on at
/// all: inheriting a formation is not a grant, so a member's child holds no channel, no token, no
/// callee and no lifeline (its parent session contains it), and a child that allowlists any of the
/// names receives nothing.
pub(crate) fn child_environment(
    request: &ChildLaunchRequest,
    handle: Option<&SpawnerHandle>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for key in &request.child_env_allow {
        if let Ok(value) = std::env::var(key) {
            env.push((key.clone(), value));
        }
    }
    env.retain(|(key, _)| {
        !matches!(key.as_str(), "PATH" | "HOME" | "MURMUR_ROOST_URL")
            && key != SPAWNER_ENV
            && key != FORMATION_ID_ENV
            && !crate::formation::is_member_grant_env(key)
    });

    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_string(), path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("HOME".to_string(), home));
    }
    env.push(("MURMUR_ROOST_URL".to_string(), request.roost_url.clone()));
    // Last, with the other runtime-owned names, and only for a launch that has a spawner: a child
    // nobody wants told holds no handle at all, whatever this process's own environment carries
    // and whatever the child declared.
    if let Some(handle) = handle {
        env.push((SPAWNER_ENV.to_string(), handle.to_env_value()));
    }
    // A parent in no formation hands none on, whatever this process's own environment carries.
    if let Some(formation_id) = &request.formation_id {
        let (name, value) = formation_id.env_pair();
        env.push((name.to_string(), value));
    }
    env
}

/// The binary a child is started from: [`MUR_BINARY_ENV`] when set, else this process's own image.
pub(crate) fn mur_binary() -> Result<PathBuf, RuntimeError> {
    if let Some(path) = std::env::var_os(MUR_BINARY_ENV) {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    std::env::current_exe().map_err(|error| {
        RuntimeError::Runtime(format!(
            "cannot locate the mur binary to launch a child capsule with: {error}"
        ))
    })
}

fn create_child_dir(workdir: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(workdir).map_err(|source| RuntimeError::CreateWorkdir {
        path: workdir.display().to_string(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(workdir, std::fs::Permissions::from_mode(CHILD_DIR_MODE))
            .map_err(|source| RuntimeError::CreateWorkdir {
                path: workdir.display().to_string(),
                source,
            })?;
    }
    Ok(())
}

/// The child's stderr as the parent keeps it: every line echoed to this process's stderr as it
/// arrives, with the last [`CHILD_STDERR_TAIL_LINES`] retained so a child that dies before
/// reporting can say why.
pub(crate) struct StderrTail {
    state: Mutex<StderrState>,
    /// Notified once [`StderrState::at_eof`] is set.
    drained: Condvar,
}

#[derive(Default)]
struct StderrState {
    lines: Vec<String>,
    /// Whether the pipe has been read to its end, which the child's exit is what causes.
    at_eof: bool,
}

impl StderrTail {
    /// What the child has said so far, without waiting for anything. For a child that is still
    /// running, which is the only state in which "so far" is the question being asked.
    pub(crate) fn lines(&self) -> Vec<String> {
        lock(&self.state).lines.clone()
    }

    /// What a child that has ended left behind, read only once the pipe is at its end.
    ///
    /// The exit that closes the child's stdout closes its stderr in the same breath, so the thread
    /// that notices the ending and the thread still draining stderr are racing, and the tail is
    /// empty for as long as the drain has not caught up. Reading it unsynchronised turns "refused
    /// because the declared floor is unmeetable here" into a refusal that names no reason —
    /// the one case the tail is kept for. Bounded by [`CHILD_STDERR_DRAIN_TIMEOUT`].
    pub(crate) fn lines_at_end(&self) -> Vec<String> {
        let (state, _) = self
            .drained
            .wait_timeout_while(lock(&self.state), CHILD_STDERR_DRAIN_TIMEOUT, |state| {
                !state.at_eof
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.lines.clone()
    }

    /// Mark the pipe finished and wake every post-mortem reader waiting on it.
    fn finish(&self) {
        lock(&self.state).at_eof = true;
        self.drained.notify_all();
    }
}

/// Start draining the child's stderr into a [`StderrTail`].
///
/// The child's diagnostics belong to the operator running the parent, so nothing is swallowed —
/// this only remembers, in addition to printing. `prefix`, when given, is written in front of
/// every echoed line and kept off the remembered ones.
fn drain_stderr(child: &mut Child, prefix: Option<&str>) -> Arc<StderrTail> {
    let tail = Arc::new(StderrTail {
        state: Mutex::new(StderrState::default()),
        drained: Condvar::new(),
    });
    let Some(stderr) = child.stderr.take() else {
        // No pipe is an end already reached: a reader that waited here would wait out the whole
        // bound for lines that can never arrive.
        tail.finish();
        return tail;
    };
    let collector = Arc::clone(&tail);
    let prefix = prefix.unwrap_or_default().to_string();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            crate::runtime_err!("{prefix}{line}");
            let mut state = lock(&collector.state);
            if state.lines.len() == CHILD_STDERR_TAIL_LINES {
                state.lines.remove(0);
            }
            state.lines.push(line);
        }
        collector.finish();
    });
    tail
}

/// Read the child's first standard-output line, then keep draining the pipe.
///
/// The drain matters: a child whose stdout pipe fills blocks forever, and a capsule that logs
/// after its launch line would otherwise deadlock against a parent that had stopped reading.
fn first_json_line(
    launched: &mut LaunchedChild,
    stderr_tail: &StderrTail,
) -> Result<String, RuntimeError> {
    let stdout = lock(&launched.process)
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| {
            RuntimeError::Runtime("the child process exposed no standard output".to_string())
        })?;

    let rx = relay_readiness_line(stdout, AfterReadiness::Stdout);

    match rx.recv_timeout(CHILD_READY_TIMEOUT) {
        Ok(Some(line)) => Ok(line),
        Ok(None) => {
            let status = lock(&launched.process)
                .child
                .as_mut()
                .and_then(|child| child.wait().ok())
                .map(|status| status.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            // The child's own refusal — an unmeetable containment floor, a registration the
            // daemon declined — is written to its stderr and is the only thing that explains this
            // failure, so it is quoted rather than replaced by a generic "did not start".
            let reason = stderr_tail.lines_at_end().join("\n");
            Err(RuntimeError::Runtime(format!(
                "the child capsule '{}' exited without reporting a launch line ({status}): {reason}",
                launched.workdir.display()
            )))
        }
        Err(_) => Err(RuntimeError::Runtime(format!(
            "the child capsule '{}' did not report a launch line within {}s",
            launched.workdir.display(),
            CHILD_READY_TIMEOUT.as_secs()
        ))),
    }
}

/// How a launched `mur run` process's environment is built.
pub(crate) enum ProcessEnv<'a> {
    /// Cleared, then exactly these pairs: a delegated child, which holds nothing of the parent's
    /// that it did not declare.
    Cleared(&'a [(String, String)]),
    /// This process's own, with `set` applied over it and every name in `remove` taken out: a
    /// formation member, which the operator launched and which reads the operator's environment.
    Inherited {
        set: &'a [(String, String)],
        remove: &'a [&'a str],
    },
}

/// Where a launched process's standard output goes once its readiness line has been read.
pub(crate) enum AfterReadiness {
    /// To this process's standard output, as written.
    Stdout,
    /// To this process's standard error, one line at a time behind this prefix.
    PrefixedStderr(String),
}

/// One `mur run` subprocess to start.
pub(crate) struct ProcessLaunch<'a> {
    pub(crate) binary: &'a Path,
    /// The argument vector after the binary.
    pub(crate) args: &'a [String],
    /// The working directory, or `None` for this process's own.
    pub(crate) cwd: Option<&'a Path>,
    pub(crate) env: ProcessEnv<'a>,
    /// One line written to the process's standard input, which is then closed; `None` gives it a
    /// null standard input.
    pub(crate) stdin_line: Option<&'a str>,
    /// Written in front of every stderr line echoed to this process's stderr.
    pub(crate) stderr_prefix: Option<&'a str>,
    /// Hand the process this process's own standard output and error rather than pipes. Its
    /// stdout is then not read here, and its stderr tail stays empty.
    pub(crate) inherit_output: bool,
    /// Start the process as the leader of a process group of its own, so a signal meant for this
    /// process's group — a terminal's `^C` — does not reach it, and its whole tree can be
    /// signalled at once.
    pub(crate) own_process_group: bool,
    /// One descriptor of this process's to hand the process at the same number: a formation
    /// member's channel. It is close-on-exec here, so no other process this one starts inherits
    /// it, and the flag is cleared in the started process alone, just before it execs.
    pub(crate) inherit_fd: Option<i32>,
    /// A formation member's lifeline, whose read end the process is handed. Every lifeline end is
    /// created close-on-exec, and this is the one the member's spawn clears it on.
    #[cfg(unix)]
    pub(crate) lifeline: Option<&'a crate::lifeline::MemberLifeline>,
}

/// A process [`start_process`] started, with its stderr already being drained.
pub(crate) struct StartedProcess {
    /// Its standard output, unless inherited, is still piped and unread.
    pub(crate) child: Child,
    pub(crate) stderr_tail: Arc<StderrTail>,
    /// Whether [`ProcessLaunch::stdin_line`] was written: `Ok` when there was none to write.
    pub(crate) stdin_written: Result<(), String>,
}

/// Start one `mur run` subprocess: spawn it, write its standard-input line, and start draining its
/// stderr. The one spawn site for a delegated child and a formation member alike.
pub(crate) fn start_process(launch: &ProcessLaunch<'_>) -> std::io::Result<StartedProcess> {
    let mut command = Command::new(launch.binary);
    command
        .args(launch.args)
        .stdin(if launch.stdin_line.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(if launch.inherit_output {
            Stdio::inherit()
        } else {
            Stdio::piped()
        })
        .stderr(if launch.inherit_output {
            Stdio::inherit()
        } else {
            Stdio::piped()
        });
    if let Some(cwd) = launch.cwd {
        command.current_dir(cwd);
    }
    match launch.env {
        ProcessEnv::Cleared(pairs) => {
            command.env_clear();
            for (key, value) in pairs {
                command.env(key, value);
            }
        }
        ProcessEnv::Inherited { set, remove } => {
            for name in remove {
                command.env_remove(name);
            }
            for (key, value) in set {
                command.env(key, value);
            }
        }
    }
    #[cfg(unix)]
    if launch.own_process_group {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(unix)]
    if let Some(fd) = launch.inherit_fd {
        keep_across_exec(&mut command, fd);
    }
    // No `apply_fd_hygiene` here: it fails the spawn outright on a kernel without
    // `CLOSE_RANGE_CLOEXEC`, and a lifeline needs none, since both ends are created close-on-exec.
    #[cfg(unix)]
    if let Some(lifeline) = launch.lifeline {
        lifeline.hand_to(&mut command);
    }

    let mut child = command.spawn()?;
    let stdin_written = match launch.stdin_line {
        None => Ok(()),
        Some(line) => child
            .stdin
            .take()
            .ok_or_else(|| "the process exposed no standard input".to_string())
            .and_then(|mut stdin| writeln!(stdin, "{line}").map_err(|error| error.to_string())),
    };
    let stderr_tail = drain_stderr(&mut child, launch.stderr_prefix);
    Ok(StartedProcess {
        child,
        stderr_tail,
        stdin_written,
    })
}

/// Clear `FD_CLOEXEC` on `fd` in the started process alone, after everything else that runs before
/// its `exec`, so that process inherits `fd` and no other does.
#[cfg(unix)]
#[allow(unsafe_code)]
fn keep_across_exec(command: &mut Command, fd: i32) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the forked child between `fork` and `exec`, where only
    // async-signal-safe calls are allowed; it makes two `fcntl` calls on an integer descriptor,
    // which allocate nothing and take no lock.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Read a launched process's first standard-output line on a thread of its own, and hand it back
/// on the returned channel: `Some(line)` without its line ending, or `None` when the stream ended
/// first.
///
/// The thread then keeps draining the pipe to `after` for as long as the process lives. The drain
/// matters: a process whose stdout pipe fills blocks forever, and a capsule that logs after its
/// launch line would otherwise deadlock against a reader that had stopped reading. This thread
/// outlives the readiness line by the whole life of the process, so it is the one relay most
/// likely to meet a reader that has already gone: it relays each line and keeps draining either
/// way.
pub(crate) fn relay_readiness_line(
    stdout: std::process::ChildStdout,
    after: AfterReadiness,
) -> mpsc::Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let first = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end().to_string()),
        };
        let _ = tx.send(first);
        let mut rest = String::new();
        while let Ok(read) = reader.read_line(&mut rest) {
            if read == 0 {
                break;
            }
            match &after {
                AfterReadiness::Stdout => {
                    crate::diagnostic::raw_to_stdout(&rest);
                }
                AfterReadiness::PrefixedStderr(prefix) => {
                    crate::runtime_err!("{prefix}{}", rest.trim_end_matches(['\r', '\n']));
                }
            }
            rest.clear();
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_directory_is_unique_per_delegation() {
        let parent = PathBuf::from("/tmp/parent");
        let first = child_workdir_for(&parent, "worker");
        let second = child_workdir_for(&parent, "worker");

        assert_ne!(first, second);
        assert_eq!(
            first.parent(),
            Some(parent.join(".murmur").join("children").as_path())
        );
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("worker-"));
        // `<name>-<16 hex>`
        assert_eq!(
            first.file_name().unwrap().to_string_lossy().len(),
            "worker-".len() + 16
        );
    }

    fn request(env_allow: &[&str], spawner: Option<Spawner>) -> ChildLaunchRequest {
        ChildLaunchRequest {
            parent_accessible_workdir: PathBuf::from("/tmp/parent"),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            grant: SpawnApproval::new("msa1.token".to_string()),
            child_env_allow: env_allow.iter().map(|name| name.to_string()).collect(),
            roost_url: "http://127.0.0.1:7700".to_string(),
            spawner,
            completion_deadline: None,
            formation_id: None,
        }
    }

    /// A handle naming some *other* delegation, for the two cases that prove a child cannot be
    /// handed this process's own.
    ///
    /// Readable rather than nonsense, because `SPAWNER_ENV` is process-wide and every other test
    /// in this binary shares it: an unreadable value here refuses an unrelated `stage_session`
    /// running beside it.
    fn decoy() -> String {
        SpawnerHandle {
            session_id: "ses_decoy".to_string(),
            context_id: "ctx_decoy".to_string(),
            delegation_id: "dlg_decoy".to_string(),
            report_to: None,
        }
        .to_env_value()
    }

    fn spawner() -> Spawner {
        Spawner {
            session_id: "ses_parent".to_string(),
            context_id: "ctx_parent".to_string(),
            report_to: Some(CompletionAddress {
                url: "http://127.0.0.1:7000".to_string(),
                trust: crate::origin::TrustClass::Trusted,
            }),
        }
    }

    #[test]
    fn a_childs_environment_is_built_from_its_own_declaration() {
        std::env::set_var("MURMUR_CHILD_LAUNCH_TEST_A", "a");
        std::env::set_var("MURMUR_CHILD_LAUNCH_TEST_B", "b");

        let env = child_environment(&request(&["MURMUR_CHILD_LAUNCH_TEST_A"], None), None);
        let names: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();

        assert!(names.contains(&"MURMUR_CHILD_LAUNCH_TEST_A"));
        assert!(!names.contains(&"MURMUR_CHILD_LAUNCH_TEST_B"));
        assert!(names.contains(&"MURMUR_ROOST_URL"));
        assert!(!env.iter().any(|(_, value)| value.contains("msa1.token")));
    }

    #[test]
    fn a_runtime_owned_name_cannot_be_displaced_by_a_declaration() {
        std::env::set_var("MURMUR_ROOST_URL", "http://127.0.0.1:1");

        let env = child_environment(&request(&["MURMUR_ROOST_URL"], None), None);
        let urls: Vec<&String> = env
            .iter()
            .filter(|(key, _)| key == "MURMUR_ROOST_URL")
            .map(|(_, value)| value)
            .collect();

        assert_eq!(urls, vec!["http://127.0.0.1:7700"]);
    }

    /// The injected handle is the one the parent composed, whatever this process's own
    /// environment holds and whatever the child declared.
    #[test]
    fn the_spawner_handle_is_injected_last_and_cannot_be_displaced() {
        std::env::set_var(SPAWNER_ENV, decoy());
        let handle = SpawnerHandle::for_delegation(&spawner(), "dlg_0001".to_string());

        let env = child_environment(&request(&[SPAWNER_ENV], Some(spawner())), Some(&handle));
        let injected: Vec<&String> = env
            .iter()
            .filter(|(key, _)| key == SPAWNER_ENV)
            .map(|(_, value)| value)
            .collect();

        assert_eq!(injected.len(), 1, "{env:?}");
        assert_eq!(
            SpawnerHandle::parse(injected[0]).expect("the injected value is a handle"),
            handle
        );
        assert_eq!(
            env.last().map(|(key, _)| key.as_str()),
            Some(SPAWNER_ENV),
            "the handle is applied in the runtime-owned tail: {env:?}"
        );
        std::env::remove_var(SPAWNER_ENV);
    }

    /// A launch with no spawner injects nothing, even from a decoy the launching process holds.
    #[test]
    fn a_launch_without_a_spawner_injects_no_handle() {
        std::env::set_var(SPAWNER_ENV, decoy());

        let env = child_environment(&request(&[SPAWNER_ENV], None), None);

        assert!(!env.iter().any(|(key, _)| key == SPAWNER_ENV), "{env:?}");
        std::env::remove_var(SPAWNER_ENV);
    }

    /// A child's formation is the one the parent's request names, applied after the spawner
    /// handle, whatever this process's own environment holds and whatever the child declared.
    #[test]
    fn the_formation_id_is_injected_last_and_cannot_be_displaced() {
        let _guard = crate::formation::FORMATION_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let decoy = FormationId::mint();
        std::env::set_var(FORMATION_ID_ENV, decoy.as_str());
        let injected = |env: &[(String, String)]| -> Vec<String> {
            env.iter()
                .filter(|(key, _)| key == FORMATION_ID_ENV)
                .map(|(_, value)| value.clone())
                .collect()
        };

        let outside = child_environment(&request(&[FORMATION_ID_ENV], None), None);
        assert!(injected(&outside).is_empty(), "{outside:?}");

        let other = FormationId::mint();
        let handle = SpawnerHandle::for_delegation(&spawner(), "dlg_0001".to_string());
        for (spawner, handle) in [(Some(spawner()), Some(&handle)), (None, None)] {
            let mut member = request(&[FORMATION_ID_ENV], spawner);
            member.formation_id = Some(other.clone());
            let env = child_environment(&member, handle);
            assert_eq!(injected(&env), vec![other.as_str().to_string()], "{env:?}");
            assert_eq!(
                env.last(),
                Some(&(FORMATION_ID_ENV.to_string(), other.as_str().to_string())),
                "the formation id is the last runtime-owned name: {env:?}"
            );
        }
        std::env::remove_var(FORMATION_ID_ENV);
    }

    /// A child's exit closes its stdout and its stderr together, so the reader that reports the
    /// ending can reach the tail before the drain has appended to it. The post-mortem read is the
    /// one that must not: an empty tail is a refusal that names no reason.
    #[test]
    fn a_tail_read_after_the_end_waits_for_the_drain_to_catch_up() {
        let tail = Arc::new(StderrTail {
            state: Mutex::new(StderrState::default()),
            drained: Condvar::new(),
        });

        let writer = Arc::clone(&tail);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            lock(&writer.state)
                .lines
                .push("error[E-CAP-003]: not achievable on this host".to_string());
            writer.finish();
        });

        assert!(tail.lines().is_empty());
        assert_eq!(
            tail.lines_at_end(),
            vec!["error[E-CAP-003]: not achievable on this host".to_string()]
        );
    }

    /// A child that exposed no stderr pipe has nothing to wait for, so the bound is never spent on
    /// lines that cannot arrive.
    #[test]
    fn a_tail_with_no_pipe_behind_it_is_at_its_end_already() {
        let tail = StderrTail {
            state: Mutex::new(StderrState::default()),
            drained: Condvar::new(),
        };
        tail.finish();

        let started = Instant::now();
        assert!(tail.lines_at_end().is_empty());
        assert!(
            started.elapsed() < CHILD_STDERR_DRAIN_TIMEOUT,
            "{started:?}"
        );
    }

    #[test]
    fn child_launch_reads_the_operator_token_from_the_readiness_line() {
        let report = serde_json::json!({
            "url": "localhost:1",
            "session_id": "ses_x",
            "tokens": {"operator": "mdt1.a.b", "watcher": "mdt1.c.d"},
        });
        assert_eq!(
            readiness_door_token(&report).map(|token| token.expose().to_string()),
            Some("mdt1.a.b".to_string())
        );
        let public = serde_json::json!({"url": "localhost:1", "session_id": "ses_x"});
        assert_eq!(readiness_door_token(&public), None);
    }

    #[test]
    fn child_launch_debug_prints_no_door_token() {
        let child = LaunchedChild {
            workdir: PathBuf::from("/tmp/child"),
            session_id: "ses_child".to_string(),
            capsule_url: "http://localhost:1".to_string(),
            argv: Vec::new(),
            env: Vec::new(),
            delegation_id: None,
            formation_id: None,
            door_token: Some(crate::door_auth::DoorToken::new(
                "mdt1.secretpayload.secretmac".to_string(),
            )),
            process: Arc::new(Mutex::new(ChildProcess {
                child: None,
                deliberate: false,
                status: None,
            })),
            stderr_tail: Arc::new(StderrTail {
                state: Mutex::new(StderrState::default()),
                drained: Condvar::new(),
            }),
            started: Instant::now(),
            released: true,
        };
        let debug = format!("{child:?}");
        assert!(!debug.contains("secretpayload"), "{debug}");
        assert!(debug.contains("door_token"), "{debug}");
    }
}
