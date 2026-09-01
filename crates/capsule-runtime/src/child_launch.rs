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

use crate::delegation::{
    self, DelegationOutcome, DelegationStatus, Reporter, Spawner, SpawnerHandle, SPAWNER_ENV,
};
use crate::errors::RuntimeError;
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
    /// Where this child reports its outcome, or `None` for a launch nobody wants told.
    ///
    /// `Some` is what turns the launch into a delegation: the launcher mints a delegation id,
    /// injects a [`SpawnerHandle`] as [`SPAWNER_ENV`], and starts the watcher that reports for a
    /// child that could not report for itself. `None` injects nothing and starts nothing.
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
}

impl Drop for LaunchedChild {
    /// Terminates and reaps, so a parent that returns early — including by panicking — leaves no
    /// orphaned capsule process behind holding a port and a directory. Deliberate on the same
    /// terms as [`LaunchedChild::shutdown`].
    fn drop(&mut self) {
        let mut process = lock(&self.process);
        process.deliberate = true;
        let _ = process.end();
    }
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

    // Started only for a launch somebody wants told about: a `capsule` plan step passes no
    // spawner, so it starts no watcher and behaves exactly as it did.
    if let Some(handle) = handle {
        watch_for_completion(
            &launched,
            handle,
            &request.capsule_name,
            &request.capsule_version,
        );
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
            let retried = delegation::report_completion(&handle, recorded, &workdir);
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
/// The three runtime-owned names are applied last, so a child cannot displace the daemon URL it is
/// required to register with by allowlisting its name.
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

    fn spawner() -> Spawner {
        Spawner {
            url: "http://127.0.0.1:7000".to_string(),
            session_id: "ses_parent".to_string(),
            context_id: "ctx_parent".to_string(),
            trust: crate::origin::TrustClass::Trusted,
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
        std::env::set_var(SPAWNER_ENV, "decoy");
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
    }

    /// A launch with no spawner injects nothing, even from a decoy the launching process holds.
    #[test]
    fn a_launch_without_a_spawner_injects_no_handle() {
        std::env::set_var(SPAWNER_ENV, "decoy");

        let env = child_environment(&request(&[SPAWNER_ENV], None), None);

        assert!(!env.iter().any(|(key, _)| key == SPAWNER_ENV), "{env:?}");
    }
}
