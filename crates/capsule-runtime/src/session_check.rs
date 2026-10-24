//! One command run in a session's work folder after the session has ended, under the containment
//! the session's own commands had: what `mur eval run` judges a case's work by.
//!
//! The check is not part of the session. [`StagedSession::check_runner`] captures, before launch,
//! what the session's [`crate::sandbox::ShellEnforcement`] is built from; [`CheckRunner::run`]
//! builds a new enforcement from it once `launch_session` has returned and every process the
//! session started is dead. The command, its files and its output reach no trace, no provider
//! request and no file the agent could read while it ran.
//!
//! The enforcement is the one `runtime::launch_session` installs: the same capability policy,
//! declared floor, host probe and `/tmp` mapping, so the same shell allowlist, Landlock scope,
//! seccomp filter, network namespace and resource limits. Only its scope is new: a cgroup scope,
//! runtime directory and network namespace of its own, each ended or removed when the check
//! returns. `$HOME` and `$TMPDIR` are the session's `home/` and `tmp/`, as for its commands.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicI32, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    sandbox::{HostProbe, ShellEnforcement},
    shell::{ShellExecError, ShellResult},
    types::{CapabilityPolicy, StagedSession},
    workdir_fs::WorkdirRoot,
};

/// How much of the check's output [`CheckOutcome::output_tail`] keeps: the last bytes of stdout
/// followed by stderr.
pub const CHECK_OUTPUT_TAIL_BYTES: usize = 4096;

/// Where the check's captured stdout then stderr are written, beneath the session's records
/// directory.
pub const CHECK_LOG_FILE: &str = "logs/check.log";

/// How long a timed-out check's command is given, once its processes are killed, to hand back
/// what it wrote before the outcome is reported without it.
const KILLED_OUTPUT_WAIT: Duration = Duration::from_secs(10);

/// A command to run in a session's work folder once the session has ended.
#[derive(Debug, Clone)]
pub struct SessionCheck {
    command: String,
    binary: String,
    args: Vec<String>,
    files: Option<PathBuf>,
    timeout: Duration,
}

impl SessionCheck {
    /// `command` split into a binary and its arguments the way the shell tool splits a command
    /// line: whitespace separates words, quotes group them, a backslash escapes one character,
    /// and nothing is expanded. `files` is a directory copied into the work folder's root before
    /// the command runs. Refuses a command with no words and a zero `timeout`.
    pub fn new(command: &str, files: Option<PathBuf>, timeout: Duration) -> Result<Self, String> {
        let mut words = crate::shell::split_shell_words(command);
        if words.is_empty() {
            return Err("the check command is empty".to_string());
        }
        if timeout.is_zero() {
            return Err("the check timeout is zero".to_string());
        }
        let binary = words.remove(0);
        Ok(Self {
            command: command.to_string(),
            binary,
            args: words,
            files,
            timeout,
        })
    }

    /// The binary the command runs, which must be in `capabilities.shell.allow`.
    #[must_use]
    pub fn binary(&self) -> &str {
        &self.binary
    }

    /// The arguments after [`Self::binary`].
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The command line as given.
    #[must_use]
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The directory copied into the work folder before the command runs.
    #[must_use]
    pub fn files(&self) -> Option<&Path> {
        self.files.as_deref()
    }

    /// How long the command may run before it is killed.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// How a check ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    /// The command exited 0.
    Pass,
    /// The command exited non-zero, or was killed for a resource limit.
    Fail,
    /// The command outran the check's timeout and was killed.
    TimedOut,
    /// The command never ran to an exit: its files could not be copied, its enforcement could
    /// not be built, or it could not be started.
    Error,
}

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub result: CheckResult,
    /// The command's exit code. `None` when it timed out or never ran.
    pub exit_code: Option<i32>,
    /// From the start of the command to its exit or kill; 0 when it never started.
    pub duration_ms: u64,
    /// The host resource limit that killed the command, as the shell tool names it.
    pub resource_limit: Option<String>,
    /// The last [`CHECK_OUTPUT_TAIL_BYTES`] bytes of stdout followed by stderr.
    pub output_tail: String,
    /// The full captured output, [`CHECK_LOG_FILE`] in the session's records directory. `None`
    /// when the command produced no result to write.
    pub output_log: Option<PathBuf>,
    /// Why the result is [`CheckResult::Error`], or what went wrong around a result.
    pub error: Option<String>,
    /// The containment class the session's `session_start` recorded as achieved, which the check
    /// ran under.
    pub containment: String,
}

