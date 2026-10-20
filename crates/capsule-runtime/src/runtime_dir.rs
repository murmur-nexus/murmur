//! The session's runtime directory: every file the runtime writes for its own use, outside the
//! workdir the capsule's commands can write.
//!
//! ```text
//! ~/.murmur/sessions/                 0700
//! └── <session-id>/                   0700, created at staging, removed when the session ends
//!     ├── .lock                       flock held from creation until removal
//!     ├── runtime/                    $MURMUR_RUNTIME_DIR: MURMUR.md, task.md, murmur.yaml,
//!     │                               tools/, plans/ — readable by commands, never writable
//!     ├── sealed-etc/                 the composed root's /etc/passwd and /etc/group
//!     └── harness/<run-id>/           a `transport: process` harness's files
//! ```
//!
//! On the Full tier a Landlock rule grants `runtime/` read-only to every command; on the Sealed
//! tier it is also bound read-only into the composed root at its own path. `sealed-etc/` and
//! `harness/` get no rule, so no command opens them. [`crate::containment::runtime_dir_report`]
//! says per tier what holds.
//!
//! Every runtime read and write beneath the directory goes through a
//! [`crate::workdir_fs::WorkdirRoot`] sharing the session's refusal latch, so a link is refused
//! there exactly as it is in the workdir.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags};

use crate::protected_paths::canonicalize_existing_prefix;
use crate::workdir_fs::WorkdirRoot;

/// The variable naming the runtime directory in every shell command's, plan shell step's, native
/// tool's and `transport: process` harness's environment. Runtime-owned like `HOME`: no
/// manifest, override or host value can set it.
pub const RUNTIME_DIR_ENV: &str = "MURMUR_RUNTIME_DIR";

/// The guest path a `runtime: tool` with no `capabilities.filesystem.scope` finds the runtime
/// directory at, preopened read-only beside its `"."`.
pub const RUNTIME_DIR_GUEST_PATH: &str = "murmur-runtime";

/// Whether an artifact's guest gets the read-only [`RUNTIME_DIR_GUEST_PATH`] preopen: a
/// `runtime: tool` with no `capabilities.filesystem.scope`, which also holds the whole workdir
/// at `"."`. A driver, a hook and a tool confined to a subtree get none.
#[must_use]
pub(crate) fn gets_runtime_dir_preopen(
    runtime: &murmur_artifact::ArtifactRuntime,
    declares_filesystem_scope: bool,
) -> bool {
    *runtime == murmur_artifact::ArtifactRuntime::Tool && !declares_filesystem_scope
}

/// The tool names, sorted, of the artifacts in `installed` whose guest is preopened at
/// [`RUNTIME_DIR_GUEST_PATH`]: the WASM ones [`gets_runtime_dir_preopen`] holds for. `scoped`
/// answers, by tool name, whether the operator's entry declares a `capabilities.filesystem.scope`.
/// A native tool is a host process and sees no preopen, whatever its runtime.
pub(crate) fn runtime_dir_preopened_tools<'a>(
    installed: impl IntoIterator<Item = &'a crate::types::InstalledArtifactSummary>,
    scoped: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut names: Vec<String> = installed
        .into_iter()
        .filter(|artifact| {
            artifact.implementation == Some(murmur_artifact::ArtifactImplementation::Wasm)
        })
        .map(|artifact| {
            (
                murmur_artifact::tool_name_of(&artifact.name),
                &artifact.runtime,
            )
        })
        .filter(|(name, runtime)| gets_runtime_dir_preopen(runtime, scoped(name)))
        .map(|(name, _)| name.to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `~/.murmur/sessions`, relative to the murmur home.
pub const SESSIONS_DIR_NAME: &str = "sessions";

/// The directory beneath `<session-id>/` that `$MURMUR_RUNTIME_DIR` names.
pub const RUNTIME_SUBDIR: &str = "runtime";

/// The directory beneath `<session-id>/` holding the composed root's synthetic `/etc` files.
pub const SEALED_ETC_SUBDIR: &str = "sealed-etc";

/// The directory beneath `<session-id>/` holding one `<run-id>/` per `transport: process`
/// harness run.
pub const HARNESS_SUBDIR: &str = "harness";

/// The lock file in `<session-id>/`, `flock`ed by the live session.
pub const LOCK_FILE_NAME: &str = ".lock";

/// How old a session directory nobody holds must be before staging removes it: long enough to
/// cover the instant between a directory's creation and its lock.
pub const STALE_AFTER: Duration = Duration::from_secs(60);

/// The mode every directory beneath `~/.murmur/sessions` is created at.
const PRIVATE_DIR_MODE: u32 = crate::murmur_home::MURMUR_HOME_DIR_MODE;

/// Where the runtime directory and the records directory lie relative to a workdir, decided
/// without creating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDirPlacement {
    /// `~/.murmur/sessions`, as `HOME` names it.
    pub sessions_dir: PathBuf,
    /// The records group of [`Self::workdir`], [`crate::records_dir::records_group`]'s answer:
    /// every session of this workdir root keeps its records directory in it.
    pub records_group: PathBuf,
    /// The workdir root it was compared with: `--workdir`, else `<manifest-dir>/workdir`.
    pub workdir: PathBuf,
    /// Whether `sessions_dir` lies inside `workdir`, both resolved through their nearest
    /// existing ancestor. When it does, the capsule's commands can change the runtime directory.
    pub sessions_inside_workdir: bool,
    /// Whether this session's records directory lies inside `workdir`, resolved the same way.
    pub records_inside_workdir: bool,
    /// Either of the two: the capsule's commands can change a directory the runtime keeps out of
    /// their reach.
    pub inside_workdir: bool,
}

