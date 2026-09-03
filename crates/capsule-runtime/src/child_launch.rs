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
//! **The approval travels on the child's standard input.** Not on the argument vector and not in
//! the environment: both are readable from `/proc/<pid>` by any process running as the same user,
//! which is exactly what a sibling capsule's shell tool is.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::delegation::{
    self, CompletionAddress, DelegationOutcome, DelegationStatus, Reporter, Spawner, SpawnerHandle,
    SPAWNER_ENV,
};
use crate::detached::{DelegationLateReport, DetachedReport};
use crate::errors::RuntimeError;
use crate::mac_token;
use crate::origin::TaskProvenance;
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
}

/// How a child's process ended, as the watcher sees it.
enum Ending {
    /// The process exited on its own. `None` when it was reaped by someone who did not keep the
    /// status.
    Exited(Option<ExitStatus>),
    /// The parent ended the delegation itself, through [`LaunchedChild::shutdown`] or `Drop`.
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
    /// Set before the kill by whoever ended the delegation on purpose, so the watcher can tell a
    /// termination the parent chose from a crash it did not.
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
    process: Arc<Mutex<ChildProcess>>,
    /// The child's last [`CHILD_STDERR_TAIL_LINES`] lines, retained so a crash can say why.
    stderr_tail: Arc<Mutex<Vec<String>>>,
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
        lock(&self.stderr_tail).clone()
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

    /// Stop owning the child's lifetime without signalling it.
    ///
    /// The one way out of [`Drop`]'s kill. After this the process keeps running and this handle
    /// will neither end nor reap it; the returned [`ReleasedChild`] is the only remaining way to
    /// learn what it eventually did, and reaping it is [`watch_released_child`]'s job. A caller
    /// that releases without starting that watcher leaves a process nothing will ever wait on.
    ///
    /// Used by the delegation deadline, which stops the parent waiting rather than stopping the
    /// child: killing a capsule mid-task destroys work that may be nearly done, and the parent
    /// cannot tell the two apart from the outside.
    pub fn release(&mut self) -> ReleasedChild {
        self.released = true;
        ReleasedChild {
            workdir: self.workdir.clone(),
            session_id: self.session_id.clone(),
            delegation_id: self.delegation_id.clone().unwrap_or_default(),
            process: Arc::clone(&self.process),
            stderr_tail: Arc::clone(&self.stderr_tail),
            started: self.started,
        }
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

/// A child this parent no longer owns the lifetime of, and still wants the outcome of.
///
/// Produced by [`LaunchedChild::release`] at a delegation deadline. Holds the same process handle
/// the dropped [`LaunchedChild`] held, so the process can still be observed and reaped — but
/// nothing here signals it, and dropping this without watching simply forgets the process.
pub struct ReleasedChild {
    /// The directory the parent created for this child, absolute.
    pub workdir: PathBuf,
    /// The session id the child's runtime minted for itself.
    pub session_id: String,
    /// The `dlg_` id this delegation is named by, or empty for one the launcher could not name.
    pub delegation_id: String,
    process: Arc<Mutex<ChildProcess>>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    started: Instant,
}

impl std::fmt::Debug for ReleasedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleasedChild")
            .field("workdir", &self.workdir)
            .field("session_id", &self.session_id)
            .field("delegation_id", &self.delegation_id)
            .field("pid", &self.pid())
            .finish()
    }
}

impl ReleasedChild {
    /// The child's operating-system process id. `0` once it has been reaped.
    pub fn pid(&self) -> u32 {
        lock(&self.process)
            .child
            .as_ref()
            .map(Child::id)
            .unwrap_or(0)
    }

    /// A released child around a process this crate started itself, for the watcher's own tests.
    #[cfg(test)]
    pub(crate) fn adopt(child: Child, workdir: PathBuf, delegation_id: String) -> Self {
        Self {
            workdir,
            session_id: "ses_released".to_string(),
            delegation_id,
            process: Arc::new(Mutex::new(ChildProcess {
                child: Some(child),
                deliberate: false,
                status: None,
            })),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
            started: Instant::now(),
        }
    }
}

/// Everything the released-child watcher needs beyond the child, decided by the parent's runtime
/// at the instant the deadline fired.
pub(crate) struct LateReportPlan {
    pub capsule_name: String,
    pub capsule_version: String,
    /// The bound that expired, in whole seconds — carried so the late report can name it.
    pub deadline_secs: u64,
    /// When the deadline fired, so the report can say how much later the child ended.
    pub deadline_at: Instant,
    /// The conversation the delegation was made from.
    pub context_id: String,
    /// [`crate::origin::TaskOrigin::Completion`] with the delegating task's trust inherited.
    pub provenance: TaskProvenance,
    /// The parent's accessible workdir: the root the child's directory and its result file are
    /// named relative to, because that is the root the parent's own tools address.
    pub result_root: PathBuf,
    /// The A2A task id the parent delivered, so the per-task result file is preferred over the
    /// unsuffixed one.
    pub child_task_id: String,
    /// Where the late report goes. A closed channel is a parent whose task loop has ended.
    pub reports: UnboundedSender<DetachedReport>,
}

