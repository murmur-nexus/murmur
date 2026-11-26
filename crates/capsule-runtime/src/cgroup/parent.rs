//! Adopting a delegated parent cgroup without asking systemd.
//!
//! When `mur` already sits in a cgroup whose parent is a delegated subtree its own uid owns — a
//! system unit with `Delegate=yes` and `User=` whose launcher moved itself into a leaf, or a
//! cgroup namespace rooted at a delegated cgroup — the parent is where the runtime's cgroups can
//! go directly. [`adopt`] creates `<parent>/murmur-<pid>-p`, moves this process and its live
//! companions into it, and from there [`super::probe_delegation`] and
//! [`super::enable_controllers`] find it as the delegated base exactly as they find a systemd
//! scope. No D-Bus connection is opened.
//!
//! [`decide`] is the whole fail-closed decision, over [`ParentFacts`] read by
//! [`read_parent_facts`]: every check must pass, and the first that fails is the decline reason.
//! Nothing a capsule can write takes part in it. Ownership alone is not enough: under a systemd
//! user manager every cgroup below `user@<uid>.service` is owned by the user, though the user
//! manager never delegated it, so the parent must also be where delegation to this uid begins
//! (check 6).
//!
//! What a launch leaves behind is removed three ways: the exit hook [`adopt`] registers, for the
//! process's own `murmur-<pid>-p`; [`sweep_stale`] at the next adoption under the same parent,
//! for the directory of a launch that died without running the hook; and the unit's stop, for
//! whatever is left when the unit ends.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The controllers the parent must enable for its children and this process's cgroup must list,
/// in the order a decline names the missing ones.
const PARENT_CONTROLLERS: &[&str] = &["memory", "pids", "cpu"];

/// How long the exit hook waits for its killed `murmur-<pid>-p` to report `populated 0`.
const RELEASE_DEADLINE: Duration = Duration::from_secs(2);

/// What [`decide`] reads about this process's cgroup, its parent and its grandparent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ParentFacts {
    /// The `0::` line's path in `/proc/self/cgroup`, relative to the visible hierarchy's root
    /// (`/launcher`); `None` without a cgroup v2 unified hierarchy.
    pub unified_path: Option<String>,
    /// Where the cgroup2 hierarchy is mounted.
    pub mount: PathBuf,
    /// This process's effective uid.
    pub euid: u32,
    /// Whether the parent has a `cgroup.type` file, which every cgroup but the hierarchy's true
    /// root has.
    pub parent_has_cgroup_type: bool,
    /// The uid owning the parent directory; `None` when it could not be read.
    pub parent_owner: Option<u32>,
    /// The uid owning the parent's own parent; `None` when it could not be read, or when the
    /// parent is the root of the visible hierarchy and has none.
    pub grandparent_owner: Option<u32>,
    /// The parent's `cgroup.subtree_control`; empty when it could not be read.
    pub parent_subtree_control: String,
    /// This process's own `cgroup.controllers`; empty when it could not be read.
    pub own_controllers: String,
}

/// Whether a delegated parent was adopted. Decided at most once per process, by
/// [`super::delegated_base`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ParentAdoption {
    /// This process (and its live companions) moved into this `<parent>/murmur-<pid>-p`; the exit
    /// hook is registered.
    Adopted(PathBuf),
    /// No parent was adopted, for this reason; the process is where it was.
    Declined(String),
}

/// The relative path of `unified_path`'s parent, `None` for the visible root itself.
fn parent_of(unified_path: &str) -> Option<&str> {
    let trimmed = unified_path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(match trimmed.rfind('/') {
        Some(0) | None => "/",
        Some(index) => &trimmed[..index],
    })
}

/// `relative` (a path from `/proc/self/cgroup`) under `mount`.
fn under_mount(mount: &Path, relative: &str) -> PathBuf {
    mount.join(relative.trim_start_matches('/'))
}

