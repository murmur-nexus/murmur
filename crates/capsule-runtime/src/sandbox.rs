//! Kernel-level enforcement for the shell-subprocess tree spawned by `shell::execute_shell`.
//!
//! Today `execute_shell` is gated only by app-level checks (`policy.shell_allow` string
//! equality, and `shell::build_shell_env`'s synthetic-HOME/credential-stripping). Nothing
//! stops the spawned process — or anything it execs/forks — from making arbitrary syscalls:
//! reading files outside its workdir, or connecting to hosts outside `policy.network_allow`.
//! This module closes that gap for the shell subprocess tree specifically, using two Linux
//! kernel primitives:
//!
//!   - **seccomp-bpf user-notify** (`SECCOMP_RET_USER_NOTIF`) to allowlist `execve`/`execveat`
//!     (exec) and `connect`/`sendto` (network) by *argument content* (pathname / destination
//!     IP), not just syscall number — classic BPF alone cannot dereference syscall argument
//!     *contents*, only the notify facility (with a supervisor reading `/proc/<pid>/mem` at
//!     the argument address) can. Exec decisions match *canonical binary identity* (the
//!     exec'd path canonicalized against the launch-time-resolved real paths of the
//!     `shell.allow` binaries — see `decide_exec_allowed`), never name/basename strings, so
//!     renaming an arbitrary binary to an allowlisted name does not allowlist it.
//!   - **Landlock LSM** to scope filesystem access to the capsule's `workdir`.
//!
//! Both are Linux-only. macOS (and any other non-Linux target) has no equivalent kernel
//! primitive and permanently falls back to the existing, unmodified synthetic-HOME/env-
//! stripping mechanism in `shell.rs` — see `EnforcementTier::EnvironmentOnly`.
//!
//! ## Three-tier model
//!
//! - `KernelFull` (Linux, Landlock ABI available — kernel 5.13+): seccomp exec/network
//!   allowlisting + Landlock filesystem scoping.
//! - `KernelSeccompOnly` (Linux, Landlock unavailable — kernel <5.13): seccomp exec/network
//!   allowlisting only. Filesystem scope stays convention-only (`current_dir`) — a
//!   documented gap, not a bug.
//! - `EnvironmentOnly` (macOS / any non-Linux target): no kernel primitive attempted at all.
//!   Permanent, not a placeholder for a future slice.
//!
//! Tier detection is always a runtime capability probe (attempt Landlock ruleset
//! construction, inspect the resulting `RulesetStatus`) — never a hardcoded kernel-version
//! string parse, which is fragile against distro backports.
//!
//! ## Fail-closed invariant
//!
//! If kernel enforcement setup fails unexpectedly on a Linux host (not the expected
//! "Landlock unsupported, degrade to `KernelSeccompOnly`" signal, but something like seccomp
//! filter install failing, or the notify-fd handshake failing before spawn) —
//! `prepare_enforcement` returns `Err` and `execute_shell` must not call `.spawn()` at all.
//! There is no code path where a Linux host silently runs a shell subprocess with zero
//! enforcement because setup failed.

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use murmur_artifact::security_warnings::{security_warning_link, W_SEC_001, W_SEC_002, W_SEC_005};

use crate::types::CapabilityPolicy;

/// Kernel-enforcement tier for the current host, in descending order of enforcement
/// strength. Always host-probed at launch time — never sourced from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnforcementTier {
    /// Linux, Landlock ABI available (kernel 5.13+): seccomp exec/network allowlisting
    /// + Landlock filesystem scoping.
    KernelFull,
    /// Linux, Landlock unavailable (kernel <5.13): seccomp exec/network allowlisting
    /// only. Filesystem scope stays convention-only (`current_dir`) — documented gap.
    KernelSeccompOnly,
    /// macOS (or any non-Linux target): no kernel sandboxing primitive exists. This is
    /// PERMANENT, not a placeholder for a future slice.
    EnvironmentOnly,
}

/// Pure decision: given whether the host is Linux and (if so) the outcome of a Landlock
/// ruleset-restriction probe, decide the tier. No syscalls here — fully unit-testable on
/// any OS.
pub(crate) fn tier_from_probe(
    is_linux: bool,
    landlock_fully_enforced: Option<bool>,
) -> EnforcementTier {
    if !is_linux {
        return EnforcementTier::EnvironmentOnly;
    }
    match landlock_fully_enforced {
        Some(true) => EnforcementTier::KernelFull,
        Some(false) | None => EnforcementTier::KernelSeccompOnly,
    }
}

/// Real host probe. On Linux, attempts to build a Landlock ruleset and call
/// `.restrict_self()`, mapping `RulesetStatus::FullyEnforced` to `Some(true)` and anything
/// else (`PartiallyEnforced`/`NotEnforced`, or any construction error) to `Some(false)`,
/// then delegates to `tier_from_probe`. Off Linux, never probes anything.
#[cfg(target_os = "linux")]
pub(crate) fn detect_enforcement_tier() -> EnforcementTier {
    tier_from_probe(true, linux_enforce::probe_landlock_full_access())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn detect_enforcement_tier() -> EnforcementTier {
    tier_from_probe(false, None)
}

/// Resolves every host in `network_allow` (via `crate::network_policy::parse_network_allow_rules`,
/// the same parser already used for the WASI-HTTP-facing allowlist) to concrete IP addresses
/// via DNS, once. Returns the deduplicated union of all resolved addresses across all allowed
/// hosts.
///
/// Hosts that fail to resolve are silently skipped, matching `resolve_exec_allowlist`'s precedent
/// below: the resolved set feeds *only* the kernel-level (Landlock/seccomp) allow set for
/// `shell.allow` subprocesses, so a host contributing zero IPs merely *shrinks* that set (deny
/// direction, fail-closed). A DNS miss must not be fatal — resolution happens unconditionally at
/// staging time, before any WASM loads and regardless of whether the capsule declares
/// `shell.allow` at all, so hard-failing here turned any transient resolver outage or
/// non-resolving allowlist host into an E-IO-003 that blocked the whole run.
///
/// This does not widen what the capsule may reach: the WASM component's own outbound WASI-HTTP
/// calls are gated separately by `network_policy::NetworkAllowRule::matches`, a host-pattern
/// match that never consults DNS. Malformed host *syntax* is still a hard error — it is caught by
/// `parse_network_allow_rules` above, before this loop.
pub(crate) fn resolve_network_allowlist_ips(network_allow: &[String]) -> Result<Vec<IpAddr>, String> {
    let rules = crate::network_policy::parse_network_allow_rules(network_allow)
        .map_err(|error| error.to_string())?;

    let mut ips = std::collections::BTreeSet::new();
    for rule in &rules {
        let host = rule.host.as_str();
        let Ok(resolved) = (host, 0u16).to_socket_addrs() else {
            continue;
        };
        for addr in resolved {
            ips.insert(addr.ip());
        }
    }

    Ok(ips.into_iter().collect())
}

/// Resolves each `capabilities.shell.allow` entry to the canonical filesystem path(s) of the
/// binary it names, once, at launch time. Entries containing a path separator are
/// canonicalized directly; bare names (the common case, per the manifest schema's
/// "bare binary name" rule) are looked up in the host `PATH` the way `execvp` would, then
/// canonicalized (resolving symlinks, `..`, etc.).
///
/// Entries that resolve to nothing (binary not installed) are silently skipped — that only
/// *shrinks* the kernel-level allow set (deny direction, fail-closed) and matches reality:
/// an exec of a binary that does not exist at launch fails anyway, and a binary *created
/// after launch* under an allowlisted name (the attack this exists to stop) must not become
/// allowlisted by virtue of its name.
///
/// This canonical set — not the raw name strings — is what the seccomp exec supervisor
/// compares against, so `cp /usr/bin/nc ./bash && ./bash` is denied: the copy's canonical
/// path is not the canonical path of the launch-time `bash`, no matter what it is named.
pub(crate) fn resolve_exec_allowlist(shell_allow: &[String]) -> Vec<PathBuf> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    resolve_exec_allowlist_in(shell_allow, &path_dirs)
}

