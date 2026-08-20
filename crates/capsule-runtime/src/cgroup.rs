//! Linux cgroup v2 scoping for a capsule's whole native-subprocess tree.
//!
//! `resources::apply_hard_rlimits` bounds each spawned process individually. That is necessary
//! and not sufficient: `RLIMIT_NPROC` is a **per-uid** ceiling, so a tree of distinct,
//! short-lived processes that fork and exit faster than the count is observed slips past it,
//! and no rlimit bounds *aggregate* memory or CPU across a tree at all. A cgroup v2 scope does
//! exactly what rlimits structurally cannot: `pids.max`, `memory.max` and `cpu.max` apply to
//! every task in the scope together, and the kernel enforces them at fork/allocate time rather
//! than after the fact.
//!
//! ## Install requirement: systemd user cgroup delegation
//!
//! Creating a cgroup requires write access to a cgroup directory, and this runtime has none by
//! default. The usual alternative — start as root, create the cgroup, then drop privileges —
//! is not available here: nothing in this codebase ever runs as root or drops privileges (there
//! is no setuid or privilege-drop path anywhere in `capsule-runtime` or `murmur-cli`), so there
//! is no privileged phase to create a cgroup from. The mechanism that fits how this runtime
//! actually runs is **systemd user delegation**: the user unit or scope `mur` runs under carries
//! `Delegate=yes` for `memory pids cpu io`, which hands the unprivileged user write access to
//! its own cgroup subtree. This is the same mechanism rootless Docker and Podman use for the
//! identical problem. The exact unit configuration is in
//! `docs/content/reference/resource-limits-manual-verification.md`.
//!
//! ## Self-relocation before probing an inherited cgroup
//!
//! cgroup v2 refuses to enable controllers for a cgroup's children while that cgroup still holds
//! tasks, so a bounded child can only be built under a base this process is the *sole* occupant
//! of. An interactive shell's cgroup holds the shell, its pane scope, and everything else the
//! user is running; under `cargo test`, `cargo` stays resident beside every test binary. Stepping
//! aside into [`SUPERVISOR_LEAF`] moves only this process, so co-residency alone makes the base
//! unusable regardless of delegation.
//!
//! [`move_self_into_own_systemd_scope`] asks the systemd *user* manager over the session bus for
//! a freshly created transient scope carrying `Delegate=yes`, moving this process into a
//! brand-new cgroup — a sibling of whatever it left, not a child — that it is the sole occupant
//! of by construction. The step-aside logic below then always succeeds, since nothing else is
//! ever in that scope to contend with.
//!
//! The request is best-effort and silent: with no systemd, no session bus, or a policy denial,
//! the runtime probes whatever cgroup it inherited, exactly as it does when this step is skipped.
//!
//! ## Fail closed, but only where the threat exists
//!
//! On Linux, a capsule that can spawn *any* native subprocess and cannot be given a cgroup
//! refuses to launch (`RuntimeError::CgroupDelegationUnavailable`) rather than running that
//! subprocess tree unbounded. A capsule that declares no subprocess capability at all needs no
//! scope and is never blocked — there is no process tree to bound. On macOS, cgroups cannot
//! exist at all, so this module is inert and the runtime falls back to rlimits-only enforcement
//! with a `W-SEC-010` warning; refusing every launch on the primary development platform for a
//! kernel feature that platform will never have would be a policy, not a protection.
//!
//! ## Cross-platform shape
//!
//! Following `sandbox.rs`: the types here are defined unconditionally and stay type-checked and
//! unit-testable on macOS, with the *behavior* gated internally by `cfg(target_os = "linux")`
//! and cross-platform-dead surface marked `#[cfg_attr(not(target_os = "linux"), allow(dead_code))]`.
//! Nothing is hidden behind a module-level `cfg`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::resources::HostResourceLimits;

/// Where the unified cgroup v2 hierarchy is mounted on a conventional host. Only used as the
/// fallback when `/proc/mounts` does not name a `cgroup2` mount explicitly.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DEFAULT_CGROUP2_MOUNT: &str = "/sys/fs/cgroup";

/// The `cpu.max` period, in microseconds. The kernel's own default; the quota this module
/// writes is expressed against it (`cgroup_cpu_percent * 1000` µs of every 100 000 µs).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CPU_MAX_PERIOD_US: u64 = 100_000;

/// Controllers that must be delegated and settable. `io` is deliberately absent: it is
/// best-effort (see [`CgroupScope::apply_io_max`]).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const REQUIRED_CONTROLLERS: &[&str] = &["memory", "pids", "cpu"];

/// Name of the leaf cgroup the runtime moves *itself* into when its delegated base still holds
/// processes. cgroup v2's "no internal processes" rule forbids enabling controllers in a
/// cgroup's `subtree_control` while that cgroup directly contains tasks, so the manager has to
/// step down into its own leaf first — the same move rootless Podman/Docker make.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SUPERVISOR_LEAF: &str = "murmur-supervisor";

/// A cgroup v2 scope created for one capsule session, holding that session's whole native
/// subprocess tree.
///
/// Constructed once per launch (never per subprocess) so that `pids.max` and `memory.max` bound
/// the tree *in aggregate* — a per-call scope would let N sequential shell calls each spend the
/// full budget, which is precisely the aggregate case rlimits already fail to cover.
#[derive(Debug)]
pub(crate) struct CgroupScope {
    /// Absolute path of the scope directory, e.g.
    /// `/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/mur.scope/murmur-ses_abc123`.
    path: PathBuf,
    /// `cgroup.procs`, opened write-only in the **parent** before any fork. The forked child's
    /// `pre_exec` closure only ever writes its own pid to this already-open descriptor — it
    /// never performs a path lookup, matching how `sandbox.rs` pre-opens its Landlock fds rather
    /// than opening paths inside `pre_exec`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    procs_fd: Option<std::os::fd::OwnedFd>,
}

