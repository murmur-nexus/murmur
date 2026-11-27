//! Which host setup this process is in, and how a launch here would get its cgroup scope.
//!
//! [`read_host_facts`] reads what a launch's scope depends on — this process's cgroup, the
//! delegated parent above it ([`super::parent::ParentFacts`]), whether a session bus is reachable,
//! the systemd user manager's delegated controllers and the open-file limit — with no privilege,
//! no write, no D-Bus connection and no move. [`diagnose`] turns them into a [`LaunchHostReport`]:
//! the setup, where a launch would take its scope from, whether that works, and when it does not,
//! the cause found here and the commands that fix it. `mur doctor` prints the report
//! ([`render_launch_host`]); a launch refused with `E-RUN-012` names the same cause and carries
//! the same fix ([`refusal_cause_and_fix`]).
//!
//! The facts are read once per process, before it moves anywhere ([`super::launch_host_facts`]),
//! so a refusal that comes after the process moved into a scope still describes where it started.

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use super::parent::{self, DeclineCheck, ParentFacts};
#[cfg(target_os = "linux")]
use super::{missing_controllers, REQUIRED_CONTROLLERS};

/// The docs section the delegated-parent fixes cite.
#[cfg(target_os = "linux")]
const URL_PARENT: &str = murmur_artifact::docs_reference_url!("resource-limits/#delegated-parent");

/// The controllers a report names, in the order it names them.
const NAMED_CONTROLLERS: &[&str] = &["memory", "pids", "cpu", "io"];

/// `E-RUN-012`'s fix when no cause particular to this host was found.
pub(crate) const FIX_GENERIC: &str = concat!(
    "the systemd user unit `mur` runs under needs `Delegate=yes` for memory, pids, cpu and io \
     \u{2014} see ",
    murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior")
);

#[cfg(target_os = "linux")]
const FIX_NO_CGROUP_V2: &str = concat!(
    "boot the host with the cgroup v2 unified hierarchy (systemd.unified_cgroup_hierarchy=1 on \
     the kernel command line) \u{2014} see ",
    murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior")
);

#[cfg(target_os = "linux")]
const FIX_USER_MANAGER: &str = concat!(
    "give the systemd user manager memory, pids, cpu and io to delegate (see ",
    murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior"),
    "), then restart it \u{2014} the restart ends this user's running processes; on a desktop, \
     log out and back in instead:\n",
    "    sudo mkdir -p /etc/systemd/system/user@.service.d\n",
    "    printf \"[Service]\\nDelegate=memory pids cpu io\\n\" | sudo tee \
     /etc/systemd/system/user@.service.d/delegate.conf\n",
    "    sudo systemctl daemon-reload && sudo systemctl restart user@$(id -u).service && sudo \
     loginctl enable-linger $(whoami)"
);

#[cfg(target_os = "linux")]
const FIX_LINGER: &str = concat!(
    "start a systemd user manager for this user, then log in again (see ",
    murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior"),
    "):\n",
    "    sudo loginctl enable-linger $(whoami)"
);

#[cfg(target_os = "linux")]
const FIX_NO_BUS: &str = concat!(
    "start mur from a login that reaches the systemd user manager's session bus (an ssh or \
     desktop login sets DBUS_SESSION_BUS_ADDRESS or $XDG_RUNTIME_DIR/bus; su and sudo do not), \
     or run the launcher in a system unit with Delegate=yes \u{2014} see ",
    murmur_artifact::docs_reference_url!("resource-limits/#delegated-parent")
);

/// Which host setup this process runs in, read from its own cgroup path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostSetup {
    /// `/proc/self/cgroup` has no `0::` line.
    NoCgroupV2,
    /// The path has a `user@<digits>.service` component: a process of a systemd user manager.
    UserManager,
    /// `/user.slice/user-<n>.slice/session-<x>.scope[/…]`: a login session's scope.
    LoginSession,
    /// `/system.slice/<name>[/…]`: a system unit.
    SystemUnit,
    /// Any other path.
    Other,
    /// Not Linux: no cgroup scope is needed.
    NotLinux,
}

/// Where a launch here would take its cgroup scope from, in the order a launch tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeSource {
    /// The delegated parent this process sits in is adopted.
    DelegatedParent,
    /// The systemd user manager is asked for a transient scope over the session bus.
    UserManager,
    /// The scope is built under this process's own cgroup.
    OwnCgroup,
    /// No delegated cgroup is reachable.
    None,
}

/// The delegated parent check: the cgroup above this process's and whether a launch adopts it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DelegatedParentReport {
    /// The parent's absolute path under the cgroup2 mount, no trailing slash; `null` when this
    /// process's cgroup has no parent or there is no cgroup v2 hierarchy.
    pub path: Option<String>,
    /// The uid owning the parent; `null` when there is none or it could not be read.
    pub owner: Option<u32>,
    /// The parent's `cgroup.subtree_control`, as listed.
    pub subtree_control: Vec<String>,
    /// Whether a launch adopts the parent.
    pub adopts: bool,
    /// Why it does not; `null` when it does.
    pub reason: Option<String>,
}

/// The systemd user manager's cgroup, when it exists.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UserManagerReport {
    /// Its absolute path under the cgroup2 mount.
    pub cgroup: String,
    /// Its `cgroup.controllers`, as listed: what it can delegate to the scopes it creates.
    pub controllers: Vec<String>,
}

/// `RLIMIT_NOFILE`, soft and hard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct OpenFiles {
    /// The soft limit; `null` for unlimited or unreadable.
    pub soft: Option<u64>,
    /// The hard limit; `null` for unlimited or unreadable.
    pub hard: Option<u64>,
    /// Whether `getrlimit` answered: with it, `None` above means unlimited.
    #[serde(skip)]
    pub read: bool,
}

/// What [`diagnose`] found: `mur doctor --json`'s `cgroup_host`, and the lines of `mur doctor`'s
/// `Cgroup scope` block ([`render_launch_host`]). Serialized keys keep the field order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LaunchHostReport {
    /// Which host setup this process runs in.
    pub setup: HostSetup,
    /// This process's cgroup path from `/proc/self/cgroup`, relative to the visible root;
    /// `null` without a cgroup v2 hierarchy and off Linux.
    pub cgroup: Option<String>,
    /// The unit the setup names (`user@1000.service`, `session-3.scope`, the system unit);
    /// `null` for the other setups.
    pub unit: Option<String>,
    /// This process's effective uid; `null` off Linux.
    pub euid: Option<u32>,
    /// Where a launch would take its scope from.
    pub scope_source: ScopeSource,
    /// Whether a launch would get its scope from there.
    pub ready: bool,
    /// The delegated parent check; `null` off Linux.
    pub delegated_parent: Option<DelegatedParentReport>,
    /// This process's own `cgroup.controllers`, as listed.
    pub own_controllers: Vec<String>,
    /// Whether a session bus is reachable, by the lookup a launch's systemd request makes.
    pub session_bus: bool,
    /// The systemd user manager's cgroup; `null` when it does not exist.
    pub user_manager: Option<UserManagerReport>,
    /// The controllers [`Self::cause`] names as missing, in the order memory, pids, cpu, io;
    /// empty when ready.
    pub missing: Vec<String>,
    /// Why a launch here gets no scope; `null` when ready.
    pub cause: Option<String>,
    /// The commands that fix it: one sentence line, then each command on its own line indented
    /// four spaces; `null` when ready.
    pub fix: Option<String>,
    /// The open-file limit.
    pub open_files: OpenFiles,
}