/// Whether `name` is a systemd user manager's own cgroup: `user@<digits>.service`.
fn is_user_manager(name: &str) -> bool {
    name.strip_prefix("user@")
        .and_then(|rest| rest.strip_suffix(".service"))
        .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The controllers a parent must provide: memory, pids and cpu, plus io when the capsule declares
/// `capabilities.resources.cgroup_io_bytes_per_sec`.
fn required_controllers(declares_io_limit: bool) -> Vec<&'static str> {
    let mut wanted = PARENT_CONTROLLERS.to_vec();
    if declares_io_limit {
        wanted.push("io");
    }
    wanted
}

/// The parent directory to adopt, or the reason it is declined: the first of the eight checks
/// that fails, in order.
///
/// | # | Check | Decline reason |
/// |---|---|---|
/// | 1 | a `0::` line | `no cgroup v2 unified hierarchy` |
/// | 2 | this process's cgroup is not the visible root | `this process's cgroup has no parent` |
/// | 3 | the parent has a `cgroup.type` file | `parent is the root cgroup` |
/// | 4 | the parent is owned by the euid | `parent not owned by this uid` |
/// | 5 | the parent is not `user@<n>.service` | `parent is a systemd user manager's own cgroup` |
/// | 6 | the parent is the visible root, or its parent is not owned by the euid | `parent is not a delegation boundary: its parent is also owned by this uid` |
/// | 7 | the parent's `cgroup.subtree_control` lists the controllers | `parent does not enable <missing>` |
/// | 8 | this process's `cgroup.controllers` lists them | `this process's cgroup does not list <missing>` |
///
/// The controllers are memory, pids and cpu, and io when `declares_io_limit`; the missing ones are
/// comma-joined in that order. An owner that could not be read fails its check.
pub(super) fn decide(facts: &ParentFacts, declares_io_limit: bool) -> Result<PathBuf, String> {
    let Some(unified_path) = facts.unified_path.as_deref() else {
        return Err("no cgroup v2 unified hierarchy".to_string());
    };
    let Some(parent_relative) = parent_of(unified_path) else {
        return Err("this process's cgroup has no parent".to_string());
    };
    if !facts.parent_has_cgroup_type {
        return Err("parent is the root cgroup".to_string());
    }
    if facts.parent_owner != Some(facts.euid) {
        return Err("parent not owned by this uid".to_string());
    }
    let parent = under_mount(&facts.mount, parent_relative);
    if parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_user_manager)
    {
        return Err("parent is a systemd user manager's own cgroup".to_string());
    }
    let parent_is_visible_root = parent_relative == "/";
    if !parent_is_visible_root && facts.grandparent_owner.is_none_or(|uid| uid == facts.euid) {
        return Err(
            "parent is not a delegation boundary: its parent is also owned by this uid".to_string(),
        );
    }
    let wanted = required_controllers(declares_io_limit);
    let not_enabled = super::missing_controllers(&facts.parent_subtree_control, &wanted);
    if !not_enabled.is_empty() {
        return Err(format!("parent does not enable {}", not_enabled.join(",")));
    }
    let not_listed = super::missing_controllers(&facts.own_controllers, &wanted);
    if !not_listed.is_empty() {
        return Err(format!(
            "this process's cgroup does not list {}",
            not_listed.join(",")
        ));
    }
    Ok(parent)
}

/// The uid owning `path`, when it is a directory whose metadata can be read.
fn directory_owner(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|metadata| metadata.uid())
}