/// Snapshot of the kernel-maintained kill/denial counters on a scope.
///
/// Read before and after a subprocess runs, so attribution rests on the *delta* for that call
/// rather than on a session-cumulative total that any earlier call could have moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CgroupEventCounters {
    /// `memory.events`' `oom_kill` — tasks the kernel killed for exceeding `memory.max`.
    pub(crate) oom_kill: u64,
    /// `pids.events`' `max` — forks refused for exceeding `pids.max`.
    pub(crate) pids_max: u64,
}

impl CgroupEventCounters {
    /// The named limit this delta unambiguously attributes to, or `None`.
    ///
    /// Both counters are maintained by the kernel and incremented only by the condition they
    /// name, so a nonzero delta is evidence rather than inference — the property that makes
    /// these two safe to report where a bare `SIGKILL` is not (see
    /// [`crate::resources::limit_from_signal`]).
    pub(crate) fn attribution_since(&self, before: Self) -> Option<&'static str> {
        if self.oom_kill > before.oom_kill {
            return Some("cgroup_memory_bytes");
        }
        if self.pids_max > before.pids_max {
            return Some("cgroup_pids_max");
        }
        None
    }
}

/// Decide-and-create entry point, called once per launch before any WASM is instantiated.
///
/// * `required == false` (the capsule declares no way to spawn a native subprocess) → `Ok(None)`
///   on every platform. There is no process tree to bound, so demanding host configuration here
///   would be a regression for WASM-only capsules with nothing to show for it.
/// * non-Linux → `Ok(None)` always. Never an error: cgroups are structurally impossible there,
///   not a host misconfiguration.
/// * Linux and required → probe delegation and create the scope; `Err(reason)` if the host
///   cannot delegate one, which the caller turns into
///   [`crate::errors::RuntimeError::CgroupDelegationUnavailable`].
pub(crate) fn prepare_scope(
    required: bool,
    limits: &HostResourceLimits,
    session_id: &str,
    workdir: &Path,
) -> Result<Option<Arc<CgroupScope>>, String> {
    if !required {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        let scope = CgroupScope::create(limits, session_id, workdir)?;
        Ok(Some(Arc::new(scope)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (limits, session_id, workdir);
        Ok(None)
    }
}

/// Does this policy give the capsule any way to run a native subprocess at all?
///
/// The exact condition the Linux fail-closed launch refusal keys on: `capabilities.shell.allow`,
/// `capabilities.spawn.allow`, or a declared artifact whose implementation is native (passed in
/// by the caller, which is the only place the installed-artifact list is in hand).
pub(crate) fn requires_process_bounding(
    policy: &crate::types::CapabilityPolicy,
    has_native_artifact: bool,
) -> bool {
    !policy.shell_allow.is_empty() || !policy.spawn_allow.is_empty() || has_native_artifact
}

impl CgroupScope {
    /// Absolute path of this scope's directory.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Move the **calling** process into this scope.
    ///
    /// Called from inside the forked child's `pre_exec` closure: it writes the child's own pid
    /// to the parent-opened `cgroup.procs` descriptor with a single `write(2)`, formatting the
    /// pid into a stack buffer so the whole path stays allocation-free and async-signal-safe.
    /// Every process the child subsequently forks inherits the scope, which is what makes the
    /// bound apply to the *tree* rather than to one process.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    #[allow(unsafe_code)]
    pub(crate) fn join_current_process(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        let Some(fd) = self.procs_fd.as_ref() else {
            return Ok(());
        };

        // SAFETY: `getpid` takes no arguments and cannot fail.
        let pid = unsafe { libc::getpid() };
        let mut buf = [0u8; 24];
        let written = format_pid(pid, &mut buf);

        let mut offset = 0;
        while offset < written {
            // SAFETY: `fd` is an open write-only descriptor owned by `self`, and the slice
            // passed is a live sub-range of the stack buffer above.
            let rc = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    buf[offset..written].as_ptr().cast::<libc::c_void>(),
                    written - offset,
                )
            };
            if rc < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            offset += rc as usize;
        }
        Ok(())
    }

    /// Current `memory.events`/`pids.events` counters.
    ///
    /// Must be read while the scope directory still exists — the files vanish with it, which is
    /// why cleanup happens in `Drop` at session end and never right after a child is reaped.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn event_counters(&self) -> CgroupEventCounters {
        CgroupEventCounters {
            oom_kill: read_event_counter(&self.path.join("memory.events"), "oom_kill"),
            pids_max: read_event_counter(&self.path.join("pids.events"), "max"),
        }
    }
}

impl Drop for CgroupScope {
    /// Best-effort `rmdir` of the scope directory.
    ///
    /// A cgroup directory can only be removed once empty, so a lingering grandchild the runtime
    /// never reaped keeps it alive. That is logged and moved past rather than retried in a loop:
    /// blocking session teardown on a process the runtime does not control would turn a leaked
    /// directory into a hung exit.
    fn drop(&mut self) {
        // Drop the descriptor before the rmdir so nothing holds the directory open.
        self.procs_fd = None;
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Err(error) = std::fs::remove_dir(&self.path) {
            eprintln!(
                "[capsule-runtime] note: could not remove cgroup scope {} ({error}); \
                 it will be reclaimed once its last task exits",
                self.path.display()
            );
        }
    }
}

