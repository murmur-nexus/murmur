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
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, FileType, FlockOperation, Mode, OFlags};

use crate::workdir_fs::WorkdirRoot;

/// The variable naming the runtime directory in every shell command's, plan shell step's, native
/// tool's and `transport: process` harness's environment. Runtime-owned like `HOME`: no
/// manifest, override or host value can set it.
pub const RUNTIME_DIR_ENV: &str = "MURMUR_RUNTIME_DIR";

/// The guest path a `runtime: tool` with no `capabilities.filesystem.scope` finds the runtime
/// directory at, preopened read-only beside its `"."`.
pub const RUNTIME_DIR_GUEST_PATH: &str = "murmur-runtime";

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

/// Where the runtime directory lies relative to a workdir, decided without creating anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDirPlacement {
    /// `~/.murmur/sessions`, as `HOME` names it.
    pub sessions_dir: PathBuf,
    /// The accessible workdir it was compared with.
    pub workdir: PathBuf,
    /// Whether `sessions_dir` lies inside `workdir`, both resolved through their nearest
    /// existing ancestor. When it does, the capsule's commands can change the runtime directory.
    pub inside_workdir: bool,
}

impl RuntimeDirPlacement {
    /// `Some(self)` when the runtime directory lies inside the workdir.
    #[must_use]
    pub fn inside(&self) -> Option<&Self> {
        self.inside_workdir.then_some(self)
    }