/// Testable core of `resolve_exec_allowlist`, with the `PATH` directory list injected. For a
/// bare name, every executable `PATH` match is included (not just the first): all of them are
/// binaries the operator's launch-time `PATH` exposes under that allowlisted name, and on
/// merged-`/usr` distros `/bin/x` and `/usr/bin/x` canonicalize to the same entry anyway.
fn resolve_exec_allowlist_in(shell_allow: &[String], path_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut resolved = std::collections::BTreeSet::new();
    for entry in shell_allow {
        if entry.is_empty() {
            continue;
        }
        if entry.contains('/') {
            if let Ok(canonical) = std::fs::canonicalize(entry) {
                resolved.insert(canonical);
            }
            continue;
        }
        for dir in path_dirs {
            let candidate = dir.join(entry);
            if is_executable_file(&candidate) {
                if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                    resolved.insert(canonical);
                }
            }
        }
    }
    resolved.into_iter().collect()
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Identity-based exec decision: canonicalizes the pathname a subprocess passed to
/// `execve`/`execveat` (relative paths resolved against `base_dir` — the notifying task's
/// cwd or `execveat` dirfd, read from `/proc` by the Linux supervisor) and allows the exec
/// only if the *canonical* path is one of the launch-time-resolved allowlist binaries.
///
/// Matching canonical identity rather than the name/basename string is the point: a copy of
/// an arbitrary binary renamed to an allowlisted name (e.g. `cp /usr/bin/nc ./bash`) has a
/// different canonical path and is denied, while a symlink *to* the real allowlisted binary
/// canonicalizes to it and is (correctly) allowed. Fail-closed: empty pathname, relative
/// pathname with no resolvable base, nonexistent path, or any canonicalization error all
/// deny.
///
/// Known residual limit, inherent to `SECCOMP_RET_USER_NOTIF` + continue (see
/// `seccomp_unotify(2)`): after this check passes, the kernel re-resolves the pathname when
/// it actually executes the syscall, so a hostile *multithreaded* child retargeting a
/// symlink in that window can still race it. Closing that fully requires fd-substitution
/// (`SECCOMP_IOCTL_NOTIF_ADDFD`-style) rather than continue semantics — out of scope for
/// this slice; the non-racing rename/copy bypass is what this closes.
// Production callers live inside the Linux-only supervisor; unit tests exercise it on every
// OS (which is the point of keeping it out of `linux_enforce`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn decide_exec_allowed(
    pathname: &str,
    base_dir: Option<&Path>,
    exec_allow: &[PathBuf],
) -> bool {
    if pathname.is_empty() || exec_allow.is_empty() {
        return false;
    }
    let path = Path::new(pathname);
    let joined;
    let candidate: &Path = if path.is_absolute() {
        path
    } else {
        match base_dir {
            Some(base) => {
                joined = base.join(path);
                &joined
            }
            None => return false,
        }
    };
    match std::fs::canonicalize(candidate) {
        Ok(canonical) => exec_allow.iter().any(|allowed| allowed == &canonical),
        Err(_) => false,
    }
}

/// Network decision for one destination IP read out of a notifying task's `sockaddr`.
/// Empty allowlist denies everything (a capsule that declared no `network.allow` hosts has
/// no reason to open any subprocess socket).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn network_ip_allowed(ip: IpAddr, network_allow_ips: &[IpAddr]) -> bool {
    network_allow_ips.contains(&ip)
}

// The bytes handed to `parse_sockaddr_ip` always come from a *Linux* child's memory (the
// seccomp-notify supervisor is Linux-only), so the address-family constants and struct
// layouts here are Linux's — spelled out as literals rather than `libc::AF_*` so the parser
// is host-independent and unit-testable on non-Linux dev machines (macOS's `AF_INET6` is 30,
// not 10, and its `sockaddr` puts the family in a different byte).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET: u16 = 2;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET6: u16 = 10;

/// Parses the destination IP out of a raw Linux `sockaddr` buffer (`sockaddr_in` /
/// `sockaddr_in6` layouts). Returns `None` for any other address family (`AF_UNIX`,
/// `AF_NETLINK`, ...) or a buffer too short to contain the address — the caller decides what
/// non-IP means (the supervisor allows non-IP families; they are outside this layer's scope).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_sockaddr_ip(bytes: &[u8]) -> Option<IpAddr> {
    if bytes.len() < 2 {
        return None;
    }
    // `sa_family_t` is a native-endian u16 at offset 0 in Linux's sockaddr layouts; the
    // supervisor runs on the same machine as the child, so native-endian is correct.
    let family = u16::from_ne_bytes([bytes[0], bytes[1]]);
    match family {
        LINUX_AF_INET => {
            let octets: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
            Some(IpAddr::from(octets))
        }
        LINUX_AF_INET6 => {
            let octets: [u8; 16] = bytes.get(8..24)?.try_into().ok()?;
            Some(IpAddr::from(octets))
        }
        _ => None,
    }
}

/// Bundles the resolved, host-independent enforcement inputs for one capsule session.
#[derive(Debug, Clone)]
pub(crate) struct ShellEnforcement {
    pub(crate) tier: EnforcementTier,
    // Only read by the Linux-only supervisor (`linux_enforce::classify_and_decide`); on
    // non-Linux builds this is resolved (for parity) but never consulted, since
    // `prepare_enforcement` is a no-op there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) network_allow_ips: Vec<IpAddr>,
    /// Canonical filesystem paths of the `shell.allow` binaries, resolved once at launch by
    /// `resolve_exec_allowlist` — the identity set the seccomp exec supervisor matches
    /// against (never the raw name strings; see `decide_exec_allowed`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) exec_allow_paths: Vec<PathBuf>,
}

impl ShellEnforcement {
    /// Resolves tier + network allowlist + canonical exec allowlist once, at launch time.
    pub(crate) fn resolve(policy: &CapabilityPolicy) -> Result<Self, String> {
        let tier = detect_enforcement_tier();
        let network_allow_ips = resolve_network_allowlist_ips(&policy.network_allow)?;
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        Ok(Self {
            tier,
            network_allow_ips,
            exec_allow_paths,
        })
    }

    /// The permanent macOS/non-Linux value, and also what tests should construct directly to
    /// exercise `execute_shell` without depending on real kernel enforcement machinery. This
    /// is not a fake test-only bypass — it's the literal value real macOS hosts get. (Only
    /// test code calls this directly today — production always goes through `resolve()`,
    /// which independently arrives at the same tier value on non-Linux hosts.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn environment_only() -> Self {
        Self {
            tier: EnforcementTier::EnvironmentOnly,
            network_allow_ips: Vec::new(),
            exec_allow_paths: Vec::new(),
        }
    }
}

/// Core messages shared between the stderr line and `logs/bootstrap.log` line for each tier
/// warning, matching the shared-const convention used by `runtime::BASH_NETWORK_BYPASS_WARNING`.
/// Full detail lives on the security-warnings doc page (`security_warning_link`); keep each to
/// one or two concise sentences.
///
/// The Landlock/seccomp enforcement has NEVER been compiled or run on real Linux
/// hardware — it was implemented and tested only on macOS, where it is a no-op — and a code
/// review found a probable-breaking Landlock-grant bug. Until a real Linux run verifies it, the
/// enforcement must not be presented as a trustworthy boundary. That is why `KernelFull` warns
/// too (`W_SEC_005`): a silent "full" tier would imply everything is enforced, which is exactly
/// the false assurance to avoid.
const KERNEL_UNVERIFIED_WARNING: &str = "capabilities.shell.allow is non-empty and this host \
resolved to a Linux kernel-enforcement tier (Landlock/seccomp), but that enforcement has NOT \
been verified on real Linux hardware — treat filesystem, exec, and network isolation for shell \
subprocesses as experimental and do not rely on it as a security boundary yet.";