/// Format `pid` into `buf` as decimal ASCII followed by a newline, returning the byte count.
///
/// Hand-rolled because [`CgroupScope::join_current_process`] runs between `fork` and `execve`,
/// where `format!` (and the allocator lock behind it) is not async-signal-safe.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn format_pid(pid: i32, buf: &mut [u8; 24]) -> usize {
    let mut digits = [0u8; 20];
    let mut count = 0;
    let mut value = pid.unsigned_abs() as u64;
    if value == 0 {
        digits[0] = b'0';
        count = 1;
    }
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }

    let mut written = 0;
    for index in (0..count).rev() {
        buf[written] = digits[index];
        written += 1;
    }
    buf[written] = b'\n';
    written + 1
}

/// Read one `<key> <value>` counter out of a cgroup `*.events` file, or `0` if the file or key
/// is absent. Absence is not an error: `memory.events` only exists once the memory controller is
/// enabled, and treating a missing counter as zero means attribution simply declines to name a
/// limit rather than inventing one.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn read_event_counter(path: &Path, key: &str) -> u64 {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return 0;
    };
    parse_event_counter(&contents, key)
}

/// Pure parse half of [`read_event_counter`] — no syscalls, so it is unit-testable on macOS.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_event_counter(contents: &str, key: &str) -> u64 {
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            return parts.next().and_then(|value| value.parse().ok()).unwrap_or(0);
        }
    }
    0
}

/// Turn a session id into a directory name safe to create under the delegated base: everything
/// outside `[A-Za-z0-9._-]` becomes `_`.
///
/// The name is also made unique per scope, with the pid and a per-process serial. The id alone
/// cannot carry that: `plan::execute` passes the plan's *authored* id, so two plans running
/// concurrently in one process would otherwise land on the same directory — and because
/// [`CgroupScope`]'s `Drop` removes the scope by path, the first to finish would delete a cgroup
/// the other is still bounded by, leaving that subprocess tree unbounded. Sanitizing stays a
/// belt-and-braces guard against a hostile id rather than a live escape vector.
///
/// Operators still find scopes by the documented `murmur-*` glob.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn scope_dir_name(session_id: &str) -> String {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let sanitized: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let pid = std::process::id();
    if sanitized.is_empty() {
        format!("murmur-{pid}-{serial}")
    } else {
        format!("murmur-{sanitized}-{pid}-{serial}")
    }
}

/// Extract the unified-hierarchy path from `/proc/self/cgroup` contents.
///
/// A cgroup v2 host writes exactly one line whose hierarchy id is `0` and whose controller list
/// is empty: `0::/user.slice/...`. Returning `None` for anything else is what makes a v1-only
/// (or hybrid, v2-less) host a clean "cannot delegate" rather than a wrong path.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_unified_cgroup_path(contents: &str) -> Option<String> {
    for line in contents.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some("0"), Some(""), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if path.starts_with('/') {
            return Some(path.to_string());
        }
    }
    None
}

/// Find the `cgroup2` mount point in `/proc/mounts` contents, falling back to the conventional
/// `/sys/fs/cgroup`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cgroup2_mount(contents: &str) -> String {
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let (Some(_source), Some(target), Some(fstype)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if fstype == "cgroup2" {
            return target.to_string();
        }
    }
    DEFAULT_CGROUP2_MOUNT.to_string()
}

/// Which of `wanted` are missing from a `cgroup.subtree_control` file's contents.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn missing_controllers(subtree_control: &str, wanted: &[&str]) -> Vec<String> {
    let enabled: Vec<&str> = subtree_control.split_whitespace().collect();
    wanted
        .iter()
        .filter(|controller| !enabled.contains(*controller))
        .map(|controller| (*controller).to_string())
        .collect()
}

/// Encode a `st_dev` value as the `MAJ:MIN` string `io.max` expects, using glibc's device-number
/// layout (12+20 bit split, not the legacy 8+8).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn device_major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