    /// A runtime directory under `/home/test` and a workdir under `/work`, apart, for a test
    /// that is not about where the directory lies.
    #[cfg(test)]
    pub(crate) fn outside_for_test() -> Self {
        Self {
            sessions_dir: PathBuf::from("/home/test/.murmur/sessions"),
            workdir: PathBuf::from("/work"),
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
}

/// The `W-SEC-033` line for a runtime directory inside the workdir, without a prefix: shared by
/// `stage_session` and `mur doctor` so the two cannot word it differently.
#[must_use]
pub fn runtime_dir_inside_warning(placement: &RuntimeDirPlacement) -> String {
    use murmur_artifact::{security_warning_link, W_SEC_033};
    format!(
        "warning[{W_SEC_033}]: the runtime directory {} lies inside the workdir {}, so this \
         capsule's commands can change the files the runtime owns — task.md, MURMUR.md, plans \
         and installed tools; run with a --workdir that does not contain ~/.murmur ({})",
        placement.sessions_dir.display(),
        placement.workdir.display(),
        security_warning_link(W_SEC_033)
    )
}

/// The `sessions/` directory every runtime directory on this host is created in, decided without
/// creating anything: `~/.murmur/sessions` when `HOME` resolves to an absolute path and
/// `~/.murmur` can be written, and `<temp dir>/murmur-<uid>/sessions` otherwise, so a home closed
/// to writes or a relative `HOME` costs a session nothing but the location.
#[must_use]
pub fn sessions_dir() -> PathBuf {
    match crate::state_store::murmur_home_dir() {
        Ok(home) if home_is_writable(&home) => home.join(SESSIONS_DIR_NAME),
        _ => fallback_parent().join(SESSIONS_DIR_NAME),
    }
}

/// Whether the deepest existing one of `~/.murmur/sessions`, `~/.murmur` and `$HOME` is a
/// directory this process may create entries in.
fn home_is_writable(murmur_home: &Path) -> bool {
    let candidates = [
        murmur_home.join(SESSIONS_DIR_NAME),
        murmur_home.to_path_buf(),
        murmur_home
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default(),
    ];
    let Some(existing) = candidates
        .iter()
        .find(|path| std::fs::symlink_metadata(path).is_ok())
    else {
        return false;
    };
    std::fs::metadata(existing).is_ok_and(|metadata| metadata.is_dir())
        && rustix::fs::access(
            existing,
            rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
        )
        .is_ok()
}

/// `<temp dir>/murmur-<uid>`, the per-user parent of `sessions/` when the home cannot hold it.
fn fallback_parent() -> PathBuf {
    std::env::temp_dir().join(format!("murmur-{}", rustix::process::getuid().as_raw()))
}

/// Creates `parent` at `0700` if absent, and holds it there only when it is a real directory this
/// user owns: a shared temp directory may hold a name another user created first.
fn hold_owned_private_dir(parent: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let located = |reason: &str| format!("{}: {reason}", parent.display());
    if std::fs::symlink_metadata(parent).is_err() {
        crate::murmur_home::hold_private_dir(parent).map_err(|reason| located(&reason))?;
    }
    let metadata =
        std::fs::symlink_metadata(parent).map_err(|error| located(&error.to_string()))?;
    if !metadata.file_type().is_dir() {
        return Err(located("not a directory"));
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(located("owned by another user"));
    }
    crate::murmur_home::hold_private_dir(parent).map_err(|reason| located(&reason))
}

/// Where the runtime directory would lie relative to `workdir`. Creates nothing.
///
/// Each path is resolved by canonicalizing its nearest existing ancestor and appending the rest,
/// so a workdir that does not exist yet, or a home with no `.murmur` in it yet, is judged by
/// where it would be created.
#[must_use]
pub fn runtime_dir_placement(workdir: &Path) -> RuntimeDirPlacement {
    let workdir = absolute(workdir);
    let sessions_dir = sessions_dir();
    let inside_workdir =
        resolve_existing_prefix(&sessions_dir).starts_with(resolve_existing_prefix(&workdir));
    RuntimeDirPlacement {
        sessions_dir,
        workdir,
        inside_workdir,
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// `path` with its nearest existing ancestor canonicalized and the components below it appended
/// lexically.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let path = absolute(path);
    let mut rest = Vec::new();
    let mut current = path.as_path();
    loop {
        if let Ok(resolved) = current.canonicalize() {
            let mut joined = resolved;
            for component in rest.iter().rev() {
                joined.push(component);
            }
            return joined;
        }
        match (current.parent(), current.components().next_back()) {
            (Some(parent), Some(Component::Normal(name))) => {
                rest.push(name.to_os_string());
                current = parent;
            }
            _ => return path,
        }
    }
}

/// The three held directories beneath one session's `<session-id>/`, each sharing the session's
/// refusal latch. Cheap to clone.
#[derive(Clone, Debug)]
pub struct RuntimeDirRoots {
    /// `runtime/`, which `$MURMUR_RUNTIME_DIR` names.
    pub runtime: WorkdirRoot,
    /// `sealed-etc/`.
    pub sealed_etc: WorkdirRoot,
    /// `harness/`.
    pub harness: WorkdirRoot,
}

impl RuntimeDirRoots {
    /// The three roots of a session's runtime directory, for a caller that holds them itself —
    /// a test, or a plan run outside a session.
    #[must_use]
    pub fn new(runtime: WorkdirRoot, sealed_etc: WorkdirRoot, harness: WorkdirRoot) -> Self {
        Self {
            runtime,
            sealed_etc,
            harness,
        }
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
    /// `sessions_path` is [`sessions_dir`]'s answer, the one the launch was judged against. Under
    /// the murmur home, the home is held through [`crate::murmur_home::ensure_murmur_home`] first.
    pub fn create(
        sessions_path: &Path,
        session_id: &str,
        latch_of: &WorkdirRoot,
    ) -> Result<Self, String> {
        let located = |path: &Path, reason: String| format!("{}: {reason}", path.display());
        let parent = sessions_path
            .parent()
            .ok_or_else(|| located(sessions_path, "has no parent directory".to_string()))?;
        let home = crate::state_store::murmur_home_dir().ok();
        if home.as_deref() == Some(parent) {
            crate::murmur_home::ensure_murmur_home()?;
        } else {
            hold_owned_private_dir(parent)?;
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
            roots: RuntimeDirRoots {
                runtime: open(RUNTIME_SUBDIR)?,
                sealed_etc: open(SEALED_ETC_SUBDIR)?,
                harness: open(HARNESS_SUBDIR)?,
            },
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

        let resolved = resolve_existing_prefix(&link.join("not-yet").join("deeper"));
        assert_eq!(
            resolved,
            real.canonicalize().unwrap().join("not-yet").join("deeper")
        );
        assert!(resolve_existing_prefix(&link.join(".murmur/sessions"))
            .starts_with(resolve_existing_prefix(&real)));
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

    /// A home that resolves and can be written holds `sessions/`; a relative `HOME` and a
    /// `~/.murmur` closed to writes fall back to the per-user temp directory.
    #[test]
    fn runtime_dir_sessions_fall_back_when_the_home_cannot_hold_them() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        assert_eq!(
            sessions_dir(),
            home.path().join(".murmur").join(SESSIONS_DIR_NAME)
        );

        let murmur = home.path().join(".murmur");
        std::fs::create_dir(&murmur).unwrap();
        std::fs::set_permissions(&murmur, std::fs::Permissions::from_mode(0o500)).unwrap();
        let closed = std::fs::write(murmur.join("probe"), b"x").is_err();
        if closed {
            assert_eq!(sessions_dir(), fallback_parent().join(SESSIONS_DIR_NAME));
        }
        std::fs::set_permissions(&murmur, std::fs::Permissions::from_mode(0o700)).unwrap();

        std::env::set_var("HOME", "relative-home");
        assert_eq!(sessions_dir(), fallback_parent().join(SESSIONS_DIR_NAME));
        assert!(fallback_parent().starts_with(std::env::temp_dir()));
    }

    /// The fallback parent is held only when it is a real directory this user owns.
    #[test]
    fn runtime_dir_fallback_parent_refuses_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let link = temp.path().join("murmur-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(hold_owned_private_dir(&link)
            .unwrap_err()
            .contains("not a directory"));

        let fresh = temp.path().join("murmur-fresh");
        hold_owned_private_dir(&fresh).unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