impl LaunchHostReport {
    /// The report off Linux, where no cgroup scope is needed.
    #[cfg(any(not(target_os = "linux"), test))]
    pub(super) fn not_linux() -> Self {
        Self {
            setup: HostSetup::NotLinux,
            cgroup: None,
            unit: None,
            euid: None,
            scope_source: ScopeSource::None,
            ready: true,
            delegated_parent: None,
            own_controllers: Vec::new(),
            session_bus: false,
            user_manager: None,
            missing: Vec::new(),
            cause: None,
            fix: None,
            open_files: OpenFiles::default(),
        }
    }

    /// The setup as `mur doctor` and the causes word it.
    fn setup_label(&self) -> String {
        let unit = self.unit.as_deref().unwrap_or_default();
        match self.setup {
            HostSetup::UserManager => format!("systemd user manager ({unit})"),
            HostSetup::LoginSession => format!("login session ({unit})"),
            HostSetup::SystemUnit => format!("system unit ({unit})"),
            HostSetup::Other => format!("cgroup {}", self.cgroup.as_deref().unwrap_or_default()),
            HostSetup::NoCgroupV2 => "no cgroup v2 unified hierarchy".to_string(),
            HostSetup::NotLinux => "not Linux".to_string(),
        }
    }
}

/// The lines `mur doctor` prints for `report` inside its `Cgroup scope` block, after the
/// `cgroup scope:` line, without the block's indentation: `host setup:`, `delegated parent:`,
/// `open files:`, and when not ready `cause:` and `fix:`, the fix's command lines following it
/// with their own four-space indentation. Off Linux only the `host setup:` line.
#[must_use]
pub fn render_launch_host(report: &LaunchHostReport) -> Vec<String> {
    if report.setup == HostSetup::NotLinux {
        return vec!["host setup: not Linux \u{2014} no cgroup scope is needed".to_string()];
    }
    let how = match report.scope_source {
        ScopeSource::DelegatedParent => format!(
            "a launch adopts the delegated parent {}",
            report
                .delegated_parent
                .as_ref()
                .and_then(|parent| parent.path.as_deref())
                .unwrap_or_default()
        ),
        ScopeSource::UserManager => format!(
            "a launch asks the systemd user manager for a scope; it delegates {}",
            report
                .user_manager
                .as_ref()
                .map(|manager| named_or_none(&manager.controllers))
                .unwrap_or_else(|| "no controllers".to_string())
        ),
        ScopeSource::OwnCgroup => format!(
            "a launch builds under mur's own cgroup, delegated to uid {}",
            report.euid.unwrap_or_default()
        ),
        ScopeSource::None => "no delegated cgroup is reachable".to_string(),
    };
    let mut lines = vec![format!(
        "host setup: {} \u{2014} {how}",
        report.setup_label()
    )];
    if let Some(parent) = &report.delegated_parent {
        lines.push(match (&parent.reason, &parent.path) {
            (None, Some(path)) => format!("delegated parent: would be adopted \u{2014} {path}"),
            (reason, _) => format!(
                "delegated parent: not adopted \u{2014} {}",
                reason.as_deref().unwrap_or_default()
            ),
        });
    }
    lines.push(render_open_files(&report.open_files));
    if let Some(cause) = &report.cause {
        lines.push(format!("cause: {cause}"));
    }
    if let Some(fix) = &report.fix {
        let mut fix_lines = fix.lines();
        lines.push(format!("fix: {}", fix_lines.next().unwrap_or_default()));
        lines.extend(fix_lines.map(str::to_string));
    }
    lines
}

/// The `open files:` line.
fn render_open_files(open_files: &OpenFiles) -> String {
    if !open_files.read {
        return "open files: unknown".to_string();
    }
    let show =
        |limit: Option<u64>| limit.map_or_else(|| "unlimited".to_string(), |n| n.to_string());
    format!(
        "open files: {} (hard {})",
        show(open_files.soft),
        show(open_files.hard)
    )
}

/// The named controllers in `listed`, comma-space joined in [`NAMED_CONTROLLERS`]'s order, or
/// `no controllers`.
fn named_or_none(listed: &[String]) -> String {
    let named = named_in(listed);
    if named.is_empty() {
        "no controllers".to_string()
    } else {
        named.join(", ")
    }
}

/// Which of [`NAMED_CONTROLLERS`] `listed` has, in that order.
fn named_in(listed: &[String]) -> Vec<&'static str> {
    NAMED_CONTROLLERS
        .iter()
        .copied()
        .filter(|controller| listed.iter().any(|entry| entry == controller))
        .collect()
}

/// What [`diagnose`] reads: the delegated parent's facts and what a launch's other two routes
/// depend on. Read once per process by [`read_host_facts`].
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostFacts {
    /// The parent facts [`parent::decide`] reads.
    pub parent: ParentFacts,
    /// Whether a session bus is reachable ([`session_bus_reachable`]).
    pub session_bus: bool,
    /// The uid owning this process's own cgroup directory; `None` when it could not be read.
    pub own_owner: Option<u32>,
    /// The systemd user manager's cgroup and its `cgroup.controllers`, `None` when that directory
    /// does not exist.
    pub user_manager: Option<(PathBuf, String)>,
    /// `RLIMIT_NOFILE`.
    pub open_files: OpenFiles,
}

/// This process's facts: `/proc/self/cgroup`, `/proc/mounts` (fallback `/sys/fs/cgroup`), the
/// effective uid, the session bus lookup and `RLIMIT_NOFILE`, then [`read_host_facts_at`]. Writes
/// nothing, needs no privilege and opens no D-Bus connection.
#[cfg(target_os = "linux")]
pub(super) fn read_host_facts() -> HostFacts {
    let proc_self_cgroup = std::fs::read_to_string("/proc/self/cgroup").ok();
    let mount = PathBuf::from(
        std::fs::read_to_string("/proc/mounts")
            .map(|contents| super::parse_cgroup2_mount(&contents))
            .unwrap_or_else(|_| super::DEFAULT_CGROUP2_MOUNT.to_string()),
    );
    let euid = crate::murmur_home::current_euid();
    read_host_facts_at(
        proc_self_cgroup.as_deref(),
        mount,
        euid,
        session_bus_reachable(euid),
        read_open_files(),
    )
}

/// Whether `zbus::Connection::session()`, which a launch's systemd request connects with, would
/// find a bus address to connect to, asked without connecting: `DBUS_SESSION_BUS_ADDRESS` set and
/// non-empty, or else `<runtime dir>/bus` exists, the runtime dir being `$XDG_RUNTIME_DIR` or,
/// when that is unset, `/run/user/<euid>`.
#[cfg(target_os = "linux")]
fn session_bus_reachable(euid: u32) -> bool {
    match std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        Some(address) => !address.is_empty(),
        None => {
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map_or_else(|| PathBuf::from(format!("/run/user/{euid}")), PathBuf::from);
            std::fs::symlink_metadata(runtime_dir.join("bus")).is_ok()
        }
    }
}