#[cfg(target_os = "linux")]
impl CgroupScope {
    /// Probe delegation, enable the controllers, create the scope directory, and write its
    /// limits. Any failure to establish `memory.max`, `pids.max` or `cpu.max` is fatal — those
    /// three are settable on any cgroup v2 host once the controllers are delegated, so a failure
    /// there means the bound genuinely does not exist and the launch must not proceed.
    fn create(
        limits: &HostResourceLimits,
        session_id: &str,
        workdir: &Path,
    ) -> Result<Self, String> {
        let base = delegated_base()?;

        let path = base.join(scope_dir_name(session_id));
        if let Err(error) = std::fs::create_dir(&path) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "could not create cgroup scope at {}: {error}",
                    path.display()
                ));
            }
        }

        let scope = Self {
            path,
            procs_fd: None,
        };

        scope.write_limit("memory.max", &limits.cgroup_memory_bytes.to_string())?;
        scope.write_limit("pids.max", &limits.cgroup_pids_max.to_string())?;
        scope.write_limit(
            "cpu.max",
            &format!(
                "{} {CPU_MAX_PERIOD_US}",
                u64::from(limits.cgroup_cpu_percent) * (CPU_MAX_PERIOD_US / 100)
            ),
        )?;
        scope.apply_io_max(limits.cgroup_io_bytes_per_sec, workdir);

        // Opened last, so a scope that failed any of the fatal writes above is never handed out
        // with a usable join descriptor.
        let procs_fd = open_write_only(&scope.path.join("cgroup.procs")).map_err(|error| {
            format!(
                "could not open {}/cgroup.procs for subprocess placement: {error}",
                scope.path.display()
            )
        })?;

        Ok(Self {
            path: scope.take_path(),
            procs_fd: Some(procs_fd),
        })
    }

    /// Move the path out of a scope that must not run its `Drop` (which would `rmdir` the
    /// directory the returned value is about to take ownership of).
    fn take_path(mut self) -> PathBuf {
        let path = std::mem::take(&mut self.path);
        // `self.path` is now empty, which `Drop` treats as "nothing to remove".
        path
    }

    fn write_limit(&self, file: &str, value: &str) -> Result<(), String> {
        let path = self.path.join(file);
        std::fs::write(&path, format!("{value}\n")).map_err(|error| {
            format!(
                "could not write {} to {}: {error}",
                value,
                path.display()
            )
        })
    }

    /// Best-effort `io.max` on the workdir's backing block device.
    ///
    /// Non-fatal by design, and the only one of the four controllers treated that way: the
    /// backing device of a path cannot always be resolved to a real `MAJ:MIN` the block layer
    /// accepts (overlayfs, tmpfs, btrfs subvolumes and device-mapper stacks all break the
    /// assumption), and I/O throughput is the least safety-critical of the four — a capsule that
    /// saturates disk bandwidth is slow, where one that exhausts memory or pids is fatal.
    fn apply_io_max(&self, bytes_per_sec: u64, workdir: &Path) {
        use std::os::linux::fs::MetadataExt;

        let Ok(metadata) = workdir.metadata() else {
            eprintln!(
                "[capsule-runtime] note: cgroup io.max not applied — could not stat workdir {}",
                workdir.display()
            );
            return;
        };
        let (major, minor) = device_major_minor(metadata.st_dev());
        let value = format!("{major}:{minor} rbps={bytes_per_sec} wbps={bytes_per_sec}");
        if let Err(error) = std::fs::write(self.path.join("io.max"), format!("{value}\n")) {
            eprintln!(
                "[capsule-runtime] note: cgroup io.max not applied ({error}); memory.max, \
                 pids.max and cpu.max are still enforced on this scope"
            );
        }
    }
}

/// The delegated cgroup root this process creates its scopes under, resolved once.
///
/// [`probe_delegation`] derives the root from `/proc/self/cgroup`, but establishing the first
/// scope may move this process into `<root>/murmur-supervisor` to vacate the root — cgroup v2
/// forbids a cgroup from both holding processes and enabling controllers for its children. Once
/// that move has happened the process's own cgroup is no longer the delegated root, so
/// re-deriving it per call mistakes the leaf for the root and nests one level deeper every time,
/// until the nested leaf has no delegated controllers at all and every further session is
/// refused. Any process that opens a second session — concurrently or in sequence — hits this.
///
/// Resolving once, under a lock, keeps every scope a sibling directly under the real root, and
/// makes concurrent first-callers agree on that root instead of racing to relocate each other.
/// Only success is cached: a host that is not delegated must keep reporting so on every attempt.
#[cfg(target_os = "linux")]
fn delegated_base() -> Result<PathBuf, String> {
    static BASE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

    // A poisoned lock guards no invariant worth honouring here: the cache is either populated or
    // it is not, and a caller that panicked cannot have left it half-written.
    let mut cached = BASE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(base) = cached.as_ref() {
        return Ok(base.clone());
    }

    // Best-effort: relocate into our own scope before probing. On failure, the probe below runs
    // against the inherited cgroup — the correct fallback on a host with no systemd to ask.
    move_self_into_own_systemd_scope();

    let base = probe_delegation()?;
    enable_controllers(&base)?;
    *cached = Some(base.clone());
    Ok(base)
}

/// Ask the systemd user manager for a freshly created, delegated transient scope holding this
/// process, so the cgroup this runtime builds under is one it is the only occupant of.
///
/// Best-effort and non-fatal: every failure here (no systemd, no session bus, policy denial, a
/// migration that never lands) leaves the process exactly where it was — the state
/// [`probe_delegation`] handles. Never let a failed optimisation become a launch refusal.
///
/// Runs at most once per process — a scope is a property of the process, not the session, and
/// [`delegated_base`] retries on failure but caches only success, so the guard has to live here.
#[cfg(target_os = "linux")]
fn move_self_into_own_systemd_scope() {
    static ATTEMPTED: std::sync::Once = std::sync::Once::new();
    ATTEMPTED.call_once(|| {
        let _ = request_transient_scope(&transient_scope_unit_name());
    });
}

/// Name of the transient scope unit to ask systemd for.
///
/// `murmur-` keeps the operator-facing convention the scope *directories* already use
/// ([`scope_dir_name`]), so `systemctl --user list-units 'murmur-*.scope'` finds the runtime's
/// units the same way a `murmur-*` glob finds its cgroup directories. The pid and a per-process
/// serial keep it unique against a recycled pid whose earlier unit systemd has not yet collected.
#[cfg(target_os = "linux")]
fn transient_scope_unit_name() -> String {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("murmur-{}-{serial}.scope", std::process::id())
}