impl RuntimeDirPlacement {
    /// `Some(self)` when the runtime directory or the records directory lies inside the workdir.
    #[must_use]
    pub fn inside(&self) -> Option<&Self> {
        self.inside_workdir.then_some(self)
    }

    /// A runtime directory and a records group under `/home/test` and a workdir under `/work`,
    /// apart, for a test that is not about where the directories lie.
    #[cfg(test)]
    pub(crate) fn outside_for_test() -> Self {
        Self {
            sessions_dir: PathBuf::from("/home/test/.murmur/sessions"),
            records_group: PathBuf::from("/home/test/.murmur/records/work"),
            workdir: PathBuf::from("/work"),
            sessions_inside_workdir: false,
            records_inside_workdir: false,
            inside_workdir: false,
        }
    }

    /// The runtime directory's path with the literal `<session-id>` segment, the form
    /// `--explain-scope` and `session_start.effective_grants` both print.
    #[must_use]
    pub fn report_path(&self) -> String {
        self.sessions_dir
            .join(crate::workdir_writes::SESSION_ID_PLACEHOLDER)
            .join(RUNTIME_SUBDIR)
            .display()
            .to_string()
    }

    /// The records directory's path with the literal `<session-id>` segment, on
    /// [`Self::report_path`]'s terms.
    #[must_use]
    pub fn records_report_path(&self) -> String {
        self.records_group
            .join(crate::workdir_writes::SESSION_ID_PLACEHOLDER)
            .display()
            .to_string()
    }

    /// The directory, or both, that lies inside the workdir, and what the capsule's commands can
    /// therefore change, as `(subject with its verb, what can be changed)`.
    fn inside_subject(&self) -> (String, &'static str) {
        let runtime = format!("the runtime directory {}", self.sessions_dir.display());
        let records = format!("the records directory {}", self.records_report_path());
        match (self.sessions_inside_workdir, self.records_inside_workdir) {
            (true, true) => (
                format!("{runtime} and {records} lie"),
                "the files the runtime owns — task.md, MURMUR.md, plans and installed tools — \
                 and the records it keeps about the session — its trace, logs, eval scores and \
                 delegation outcome",
            ),
            (true, false) => (
                format!("{runtime} lies"),
                "the files the runtime owns — task.md, MURMUR.md, plans and installed tools",
            ),
            (false, true) => (
                format!("{records} lies"),
                "the records the runtime keeps about the session — its trace, logs, eval scores \
                 and delegation outcome",
            ),
            (false, false) => (String::new(), ""),
        }
    }