/// What a session's containment was built from, captured before launch, to run a
/// [`SessionCheck`] under it after the session ends.
#[derive(Debug, Clone)]
pub struct CheckRunner {
    session_id: String,
    policy: CapabilityPolicy,
    declared_floor: murmur_artifact::ContainmentClass,
    host_probe: HostProbe,
    temp_mapping: crate::temp_dir::TempMapping,
    accessible_root: WorkdirRoot,
    records_root: WorkdirRoot,
    records_dir: PathBuf,
    /// `~/.murmur/sessions`, where the session's runtime directory was and the check's goes.
    sessions_dir: Option<PathBuf>,
    containment: murmur_artifact::ContainmentClass,
}

impl StagedSession {
    /// What [`CheckRunner::run`] needs to run a check under this session's containment once the
    /// session has ended. Taken before `launch_session`, which consumes the session.
    #[must_use]
    pub fn check_runner(&self) -> CheckRunner {
        CheckRunner {
            session_id: self.session_id.clone(),
            policy: self.capability_policy.clone(),
            declared_floor: self.declared_containment_floor,
            host_probe: self.host_probe.clone(),
            temp_mapping: self.temp_mapping,
            accessible_root: self.accessible_root.clone(),
            records_root: self.records_root.clone(),
            records_dir: self.records_dir.clone(),
            sessions_dir: self
                .runtime_dir
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            containment: self.scope_report.achieved_containment,
        }
    }
}

impl CheckRunner {
    /// The session's records directory, where [`CHECK_LOG_FILE`] goes.
    #[must_use]
    pub fn records_dir(&self) -> &Path {
        &self.records_dir
    }

    /// Copies `check`'s files into the work folder and runs its command there, under the
    /// session's containment, then kills everything it started. Every failure is an outcome.
    ///
    /// Call only after `launch_session` has returned: the session's processes are dead then, and
    /// nothing the session started can see the files or the command.
    pub fn run(&self, check: &SessionCheck) -> CheckOutcome {
        let containment = self.containment.as_str().to_string();
        let error = |error: String| CheckOutcome {
            result: CheckResult::Error,
            exit_code: None,
            duration_ms: 0,
            resource_limit: None,
            output_tail: String::new(),
            output_log: None,
            error: Some(error),
            containment: containment.clone(),
        };

        if let Some(files) = check.files() {
            if let Err(refusal) = copy_tree_into(files, &self.accessible_root) {
                return error(format!("could not copy the check's files: {refusal}"));
            }
        }

        let checked = match self.enforcement() {
            Ok((enforcement, runtime_dir)) => {
                let checked =
                    run_with_timeout(check, &self.accessible_root, &self.policy, &enforcement);
                // Whatever the command left running is killed and its namespace ended before the
                // runtime directory it was told of is removed.
                enforcement.end_session();
                drop(runtime_dir);
                checked
            }
            Err(reason) => return error(reason),
        };

        let (result, exit_code, duration_ms, shell) = match checked {
            Checked::Exited(shell) => {
                let result = if shell.exit_code == 0 {
                    CheckResult::Pass
                } else {
                    CheckResult::Fail
                };
                (
                    result,
                    Some(shell.exit_code),
                    shell.duration_ms,
                    Some(shell),
                )
            }
            Checked::TimedOut { elapsed, shell } => (
                CheckResult::TimedOut,
                None,
                elapsed.as_millis().try_into().unwrap_or(u64::MAX),
                shell,
            ),
            Checked::Failed(reason) => return error(reason),
        };

        let (stdout, stderr) = shell
            .as_ref()
            .map(|shell| (shell.stdout.as_str(), shell.stderr.as_str()))
            .unwrap_or_default();
        let mut output = Vec::with_capacity(stdout.len() + stderr.len());
        output.extend_from_slice(stdout.as_bytes());
        output.extend_from_slice(stderr.as_bytes());
        let (output_log, log_error) = match self.records_root.write_file_exact_mode(
            CHECK_LOG_FILE,
            &output,
            crate::murmur_home::PRIVATE_FILE_MODE,
        ) {
            Ok(()) => (Some(self.records_dir.join(CHECK_LOG_FILE)), None),
            Err(error) => (
                None,
                Some(format!("could not write {CHECK_LOG_FILE}: {error}")),
            ),
        };
        let tail_start = output.len().saturating_sub(CHECK_OUTPUT_TAIL_BYTES);

        CheckOutcome {
            result,
            exit_code,
            duration_ms,
            resource_limit: shell
                .as_ref()
                .and_then(|shell| shell.resource_limit_hit.clone()),
            output_tail: String::from_utf8_lossy(&output[tail_start..]).into_owned(),
            output_log,
            error: log_error,
            containment,
        }
    }