/// The D-Bus half of [`move_self_into_own_systemd_scope`], plus confirmation that the kernel
/// actually moved us.
#[cfg(target_os = "linux")]
fn request_transient_scope(unit: &str) -> Result<(), String> {
    // zbus's blocking API drives its own executor; run it off-thread so it never blocks a
    // caller already inside another async runtime (e.g. tokio) on a nested reactor.
    let owned_unit = unit.to_string();
    std::thread::Builder::new()
        .name("murmur-cgroup-scope".to_string())
        .spawn(move || start_transient_scope(&owned_unit))
        .map_err(|error| format!("could not spawn the D-Bus thread: {error}"))?
        .join()
        .map_err(|_| "the D-Bus thread panicked".to_string())??;

    await_scope_membership(unit)
}

/// `StartTransientUnit` on the systemd user manager: create `unit` as a scope containing this
/// process, with its cgroup subtree delegated to us.
///
/// The call is
/// `org.freedesktop.systemd1.Manager.StartTransientUnit(in s name, in s mode, in a(sv) properties,
/// in a(sa(sv)) aux)` on `/org/freedesktop/systemd1` at the well-known name
/// `org.freedesktop.systemd1`, over the **session** bus (`$DBUS_SESSION_BUS_ADDRESS`, falling back
/// to `$XDG_RUNTIME_DIR/bus`) so the unit lands in this user's manager and needs no privilege.
/// The properties are the same set `systemd-run --user --scope -p Delegate=yes` sends:
///
/// * `PIDs` (`au`) — this process, which is what makes systemd *move* it rather than fork a new one.
/// * `Delegate` (`b`) — hand the scope's subtree to us, controllers and all, and stop managing it.
/// * `CollectMode` (`s`) — `inactive-or-failed`, so the unit is garbage-collected when this
///   process exits rather than lingering as a failed unit an operator has to reset.
/// * `Description` (`s`) — what `systemctl --user list-units` shows beside the unit.
#[cfg(target_os = "linux")]
fn start_transient_scope(unit: &str) -> Result<(), String> {
    use zbus::zvariant::Value;

    let connection = zbus::blocking::Connection::session()
        .map_err(|error| format!("no systemd session bus: {error}"))?;

    let properties: Vec<(&str, Value<'_>)> = vec![
        ("Description", Value::from("Murmur capsule subprocess scope")),
        ("PIDs", Value::from(vec![std::process::id()])),
        ("Delegate", Value::from(true)),
        ("CollectMode", Value::from("inactive-or-failed")),
    ];
    // No auxiliary units: a scope has none.
    let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = Vec::new();

    connection
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "StartTransientUnit",
            // "fail" rather than "replace": a name collision means something is wrong with our
            // assumption of uniqueness, and displacing an unrelated unit is never the fix.
            &(unit, "fail", properties, aux),
        )
        .map(|_| ())
        .map_err(|error| format!("systemd refused to start {unit}: {error}"))
}