    /// `E-CAP-003`'s reason for a placement inside the workdir.
    #[must_use]
    pub(crate) fn inside_shortfall(&self) -> String {
        let (subject, changes) = self.inside_subject();
        format!(
            "{subject} inside the workdir {}, so this capsule's commands can change {changes}, \
             and no host can back a class above advisory for it; run with a --workdir that does \
             not contain ~/.murmur",
            self.workdir.display()
        )
    }
}

/// The `W-SEC-033` line for a runtime or records directory inside the workdir, without a prefix:
/// shared by `stage_session` and `mur doctor` so the two cannot word it differently.
#[must_use]
pub fn runtime_dir_inside_warning(placement: &RuntimeDirPlacement) -> String {
    use murmur_artifact::{security_warning_link, W_SEC_033};
    let (subject, changes) = placement.inside_subject();
    format!(
        "warning[{W_SEC_033}]: {subject} inside the workdir {}, so this capsule's commands can \
         change {changes}; run with a --workdir that does not contain ~/.murmur ({})",
        placement.workdir.display(),
        security_warning_link(W_SEC_033)
    )
}

/// The `sessions/` directory every runtime directory on this host is created in,
/// `~/.murmur/sessions`, decided without creating anything. A `HOME` that cannot hold it is
/// refused, never replaced by another location.
pub fn sessions_dir() -> Result<PathBuf, crate::murmur_home::HomeUnusable> {
    Ok(crate::murmur_home::usable_murmur_home()?.join(SESSIONS_DIR_NAME))
}

/// Where the runtime directory and the records directory would lie relative to `workdir`, the
/// session's workdir root. Creates nothing.
///
/// Each path is resolved by canonicalizing its nearest existing ancestor and appending the rest,
/// so a workdir that does not exist yet, or a home with no `.murmur` in it yet, is judged by
/// where it would be created.
pub fn runtime_dir_placement(
    workdir: &Path,
) -> Result<RuntimeDirPlacement, crate::murmur_home::HomeUnusable> {
    let workdir = std::path::absolute(workdir).unwrap_or_else(|_| workdir.to_path_buf());
    let sessions_dir = sessions_dir()?;
    let records_group = crate::records_dir::records_group(&workdir)?;
    let canonical_workdir = canonicalize_existing_prefix(&workdir);
    let sessions_inside_workdir =
        canonicalize_existing_prefix(&sessions_dir).starts_with(&canonical_workdir);
    let records_inside_workdir =
        canonicalize_existing_prefix(&records_group).starts_with(&canonical_workdir);
    Ok(RuntimeDirPlacement {
        sessions_dir,
        records_group,
        workdir,
        sessions_inside_workdir,
        records_inside_workdir,
        inside_workdir: sessions_inside_workdir || records_inside_workdir,
    })
}

/// The three held directories beneath one session's `<session-id>/`, and the session's records
/// directory beside them, each sharing the session's refusal latch: every directory outside the
/// workdir a command is told of or a sealed root is composed from. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RuntimeDirRoots {
    /// `runtime/`, which `$MURMUR_RUNTIME_DIR` names.
    pub runtime: WorkdirRoot,
    /// `sealed-etc/`.
    pub sealed_etc: WorkdirRoot,
    /// `harness/`.
    pub harness: WorkdirRoot,
    /// The session's records directory, which `$MURMUR_RECORDS_DIR` names, given the same
    /// read-only rule and bind as `runtime/`. `None` for roots held outside a session, where no
    /// records directory exists.
    pub records: Option<WorkdirRoot>,
    /// The session's temporary folder, `tmp/` in its records directory: `$TMPDIR` in every
    /// command, and `/tmp` wherever the session maps it ([`crate::temp_dir`]). `None` for roots
    /// held outside a session, whose commands are told of no temporary folder.
    pub temp: Option<WorkdirRoot>,
}

impl RuntimeDirRoots {
    /// The three roots of a session's runtime directory, for a caller that holds them itself —
    /// a test, or a plan run outside a session. No records directory.
    #[must_use]
    pub fn new(runtime: WorkdirRoot, sealed_etc: WorkdirRoot, harness: WorkdirRoot) -> Self {
        Self {
            runtime,
            sealed_etc,
            harness,
            records: None,
            temp: None,
        }
    }