/// The facts [`decide`] needs, read from `/proc/self/cgroup`, `/proc/mounts` and the cgroup
/// directories. Needs no privilege; whatever cannot be read is recorded as absent, which fails
/// the check that needs it.
pub(super) fn read_parent_facts() -> ParentFacts {
    let unified_path = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|contents| super::parse_unified_cgroup_path(&contents));
    let mount = PathBuf::from(
        std::fs::read_to_string("/proc/mounts")
            .map(|contents| super::parse_cgroup2_mount(&contents))
            .unwrap_or_else(|_| super::DEFAULT_CGROUP2_MOUNT.to_string()),
    );
    let mut facts = ParentFacts {
        unified_path: unified_path.clone(),
        mount,
        euid: crate::murmur_home::current_euid(),
        parent_has_cgroup_type: false,
        parent_owner: None,
        grandparent_owner: None,
        parent_subtree_control: String::new(),
        own_controllers: String::new(),
    };
    let Some(unified_path) = unified_path else {
        return facts;
    };
    let Some(parent_relative) = parent_of(&unified_path) else {
        return facts;
    };
    let parent = under_mount(&facts.mount, parent_relative);
    facts.parent_has_cgroup_type = std::fs::symlink_metadata(parent.join("cgroup.type")).is_ok();
    facts.parent_owner = directory_owner(&parent);
    facts.grandparent_owner =
        parent_of(parent_relative).and_then(|up| directory_owner(&under_mount(&facts.mount, up)));
    facts.parent_subtree_control =
        std::fs::read_to_string(parent.join("cgroup.subtree_control")).unwrap_or_default();
    facts.own_controllers = std::fs::read_to_string(
        under_mount(&facts.mount, &unified_path).join("cgroup.controllers"),
    )
    .unwrap_or_default();
    facts
}

/// The name of the cgroup a launch with `pid` creates under its parent.
fn own_cgroup_name(pid: u32) -> String {
    format!("murmur-{pid}-p")
}

/// The pid in a `murmur-<digits>-p` name, `None` for any other name.
fn launch_pid(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("murmur-")?.strip_suffix("-p")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `rmdir` of `path`'s subdirectories, depth first, then of `path` itself. Returns the error of
/// the last `rmdir`, `Ok` when `path` is gone. Never follows a symlink and never removes a file:
/// on cgroupfs the control files go with their directory, and anywhere else a file keeps the
/// directory it is in.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let _ = remove_tree(&entry.path());
            }
        }
    }
    match std::fs::remove_dir(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// Removes, with `rmdir` only, every `murmur-<digits>-p` directly under `parent` whose pid has no
/// `/proc/<pid>` entry, and the one named for `own_pid`, which is necessarily stale.
///
/// A directory named for a live pid is skipped, so a launch that has created its directory but not
/// yet moved in is never touched. A populated cgroup refuses `rmdir` with `EBUSY` and is left: the
/// sweep writes no control file, so it neither kills nor moves another launch's processes. Every
/// error is ignored, including the `ENOENT` and `EBUSY` of two launches sweeping at once.
pub(super) fn sweep_stale(parent: &Path, own_pid: u32) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(launch_pid) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if pid == own_pid || !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = remove_tree(&entry.path());
        }
    }
}

/// What the exit hook needs: which process adopted, the cgroup it came from and the one it made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExitRecord {
    /// The adopting process; the hook does nothing in any other.
    pid: u32,
    /// The cgroup the process left, which it moves back into before killing its own.
    origin: PathBuf,
    /// `<parent>/murmur-<pid>-p`.
    own: PathBuf,
}

/// The adoption the exit hook releases. Set once, by [`adopt`].
static EXIT_RECORD: OnceLock<ExitRecord> = OnceLock::new();

/// Sweeps `parent`, creates `<parent>/murmur-<own_pid>-p` and moves `own_pid`, then every live
/// one of `companions`, into it. Returns the created directory, or the decline reason after
/// removing what it created.
fn take_parent(parent: &Path, own_pid: u32, companions: &[u32]) -> Result<PathBuf, String> {
    sweep_stale(parent, own_pid);
    let own = parent.join(own_cgroup_name(own_pid));
    if let Err(error) = std::fs::create_dir(&own) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("could not create {}: {error}", own.display()));
        }
        // The sweep could not remove it; once more for the reason why.
        if let Err(error) = remove_tree(&own) {
            return Err(format!(
                "{} already exists and could not be removed: {error}",
                own.display()
            ));
        }
        std::fs::create_dir(&own)
            .map_err(|error| format!("could not create {}: {error}", own.display()))?;
    }
    if let Err(error) = super::write_existing(&own.join("cgroup.procs"), &format!("{own_pid}\n")) {
        let _ = std::fs::remove_dir(&own);
        return Err(format!("could not move into {}: {error}", own.display()));
    }
    for pid in companions {
        if Path::new(&format!("/proc/{pid}")).exists() {
            // A companion that exited meanwhile has nothing to move.
            let _ = super::write_existing(&own.join("cgroup.procs"), &format!("{pid}\n"));
        }
    }
    Ok(own)
}