/// Wait until `/proc/self/cgroup` actually reports us inside `unit`'s cgroup.
///
/// `StartTransientUnit` returns once the job is *enqueued*, not once the move has happened, and
/// every later step (the delegation probe, the controller enable, the scope directory) depends on
/// where this process actually lives — so migration is confirmed from `/proc/self/cgroup`, the
/// same source the rest of this module reads, rather than inferred from a job object path.
///
/// The 2s deadline in 20ms steps mirrors [`enable_controllers`]: both wait on systemd/kernel
/// bookkeeping that normally completes in a millisecond or two, and a wait that long means
/// something structural is wrong. A timeout here is cheap — the caller ignores it and falls back
/// to probing the inherited cgroup.
#[cfg(target_os = "linux")]
fn await_scope_membership(unit: &str) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let current = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        if let Some(path) = parse_unified_cgroup_path(&current) {
            if path.rsplit('/').next() == Some(unit) {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "systemd accepted the request for {unit} but this process is not in it"
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Locate the delegated cgroup v2 directory this process may create children under, confirming
/// the controllers this runtime needs are actually available there.
///
/// Returns the base directory on success, or a message naming what is missing — that message is
/// what reaches the operator inside
/// [`crate::errors::RuntimeError::CgroupDelegationUnavailable`], so it names the delegation
/// requirement rather than only the failed syscall.
#[cfg(target_os = "linux")]
fn probe_delegation() -> Result<PathBuf, String> {
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|error| format!("could not read /proc/self/cgroup: {error}"))?;
    let relative = parse_unified_cgroup_path(&self_cgroup).ok_or_else(|| {
        "this host has no cgroup v2 unified hierarchy (no `0::` line in /proc/self/cgroup); \
         cgroup v1-only and hybrid hosts cannot be delegated a v2 scope"
            .to_string()
    })?;

    let mount = std::fs::read_to_string("/proc/mounts")
        .map(|contents| parse_cgroup2_mount(&contents))
        .unwrap_or_else(|_| DEFAULT_CGROUP2_MOUNT.to_string());

    let base = ascend_out_of_supervisor_leaves(PathBuf::from(mount).join(relative.trim_start_matches('/')));
    if !base.is_dir() {
        return Err(format!(
            "delegated cgroup directory {} does not exist",
            base.display()
        ));
    }

    let controllers = std::fs::read_to_string(base.join("cgroup.controllers")).map_err(|error| {
        format!(
            "could not read {}/cgroup.controllers: {error}",
            base.display()
        )
    })?;
    let unavailable = missing_controllers(&controllers, REQUIRED_CONTROLLERS);
    if !unavailable.is_empty() {
        return Err(format!(
            "cgroup controllers [{}] are not delegated to {}; the unit `mur` runs under needs \
             `Delegate=yes` (or `Delegate=memory pids cpu io`)",
            unavailable.join(", "),
            base.display()
        ));
    }

    // The probe must confirm we can actually *create* a child here, not just read the directory:
    // a readable cgroup with no write permission is the exact shape of an undelegated host.
    // Named per-pid so two `mur` processes probing concurrently never race on the same path.
    let probe_dir = base.join(format!("murmur-delegation-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir(&probe_dir);
    std::fs::create_dir(&probe_dir).map_err(|error| {
        format!(
            "cannot create a child cgroup under {} ({error}); the unit `mur` runs under needs \
             `Delegate=yes` for memory, pids, cpu and io",
            base.display()
        )
    })?;
    let _ = std::fs::remove_dir(&probe_dir);

    Ok(base)
}

/// Enable `memory`, `pids`, `cpu` (and `io`, best-effort) in the delegated base's
/// `cgroup.subtree_control` so a child cgroup can carry their limit files.
///
/// If the base still holds this process directly, the write is refused (`EBUSY`) by cgroup v2's
/// "no internal processes" rule; the recovery is to move this process into its own leaf and try
/// once more. That relocation stays entirely inside the delegated subtree.
#[cfg(target_os = "linux")]
fn enable_controllers(base: &Path) -> Result<(), String> {
    let subtree_control = base.join("cgroup.subtree_control");
    let current = std::fs::read_to_string(&subtree_control).unwrap_or_default();

    // `io` is requested opportunistically: a host that delegates only the three required
    // controllers still gets a fully-enforced scope, just without `io.max`.
    let available = std::fs::read_to_string(base.join("cgroup.controllers")).unwrap_or_default();
    let mut wanted: Vec<&str> = REQUIRED_CONTROLLERS.to_vec();
    if available.split_whitespace().any(|c| c == "io") {
        wanted.push("io");
    }

    let missing = missing_controllers(&current, &wanted);
    if missing.is_empty() {
        return Ok(());
    }
    let directive = missing
        .iter()
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" ");

    if std::fs::write(&subtree_control, format!("{directive}\n")).is_ok() {
        return Ok(());
    }

    move_self_to_supervisor_leaf(base)?;

    // Stepping aside vacates *this* process, and cgroup v2 refuses the write while the base holds
    // any task at all. When several murmur processes share a base — a test binary and the `mur`
    // children it spawns, or two capsules launched together — each is independently discovering
    // it must step aside, so the first write after our own move can still hit `EBUSY` on peers
    // that have not moved yet. Retrying briefly lets the group finish draining; a peer that wins
    // the race and enables the controllers first is just as good, which is why the loop re-reads
    // `subtree_control` rather than only retrying the write.
    //
    // Bounded, because a task that never leaves is a genuine misconfiguration and has to surface
    // as one rather than hang the launch.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut last_error = None;
    loop {
        let current = std::fs::read_to_string(&subtree_control).unwrap_or_default();
        if missing_controllers(&current, &wanted).is_empty() {
            return Ok(());
        }
        match std::fs::write(&subtree_control, format!("{directive}\n")) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    Err(format!(
        "could not enable controllers [{}] in {} ({}); the unit `mur` runs under needs \
         `Delegate=yes` for memory, pids, cpu and io",
        missing.join(", "),
        subtree_control.display(),
        last_error.map_or_else(|| "still occupied".to_string(), |error| error.to_string()),
    ))
}

/// Walk up out of any `murmur-supervisor` leaves the path ends in.
///
/// The leaf is where a murmur process steps aside so controllers can be enabled on the delegated
/// base. Every process it then spawns *inherits that leaf as its own cgroup* — so a `mur`
/// subprocess reading `/proc/self/cgroup` would take the leaf for its delegated base and step
/// aside again inside it, one level deeper per generation, until the innermost leaf has no
/// controllers delegated to it at all and every launch below that point is refused.
///
/// Ascending here makes every generation agree on the same real base, which is also what lets
/// them share one scope parent rather than fragmenting the delegation.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn ascend_out_of_supervisor_leaves(mut base: PathBuf) -> PathBuf {
    while base.file_name().and_then(|name| name.to_str()) == Some(SUPERVISOR_LEAF) {
        base.pop();
    }
    base
}

/// Move this process into `<base>/murmur-supervisor`, creating it if needed.
#[cfg(target_os = "linux")]
fn move_self_to_supervisor_leaf(base: &Path) -> Result<(), String> {
    let leaf = base.join(SUPERVISOR_LEAF);
    if let Err(error) = std::fs::create_dir(&leaf) {
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!(
                "could not create {} to vacate the delegated cgroup: {error}",
                leaf.display()
            ));
        }
    }
    std::fs::write(leaf.join("cgroup.procs"), format!("{}\n", std::process::id())).map_err(
        |error| {
            format!(
                "could not move this process into {} to vacate the delegated cgroup: {error}",
                leaf.display()
            )
        },
    )
}

/// `open(path, O_WRONLY | O_CLOEXEC)`.
///
/// `CLOEXEC` matters twice over: the descriptor is written from `pre_exec` (still open at that
/// point) and must not survive into the exec'd binary, where it would hand a capsule the ability
/// to move arbitrary pids into its own cgroup.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn open_write_only(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
        .map(std::os::fd::OwnedFd::from)
}