    /// These roots with `records` as the session's records directory.
    #[must_use]
    pub fn with_records(mut self, records: WorkdirRoot) -> Self {
        self.records = Some(records);
        self
    }

    /// These roots with `temp` as the session's temporary folder.
    #[must_use]
    pub fn with_temp(mut self, temp: WorkdirRoot) -> Self {
        self.temp = Some(temp);
        self
    }
}

/// One session's runtime directory, from its creation at staging to its removal when the value
/// is dropped.
pub struct RuntimeDir {
    session_id: String,
    /// `~/.murmur/sessions`, held; the session directory is removed through it.
    sessions: WorkdirRoot,
    /// `<session-id>/.lock`, `flock`ed for as long as this value lives.
    _lock: OwnedFd,
    roots: RuntimeDirRoots,
}

impl std::fmt::Debug for RuntimeDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeDir")
            .field("path", &self.roots.runtime.path())
            .finish()
    }
}

impl RuntimeDir {
    /// Creates `<sessions_path>/<session_id>/`, locks it, sweeps stale siblings and creates
    /// `runtime/`, `sealed-etc/` and `harness/` beneath it at `0700`. Every refusal beneath the
    /// three lands on `latch_of`'s latch and names its path absolutely.
    ///
    /// `sessions_path` is [`sessions_dir`]'s answer, the one the launch was judged against. The
    /// murmur home is held through [`crate::murmur_home::ensure_murmur_home`] first.
    pub fn create(
        sessions_path: &Path,
        session_id: &str,
        latch_of: &WorkdirRoot,
    ) -> Result<Self, String> {
        let located = |path: &Path, reason: String| format!("{}: {reason}", path.display());
        let home = crate::murmur_home::ensure_murmur_home()?;
        if sessions_path.parent() != Some(home.as_path()) {
            return Err(located(
                sessions_path,
                format!("is not beneath {}", home.display()),
            ));
        }
        let sessions_path = sessions_path.to_path_buf();
        crate::murmur_home::hold_private_dir(&sessions_path)
            .map_err(|reason| located(&sessions_path, reason))?;
        let sessions = WorkdirRoot::open(&sessions_path).map_err(|error| error.to_string())?;
        sweep_stale(&sessions, SystemTime::now());

        let session_path = sessions_path.join(session_id);
        crate::murmur_home::hold_private_dir(&session_path)
            .map_err(|reason| located(&session_path, reason))?;
        let session_dir_root = sessions
            .open_beneath(session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| located(&session_path, "vanished after its creation".to_string()))?;
        let lock = take_lock(&session_dir_root, &session_path)?;
        for sub in [RUNTIME_SUBDIR, SEALED_ETC_SUBDIR, HARNESS_SUBDIR] {
            session_dir_root
                .create_dir_all(sub, PRIVATE_DIR_MODE)
                .map_err(|error| error.to_string())?;
        }
        let open = |sub: &str| {
            WorkdirRoot::open_sharing_latch_absolute(&session_path.join(sub), latch_of)
                .map_err(|error| error.to_string())
        };
        Ok(Self {
            session_id: session_id.to_string(),
            sessions,
            _lock: lock,
            roots: RuntimeDirRoots::new(
                open(RUNTIME_SUBDIR)?,
                open(SEALED_ETC_SUBDIR)?,
                open(HARNESS_SUBDIR)?,
            ),
        })
    }

    /// `runtime/`'s absolute path, the value of `$MURMUR_RUNTIME_DIR`.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.roots.runtime.path()
    }

    /// The three held directories.
    #[must_use]
    pub fn roots(&self) -> &RuntimeDirRoots {
        &self.roots
    }
}

impl Drop for RuntimeDir {
    /// Removes `<session-id>/` by descriptor from the held `sessions/`, before the lock is
    /// released, so no sweep can race the removal.
    fn drop(&mut self) {
        if let Err(error) = self.sessions.remove_dir_all(&self.session_id) {
            if !error.is_not_found() {
                crate::runtime_err!(
                    "[capsule-runtime] warning: failed to remove the runtime directory {}: {error}",
                    self.sessions.host_path(&self.session_id).display()
                );
            }
        }
    }
}