/// Watch a released child until it ends, then tell the parent once.
///
/// Polls the shared process handle exactly as [`watch_for_completion`] does — never blocking on
/// `wait`, so nothing contends for ownership of the `Child` — and reaps the process when it ends.
/// The parent has already been told about the deadline by the time this thread exists, so the one
/// report it sends is always the second word on this delegation and says so.
///
/// A report that cannot be handed over is recorded rather than dropped: the child's own
/// [`crate::delegation::COMPLETION_FILE`] is written with `delivered: false` and a reason, and a
/// line goes to stderr. That is the shape a parent whose task loop has already ended leaves
/// behind, and it is the only record anything will have of the work.
///
/// Sends once, by construction: one thread, one loop exit, one send.
pub(crate) fn watch_released_child(
    child: ReleasedChild,
    plan: LateReportPlan,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let ending = loop {
            if let Some(ending) = lock(&child.process).poll() {
                break ending;
            }
            std::thread::sleep(CHILD_WATCH_INTERVAL);
        };
        let duration_ms = child
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let after_deadline_ms = plan
            .deadline_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);

        let (status, detail) = match ending {
            // Nothing sets `deliberate` on a released child — release is the opposite of ending
            // it — so this is only reachable if some other owner stopped the process.
            Ending::Deliberate => (
                DelegationStatus::Terminated,
                Some("the process was ended by its launcher after being released".to_string()),
            ),
            Ending::Exited(Some(status)) if status.success() => (DelegationStatus::Ok, None),
            Ending::Exited(status) => {
                let status_text = match status {
                    Some(status) => status.to_string(),
                    None => "unknown exit status".to_string(),
                };
                let tail = lock(&child.stderr_tail).join("\n");
                let detail = if tail.is_empty() {
                    format!("the released capsule ended with {status_text}")
                } else {
                    format!(
                        "the released capsule ended with {status_text}; its last stderr lines \
                         were:\n{tail}"
                    )
                };
                (
                    match status {
                        Some(_) => DelegationStatus::Error,
                        None => DelegationStatus::Crashed,
                    },
                    Some(detail),
                )
            }
        };
        let detail = detail.map(delegation::bound_detail);

        // Two roots, both named from the same file. The completion the child's directory holds is
        // relative to that directory, which is what every reader of a `completion.json` expects;
        // the parent's task names it relative to the parent's own accessible workdir, which is the
        // only root the parent's tools can address.
        let found =
            delegation::child_result_path(&child.workdir, &child.session_id, &plan.child_task_id);
        let relative_to = |root: &Path| -> Option<String> {
            found.as_ref().and_then(|path| {
                path.strip_prefix(root)
                    .ok()
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
            })
        };

        let report = DelegationLateReport {
            delegation_id: child.delegation_id.clone(),
            capsule_name: plan.capsule_name.clone(),
            capsule_version: plan.capsule_version.clone(),
            child_session_id: child.session_id.clone(),
            child_workdir: workdir_relative_to(&child.workdir, &plan.result_root),
            status,
            duration_ms,
            after_deadline_ms,
            deadline_secs: plan.deadline_secs,
            result_path: relative_to(&plan.result_root),
            detail: detail.clone(),
            context_id: plan.context_id.clone(),
            provenance: plan.provenance,
        };

        let delivery_error = plan
            .reports
            .send(DetachedReport::DelegationLate(report))
            .err()
            .map(|_| {
                "the delegating session's task loop was no longer listening, so this outcome \
                 reached no task"
                    .to_string()
            });
        if let Some(reason) = &delivery_error {
            eprintln!(
                "[capsule-runtime] delegation {}: the released capsule ended after its deadline \
                 but {reason}; recorded in {}",
                child.delegation_id,
                delegation::completion_path(&child.workdir).display(),
            );
        }

        // Written whichever way the hand-over went, because a delegation nobody waited for is
        // exactly the one whose only record is this file.
        let outcome = DelegationOutcome {
            delegation_id: child.delegation_id.clone(),
            capsule_name: plan.capsule_name,
            capsule_version: plan.capsule_version,
            session_id: child.session_id.clone(),
            status,
            result_path: relative_to(&child.workdir),
            workdir: child.workdir.display().to_string(),
            duration_ms,
            detail,
            reported_by: Reporter::Launcher,
            delivered: delivery_error.is_none(),
            delivery_error,
        };
        if let Err(reason) = delegation::write_completion(&child.workdir, &outcome) {
            eprintln!(
                "[capsule-runtime] delegation {}: {reason}",
                child.delegation_id
            );
        }
    })
}