/// Whether this host can hand the runtime a delegated cgroup v2 scope — the precondition
/// [`prepare_scope`] fails closed on.
///
/// Test support, not a runtime code path: `mur` never asks this question, it asks systemd for a
/// scope and reports [`crate::errors::RuntimeError::CgroupDelegationUnavailable`] when it cannot
/// get one. Tests that launch a capsule tripping [`requires_process_bounding`] call this to skip
/// visibly instead, because a containerised runner has no delegable subtree to give and the
/// refusal there says nothing about the code under test. What CI therefore cannot cover is
/// covered by hand: `docs/content/reference/resource-limits-manual-verification.md`.
///
/// Probed by actually creating a transient `--user` scope with `Delegate=yes`, since a
/// `systemd-run` binary on `PATH` says nothing about a reachable user manager — and the user
/// manager is the half a runner is missing. Off Linux there are no cgroups, this module is inert
/// and nothing is ever refused, so the answer is `true`.
pub fn cgroup_delegation_available() -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        true
    }

    #[cfg(target_os = "linux")]
    {
        // Answered through `delegated_base`, the same call `prepare_scope` makes, so the answer
        // cannot disagree with what a launch does on this host. A separate `systemd-run` probe
        // answers a different question and is wrong in both directions: a user manager hands out
        // a scope on a host whose controllers `enable_controllers` cannot delegate, and the
        // inherited cgroup carries delegated controllers on a host with no reachable session bus.
        delegated_base().is_ok()
    }
}