/// The session's [`RuntimeDir`], shared between `launch` and the session's `ShellEnforcement`
/// so the process exits that run no destructor can still remove it. Cheap to clone; every clone
/// removes the one directory.
#[derive(Clone, Default)]
pub(crate) struct SharedRuntimeDir(Arc<Mutex<Option<RuntimeDir>>>);

impl std::fmt::Debug for SharedRuntimeDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedRuntimeDir")
    }
}

impl SharedRuntimeDir {
    pub(crate) fn new(dir: Option<RuntimeDir>) -> Self {
        Self(Arc::new(Mutex::new(dir)))
    }

    /// Removes the runtime directory now. Later calls do nothing.
    pub(crate) fn remove(&self) {
        let taken = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(taken);
    }

    /// A guard that calls [`Self::remove`] when dropped, whatever other clones still exist.
    pub(crate) fn remove_on_drop(&self) -> RemoveRuntimeDirOnDrop {
        RemoveRuntimeDirOnDrop(self.clone())
    }
}

/// Removes a [`SharedRuntimeDir`] when dropped.
pub(crate) struct RemoveRuntimeDirOnDrop(SharedRuntimeDir);

impl Drop for RemoveRuntimeDirOnDrop {
    fn drop(&mut self) {
        self.0.remove();
    }
}

/// Opens (creating it at `0600`) and exclusively locks `<session>/.lock`, following no link.
fn take_lock(session_dir_root: &WorkdirRoot, session_path: &Path) -> Result<OwnedFd, String> {
    let lock_path = session_path.join(LOCK_FILE_NAME);
    let dir = session_dir_root
        .dir_fd()
        .map_err(|error| format!("{}: {error}", session_path.display()))?;
    let fd = rustix::fs::openat(
        &dir,
        LOCK_FILE_NAME,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(crate::murmur_home::PRIVATE_FILE_MODE as rustix::fs::RawMode),
    )
    .map_err(|errno| format!("{}: {}", lock_path.display(), std::io::Error::from(errno)))?;
    rustix::fs::flock(&fd, FlockOperation::NonBlockingLockExclusive)
        .map_err(|errno| format!("{}: {}", lock_path.display(), std::io::Error::from(errno)))?;
    Ok(fd)
}

/// Removes every entry of `sessions` no live session holds and older than [`STALE_AFTER`]: a
/// directory whose `.lock` can be taken and was last modified before then, and any entry with no
/// `.lock` last modified before then. A live session's lock is never free, so its directory is
/// never removed.
fn sweep_stale(sessions: &WorkdirRoot, now: SystemTime) {
    let Ok(Some(entries)) = sessions.list_dir("") else {
        return;
    };
    let Ok(dir) = sessions.dir_fd() else {
        return;
    };
    let older_than_floor = |stat: &rustix::fs::Stat| {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime.max(0) as u64);
        now.duration_since(modified)
            .is_ok_and(|age| age > STALE_AFTER)
    };
    for (name, _) in entries {
        let Ok(entry_stat) = rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
        else {
            continue;
        };
        if FileType::from_raw_mode(entry_stat.st_mode) != FileType::Directory {
            if older_than_floor(&entry_stat) {
                let _ = sessions.remove_file(&name);
            }
            continue;
        }
        let lock = rustix::fs::openat(&dir, name.as_str(), DIR_NOFOLLOW, Mode::empty()).and_then(
            |session| {
                rustix::fs::openat(
                    &session,
                    LOCK_FILE_NAME,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                )
            },
        );
        match lock {
            Ok(lock) => {
                let free = rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive);
                let stale = rustix::fs::fstat(&lock).is_ok_and(|stat| older_than_floor(&stat));
                if free.is_ok() && stale {
                    let _ = sessions.remove_dir_all(&name);
                }
            }
            Err(rustix::io::Errno::NOENT) if older_than_floor(&entry_stat) => {
                let _ = sessions.remove_dir_all(&name);
            }
            Err(_) => {}
        }
    }
}