/// `getrlimit(RLIMIT_NOFILE)`, `RLIM_INFINITY` as `None`.
#[cfg(target_os = "linux")]
// `read_rlimit` takes `u32` for every platform; on glibc `RLIMIT_NOFILE` already is one, on musl
// it is `c_int`.
#[allow(clippy::unnecessary_cast)]
fn read_open_files() -> OpenFiles {
    let Ok(limit) = crate::resources::read_rlimit(libc::RLIMIT_NOFILE as u32) else {
        return OpenFiles::default();
    };
    let finite = |value: libc::rlim_t| (value != libc::RLIM_INFINITY).then_some(value);
    OpenFiles {
        soft: finite(limit.rlim_cur),
        hard: finite(limit.rlim_max),
        read: true,
    }
}

/// The facts for `proc_self_cgroup` (the contents of `/proc/self/cgroup`, `None` when unreadable)
/// on the cgroup2 hierarchy at `mount`, for `euid`, with the session bus and open-file facts as
/// given: the parent facts, the owner of this process's own cgroup and the user manager's
/// controllers, read from the cgroup directories alone.
#[cfg(target_os = "linux")]
pub(super) fn read_host_facts_at(
    proc_self_cgroup: Option<&str>,
    mount: PathBuf,
    euid: u32,
    session_bus: bool,
    open_files: OpenFiles,
) -> HostFacts {
    let parent = parent::read_parent_facts_from(proc_self_cgroup, mount, euid);
    let own_owner = parent
        .unified_path
        .as_deref()
        .and_then(|path| parent::directory_owner(&parent::under_mount(&parent.mount, path)));
    let user_manager = parent.unified_path.as_deref().and_then(|path| {
        let directory = parent::under_mount(&parent.mount, &user_manager_path(path, euid));
        directory.is_dir().then(|| {
            let controllers =
                std::fs::read_to_string(directory.join("cgroup.controllers")).unwrap_or_default();
            (directory, controllers)
        })
    });
    HostFacts {
        parent,
        session_bus,
        own_owner,
        user_manager,
        open_files,
    }
}

/// The user manager's cgroup path for this process: `unified_path` through its
/// `user@<n>.service` component when it has one, else
/// `/user.slice/user-<euid>.slice/user@<euid>.service`.
#[cfg(target_os = "linux")]
fn user_manager_path(unified_path: &str, euid: u32) -> String {
    let mut prefix = String::new();
    for component in unified_path.split('/').filter(|part| !part.is_empty()) {
        prefix.push('/');
        prefix.push_str(component);
        if parent::is_user_manager(component) {
            return prefix;
        }
    }
    format!("/user.slice/user-{euid}.slice/user@{euid}.service")
}

/// The setup and its unit for this process's cgroup path, first match wins.
#[cfg(target_os = "linux")]
pub(super) fn classify(unified_path: Option<&str>) -> (HostSetup, Option<String>) {
    let Some(path) = unified_path else {
        return (HostSetup::NoCgroupV2, None);
    };
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if let Some(unit) = components
        .iter()
        .find(|component| parent::is_user_manager(component))
    {
        return (HostSetup::UserManager, Some((*unit).to_string()));
    }
    match components.as_slice() {
        ["user.slice", user, session, ..]
            if is_user_slice(user)
                && session.starts_with("session-")
                && session.ends_with(".scope") =>
        {
            (HostSetup::LoginSession, Some((*session).to_string()))
        }
        ["system.slice", unit, ..] => (HostSetup::SystemUnit, Some((*unit).to_string())),
        _ => (HostSetup::Other, None),
    }
}