/// `workdir` named from `root`, falling back to the absolute path when it sits outside it.
fn workdir_relative_to(workdir: &Path, root: &Path) -> String {
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

    let mut command = Command::new(&binary);
    command
        .args(&argv[1..])
        .current_dir(&workdir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &env {
        command.env(key, value);
    }

    let mut child = command.spawn().map_err(|error| {
        RuntimeError::Runtime(format!(
            "failed to start the child capsule process '{}': {error}",
            binary.display()
        ))
    })?;

    // The grant's one and only appearance outside the parent's memory: one line on a pipe that is
    // closed immediately afterwards. Failing to write it is fatal — the child would otherwise sit
    // waiting on a line that will never come.
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "the child process exposed no standard input".to_string())
        .and_then(|mut stdin| {
            writeln!(stdin, "{}", request.grant.expose())
                .map_err(|error| format!("failed to hand the child its launch grant: {error}"))
        });
    let stderr_tail = drain_stderr(&mut child);
    let mut launched = LaunchedChild {
        workdir,
        session_id: String::new(),
        capsule_url: String::new(),
        argv,
        env,
        delegation_id: handle.as_ref().map(|handle| handle.delegation_id.clone()),
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
            );
        }
    }
    Ok(launched)
}

/// Report for a child that ended without reporting for itself.
///
/// Polls the shared process handle rather than blocking on `wait`, so it never contends with
/// [`LaunchedChild::shutdown`] or `Drop` for ownership of the `Child`. What it does when the
/// process ends is decided by what the child left behind:
///
/// * A completion the child already delivered is left alone — exactly one completion per
///   delegation reaches the parent.
/// * A completion the child recorded but could not deliver is retried once, and the file is
///   rewritten with the result. Nothing is retried after that.
/// * No completion at all means the child died without a word: the watcher builds one with
///   `status: crashed`, carrying the exit status and the child's bounded stderr tail, and posts
///   it in the child's place.
/// * An ending the parent chose is recorded as `terminated` and posted to nobody.
fn watch_for_completion(
    launched: &LaunchedChild,
    handle: SpawnerHandle,
    address: CompletionAddress,
    capsule_name: &str,
    capsule_version: &str,
) {
    let process = Arc::clone(&launched.process);
    let stderr_tail = Arc::clone(&launched.stderr_tail);
    let workdir = launched.workdir.clone();
    let session_id = launched.session_id.clone();
    let capsule_name = capsule_name.to_string();
    let capsule_version = capsule_version.to_string();
    let started = launched.started;

    std::thread::spawn(move || {
        let ending = loop {
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
                eprintln!(
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
                let tail = lock(&stderr_tail).join("\n");
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
/// The runtime-owned names — `PATH`, `HOME`, `MURMUR_ROOST_URL`, and [`SPAWNER_ENV`] on a
/// delegated launch — are applied last, so a child cannot displace the daemon URL it is required
/// to register with, or the handle it reports its outcome to, by allowlisting the name.
fn child_environment(
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
        !matches!(key.as_str(), "PATH" | "HOME" | "MURMUR_ROOST_URL") && key != SPAWNER_ENV
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
    env
}

/// The binary a child is started from: [`MUR_BINARY_ENV`] when set, else this process's own image.
fn mur_binary() -> Result<PathBuf, RuntimeError> {
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

/// Echo the child's stderr to this process's, keeping the last [`CHILD_STDERR_TAIL_LINES`] so a
/// child that dies before reporting can say why.
///
/// The child's diagnostics belong to the operator running the parent, so nothing is swallowed —
/// this only remembers, in addition to printing.
fn drain_stderr(child: &mut Child) -> Arc<Mutex<Vec<String>>> {
    let tail = Arc::new(Mutex::new(Vec::new()));
    let Some(stderr) = child.stderr.take() else {
        return tail;
    };
    let collector = Arc::clone(&tail);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("{line}");
            let mut tail = collector.lock().unwrap_or_else(|e| e.into_inner());
            if tail.len() == CHILD_STDERR_TAIL_LINES {
                tail.remove(0);
            }
            tail.push(line);
        }
    });
    tail
}

/// Read the child's first standard-output line, then keep draining the pipe.
///
/// The drain matters: a child whose stdout pipe fills blocks forever, and a capsule that logs
/// after its launch line would otherwise deadlock against a parent that had stopped reading.
fn first_json_line(
    launched: &mut LaunchedChild,
    stderr_tail: &Arc<Mutex<Vec<String>>>,
) -> Result<String, RuntimeError> {
    let stdout = lock(&launched.process)
        .child
        .as_mut()
        .and_then(|child| child.stdout.take())
        .ok_or_else(|| {
            RuntimeError::Runtime("the child process exposed no standard output".to_string())
        })?;

    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let first = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end().to_string()),
        };
        let _ = tx.send(first);
        // Everything the child says after its launch line goes to this process's own stdout, so
        // the pipe stays drained for as long as the child lives.
        let mut rest = String::new();
        while let Ok(read) = reader.read_line(&mut rest) {
            if read == 0 {
                break;
            }
            print!("{rest}");
            rest.clear();
        }
    });

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
            let reason = stderr_tail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .join("\n");
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

    fn late_plan(reports: UnboundedSender<DetachedReport>, workdir: &Path) -> LateReportPlan {
        LateReportPlan {
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            deadline_secs: 20,
            deadline_at: Instant::now(),
            context_id: "ctx_released".to_string(),
            provenance: TaskProvenance::derive(crate::origin::TaskOrigin::Completion, None),
            result_root: workdir.to_path_buf(),
            child_task_id: "task-1".to_string(),
            reports,
        }
    }

    /// A released child that ends after its deadline is reported once, on the channel.
    #[test]
    fn a_released_child_that_ends_is_reported_once() {
        let workdir = tempfile::tempdir().unwrap();
        let child = Command::new("sh")
            .args(["-c", "sleep 0.2"])
            .spawn()
            .expect("sh is on the test host");
        let (reports, mut receiver) = tokio::sync::mpsc::unbounded_channel();

        let released =
            ReleasedChild::adopt(child, workdir.path().to_path_buf(), "dlg_late".to_string());
        watch_released_child(released, late_plan(reports, workdir.path()))
            .join()
            .expect("the watcher thread returns");

        let report = match receiver.try_recv() {
            Ok(DetachedReport::DelegationLate(report)) => report,
            other => panic!("expected one late delegation report; got {other:?}"),
        };
        assert_eq!(report.delegation_id, "dlg_late");
        assert_eq!(report.status, DelegationStatus::Ok);
        assert_eq!(report.deadline_secs, 20);
        assert!(report.detail.is_none(), "{report:?}");
        // The watcher owned the only remaining sender, so the channel is closed behind its one
        // report rather than empty: either way there is no second report on it.
        assert!(
            receiver.try_recv().is_err(),
            "the watcher reports once and no more"
        );

        let recorded =
            delegation::read_completion(workdir.path()).expect("a completion is written");
        assert!(recorded.delivered, "{recorded:?}");
        assert_eq!(recorded.reported_by, Reporter::Launcher);
        assert!(recorded.delivery_error.is_none(), "{recorded:?}");
    }

    /// A late outcome with nowhere to go is recorded rather than dropped, and the released child is
    /// still reaped. This is the shape a parent whose task loop has already ended leaves behind.
    #[test]
    fn a_late_outcome_with_no_listener_is_recorded_and_the_child_is_reaped() {
        let workdir = tempfile::tempdir().unwrap();
        let child = Command::new("sh")
            .args(["-c", "sleep 0.2; exit 3"])
            .spawn()
            .expect("sh is on the test host");
        let pid = child.id();
        let (reports, receiver) = tokio::sync::mpsc::unbounded_channel();
        // The parent's task loop is gone before the child ends.
        drop(receiver);

        let released = ReleasedChild::adopt(
            child,
            workdir.path().to_path_buf(),
            "dlg_orphan".to_string(),
        );
        watch_released_child(released, late_plan(reports, workdir.path()))
            .join()
            .expect("the watcher thread returns rather than panicking");

        let recorded =
            delegation::read_completion(workdir.path()).expect("a completion is written");
        assert!(!recorded.delivered, "{recorded:?}");
        assert_eq!(recorded.status, DelegationStatus::Error);
        assert_eq!(recorded.delegation_id, "dlg_orphan");
        let reason = recorded
            .delivery_error
            .clone()
            .expect("the refusal is recorded");
        assert!(
            reason.contains("no longer listening"),
            "the reason names why the outcome reached no task: {reason}"
        );
        assert!(
            recorded
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("exit status: 3")),
            "{recorded:?}"
        );

        // Reaped by the watcher and by nobody else: a process the runtime waited on has no
        // `/proc` entry at all, while a zombie would still have one.
        #[cfg(target_os = "linux")]
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the released child is reaped by the watcher"
        );
        let _ = pid;
    }

    /// A launch with no spawner injects nothing, even from a decoy the launching process holds.
    #[test]
    fn a_launch_without_a_spawner_injects_no_handle() {
        std::env::set_var(SPAWNER_ENV, decoy());

        let env = child_environment(&request(&[SPAWNER_ENV], None), None);

        assert!(!env.iter().any(|(key, _)| key == SPAWNER_ENV), "{env:?}");
        std::env::remove_var(SPAWNER_ENV);
    }
}