const DIR_NOFOLLOW: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn set_mtime(path: &Path, age: Duration) {
        let file = std::fs::File::open(path).unwrap();
        file.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn runtime_dir_sweep_removes_unlocked_stale_and_keeps_locked_and_recent() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path();
        let make = |name: &str, age: Duration| {
            let dir = sessions.join(name);
            std::fs::create_dir_all(dir.join("runtime")).unwrap();
            std::fs::write(dir.join(LOCK_FILE_NAME), b"").unwrap();
            set_mtime(&dir.join(LOCK_FILE_NAME), age);
            dir
        };
        let stale = make("ses_stale", Duration::from_secs(61));
        let locked = make("ses_locked", Duration::from_secs(61));
        let recent = make("ses_recent", Duration::from_secs(10));
        let unlocked_dir = sessions.join("ses_nolock");
        std::fs::create_dir_all(&unlocked_dir).unwrap();
        set_mtime(&unlocked_dir, Duration::from_secs(61));

        let held = std::fs::File::open(locked.join(LOCK_FILE_NAME)).unwrap();
        rustix::fs::flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();

        let root = WorkdirRoot::open(sessions).unwrap();
        sweep_stale(&root, SystemTime::now());

        assert!(
            !stale.exists(),
            "an unlocked 61-second-old directory is removed"
        );
        assert!(
            !unlocked_dir.exists(),
            "an old directory with no lock is removed"
        );
        assert!(locked.exists(), "a locked directory is kept");
        assert!(recent.exists(), "a 10-second-old directory is kept");
        drop(held);
    }

    #[test]
    fn runtime_dir_placement_resolves_through_the_nearest_existing_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved = canonicalize_existing_prefix(&link.join("not-yet").join("deeper"));
        assert_eq!(
            resolved,
            real.canonicalize().unwrap().join("not-yet").join("deeper")
        );
        assert!(canonicalize_existing_prefix(&link.join(".murmur/sessions"))
            .starts_with(canonicalize_existing_prefix(&real)));
    }

    /// A workdir root that holds the murmur home holds both directories; one that holds
    /// `~/.murmur/records` alone holds the records directory alone; one elsewhere holds neither.
    #[test]
    fn runtime_dir_placement_judges_the_records_directory_too() {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            "runtime_dir::tests::runtime_dir_placement_records_inner",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by runtime_dir_placement_judges_the_records_directory_too"]
    fn runtime_dir_placement_records_inner() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let both = runtime_dir_placement(&home).unwrap();
        assert!(both.sessions_inside_workdir && both.records_inside_workdir);
        assert!(both.inside_workdir);
        assert!(runtime_dir_inside_warning(&both).contains("the records directory"));

        let records = home
            .join(".murmur")
            .join(crate::records_dir::RECORDS_DIR_NAME);
        let records_only = runtime_dir_placement(&records).unwrap();
        assert!(!records_only.sessions_inside_workdir);
        assert!(records_only.records_inside_workdir && records_only.inside_workdir);
        let reason = records_only.inside_shortfall();
        assert!(reason.starts_with("the records directory "), "{reason}");
        assert!(reason.contains("run with a --workdir that does not contain ~/.murmur"));

        let elsewhere = runtime_dir_placement(&home.join("project/workdir")).unwrap();
        assert!(!elsewhere.inside_workdir);
        assert_eq!(
            elsewhere.records_report_path(),
            crate::records_dir::records_group(&home.join("project/workdir"))
                .unwrap()
                .join("<session-id>")
                .display()
                .to_string()
        );
        assert!(!home.join(".murmur").exists(), "judging creates nothing");
    }

    #[test]
    fn runtime_dir_lock_is_exclusive_across_open_file_descriptions() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("ses_x");
        std::fs::create_dir_all(&session).unwrap();
        let root = WorkdirRoot::open(&session).unwrap();
        let held = take_lock(&root, &session).unwrap();
        let mode = std::fs::metadata(session.join(LOCK_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode & 0o077, 0, "the lock file is owner-only");
        assert!(
            take_lock(&root, &session).is_err(),
            "a second lock is refused"
        );
        drop(held);
        assert!(take_lock(&root, &session).is_ok());
    }
}