const SECCOMP_ONLY_WARNING: &str = "capabilities.shell.allow is non-empty and this Linux kernel \
lacks Landlock (kernel <5.13) — filesystem access outside the capsule workdir is not \
kernel-enforced at all, and the seccomp exec/network enforcement that would apply has not been \
verified on real Linux hardware. Treat shell subprocess isolation as experimental on this host.";

const ENVIRONMENT_ONLY_WARNING: &str = "capabilities.shell.allow is non-empty but this \
platform has no kernel-level subprocess sandbox (Landlock/seccomp are Linux-only) — \
enforcement is environment-only (synthetic HOME + credential env-stripping). This is \
permanent on this platform.";

/// Tier-aware replacement for calling `runtime::warn_if_bash_network_bypass` unconditionally.
///
///   - `KernelFull` → warns (if `shell_allow` is non-empty) that the Linux kernel enforcement
///     is unverified and must not be trusted yet — NOT silent. See the const doc above for why.
///   - `KernelSeccompOnly` → warns (if `shell_allow` is non-empty) that Landlock is unavailable
///     (filesystem scope unenforced) AND that the remaining seccomp enforcement is unverified.
///   - `EnvironmentOnly` → warns (if `shell_allow` is non-empty) that there is no kernel
///     sandboxing primitive at all on this platform, and this is permanent. Also still calls
///     `runtime::warn_if_bash_network_bypass`, which remains accurate on this tier since
///     nothing is kernel-enforced.
///
/// Pure tier→warning-text decision, split out of `warn_for_enforcement_tier` so tests can
/// assert what each tier warns without capturing stderr.
pub(crate) fn tier_warning(
    tier: EnforcementTier,
    shell_allow_is_empty: bool,
) -> Option<(&'static str, &'static str)> {
    if shell_allow_is_empty {
        return None;
    }
    match tier {
        EnforcementTier::KernelFull => Some((W_SEC_005, KERNEL_UNVERIFIED_WARNING)),
        EnforcementTier::KernelSeccompOnly => Some((W_SEC_002, SECCOMP_ONLY_WARNING)),
        EnforcementTier::EnvironmentOnly => Some((W_SEC_001, ENVIRONMENT_ONLY_WARNING)),
    }
}

/// Fires at every launch, not just once.
pub(crate) fn warn_for_enforcement_tier(tier: EnforcementTier, workdir: &Path, policy: &CapabilityPolicy) {
    if let Some((code, message)) = tier_warning(tier, policy.shell_allow.is_empty()) {
        let link = security_warning_link(code);
        eprintln!("[capsule-runtime] warning[{code}]: {message} ({link})");
        crate::agent::append_bootstrap_log(
            workdir,
            &format!("[capability-policy] warning[{code}]: {message} ({link})"),
        );
    }
    if tier == EnforcementTier::EnvironmentOnly {
        crate::runtime::warn_if_bash_network_bypass(workdir, policy);
    }
}

/// Handle to a (possibly no-op) supervisor. On Linux tiers, the background thread is already
/// running by the time this is returned by `prepare_enforcement` — started BEFORE the caller
/// calls `.spawn()`, not after.
///
/// This ordering is required, not incidental: seccomp filters (installed inside `pre_exec`,
/// i.e. after `fork()` but before the target binary's `execve`) are inherited across that very
/// `execve` — so the FIRST notified syscall the forked child ever makes is the one that turns
/// it into the target binary (e.g. `bash`) itself. `std::process::Command::spawn()`'s parent
/// side blocks internally (via a close-on-exec error pipe) until that `execve` either succeeds
/// or fails. If the supervisor only started listening for notifications after `.spawn()`
/// returned, `.spawn()` could never return in the first place: the child would be stuck
/// waiting for a notify response no one is listening for yet, and the parent would be stuck
/// waiting for the child's exec to resolve. Starting the receiver+supervisor thread first
/// (racing concurrently with the fork/exec inside `.spawn()`, not gated behind it) avoids that
/// deadlock — by the time `.spawn()`'s internal blocking read is waiting on the exec-status
/// pipe, the supervisor thread is already independently waiting to receive the notify fd and
/// answer requests on it.
pub(crate) enum SupervisorHandle {
    Noop,
    #[cfg(target_os = "linux")]
    Linux(std::sync::mpsc::Receiver<()>),
}

impl SupervisorHandle {
    /// Best-effort join with a short bound so `execute_shell` never hangs waiting for the
    /// supervisor thread. In practice it returns immediately: the notify fd closes (which
    /// makes the thread return) once every process holding the seccomp filter — the child and
    /// all its descendants — has exited, which `wait_with_output` (called by `execute_shell`
    /// right before this) has already waited for.
    pub(crate) fn join_best_effort(self) {
        match self {
            SupervisorHandle::Noop => {}
            #[cfg(target_os = "linux")]
            SupervisorHandle::Linux(done_rx) => {
                let _ = done_rx.recv_timeout(std::time::Duration::from_secs(5));
                // Deliberately not joining the underlying `JoinHandle`: dropping it without
                // joining leaves the thread detached (it keeps running to completion in the
                // background, reclaimed by the OS on exit), which is fine here since the
                // `done_rx` signal above already tells us the loop returned.
            }
        }
    }
}

// Forced-failure seam for the fail-closed contract, per the slice design's own wording
// ("simulated via a forced error path in a unit/integration test, since inducing a real
// kernel-level failure requires already-hostile conditions"). Only compiled into test
// builds; production `prepare_enforcement` has no bypass or injection point.
#[cfg(test)]
thread_local! {
    pub(crate) static FORCE_PREPARE_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn forced_prepare_failure() -> Result<(), String> {
    if FORCE_PREPARE_FAILURE.with(|flag| flag.get()) {
        return Err(
            "sandbox: kernel enforcement setup failed (forced by test seam)".to_string(),
        );
    }
    Ok(())
}

/// Installs kernel-level exec/network/filesystem enforcement into `command` so it applies to
/// the spawned process and everything it forks/execs. No-op when
/// `enforcement.tier == EnvironmentOnly` — does not even attempt a Landlock/seccomp call in
/// that case.
#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare_enforcement(
    _command: &mut std::process::Command,
    enforcement: &ShellEnforcement,
    _workdir: &Path,
) -> Result<SupervisorHandle, String> {
    #[cfg(test)]
    forced_prepare_failure()?;

    debug_assert_eq!(
        enforcement.tier,
        EnforcementTier::EnvironmentOnly,
        "non-Linux targets must always resolve to EnforcementTier::EnvironmentOnly"
    );
    Ok(SupervisorHandle::Noop)
}