/// Adopts the delegated parent `facts` describe, or declines with the reason why: [`decide`],
/// then the sweep, the `mkdir` of `murmur-<pid>-p` and the move of this process and the live
/// `companions` into it. On adoption the exit hook is registered.
pub(super) fn adopt(
    facts: &ParentFacts,
    declares_io_limit: bool,
    companions: &[u32],
) -> ParentAdoption {
    let parent = match decide(facts, declares_io_limit) {
        Ok(parent) => parent,
        Err(reason) => return ParentAdoption::Declined(reason),
    };
    let Some(unified_path) = facts.unified_path.as_deref() else {
        return ParentAdoption::Declined("no cgroup v2 unified hierarchy".to_string());
    };
    let pid = std::process::id();
    match take_parent(&parent, pid, companions) {
        Ok(own) => {
            let record = ExitRecord {
                pid,
                origin: under_mount(&facts.mount, unified_path),
                own: own.clone(),
            };
            if EXIT_RECORD.set(record).is_ok() {
                register_exit_hook();
            }
            ParentAdoption::Adopted(own)
        }
        Err(reason) => ParentAdoption::Declined(reason),
    }
}

/// Registers [`release_at_exit`] with `atexit`, so it runs on every `std::process::exit` and on
/// return from `main`. A failed registration leaves the directory to the next launch's sweep.
#[allow(unsafe_code)]
fn register_exit_hook() {
    // SAFETY: `atexit` stores a pointer to a function with the C ABI that takes no arguments and
    // returns nothing, which `release_at_exit` is; it lives for the whole program.
    let _ = unsafe { libc::atexit(release_at_exit) };
}

/// The exit hook: releases the adoption [`EXIT_RECORD`] holds, in the process that adopted only.
extern "C" fn release_at_exit() {
    if let Some(record) = EXIT_RECORD.get() {
        release(record, std::process::id());
    }
}

/// Moves `current_pid` back into the cgroup it came from, kills what is left in its
/// `murmur-<pid>-p`, waits up to [`RELEASE_DEADLINE`] for it to empty and removes it depth first.
///
/// Does nothing unless `current_pid` is the adopter, so a forked child that calls `exit` leaves
/// the adopter's cgroups alone. When the origin is gone or the move back fails nothing is killed,
/// since this process is still inside, and the directory is left for the next sweep.
fn release(record: &ExitRecord, current_pid: u32) {
    if current_pid != record.pid {
        return;
    }
    if super::write_existing(
        &record.origin.join("cgroup.procs"),
        &format!("{current_pid}\n"),
    )
    .is_err()
    {
        return;
    }
    match super::write_existing(&record.own.join("cgroup.kill"), "1") {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            kill_tree_by_freezing(&record.own);
        }
        _ => {}
    }
    let deadline = std::time::Instant::now() + RELEASE_DEADLINE;
    while super::read_event_counter(&record.own.join("cgroup.events"), "populated") != 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(super::KILL_POLL_INTERVAL);
    }
    let _ = remove_tree(&record.own);
}