    /// The enforcement `launch_session` builds for a session's commands, rebuilt from what the
    /// session's was built from, with a cgroup scope, workdir guard and runtime directory of its
    /// own. The runtime directory is removed when the returned value drops.
    fn enforcement(&self) -> Result<(ShellEnforcement, crate::runtime_dir::RuntimeDir), String> {
        let check_id = format!("{}-check", self.session_id);
        let prepared = crate::runtime::prepare_launch_scope(
            &self.policy,
            false,
            &check_id,
            self.accessible_root.path(),
        )
        .map_err(|error| error.to_string())?;
        let temp = self
            .records_root
            .open_beneath(crate::temp_dir::SESSION_TEMP_DIR_NAME)
            .map_err(|error| error.to_string())?;
        let home = self
            .records_root
            .open_beneath(crate::records_dir::HOME_SUBDIR)
            .map_err(|error| error.to_string())?;
        let workdir_guard = crate::resources::WorkdirGuard::spawn(
            &self.accessible_root,
            temp.as_ref(),
            home.as_ref(),
            Some(&self.records_root),
            self.policy.resources.workdir_max_bytes,
        );
        let sessions_dir = match &self.sessions_dir {
            Some(dir) => dir.clone(),
            None => crate::runtime_dir::sessions_dir().map_err(|error| error.to_string())?,
        };
        let runtime_dir =
            crate::runtime_dir::RuntimeDir::create(&sessions_dir, &check_id, &self.accessible_root)
                .map_err(|reason| format!("the check's runtime directory: {reason}"))?;
        let mut roots = runtime_dir
            .roots()
            .clone()
            .with_records(self.records_root.clone());
        if let Some(temp) = temp {
            roots = roots.with_temp(temp);
        }
        if let Some(home) = home {
            roots = roots.with_home(home);
        }
        let enforcement =
            ShellEnforcement::resolve(&self.policy, self.declared_floor, self.host_probe.clone())?
                .with_host_bounding(prepared.scope, Some(workdir_guard))
                .with_runtime_dir(roots, None)
                .with_temp_mapping(self.temp_mapping);
        Ok((enforcement, runtime_dir))
    }
}

/// How the check's command ended, before it is judged.
enum Checked {
    Exited(ShellResult),
    /// Killed at the timeout. `shell` is what it had written, when it handed that back in time.
    TimedOut {
        elapsed: Duration,
        shell: Option<ShellResult>,
    },
    Failed(String),
}