/// Linux implementation of `prepare_enforcement`. See the module-level docs and the `linux_enforce`
/// submodule for the mechanics (socketpair fd-passing side channel + `pre_exec` seccomp/Landlock
/// installation + background notify-fd supervisor thread).
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn prepare_enforcement(
    command: &mut std::process::Command,
    enforcement: &ShellEnforcement,
    workdir: &Path,
) -> Result<SupervisorHandle, String> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    #[cfg(test)]
    forced_prepare_failure()?;

    if enforcement.tier == EnforcementTier::EnvironmentOnly {
        return Ok(SupervisorHandle::Noop);
    }

    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a 2-element stack array, exactly the size `socketpair` writes into;
    // no other precondition applies.
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "sandbox: socketpair() failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: both fds were just returned by the successful socketpair() call above; nothing
    // else has taken ownership of them yet, so wrapping them in OwnedFd is exclusive/sound.
    let (parent_sock, child_sock) = unsafe {
        (
            OwnedFd::from_raw_fd(fds[0]),
            OwnedFd::from_raw_fd(fds[1]),
        )
    };

    let tier = enforcement.tier;
    let workdir_for_child = workdir.to_path_buf();

    // SAFETY: this closure runs in the forked child, after fork() but before execve() — the
    // narrow pre_exec window where only async-signal-safe operations are permitted. It only
    // performs libseccomp filter construction/load (kernel syscalls), one `sendmsg` call to
    // hand the notify fd to the parent over `child_sock`, and (KernelFull only) Landlock
    // ruleset construction/`restrict_self` (also kernel syscalls) — no locks are taken beyond
    // what those syscalls themselves need. `child_sock` is moved in and closes automatically
    // when the closure body finishes, i.e. after it has already handed the notify fd to the
    // parent.
    unsafe {
        command.pre_exec(move || {
            let fd = child_sock.as_raw_fd();
            linux_enforce::child_install_enforcement(tier, &workdir_for_child, fd)
        });
    }

    // Start the receiver+supervisor thread now — BEFORE the caller calls `.spawn()`. See
    // `SupervisorHandle`'s doc comment for why this ordering (not "after spawn") is required.
    Ok(linux_enforce::start_supervisor(
        parent_sock,
        enforcement.exec_allow_paths.clone(),
        enforcement.network_allow_ips.clone(),
    ))
}

/// Linux-only mechanics: fd-passing side channel, seccomp filter construction, Landlock
/// application, and the background notify-request supervisor loop.
///
/// NOTE on `libseccomp` types used here: `ScmpFd` (the type `get_notify_fd`/`ScmpNotifReq::receive`/
/// `notify_id_valid`/`ScmpNotifResp::respond` all use) is treated as an alias for `RawFd` (`i32`)
/// per the libseccomp 0.4.0 docs — if a future version changes this, these call sites need an
/// explicit `.into()`/`as` conversion.
#[cfg_attr(target_os = "linux", allow(unsafe_code))]
#[cfg(target_os = "linux")]
mod linux_enforce {
    use std::io;
    use std::net::IpAddr;
    use std::os::fd::{AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    use super::{decide_exec_allowed, network_ip_allowed, parse_sockaddr_ip};
    use super::{EnforcementTier, SupervisorHandle};

    enum Decision {
        Allow,
        Deny,
    }

    fn to_decision(allowed: bool) -> Decision {
        if allowed {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }

    /// Probes real Landlock support by actually building a ruleset and calling
    /// `restrict_self()`, granting itself full access to `/` so the probe has no observable
    /// effect on the calling (host) process beyond placing it into a Landlock domain that
    /// permits everything — Landlock domains only ever get *stricter* on further nesting, and
    /// the real per-shell-call restriction (scoped to `workdir`) is applied separately, inside
    /// each spawned child's own `pre_exec`, not here. This is the "runtime capability probe"
    /// the tier detection requires, without functionally sandboxing the whole host process
    /// from a mere capability check.
    pub(super) fn probe_landlock_full_access() -> Option<bool> {
        use landlock::{
            Access, AccessFs, Compatible, CompatLevel, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };

        let abi = ABI::V1;
        let access_all = AccessFs::from_all(abi);

        let root_fd = PathFd::new("/").ok()?;

        let status = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(access_all)
            .ok()?
            .create()
            .ok()?
            .add_rule(PathBeneath::new(root_fd, access_all))
            .ok()?
            .restrict_self()
            .ok()?;

        Some(matches!(status.ruleset, RulesetStatus::FullyEnforced))
    }

    /// Runs inside the forked child, pre-exec: installs the seccomp exec/network-notify
    /// filter, hands its notify fd to the parent over `child_sock_fd`, and (on `KernelFull`)
    /// applies the Landlock filesystem scope. Returning `Err` here aborts the exec (std's
    /// `Command` machinery propagates it back to the parent's `.spawn()` call as an `io::Error`)
    /// — the fail-closed path for setup failures that happen after fork.
    pub(super) fn child_install_enforcement(
        tier: EnforcementTier,
        workdir: &Path,
        child_sock_fd: RawFd,
    ) -> io::Result<()> {
        install_seccomp_filter(child_sock_fd)?;

        if tier == EnforcementTier::KernelFull {
            apply_landlock_scope(workdir).map_err(|error| io::Error::other(error))?;
        }

        Ok(())
    }

    fn install_seccomp_filter(child_sock_fd: RawFd) -> io::Result<()> {
        let mut filter = libseccomp::ScmpFilterContext::new(libseccomp::ScmpAction::Allow)
            .map_err(to_io_err)?;

        for name in ["execve", "execveat", "connect", "sendto"] {
            let syscall = libseccomp::ScmpSyscall::from_name(name).map_err(to_io_err)?;
            filter
                .add_rule(libseccomp::ScmpAction::Notify, syscall)
                .map_err(to_io_err)?;
        }

        filter.load().map_err(to_io_err)?;

        // NOTE: `get_notify_fd` only becomes valid after `load()`. Dropping `filter` afterward
        // is safe — libseccomp's `seccomp_release()` (invoked on Drop) only frees the
        // userspace filter-building context; the filter itself is already installed in the
        // kernel and the notify fd is an independent, already-open fd that is unaffected by
        // dropping this handle.
        let notify_fd: RawFd = filter.get_notify_fd().map_err(to_io_err)?;
        set_cloexec(notify_fd)?;

        send_fd_over_socket(child_sock_fd, notify_fd)
    }

    fn to_io_err<E: std::fmt::Display>(error: E) -> io::Error {
        io::Error::other(error.to_string())
    }

    #[allow(unsafe_code)]
    fn set_cloexec(fd: RawFd) -> io::Result<()> {
        // SAFETY: `fd` is the just-created, valid, open seccomp notify fd; F_GETFD/F_SETFD
        // with FD_CLOEXEC only touch a per-fd integer flag, no pointer/lifetime hazards.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: same as above.
        let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn apply_landlock_scope(workdir: &Path) -> Result<(), String> {
        use landlock::{
            Access, AccessFs, Compatible, CompatLevel, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };

        let abi = ABI::V1;
        let access_all = AccessFs::from_all(abi);

        let workdir_fd = PathFd::new(workdir).map_err(|error| {
            format!(
                "landlock: failed to open workdir {} for scoping: {error}",
                workdir.display()
            )
        })?;

        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(access_all)
            .map_err(|error| format!("landlock: handle_access failed: {error}"))?
            .create()
            .map_err(|error| format!("landlock: ruleset create failed: {error}"))?
            .add_rule(PathBeneath::new(workdir_fd, access_all))
            .map_err(|error| format!("landlock: add_rule failed: {error}"))?
            .restrict_self()
            .map_err(|error| format!("landlock: restrict_self failed: {error}"))?;

        Ok(())
    }

    /// Sends one fd (`fd_to_send`) over an already-connected `SOCK_DGRAM` unix socket
    /// (`sock_fd`) via `SCM_RIGHTS`. Runs inside `pre_exec` — only stack buffers are used (no
    /// heap allocation) and the only syscall made is `sendmsg`, which is async-signal-safe.
    #[allow(unsafe_code)]
    fn send_fd_over_socket(sock_fd: RawFd, fd_to_send: RawFd) -> io::Result<()> {
        // SAFETY: `sock_fd` and `fd_to_send` are both valid, open fds owned by this process.
        // All buffers (`payload`, `cmsg_buf`, `iov`, `msg`) are stack-allocated locals that
        // outlive the single `sendmsg` call using them.
        unsafe {
            let mut payload = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr() as *mut libc::c_void,
                iov_len: payload.len(),
            };

            let mut cmsg_buf = [0u8; 128];
            let cmsg_space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;

            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_space as _;

            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd_to_send);

            let ret = libc::sendmsg(sock_fd, &msg, 0);
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Parent-side counterpart of `send_fd_over_socket`. Not called from `pre_exec`, so
    /// ordinary heap-allocating code would be fine here too, but it stays allocation-free to
    /// match its sibling.
    #[allow(unsafe_code)]
    fn receive_fd_over_socket(sock_fd: RawFd) -> io::Result<RawFd> {
        // SAFETY: `sock_fd` is a valid, open fd; buffers are stack-allocated and sized for
        // exactly one fd's worth of ancillary data.
        unsafe {
            let mut payload = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr() as *mut libc::c_void,
                iov_len: payload.len(),
            };

            let mut cmsg_buf = [0u8; 128];
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = cmsg_buf.len() as _;

            let ret = libc::recvmsg(sock_fd, &mut msg, libc::MSG_CMSG_CLOEXEC);
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }

            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null()
                || (*cmsg).cmsg_level != libc::SOL_SOCKET
                || (*cmsg).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(io::Error::other(
                    "recvmsg did not return an SCM_RIGHTS control message",
                ));
            }

            let fd_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
            Ok(std::ptr::read_unaligned(fd_ptr))
        }
    }