/// `cgroup.kill` for a kernel that has none, over a whole tree: every cgroup in it, depth first,
/// frozen, its processes `SIGKILL`ed and thawed, as [`super::CgroupScope::kill_all`] does for one.
fn kill_tree_by_freezing(path: &Path) {
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                kill_tree_by_freezing(&entry.path());
            }
        }
    }
    let _ = super::kill_cgroup_by_freezing(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    const UID: u32 = 1000;

    /// The system-unit shape: `/system.slice/launch.service/launcher`, the unit owned by the uid,
    /// `system.slice` by root, every controller enabled and listed.
    fn system_unit() -> ParentFacts {
        ParentFacts {
            unified_path: Some("/system.slice/launch.service/launcher".to_string()),
            mount: PathBuf::from("/sys/fs/cgroup"),
            euid: UID,
            parent_has_cgroup_type: true,
            parent_owner: Some(UID),
            grandparent_owner: Some(0),
            parent_subtree_control: "cpu io memory pids".to_string(),
            own_controllers: "cpu io memory pids".to_string(),
        }
    }

    fn declined(facts: &ParentFacts, declares_io_limit: bool) -> String {
        decide(facts, declares_io_limit).expect_err("declined")
    }

    #[test]
    fn a_system_unit_with_delegate_and_user_is_adopted() {
        assert_eq!(
            decide(&system_unit(), true),
            Ok(PathBuf::from("/sys/fs/cgroup/system.slice/launch.service"))
        );
    }

    #[test]
    fn a_parent_at_the_root_of_the_visible_mount_is_adopted() {
        let facts = ParentFacts {
            unified_path: Some("/launcher".to_string()),
            grandparent_owner: None,
            ..system_unit()
        };
        assert_eq!(decide(&facts, false), Ok(PathBuf::from("/sys/fs/cgroup")));
    }

    #[test]
    fn no_unified_hierarchy_declines_first() {
        let facts = ParentFacts {
            unified_path: None,
            parent_owner: None,
            ..system_unit()
        };
        assert_eq!(declined(&facts, false), "no cgroup v2 unified hierarchy");
    }

    #[test]
    fn a_cgroup_at_the_visible_root_has_no_parent() {
        for path in ["/", ""] {
            let facts = ParentFacts {
                unified_path: Some(path.to_string()),
                ..system_unit()
            };
            assert_eq!(
                declined(&facts, false),
                "this process's cgroup has no parent",
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_true_root_cgroup_is_never_adopted_even_by_root() {
        let facts = ParentFacts {
            unified_path: Some("/launcher".to_string()),
            euid: 0,
            parent_has_cgroup_type: false,
            parent_owner: Some(0),
            grandparent_owner: None,
            ..system_unit()
        };
        assert_eq!(declined(&facts, false), "parent is the root cgroup");
    }

    #[test]
    fn a_parent_of_another_uid_or_unreadable_is_not_owned() {
        for owner in [Some(0), Some(UID + 1), None] {
            let facts = ParentFacts {
                parent_owner: owner,
                ..system_unit()
            };
            assert_eq!(
                declined(&facts, false),
                "parent not owned by this uid",
                "{owner:?}"
            );
        }
    }

    #[test]
    fn a_user_managers_own_cgroup_is_declined() {
        let facts = ParentFacts {
            unified_path: Some("/user.slice/user-1000.slice/user@1000.service/init.scope".into()),
            grandparent_owner: Some(0),
            ..system_unit()
        };
        assert_eq!(
            declined(&facts, false),
            "parent is a systemd user manager's own cgroup"
        );
        assert!(!is_user_manager("user@.service"));
        assert!(!is_user_manager("user@10x.service"));
        assert!(is_user_manager("user@0.service"));
    }

    const NOT_A_BOUNDARY: &str =
        "parent is not a delegation boundary: its parent is also owned by this uid";

    /// A desktop shape: a terminal's scope in `app.slice`, which the user owns as it owns
    /// `user@1000.service` above it, every controller enabled.
    #[test]
    fn a_desktop_scope_under_a_user_manager_is_not_a_delegation_boundary() {
        let facts = ParentFacts {
            unified_path: Some(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/tmux-spawn-1.scope"
                    .to_string(),
            ),
            grandparent_owner: Some(UID),
            ..system_unit()
        };
        assert_eq!(declined(&facts, true), NOT_A_BOUNDARY);
    }

    #[test]
    fn a_launch_nested_in_an_adopted_launch_is_not_a_delegation_boundary() {
        for leaf in ["murmur-supervisor", "murmur-ses_abc-0"] {
            let facts = ParentFacts {
                unified_path: Some(format!("/system.slice/launch.service/murmur-4242-p/{leaf}")),
                grandparent_owner: Some(UID),
                ..system_unit()
            };
            assert_eq!(declined(&facts, false), NOT_A_BOUNDARY, "{leaf}");
        }
    }

    #[test]
    fn an_unreadable_grandparent_is_not_a_delegation_boundary() {
        let facts = ParentFacts {
            grandparent_owner: None,
            ..system_unit()
        };
        assert_eq!(declined(&facts, false), NOT_A_BOUNDARY);
    }

    #[test]
    fn missing_parent_controllers_are_named_in_order() {
        for (enabled, io, reason) in [
            ("", false, "parent does not enable memory,pids,cpu"),
            ("", true, "parent does not enable memory,pids,cpu,io"),
            ("cpu io", false, "parent does not enable memory,pids"),
            ("memory pids cpu", true, "parent does not enable io"),
            ("pids", true, "parent does not enable memory,cpu,io"),
        ] {
            let facts = ParentFacts {
                parent_subtree_control: enabled.to_string(),
                ..system_unit()
            };
            assert_eq!(declined(&facts, io), reason, "{enabled:?} io={io}");
        }
    }

    #[test]
    fn io_is_required_only_when_declared() {
        let facts = ParentFacts {
            parent_subtree_control: "cpu memory pids".to_string(),
            own_controllers: "cpu memory pids".to_string(),
            ..system_unit()
        };
        assert!(decide(&facts, false).is_ok());
        assert_eq!(declined(&facts, true), "parent does not enable io");
    }

    #[test]
    fn missing_own_controllers_are_named_in_order() {
        for (listed, io, reason) in [
            (
                "",
                false,
                "this process's cgroup does not list memory,pids,cpu",
            ),
            (
                "memory cpu",
                true,
                "this process's cgroup does not list pids,io",
            ),
        ] {
            let facts = ParentFacts {
                own_controllers: listed.to_string(),
                ..system_unit()
            };
            assert_eq!(declined(&facts, io), reason, "{listed:?} io={io}");
        }
    }

    #[test]
    fn launch_pids_are_read_only_from_murmur_digits_p() {
        assert_eq!(launch_pid("murmur-12-p"), Some(12));
        for name in [
            "murmur-x-p",
            "murmur-12-q",
            "murmur-12",
            "murmur--p",
            "launcher",
        ] {
            assert_eq!(launch_pid(name), None, "{name}");
        }
    }

    /// A pid no process has: a short child spawned and reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn mkdirs(root: &Path, relative: &str) {
        std::fs::create_dir_all(root.join(relative)).unwrap();
    }

    #[test]
    fn sweep_removes_only_empty_trees_of_gone_launches_and_its_own() {
        let parent = tempfile::TempDir::new().unwrap();
        let root = parent.path();
        let dead = dead_pid();
        let live = std::os::unix::process::parent_id();
        let own = std::process::id();
        let busy = dead_pid();
        mkdirs(root, &format!("murmur-{dead}-p/murmur-supervisor"));
        mkdirs(root, &format!("murmur-{dead}-p/murmur-ses_x-0"));
        mkdirs(root, &format!("murmur-{live}-p/murmur-supervisor"));
        mkdirs(root, &format!("murmur-{own}-p/murmur-supervisor"));
        mkdirs(root, &format!("murmur-{busy}-p"));
        std::fs::write(root.join(format!("murmur-{busy}-p/cgroup.procs")), "1\n").unwrap();
        for untouched in ["launcher", "murmur-x-p", "murmur-12-q", "murmur-12"] {
            mkdirs(root, untouched);
        }

        sweep_stale(root, own);

        assert!(!root.join(format!("murmur-{dead}-p")).exists());
        assert!(!root.join(format!("murmur-{own}-p")).exists());
        assert!(root
            .join(format!("murmur-{live}-p/murmur-supervisor"))
            .is_dir());
        assert!(root.join(format!("murmur-{busy}-p/cgroup.procs")).is_file());
        for untouched in ["launcher", "murmur-x-p", "murmur-12-q", "murmur-12"] {
            assert!(root.join(untouched).is_dir(), "{untouched}");
        }
    }

    #[test]
    fn sweep_of_a_missing_parent_returns() {
        sweep_stale(
            Path::new("/nonexistent/murmur-sweep-parent"),
            std::process::id(),
        );
    }

    /// A tree with an origin cgroup whose `cgroup.procs` and an own cgroup whose `cgroup.kill`
    /// are plain files, so what the hook wrote can be read back.
    fn release_tree(root: &Path) -> ExitRecord {
        mkdirs(root, "launcher");
        mkdirs(root, "murmur-1-p/murmur-supervisor");
        std::fs::write(root.join("launcher/cgroup.procs"), "").unwrap();
        std::fs::write(root.join("murmur-1-p/cgroup.kill"), "").unwrap();
        ExitRecord {
            pid: std::process::id(),
            origin: root.join("launcher"),
            own: root.join("murmur-1-p"),
        }
    }

    #[test]
    fn release_does_nothing_in_a_process_that_did_not_adopt() {
        let parent = tempfile::TempDir::new().unwrap();
        let record = release_tree(parent.path());

        release(&record, record.pid + 1);

        assert_eq!(
            std::fs::read_to_string(record.origin.join("cgroup.procs")).unwrap(),
            ""
        );
        assert_eq!(
            std::fs::read_to_string(record.own.join("cgroup.kill")).unwrap(),
            ""
        );
        assert!(record.own.join("murmur-supervisor").is_dir());
    }

    #[test]
    fn release_leaves_the_tree_when_the_origin_is_gone() {
        let parent = tempfile::TempDir::new().unwrap();
        let record = release_tree(parent.path());
        std::fs::remove_file(record.origin.join("cgroup.procs")).unwrap();
        std::fs::remove_dir(&record.origin).unwrap();

        release(&record, record.pid);

        assert_eq!(
            std::fs::read_to_string(record.own.join("cgroup.kill")).unwrap(),
            "",
            "nothing is killed while this process may still be inside"
        );
        assert!(record.own.join("murmur-supervisor").is_dir());
    }

    #[test]
    fn release_moves_back_kills_and_removes_what_it_can() {
        let parent = tempfile::TempDir::new().unwrap();
        let record = release_tree(parent.path());

        release(&record, record.pid);

        assert_eq!(
            std::fs::read_to_string(record.origin.join("cgroup.procs")).unwrap(),
            format!("{}\n", record.pid)
        );
        assert_eq!(
            std::fs::read_to_string(record.own.join("cgroup.kill")).unwrap(),
            "1"
        );
        // On cgroupfs the control files go with the directory; here the plain file keeps it, and
        // the empty child is gone.
        assert!(!record.own.join("murmur-supervisor").exists());
    }

    #[test]
    fn a_failed_move_removes_the_directory_it_created() {
        let parent = tempfile::TempDir::new().unwrap();
        // No `cgroup.procs` appears in a plain directory, so the move fails with ENOENT.
        let reason = take_parent(parent.path(), std::process::id(), &[]).expect_err("declined");
        let own = parent.path().join(own_cgroup_name(std::process::id()));
        assert!(
            reason.starts_with(&format!("could not move into {}: ", own.display())),
            "{reason}"
        );
        assert!(!own.exists());
    }

    #[test]
    fn a_recycled_directory_that_cannot_be_removed_declines() {
        let parent = tempfile::TempDir::new().unwrap();
        let own = parent.path().join(own_cgroup_name(std::process::id()));
        std::fs::create_dir(&own).unwrap();
        std::fs::write(own.join("held"), "").unwrap();
        let reason = take_parent(parent.path(), std::process::id(), &[]).expect_err("declined");
        assert!(
            reason.starts_with(&format!(
                "{} already exists and could not be removed: ",
                own.display()
            )),
            "{reason}"
        );
        assert!(own.join("held").is_file());
    }

    #[test]
    fn a_failed_create_names_the_path() {
        let reason = take_parent(
            Path::new("/nonexistent/murmur-parent"),
            std::process::id(),
            &[],
        )
        .expect_err("declined");
        assert!(
            reason.starts_with(&format!(
                "could not create /nonexistent/murmur-parent/murmur-{}-p: ",
                std::process::id()
            )),
            "{reason}"
        );
    }
}