/// Runs the command on a thread of its own and waits at most `check.timeout` for it. At the
/// timeout every process in the check's cgroup scope is killed, and its process group with them,
/// which is the only kill where there is no scope.
fn run_with_timeout(
    check: &SessionCheck,
    workdir: &WorkdirRoot,
    policy: &CapabilityPolicy,
    enforcement: &ShellEnforcement,
) -> Checked {
    let (sender, receiver) = mpsc::channel::<Result<ShellResult, ShellExecError>>();
    let group = Arc::new(AtomicI32::new(0));
    let started = Instant::now();
    {
        let binary = check.binary.clone();
        let args = check.args.clone();
        let workdir = workdir.clone();
        let policy = policy.clone();
        let enforcement = enforcement.clone();
        let group = Arc::clone(&group);
        let spawned = std::thread::Builder::new()
            .name("session-check".to_string())
            .spawn(move || {
                crate::shell::report_spawned_group_to(group);
                let args = args.iter().map(String::as_str).collect::<Vec<_>>();
                let result = crate::shell::execute_shell(
                    &binary,
                    &args,
                    &[],
                    &workdir,
                    &policy,
                    &enforcement,
                );
                let _ = sender.send(result);
            });
        if let Err(error) = spawned {
            return Checked::Failed(format!("could not start the check: {error}"));
        }
    }

    let settled = |result: Result<ShellResult, ShellExecError>| match result {
        Ok(shell) => Checked::Exited(shell),
        Err(error) => Checked::Failed(error.to_string()),
    };
    match receiver.recv_timeout(check.timeout) {
        Ok(result) => settled(result),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Checked::Failed("the check's thread ended without a result".to_string())
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let elapsed = started.elapsed();
            enforcement.kill_session_processes();
            let pgid = group.load(Ordering::SeqCst);
            if pgid > 0 {
                crate::formation_launch::signal_group(i64::from(pgid), libc::SIGKILL);
            }
            let shell = match receiver.recv_timeout(KILLED_OUTPUT_WAIT) {
                Ok(Ok(shell)) => Some(shell),
                _ => None,
            };
            Checked::TimedOut { elapsed, shell }
        }
    }
}

/// One entry beneath an operator's directory tree: an eval case's `start` or `check.files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Relative to the tree's root.
    pub relative: PathBuf,
    pub is_dir: bool,
    /// `mode & 0o7777`.
    pub mode: u32,
}

/// Every entry beneath `root`, in byte-wise order of the relative path, so a parent comes before
/// what it holds. Read with no link followed: a symbolic link or a special file anywhere in the
/// tree is refused, naming it.
pub fn walk_tree(root: &Path) -> Result<Vec<TreeEntry>, String> {
    use std::os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _};

    fn walk(dir: &Path, relative: &Path, out: &mut Vec<TreeEntry>) -> Result<(), String> {
        let read = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
        for entry in std::fs::read_dir(dir).map_err(|error| read(dir, error))? {
            let entry = entry.map_err(|error| read(dir, error))?;
            let path = entry.path();
            let rel = relative.join(entry.file_name());
            let meta = std::fs::symlink_metadata(&path).map_err(|error| read(&path, error))?;
            let mode = meta.permissions().mode() & 0o7777;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                return Err(format!("{} is a symbolic link", path.display()));
            }
            if file_type.is_dir() {
                out.push(TreeEntry {
                    relative: rel.clone(),
                    is_dir: true,
                    mode,
                });
                walk(&path, &rel, out)?;
            } else if file_type.is_file() {
                out.push(TreeEntry {
                    relative: rel,
                    is_dir: false,
                    mode,
                });
            } else {
                return Err(format!("{} is a special file", path.display()));
            }
        }
        Ok(())
    }

    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries)?;
    entries.sort_by(|a, b| {
        a.relative
            .as_os_str()
            .as_bytes()
            .cmp(b.relative.as_os_str().as_bytes())
    });
    Ok(entries)
}