/// Whether `name` is `user-<digits>.slice`.
#[cfg(target_os = "linux")]
fn is_user_slice(name: &str) -> bool {
    name.strip_prefix("user-")
        .and_then(|rest| rest.strip_suffix(".slice"))
        .is_some_and(|uid| !uid.is_empty() && uid.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `path` as the reports show it: no trailing slash, the root itself as `/`.
#[cfg(target_os = "linux")]
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let trimmed = text.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The entries of a control file's contents.
#[cfg(target_os = "linux")]
fn listed(contents: &str) -> Vec<String> {
    contents.split_whitespace().map(str::to_string).collect()
}

/// The report for `facts` and a capsule that does or does not declare
/// `capabilities.resources.cgroup_io_bytes_per_sec`, which makes `io` a controller the delegated
/// parent must provide.
///
/// The scope source is the first of a launch's routes that applies: the delegated parent when
/// [`parent::decide`] passes; else the systemd user manager when a session bus is reachable and
/// its cgroup exists, ready when that cgroup lists memory, pids and cpu; else this process's own
/// cgroup when the euid owns it and it lists them; else none.
#[cfg(target_os = "linux")]
pub(super) fn diagnose(facts: &HostFacts, declares_io_limit: bool) -> LaunchHostReport {
    let parent_facts = &facts.parent;
    let euid = parent_facts.euid;
    let unified_path = parent_facts.unified_path.as_deref();
    let (setup, unit) = classify(unified_path);
    let parent_path = unified_path
        .and_then(parent::parent_of)
        .map(|relative| parent::under_mount(&parent_facts.mount, relative));
    let decision = parent::decide_with_check(parent_facts, declares_io_limit);
    let delegated_parent = DelegatedParentReport {
        path: parent_path.as_deref().map(display_path),
        owner: parent_facts.parent_owner.filter(|_| parent_path.is_some()),
        subtree_control: listed(&parent_facts.parent_subtree_control),
        adopts: decision.is_ok(),
        reason: decision.as_ref().err().map(|(_, reason)| reason.clone()),
    };
    let user_manager =
        facts
            .user_manager
            .as_ref()
            .map(|(directory, controllers)| UserManagerReport {
                cgroup: display_path(directory),
                controllers: listed(controllers),
            });
    let user_manager_lacks = facts
        .user_manager
        .as_ref()
        .map(|(_, controllers)| missing_controllers(controllers, REQUIRED_CONTROLLERS));

    let (scope_source, ready) = if decision.is_ok() {
        (ScopeSource::DelegatedParent, true)
    } else if let (true, Some(lacks)) = (facts.session_bus, &user_manager_lacks) {
        (ScopeSource::UserManager, lacks.is_empty())
    } else if facts.own_owner == Some(euid)
        && missing_controllers(&parent_facts.own_controllers, REQUIRED_CONTROLLERS).is_empty()
    {
        (ScopeSource::OwnCgroup, true)
    } else {
        (ScopeSource::None, false)
    };

    let mut report = LaunchHostReport {
        setup,
        cgroup: unified_path.map(str::to_string),
        unit,
        euid: Some(euid),
        scope_source,
        ready,
        delegated_parent: Some(delegated_parent),
        own_controllers: listed(&parent_facts.own_controllers),
        session_bus: facts.session_bus,
        user_manager,
        missing: Vec::new(),
        cause: None,
        fix: None,
        open_files: facts.open_files,
    };
    if !ready {
        let (missing, cause, fix) = cause_and_fix(facts, &report, &decision, declares_io_limit);
        report.missing = missing;
        report.cause = Some(cause);
        report.fix = Some(fix);
    }
    report
}

/// The missing controllers, cause and fix for a report that is not ready: the first of the
/// causes below that applies.
#[cfg(target_os = "linux")]
fn cause_and_fix(
    facts: &HostFacts,
    report: &LaunchHostReport,
    decision: &Result<PathBuf, (DeclineCheck, String)>,
    declares_io_limit: bool,
) -> (Vec<String>, String, String) {
    let parent_facts = &facts.parent;
    let euid = parent_facts.euid;
    let mount = display_path(&parent_facts.mount);
    let decline_reason = match decision {
        Err((_, reason)) => reason.as_str(),
        Ok(_) => "",
    };
    if report.setup == HostSetup::NoCgroupV2 {
        return (
            Vec::new(),
            "this host has no cgroup v2 unified hierarchy".to_string(),
            FIX_NO_CGROUP_V2.to_string(),
        );
    }
    let parent_path = report
        .delegated_parent
        .as_ref()
        .and_then(|parent| parent.path.clone())
        .unwrap_or_default();
    let wanted = parent::required_controllers(declares_io_limit);
    match decision {
        Err((DeclineCheck::ParentDoesNotEnable, _)) => {
            let missing = missing_controllers(&parent_facts.parent_subtree_control, &wanted);
            let cause = format!(
                "the cgroup above mur's ({parent_path}) is delegated to uid {euid} but does not \
                 enable {} for its children",
                missing.join(", ")
            );
            return (missing, cause, fix_parent(&parent_path));
        }
        Err((DeclineCheck::OwnDoesNotList, _)) => {
            let missing = missing_controllers(&parent_facts.own_controllers, &wanted);
            let cause = format!(
                "the cgroup above mur's ({parent_path}) is delegated to uid {euid} but mur's own \
                 cgroup does not list {}",
                missing.join(", ")
            );
            return (missing, cause, fix_parent(&parent_path));
        }
        _ => {}
    }
    if facts.session_bus {
        if let Some((directory, controllers)) = &facts.user_manager {
            let missing = missing_controllers(controllers, REQUIRED_CONTROLLERS);
            let named = named_in(&listed(controllers));
            let delegated = if named.is_empty() {
                "no controllers".to_string()
            } else {
                format!("only {}", named.join(", "))
            };
            let unit = directory
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cause = format!(
                "this host's systemd user manager ({unit}) delegates {delegated} to the scopes it \
                 creates, and a launch needs memory, pids, cpu"
            );
            return (missing, cause, FIX_USER_MANAGER.to_string());
        }
        let cause = format!(
            "no systemd user manager runs for uid {euid} (no \
             {mount}/user.slice/user-{euid}.slice/user@{euid}.service), so no launch can ask it \
             for a scope"
        );
        return (Vec::new(), cause, FIX_LINGER.to_string());
    }
    match report.setup {
        HostSetup::SystemUnit => {
            let cause = format!(
                "mur runs in the system unit {}, whose cgroup is not delegated to uid {euid} \
                 ({decline_reason}), and no session bus reaches a systemd user manager",
                report.unit.as_deref().unwrap_or_default()
            );
            (Vec::new(), cause, fix_system_unit(&mount))
        }
        HostSetup::UserManager | HostSetup::LoginSession => {
            let cause = format!(
                "mur runs in {} but no session bus reaches the systemd user manager \
                 (DBUS_SESSION_BUS_ADDRESS is unset and $XDG_RUNTIME_DIR/bus does not exist), and \
                 its own cgroup is not delegated to uid {euid}",
                report.setup_label()
            );
            (Vec::new(), cause, FIX_NO_BUS.to_string())
        }
        _ => {
            let cause = format!(
                "mur's cgroup ({}) is not delegated to uid {euid}, no delegated parent was adopted \
                 ({decline_reason}), and no session bus reaches a systemd user manager",
                report.cgroup.as_deref().unwrap_or_default()
            );
            (Vec::new(), cause, FIX_GENERIC.to_string())
        }
    }
}

/// The fix for a delegated parent that does not provide the controllers.
#[cfg(target_os = "linux")]
fn fix_parent(parent_path: &str) -> String {
    format!(
        "enable the controllers for the children of {parent_path}, with no process left directly \
         in it (see {URL_PARENT}):\n    echo \"+memory +pids +cpu +io\" > \
         {parent_path}/cgroup.subtree_control"
    )
}

/// The fix for a system unit whose cgroup is not delegated, for the cgroup2 hierarchy at `mount`.
#[cfg(target_os = "linux")]
fn fix_system_unit(mount: &str) -> String {
    format!(
        "run the launcher in a system unit with Delegate=yes as this user, moved into a leaf with \
         the controllers enabled for its children (see {URL_PARENT}):\n    sudo systemd-run \
         --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p LimitNOFILE=65536 --wait --collect \
         --pipe --setenv=HOME=$HOME --setenv=PATH=$PATH sh -c 'U={mount}$(cut -d: -f3 \
         /proc/self/cgroup); mkdir -p \"$U/launcher\" && echo $$ > \"$U/launcher/cgroup.procs\" \
         && echo \"+memory +pids +cpu +io\" > \"$U/cgroup.subtree_control\" && exec \"$@\"' sh \
         <launcher command>"
    )
}

/// The cause and fix `E-RUN-012` carries for a launch refused on the host `report` describes:
/// the report's own when it is not ready, and when it is (the cgroups look delegated, so the
/// operation itself failed: `EROFS`, `EBUSY`, a systemd give-up) a cause saying so and
/// [`FIX_GENERIC`].
#[must_use]
pub(super) fn refusal_cause_and_fix(report: &LaunchHostReport) -> (String, String) {
    match (&report.cause, &report.fix, report.setup) {
        (Some(cause), Some(fix), _) => (cause.clone(), fix.clone()),
        (_, _, HostSetup::NotLinux) => (
            "this host is not Linux, where no cgroup scope is needed".to_string(),
            FIX_GENERIC.to_string(),
        ),
        _ => (
            format!(
                "the cgroups mur reads look delegated to uid {}, so the operation named above is \
                 what failed",
                report.euid.unwrap_or_default()
            ),
            FIX_GENERIC.to_string(),
        ),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::SystemTime;

    /// A cgroup tree under a temporary directory standing in for the cgroup2 mount.
    struct FakeRoot {
        dir: tempfile::TempDir,
    }

    impl FakeRoot {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("tempdir"),
            }
        }

        fn mount(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }

        fn mount_text(&self) -> String {
            self.dir.path().display().to_string()
        }

        /// Creates the cgroup at `relative` (and its parents).
        fn cgroup(&self, relative: &str) -> &Self {
            std::fs::create_dir_all(self.path(relative)).expect("mkdir");
            self
        }

        /// Writes `name` in the cgroup at `relative`, creating the cgroup.
        fn file(&self, relative: &str, name: &str, contents: &str) -> &Self {
            self.cgroup(relative);
            std::fs::write(self.path(relative).join(name), contents).expect("write");
            self
        }

        fn path(&self, relative: &str) -> PathBuf {
            parent::under_mount(self.dir.path(), relative)
        }

        /// The facts for a process in `own` (`None`: no `0::` line), as `euid`.
        fn facts(&self, own: Option<&str>, euid: u32, session_bus: bool) -> HostFacts {
            let proc_self_cgroup = match own {
                Some(path) => format!("0::{path}\n"),
                None => "12:pids:/user.slice\n1:name=systemd:/user.slice\n".to_string(),
            };
            read_host_facts_at(
                Some(&proc_self_cgroup),
                self.mount(),
                euid,
                session_bus,
                OpenFiles {
                    soft: Some(1024),
                    hard: None,
                    read: true,
                },
            )
        }
    }

    fn uid() -> u32 {
        crate::murmur_home::current_euid()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// `/user.slice/user-<uid>.slice` with a login session and a user manager whose
    /// `cgroup.controllers` is `delegates`.
    fn login_session(root: &FakeRoot, delegates: Option<&str>) -> String {
        let uid = uid();
        let session = format!("/user.slice/user-{uid}.slice/session-3.scope");
        root.file(&session, "cgroup.controllers", "");
        if let Some(delegates) = delegates {
            root.file(
                &format!("/user.slice/user-{uid}.slice/user@{uid}.service"),
                "cgroup.controllers",
                delegates,
            );
        }
        session
    }

    #[test]
    fn no_delegation_names_the_user_manager_and_its_fix() {
        let root = FakeRoot::new();
        let own = login_session(&root, Some(""));
        let uid = uid();
        let report = diagnose(&root.facts(Some(&own), uid, true), false);
        assert_eq!(report.setup, HostSetup::LoginSession);
        assert_eq!(report.unit.as_deref(), Some("session-3.scope"));
        assert_eq!(report.scope_source, ScopeSource::UserManager);
        assert!(!report.ready);
        assert_eq!(report.missing, strings(&["memory", "pids", "cpu"]));
        assert_eq!(
            report.cause.as_deref(),
            Some(
                format!(
                    "this host's systemd user manager (user@{uid}.service) delegates no \
                     controllers to the scopes it creates, and a launch needs memory, pids, cpu"
                )
                .as_str()
            )
        );
        assert_eq!(report.fix.as_deref(), Some(FIX_USER_MANAGER));
        assert_eq!(
            FIX_USER_MANAGER,
            "give the systemd user manager memory, pids, cpu and io to delegate (see \
             https://docs.murmur.nexus/reference/resource-limits/#platform-behavior), then \
             restart it \u{2014} the restart ends this user's running processes; on a desktop, log \
             out and back in instead:\n    sudo mkdir -p /etc/systemd/system/user@.service.d\n    \
             printf \"[Service]\\nDelegate=memory pids cpu io\\n\" | sudo tee \
             /etc/systemd/system/user@.service.d/delegate.conf\n    sudo systemctl daemon-reload \
             && sudo systemctl restart user@$(id -u).service && sudo loginctl enable-linger \
             $(whoami)"
        );
        let manager = report.user_manager.as_ref().expect("user manager");
        assert_eq!(
            manager.cgroup,
            format!(
                "{}/user.slice/user-{uid}.slice/user@{uid}.service",
                root.mount_text()
            )
        );
        assert!(manager.controllers.is_empty());
    }

    #[test]
    fn delegation_without_cpu_and_io_names_what_the_user_manager_delegates() {
        let root = FakeRoot::new();
        let own = login_session(&root, Some("memory pids\n"));
        let uid = uid();
        let report = diagnose(&root.facts(Some(&own), uid, true), true);
        assert_eq!(report.scope_source, ScopeSource::UserManager);
        assert!(!report.ready);
        assert_eq!(report.missing, strings(&["cpu"]));
        assert_eq!(
            report.cause.as_deref(),
            Some(
                format!(
                    "this host's systemd user manager (user@{uid}.service) delegates only memory, \
                     pids to the scopes it creates, and a launch needs memory, pids, cpu"
                )
                .as_str()
            )
        );
        assert_eq!(report.fix.as_deref(), Some(FIX_USER_MANAGER));
    }

    #[test]
    fn a_user_manager_delegating_every_controller_is_ready_and_quiet() {
        let root = FakeRoot::new();
        let uid = uid();
        let manager = format!("/user.slice/user-{uid}.slice/user@{uid}.service");
        let own = format!("{manager}/app.slice/tmux-spawn-1.scope");
        root.file(&manager, "cgroup.controllers", "cpu io memory pids\n")
            .file(&own, "cgroup.controllers", "cpu io memory pids\n");
        let report = diagnose(&root.facts(Some(&own), uid, true), true);
        assert_eq!(report.setup, HostSetup::UserManager);
        assert_eq!(report.unit, Some(format!("user@{uid}.service")));
        assert_eq!(report.scope_source, ScopeSource::UserManager);
        assert!(report.ready);
        assert!(report.missing.is_empty());
        assert_eq!(
            (report.cause.as_deref(), report.fix.as_deref()),
            (None, None)
        );
        let lines = render_launch_host(&report);
        assert_eq!(
            lines[0],
            format!(
                "host setup: systemd user manager (user@{uid}.service) \u{2014} a launch asks the \
                 systemd user manager for a scope; it delegates memory, pids, cpu, io"
            )
        );
        assert!(
            lines[1].starts_with("delegated parent: not adopted \u{2014} "),
            "{lines:?}"
        );
        assert_eq!(lines[2], "open files: 1024 (hard unlimited)");
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    /// The visible root as the delegated parent, `/launcher` the process's cgroup.
    fn delegated_root(root: &FakeRoot, enabled: &str, listed: &str) {
        root.file("/", "cgroup.type", "domain\n")
            .file("/", "cgroup.subtree_control", enabled)
            .file("/launcher", "cgroup.controllers", listed);
    }

    #[test]
    fn a_correct_delegated_parent_is_adopted_and_quiet() {
        let root = FakeRoot::new();
        delegated_root(&root, "memory pids cpu io\n", "memory pids cpu io\n");
        let report = diagnose(&root.facts(Some("/launcher"), uid(), false), true);
        assert_eq!(report.setup, HostSetup::Other);
        assert_eq!(report.cgroup.as_deref(), Some("/launcher"));
        assert_eq!(report.scope_source, ScopeSource::DelegatedParent);
        assert!(report.ready);
        let parent = report.delegated_parent.as_ref().expect("parent");
        assert!(parent.adopts);
        assert_eq!(parent.reason, None);
        assert_eq!(parent.owner, Some(uid()));
        // The visible root shows without a trailing slash.
        assert_eq!(parent.path.as_deref(), Some(root.mount_text().as_str()));
        assert_eq!(
            parent.subtree_control,
            strings(&["memory", "pids", "cpu", "io"])
        );
        assert_eq!(
            (report.cause.as_deref(), report.fix.as_deref()),
            (None, None)
        );
        assert!(report.missing.is_empty());
        let lines = render_launch_host(&report);
        assert_eq!(
            lines,
            vec![
                format!(
                    "host setup: cgroup /launcher \u{2014} a launch adopts the delegated parent {}",
                    root.mount_text()
                ),
                format!(
                    "delegated parent: would be adopted \u{2014} {}",
                    root.mount_text()
                ),
                "open files: 1024 (hard unlimited)".to_string(),
            ]
        );
    }

    #[test]
    fn a_parent_not_owned_by_this_uid_names_the_system_unit_and_its_fix() {
        let root = FakeRoot::new();
        root.file("/system.slice", "cgroup.type", "domain\n")
            .file(
                "/system.slice",
                "cgroup.subtree_control",
                "memory pids cpu io\n",
            )
            .file(
                "/system.slice/fake.service",
                "cgroup.controllers",
                "memory pids cpu io\n",
            );
        let euid = uid() + 1;
        let report = diagnose(
            &root.facts(Some("/system.slice/fake.service"), euid, false),
            false,
        );
        assert_eq!(report.setup, HostSetup::SystemUnit);
        assert_eq!(report.unit.as_deref(), Some("fake.service"));
        assert_eq!(report.scope_source, ScopeSource::None);
        assert!(!report.ready);
        assert!(report.missing.is_empty());
        assert_eq!(report.user_manager, None);
        let parent = report.delegated_parent.as_ref().expect("parent");
        assert_eq!(
            parent.reason.as_deref(),
            Some("parent not owned by this uid")
        );
        assert_eq!(
            report.cause.as_deref(),
            Some(
                format!(
                    "mur runs in the system unit fake.service, whose cgroup is not delegated to \
                     uid {euid} (parent not owned by this uid), and no session bus reaches a \
                     systemd user manager"
                )
                .as_str()
            )
        );
        let mount = root.mount_text();
        assert_eq!(
            report.fix.as_deref(),
            Some(
                format!(
                    "run the launcher in a system unit with Delegate=yes as this user, moved into \
                     a leaf with the controllers enabled for its children (see \
                     https://docs.murmur.nexus/reference/resource-limits/#delegated-parent):\n    \
                     sudo systemd-run --uid=$(id -u) --gid=$(id -g) -p Delegate=yes -p \
                     LimitNOFILE=65536 --wait --collect --pipe --setenv=HOME=$HOME \
                     --setenv=PATH=$PATH sh -c 'U={mount}$(cut -d: -f3 /proc/self/cgroup); mkdir \
                     -p \"$U/launcher\" && echo $$ > \"$U/launcher/cgroup.procs\" && echo \
                     \"+memory +pids +cpu +io\" > \"$U/cgroup.subtree_control\" && exec \"$@\"' sh \
                     <launcher command>"
                )
                .as_str()
            )
        );
    }

    fn fix_parent_for(path: &str) -> String {
        format!(
            "enable the controllers for the children of {path}, with no process left directly in \
             it (see https://docs.murmur.nexus/reference/resource-limits/#delegated-parent):\n    \
             echo \"+memory +pids +cpu +io\" > {path}/cgroup.subtree_control"
        )
    }

    #[test]
    fn a_parent_that_does_not_enable_the_controllers_names_them_and_its_fix() {
        let root = FakeRoot::new();
        delegated_root(&root, "memory pids\n", "memory pids\n");
        let uid = uid();
        let report = diagnose(&root.facts(Some("/launcher"), uid, false), false);
        assert_eq!(report.scope_source, ScopeSource::None);
        assert!(!report.ready);
        assert_eq!(report.missing, strings(&["cpu"]));
        let mount = root.mount_text();
        assert_eq!(
            report.cause.as_deref(),
            Some(
                format!(
                    "the cgroup above mur's ({mount}) is delegated to uid {uid} but does not \
                     enable cpu for its children"
                )
                .as_str()
            )
        );
        assert_eq!(report.fix, Some(fix_parent_for(&mount)));
        assert_eq!(
            report
                .delegated_parent
                .as_ref()
                .and_then(|parent| parent.reason.as_deref()),
            Some("parent does not enable cpu")
        );
    }

    #[test]
    fn a_parent_that_enables_nothing_names_every_controller() {
        let root = FakeRoot::new();
        delegated_root(&root, "", "");
        let uid = uid();
        let report = diagnose(&root.facts(Some("/launcher"), uid, false), false);
        assert_eq!(report.missing, strings(&["memory", "pids", "cpu"]));
        let mount = root.mount_text();
        assert_eq!(
            report.cause,
            Some(format!(
                "the cgroup above mur's ({mount}) is delegated to uid {uid} but does not enable \
                 memory, pids, cpu for its children"
            ))
        );
        assert_eq!(report.fix, Some(fix_parent_for(&mount)));
    }

    /// `io` blocks the parent only when the capsule declares an io limit, and never the own
    /// cgroup: with the parent lacking `io` and the own cgroup listing memory, pids and cpu, the
    /// launch builds under the own cgroup.
    #[test]
    fn a_declared_io_limit_makes_io_blocking_for_the_parent_only() {
        let root = FakeRoot::new();
        delegated_root(&root, "memory pids cpu\n", "memory pids cpu\n");
        let uid = uid();
        let undeclared = diagnose(&root.facts(Some("/launcher"), uid, false), false);
        assert_eq!(undeclared.scope_source, ScopeSource::DelegatedParent);
        let declared = diagnose(&root.facts(Some("/launcher"), uid, false), true);
        assert_eq!(declared.scope_source, ScopeSource::OwnCgroup);
        assert!(declared.ready);
        assert_eq!(declared.cause, None);
        assert_eq!(
            declared
                .delegated_parent
                .as_ref()
                .and_then(|parent| parent.reason.as_deref()),
            Some("parent does not enable io")
        );
        assert_eq!(
            render_launch_host(&declared)[0],
            format!(
                "host setup: cgroup /launcher \u{2014} a launch builds under mur's own cgroup, \
                 delegated to uid {uid}"
            )
        );
    }

    /// With the own cgroup unreadable, nothing else is ready, and the parent's missing `io` is the
    /// cause.
    #[test]
    fn a_parent_without_io_under_a_declared_io_limit_names_io() {
        let root = FakeRoot::new();
        root.file("/", "cgroup.type", "domain\n").file(
            "/",
            "cgroup.subtree_control",
            "memory pids cpu\n",
        );
        let uid = uid();
        let report = diagnose(&root.facts(Some("/launcher"), uid, false), true);
        assert!(!report.ready);
        assert_eq!(report.missing, strings(&["io"]));
        let mount = root.mount_text();
        assert_eq!(
            report.cause,
            Some(format!(
                "the cgroup above mur's ({mount}) is delegated to uid {uid} but does not enable io \
                 for its children"
            ))
        );
        assert_eq!(report.fix, Some(fix_parent_for(&mount)));
    }

    #[test]
    fn an_own_cgroup_that_does_not_list_the_controllers_names_them() {
        let root = FakeRoot::new();
        delegated_root(&root, "memory pids cpu io\n", "memory pids\n");
        let uid = uid();
        let report = diagnose(&root.facts(Some("/launcher"), uid, false), false);
        assert!(!report.ready);
        assert_eq!(report.missing, strings(&["cpu"]));
        let mount = root.mount_text();
        assert_eq!(
            report.cause,
            Some(format!(
                "the cgroup above mur's ({mount}) is delegated to uid {uid} but mur's own cgroup \
                 does not list cpu"
            ))
        );
        assert_eq!(report.fix, Some(fix_parent_for(&mount)));
    }

    #[test]
    fn a_login_session_without_a_session_bus_names_the_missing_bus() {
        let root = FakeRoot::new();
        let own = login_session(&root, Some("cpu io memory pids"));
        let uid = uid();
        let report = diagnose(&root.facts(Some(&own), uid, false), false);
        assert_eq!(report.setup, HostSetup::LoginSession);
        assert_eq!(report.scope_source, ScopeSource::None);
        assert!(!report.ready);
        assert!(report.missing.is_empty());
        assert_eq!(
            report.cause,
            Some(format!(
                "mur runs in login session (session-3.scope) but no session bus reaches the \
                 systemd user manager (DBUS_SESSION_BUS_ADDRESS is unset and $XDG_RUNTIME_DIR/bus \
                 does not exist), and its own cgroup is not delegated to uid {uid}"
            ))
        );
        assert_eq!(
            report.fix.as_deref(),
            Some(
                "start mur from a login that reaches the systemd user manager's session bus (an \
                 ssh or desktop login sets DBUS_SESSION_BUS_ADDRESS or $XDG_RUNTIME_DIR/bus; su \
                 and sudo do not), or run the launcher in a system unit with Delegate=yes \u{2014} \
                 see https://docs.murmur.nexus/reference/resource-limits/#delegated-parent"
            )
        );
    }

    #[test]
    fn a_session_bus_without_a_user_manager_names_linger() {
        let root = FakeRoot::new();
        let own = login_session(&root, None);
        let uid = uid();
        let report = diagnose(&root.facts(Some(&own), uid, true), false);
        assert_eq!(report.scope_source, ScopeSource::None);
        assert_eq!(report.user_manager, None);
        assert!(!report.ready);
        let mount = root.mount_text();
        assert_eq!(
            report.cause,
            Some(format!(
                "no systemd user manager runs for uid {uid} (no \
                 {mount}/user.slice/user-{uid}.slice/user@{uid}.service), so no launch can ask it \
                 for a scope"
            ))
        );
        assert_eq!(
            report.fix.as_deref(),
            Some(
                "start a systemd user manager for this user, then log in again (see \
                 https://docs.murmur.nexus/reference/resource-limits/#platform-behavior):\n    \
                 sudo loginctl enable-linger $(whoami)"
            )
        );
    }

    #[test]
    fn no_cgroup_v2_names_the_boot_option() {
        let root = FakeRoot::new();
        let report = diagnose(&root.facts(None, uid(), true), false);
        assert_eq!(report.setup, HostSetup::NoCgroupV2);
        assert_eq!(report.cgroup, None);
        assert_eq!(report.scope_source, ScopeSource::None);
        assert!(!report.ready);
        assert_eq!(
            report.cause.as_deref(),
            Some("this host has no cgroup v2 unified hierarchy")
        );
        assert_eq!(
            report.fix.as_deref(),
            Some(
                "boot the host with the cgroup v2 unified hierarchy \
                 (systemd.unified_cgroup_hierarchy=1 on the kernel command line) \u{2014} see \
                 https://docs.murmur.nexus/reference/resource-limits/#platform-behavior"
            )
        );
        assert_eq!(
            render_launch_host(&report)[0],
            "host setup: no cgroup v2 unified hierarchy \u{2014} no delegated cgroup is reachable"
        );
    }

    #[test]
    fn an_own_cgroup_owned_and_listing_the_controllers_is_ready_without_a_bus() {
        let root = FakeRoot::new();
        root.file("/srv/mur", "cgroup.controllers", "cpu memory pids\n");
        let report = diagnose(&root.facts(Some("/srv/mur"), uid(), false), false);
        assert_eq!(report.setup, HostSetup::Other);
        assert_eq!(report.scope_source, ScopeSource::OwnCgroup);
        assert!(report.ready);
        assert_eq!((report.cause, report.fix), (None, None));
    }

    #[test]
    fn any_other_cgroup_gets_the_generic_cause_and_fix() {
        let root = FakeRoot::new();
        root.file("/srv", "cgroup.type", "domain\n").file(
            "/srv/mur",
            "cgroup.controllers",
            "memory\n",
        );
        let uid = uid();
        let report = diagnose(&root.facts(Some("/srv/mur"), uid, false), false);
        assert_eq!(report.scope_source, ScopeSource::None);
        assert_eq!(
            report.cause,
            Some(format!(
                "mur's cgroup (/srv/mur) is not delegated to uid {uid}, no delegated parent was \
                 adopted (parent is not a delegation boundary: its parent is also owned by this \
                 uid), and no session bus reaches a systemd user manager"
            ))
        );
        assert_eq!(report.fix.as_deref(), Some(FIX_GENERIC));
    }

    #[test]
    fn the_path_classification_table() {
        let cases: &[(Option<&str>, HostSetup, Option<&str>)] = &[
            (None, HostSetup::NoCgroupV2, None),
            (
                Some("/user.slice/user-1000.slice/user@1000.service/app.slice/tmux.scope"),
                HostSetup::UserManager,
                Some("user@1000.service"),
            ),
            (
                Some(
                    "/user.slice/user-1000.slice/user@1000.service/app.slice/murmur-1-0.scope/\
                     murmur-supervisor",
                ),
                HostSetup::UserManager,
                Some("user@1000.service"),
            ),
            (
                Some("/user.slice/user-1000.slice/session-3.scope"),
                HostSetup::LoginSession,
                Some("session-3.scope"),
            ),
            (
                Some("/user.slice/user-1000.slice/session-c2.scope/murmur-supervisor"),
                HostSetup::LoginSession,
                Some("session-c2.scope"),
            ),
            (
                Some("/system.slice/runner.service"),
                HostSetup::SystemUnit,
                Some("runner.service"),
            ),
            (
                Some("/system.slice/runner.service/murmur-4242-p/murmur-supervisor"),
                HostSetup::SystemUnit,
                Some("runner.service"),
            ),
            (Some("/system.slice"), HostSetup::Other, None),
            (
                Some("/user.slice/user-x.slice/session-3.scope"),
                HostSetup::Other,
                None,
            ),
            (Some("/launcher"), HostSetup::Other, None),
            (Some("/"), HostSetup::Other, None),
        ];
        for (path, setup, unit) in cases {
            assert_eq!(
                classify(*path),
                (*setup, unit.map(str::to_string)),
                "{path:?}"
            );
        }
    }

    #[test]
    fn the_user_manager_is_found_through_the_own_path_or_the_euid() {
        assert_eq!(
            user_manager_path(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope",
                7
            ),
            "/user.slice/user-1000.slice/user@1000.service"
        );
        assert_eq!(
            user_manager_path("/system.slice/runner.service", 1001),
            "/user.slice/user-1001.slice/user@1001.service"
        );
    }

    #[test]
    fn unlimited_and_unreadable_open_files_render_apart() {
        let unlimited = OpenFiles {
            soft: None,
            hard: None,
            read: true,
        };
        assert_eq!(
            render_open_files(&unlimited),
            "open files: unlimited (hard unlimited)"
        );
        assert_eq!(
            render_open_files(&OpenFiles::default()),
            "open files: unknown"
        );
        let json = serde_json::to_value(unlimited).expect("json");
        assert_eq!(json, serde_json::json!({ "soft": null, "hard": null }));
    }

    #[test]
    fn the_report_serializes_with_its_keys_in_order() {
        let root = FakeRoot::new();
        let own = login_session(&root, Some("memory pids"));
        let report = diagnose(&root.facts(Some(&own), uid(), true), false);
        let text = serde_json::to_string(&report).expect("json");
        let keys = [
            "setup",
            "cgroup",
            "unit",
            "euid",
            "scope_source",
            "ready",
            "delegated_parent",
            "own_controllers",
            "session_bus",
            "user_manager",
            "missing",
            "cause",
            "fix",
            "open_files",
        ];
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| text.find(&format!("\"{key}\":")).expect(key))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        let value = serde_json::to_value(&report).expect("json");
        assert_eq!(value["setup"], "login-session");
        assert_eq!(value["scope_source"], "user-manager");
        assert_eq!(value["missing"], serde_json::json!(["cpu"]));
        assert_eq!(
            value["open_files"],
            serde_json::json!({ "soft": 1024, "hard": null })
        );
    }

    /// Every entry under `root` with its modification time.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, SystemTime> {
        let mut entries = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let metadata = std::fs::symlink_metadata(&path).expect("metadata");
            entries.insert(path.clone(), metadata.modified().expect("mtime"));
            if metadata.is_dir() {
                for entry in std::fs::read_dir(&path).expect("read_dir").flatten() {
                    pending.push(entry.path());
                }
            }
        }
        entries
    }

    #[test]
    fn reading_and_diagnosing_change_nothing_in_the_tree() {
        let root = FakeRoot::new();
        let own = login_session(&root, Some("memory"));
        delegated_root(&root, "memory pids\n", "memory pids\n");
        let before = snapshot(&root.mount());
        for (path, bus) in [
            (own.as_str(), true),
            ("/launcher", false),
            ("/gone/x", true),
        ] {
            for declares_io_limit in [false, true] {
                let report = diagnose(&root.facts(Some(path), uid(), bus), declares_io_limit);
                let _ = render_launch_host(&report);
                let _ = refusal_cause_and_fix(&report);
            }
        }
        assert_eq!(snapshot(&root.mount()), before);
    }

    #[test]
    fn refusal_on_a_host_that_is_not_ready_carries_its_cause_and_fix() {
        let root = FakeRoot::new();
        delegated_root(&root, "", "");
        let report = diagnose(&root.facts(Some("/launcher"), uid(), false), false);
        assert_eq!(
            refusal_cause_and_fix(&report),
            (
                report.cause.clone().expect("cause"),
                report.fix.clone().expect("fix")
            )
        );
    }

    #[test]
    fn refusal_on_a_host_that_looks_ready_keeps_the_generic_fix() {
        let root = FakeRoot::new();
        delegated_root(&root, "memory pids cpu io\n", "memory pids cpu io\n");
        let uid = uid();
        let report = diagnose(&root.facts(Some("/launcher"), uid, false), false);
        assert!(report.ready);
        assert_eq!(
            refusal_cause_and_fix(&report),
            (
                format!(
                    "the cgroups mur reads look delegated to uid {uid}, so the operation named \
                     above is what failed"
                ),
                FIX_GENERIC.to_string()
            )
        );
        assert_eq!(
            FIX_GENERIC,
            "the systemd user unit `mur` runs under needs `Delegate=yes` for memory, pids, cpu \
             and io \u{2014} see https://docs.murmur.nexus/reference/resource-limits/#platform-behavior"
        );
    }

    /// `E-RUN-012`'s message for the four acceptance cases and a parent that enables too little:
    /// the operation's reason, then `this host:` and the cause the report found.
    #[test]
    fn refusal_messages_name_this_hosts_cause() {
        let uid = uid();
        let no_delegation = FakeRoot::new();
        let no_delegation_own = login_session(&no_delegation, Some(""));
        let without_cpu = FakeRoot::new();
        let without_cpu_own = login_session(&without_cpu, Some("memory pids"));
        let adopted = FakeRoot::new();
        delegated_root(&adopted, "memory pids cpu io", "memory pids cpu io");
        let not_owned = FakeRoot::new();
        not_owned
            .file("/system.slice", "cgroup.type", "domain\n")
            .file("/system.slice/fake.service", "cgroup.controllers", "");
        let enables_too_little = FakeRoot::new();
        delegated_root(&enables_too_little, "memory pids", "memory pids");
        let cases = [
            (
                no_delegation.facts(Some(&no_delegation_own), uid, true),
                format!(
                    "this host's systemd user manager (user@{uid}.service) delegates no \
                     controllers to the scopes it creates, and a launch needs memory, pids, cpu"
                ),
                FIX_USER_MANAGER.to_string(),
            ),
            (
                without_cpu.facts(Some(&without_cpu_own), uid, true),
                format!(
                    "this host's systemd user manager (user@{uid}.service) delegates only memory, \
                     pids to the scopes it creates, and a launch needs memory, pids, cpu"
                ),
                FIX_USER_MANAGER.to_string(),
            ),
            (
                adopted.facts(Some("/launcher"), uid, false),
                format!(
                    "the cgroups mur reads look delegated to uid {uid}, so the operation named \
                     above is what failed"
                ),
                FIX_GENERIC.to_string(),
            ),
            (
                not_owned.facts(Some("/system.slice/fake.service"), uid + 1, false),
                format!(
                    "mur runs in the system unit fake.service, whose cgroup is not delegated to \
                     uid {} (parent not owned by this uid), and no session bus reaches a systemd \
                     user manager",
                    uid + 1
                ),
                fix_system_unit(&not_owned.mount_text()),
            ),
            (
                enables_too_little.facts(Some("/launcher"), uid, false),
                format!(
                    "the cgroup above mur's ({}) is delegated to uid {uid} but does not enable cpu \
                     for its children",
                    enables_too_little.mount_text()
                ),
                fix_parent_for(&enables_too_little.mount_text()),
            ),
        ];
        for (facts, cause, fix) in cases {
            let (found_cause, found_fix) = refusal_cause_and_fix(&diagnose(&facts, false));
            assert_eq!((&found_cause, &found_fix), (&cause, &fix));
            let reason = "cgroup controllers [cpu] are not delegated to /x".to_string();
            let message = crate::errors::RuntimeError::CgroupDelegationUnavailable {
                reason: reason.clone(),
                cause: found_cause,
                fix: found_fix,
            }
            .to_string();
            assert!(
                message.contains(&format!(
                    "({reason}); this host: {cause}; on Linux the runtime refuses"
                )),
                "{message}"
            );
        }
    }

    #[test]
    fn the_report_off_linux_is_ready_and_says_so() {
        let report = LaunchHostReport::not_linux();
        assert!(report.ready);
        assert_eq!(
            render_launch_host(&report),
            vec!["host setup: not Linux \u{2014} no cgroup scope is needed".to_string()]
        );
        assert_eq!(refusal_cause_and_fix(&report).1, FIX_GENERIC);
    }
}