/// Whether a test that launches a subprocess-capable capsule must stand down on this host,
/// printing the blocker when it must.
///
/// Both blockers are asked through the same calls a launch makes -- `delegated_base` for the
/// cgroup scope, `detect_egress_namespace_blocker` for the network namespace -- so the gate and
/// the launch cannot reach different conclusions about the host. A containerised CI runner
/// typically clears the first and fails the second.
pub fn skip_without_host_support(test_name: &str) -> bool {
    if let Some(blocker) = crate::network_namespace::detect_egress_namespace_blocker() {
        eprintln!(
            "[SKIP-HOST] {test_name}: {}",
            blocker.reason()
        );
        return true;
    }
    if !cgroup_delegation_available() {
        eprintln!(
            "[SKIP-HOST] {test_name}: this host cannot delegate a cgroup v2 scope, so a capsule \
             that can spawn native subprocesses refuses to launch with E-RUN-012 before anything \
             this test observes happens -- see \
             docs/content/reference/resource-limits-manual-verification.md"
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CapabilityPolicy;

    #[test]
    fn scope_is_required_by_any_route_to_a_native_subprocess() {
        let shell = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let spawn = CapabilityPolicy {
            spawn_allow: vec!["child".to_string()],
            ..CapabilityPolicy::default()
        };
        let bare = CapabilityPolicy::default();

        assert!(requires_process_bounding(&shell, false));
        assert!(requires_process_bounding(&spawn, false));
        assert!(requires_process_bounding(&bare, true), "a native artifact alone is enough");
        assert!(
            !requires_process_bounding(&bare, false),
            "a WASM-only capsule has no process tree to bound and must not be blocked"
        );
    }

    /// A capsule with no subprocess capability never needs delegation — on any platform, and
    /// notably including Linux, where a blanket requirement would be a disproportionate
    /// regression for WASM-only capsules.
    #[test]
    fn prepare_scope_returns_none_when_bounding_is_not_required() {
        let temp = tempfile::tempdir().unwrap();
        let scope = prepare_scope(
            false,
            &HostResourceLimits::default(),
            "ses_test",
            temp.path(),
        )
        .expect("a capsule with no subprocess capability must never be refused");
        assert!(scope.is_none());
    }

    /// macOS can never have a cgroup, so a required scope there must be a silent `None` rather
    /// than a launch-blocking error.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn prepare_scope_is_never_an_error_off_linux() {
        let temp = tempfile::tempdir().unwrap();
        let scope = prepare_scope(true, &HostResourceLimits::default(), "ses_test", temp.path())
            .expect("a non-Linux host must not refuse to launch for a missing cgroup");
        assert!(scope.is_none());
    }

    /// The point of asking systemd for a scope: a Linux host with a working user session hands
    /// out a bounded scope no matter what this process's *inherited* cgroup looked like — an
    /// interactive shell's cgroup full of co-resident processes, or the one `cargo test` keeps
    /// itself resident in while running this very binary.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_required_scope_is_created_whatever_cgroup_this_process_inherited() {
        if skip_without_host_support(
            "a_required_scope_is_created_whatever_cgroup_this_process_inherited",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let scope = prepare_scope(true, &HostResourceLimits::default(), "ses_selftest", temp.path())
            .expect("a Linux host with a systemd user session must be able to bound a subprocess tree")
            .expect("a required scope on Linux is never `None`");

        for file in ["memory.max", "pids.max", "cpu.max"] {
            let contents = std::fs::read_to_string(scope.path().join(file))
                .unwrap_or_else(|error| panic!("{file} unreadable in the scope: {error}"));
            assert!(!contents.trim().is_empty(), "{file} was left unset");
            assert_ne!(contents.trim(), "max", "{file} carries no ceiling");
        }
    }

    /// The unit name systemd is asked for stays inside the `murmur-*` convention operators
    /// already use to find this runtime's cgroup directories, and stays unique per request.
    #[cfg(target_os = "linux")]
    #[test]
    fn transient_scope_unit_names_are_murmur_prefixed_and_unique() {
        let first = transient_scope_unit_name();
        let second = transient_scope_unit_name();
        assert!(first.starts_with(&format!("murmur-{}-", std::process::id())));
        assert!(first.ends_with(".scope"));
        assert_ne!(first, second);
    }

    #[test]
    fn unified_cgroup_path_is_read_from_the_zero_hierarchy_line() {
        let v2_only = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/mur.scope\n";
        assert_eq!(
            parse_unified_cgroup_path(v2_only).as_deref(),
            Some("/user.slice/user-1000.slice/user@1000.service/app.slice/mur.scope")
        );

        let hybrid = "12:pids:/user.slice\n11:memory:/user.slice\n0::/user.slice/session.scope\n";
        assert_eq!(
            parse_unified_cgroup_path(hybrid).as_deref(),
            Some("/user.slice/session.scope")
        );

        let v1_only = "12:pids:/user.slice\n11:memory:/user.slice\n";
        assert_eq!(parse_unified_cgroup_path(v1_only), None);
    }

    #[test]
    fn cgroup2_mount_falls_back_to_the_conventional_path() {
        let mounts = "cgroup2 /custom/cgroup cgroup2 rw,nsdelegate 0 0\n";
        assert_eq!(parse_cgroup2_mount(mounts), "/custom/cgroup");
        assert_eq!(parse_cgroup2_mount("proc /proc proc rw 0 0\n"), "/sys/fs/cgroup");
    }

    #[test]
    fn missing_controllers_reports_only_what_is_absent() {
        assert_eq!(
            missing_controllers("cpu memory", REQUIRED_CONTROLLERS),
            vec!["pids".to_string()]
        );
        assert!(missing_controllers("cpu io memory pids", REQUIRED_CONTROLLERS).is_empty());
        assert_eq!(
            missing_controllers("", REQUIRED_CONTROLLERS),
            vec!["memory".to_string(), "pids".to_string(), "cpu".to_string()]
        );
    }

    #[test]
    fn event_counters_parse_the_named_key_only() {
        let memory_events = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\n";
        assert_eq!(parse_event_counter(memory_events, "oom_kill"), 2);
        assert_eq!(parse_event_counter(memory_events, "max"), 12);
        assert_eq!(parse_event_counter("max 7\n", "max"), 7);
        assert_eq!(parse_event_counter("", "oom_kill"), 0);
        assert_eq!(parse_event_counter("oom_kill\n", "oom_kill"), 0);
    }

    /// Attribution keys on the delta, not the absolute counter: a session-cumulative total would
    /// mis-attribute every later call once any earlier one had been OOM-killed.
    #[test]
    fn attribution_uses_the_delta_across_one_call() {
        let before = CgroupEventCounters {
            oom_kill: 1,
            pids_max: 4,
        };
        assert_eq!(before.attribution_since(before), None);
        assert_eq!(
            CgroupEventCounters {
                oom_kill: 2,
                pids_max: 4
            }
            .attribution_since(before),
            Some("cgroup_memory_bytes")
        );
        assert_eq!(
            CgroupEventCounters {
                oom_kill: 1,
                pids_max: 5
            }
            .attribution_since(before),
            Some("cgroup_pids_max")
        );
    }

    #[test]
    fn scope_dir_name_is_sanitized_and_never_empty() {
        assert!(scope_dir_name("ses_abc123").starts_with("murmur-ses_abc123-"));
        assert!(scope_dir_name("../escape").starts_with("murmur-.._escape-"));
        assert!(!scope_dir_name("../escape").contains('/'));
        assert!(scope_dir_name("").starts_with("murmur-"));
    }

    /// A subprocess inherits its parent's cgroup, so a `mur` spawned by a runtime that already
    /// stepped aside starts life *inside* the supervisor leaf. Reading that as its delegated base
    /// would nest another leaf inside it, one per generation, until nothing is delegated.
    #[test]
    fn delegated_base_ascends_out_of_inherited_supervisor_leaves() {
        let base = PathBuf::from("/sys/fs/cgroup/user.slice/app.slice/mur.scope");

        assert_eq!(ascend_out_of_supervisor_leaves(base.clone()), base);
        assert_eq!(
            ascend_out_of_supervisor_leaves(base.join(SUPERVISOR_LEAF)),
            base
        );
        assert_eq!(
            ascend_out_of_supervisor_leaves(base.join(SUPERVISOR_LEAF).join(SUPERVISOR_LEAF)),
            base,
            "a base already nested by an earlier build must still resolve to the real root"
        );
    }

    /// Two scopes must never share a directory even when handed the same id: `Drop` removes the
    /// scope by path, so a collision would have one session delete the cgroup still bounding
    /// another's subprocess tree.
    #[test]
    fn scope_dir_name_is_unique_per_call_for_one_id() {
        let first = scope_dir_name("p");
        let second = scope_dir_name("p");
        assert_ne!(first, second, "same plan id must not reuse a scope directory");
    }

    #[test]
    fn pid_is_formatted_without_allocating() {
        let mut buf = [0u8; 24];
        let len = format_pid(4321, &mut buf);
        assert_eq!(&buf[..len], b"4321\n");

        let len = format_pid(7, &mut buf);
        assert_eq!(&buf[..len], b"7\n");
    }

    #[test]
    fn device_numbers_use_the_wide_glibc_encoding() {
        // 8:0 (`/dev/sda`) encodes as 0x800 under the 12+20 bit layout.
        assert_eq!(device_major_minor(0x800), (8, 0));
        // 259:3 (`nvme0n1p3`) needs the wide major field, which the legacy 8+8 layout truncates.
        assert_eq!(device_major_minor((259 << 8) | 3), (259, 3));
    }
}