/// Copies the tree at `source` into `workdir`'s root: directories and regular files, each
/// entry's permission bits kept, an existing file overwritten.
///
/// The whole of `source` is walked by [`walk_tree`] before anything is written, so a link or
/// special file in it refuses the copy with the work folder untouched. Every write goes through
/// `workdir`, so nothing already in the work folder — a link where a directory of the tree goes,
/// notably — is followed; such a refusal names the path. Each directory is held at owner `rwx`
/// while it is filled and given its own mode last, deepest first, so a read-only directory is
/// filled before it is closed.
pub fn copy_tree_into(source: &Path, workdir: &WorkdirRoot) -> Result<(), String> {
    const FILLING_DIR_MODE: u32 = 0o700;

    let entries = walk_tree(source)?;
    for entry in &entries {
        let rel = entry.relative.to_string_lossy();
        if entry.is_dir {
            workdir
                .create_dir_all(rel.as_ref(), FILLING_DIR_MODE)
                .and_then(|()| workdir.set_dir_mode(rel.as_ref(), FILLING_DIR_MODE))
                .map_err(|error| error.to_string())?;
        } else {
            let path = source.join(&entry.relative);
            let bytes =
                std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            workdir
                .write_file_exact_mode(rel.as_ref(), &bytes, entry.mode)
                .map_err(|error| error.to_string())?;
        }
    }
    for entry in entries.iter().rev().filter(|entry| entry.is_dir) {
        let rel = entry.relative.to_string_lossy();
        workdir
            .set_dir_mode(rel.as_ref(), entry.mode)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_splits_as_the_shell_tool_splits_it() {
        let command = r#"grep -q "fixed value" 'a b'/c\ d.txt"#;
        let check = SessionCheck::new(command, None, Duration::from_secs(1)).unwrap();
        let mut words = crate::shell::split_shell_words(command);
        assert_eq!(check.binary(), words.remove(0));
        assert_eq!(check.args(), words.as_slice());
        assert_eq!(check.binary(), "grep");
        assert_eq!(check.args(), ["-q", "fixed value", "a b/c d.txt"]);
        assert_eq!(check.command(), command);
    }

    #[test]
    fn an_empty_command_is_refused() {
        for command in ["", "   ", "\t\n"] {
            let refused = SessionCheck::new(command, None, Duration::from_secs(1)).unwrap_err();
            assert!(refused.contains("empty"), "{refused}");
        }
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        let refused = SessionCheck::new("true", None, Duration::ZERO).unwrap_err();
        assert!(refused.contains("timeout"), "{refused}");
    }

    #[test]
    fn outcomes_serialize_in_snake_case() {
        let outcome = CheckOutcome {
            result: CheckResult::TimedOut,
            exit_code: None,
            duration_ms: 2000,
            resource_limit: None,
            output_tail: String::new(),
            output_log: None,
            error: None,
            containment: "scoped".to_string(),
        };
        let json = serde_json::to_value(&outcome).unwrap();
        assert_eq!(json["result"], "timed_out");
        assert_eq!(json["containment"], "scoped");
        let back: CheckOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, outcome);
    }

    #[test]
    fn check_files_with_a_link_are_refused_before_anything_is_written() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", source.path().join("z-link")).unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        let refused = copy_tree_into(source.path(), &root).unwrap_err();
        assert!(refused.contains("z-link"), "{refused}");
        assert!(!workdir.path().join("a.txt").exists());
    }

    #[test]
    fn check_files_keep_their_mode_and_overwrite_what_is_there() {
        use std::os::unix::fs::PermissionsExt as _;
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("sub")).unwrap();
        std::fs::write(source.path().join("sub/run.sh"), "new").unwrap();
        std::fs::set_permissions(
            source.path().join("sub/run.sh"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
        let workdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(workdir.path().join("sub")).unwrap();
        std::fs::write(workdir.path().join("sub/run.sh"), "old").unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        copy_tree_into(source.path(), &root).unwrap();
        let copied = workdir.path().join("sub/run.sh");
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&copied).unwrap().permissions().mode() & 0o7777,
            0o751
        );
    }

    #[test]
    fn a_read_only_directory_in_check_files_is_filled_then_closed() {
        use std::os::unix::fs::PermissionsExt as _;
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("ro")).unwrap();
        std::fs::write(source.path().join("ro/t.txt"), "t").unwrap();
        std::fs::set_permissions(
            source.path().join("ro"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        copy_tree_into(source.path(), &root).unwrap();
        let copied = workdir.path().join("ro");
        assert_eq!(std::fs::read_to_string(copied.join("t.txt")).unwrap(), "t");
        assert_eq!(
            std::fs::metadata(&copied).unwrap().permissions().mode() & 0o7777,
            0o555
        );
        // Copied again over the read-only directory now in the work folder, as a second check
        // into the same folder would.
        copy_tree_into(source.path(), &root).unwrap();
        for dir in [source.path().join("ro"), copied] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn a_link_the_agent_left_is_not_followed() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("hidden")).unwrap();
        std::fs::write(source.path().join("hidden/check.txt"), "x").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), workdir.path().join("hidden")).unwrap();
        let root = WorkdirRoot::open(workdir.path()).unwrap();
        let refused = copy_tree_into(source.path(), &root).unwrap_err();
        assert!(refused.contains("hidden"), "{refused}");
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}
