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
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

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
    /// `None` once [`LaunchedChild::shutdown`] has reaped the process.
    child: Option<Child>,
}

impl std::fmt::Debug for LaunchedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchedChild")
            .field("workdir", &self.workdir)
            .field("session_id", &self.session_id)
            .field("capsule_url", &self.capsule_url)
            .field("pid", &self.child.as_ref().map(Child::id))
            .finish()
    }
}

impl LaunchedChild {
    /// The child's operating-system process id. `0` once it has been reaped.
    pub fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    /// Terminate the child and reap it. Idempotent: a second call, or a call after `Drop` has
    /// already run, does nothing.
    pub fn shutdown(&mut self) -> Result<(), RuntimeError> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // `kill` on an already-exited process is not an error worth surfacing — the wait below is
        // what actually retires the entry in the process table.
        let _ = child.kill();
        child.wait().map(|_| ()).map_err(|error| {
            RuntimeError::Runtime(format!("failed to reap child capsule: {error}"))
        })
    }
}

impl Drop for LaunchedChild {
    /// Terminates and reaps, so a parent that returns early — including by panicking — leaves no
    /// orphaned capsule process behind holding a port and a directory.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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
    let env = child_environment(&request);

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
        child: Some(child),
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
    Ok(launched)
}

/// The child's complete environment, built from a cleared one.
///
/// `capabilities.env.allow` is the child's own, not the parent's: a sibling's declaration reaches
/// nothing here, and a variable the parent holds but the child did not declare is simply absent.
/// The three runtime-owned names are applied last, so a child cannot displace the daemon URL it is
/// required to register with by allowlisting its name.
fn child_environment(request: &ChildLaunchRequest) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for key in &request.child_env_allow {
        if let Ok(value) = std::env::var(key) {
            env.push((key.clone(), value));
        }
    }
    env.retain(|(key, _)| !matches!(key.as_str(), "PATH" | "HOME" | "MURMUR_ROOST_URL"));

    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_string(), path));
    }
    if let Ok(home) = std::env::var("HOME") {
        env.push(("HOME".to_string(), home));
    }
    env.push(("MURMUR_ROOST_URL".to_string(), request.roost_url.clone()));
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
    let stdout = launched
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
            let status = launched
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

    #[test]
    fn a_childs_environment_is_built_from_its_own_declaration() {
        std::env::set_var("MURMUR_CHILD_LAUNCH_TEST_A", "a");
        std::env::set_var("MURMUR_CHILD_LAUNCH_TEST_B", "b");
        let request = ChildLaunchRequest {
            parent_accessible_workdir: PathBuf::from("/tmp/parent"),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            grant: SpawnApproval::new("msa1.token".to_string()),
            child_env_allow: vec!["MURMUR_CHILD_LAUNCH_TEST_A".to_string()],
            roost_url: "http://127.0.0.1:7700".to_string(),
        };

        let env = child_environment(&request);
        let names: Vec<&str> = env.iter().map(|(key, _)| key.as_str()).collect();

        assert!(names.contains(&"MURMUR_CHILD_LAUNCH_TEST_A"));
        assert!(!names.contains(&"MURMUR_CHILD_LAUNCH_TEST_B"));
        assert!(names.contains(&"MURMUR_ROOST_URL"));
        assert!(!env.iter().any(|(_, value)| value.contains("msa1.token")));
    }

    #[test]
    fn a_runtime_owned_name_cannot_be_displaced_by_a_declaration() {
        std::env::set_var("MURMUR_ROOST_URL", "http://127.0.0.1:1");
        let request = ChildLaunchRequest {
            parent_accessible_workdir: PathBuf::from("/tmp/parent"),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            grant: SpawnApproval::new("msa1.token".to_string()),
            child_env_allow: vec!["MURMUR_ROOST_URL".to_string()],
            roost_url: "http://127.0.0.1:7700".to_string(),
        };

        let env = child_environment(&request);
        let urls: Vec<&String> = env
            .iter()
            .filter(|(key, _)| key == "MURMUR_ROOST_URL")
            .map(|(_, value)| value)
            .collect();

        assert_eq!(urls, vec!["http://127.0.0.1:7700"]);
    }
}