    /// Spawns the background thread that will receive the notify fd from the child — blocking
    /// on that receive, racing concurrently with the caller's (not-yet-issued) `.spawn()` call
    /// rather than running after it, per `SupervisorHandle`'s doc comment — and then supervises
    /// notify requests on it until it closes.
    ///
    /// If the fd is never received (e.g. `fork()` itself failed, or the child's `pre_exec`
    /// closure errored before reaching the send), there is no live child left unsupervised:
    /// `Command::spawn()` surfaces that same underlying failure as its own `Err`, via the same
    /// close-on-exec error pipe, independently of this thread. This thread just logs and exits.
    pub(super) fn start_supervisor(
        parent_sock: OwnedFd,
        exec_allow: Vec<PathBuf>,
        network_allow_ips: Vec<IpAddr>,
    ) -> SupervisorHandle {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            match receive_fd_over_socket(parent_sock.as_raw_fd()) {
                Ok(notify_fd) => supervisor_loop(notify_fd, &exec_allow, &network_allow_ips),
                Err(error) => {
                    eprintln!(
                        "[capsule-runtime] warning: failed to receive seccomp notify fd: \
                         {error} (if a child process was actually forked, its own \
                         Command::spawn() call reports that failure independently)"
                    );
                }
            }
            drop(done_tx);
        });

        SupervisorHandle::Linux(done_rx)
    }

    /// Reads and responds to notify requests until the notify fd errors/EOFs — which happens
    /// once every process holding this seccomp filter (the child and all its descendants) has
    /// exited. Ordinary (non-`pre_exec`) code: heap allocation, `/proc` reads, etc. are all
    /// fine here.
    fn supervisor_loop(notify_fd: RawFd, exec_allow: &[PathBuf], network_allow_ips: &[IpAddr]) {
        loop {
            let req = match libseccomp::ScmpNotifReq::receive(notify_fd) {
                Ok(req) => req,
                Err(_) => break,
            };

            let decision = classify_and_decide(&req, exec_allow, network_allow_ips);

            // TOCTOU-safe pattern: read memory, THEN validate the notification id is still
            // valid, THEN respond. If the id went stale (the target thread was resumed/killed
            // via another path between our read and now), treat it as a race and let the
            // kernel's own handling take over rather than responding to a dead notification.
            if libseccomp::notify_id_valid(notify_fd, req.id).is_err() {
                continue;
            }

            let resp = match decision {
                Decision::Allow => {
                    libseccomp::ScmpNotifResp::new_continue(req.id, libseccomp::ScmpNotifRespFlags::empty())
                }
                Decision::Deny => libseccomp::ScmpNotifResp::new_error(
                    req.id,
                    -libc::EACCES,
                    libseccomp::ScmpNotifRespFlags::empty(),
                ),
            };
            let _ = resp.respond(notify_fd);
        }
    }

    /// Classifies one notification and decides allow/deny. Uses `req.pid` — the pid the
    /// *kernel* reports as the actual notifying task — rather than a pid threaded in from the
    /// original `Child::id()`, since any descendant the original child forks/execs also
    /// inherits this same seccomp filter and can itself be the notifying task (e.g. `bash -c
    /// "sh -c 'nc ...'"` — the innermost `sh`'s own `execve` of `nc` notifies with `sh`'s pid,
    /// not the original `bash`'s).
    fn classify_and_decide(
        req: &libseccomp::ScmpNotifReq,
        exec_allow: &[PathBuf],
        network_allow_ips: &[IpAddr],
    ) -> Decision {
        let pid = req.pid;
        let is_syscall = |name: &str| {
            libseccomp::ScmpSyscall::from_name(name)
                .map(|target| target == req.data.syscall)
                .unwrap_or(false)
        };

        if is_syscall("execve") {
            return match read_cstr_from_child(pid, req.data.args[0]) {
                // A relative exec path is relative to the notifying task's own cwd — read it
                // from /proc; if that read fails, `decide_exec_allowed` denies relative
                // paths (absolute paths never consult it).
                Ok(path) => {
                    let cwd = read_child_cwd(pid);
                    to_decision(decide_exec_allowed(&path, cwd.as_deref(), exec_allow))
                }
                Err(_) => Decision::Deny,
            };
        }
        if is_syscall("execveat") {
            // execveat(dirfd, pathname, argv, envp, flags): the pathname is resolved
            // relative to `dirfd` (unless absolute), and with AT_EMPTY_PATH + empty
            // pathname, `dirfd` IS the binary. All three bases are recoverable from
            // /proc/<pid>/{cwd,fd/<n>}; any resolution failure denies.
            let dirfd = req.data.args[0] as libc::c_int;
            let flags = req.data.args[4] as libc::c_int;
            let path = match read_cstr_from_child(pid, req.data.args[1]) {
                Ok(path) => path,
                Err(_) => return Decision::Deny,
            };
            if path.is_empty() && (flags & libc::AT_EMPTY_PATH) != 0 {
                return match read_child_fd_path(pid, dirfd) {
                    Some(target) => to_decision(decide_exec_allowed(
                        &target.to_string_lossy(),
                        None,
                        exec_allow,
                    )),
                    None => Decision::Deny,
                };
            }
            let base = if Path::new(&path).is_absolute() {
                None
            } else if dirfd == libc::AT_FDCWD {
                read_child_cwd(pid)
            } else {
                read_child_fd_path(pid, dirfd)
            };
            return to_decision(decide_exec_allowed(&path, base.as_deref(), exec_allow));
        }
        if is_syscall("connect") {
            return match read_sockaddr_ip_from_child(pid, req.data.args[1]) {
                Ok(Some(ip)) => to_decision(network_ip_allowed(ip, network_allow_ips)),
                // Non-IP address family (e.g. AF_UNIX, AF_NETLINK) — outside this layer's scope.
                Ok(None) => Decision::Allow,
                Err(_) => Decision::Deny,
            };
        }
        if is_syscall("sendto") {
            // A null dest_addr means the socket is already connected (validated at connect()
            // time); nothing new to check.
            if req.data.args[4] == 0 {
                return Decision::Allow;
            }
            return match read_sockaddr_ip_from_child(pid, req.data.args[4]) {
                Ok(Some(ip)) => to_decision(network_ip_allowed(ip, network_allow_ips)),
                Ok(None) => Decision::Allow,
                Err(_) => Decision::Deny,
            };
        }

        // We only ever install NOTIFY rules for the four syscalls above; reaching here would
        // mean an unexpected notification. Deny conservatively.
        Decision::Deny
    }

    /// The notifying task's current working directory, via the /proc magic symlink.
    fn read_child_cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The filesystem path behind one of the notifying task's open fds, via /proc.
    fn read_child_fd_path(pid: u32, fd: libc::c_int) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()
    }

    /// Reads a NUL-terminated string out of the child's address space via `/proc/<pid>/mem`.
    /// Falls back to a smaller read if the first (larger) attempt hits an unmapped page
    /// boundary — pathnames are bounded by `PATH_MAX` (4096) but the NUL terminator is
    /// typically well within the first page.
    fn read_cstr_from_child(pid: u32, addr: u64) -> io::Result<String> {
        let file = std::fs::File::open(format!("/proc/{pid}/mem"))?;

        for len in [4096usize, 256] {
            let mut buf = vec![0u8; len];
            match file.read_at(&mut buf, addr) {
                Ok(n) if n > 0 => {
                    let nul_pos = buf[..n].iter().position(|&b| b == 0).unwrap_or(n);
                    return Ok(String::from_utf8_lossy(&buf[..nul_pos]).into_owned());
                }
                Ok(_) => continue,
                Err(_) => continue,
            }
        }

        Err(io::Error::other(format!(
            "failed to read pathname from /proc/{pid}/mem at {addr:#x}"
        )))
    }

    /// Reads a raw `sockaddr` out of the child's address space and extracts the destination
    /// IP, if the address family is `AF_INET`/`AF_INET6` (parsing lives in the OS-agnostic
    /// `super::parse_sockaddr_ip` so it is unit-testable off-Linux).
    fn read_sockaddr_ip_from_child(pid: u32, addr: u64) -> io::Result<Option<IpAddr>> {
        let file = std::fs::File::open(format!("/proc/{pid}/mem"))?;
        // sizeof(sockaddr_in6) is the largest layout we parse.
        let mut buf = [0u8; 28];
        let n = file.read_at(&mut buf, addr)?;
        Ok(parse_sockaddr_ip(&buf[..n]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::security_warnings::W_SEC_003;

    #[test]
    fn tier_from_probe_non_linux_is_always_environment_only() {
        assert_eq!(
            tier_from_probe(false, None),
            EnforcementTier::EnvironmentOnly
        );
        assert_eq!(
            tier_from_probe(false, Some(true)),
            EnforcementTier::EnvironmentOnly
        );
        assert_eq!(
            tier_from_probe(false, Some(false)),
            EnforcementTier::EnvironmentOnly
        );
    }

    #[test]
    fn tier_from_probe_linux_fully_enforced_is_kernel_full() {
        assert_eq!(tier_from_probe(true, Some(true)), EnforcementTier::KernelFull);
    }

    #[test]
    fn tier_from_probe_linux_partially_enforced_is_seccomp_only() {
        assert_eq!(
            tier_from_probe(true, Some(false)),
            EnforcementTier::KernelSeccompOnly
        );
    }

    #[test]
    fn tier_from_probe_linux_no_probe_result_is_seccomp_only() {
        assert_eq!(tier_from_probe(true, None), EnforcementTier::KernelSeccompOnly);
    }

    #[test]
    fn resolve_network_allowlist_ips_empty_list_is_empty() {
        let ips = resolve_network_allowlist_ips(&[]).unwrap();
        assert!(ips.is_empty());
    }

    #[test]
    fn resolve_network_allowlist_ips_resolves_localhost_to_loopback() {
        let ips = resolve_network_allowlist_ips(&["localhost".to_string()]).unwrap();
        assert!(!ips.is_empty(), "localhost should resolve to at least one address");
        assert!(
            ips.iter().all(|ip| ip.is_loopback()),
            "every resolved localhost address should be loopback, got {ips:?}"
        );
    }

    #[test]
    fn resolve_network_allowlist_ips_rejects_invalid_host_syntax_cleanly() {
        let error = resolve_network_allowlist_ips(&["".to_string()]).unwrap_err();
        assert!(!error.is_empty());
    }

    /// A host that does not resolve contributes zero IPs instead of failing the whole call. The
    /// resolved set only ever *shrinks* the kernel-level `shell.allow` enforcement set, so a DNS
    /// miss is fail-closed, not fatal. `.invalid` is reserved by RFC 2606 and is guaranteed never
    /// to resolve, which keeps this test independent of the host's DNS.
    #[test]
    fn resolve_network_allowlist_ips_skips_unresolvable_host_without_failing() {
        let ips = resolve_network_allowlist_ips(&["definitely-not-a-real-host.invalid".to_string()])
            .expect("an unresolvable host must not be a hard error");
        assert!(
            ips.is_empty(),
            "an unresolvable host should contribute no IPs, got {ips:?}"
        );
    }

    /// The unresolvable host is skipped while the resolvable one still contributes its IPs — the
    /// loop continues rather than aborting on the first miss.
    #[test]
    fn resolve_network_allowlist_ips_keeps_resolvable_hosts_when_another_is_unresolvable() {
        let ips = resolve_network_allowlist_ips(&[
            "definitely-not-a-real-host.invalid".to_string(),
            "localhost".to_string(),
        ])
        .expect("a mix of resolvable and unresolvable hosts must not be a hard error");

        assert!(
            !ips.is_empty(),
            "localhost's addresses should survive alongside an unresolvable host"
        );
        assert!(
            ips.iter().all(|ip| ip.is_loopback()),
            "only localhost should have contributed addresses, got {ips:?}"
        );
    }

    #[test]
    fn resolve_network_allowlist_ips_dedupes_across_hosts() {
        // Two entries that both resolve to loopback should not produce duplicate IPs beyond
        // what's actually distinct.
        let ips = resolve_network_allowlist_ips(&[
            "localhost".to_string(),
            "localhost:8080".to_string(),
        ])
        .unwrap();
        let mut sorted = ips.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ips.len(), sorted.len(), "resolved IPs should already be deduplicated");
    }

    #[test]
    fn environment_only_has_no_resolved_ips_and_environment_only_tier() {
        let enforcement = ShellEnforcement::environment_only();
        assert_eq!(enforcement.tier, EnforcementTier::EnvironmentOnly);
        assert!(enforcement.network_allow_ips.is_empty());
    }

    fn make_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn bootstrap_log_contents(workdir: &Path) -> String {
        std::fs::read_to_string(workdir.join("logs").join("bootstrap.log")).unwrap_or_default()
    }

    // ---- exec allowlist resolution (identity, not names) ----

    #[test]
    fn resolve_exec_allowlist_resolves_bare_name_via_path_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let tool = bin_dir.join("mytool");
        make_executable(&tool, "#!/bin/sh\nexit 0\n");

        let resolved = resolve_exec_allowlist_in(&["mytool".to_string()], &[bin_dir]);
        assert_eq!(resolved, vec![std::fs::canonicalize(&tool).unwrap()]);
    }

    #[test]
    fn resolve_exec_allowlist_skips_entries_that_do_not_resolve() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = resolve_exec_allowlist_in(
            &["definitely-not-a-real-binary".to_string(), String::new()],
            &[temp.path().to_path_buf()],
        );
        assert!(
            resolved.is_empty(),
            "unresolvable entries must shrink (not fail) the allow set: {resolved:?}"
        );
    }

    #[test]
    fn resolve_exec_allowlist_ignores_non_executable_path_matches() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("mytool"), "not executable").unwrap();
        let resolved =
            resolve_exec_allowlist_in(&["mytool".to_string()], &[temp.path().to_path_buf()]);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_exec_allowlist_canonicalizes_path_entries_through_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-tool");
        make_executable(&real, "#!/bin/sh\nexit 0\n");
        let link = temp.path().join("link-tool");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved =
            resolve_exec_allowlist_in(&[link.to_string_lossy().into_owned()], &[]);
        assert_eq!(resolved, vec![std::fs::canonicalize(&real).unwrap()]);
    }

    // ---- exec decision (canonical identity match; finding #1's attack) ----

    /// A tempdir "bin" with a real allowlisted `bash`, plus the canonical allow set for it.
    fn exec_fixture() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        make_executable(&bin_dir.join("bash"), "#!/bin/sh\nexit 0\n");
        let allow =
            resolve_exec_allowlist_in(&["bash".to_string()], std::slice::from_ref(&bin_dir));
        assert_eq!(allow.len(), 1);
        (temp, bin_dir, allow)
    }

    #[test]
    fn decide_exec_denies_renamed_copy_with_allowlisted_basename() {
        // The review's exact bypass: cp <anything> ./bash && ./bash — same basename as an
        // allowlisted entry, different binary identity. Must be denied.
        let (temp, bin_dir, allow) = exec_fixture();
        let workdir = temp.path().join("work");
        std::fs::create_dir(&workdir).unwrap();
        let imposter = workdir.join("bash");
        make_executable(&imposter, "#!/bin/sh\necho imposter\n");

        assert!(
            !decide_exec_allowed("./bash", Some(&workdir), &allow),
            "relative exec of a renamed copy must be denied despite the allowlisted basename"
        );
        assert!(
            !decide_exec_allowed(imposter.to_str().unwrap(), None, &allow),
            "absolute exec of a renamed copy must be denied despite the allowlisted basename"
        );
        // Sanity: the real allowlisted binary itself is still allowed.
        assert!(decide_exec_allowed(
            bin_dir.join("bash").to_str().unwrap(),
            None,
            &allow
        ));
    }

    #[test]
    fn decide_exec_allows_symlink_and_relative_paths_to_the_real_allowlisted_binary() {
        let (temp, bin_dir, allow) = exec_fixture();
        let workdir = temp.path().join("work");
        std::fs::create_dir(&workdir).unwrap();
        // A symlink IS the allowlisted binary once canonicalized — allowing it is correct.
        let link = workdir.join("bash");
        std::os::unix::fs::symlink(bin_dir.join("bash"), &link).unwrap();

        assert!(decide_exec_allowed("./bash", Some(&workdir), &allow));
        assert!(decide_exec_allowed("bin/bash", Some(temp.path()), &allow));
    }

    #[test]
    fn decide_exec_denies_relative_path_without_base_dir() {
        let (_temp, _bin_dir, allow) = exec_fixture();
        assert!(
            !decide_exec_allowed("bash", None, &allow),
            "a relative path whose base (child cwd / dirfd) could not be resolved must deny"
        );
    }

    #[test]
    fn decide_exec_denies_nonexistent_and_empty_paths() {
        let (temp, _bin_dir, allow) = exec_fixture();
        assert!(!decide_exec_allowed("/definitely/not/a/real/binary", None, &allow));
        assert!(!decide_exec_allowed("", Some(temp.path()), &allow));
    }

    #[test]
    fn decide_exec_denies_everything_on_empty_allowlist() {
        let (_temp, bin_dir, _allow) = exec_fixture();
        assert!(!decide_exec_allowed(
            bin_dir.join("bash").to_str().unwrap(),
            None,
            &[]
        ));
    }

    // ---- network decision (finding #2: C-7 decision path, unit level) ----

    #[test]
    fn parse_sockaddr_ip_parses_linux_sockaddr_in() {
        let mut bytes = [0u8; 16];
        bytes[0..2].copy_from_slice(&2u16.to_ne_bytes()); // Linux AF_INET
        bytes[2..4].copy_from_slice(&443u16.to_be_bytes()); // port — irrelevant to the decision
        bytes[4..8].copy_from_slice(&[93, 184, 216, 34]);
        assert_eq!(
            parse_sockaddr_ip(&bytes),
            Some(IpAddr::from([93, 184, 216, 34]))
        );
    }

    #[test]
    fn parse_sockaddr_ip_parses_linux_sockaddr_in6() {
        let ip = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
        let mut bytes = [0u8; 28];
        bytes[0..2].copy_from_slice(&10u16.to_ne_bytes()); // Linux AF_INET6 (10, not macOS's 30)
        bytes[8..24].copy_from_slice(&ip.octets());
        assert_eq!(parse_sockaddr_ip(&bytes), Some(IpAddr::V6(ip)));
    }

    #[test]
    fn parse_sockaddr_ip_returns_none_for_non_ip_families_and_short_buffers() {
        let mut af_unix = [0u8; 16];
        af_unix[0..2].copy_from_slice(&1u16.to_ne_bytes()); // Linux AF_UNIX
        assert_eq!(parse_sockaddr_ip(&af_unix), None);
        assert_eq!(parse_sockaddr_ip(&[]), None);
        assert_eq!(parse_sockaddr_ip(&[2]), None);
        // Family claims AF_INET but the buffer is too short to hold an address.
        assert_eq!(parse_sockaddr_ip(&2u16.to_ne_bytes()), None);
    }

    #[test]
    fn network_ip_allowed_matches_only_the_resolved_allowlist() {
        let allow = vec![IpAddr::from([127, 0, 0, 1])];
        assert!(network_ip_allowed(IpAddr::from([127, 0, 0, 1]), &allow));
        assert!(!network_ip_allowed(IpAddr::from([8, 8, 8, 8]), &allow));
        assert!(
            !network_ip_allowed(IpAddr::from([127, 0, 0, 1]), &[]),
            "an empty resolved allowlist must deny every destination"
        );
    }

    // ---- tier warnings (finding #3: assert behavior, not just "doesn't panic") ----

    #[test]
    fn tier_warning_kernel_full_warns_enforcement_is_unverified() {
        // The Linux "full" tier must NOT be silent — a silent tier implies enforcement is
        // trustworthy, but the enforcement has never run on real Linux hardware. It warns when a shell
        // is declared, and stays silent only when there is no subprocess to enforce anything on.
        assert_eq!(
            tier_warning(EnforcementTier::KernelFull, false),
            Some((W_SEC_005, KERNEL_UNVERIFIED_WARNING))
        );
        assert_eq!(tier_warning(EnforcementTier::KernelFull, true), None);
    }

    #[test]
    fn tier_warning_all_tiers_warn_only_when_shell_allow_is_nonempty() {
        assert_eq!(
            tier_warning(EnforcementTier::KernelSeccompOnly, false),
            Some((W_SEC_002, SECCOMP_ONLY_WARNING))
        );
        assert_eq!(
            tier_warning(EnforcementTier::EnvironmentOnly, false),
            Some((W_SEC_001, ENVIRONMENT_ONLY_WARNING))
        );
        assert_eq!(tier_warning(EnforcementTier::KernelFull, true), None);
        assert_eq!(tier_warning(EnforcementTier::KernelSeccompOnly, true), None);
        assert_eq!(tier_warning(EnforcementTier::EnvironmentOnly, true), None);
    }

    fn warning_test_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            network_allow: vec!["https://api.example.com".to_string()],
            ..CapabilityPolicy::default()
        }
    }

    #[test]
    fn warn_for_enforcement_tier_kernel_full_logs_unverified_when_shell_declared() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(EnforcementTier::KernelFull, temp.path(), &warning_test_policy());
        let log = bootstrap_log_contents(temp.path());
        assert!(
            log.contains(W_SEC_005),
            "KernelFull must not be silent — it must warn its enforcement is unverified: {log}"
        );
        assert!(
            log.contains("not been verified") || log.contains("experimental"),
            "must state the enforcement is unverified/experimental: {log}"
        );
        assert!(
            log.contains(&security_warning_link(W_SEC_005)),
            "must link to the security-warnings doc page: {log}"
        );
    }

    #[test]
    fn warn_for_enforcement_tier_kernel_full_silent_without_shell_allow() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::KernelFull,
            temp.path(),
            &CapabilityPolicy::default(),
        );
        assert_eq!(
            bootstrap_log_contents(temp.path()),
            "",
            "with no shell declared there is no subprocess to enforce on — stay silent"
        );
    }

    #[test]
    fn warn_for_enforcement_tier_seccomp_only_logs_the_landlock_gap() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::KernelSeccompOnly,
            temp.path(),
            &warning_test_policy(),
        );
        let log = bootstrap_log_contents(temp.path());
        assert!(log.contains("Landlock"), "must name the missing primitive: {log}");
        assert!(log.contains("filesystem"), "must name what is not enforced: {log}");
        assert!(log.contains(W_SEC_002), "must carry its warning code: {log}");
        assert!(
            log.contains(&security_warning_link(W_SEC_002)),
            "must link to the security-warnings doc page: {log}"
        );
    }

    #[test]
    fn warn_for_enforcement_tier_environment_only_logs_permanent_and_bypass_warnings() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::EnvironmentOnly,
            temp.path(),
            &warning_test_policy(),
        );
        let log = bootstrap_log_contents(temp.path());
        assert!(
            log.contains("permanent"),
            "must state the tier is permanent, not pending: {log}"
        );
        assert!(
            log.contains("bash"),
            "the still-accurate bash/network bypass warning must also fire: {log}"
        );
        assert!(log.contains(W_SEC_001), "must carry the tier warning code: {log}");
        assert!(
            log.contains(W_SEC_003),
            "must carry the bash/network bypass warning code: {log}"
        );
    }

    // ---- fail-closed (finding #2: forced setup failure must prevent spawn) ----

    struct ForcePrepareFailureGuard;

    impl ForcePrepareFailureGuard {
        fn new() -> Self {
            FORCE_PREPARE_FAILURE.with(|flag| flag.set(true));
            Self
        }
    }

    impl Drop for ForcePrepareFailureGuard {
        fn drop(&mut self) {
            FORCE_PREPARE_FAILURE.with(|flag| flag.set(false));
        }
    }

    #[test]
    fn execute_shell_fails_closed_when_enforcement_setup_fails() {
        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let marker = temp.path().join("spawned-anyway.marker");
        let script = format!("echo ran > '{}'", marker.display());

        {
            let _guard = ForcePrepareFailureGuard::new();
            let error = crate::shell::execute_shell(
                "bash",
                &["-c", &script],
                &[],
                temp.path(),
                &policy,
                &ShellEnforcement::environment_only(),
            )
            .unwrap_err();
            assert!(
                error.contains("sandbox"),
                "the error must name what failed to initialize: {error}"
            );
            assert!(
                !marker.exists(),
                "no subprocess may be spawned at all when enforcement setup fails"
            );
        }

        // Control run without the forced failure: the identical call spawns and writes the
        // marker — proving the marker really would have caught a spawn above.
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(marker.exists());
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod linux_integration_tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn prepare_enforcement_is_noop_for_environment_only_tier_even_on_linux() {
        let mut command = std::process::Command::new("true");
        let enforcement = ShellEnforcement::environment_only();
        let supervisor = prepare_enforcement(&mut command, &enforcement, Path::new("/tmp"))
            .expect("environment_only must never fail");
        supervisor.join_best_effort();
    }

    #[test]
    fn kernel_tier_denies_exec_outside_shell_allowlist() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_denies_exec_outside_shell_allowlist: this Linux host \
                 resolved to EnforcementTier::EnvironmentOnly, which should not happen — but \
                 degrade gracefully rather than fail the suite"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        let result = crate::shell::execute_shell(
            "bash",
            &["-c", "id"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok with a nonzero exit code, not Err");

        assert_ne!(
            result.exit_code, 0,
            "`id` is not in shell_allow, so the seccomp exec-notify supervisor must deny its \
             execve, making bash report a nonzero exit code"
        );
    }

    #[test]
    fn kernel_tier_allows_exec_within_shell_allowlist() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_allows_exec_within_shell_allowlist: resolved to \
                 EnvironmentOnly on a Linux host — degrading gracefully"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        let result = crate::shell::execute_shell(
            "bash",
            &["-c", "exit 7"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_eq!(
            result.exit_code, 7,
            "bash itself is allowlisted, so kernel enforcement must permit its own execve — \
             proving the allowlist isn't a blanket deny"
        );
    }

    #[test]
    fn kernel_tier_allows_nested_exec_of_second_allowlisted_binary() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_allows_nested_exec_of_second_allowlisted_binary: \
                 resolved to EnvironmentOnly on a Linux host — degrading gracefully"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "ls".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        // Unlike `exit 7` (a builtin), `ls` forces a genuinely *nested* execve from inside
        // bash — the case the no-regression scenario ("allowlisted binary still functions")
        // actually cares about.
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", "ls"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_eq!(
            result.exit_code, 0,
            "a second allowlisted binary exec'd from inside allowlisted bash must succeed \
             (stderr: {})",
            result.stderr
        );
    }

    #[test]
    fn kernel_tier_denies_network_connect_outside_allowlist() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_denies_network_connect_outside_allowlist: resolved to \
                 EnvironmentOnly on a Linux host — degrading gracefully"
            );
            return;
        }

        // A real listener, so the destination port is genuinely open — the failure below must
        // come from the seccomp supervisor's deny (EACCES on connect), not from an
        // incidental ECONNREFUSED against a closed port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            // Empty resolved allowlist: every destination must be denied.
            network_allow_ips: Vec::new(),
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        // bash's /dev/tcp is a builtin socket+connect — no extra binary needed, no DNS.
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_ne!(
            result.exit_code, 0,
            "connect() to a destination outside the resolved network allowlist must be \
             denied by the seccomp supervisor even though the port is really open"
        );
    }

    #[test]
    fn kernel_tier_allows_network_connect_to_allowlisted_ip() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_allows_network_connect_to_allowlisted_ip: resolved to \
                 EnvironmentOnly on a Linux host — degrading gracefully"
            );
            return;
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: vec![std::net::IpAddr::from([127, 0, 0, 1])],
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_eq!(
            result.exit_code, 0,
            "connect() to an allowlisted destination must succeed — enforcement adds \
             denials for out-of-policy actions, it does not degrade permitted ones \
             (stderr: {})",
            result.stderr
        );
    }

    #[test]
    fn kernel_full_denies_filesystem_access_outside_workdir() {
        let tier = detect_enforcement_tier();
        if tier != EnforcementTier::KernelFull {
            eprintln!(
                "skipping kernel_full_denies_filesystem_access_outside_workdir: requires \
                 EnforcementTier::KernelFull (Landlock ABI, kernel 5.13+); detected {tier:?} \
                 instead"
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
        };

        let result = crate::shell::execute_shell(
            "bash",
            &["-c", "cat /etc/hostname"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_ne!(
            result.exit_code, 0,
            "reading a file outside the Landlock-scoped workdir must fail under KernelFull"
        );
    }
}
