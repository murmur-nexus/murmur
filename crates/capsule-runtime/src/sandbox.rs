//! Kernel-level enforcement for the shell-subprocess tree spawned by `shell::execute_shell`.
//!
//! Today `execute_shell` is gated only by app-level checks (`policy.shell_allow` string
//! equality, and `shell::build_shell_env`'s synthetic-HOME/credential-stripping). Nothing
//! stops the spawned process — or anything it execs/forks — from making arbitrary syscalls:
//! reading files outside its workdir, or connecting to hosts outside `policy.network_allow`.
//! This module closes that gap for the shell subprocess tree specifically, using two Linux
//! kernel primitives:
//!
//!   - **Landlock `Execute` rights** to allowlist exec by *path*, decided by the kernel on the
//!     path it resolved itself. Each `shell.allow` binary — plus its ELF interpreter and its
//!     `DT_NEEDED` closure — gets a narrow read+execute grant (`resolve_landlock_grants`), and the
//!     capsule's own workdir gets `Execute` **only** when the manifest declares
//!     `capabilities.filesystem.workdir_exec: true` (`linux_enforce::workdir_access_rights`). With
//!     the default, nothing the capsule writes into its workdir can run under any name, so the
//!     rename-to-an-allowlisted-basename bypass has no path left. This replaced a seccomp-notify
//!     exec supervisor that read the invoked pathname out of `/proc/<pid>/mem` and answered
//!     `CONTINUE`, which `seccomp_unotify(2)` documents as inherently racy — see
//!     `docs/content/reference/seccomp-notify-toctou-audit.md` for the audit that retired it.
//!     There is no seccomp-notify mechanism left in this module at all: `execve`/`execveat` are
//!     ordinary allowed syscalls, decided entirely by the Landlock domain they run in.
//!   - **classic seccomp-bpf argument matching** (`SECCOMP_RET_ERRNO`) on `socket(2)`'s `domain`,
//!     to refuse whole address families outright — see `denied_socket_domains`. Unlike a
//!     destination `sockaddr`, `domain` is an integer in a register, so the kernel's own BPF can
//!     compare it with no userspace round-trip and no notification at all. This is what stops a
//!     capsule reaching `/var/run/docker.sock` (host root) over `AF_UNIX`, which no other
//!     mechanism here covers: a network namespace does not mediate `AF_UNIX` at all, and Landlock
//!     ABI v1 does not mediate an abstract-namespace socket. `AF_NETLINK`/`AF_PACKET` are denied
//!     unconditionally; `AF_UNIX` is denied unless `capabilities.network.unix_sockets` says
//!     otherwise.
//!   - **a default-deny syscall allowlist** modelled on the OCI/Docker default seccomp profile —
//!     see `SECCOMP_SYSCALL_ALLOWLIST`. The filter's default action is `SECCOMP_RET_ERRNO(EPERM)`,
//!     so a syscall named by neither that array nor one of the rules above is refused outright,
//!     with no argument inspection at all. This is what puts `io_uring_*` (historically an
//!     LSM-path-hook bypass, and so a candidate route around Landlock itself), `bpf`,
//!     `userfaultfd`, `perf_event_open`, `ptrace` and the rest of `SECCOMP_MUST_STAY_DENIED` out
//!     of a capsule's reach. The filter also sets `SCMP_FLTATR_CTL_LOG`, so each denial reaches
//!     the kernel audit trail with a syscall number, pid and comm.
//!   - **Landlock LSM** to scope filesystem access to the capsule's `workdir`. The workdir's own
//!     grant deliberately withholds `MakeChar`/`MakeBlock`/`MakeSock`, and withholds `Execute`
//!     unless `capabilities.filesystem.workdir_exec` is declared — see
//!     `linux_enforce::WORKDIR_ACCESS_RIGHTS_NO_EXEC` and `linux_enforce::workdir_access_rights`.
//!     Outside the workdir, alongside the derived
//!     read+execute grants, a fixed three-device set is granted unconditionally — see
//!     `CAPSULE_DEVICE_GRANTS`: `/dev/null` read+write (the only writable path outside the
//!     workdir), `/dev/zero` and `/dev/urandom` read-only, and no other device at all.
//!
//! Both Linux tiers additionally strip the forked child's **Linux capabilities** (bounding set,
//! then permitted/effective/inheritable, then `no_new_privs`) before `execve()` — see
//! `linux_enforce::drop_all_capabilities`. Landlock and the capability model are independent,
//! both-must-allow gates: a root-uid capsule keeps `CAP_MKNOD` no matter what Landlock permits,
//! so the Landlock narrowing above does not subsume this.
//!
//! Both are Linux-only. macOS (and any other non-Linux target) has no equivalent kernel
//! primitive and permanently falls back to the existing, unmodified synthetic-HOME/env-
//! stripping mechanism in `shell.rs` — see `EnforcementTier::EnvironmentOnly`.
//!
//! ## Four-tier model
//!
//! - `KernelSealed` (Linux, everything `KernelFull` has, plus a usable unprivileged user+mount
//!   namespace — AppArmor out of the way, `unshare` + `mount` verified by a real probe): the
//!   subprocess tree additionally runs inside a private mount namespace pivoted onto a composed
//!   root, so paths outside it are *absent* rather than denied. Landlock and seccomp still install,
//!   inside the new root, as defence in depth — see [`crate::sealed`]. Only capsules that declare
//!   `capabilities.containment: sealed` get this; see `applied_tier`.
//! - `KernelFull` (Linux, Landlock ABI available — kernel 5.13+): seccomp syscall allowlist +
//!   socket-domain denial + Landlock filesystem scoping, which is also what makes
//!   `capabilities.shell.allow` enforceable (exec is a Landlock right, not a syscall filter).
//! - `KernelSeccompOnly` (Linux, Landlock unavailable — kernel <5.13): seccomp syscall allowlist +
//!   socket-domain denial only. Filesystem scope stays convention-only (`current_dir`) — a
//!   documented gap, not a bug — and, since this slice retired the exec supervisor, so does
//!   `shell.allow`: with no Landlock domain there is no kernel-level exec mediation on this tier
//!   at all. `W-SEC-002` says so. The syscall allowlist and the socket-domain denial are identical
//!   on both tiers: they are seccomp rules, so they do not depend on Landlock in any way.
//! - `EnvironmentOnly` (macOS / any non-Linux target): no kernel primitive attempted at all.
//!   Permanent, not a placeholder for a future slice.
//!
//! Tier detection is always a runtime capability probe (attempt Landlock ruleset
//! construction, inspect the resulting `RulesetStatus`; fork a child and really call
//! `unshare(CLONE_NEWUSER | CLONE_NEWNS)` followed by a `mount(2)`) — never a hardcoded
//! kernel-version string parse, which is fragile against distro backports, and never a config
//! file. The probes run once per process and are cached; see `host_probe`.
//!
//! ## Fail-closed invariant
//!
//! If kernel enforcement setup fails unexpectedly on a Linux host (not the expected
//! "Landlock unsupported, degrade to `KernelSeccompOnly`" signal, but something like seccomp
//! filter install failing, or the namespace-socket handshake failing before spawn) —
//! `prepare_enforcement` returns `Err` and `execute_shell` must not call `.spawn()` at all.
//! There is no code path where a Linux host silently runs a shell subprocess with zero
//! enforcement because setup failed.

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use murmur_artifact::security_warnings::{
    security_warning_link, W_SEC_001, W_SEC_002, W_SEC_005, W_SEC_010,
};
use murmur_artifact::InterpreterRuntimeGrant;

use crate::types::CapabilityPolicy;

/// Kernel-enforcement tier for the current host, in descending order of enforcement
/// strength. Always host-probed at launch time — never sourced from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnforcementTier {
    /// Linux, and everything `KernelFull` has *plus* a private mount namespace pivoted onto a
    /// composed root (see [`crate::sealed`]): the shipped AppArmor profile is loaded (or AppArmor
    /// imposes no unprivileged-userns restriction), a real `unshare` + `mount` probe succeeded,
    /// and the Landlock ABI is usable. Paths outside the composed root are *absent*, not merely
    /// denied — and Landlock and seccomp still install inside it, as defence in depth.
    ///
    /// Ranked first because it is the strongest tier; the enum is documented weakest-last and
    /// carries no `Ord`, so this position is documentation, not a comparison.
    KernelSealed,
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

/// Pure decision: given whether the host is Linux, the outcome of a Landlock
/// ruleset-restriction probe, and what [`crate::sealed::probe_sealed_support`] found, decide the
/// tier. No syscalls here — fully unit-testable on any OS.
///
/// `KernelSealed` requires the whole conjunction — Linux, a usable Landlock ABI, AppArmor out of
/// the way, and a namespace probe that really created one. Any missing element falls back to the
/// tier that element still supports; nothing here ever reports a mechanism the host did not
/// demonstrate.
pub(crate) fn tier_from_probe(
    is_linux: bool,
    landlock_fully_enforced: Option<bool>,
    sealed: crate::sealed::SealedProbe,
) -> EnforcementTier {
    if !is_linux {
        return EnforcementTier::EnvironmentOnly;
    }
    match landlock_fully_enforced {
        Some(true)
            if sealed.apparmor_permits_userns
                && sealed.namespace == crate::sealed::NamespaceProbe::Ok =>
        {
            EnforcementTier::KernelSealed
        }
        Some(true) => EnforcementTier::KernelFull,
        Some(false) | None => EnforcementTier::KernelSeccompOnly,
    }
}

/// The tier this *session* actually applies, which is never stronger than the host can back and
/// never stronger than the capsule asked for.
///
/// A capsule declaring `scoped` on a sealed-capable host keeps running exactly as it does today:
/// the composed root would otherwise silently delete host paths its `interpreter_runtime` grants
/// legitimately point at, which is a regression to `scoped`'s behaviour dressed up as extra
/// security. The declared floor is honoured, not merely met.
///
/// It is deliberately *not* symmetric with the achieved class recorded in the trace: `achieved`
/// answers "what can this host back" (a host fact) and is probed independently, while this answers
/// "what did this session install" (a session fact).
pub(crate) fn applied_tier(
    host_tier: EnforcementTier,
    declared: murmur_artifact::ContainmentClass,
) -> EnforcementTier {
    match host_tier {
        EnforcementTier::KernelSealed
            if declared < murmur_artifact::ContainmentClass::Sealed =>
        {
            EnforcementTier::KernelFull
        }
        other => other,
    }
}

/// The three host facts the tier decision is made from, probed once per process.
///
/// Cached because two of the three probes have a cost worth paying once and not per session: the
/// Landlock probe places the runtime process into a (fully permissive) Landlock domain, and the
/// namespace probe forks a child.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct HostProbe {
    landlock_fully_enforced: Option<bool>,
    sealed: crate::sealed::SealedProbe,
}

#[cfg(target_os = "linux")]
fn host_probe() -> HostProbe {
    static PROBE: std::sync::OnceLock<HostProbe> = std::sync::OnceLock::new();
    *PROBE.get_or_init(|| HostProbe {
        landlock_fully_enforced: linux_enforce::probe_landlock_full_access(),
        sealed: crate::sealed::probe_sealed_support(),
    })
}

/// Real host probe. On Linux, attempts to build a Landlock ruleset and call
/// `.restrict_self()`, mapping `RulesetStatus::FullyEnforced` to `Some(true)` and anything
/// else (`PartiallyEnforced`/`NotEnforced`, or any construction error) to `Some(false)`,
/// probes the sealed mechanism, then delegates to `tier_from_probe`. Off Linux, never probes
/// anything.
#[cfg(target_os = "linux")]
pub(crate) fn detect_enforcement_tier() -> EnforcementTier {
    let probe = host_probe();
    tier_from_probe(true, probe.landlock_fully_enforced, probe.sealed)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn detect_enforcement_tier() -> EnforcementTier {
    tier_from_probe(false, None, crate::sealed::probe_sealed_support())
}

/// Which single mechanism keeps this host below `sealed`, or `None` when nothing does.
///
/// Reads the same cached probe [`detect_enforcement_tier`] does, so the refusal an operator sees
/// and the tier the runtime installs can never disagree about the host.
#[cfg(target_os = "linux")]
pub fn detect_sealed_blocker() -> Option<crate::sealed::SealedBlocker> {
    let probe = host_probe();
    crate::sealed::sealed_blocker(
        true,
        probe.landlock_fully_enforced == Some(true),
        probe.sealed,
    )
}

#[cfg(not(target_os = "linux"))]
pub fn detect_sealed_blocker() -> Option<crate::sealed::SealedBlocker> {
    crate::sealed::sealed_blocker(false, false, crate::sealed::probe_sealed_support())
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
/// This canonical set — not the raw name strings — is what `resolve_landlock_grants` derives the
/// narrow read+execute Landlock grants from, so `cp /usr/bin/nc ./bash && ./bash` is denied: the
/// copy lives under the workdir, which carries no `Execute` right by default, and the grant that
/// does carry one names the launch-time `bash`'s own inode, not a basename.
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

/// Resolves the program name a shell tool invoked to the canonical filesystem path of the
/// binary that `execvp` would actually run, for reporting purposes: `shell-event.binary`
/// on the hook side and the `binary` key of `trace.jsonl`'s `shell` record.
///
/// Unlike [`resolve_exec_allowlist`] — which collects *every* `PATH` match because each one needs
/// its own Landlock grant — this returns the **first** match, which is the one `execvp` picks and
/// therefore the one that ran. Resolution rules are otherwise identical (same `PATH` source, same
/// [`is_executable_file`] test, same `canonicalize`), so a reported path is always a member of the
/// set the Landlock grants were derived from.
///
/// A name that resolves to nothing falls back to the bare invoked name, unchanged — never
/// an error, never a panic, matching `resolve_exec_allowlist`'s "entries that resolve to
/// nothing are silently skipped" precedent. Observability must not be able to fail a shell
/// call that the OS is perfectly willing to run.
pub(crate) fn resolve_invoked_binary_path(binary: &str) -> String {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    resolve_invoked_binary_path_in(binary, &path_dirs)
}

/// Testable core of [`resolve_invoked_binary_path`], with the `PATH` directory list
/// injected.
fn resolve_invoked_binary_path_in(binary: &str, path_dirs: &[PathBuf]) -> String {
    let fallback = || binary.to_string();
    if binary.is_empty() {
        return fallback();
    }
    if binary.contains('/') {
        return match std::fs::canonicalize(binary) {
            Ok(canonical) => canonical.to_string_lossy().into_owned(),
            Err(_) => fallback(),
        };
    }
    for dir in path_dirs {
        let candidate = dir.join(binary);
        if is_executable_file(&candidate) {
            if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                return canonical.to_string_lossy().into_owned();
            }
        }
    }
    fallback()
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ---- derived Landlock read+execute grant set (this slice) ----
//
// On `KernelFull`, Landlock scopes the shell subprocess tree to the workdir. The workdir rule
// grants `linux_enforce::WORKDIR_ACCESS_RIGHTS` (everything in ABI v1 except device-node and
// unix-socket creation), but — once `restrict_self()` lands — `Execute`/`ReadFile` are then denied
// on every path *outside* the workdir, including the `shell.allow` binaries themselves, their ELF
// interpreter (dynamic loader), and every shared library they pull in. That silently defeats the
// seccomp exec-allowlist: each allowlisted `execve` fails with EACCES before it runs.
//
// The functions below derive the exact extra paths that must get a narrow read+execute grant
// outside the workdir: the canonical `shell.allow` binaries (already resolved by
// `resolve_exec_allowlist`), each one's `PT_INTERP`, and the transitive closure of the shared
// libraries their ELF `DT_NEEDED` entries name (resolved via `DT_RPATH`/`DT_RUNPATH` first, then a
// fixed set of standard library directories). Nothing broader — no directory is granted wholesale.
//
// All of this is *parsing and filesystem resolution*, so it runs in the parent at launch time
// (inside `ShellEnforcement::resolve`) and the resulting `Vec<PathBuf>` is threaded into the
// forked child's `pre_exec`, which only opens each path and adds a Landlock rule (no parsing in
// the async-signal-safe window). Every resolution step is shrink-not-fail, matching
// `resolve_exec_allowlist`: an entry that does not exist, does not parse as ELF, or names a
// soname that does not resolve simply contributes nothing further — it never errors out.

/// Standard Linux shared-library search directories used as the fallback resolution set for a
/// `DT_NEEDED` soname when the binary declares no `DT_RPATH`/`DT_RUNPATH` (or it doesn't match).
/// Covers both of this repo's platform targets (`linux/x86_64` and `linux/aarch64`), including the
/// Debian/Ubuntu multiarch-triplet subdirectories. A later slice or a reviewer targeting a
/// platform this list didn't anticipate can extend it here — it is the single source of truth.
const DEFAULT_LIBRARY_SEARCH_DIRS: &[&str] = &[
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
];

/// The subset of an ELF binary's dynamic-linking metadata that decides which extra files the
/// dynamic loader will touch at load time: the interpreter path (`PT_INTERP`), the direct
/// shared-library dependencies (`DT_NEEDED` sonames), and the runtime library search paths
/// (`DT_RPATH`/`DT_RUNPATH`, already split on `:`, `$ORIGIN` left unexpanded here).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ElfDependencies {
    pub(crate) interp: Option<String>,
    pub(crate) needed: Vec<String>,
    pub(crate) runpaths: Vec<String>,
}

fn read_u16(bytes: &[u8], off: usize, le: bool) -> Option<u16> {
    let raw: [u8; 2] = bytes.get(off..off + 2)?.try_into().ok()?;
    Some(if le { u16::from_le_bytes(raw) } else { u16::from_be_bytes(raw) })
}

fn read_u32(bytes: &[u8], off: usize, le: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(off..off + 4)?.try_into().ok()?;
    Some(if le { u32::from_le_bytes(raw) } else { u32::from_be_bytes(raw) })
}

fn read_u64(bytes: &[u8], off: usize, le: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(off..off + 8)?.try_into().ok()?;
    Some(if le { u64::from_le_bytes(raw) } else { u64::from_be_bytes(raw) })
}

/// Reads a NUL-terminated string starting at the beginning of `region` (used for both the
/// `PT_INTERP` segment and dynamic-string-table entries).
fn cstr_at_start(region: &[u8]) -> String {
    let nul = region.iter().position(|&b| b == 0).unwrap_or(region.len());
    String::from_utf8_lossy(&region[..nul]).into_owned()
}

/// Maps a virtual address into a file offset using the ELF's `PT_LOAD` segments — required
/// because `DT_STRTAB` records the string table's virtual address, not its file offset.
fn vaddr_to_file_offset(vaddr: u64, loads: &[(u64, u64, u64)]) -> Option<u64> {
    for &(seg_vaddr, seg_off, seg_filesz) in loads {
        if vaddr >= seg_vaddr && vaddr < seg_vaddr.checked_add(seg_filesz)? {
            return Some(seg_off + (vaddr - seg_vaddr));
        }
    }
    None
}

/// Pure ELF64 parser: extracts the dynamic-linking metadata that determines which extra files the
/// loader touches. Returns `None` for anything that is not a well-formed ELF64 image (bad magic,
/// 32-bit, truncated headers) — a non-ELF or unparseable binary simply contributes nothing to the
/// grant set (shrink-not-fail). Deliberately hand-rolled (no crate dependency) and byte-slice-in /
/// struct-out so it is fully unit-testable off-Linux with synthetic fixtures; it makes no syscalls.
///
/// Only ELF64 is parsed: both of this repo's Linux targets (`x86_64`, `aarch64`) are 64-bit, so a
/// 32-bit image is not a binary this runtime would exec and is treated as unparseable.
pub(crate) fn parse_elf_dependencies(bytes: &[u8]) -> Option<ElfDependencies> {
    // e_ident (16) + fixed ELF64 header fields extend to offset 64.
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" {
        return None;
    }
    // EI_CLASS: 2 == ELFCLASS64. EI_DATA: 1 == little-endian, 2 == big-endian.
    if bytes[4] != 2 {
        return None;
    }
    let le = match bytes[5] {
        1 => true,
        2 => false,
        _ => return None,
    };

    let e_phoff = read_u64(bytes, 32, le)? as usize;
    let e_phentsize = read_u16(bytes, 54, le)? as usize;
    let e_phnum = read_u16(bytes, 56, le)? as usize;
    // Each Elf64_Phdr is 56 bytes; a smaller entsize means this is not the layout we parse.
    if e_phentsize < 56 {
        return None;
    }

    let mut interp: Option<String> = None;
    let mut dynamic_region: Option<(u64, u64)> = None; // (file offset, size)
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (p_vaddr, p_offset, p_filesz)

    for i in 0..e_phnum {
        let base = e_phoff.checked_add(i.checked_mul(e_phentsize)?)?;
        let p_type = read_u32(bytes, base, le)?;
        let p_offset = read_u64(bytes, base + 8, le)?;
        let p_vaddr = read_u64(bytes, base + 16, le)?;
        let p_filesz = read_u64(bytes, base + 32, le)?;
        match p_type {
            1 => loads.push((p_vaddr, p_offset, p_filesz)), // PT_LOAD
            2 => dynamic_region = Some((p_offset, p_filesz)), // PT_DYNAMIC
            3 => {
                // PT_INTERP: a NUL-terminated interpreter path in the file image.
                let start = p_offset as usize;
                let end = start.checked_add(p_filesz as usize)?;
                interp = Some(cstr_at_start(bytes.get(start..end)?));
            }
            _ => {}
        }
    }

    // A static binary (no PT_DYNAMIC) contributes only itself — no interp, no needed libs.
    let Some((dyn_off, dyn_size)) = dynamic_region else {
        return Some(ElfDependencies { interp, needed: Vec::new(), runpaths: Vec::new() });
    };

    let mut needed_offsets: Vec<u64> = Vec::new();
    let mut runpath_offsets: Vec<u64> = Vec::new();
    let mut strtab_vaddr: Option<u64> = None;
    let mut strsz: Option<u64> = None;

    let mut off = dyn_off as usize;
    let dyn_end = off.checked_add(dyn_size as usize)?;
    while off + 16 <= dyn_end {
        let d_tag = read_u64(bytes, off, le)?;
        let d_val = read_u64(bytes, off + 8, le)?;
        match d_tag {
            0 => break,                             // DT_NULL — end of dynamic array
            1 => needed_offsets.push(d_val),        // DT_NEEDED
            5 => strtab_vaddr = Some(d_val),        // DT_STRTAB (virtual address)
            10 => strsz = Some(d_val),              // DT_STRSZ
            15 | 29 => runpath_offsets.push(d_val), // DT_RPATH | DT_RUNPATH
            _ => {}
        }
        off += 16;
    }

    let mut needed = Vec::new();
    let mut runpaths = Vec::new();
    if let (Some(strtab_vaddr), Some(strsz)) = (strtab_vaddr, strsz) {
        if let Some(strtab_off) = vaddr_to_file_offset(strtab_vaddr, &loads) {
            let start = strtab_off as usize;
            // Prefer the declared string-table bounds, but tolerate an over-long DT_STRSZ by
            // falling back to the rest of the file rather than dropping every soname.
            let strtab = start
                .checked_add(strsz as usize)
                .and_then(|end| bytes.get(start..end))
                .or_else(|| bytes.get(start..));
            if let Some(strtab) = strtab {
                for offset in needed_offsets {
                    if let Some(region) = strtab.get(offset as usize..) {
                        needed.push(cstr_at_start(region));
                    }
                }
                for offset in runpath_offsets {
                    if let Some(region) = strtab.get(offset as usize..) {
                        for part in cstr_at_start(region).split(':') {
                            if !part.is_empty() {
                                runpaths.push(part.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    Some(ElfDependencies { interp, needed, runpaths })
}

/// Expands a leading/embedded `$ORIGIN` (or `${ORIGIN}`) in an rpath entry to the directory of the
/// binary that declared it — the dynamic loader's own rule for `DT_RUNPATH`/`DT_RPATH`.
fn expand_origin(rpath: &str, origin_dir: &Path) -> PathBuf {
    if rpath.contains("$ORIGIN") {
        let origin = origin_dir.to_string_lossy();
        PathBuf::from(
            rpath
                .replace("${ORIGIN}", &origin)
                .replace("$ORIGIN", &origin),
        )
    } else {
        PathBuf::from(rpath)
    }
}

/// Resolves one `DT_NEEDED` soname to a canonical file path, mirroring the loader's search order:
/// a soname containing a `/` is taken as a path (absolute, or relative to the binary's directory);
/// otherwise the binary's own `DT_RUNPATH`/`DT_RPATH` entries (with `$ORIGIN` expanded) are tried
/// first, then the standard `search_dirs`. Returns `None` (shrink-not-fail) if nothing matches.
pub(crate) fn resolve_soname(
    soname: &str,
    origin_dir: &Path,
    runpaths: &[String],
    search_dirs: &[PathBuf],
) -> Option<PathBuf> {
    if soname.is_empty() {
        return None;
    }
    if soname.contains('/') {
        let candidate = if Path::new(soname).is_absolute() {
            PathBuf::from(soname)
        } else {
            origin_dir.join(soname)
        };
        return std::fs::canonicalize(&candidate).ok();
    }
    for rpath in runpaths {
        let candidate = expand_origin(rpath, origin_dir).join(soname);
        if candidate.is_file() {
            if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                return Some(canonical);
            }
        }
    }
    for dir in search_dirs {
        let candidate = dir.join(soname);
        if candidate.is_file() {
            if let Ok(canonical) = std::fs::canonicalize(&candidate) {
                return Some(canonical);
            }
        }
    }
    None
}

/// Derives the full set of canonical file paths that need a narrow read+execute Landlock grant
/// *outside* the workdir so the `shell.allow` binaries can actually run: each allowlisted binary,
/// its ELF interpreter, and the transitive closure of its `DT_NEEDED` shared libraries. Uses the
/// standard [`DEFAULT_LIBRARY_SEARCH_DIRS`] as the soname fallback search set.
///
/// Every step is shrink-not-fail (a binary that can't be read/parsed, or a soname that doesn't
/// resolve, contributes nothing further); the returned set is deduplicated and canonical. Runs in
/// the parent at launch time — never in the forked child's `pre_exec`.
pub(crate) fn resolve_landlock_grants(exec_allow_paths: &[PathBuf]) -> Vec<PathBuf> {
    let search_dirs: Vec<PathBuf> = DEFAULT_LIBRARY_SEARCH_DIRS
        .iter()
        .map(PathBuf::from)
        .collect();
    resolve_landlock_grants_in(exec_allow_paths, &search_dirs)
}

/// Testable core of [`resolve_landlock_grants`], with the library search directories injected so
/// unit tests can point it at synthetic fixtures instead of the real system directories.
fn resolve_landlock_grants_in(
    exec_allow_paths: &[PathBuf],
    search_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut grants: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut visited: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();

    for path in exec_allow_paths {
        // The allowlist paths are already canonical/existing (from `resolve_exec_allowlist`), but
        // canonicalize defensively — this both makes dedup against interp/library paths exact and
        // drops any seed that no longer exists (shrink-not-fail: a vanished binary contributes
        // nothing, and could not be exec'd anyway).
        if let Ok(canonical) = std::fs::canonicalize(path) {
            queue.push_back(canonical);
        }
    }

    while let Some(path) = queue.pop_front() {
        if !visited.insert(path.clone()) {
            continue;
        }
        // The binary/library itself is granted. If it lives inside the workdir, the workdir's own
        // full-access rule already covers it and this extra read+execute rule is simply redundant.
        grants.insert(path.clone());

        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(deps) = parse_elf_dependencies(&bytes) else {
            continue;
        };
        let origin_dir = path.parent().unwrap_or_else(|| Path::new("/"));

        if let Some(interp) = &deps.interp {
            if let Ok(canonical) = std::fs::canonicalize(interp) {
                queue.push_back(canonical);
            }
        }
        for soname in &deps.needed {
            if let Some(resolved) = resolve_soname(soname, origin_dir, &deps.runpaths, search_dirs) {
                queue.push_back(resolved);
            }
        }
    }

    grants.into_iter().collect()
}

/// One resolved Landlock filesystem grant outside the workdir: a canonical path plus the two
/// independent bits that decide the rule's access set.
///
/// `list_dir` decides enumerability (`ReadDir`):
///
///   - `false` → no `getdents64` on the granted directory itself. Landlock's read rights apply to
///     the whole subtree beneath a granted directory, so a file *inside* it can still be opened by
///     its exact name — the listing is what is denied. This is what every derived
///     `DT_NEEDED`-closure grant gets — they are individual files, where `ReadDir` was always a
///     no-op anyway (Landlock's `ReadDir` only has meaning on a directory inode).
///   - `true` → adds enumerability, which a path-based interpreter's import machinery needs on
///     each `sys.path` entry (CPython's `FileFinder` `listdir`-caches each one). Set by an author
///     writing `list_dir: true` next to a specific `interpreter_runtime` directory, and by the two
///     whole-tree resolvers ([`resolve_staged_runtime_landlock_grants`] and
///     [`resolve_sealed_runtime_landlock_grants`]) where a runtime tree is walked by definition —
///     never inferred from anything else, and never applied to an ancestor or sibling of a granted
///     directory.
///
/// `executable` decides `Execute`, and it is a real access decision rather than a formality
/// because **`Execute` is this runtime's exec allowlist**: the seccomp `execve` supervisor was
/// retired in favour of Landlock `Execute` rights, so a path with no `Execute` rule cannot be
/// `execve`d at all, and a path with one can. That makes the bit sharply asymmetric:
///
///   - `true` → `Execute + ReadFile`, the right shape for a path the manifest asked to *run*: a
///     `shell.allow` binary, its `DT_NEEDED` closure, and the interpreter/staged trees an author
///     named explicitly (each of which is a runtime whose helper binaries are the point of
///     declaring it).
///   - `false` → `ReadFile` only. Readable and — with `list_dir` — enumerable, but not runnable.
///     Reserved for a grant the *tier* issues rather than the manifest: see
///     [`resolve_sealed_runtime_landlock_grants`], where granting `Execute` over `/usr`, `/bin`
///     and `/sbin` wholesale would quietly turn `shell.allow` into a no-op on `sealed`, since
///     every binary a host ships lives under one of them.
///
/// Note this bit affects `execve` only. A shared object opened `O_RDONLY` and mapped `PROT_EXEC`
/// by `dlopen(3)` needs `ReadFile` and not `Execute` — Landlock checks `Execute` on the open that
/// carries `FMODE_EXEC`, not on the mapping — so an interpreter can still load its own C
/// extensions out of an `executable: false` tree. Verified by hand, not assumed; see
/// `docs/content/reference/sealed-containment-manual-verification.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandlockGrant {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) path: PathBuf,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) list_dir: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) executable: bool,
}

impl LandlockGrant {
    /// Wraps each derived `DT_NEEDED`-closure file path as a non-listable grant. A regular file
    /// has no meaningful `ReadDir`, so `list_dir: false` is both correct and a pure simplification
    /// of b3220cb5's old uniform `Execute|ReadFile|ReadDir` — it never changes *which* files are
    /// granted. `executable: true`: this closure is exactly the set of paths `shell.allow` asked
    /// to run, so it is the allowlist the `Execute` right exists to express.
    fn non_listable_files(paths: Vec<PathBuf>) -> Vec<LandlockGrant> {
        paths
            .into_iter()
            .map(|path| LandlockGrant { path, list_dir: false, executable: true })
            .collect()
    }
}

/// Turns each `capabilities.shell.interpreter_runtime` directory into a [`LandlockGrant`] carrying
/// exactly the `list_dir` its author wrote. Pure and syscall-free (the declared paths need not
/// exist here — `apply_landlock_scope` skips any that fail to open), so it is unit-testable on
/// every platform including a non-Linux dev machine.
///
/// There is deliberately no path here that could widen a grant to a whole install prefix: it emits
/// one grant per explicitly declared directory and copies the author's `list_dir` verbatim,
/// inferring enumerability from nothing.
pub(crate) fn resolve_interpreter_runtime_grants(
    grants: &[InterpreterRuntimeGrant],
) -> Vec<LandlockGrant> {
    grants
        .iter()
        .flat_map(|grant| {
            grant.dirs.iter().map(|dir| LandlockGrant {
                path: PathBuf::from(&dir.path),
                list_dir: dir.list_dir,
                // Executable: an `interpreter_runtime` directory is named by an author precisely so
                // the interpreter it belongs to can run out of it, helper binaries included.
                executable: true,
            })
        })
        .collect()
}

/// The host directories a composed root must carry *beyond* [`crate::sealed::SEALED_RUNTIME_PATHS`]
/// for this capsule's `shell.allow` to keep working: the containing directory of each resolved
/// allowlisted binary, and each declared `interpreter_runtime` directory.
///
/// Whole directories, deduplicated, and only the ones not already inside a fixed runtime path.
/// This is the deliberate opposite of `resolve_landlock_grants`: no ELF parsing, no `DT_NEEDED`
/// closure, no per-file grant — a bind mount carries a directory's whole contents, so the closure
/// derivation `scoped` needs has nothing left to do here.
///
/// Every entry is **optional** by design — see `sealed::PlanBuilder::mirror`. A host missing one
/// makes the composed root narrower rather than refusing the launch, which is why
/// `capabilities.shell.staged_runtime` does *not* come through here: a declared runtime tree must
/// fail closed, so it is resolved separately by [`resolve_staged_runtime_dirs`] and planned as a
/// required bind. This function is additive to that, not replaced by it.
///
/// Pure and syscall-free, so it is unit-testable on any platform.
pub(crate) fn resolve_sealed_bind_dirs(
    exec_allow_paths: &[PathBuf],
    policy: &CapabilityPolicy,
) -> Vec<PathBuf> {
    let fixed: Vec<&Path> = crate::sealed::SEALED_RUNTIME_PATHS
        .iter()
        .map(Path::new)
        .collect();

    let mut dirs: Vec<PathBuf> = Vec::new();
    let candidates = exec_allow_paths
        .iter()
        .filter_map(|binary| binary.parent().map(Path::to_path_buf))
        .chain(
            policy
                .shell_interpreter_runtime
                .iter()
                .flat_map(|grant| grant.dirs.iter().map(|dir| PathBuf::from(&dir.path))),
        );

    for candidate in candidates {
        if !candidate.is_absolute() {
            continue;
        }
        if fixed.iter().any(|root| candidate.starts_with(root)) {
            continue;
        }
        if dirs.contains(&candidate) {
            continue;
        }
        dirs.push(candidate);
    }
    dirs
}

/// The `source_path` of each declared `capabilities.shell.staged_runtime` grant, as the composed
/// root's *required* read-only binds.
///
/// Deliberately not folded into [`resolve_sealed_bind_dirs`], and deliberately not filtered the
/// way that function filters. Both differences follow from the same fact — these fail closed:
///
///   * A path already inside [`crate::sealed::SEALED_RUNTIME_PATHS`] is **not** dropped here.
///     Dropping it would be sound for an optional bind (the fixed list already covers it) but it
///     would quietly convert a required grant into a dependency on an unrelated list staying the
///     way it is today. `plan_composed_root` runs the required loop first and deduplicates by
///     target, so naming an already-covered path costs one registration and stays required.
///   * A non-existent path is **not** filtered out, here or anywhere. The whole point is that a
///     grant naming a tree this host does not have refuses the launch — see
///     `sealed::PlanBuilder::require_bind`.
///
/// Relative paths are still skipped: a relative `source_path` cannot be re-based under the
/// composed root's base at all, so it names nothing to fail *about*. The manifest parser is the
/// layer that rejects one.
///
/// Pure and syscall-free, so it is unit-testable on any platform.
pub(crate) fn resolve_staged_runtime_dirs(policy: &CapabilityPolicy) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for grant in &policy.shell_staged_runtime {
        let path = PathBuf::from(&grant.source_path);
        if !path.is_absolute() || dirs.contains(&path) {
            continue;
        }
        dirs.push(path);
    }
    dirs
}

/// One listable [`LandlockGrant`] per staged runtime directory.
///
/// The bind mount alone does not make a staged tree usable, and the reason is easy to miss: the
/// composed root is not the only thing standing between the subprocess and a path. `sealed` still
/// installs Landlock *inside* the root as defence in depth (see `applied_tier` and the
/// `KernelSealed` arm of `prepare_enforcement`), and Landlock denies any path with no matching
/// rule. A staged tree that is bind-mounted but ungranted is therefore present, read-only, and
/// unreadable — `open()` returns `EACCES` — which is indistinguishable from the capability not
/// working at all.
///
/// So a grant is emitted here for exactly the same directories [`resolve_staged_runtime_dirs`]
/// binds, and the two are resolved from the same list so they cannot drift apart. `list_dir: true`
/// is not configurable, unlike `interpreter_runtime`'s per-directory flag: a staged tree is a whole
/// runtime a program walks (an interpreter enumerating its stdlib, a toolchain resolving its
/// libexec), and the author already made the enumerability decision by naming the tree. Landlock
/// rules attach to the opened inode, so a rule taken on the host path in the parent covers the
/// same inode reached through the bind inside the root.
///
/// Read and execute only — never write. The bind is `MS_RDONLY` regardless, so this is the second
/// of two independent reasons the tree cannot be mutated from inside a session.
///
/// Pure and syscall-free, so it is unit-testable on any platform.
pub(crate) fn resolve_staged_runtime_landlock_grants(
    policy: &CapabilityPolicy,
) -> Vec<LandlockGrant> {
    resolve_staged_runtime_dirs(policy)
        .into_iter()
        .map(|path| LandlockGrant {
            path,
            list_dir: true,
            executable: true,
        })
        .collect()
}

/// One listable [`LandlockGrant`] per [`crate::sealed::SEALED_RUNTIME_PATHS`] entry — and only
/// when this session actually installed a composed root.
///
/// Same gap as [`resolve_staged_runtime_landlock_grants`] closes for a *declared* runtime tree,
/// for the tree nobody declares: `plan_composed_root` bind-mounts `/usr`, `/bin`, `/sbin`, `/lib`…
/// read-only into every composed root unconditionally, but Landlock still installs inside that
/// root and denies any path with no matching rule. The `shell.allow` ELF closure grants those
/// files one at a time and non-listably, so a binary can be opened by exact name while the
/// directory holding it cannot be enumerated. That is precisely enough to start an interpreter and
/// not enough to let it work: CPython dies in `init_fs_encoding` because it cannot `getdents64`
/// `/usr/lib/python3.N` to find `encodings`, and a path-walking runtime of any kind fails the same
/// way. Mounted but unenumerable is indistinguishable from not mounted at all.
///
/// `list_dir: true` is not configurable here, for the same reason it is not configurable for a
/// staged tree: this is a whole runtime tree a program walks, not a case-by-case author choice.
/// Never write; the binds are `MS_RDONLY` regardless, which is the second of two independent
/// reasons the tree cannot be mutated.
///
/// **`executable: false`, and that is not a detail.** This is the one grant in the runtime that
/// withholds `Execute`, because it is also the only one covering a whole host tree that the
/// manifest never named. Since the seccomp `execve` supervisor was retired in favour of Landlock
/// `Execute` rights, an `Execute` rule *is* permission to run: granting it over `/usr`, `/bin` and
/// `/sbin` would make every binary the host ships runnable inside a `sealed` session and reduce
/// `capabilities.shell.allow` to documentation. Measured, not reasoned about — with `Execute` on
/// these paths, `/bin/sh -c 'echo sh-ran'` runs inside a capsule whose allowlist never mentioned
/// `sh`; without it, the same command is `Permission denied` while `import ast` still succeeds.
/// The gap this function closes is enumeration, so enumeration is all it grants.
///
/// **The tier gate is the whole safety argument, so it lives in the resolver rather than at the
/// call site.** `apply_landlock_scope` runs on `KernelFull` as well as `KernelSealed`, and
/// `KernelFull` is the tier a `scoped` capsule runs on — no composed root, Landlock applied
/// straight over the *real host* filesystem. Granting `ReadDir` on `/usr` there would newly expose
/// host directory shape to every `scoped` capsule. Under `KernelSealed` the same grant reveals
/// nothing about the host: the tree being enumerated is a private read-only bind inside a pivoted
/// root, holding only what was staged into it, and everything else is absent rather than denied.
/// So the grants exist for exactly one tier, and returning an empty set everywhere else is the
/// invariant, not an optimisation.
///
/// Pure and syscall-free — the paths need not exist on this host, since `apply_landlock_scope`
/// skips any declared grant path that fails to open — so it is unit-testable on every platform.
pub(crate) fn resolve_sealed_runtime_landlock_grants(tier: EnforcementTier) -> Vec<LandlockGrant> {
    if tier != EnforcementTier::KernelSealed {
        return Vec::new();
    }
    crate::sealed::SEALED_RUNTIME_PATHS
        .iter()
        .map(|path| LandlockGrant {
            path: PathBuf::from(path),
            list_dir: true,
            executable: false,
        })
        .collect()
}

// ---- fixed capsule device set (this slice) ----------------------------------------------
//
// Everything above derives its grants from the manifest: `shell.allow` binaries and their
// `DT_NEEDED` closure, `interpreter_runtime` directories. The device set below derives from
// nothing. It is a fixed property of the sandbox, like `linux_enforce::WORKDIR_ACCESS_RIGHTS`,
// present on every capsule that reaches `KernelFull` — there is no manifest key that adds a
// device, and none that takes one away.
//
// Why it has to exist at all: once `restrict_self()` lands, a path outside the workdir with no
// matching rule is denied, and `/dev/null` is outside every workdir. Programs treat `/dev/null`
// as infallible — `open("/dev/null")` failing is a case almost nothing handles — so without an
// explicit rule an ordinary tool dies in a way that reads as a runtime bug, not a policy denial.

/// One fixed device path granted to every `KernelFull` capsule, with the access level it gets.
///
/// Deliberately *not* a [`LandlockGrant`]: that type means "an executable or library path the
/// manifest asked for, granted read(+list)+execute, never write". A device is a different intent
/// (I/O access to a character device, possibly writable) and a different lifetime (fixed at
/// compile time, not resolved per capsule), so it gets its own type rather than overloading
/// `list_dir` into a third meaning.
///
/// `writable` is the whole reason this type exists as more than a path list:
///
///   - `false` → `ReadFile` only. The device is readable and nothing more.
///   - `true` → `ReadFile | WriteFile`. Granted to exactly one path, `/dev/null`, and it is the
///     only writable path outside the workdir in the entire sandbox.
///
/// Only `mod linux_enforce` consumes this outside of tests, so on a non-Linux build the type and
/// the constant below are dead — the same deliberate cross-platform exception the rest of this
/// module's Linux-only surface carries.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapsuleDeviceGrant {
    pub(crate) path: &'static str,
    pub(crate) writable: bool,
}

/// The complete device set a capsule's shell subprocess tree can reach outside its workdir.
/// Three entries, no more — `apply_landlock_scope` adds one `PathBeneath` rule per entry, and
/// every device path *not* listed here (`/dev/random`, `/dev/full`, `/dev/tty`, `/dev/sda`, …)
/// stays denied by the same mechanism that already denies every other unlisted path: with
/// `handle_access` declaring the full ABI v1 right-set, "no rule" means "denied", not "ungoverned".
///
///   - **`/dev/null` — read *and* write.** This is the one deliberate exception to "nothing
///     outside the workdir is writable". A read-only grant would be broken, not merely strict:
///     bare-host `strace` shows Python's `subprocess.DEVNULL` opening it `O_RDWR|O_CLOEXEC`, and a
///     shell `2>/dev/null` redirect opening it `O_WRONLY|O_CREAT`. Both fail without `WriteFile`,
///     and `subprocess.DEVNULL` is reachable from any Python tool a capsule might run. Granting
///     write on a character device whose write side is defined to discard everything gives up no
///     confidentiality and no integrity — there is no state behind it to reach.
///     (The `O_CREAT` in the redirect costs nothing extra: Landlock only checks `MakeReg` when a
///     file is actually created, and `/dev/null` already exists.)
///   - **`/dev/zero` — read only.** Zero-fill reads and `MAP_PRIVATE` anonymous-mapping fallbacks
///     in older allocators. Writes to it are discarded like `/dev/null`'s, but nothing needs to
///     write there, so it does not get `WriteFile`.
///   - **`/dev/urandom` — read only.** Not for `getrandom(2)`, which is a syscall needing no
///     filesystem grant at all, but because OpenSSL and older glibc paths still `open()` the
///     device outright and fall over if they cannot.
///
/// **Why `/dev/random` is excluded.** Not because writing to it is dangerous: a plain `write()` to
/// `/dev/random` mixes bytes into the pool but credits **zero** entropy — crediting requires the
/// `RNDADDENTROPY` ioctl, which requires `CAP_SYS_ADMIN`, which the shell child has already
/// dropped. It is excluded for a duller reason: since Linux 5.6 `/dev/random` blocks until the CRNG
/// is initialized and `/dev/urandom` does not, and no workload needs the blocking variant when the
/// non-blocking one is granted. It stays out because nothing has demonstrated a need for it.
///
/// Widening this list is allowed but must be evidence-driven: add a fourth entry only after a real
/// workload has been observed failing on the missing device, and record that failure in the same
/// place this list is documented (`docs/content/reference/security-warnings.md`, "Manual acceptance
/// procedure — the fixed capsule device set").
///
/// **Future shape.** Enumerating devices path by path is the `scoped`-containment-class answer.
/// Once a `sealed` class exists, the capsule gets a private `/dev` tmpfs carrying the OCI default
/// device set, and device access stops being a Landlock rule list entirely. A `sealed`
/// implementation should *retire* this constant, not merge with it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const CAPSULE_DEVICE_GRANTS: &[CapsuleDeviceGrant] = &[
    CapsuleDeviceGrant {
        path: "/dev/null",
        writable: true,
    },
    CapsuleDeviceGrant {
        path: "/dev/zero",
        writable: false,
    },
    CapsuleDeviceGrant {
        path: "/dev/urandom",
        writable: false,
    },
];

/// Network decision for one destination IP read out of a notifying task's `sockaddr`.
/// Empty allowlist denies everything (a capsule that declared no `network.allow` hosts has
/// no reason to open any subprocess socket).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn network_ip_allowed(ip: IpAddr, network_allow_ips: &[IpAddr]) -> bool {
    network_allow_ips.contains(&ip)
}

// `socket(2)`'s `domain` argument values, as Linux's ABI defines them. Spelled out as literals
// rather than `libc::AF_*`: the filter these feed is
// always compiled *for a Linux child*, so the numbers must be Linux's regardless of what host the
// build runs on — and `denied_socket_domains` has to stay compilable and unit-testable on a macOS
// dev machine, where `libc::AF_NETLINK` and `libc::AF_PACKET` do not exist at all (the `libc`
// crate defines them only under `linux_like`). `libc::AF_UNIX` does exist on macOS, but taking one
// of the three from `libc` and two from literals would be worse than taking all three the same
// way. Values are stable kernel ABI: `include/linux/socket.h`.
const LINUX_AF_UNIX: i32 = 1;
const LINUX_AF_INET: i32 = 2;
const LINUX_AF_INET6: i32 = 10;
const LINUX_AF_NETLINK: i32 = 16;
const LINUX_AF_PACKET: i32 = 17;

/// The `socket(2)` domains the child's seccomp filter refuses outright, given the capsule's
/// `capabilities.network.unix_sockets` declaration.
///
/// This is the *whole* policy decision behind the `socket()` rule in `install_seccomp_filter` —
/// deliberately pure, `libseccomp`-free, and platform-independent so it can be tested on any dev
/// machine (same split as `resolve_landlock_grants` vs. `apply_landlock_scope`).
///
/// Why a domain check is a classic BPF rule and not a notify:
/// `socket()`'s `domain` is a plain integer in a register, so the kernel's own BPF can compare it
/// directly. That is exactly what `connect()`'s destination address is *not* — it sits behind a
/// pointer BPF cannot dereference, which is why the retired notify supervisor had to read it out of
/// the calling task's memory (see this module's header). Denying the domain at *creation* time
/// needs no userspace round-trip, and is structurally immune to the TOCTOU class of problem that
/// reading a pointed-to argument out of another task's memory invites — which is why this rule
/// survived both retirements unchanged.
///
/// The three families, and why they are not symmetric:
///   - `AF_UNIX` is gated, not banned. `/var/run/docker.sock` is host root, so it cannot be open
///     by default — but "talk to a local daemon socket" is a legitimate thing for an agent to
///     need, so a capsule can declare `capabilities.network.unix_sockets: true` and take it back.
///     Note this is coarse: the opt-in is per *capsule*, per *domain* — not per socket path.
///   - `AF_NETLINK` (routing tables, interface and firewall state) and `AF_PACKET` (raw frame
///     capture and injection) are denied unconditionally, with no manifest key to widen them.
///     Neither has a shell-tool use case comparable to a local daemon socket, and both are
///     strictly more dangerous than one. Widening them would be a deliberate future decision with
///     its own justification, not an oversight to be fixed by adding a flag here.
///
/// `AF_INET`/`AF_INET6` are never denied here: they stay governed by the capsule's own network
/// namespace and egress proxy against `capabilities.network.allow`. This function only ever
/// *adds* denials, and is paired with [`allowed_socket_domains`], which names the families that
/// get a positive rule — since the filter's default action is a deny, a family named by neither
/// function is refused with the filter default (`EPERM`) rather than this rule's `EACCES`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn denied_socket_domains(unix_sockets_allowed: bool) -> Vec<i32> {
    let mut denied = vec![LINUX_AF_NETLINK, LINUX_AF_PACKET];
    if !unix_sockets_allowed {
        denied.push(LINUX_AF_UNIX);
    }
    denied
}

/// The `socket(2)` domains the child's seccomp filter permits, given the capsule's
/// `capabilities.network.unix_sockets` declaration. The exact complement of
/// [`denied_socket_domains`] over the families anything in a shell toolchain actually opens.
///
/// This exists because the filter's default action is `Errno(EPERM)`, not `Allow`: `socket` needs
/// a positive rule or no socket of any family could be created at all. It is deliberately *not*
/// expressed as one unconditional `Allow` rule on `socket` — see `install_seccomp_filter` for why
/// that would silently delete the [`denied_socket_domains`] rules — and deliberately not as a
/// "not any of the denied families" rule either, so the two functions stay a pair of disjoint,
/// equality-matched sets that a test can check against each other.
///
/// The cost of naming families positively is that a family in neither set (`AF_ALG`, `AF_VSOCK`,
/// `AF_BLUETOOTH`, `AF_XDP`, ...) is now refused rather than allowed. That is stricter than
/// Docker's default profile, which allows every family except `AF_ALG`/`AF_VSOCK`; no shell,
/// coreutils, git, compiler or interpreter workload opens one, and the whole point of a
/// default-deny filter is that an unenumerated case fails closed. Add a family here (with a
/// reason) if a real workload needs it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn allowed_socket_domains(unix_sockets_allowed: bool) -> Vec<i32> {
    let mut allowed = vec![LINUX_AF_INET_DOMAIN, LINUX_AF_INET6_DOMAIN];
    if unix_sockets_allowed {
        allowed.push(LINUX_AF_UNIX);
    }
    allowed
}

/// `socket(2)` `domain` values for the two IP families, named apart from the bare `LINUX_AF_*`
/// numbers above so a rule's argument type reads as a domain rather than as a loose integer.
///
/// These used to be derived from a second pair of constants belonging to the `sockaddr` parser the
/// retired `connect`/`sendto` supervisor used. That parser is gone with it, so they are literals
/// from the same `include/linux/socket.h` list as their neighbours now.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET_DOMAIN: i32 = LINUX_AF_INET;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET6_DOMAIN: i32 = LINUX_AF_INET6;

/// The syscalls the child's seccomp filter permits outright, modelled on the OCI/Docker default
/// seccomp profile (reconciled against `containerd`'s `contrib/seccomp/seccomp_default.go`, which
/// is the maintained source of the same list Docker ships).
///
/// The filter's **default action is a deny** (`Errno(EPERM)`), so this array is the whole of what
/// a shell subprocess may call, minus two carve-outs handled by rules rather than by name here:
///
///   - `socket` is absent: it carries argument-conditional rules (see [`denied_socket_domains`] /
///     [`allowed_socket_domains`]), and an unconditional `Allow` rule would discard them.
///
/// `execve`/`execveat` are in this array as ordinary allowed syscalls, which is the visible half of
/// the slice that retired the exec supervisor. They used to carry `Notify` rules so a userspace
/// loop could read the invoked pathname out of the calling task's memory and match it against a
/// canonical allowlist — a decision `seccomp_unotify(2)` documents as inherently racy under
/// `CONTINUE` semantics. `capabilities.shell.allow` is now enforced by the Landlock domain instead
/// (`Execute` on each allowlisted binary's own path, withheld from the workdir unless
/// `capabilities.filesystem.workdir_exec` is declared), so what these rules permit is reaching the
/// kernel — where the LSM decides, on the path it resolved itself. Permitting them here is not
/// optional: the child's very first act after this filter loads is the `execve` that turns it into
/// the tool binary, so a denial here refuses every capsule outright.
///
/// Everything else is refused by the default action, which is the point of the card this list
/// implements: before it, `io_uring_setup`, `bpf`, `userfaultfd`, `perf_event_open`,
/// `open_by_handle_at`, `keyctl` and `ptrace` were all reachable from a capsule shell on bare
/// metal, while the same probe under Docker got `EPERM` for each. `io_uring` is the one that
/// matters most: it has historically bypassed LSM path hooks, so leaving it reachable undermines
/// the Landlock filesystem boundary rather than merely widening the syscall surface. See
/// [`SECCOMP_MUST_STAY_DENIED`] for the full set that must never be added back.
///
/// Names, not numbers: `libseccomp` resolves each one for the architecture the filter is being
/// built on. A name this host's `libseccomp` does not know (a syscall newer than the library) or
/// that does not exist on this architecture (`open`, `dup2`, `poll`, ... on aarch64) is skipped,
/// leaving that syscall denied by default — see `install_seccomp_filter`. That is why legacy
/// x86_64-only spellings sit next to their modern equivalents here: on x86_64 glibc uses `open`
/// and `stat`, on aarch64 it uses `openat` and `newfstatat`, and the same array serves both.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const SECCOMP_SYSCALL_ALLOWLIST: &[&str] = &[
    // ---- process / thread lifecycle ----
    // `execve`/`execveat` reach the kernel; the Landlock domain decides them. See this array's
    // doc comment for the mechanism that replaced their former `Notify` rules.
    "execve",
    "execveat",
    // `clone`/`clone3` are allowed *unconditionally*, which is this filter's one significant
    // widening relative to Docker: Docker masks `clone`'s namespace-creation flags with an
    // argument comparison and forces `clone3` to `ENOSYS` so glibc falls back to the masked
    // `clone`. Reproducing that faithfully is possible for `clone` but pointless in isolation,
    // and the card scopes this slice to a syscall-name allowlist. `unshare`/`setns`/`mount`/
    // `pivot_root`/`chroot` all stay denied, so a new namespace stays largely inert.
    "clone",
    "clone3",
    "fork",
    "vfork",
    "exit",
    "exit_group",
    "wait4",
    "waitid",
    "kill",
    "tkill",
    "tgkill",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "rt_sigsuspend",
    "rt_sigpending",
    "rt_sigtimedwait",
    "rt_sigqueueinfo",
    "rt_tgsigqueueinfo",
    "sigaltstack",
    // The kernel re-issues an interrupted, restartable syscall as `restart_syscall`; denying it
    // would break every `sleep`/`read` that takes a signal.
    "restart_syscall",
    "pause",
    "set_tid_address",
    "set_robust_list",
    "get_robust_list",
    // glibc 2.35+ registers a restartable sequence on every thread start.
    "rseq",
    "prctl",
    "arch_prctl",
    "getpid",
    "getppid",
    "gettid",
    "getpgrp",
    "getpgid",
    "setpgid",
    "getsid",
    "setsid",
    "getpriority",
    "setpriority",
    "ioprio_get",
    "ioprio_set",
    "sched_yield",
    "sched_getaffinity",
    "sched_setaffinity",
    "sched_getparam",
    "sched_setparam",
    "sched_getscheduler",
    "sched_setscheduler",
    "sched_getattr",
    "sched_setattr",
    "sched_get_priority_max",
    "sched_get_priority_min",
    "sched_rr_get_interval",
    "getcpu",
    "capget",
    "capset",
    "prlimit64",
    "getrlimit",
    "setrlimit",
    "getrusage",
    "getitimer",
    "setitimer",
    "times",
    "uname",
    "sysinfo",
    // pidfd process handles: they only ever address a process the caller could already signal.
    "pidfd_open",
    "pidfd_send_signal",
    // ---- memory ----
    "mmap",
    "munmap",
    "mprotect",
    "mremap",
    "madvise",
    "brk",
    "mlock",
    "mlock2",
    "munlock",
    "mlockall",
    "munlockall",
    "membarrier",
    "msync",
    "mincore",
    "memfd_create",
    "pkey_alloc",
    "pkey_free",
    "pkey_mprotect",
    "shmget",
    "shmat",
    "shmdt",
    "shmctl",
    // ---- file I/O ----
    "open",
    "openat",
    "openat2",
    "creat",
    "close",
    "close_range",
    "read",
    "write",
    "pread64",
    "pwrite64",
    "readv",
    "writev",
    "preadv",
    "pwritev",
    "preadv2",
    "pwritev2",
    "lseek",
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "flock",
    "fsync",
    "fdatasync",
    "sync",
    "syncfs",
    "sync_file_range",
    "fadvise64",
    "readahead",
    "truncate",
    "ftruncate",
    "fallocate",
    "stat",
    "fstat",
    "lstat",
    "newfstatat",
    "statx",
    "access",
    "faccessat",
    "faccessat2",
    "getdents",
    "getdents64",
    "getcwd",
    "chdir",
    "fchdir",
    "mkdir",
    "mkdirat",
    "rmdir",
    "unlink",
    "unlinkat",
    "rename",
    "renameat",
    "renameat2",
    "link",
    "linkat",
    "symlink",
    "symlinkat",
    "readlink",
    "readlinkat",
    "chmod",
    "fchmod",
    "fchmodat",
    // glibc 2.39+ reaches for `fchmodat2` first; it has no `EPERM` fallback path, only an
    // `ENOSYS` one, so denying it would surface as a hard `chmod` failure.
    "fchmodat2",
    "chown",
    "fchown",
    "fchownat",
    "lchown",
    "umask",
    "utime",
    "utimes",
    "utimensat",
    "futimesat",
    // `mknod`/`mknodat` are allowed at the syscall layer and denied where the actual escape
    // lives: the Landlock workdir grant withholds `MakeChar`/`MakeBlock`/`MakeSock` (see
    // `linux_enforce::WORKDIR_ACCESS_RIGHTS`) and the child drops `CAP_MKNOD` before execve.
    "mknod",
    "mknodat",
    "sendfile",
    "copy_file_range",
    "splice",
    "tee",
    "vmsplice",
    "ioctl",
    "poll",
    "ppoll",
    "select",
    "pselect6",
    "epoll_create",
    "epoll_create1",
    "epoll_ctl",
    "epoll_wait",
    "epoll_pwait",
    "epoll_pwait2",
    "eventfd",
    "eventfd2",
    "pipe",
    "pipe2",
    "inotify_init",
    "inotify_init1",
    "inotify_add_watch",
    "inotify_rm_watch",
    "signalfd",
    "signalfd4",
    "timerfd_create",
    "timerfd_settime",
    "timerfd_gettime",
    "getxattr",
    "lgetxattr",
    "fgetxattr",
    "setxattr",
    "lsetxattr",
    "fsetxattr",
    "listxattr",
    "llistxattr",
    "flistxattr",
    "removexattr",
    "lremovexattr",
    "fremovexattr",
    "statfs",
    "fstatfs",
    // Harmless without its partner: `open_by_handle_at` (the syscall that turns a handle back
    // into an open file, ignoring the path it was reached by) stays denied.
    "name_to_handle_at",
    // Legacy POSIX AIO. Unlike `io_uring` it operates only on already-open fds and performs no
    // path resolution of its own, so it carries none of the LSM-bypass property that got the
    // `io_uring_*` family denied.
    "io_setup",
    "io_destroy",
    "io_submit",
    "io_cancel",
    "io_getevents",
    // ---- sockets ----
    // `socket` is NOT here (argument-conditional rules — see this array's doc comment).
    //
    // `connect`/`sendto` ARE here: they used to carry `Notify` rules so a userspace supervisor
    // could read the destination `sockaddr` out of the calling task's memory and compare it against
    // a resolved allowlist. That mechanism is gone (see `crate::network_namespace` for what
    // replaced it), so these are ordinary allowed syscalls again — allowed to *reach the kernel*,
    // where the capsule's own network namespace has no route to anything except the egress proxy.
    // The decision moved from a racy pointer read into the routing table; it did not disappear.
    // `execve`/`execveat` at the top of this array made the same move, into the Landlock domain.
    "connect",
    "sendto",
    "socketpair",
    "bind",
    "listen",
    "accept",
    "accept4",
    "getsockname",
    "getpeername",
    "getsockopt",
    "setsockopt",
    "shutdown",
    "sendmsg",
    "sendmmsg",
    "recvfrom",
    "recvmsg",
    "recvmmsg",
    // ---- time ----
    "clock_gettime",
    "clock_getres",
    "clock_nanosleep",
    "nanosleep",
    "gettimeofday",
    "time",
    "timer_create",
    "timer_settime",
    "timer_gettime",
    "timer_getoverrun",
    "timer_delete",
    "alarm",
    // ---- System V / POSIX IPC ----
    "msgget",
    "msgsnd",
    "msgrcv",
    "msgctl",
    "semget",
    "semop",
    "semctl",
    "semtimedop",
    "mq_open",
    "mq_unlink",
    "mq_timedsend",
    "mq_timedreceive",
    "mq_notify",
    "mq_getsetattr",
    // ---- misc ----
    "getrandom",
    "futex",
    "futex_waitv",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "setuid",
    "setgid",
    "setreuid",
    "setregid",
    "setresuid",
    "setresgid",
    "getresuid",
    "getresgid",
    "getgroups",
    "setgroups",
    "setfsuid",
    "setfsgid",
];

/// The syscalls that must **never** appear in [`SECCOMP_SYSCALL_ALLOWLIST`], with the reason each
/// one is out. A `#[test]` asserts the two lists stay disjoint, so re-permitting one of these can
/// only ever be a deliberate edit to *both* lists rather than an unnoticed line added to the
/// allowlist while reconciling it against a newer upstream profile.
///
/// Three groups, and they are not equally negotiable:
///
///   1. **Named in the card's evidence table** — the ones a probe demonstrated were reachable on
///      bare metal while Docker refused them: `io_uring_setup`/`io_uring_enter`/
///      `io_uring_register` (historically bypasses LSM path hooks, so it is a candidate route
///      around Landlock itself), `bpf`, `userfaultfd`, `perf_event_open`, `open_by_handle_at`,
///      `keyctl`/`add_key`/`request_key`, `process_vm_readv`/`process_vm_writev`, and `ptrace`
///      (which also has an availability dimension: a probe left wedged in `ptrace_stop` blocked
///      `mur` in `futex_wait` for twenty minutes). Docker's default profile actually *allows*
///      `ptrace`/`process_vm_readv`/`process_vm_writev` on kernels ≥ 4.8; this filter is
///      deliberately stricter there.
///   2. **Privileged / host-state operations** with no shell-toolchain use case: namespace and
///      mount manipulation, module loading, reboot, swap, accounting, quota, raw port I/O,
///      clock setting, NUMA policy, `fanotify`.
///   3. **Denied instead of argument-conditioned.** Docker allows these only for specific
///      argument values, which a per-syscall-name allowlist cannot express and which would
///      otherwise have to be routed through a userspace decision this filter no longer makes.
///      Omitting them outright costs nothing, because nothing in bash, coreutils, git, or a
///      standard compiler/interpreter toolchain needs them: `personality`, `kcmp`, `seccomp`
///      itself (a nested filter cannot loosen this one, but it can slow it down for no benefit).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const SECCOMP_MUST_STAY_DENIED: &[&str] = &[
    // 1. the card's evidence table
    "io_uring_setup",
    "io_uring_enter",
    "io_uring_register",
    "bpf",
    "userfaultfd",
    "perf_event_open",
    "process_vm_readv",
    "process_vm_writev",
    "open_by_handle_at",
    "keyctl",
    "add_key",
    "request_key",
    "ptrace",
    // 2. privileged / host-state
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "unshare",
    "setns",
    "init_module",
    "finit_module",
    "delete_module",
    "kexec_load",
    "kexec_file_load",
    "reboot",
    "swapon",
    "swapoff",
    "acct",
    "quotactl",
    "nfsservctl",
    "iopl",
    "ioperm",
    "vm86",
    "vm86old",
    "_sysctl",
    "sysfs",
    "uselib",
    "create_module",
    "get_kernel_syms",
    "query_module",
    "lookup_dcookie",
    "clock_adjtime",
    "clock_settime",
    "settimeofday",
    "stime",
    "adjtimex",
    "set_mempolicy",
    "get_mempolicy",
    "mbind",
    "migrate_pages",
    "move_pages",
    "fanotify_init",
    "fanotify_mark",
    // 3. denied instead of argument-conditioned
    "seccomp",
    "personality",
    "kcmp",
];

/// Bundles the resolved, host-independent enforcement inputs for one capsule session.
#[derive(Debug, Clone)]
pub(crate) struct ShellEnforcement {
    pub(crate) tier: EnforcementTier,
    /// Every `capabilities.network.allow` host resolved to concrete addresses once, at launch,
    /// by [`resolve_network_allowlist_ips`].
    ///
    /// This used to be the *whole* network policy: a seccomp-notify supervisor read a
    /// destination `sockaddr` out of the stopped child and compared the IP against this list. It
    /// is now the narrower of the egress proxy's two checks — the one applied to a destination
    /// the capsule reached without ever resolving a name for it (a hardcoded literal). See
    /// `egress_proxy::EgressPolicy::allows_connection`, which consults exactly this list through
    /// the unchanged [`network_ip_allowed`].
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) network_allow_ips: Vec<IpAddr>,
    /// The same allowlist in its *parsed*, name-keyed form — the ordinary path. The egress proxy
    /// checks a name when it resolves it and again when the connection to the address that name
    /// produced is accepted, which is what a resolved-IP set alone cannot express (one address,
    /// many tenants) in either direction.
    ///
    /// The same `NetworkAllowRule` type and the same parser the WASI-HTTP path uses, so a manifest
    /// entry cannot mean one thing to a WASM guest and another to a subprocess.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) network_allow_rules: Vec<crate::network_policy::NetworkAllowRule>,
    /// The TCP ports the capsule's network namespace opens listeners on, derived from the
    /// allowlist by `egress_proxy::egress_listen_ports`. A port no allow entry implies gets no
    /// listener at all, so a connection to it is refused by the kernel with nothing in userspace
    /// consulted — the strongest form the refusal can take.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) egress_tcp_ports: Vec<u16>,
    /// `capabilities.network.unix_sockets`, threaded through to the seccomp `socket()` rule.
    /// `false` (the default) means the forked child cannot create an `AF_UNIX` socket at all, so
    /// a local daemon socket — `/var/run/docker.sock` above all — is unreachable regardless of
    /// what any Landlock ABI does or does not mediate for pathname sockets. Consumed only by
    /// `linux_enforce::install_seccomp_filter` (via `denied_socket_domains`); resolved on every
    /// platform for parity but never read off Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) unix_sockets_allowed: bool,
    /// `capabilities.filesystem.workdir_exec`, threaded through to the workdir's own Landlock
    /// rule. `false` (the default) withholds the `Execute` right from it, which is what makes
    /// `shell_allow` enforceable at all now that the exec supervisor is gone: nothing the capsule
    /// writes into its workdir can run, under any name. Consumed only by
    /// `linux_enforce::workdir_access_rights` on the Landlock tiers; resolved on every platform
    /// for parity but never read off Linux.
    ///
    /// It has no seccomp counterpart on `KernelSeccompOnly` — without a Landlock domain there is
    /// nothing to withhold — which is part of why that tier cannot reach `scoped`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) workdir_exec: bool,
    /// Narrow read (never write) Landlock grants *outside* the workdir so the allowlisted
    /// binaries can actually exec, dynamically link, and (for a path-based interpreter) reach their
    /// stdlib. Four origins, combined here:
    ///
    ///   - the `DT_NEEDED`-closure files (`shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure), from `resolve_landlock_grants` — each wrapped `list_dir: false`
    ///     (they are individual files, where `ReadDir` was always a no-op);
    ///   - one grant per `capabilities.shell.interpreter_runtime` directory, from
    ///     `resolve_interpreter_runtime_grants` — each carrying exactly the `list_dir` its author
    ///     declared;
    ///   - one listable grant per `capabilities.shell.staged_runtime` tree, from
    ///     `resolve_staged_runtime_landlock_grants`;
    ///   - on `KernelSealed` **only**, one listable but *non-executable* grant per fixed
    ///     [`crate::sealed::SEALED_RUNTIME_PATHS`] entry, from
    ///     `resolve_sealed_runtime_landlock_grants` — the composed root's own bind-mounted runtime
    ///     tree, which nothing else grants.
    ///
    /// The first three carry `executable: true`; the last does not, and that asymmetry is load-
    /// bearing — see [`LandlockGrant`].
    ///
    /// Resolved once at launch (in the parent) and threaded into the forked child's `pre_exec`,
    /// where `apply_landlock_scope` turns each into a per-path `PathBeneath` rule with an access
    /// set that depends on `list_dir` and `executable`. Only consulted on `KernelFull` and
    /// `KernelSealed`; resolved on every platform for parity but never read off Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) landlock_grants: Vec<LandlockGrant>,
    /// Host directories this capsule needs bind-mounted read-only into its composed root *beyond*
    /// the fixed [`crate::sealed::SEALED_RUNTIME_PATHS`]: the containing directory of each
    /// resolved `shell.allow` binary, and each `capabilities.shell.interpreter_runtime` directory.
    ///
    /// Whole directories, never files — this slice must not extend the ELF-closure grant
    /// derivation it exists to make unnecessary. Consumed only on `KernelSealed`; resolved on
    /// every platform for parity, exactly like `landlock_grants` above.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) sealed_bind_dirs: Vec<PathBuf>,
    /// The `source_path` of each declared `capabilities.shell.staged_runtime` grant, bind-mounted
    /// read-only into the composed root at its own absolute path.
    ///
    /// Separate from `sealed_bind_dirs` above because the failure semantics are opposite, and that
    /// is the capability: `sealed_bind_dirs` shrinks the root when the host is missing something,
    /// while a staged tree that cannot be mounted aborts the composed-root construction before
    /// `pivot_root`, which surfaces as `RuntimeError::SealedRootConstructionFailed` (`E-RUN-014`)
    /// and is session-fatal. A capsule never gets a shell tool call inside a root missing a
    /// runtime tree it declared. Consumed only on `KernelSealed`; resolved on every platform for
    /// parity, exactly like the two fields above.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) staged_runtime_dirs: Vec<PathBuf>,
    /// Fully-resolved OS-level bounds for every subprocess this session spawns — `rlimit(2)`
    /// ceilings applied before `execve`, plus the values the cgroup scope below was built from.
    /// Unlike everything above it, this is enforced on **every** platform: `setrlimit` is POSIX,
    /// so macOS gets the per-process half of this slice even though it can never get the
    /// aggregate half. See [`crate::resources`].
    pub(crate) resource_limits: crate::resources::HostResourceLimits,
    /// The runtime's own uid task count, measured once here in the parent by
    /// `crate::resources::uid_task_count` in whichever unit this platform's `RLIMIT_NPROC` is
    /// enforced against: **threads** on Linux (`setrlimit(2)`: "the maximum number of processes
    /// (or, more precisely on Linux, threads)"), **processes** on macOS. `RLIMIT_NPROC` is a
    /// per-uid limit, so `resource_limits.max_processes` is applied as headroom above this rather
    /// than as an absolute ceiling — see `crate::resources::apply_hard_rlimits`. `0` when the host
    /// cannot be asked, which makes the declared value apply literally (the tighter reading).
    pub(crate) nproc_baseline: u64,
    /// The session's cgroup v2 scope, when the host could delegate one (Linux only, and only
    /// for capsules that can actually spawn a native subprocess). `None` on macOS always, and on
    /// Linux only for capsules with no subprocess capability at all — a Linux capsule that *can*
    /// spawn one and could not be given a scope never reaches here, because the launch is
    /// refused first with `RuntimeError::CgroupDelegationUnavailable`.
    pub(crate) cgroup_scope: Option<Arc<crate::cgroup::CgroupScope>>,
    /// The session's periodic workdir-size check. Consulted before every subprocess spawn, so a
    /// disk filler stops writing at the first spawn after the ceiling is crossed rather than
    /// only at the next agent turn.
    pub(crate) workdir_guard: Option<Arc<crate::resources::WorkdirGuard>>,
}

impl ShellEnforcement {
    /// Resolves tier + network allowlist + canonical exec allowlist once, at launch time.
    ///
    /// The rlimit half of [`crate::resources`] is resolved here too (it needs nothing but the
    /// policy), while the cgroup scope and workdir guard are session-scoped live handles the
    /// caller creates and attaches with [`Self::with_host_bounding`].
    ///
    /// `declared` is the already-combined containment floor for the session. It is what decides
    /// whether a sealed-capable host actually installs a composed root — see [`applied_tier`].
    pub(crate) fn resolve(
        policy: &CapabilityPolicy,
        declared: murmur_artifact::ContainmentClass,
    ) -> Result<Self, String> {
        let tier = applied_tier(detect_enforcement_tier(), declared);
        let network_allow_ips = resolve_network_allowlist_ips(&policy.network_allow)?;
        let network_allow_rules =
            crate::network_policy::parse_network_allow_rules(&policy.network_allow)
                .map_err(|error| error.to_string())?;
        let egress_tcp_ports = crate::egress_proxy::egress_listen_ports(&network_allow_rules);
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        // The b3220cb5 `DT_NEEDED`-closure files (individual files → non-listable) plus one grant
        // per author-declared `interpreter_runtime` directory (each with its own `list_dir`). A
        // directory not named in the manifest never receives a rule, regardless of what it holds.
        let mut landlock_grants =
            LandlockGrant::non_listable_files(resolve_landlock_grants(&exec_allow_paths));
        landlock_grants
            .extend(resolve_interpreter_runtime_grants(&policy.shell_interpreter_runtime));
        // Plus one per staged runtime tree. Without this the tree is bind-mounted into the
        // composed root and then denied by the Landlock ruleset installed inside it — see
        // `resolve_staged_runtime_landlock_grants`.
        landlock_grants.extend(resolve_staged_runtime_landlock_grants(policy));
        // Plus, on `KernelSealed` only, one listable grant per fixed `SEALED_RUNTIME_PATHS`
        // directory — the composed root binds them but Landlock inside the root still denies
        // enumerating them. Gated on `tier`, not on the manifest: on `KernelFull` (the tier a
        // `scoped` capsule runs on) the same grant would expose real host directory shape. The
        // resolver applies that gate itself and returns nothing on every other tier.
        landlock_grants.extend(resolve_sealed_runtime_landlock_grants(tier));
        let sealed_bind_dirs = resolve_sealed_bind_dirs(&exec_allow_paths, policy);
        let staged_runtime_dirs = resolve_staged_runtime_dirs(policy);
        Ok(Self {
            tier,
            network_allow_ips,
            network_allow_rules,
            egress_tcp_ports,
            unix_sockets_allowed: policy.unix_sockets_allowed,
            workdir_exec: policy.workdir_exec_allowed,
            landlock_grants,
            sealed_bind_dirs,
            staged_runtime_dirs,
            resource_limits: policy.resources,
            nproc_baseline: crate::resources::uid_task_count().unwrap_or(0),
            cgroup_scope: None,
            workdir_guard: None,
        })
    }

    /// Attach the session-scoped host-bounding handles: the cgroup v2 scope (Linux, when the
    /// capsule can spawn a subprocess) and the workdir-size guard (every platform).
    ///
    /// Separate from [`Self::resolve`] because both are *live* per-session resources with
    /// lifetimes and side effects — a directory under `/sys/fs/cgroup` and a running thread —
    /// while `resolve` is a pure resolution of manifest and host facts. The caller owns their
    /// creation so the fail-closed launch refusal can happen with a typed error before this is
    /// ever reached.
    pub(crate) fn with_host_bounding(
        mut self,
        cgroup_scope: Option<Arc<crate::cgroup::CgroupScope>>,
        workdir_guard: Option<Arc<crate::resources::WorkdirGuard>>,
    ) -> Self {
        self.cgroup_scope = cgroup_scope;
        self.workdir_guard = workdir_guard;
        self
    }

    /// The latched workdir-size breach, if the guard has seen one.
    pub(crate) fn workdir_breach(&self) -> Option<crate::resources::WorkdirBreach> {
        self.workdir_guard
            .as_ref()
            .and_then(|guard| guard.breach())
    }

    /// Refuse to spawn once the workdir ceiling has been crossed.
    ///
    /// Called by both subprocess spawn paths before `Command::spawn()`. Stopping at the spawn
    /// boundary is what keeps a disk filler from writing another byte while the session unwinds:
    /// the periodic check notices the breach, and the very next subprocess never starts.
    pub(crate) fn check_workdir_budget(&self) -> Result<(), String> {
        match self.workdir_breach() {
            Some(breach) => Err(crate::resources::workdir_breach_message(breach)),
            None => Ok(()),
        }
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
            network_allow_rules: Vec::new(),
            egress_tcp_ports: Vec::new(),
            // Denied, matching every other grant this constructor zeroes out. Inert on this tier
            // (no seccomp filter is installed at all), but it must not read as "allowed".
            unix_sockets_allowed: false,
            // Denied, like every other grant this constructor zeroes out. Inert on this tier (no
            // Landlock domain exists to withhold anything from), but it must not read as "allowed".
            workdir_exec: false,
            landlock_grants: Vec::new(),
            sealed_bind_dirs: Vec::new(),
            staged_runtime_dirs: Vec::new(),
            // Defaults, not "no limits": the rlimit ceilings are the one part of this slice that
            // applies unchanged on this tier, so zeroing them out here would misrepresent what a
            // real macOS host does.
            resource_limits: crate::resources::HostResourceLimits::default(),
            nproc_baseline: crate::resources::uid_task_count().unwrap_or(0),
            cgroup_scope: None,
            workdir_guard: None,
        }
    }
}

/// Core messages shared between the stderr line and `logs/bootstrap.log` line for each tier
/// warning, matching the shared-const convention used by `runtime::BASH_NETWORK_BYPASS_WARNING`.
/// Full detail lives on the security-warnings doc page (`security_warning_link`); keep each to
/// one or two concise sentences.
///
/// The Landlock filesystem scope now derives a narrow read+execute grant (the `shell.allow`
/// binaries, their ELF interpreter, and their shared-library closure) *outside* the workdir, in
/// addition to the workdir's own grant (which withholds device-node and unix-socket creation, see
/// `linux_enforce::WORKDIR_ACCESS_RIGHTS`) — so allowlisted programs can actually exec and
/// dynamically link, and the only writable path outside the workdir is `/dev/null`, one of the
/// three fixed devices in `CAPSULE_DEVICE_GRANTS`. Seccomp additionally refuses the
/// `socket(2)` domains in `denied_socket_domains` — closing the `/var/run/docker.sock` path, which
/// no Landlock ABI mediates for pathname sockets. The forked shell child also
/// drops every Linux capability before `execve` (see `linux_enforce::drop_all_capabilities`).
/// None of this has yet been verified by the team on real Landlock-capable Linux hardware (the
/// manual acceptance check happens after this ships), so `KernelFull` still warns (`W_SEC_005`): a
/// silent "full" tier would imply everything is confirmed-enforced, which is the false assurance
/// to avoid until a real Linux run lands.
const KERNEL_UNVERIFIED_WARNING: &str = "capabilities.shell.allow is non-empty and this host \
resolved to a Linux kernel-enforcement tier (Landlock/seccomp). Landlock now grants a narrow, \
derived read+execute scope outside the workdir (the allowlisted binaries, their loader, and their \
shared libraries — nothing writable, no directory granted wholesale), plus a fixed device set \
every capsule gets unconditionally: /dev/null read+write (the only writable path outside the \
workdir), \
/dev/zero and /dev/urandom read-only, and no other device — /dev/random, /dev/full, /dev/tty and \
raw block devices stay denied. The workdir's own grant also \
withholds device-node and unix-socket creation; seccomp refuses socket(AF_UNIX) outright unless \
capabilities.network.unix_sockets is declared, and always refuses AF_NETLINK/AF_PACKET, so a \
capsule cannot reach a host daemon socket such as /var/run/docker.sock; the seccomp filter now \
defaults to deny and permits only a fixed syscall allowlist modelled on the OCI/Docker default \
profile, so io_uring, bpf, userfaultfd, perf_event_open, ptrace and similar are refused outright; \
and the forked shell child \
drops every Linux capability and sets no_new_privs before execve — but this mechanism has not yet \
been verified by the team on real Landlock-capable Linux hardware — treat shell-subprocess \
isolation as not-yet-confirmed and do not rely on it as a hardened boundary until it is.";

/// `KernelSealed`'s counterpart to [`KERNEL_UNVERIFIED_WARNING`], carrying the same `W_SEC_005`
/// code for the same reason: the mechanism is real and installed, but the team has not yet run the
/// manual acceptance procedure against it on real hardware, and a silent strongest tier would
/// imply otherwise. It names what `sealed` adds over `scoped` so the warning is not merely a
/// louder copy of the one below.
const KERNEL_SEALED_UNVERIFIED_WARNING: &str = "capabilities.shell.allow is non-empty and this \
host resolved to the sealed kernel-enforcement tier: the subprocess tree runs in a private mount \
namespace pivoted onto a composed root (host runtime bind-mounted read-only, the session workdir \
the only writable path, a private /dev tmpfs carrying the OCI default device set), with Landlock \
and seccomp still installed inside it as defence in depth. Paths outside that root are absent \
rather than denied — with one documented exception: /proc is a bind of the host's, not a masked \
private procfs, because mounting one unprivileged needs a PID namespace this tier does not create, \
so host process metadata stays visible there exactly as it is under scoped. This mechanism has \
been checked by hand on one host and not yet by the team on hardware of their own — see \
docs/content/reference/sealed-containment-manual-verification.md — so treat the containment \
boundary as not-yet-confirmed until that procedure has been run again independently.";

const SECCOMP_ONLY_WARNING: &str = "capabilities.shell.allow is non-empty and this Linux kernel \
lacks Landlock (kernel <5.13) — filesystem access outside the capsule workdir is not \
kernel-enforced at all, and the seccomp exec/network enforcement that would apply has not been \
verified on real Linux hardware. The fixed capsule device set the Full tier applies (/dev/null \
read+write, /dev/zero and /dev/urandom read-only, every other device denied) is a Landlock \
mechanism, so it does not apply here either: on this tier every device under /dev is reachable \
exactly as it would be without any sandbox. Seccomp also refuses socket(AF_UNIX) unless \
capabilities.network.unix_sockets is declared, and always refuses AF_NETLINK/AF_PACKET — that \
rule needs no Landlock and so applies identically on this tier, but it has not been verified \
either. The same is true of the default-deny syscall allowlist (modelled on the OCI/Docker default \
profile), which refuses io_uring, bpf, userfaultfd, perf_event_open, ptrace and similar on this \
tier too. The forked shell child still drops every Linux capability and \
sets no_new_privs before execve on this tier, independently of Landlock, but that has not been \
verified either. Treat shell subprocess isolation as experimental on this host.";

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
        EnforcementTier::KernelSealed => Some((W_SEC_005, KERNEL_SEALED_UNVERIFIED_WARNING)),
        EnforcementTier::KernelFull => Some((W_SEC_005, KERNEL_UNVERIFIED_WARNING)),
        EnforcementTier::KernelSeccompOnly => Some((W_SEC_002, SECCOMP_ONLY_WARNING)),
        EnforcementTier::EnvironmentOnly => Some((W_SEC_001, ENVIRONMENT_ONLY_WARNING)),
    }
}

const NO_AGGREGATE_BOUNDING_WARNING: &str = "this capsule can spawn native subprocesses, but \
this platform has no cgroup v2 (Linux-only), so no aggregate bound exists across the subprocess \
tree: nothing caps its total memory, task count or CPU. Per-process rlimits from \
capabilities.resources still apply (hard, not soft), but RLIMIT_NPROC is a per-uid ceiling and \
does NOT stop a fork bomb of distinct, short-lived processes — only a cgroup's pids.max does. \
This is permanent on this platform, not a placeholder for a future slice.";

/// Pure decision for the aggregate-bounding warning, split out of
/// [`warn_for_missing_aggregate_bounding`] the same way [`tier_warning`] is split out of
/// [`warn_for_enforcement_tier`], so tests can assert it without capturing stderr.
///
/// Fires only where the gap is both real and unclosable: a non-Linux host running a capsule that
/// can spawn a subprocess. On Linux the same condition is not a warning at all — a capsule that
/// can spawn a subprocess and has no scope never launches (`CgroupDelegationUnavailable`), so
/// there is no running session left to warn about.
pub(crate) fn aggregate_bounding_warning(
    is_linux: bool,
    requires_bounding: bool,
    has_scope: bool,
) -> Option<(&'static str, &'static str)> {
    if is_linux || !requires_bounding || has_scope {
        return None;
    }
    Some((W_SEC_010, NO_AGGREGATE_BOUNDING_WARNING))
}

/// Fires at every launch, not just once.
pub(crate) fn warn_for_missing_aggregate_bounding(
    workdir: &Path,
    requires_bounding: bool,
    has_scope: bool,
) {
    let is_linux = cfg!(target_os = "linux");
    if let Some((code, message)) = aggregate_bounding_warning(is_linux, requires_bounding, has_scope)
    {
        let link = security_warning_link(code);
        eprintln!("[capsule-runtime] warning[{code}]: {message} ({link})");
        crate::agent::append_bootstrap_log(
            workdir,
            &format!("[capability-policy] warning[{code}]: {message} ({link})"),
        );
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

/// Handle to the (possibly no-op) side-channel machinery attached to one subprocess spawn. On
/// Linux tiers, the background thread is already running by the time this is returned by
/// `prepare_enforcement` — started BEFORE the caller calls `.spawn()`, not after.
///
/// This ordering is required, not incidental. The child builds its network namespace inside
/// `pre_exec` and hands the namespace's listening sockets back over a `SOCK_DGRAM` socketpair;
/// `std::process::Command::spawn()`'s parent side then blocks internally (via a close-on-exec
/// error pipe) until the child's `execve` resolves. If the receiving thread only started after
/// `.spawn()` returned, the capsule's very first outbound connection could beat the egress proxy
/// to its own socket. Starting the thread first — racing concurrently with the fork/exec inside
/// `.spawn()`, not gated behind it — closes that window.
///
/// The name is historical, and kept deliberately: this used to also run the seccomp-notify
/// supervisor loop for `execve`/`execveat`, and before that for `connect`/`sendto`. Both mechanisms
/// are retired (Landlock `Execute` rights and a network namespace respectively), so nothing is
/// "supervised" here any more — what is left is the fd hand-off, the egress proxy's lifetime, and
/// the child's diagnostic pipe.
#[derive(Debug)]
pub(crate) enum SupervisorHandle {
    Noop,
    #[cfg(target_os = "linux")]
    Linux {
        /// Carries the started egress proxy back from the receiving thread, so the proxy's
        /// lifetime ends with the subprocess tree it serves rather than outliving it. `None`
        /// travels through here too — the hand-off can fail, and that is reported (and fatal to
        /// the spawn) on its own path.
        proxy_rx: std::sync::mpsc::Receiver<Option<crate::egress_proxy::EgressProxyHandle>>,
        /// Read end of the `CLOEXEC` diagnostic pipe whose write end was moved into the forked
        /// child's `pre_exec` closure. On a successful `execve` the write end closes
        /// automatically with nothing written (so a read yields immediate EOF); on a `pre_exec`
        /// setup failure the child writes the real error message here before returning `Err`,
        /// letting `execute_shell` fold that legible detail into the error it returns instead of
        /// the bare, undifferentiated `EINVAL` that `Command::spawn()` surfaces.
        diag_read: std::os::fd::OwnedFd,
    },
}

impl SupervisorHandle {
    /// Shuts the session's egress proxy down, with a short bound so `execute_shell` never hangs.
    /// In practice the receive returns immediately: the thread's only blocking step is the
    /// namespace-socket hand-off, which completed before the child could `execve` at all — long
    /// before `wait_with_output` (called by `execute_shell` right before this) returned.
    ///
    /// A proxy outliving its namespace would be serving sockets whose peers no longer exist, so
    /// this is the teardown, not merely a join.
    pub(crate) fn join_best_effort(self) {
        match self {
            SupervisorHandle::Noop => {}
            #[cfg(target_os = "linux")]
            SupervisorHandle::Linux { proxy_rx, .. } => {
                if let Ok(Some(proxy)) = proxy_rx.recv_timeout(std::time::Duration::from_secs(5)) {
                    proxy.shutdown();
                }
                // Deliberately not joining the underlying `JoinHandle`: dropping it without
                // joining leaves the thread detached (it keeps running to completion in the
                // background, reclaimed by the OS on exit), which is fine here since the value
                // received above is the only thing it produces.
            }
        }
    }

    /// Best-effort read of any failure detail the forked child wrote to the diagnostic pipe
    /// before its `pre_exec` closure returned `Err`. Only consulted by `execute_shell` when
    /// `Command::spawn()` itself returns `Err` — on the success path this is never called, so
    /// there is zero cost there.
    ///
    /// The caller must have already released every *parent-side* copy of the pipe's write end
    /// (the one captured by the `pre_exec` closure stored in the `Command`) — otherwise the
    /// read below never sees EOF. `execute_shell` guarantees this by dropping the `Command`
    /// before calling this. Returns `None` when there is no pipe (the `Noop` handle used for
    /// `EnvironmentOnly`/non-Linux) or when the child wrote nothing (e.g. a failure with no
    /// message, or a `spawn` failure that never ran `pre_exec` at all, like `fork()` itself
    /// failing).
    pub(crate) fn read_diagnostic(&self) -> Option<String> {
        match self {
            SupervisorHandle::Noop => None,
            #[cfg(target_os = "linux")]
            SupervisorHandle::Linux { diag_read, .. } => {
                use std::os::fd::AsRawFd;
                linux_enforce::read_diagnostic_pipe(diag_read.as_raw_fd())
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

// Child-side (`pre_exec`) forced-failure seams, one per distinct setup step that can fail
// independently: the explicit `no_new_privs` `prctl` and the Landlock ruleset construction.
// Unlike `FORCE_PREPARE_FAILURE` (which fails the *outer*, pre-fork `prepare_enforcement` path),
// these are read from inside the forked child's `pre_exec` closure — `fork()` copy-on-write
// duplicates the calling thread's TLS block, so a flag set `true` in the parent thread before
// `command.spawn()` remains readable as `true` inside `pre_exec` with no cross-process IPC.
// Only compiled into Linux test builds (their only readers — `set_no_new_privs` and
// `apply_landlock_scope` — are Linux-only); production has no bypass or injection point.
#[cfg(all(test, target_os = "linux"))]
thread_local! {
    pub(crate) static FORCE_NO_NEW_PRIVS_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    pub(crate) static FORCE_LANDLOCK_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Fails the composed-root step of a `KernelSealed` launch. Exists because the real step can
    /// only fail on a host that can attempt it at all, and the thing under test — that a
    /// composed-root failure keeps its identity from the child's diagnostic pipe all the way to
    /// the CLI's `E-RUN-014` — must be verifiable on any Linux host, including the CI containers
    /// that can never reach `KernelSealed`. Short-circuits before any real namespace syscall.
    pub(crate) static FORCE_SEALED_ROOT_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// RAII arm for [`FORCE_SEALED_ROOT_FAILURE`]. Lives beside the flag rather than in one test
/// module because two of them need it: `sandbox`'s (which checks what `execute_shell` returns)
/// and `runtime`'s (which checks what the dispatch layer does with it).
#[cfg(all(test, target_os = "linux"))]
pub(crate) struct ForceSealedRootFailureGuard;

#[cfg(all(test, target_os = "linux"))]
impl ForceSealedRootFailureGuard {
    pub(crate) fn new() -> Self {
        FORCE_SEALED_ROOT_FAILURE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for ForceSealedRootFailureGuard {
    fn drop(&mut self) {
        FORCE_SEALED_ROOT_FAILURE.with(|flag| flag.set(false));
    }
}

/// A `KernelSealed` enforcement with no grants, for the forced-failure tests. The parent-side
/// half of the sealed mechanism still runs for real against it (the composed root is planned and
/// its spec built); only the child's execution of that spec is short-circuited by the seam above,
/// which is what makes these tests run identically on a sealed-capable host and on one that could
/// never reach `KernelSealed` at all.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn sealed_test_enforcement() -> ShellEnforcement {
    ShellEnforcement {
        tier: EnforcementTier::KernelSealed,
        ..ShellEnforcement::environment_only()
    }
}

/// Attach the OS-level *process* bounds — hard rlimits, and cgroup membership where a scope
/// exists — to `command`, so they apply to the spawned process and everything it forks.
///
/// Deliberately independent of the tier machinery above. Landlock and seccomp are Linux kernel
/// primitives that exist on some hosts; `setrlimit` is POSIX and exists on all of them, and the
/// cgroup self-move is a single `write(2)` to a descriptor the parent already opened. Splitting
/// this out is what lets the two subprocess spawn paths converge: `execute_shell` gets it as
/// part of `prepare_enforcement`, and `dispatch_native_tool` — which installs no seccomp or
/// Landlock at all, a pre-existing gap this slice does not close — gets it on its own.
///
/// Everything the returned closure does runs in the forked child between `fork` and `execve`,
/// so it is restricted to syscalls: `getrlimit`/`setrlimit` pairs and one `write`. No
/// allocation, no locking, no path resolution.
#[cfg(unix)]
pub(crate) fn attach_process_limits(
    command: &mut std::process::Command,
    enforcement: &ShellEnforcement,
) {
    use std::os::unix::process::CommandExt;

    let limits = enforcement.resource_limits;
    let nproc_baseline = enforcement.nproc_baseline;
    let cgroup_scope = enforcement.cgroup_scope.clone();

    // SAFETY: the closure runs in the forked child before `execve`, where only async-signal-safe
    // operations are permitted. It performs `getrlimit`/`setrlimit` pairs and, when a cgroup
    // scope exists, a single `write` to a descriptor opened by the parent before the fork —
    // no allocation, no locks, no path lookups.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            crate::resources::apply_hard_rlimits(&limits, nproc_baseline)?;
            if let Some(scope) = cgroup_scope.as_ref() {
                scope.join_current_process()?;
            }
            Ok(())
        });
    }
}

/// Lowest file descriptor the fd-hygiene step touches. Everything from here upward is marked
/// close-on-exec in the forked child; fds 0, 1 and 2 are deliberately left alone.
///
/// **The stdio decision, recorded here so no future reader has to re-derive it: stdio is NOT
/// marked close-on-exec, and is NOT reopened.** The whole point of the `Stdio::piped()` handles
/// both spawn paths install *before* `pre_exec` runs is that the exec'd program inherits them
/// across its own `execve` and talks to the parent over them — `execute_shell` reads the
/// subprocess's stdout/stderr, and `dispatch_native_tool` additionally writes its `ToolInput`
/// JSON to the subprocess's stdin and parses its `ToolResult` JSON back off stdout. Marking
/// 0/1/2 close-on-exec would leave the exec'd program with those descriptors closed, breaking
/// every invocation on both paths; and there is no meaningful thing to "reopen" them onto,
/// because the pipes themselves are the intended destination, not an accident of inheritance.
/// Stdio inheritance is the correct outcome. The gap this constant closes is everything
/// *above* stdio, where inheritance was never intended and was only ever prevented by the
/// empirical accident of nothing else happening to be open at spawn time.
///
/// Its only non-test reader is Linux-only (`linux_enforce::mark_inherited_fds_cloexec`), so this
/// carries the same cross-platform `dead_code` exception the other Linux-consumed items in this
/// module carry.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const FD_HYGIENE_FIRST_FD: u32 = 3;

/// Non-Linux counterpart of [`apply_fd_hygiene`]: a documented no-op.
///
/// `close_range(2)` is Linux-only, and this mirrors the shape `prepare_enforcement` already
/// uses on non-Linux targets — no `pre_exec` closure is installed at all, rather than one that
/// does nothing. macOS is `EnforcementTier::EnvironmentOnly` by construction; there is no
/// kernel primitive here to call.
#[cfg(not(target_os = "linux"))]
pub(crate) fn apply_fd_hygiene(_command: &mut std::process::Command) {}

/// Attaches the fd-hygiene step — mark every fd >= [`FD_HYGIENE_FIRST_FD`] close-on-exec — to a
/// `Command` that is about to be spawned, for spawn paths that do **not** go through
/// `prepare_enforcement`.
///
/// This exists so `runtime::dispatch_native_tool` (which has no `ShellEnforcement`, no tier and
/// no kernel sandboxing of its own) gets exactly the same fd-inheritance guarantee
/// `execute_shell` gets, without pulling the seccomp/Landlock apparatus into it. It is
/// deliberately a free function over `&mut Command` taking no policy input: fd hygiene is
/// unconditional — it is not derived from `CapabilityPolicy`, the manifest, or the enforcement
/// tier, and there is no configuration under which a subprocess should inherit an fd the
/// runtime did not deliberately hand it.
///
/// Do **not** call this on a `Command` that also goes through `prepare_enforcement`:
/// `linux_enforce::child_install_enforcement` already performs the same step as its first
/// statement, and `pre_exec` closures run in registration order, so a second one would be a
/// redundant syscall rather than an additional guarantee.
///
/// On a kernel older than 5.11 this makes `.spawn()` itself fail — see
/// [`linux_enforce::mark_inherited_fds_cloexec`] for that fail-closed consequence, which
/// applies identically here.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn apply_fd_hygiene(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: this closure runs in the forked child, after fork() but before execve() — the
    // narrow pre_exec window where only async-signal-safe operations are permitted. Its body is
    // a single `close_range(2)` syscall plus, on failure only, one `io::Error` construction: no
    // allocation on the success path, no locks, no captured state at all.
    unsafe {
        command.pre_exec(linux_enforce::mark_inherited_fds_cloexec);
    }
}

/// Installs kernel-level exec/network/filesystem enforcement into `command` so it applies to
/// the spawned process and everything it forks/execs. The Landlock/seccomp half is a no-op when
/// `enforcement.tier == EnvironmentOnly` — it does not even attempt those calls in that case —
/// but the rlimit half below is not: `setrlimit` is POSIX and applies on every platform this
/// runtime targets, so a macOS capsule still gets per-process ceilings even though this tier has
/// no kernel sandbox at all.
#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare_enforcement(
    command: &mut std::process::Command,
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
    attach_process_limits(command, enforcement);
    Ok(SupervisorHandle::Noop)
}

/// Linux implementation of `prepare_enforcement`. See the module-level docs and the `linux_enforce`
/// submodule for the mechanics (socketpair fd-passing side channel + `pre_exec` seccomp/Landlock
/// installation + background namespace-socket receiving thread).
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
        // No kernel sandbox to install, but the POSIX rlimit ceilings still apply — this tier is
        // only reachable on Linux from tests, and even there it must not read as "unbounded".
        attach_process_limits(command, enforcement);
        return Ok(SupervisorHandle::Noop);
    }

    // Resolve/open every Landlock path in the PARENT, before fork(). Failing to open the workdir
    // is a normal synchronous error here — it names the path and returns before any subprocess is
    // spawned — rather than an `open()` that, buried inside `pre_exec`, would collapse to a bare
    // EINVAL at the `.spawn()` call site. Grant paths that fail to open are silently dropped
    // (shrink-not-fail), exactly as before; only the *where* of the open moved. Landlock rules
    // only apply on `KernelFull`, so `KernelSeccompOnly` opens nothing.
    let landlock_fds = if matches!(
        enforcement.tier,
        EnforcementTier::KernelFull | EnforcementTier::KernelSealed
    ) {
        Some(linux_enforce::open_landlock_fds(
            workdir,
            &enforcement.landlock_grants,
            &landlock_device_grants(enforcement.tier),
            enforcement.workdir_exec,
        )?)
    } else {
        None
    };

    // Resolve the whole composed root in the PARENT for the same reason the Landlock fds are
    // opened here: every path lookup, every `PathBuf::join` and every allocation the child would
    // otherwise perform inside `pre_exec` — the window where the allocator lock may be held by a
    // thread that no longer exists — happens now instead, synchronously, where a failure names
    // itself. See `crate::sealed` for the plan/execute split.
    let sealed_spec = if enforcement.tier == EnforcementTier::KernelSealed {
        Some(build_sealed_root(
            workdir,
            &enforcement.sealed_bind_dirs,
            &enforcement.staged_runtime_dirs,
        )?)
    } else {
        None
    };

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

    // Dedicated CLOEXEC pipe carrying failure detail out of the child. The write end is moved
    // into the `pre_exec` closure; because it is CLOEXEC, a successful `execve` closes it with
    // nothing written (the parent's read then returns immediate EOF — zero cost on the success
    // path), while any `pre_exec` setup failure writes its real message here before the closure
    // returns `Err`. Raw `pipe2` mirrors the raw `socketpair` convention used just above.
    let mut diag_fds = [0i32; 2];
    // SAFETY: `diag_fds` is a 2-element stack array, exactly the size `pipe2` writes into; no
    // other precondition applies.
    let rc = unsafe { libc::pipe2(diag_fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if rc != 0 {
        return Err(format!(
            "sandbox: pipe2() for enforcement diagnostics failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: both fds were just returned by the successful pipe2() call above; nothing else has
    // taken ownership of them yet, so wrapping them in OwnedFd is exclusive/sound.
    let (diag_read, diag_write) = unsafe {
        (
            OwnedFd::from_raw_fd(diag_fds[0]),
            OwnedFd::from_raw_fd(diag_fds[1]),
        )
    };

    // Everything the child's namespace construction needs, resolved here in the parent: the
    // `pre_exec` window permits no allocation, and `getuid()` in particular has to be read
    // *before* the `unshare` — inside a fresh user namespace with no map written yet it reports
    // the overflow id, and writing that back is refused on every host.
    let netns_plan = crate::network_namespace::CapsuleNetnsPlan::resolve(enforcement.egress_tcp_ports.clone());
    let expected_namespace_sockets = netns_plan.socket_count();
    let egress_policy = crate::egress_proxy::EgressPolicy::new(
        enforcement.network_allow_rules.clone(),
        enforcement.network_allow_ips.clone(),
    );

    let tier = enforcement.tier;
    // Copied out before the `move` closure below takes ownership of everything it touches, the
    // same clone-before-move shape `tier` and the Landlock fds already use. Note the fd-passing
    // socketpair above is created in the *parent*, before fork, so the child's own `sendmsg` over
    // it is unaffected by a filter that denies `socket(AF_UNIX, ...)`.
    let unix_sockets_allowed = enforcement.unix_sockets_allowed;
    // Same clone-before-move shape. `HostResourceLimits` is `Copy`; the cgroup scope is an `Arc`
    // whose `cgroup.procs` descriptor was opened in the parent at scope-creation time, so the
    // child performs no path lookup to join it.
    let resource_limits = enforcement.resource_limits;
    let nproc_baseline = enforcement.nproc_baseline;
    let cgroup_scope = enforcement.cgroup_scope.clone();

    // SAFETY: this closure runs in the forked child, after fork() but before execve() — the
    // narrow pre_exec window where only async-signal-safe operations are permitted. It performs
    // the capability-dropping `prctl`/`capset` sequence (bounding-set drop, capset clear, and the
    // explicit `no_new_privs` `prctl`) plus one allocation-free `open`/`read`/`close` of
    // `/proc/sys/kernel/cap_last_cap`, libseccomp filter construction/load (kernel syscalls), one
    // `sendmsg` batch handing the network namespace's sockets to the parent over `child_sock`, and
    // (KernelFull only)
    // Landlock ruleset construction/`restrict_self` against already-open fds (also kernel
    // syscalls) — no `open()`/`canonicalize()` beyond that one `cap_last_cap` read, no locks
    // beyond what those syscalls need. On any failure it writes the real error message to the
    // CLOEXEC diagnostic pipe (best-effort, bounded, raw `write` loop) before returning `Err`.
    // `child_sock`, `diag_write`, and the Landlock fds are moved in and close automatically when
    // the closure body finishes.
    unsafe {
        command.pre_exec(move || {
            // Ordered first, before any seccomp filter is installed: these are the bounds that
            // must hold even if a later step in this closure fails, and `prlimit64`/`setrlimit`
            // plus `write` are all already in `SECCOMP_SYSCALL_ALLOWLIST` either way.
            if let Err(error) = crate::resources::apply_hard_rlimits(&resource_limits, nproc_baseline)
            {
                linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                return Err(error);
            }
            if let Some(scope) = cgroup_scope.as_ref() {
                if let Err(error) = scope.join_current_process() {
                    linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                    return Err(error);
                }
            }

            let sock_fd = child_sock.as_raw_fd();
            match linux_enforce::child_install_enforcement(
                tier,
                &netns_plan,
                sealed_spec.as_ref(),
                landlock_fds.as_ref(),
                sock_fd,
                unix_sockets_allowed,
            ) {
                Ok(()) => Ok(()),
                Err(error) => {
                    linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                    Err(error)
                }
            }
        });
    }

    // Start the namespace-socket receiving thread now — BEFORE the caller calls `.spawn()`. See
    // `SupervisorHandle`'s doc comment for why this ordering (not "after spawn") is required.
    Ok(linux_enforce::start_supervisor(
        parent_sock,
        egress_policy,
        expected_namespace_sockets,
        diag_read,
    ))
}

/// Which fixed device set Landlock grants, keyed on the tier.
///
/// Landlock keeps mediating inside a composed root, so the two device *lists* have to agree: a
/// device present in the sealed `/dev` but absent from the Landlock rules would exist and be
/// unopenable, which reads as a runtime bug rather than as policy. `KernelFull` keeps
/// [`CAPSULE_DEVICE_GRANTS`] exactly as it is — that constant services `scoped` and this slice
/// does not touch it — while `KernelSealed` derives its own from
/// [`crate::sealed::SEALED_DEVICE_NODES`], the same list the private `/dev` tmpfs is built from.
///
/// The parent opens these paths on the *host*, before the namespace exists; because the composed
/// root bind-mounts the host's own nodes rather than creating new ones, the inodes the rules name
/// are the inodes the capsule reaches.
#[cfg(target_os = "linux")]
fn landlock_device_grants(tier: EnforcementTier) -> Vec<CapsuleDeviceGrant> {
    match tier {
        EnforcementTier::KernelSealed => crate::sealed::SEALED_DEVICE_NODES
            .iter()
            .map(|device| CapsuleDeviceGrant {
                path: device.path,
                writable: device.writable,
            })
            .collect(),
        _ => CAPSULE_DEVICE_GRANTS.to_vec(),
    }
}

/// Parent-side half of the sealed mechanism: pick a base, plan the composed root against the real
/// host layout, create the workdir-backed `/tmp` store, and lower the plan into the C-string form
/// the forked child executes.
///
/// Every failure here is synchronous and named, before `.spawn()` is called at all — the same
/// reason `open_landlock_fds` moved its `open()` calls out of `pre_exec`. A capsule that reaches
/// this point has already cleared `check_containment_floor`, so a failure here is a genuine
/// host-state surprise (an unwritable workdir, a host with none of the base candidates), not a
/// declared-vs-achieved mismatch.
///
/// `staged_runtime_read_only` is passed straight through to the planner rather than validated
/// here. A missing `source_path` is *not* diagnosed in this function on purpose: a parent-side
/// error would return `Err(String)`, which converts to the ordinary, retryable
/// `ShellExecError::Failed`. Letting the required bind fail at `mount(2)` in the child instead
/// routes it to the session-fatal `SealedRootConstructionFailed` (`E-RUN-014`) — the correct
/// classification for a capsule that did not get a runtime tree it declared.
#[cfg(target_os = "linux")]
fn build_sealed_root(
    workdir: &Path,
    extra_read_only: &[PathBuf],
    staged_runtime_read_only: &[PathBuf],
) -> Result<crate::sealed::SealedRootSpec, String> {
    use crate::sealed;

    // The workdir must be absolute for its path to mean the same thing inside the composed root,
    // and `launch_session` always creates it as one — but the composed root's whole design rests
    // on it, so it is checked rather than assumed.
    if !workdir.is_absolute() {
        return Err(format!(
            "sealed: session workdir {} is not absolute; a composed root reproduces the workdir at \
             its own absolute path",
            workdir.display()
        ));
    }

    let base = sealed::choose_root_base(workdir, sealed::SEALED_ROOT_BASE_CANDIDATES, |path| {
        path.is_dir()
    })
    .ok_or_else(|| {
        format!(
            "sealed: no usable base directory for the composed root; none of {:?} exists outside \
             the session workdir's own path",
            sealed::SEALED_ROOT_BASE_CANDIDATES
        )
    })?;

    // `/tmp` inside the composed root is backed by this directory, so it is created here rather
    // than from inside `pre_exec`. See `sealed::SEALED_TMP_DIR_NAME` for why /tmp is workdir-backed
    // rather than a second tmpfs.
    let tmp_store = workdir.join(sealed::SEALED_TMP_DIR_NAME);
    std::fs::create_dir_all(&tmp_store).map_err(|error| {
        format!(
            "sealed: failed to create the workdir-backed /tmp store at {}: {error}",
            tmp_store.display()
        )
    })?;

    let plan = sealed::plan_composed_root(
        workdir,
        &base,
        extra_read_only,
        staged_runtime_read_only,
        &sealed::RealHostLayout,
    );
    sealed::build_sealed_root_spec(&plan)
}

/// Linux-only mechanics: fd-passing side channel, seccomp filter construction, Landlock
/// application, and the background thread that receives the capsule's network-namespace sockets.
///
/// Nothing here uses `libseccomp`'s notify facility any more. It did, twice over: `connect`/
/// `sendto` were `Notify` syscalls until the network namespace replaced them, and `execve`/
/// `execveat` until Landlock `Execute` rights replaced them. Both retirements are recorded in
/// `docs/content/reference/seccomp-notify-toctou-audit.md`; re-introducing a `Notify` rule here
/// means re-introducing a `/proc/<pid>/mem` read on the hot path, which that audit found racy.
#[cfg_attr(target_os = "linux", allow(unsafe_code))]
#[cfg(target_os = "linux")]
mod linux_enforce {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
    use std::path::Path;

    use landlock::{make_bitflags, AccessFs, BitFlags};

    use super::{EnforcementTier, LandlockGrant, SupervisorHandle};

    /// Longest diagnostic message written to (or read from) the child-failure pipe. A message
    /// naming the failed step is far shorter than this; the bound just keeps the best-effort
    /// `write`/`read` loops trivially terminating.
    const MAX_DIAG_LEN: usize = 1024;

    /// One Landlock grant path, opened (`O_PATH | O_CLOEXEC`) in the PARENT before fork(), paired
    /// with the two bits that decide its rule's access set: `list_dir` (`ReadDir`) and
    /// `executable` (`Execute`). Only the already-open fd crosses into the child's `pre_exec` —
    /// never a path to re-open there.
    pub(super) struct OpenLandlockGrant {
        fd: OwnedFd,
        list_dir: bool,
        executable: bool,
    }

    /// One entry of [`CAPSULE_DEVICE_GRANTS`], opened (`O_PATH | O_CLOEXEC`) in the PARENT before
    /// fork(), paired with the `writable` bit that decides whether its rule carries `WriteFile` on
    /// top of `ReadFile`. Structurally a sibling of [`OpenLandlockGrant`] rather than the same
    /// type, mirroring the split between [`CapsuleDeviceGrant`](super::CapsuleDeviceGrant) and
    /// [`LandlockGrant`]: read+execute-a-binary and read/write-a-device are different intents and
    /// their bits must not be confusable.
    pub(super) struct OpenDeviceGrant {
        fd: OwnedFd,
        writable: bool,
    }

    /// All Landlock file descriptors resolved in the parent for one shell subprocess: the
    /// workdir's fd (scoped by [`workdir_access_rights`]), each successfully-opened grant fd, and
    /// each successfully-opened fixed-device fd.
    /// Handed by reference into the child's `pre_exec`, where `apply_landlock_scope` builds rules
    /// against these fds without performing a single `open()`.
    pub(super) struct LandlockChildFds {
        workdir_fd: OwnedFd,
        /// The capsule's `capabilities.filesystem.workdir_exec`, carried alongside the fd it
        /// applies to so `apply_landlock_scope` — which runs in `pre_exec` and must read nothing
        /// but its argument — has the whole workdir rule decided for it before fork.
        workdir_exec: bool,
        grants: Vec<OpenLandlockGrant>,
        devices: Vec<OpenDeviceGrant>,
    }

    /// The Landlock ABI v1 rights granted on the capsule **workdir's own** `PathBeneath` rule,
    /// *excluding* `Execute` — see [`workdir_access_rights`], which adds it back for a capsule that
    /// declared `capabilities.filesystem.workdir_exec: true`.
    ///
    /// Spelled out variant by variant rather than `AccessFs::from_all(ABI::V1)` for two reasons:
    /// `from_all` hides what is actually handed to the capsule, and four of ABI v1's thirteen
    /// rights are withheld from this constant.
    ///
    ///   - `Execute` — the one this constant withholds *conditionally*. Withholding it is what
    ///     makes `capabilities.shell.allow` a complete, sound statement: Landlock refuses the exec
    ///     on the path the kernel itself resolved, so a binary the capsule wrote into its workdir
    ///     cannot run under any name — including an allowlisted basename, the exact bypass a prior
    ///     release shipped. No userspace supervisor, no pointer read out of another task, and
    ///     therefore no race. The cost is real and documented: a binary the capsule legitimately
    ///     compiled in its workdir cannot run either, which is what `workdir_exec: true` buys back.
    ///   - `MakeChar` / `MakeBlock` — creating a device node inside the workdir escapes this
    ///     whole scope. A capsule running as root could `mknod` a node for the host's own disk
    ///     (e.g. major/minor `8:0` = `sda`) *inside* the granted directory — which Landlock
    ///     permits, because the new inode lives beneath a granted path — then `open()` it and
    ///     read the raw host filesystem underneath every other restriction. No known workload
    ///     creates device nodes in its working tree.
    ///   - `MakeSock` — binding an `AF_UNIX` socket file. Withheld by the same rule (no
    ///     filesystem object with a kernel-side identity beyond a regular file), but unlike the
    ///     two device rights this one is a genuine open question: some build tooling and some
    ///     language-toolchain daemons `bind()` a unix socket in their working tree, and a fix
    ///     that breaks those is not a fix. The manual acceptance procedure on the
    ///     security-warnings reference page ("Manual acceptance procedure — workdir device-node
    ///     escape") carries an explicit unix-socket scenario for exactly this; if the team's
    ///     real-hardware run finds a workload that needs it, add `MakeSock` back to this list.
    ///
    /// `MakeFifo` stays granted: real build tooling does create named pipes in its working tree,
    /// and a FIFO carries none of the raw-device risk above.
    ///
    /// Because `apply_landlock_scope`'s `handle_access` still declares the *full*
    /// `from_all(ABI::V1)` set, a right this rule withholds is not merely "not extra-granted" — it
    /// is **denied on every path this rule covers**, workdir included, once `restrict_self()` takes
    /// effect. That is precisely why withholding `Execute` here is enforcement and not a mere
    /// omission. The three `Make*` rights are denied domain-wide because no other rule grants them
    /// either; `Execute` is denied *on the workdir specifically*, while the narrow read+execute
    /// grants below still carry it for the allowlisted binaries outside it — which is the whole
    /// shape of the replacement: exec where the operator named a binary, nowhere the capsule
    /// writes.
    pub(super) const WORKDIR_ACCESS_RIGHTS_NO_EXEC: BitFlags<AccessFs> = make_bitflags!(AccessFs::{
        WriteFile
            | ReadFile
            | ReadDir
            | RemoveDir
            | RemoveFile
            | MakeDir
            | MakeReg
            | MakeFifo
            | MakeSym
    });

    /// The workdir's `PathBeneath` rights for one capsule: [`WORKDIR_ACCESS_RIGHTS_NO_EXEC`], plus
    /// `Execute` only when the manifest declared `capabilities.filesystem.workdir_exec: true`.
    ///
    /// A function rather than a second constant so the two cases cannot drift: adding a right to
    /// the workdir grant is one edit, and the `workdir_exec` axis stays exactly one bit wide.
    /// Note what this does *not* consult — `shell.allow`, the exec allowlist, any resolved path.
    /// The whole point of the default is that the workdir grant is no longer name-based at all.
    pub(super) fn workdir_access_rights(workdir_exec: bool) -> BitFlags<AccessFs> {
        if workdir_exec {
            WORKDIR_ACCESS_RIGHTS_NO_EXEC | AccessFs::Execute
        } else {
            WORKDIR_ACCESS_RIGHTS_NO_EXEC
        }
    }

    /// Probes real Landlock support by building a ruleset granting full access to `/` and
    /// calling `restrict_self()` **in a forked child**, so the host process is never placed
    /// into a Landlock domain by a mere capability check.
    ///
    /// Forking is required, not defensive. An earlier version restricted the calling process
    /// on the grounds that a domain permitting everything has "no observable effect" — that is
    /// false, and the rights the ruleset grants are beside the point. Entering *any* Landlock
    /// domain permanently forbids the mount family to the task and every descendant it will
    /// ever have; Landlock is designed that way precisely to stop a sandbox being escaped
    /// through a nested namespace. Since `host_probe` runs this before the sealed probe, and
    /// both run inside `ShellEnforcement::resolve` before any fork that would build a composed
    /// root, restricting in-process poisoned `sealed` on every host, on every invocation — the
    /// namespace was created and then `mount` was refused, with nothing in the error to
    /// suggest the runtime had done it to itself.
    ///
    /// The ruleset is built in the parent because creating one and adding rules restricts
    /// nothing; only `restrict_self` applies it. That keeps everything the child does after
    /// the fork down to a `prctl` and one syscall.
    pub(super) fn probe_landlock_full_access() -> Option<bool> {
        use landlock::{
            Access, AccessFs, Compatible, CompatLevel, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };

        // The child reports which of the three outcomes it reached through its exit status;
        // nothing else crosses the fork.
        const PROBE_FULLY_ENFORCED: i32 = 0;
        const PROBE_PARTIAL: i32 = 1;
        const PROBE_FAILED: i32 = 2;

        let abi = ABI::V1;
        let access_all = AccessFs::from_all(abi);

        let root_fd = PathFd::new("/").ok()?;

        let ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::BestEffort)
            .handle_access(access_all)
            .ok()?
            .create()
            .ok()?
            .add_rule(PathBeneath::new(root_fd, access_all))
            .ok()?;

        // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the child
        // confines itself to async-signal-safe work. Everything this child does is the
        // `restrict_self` syscall pair and `_exit`; the allocating part of building the
        // ruleset already happened above, in the parent.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return None;
        }
        if pid == 0 {
            let code = match ruleset.restrict_self() {
                Ok(status) => {
                    if matches!(status.ruleset, RulesetStatus::FullyEnforced) {
                        PROBE_FULLY_ENFORCED
                    } else {
                        PROBE_PARTIAL
                    }
                }
                Err(_) => PROBE_FAILED,
            };
            // SAFETY: forked-child context; `_exit` skips every destructor and atexit hook,
            // which is what keeps the parent's state untouched.
            unsafe { libc::_exit(code) }
        }

        let mut wait_status: libc::c_int = 0;
        // SAFETY: `pid` is the child just forked; `wait_status` is a live local.
        let waited = unsafe { libc::waitpid(pid, &mut wait_status, 0) };
        if waited < 0 || !libc::WIFEXITED(wait_status) {
            return None;
        }
        match libc::WEXITSTATUS(wait_status) {
            PROBE_FULLY_ENFORCED => Some(true),
            PROBE_PARTIAL => Some(false),
            // A `restrict_self` that errored tells us nothing either way, which is the same
            // answer the pre-fork version gave by propagating `None` out of `.ok()?`.
            _ => None,
        }
    }

    /// Runs inside the forked child, pre-exec: builds the capsule's network namespace and hands
    /// its listening sockets to the parent over `child_sock_fd`, strips the child's Linux
    /// capabilities, installs the seccomp filter (the `socket()` domain denials keyed on
    /// `unix_sockets_allowed`, over a default-deny syscall allowlist), and (on
    /// `KernelFull`/`KernelSealed`) applies the Landlock filesystem scope — which is also what
    /// enforces `capabilities.shell.allow`, now that the exec notify supervisor is gone. Returning
    /// `Err` here aborts the exec (std's `Command` machinery propagates it back to the parent's
    /// `.spawn()` call as an `io::Error`) — the fail-closed path for setup failures that happen
    /// after fork.
    ///
    /// `child_sock_fd` carries exactly one hand-off now: the namespace sockets. It used to carry a
    /// second (the seccomp notify fd), which went with the supervisor.
    pub(super) fn child_install_enforcement(
        tier: EnforcementTier,
        netns_plan: &crate::network_namespace::CapsuleNetnsPlan,
        sealed_spec: Option<&crate::sealed::SealedRootSpec>,
        landlock_fds: Option<&LandlockChildFds>,
        child_sock_fd: RawFd,
        unix_sockets_allowed: bool,
    ) -> io::Result<()> {
        // FIRST, on every kernel tier: the capsule's own network namespace. It has to precede
        // everything below it for three separate reasons, none of them stylistic:
        //
        //   * `SECCOMP_MUST_STAY_DENIED` denies `unshare` and the filter installed further down
        //     denies `socket(AF_NETLINK)`, so this is the only window in which the namespace can
        //     be built at all — the same argument the composed root makes.
        //   * `drop_all_capabilities` (below) removes the `CAP_NET_ADMIN` that bringing `lo` up
        //     and installing the local route require. The namespace is configured while the child
        //     still holds them *inside its own new user namespace*, and never afterwards.
        //   * on `KernelSealed`, the composed root's own `unshare(CLONE_NEWUSER)` nests a second
        //     user namespace inside this one. A task in a descendant user namespace holds no
        //     capability in the ancestor, so once that has happened the capsule cannot reconfigure
        //     the network namespace it is confined to — which only holds if this runs first.
        //
        // This is what replaced the `connect`/`sendto` seccomp-notify interception. There is no
        // path that skips it and falls back: a host that cannot do this refused the launch back in
        // `stage_session` (`RuntimeError::EgressNamespaceUnavailable`, `E-CAP-005`).
        //
        // SAFETY: post-`fork()`, pre-exec child context — exactly the window this function
        // documents itself as running in, and the one `create_capsule_netns` requires.
        unsafe { crate::network_namespace::create_capsule_netns(netns_plan, child_sock_fd)? };

        // NEXT, and only on `KernelSealed`: build the private mount namespace and pivot onto the
        // composed root. This has to precede every *remaining* step in this closure, for one reason
        // that is not a matter of taste — `SECCOMP_MUST_STAY_DENIED` denies `unshare`, `mount` and
        // `pivot_root` to every process the filter below covers, permanently and deliberately.
        // The composed root is therefore built by this process while it still has its pre-filter
        // credentials, never by a syscall the sandboxed subprocess is later permitted to make.
        // The three steps that follow then run *inside* the new root: `drop_all_capabilities`
        // strips the (namespace-local) capabilities `unshare` handed out, seccomp installs, and
        // Landlock's rules — built from fds the parent opened, so unaffected by the pivot —
        // re-scope the same inodes under their new pathnames. Defence in depth, in that order.
        if tier == EnforcementTier::KernelSealed {
            if let Some(spec) = sealed_spec {
                #[cfg(test)]
                if super::FORCE_SEALED_ROOT_FAILURE.with(|flag| flag.get()) {
                    return Err(io::Error::other(format!(
                        "{} unshare(CLONE_NEWUSER|CLONE_NEWNS) failed (forced by test seam)",
                        crate::sealed::SEALED_ROOT_FAILURE_PREFIX
                    )));
                }
                crate::sealed::construct_composed_root(spec)
                    .map_err(|failure| io::Error::other(spec.describe(failure)))?;
            }
        }

        // Before anything else in this window, on every kernel tier: shut the fd-inheritance
        // door. Landlock (KernelFull only) mediates *new* filesystem operations against the
        // ruleset installed further down this function — it can do nothing about a descriptor
        // the child already holds open at that point, because that fd was opened before the
        // ruleset existed and there is no operation left for the ruleset to intercept. Until
        // this call existed, the only thing stopping a spawned shell from inheriting an
        // arbitrary open fd was the empirical accident that nothing else happened to be open in
        // the runtime process at spawn time; one `open()` added anywhere before a spawn — a
        // config read, a lock file, a log handle — would have leaked silently into every
        // subsequent subprocess, on every platform and every tier, with no test to catch it.
        //
        // Running it first is also what makes it cover this function's *own* descriptors: the
        // namespace-socket socketpair end, the diagnostic pipe and the Landlock grant fds are all used
        // below but must never survive the `execve`.
        mark_inherited_fds_cloexec()?;

        // Next, and on every kernel tier (not just `KernelFull`): Landlock and the Linux
        // capability model are independent, both-must-allow gates, so narrowing the Landlock
        // workdir grant does not by itself take `CAP_MKNOD` away from a root-uid capsule. This
        // must also run *before* `install_seccomp_filter`, because it is what sets
        // `no_new_privs` — which `seccomp(2)` requires once `CAP_SYS_ADMIN` is gone. Setting it
        // here (rather than depending on libseccomp's `SCMP_FLTATR_CTL_NNP` default) also means a
        // failure is its own distinct, fail-closed error path before any filter is installed.
        drop_all_capabilities()?;

        // The child stays *non-dumpable*, inherited from the runtime process
        // (`security::harden_process_dumpable`). Until this slice it did not: `restore_child_dumpable`
        // set `PR_SET_DUMPABLE` back to 1 here, because the kernel's `ptrace_may_access` check gates
        // every `/proc/<pid>/*` read and the seccomp-notify supervisor had to read
        // `/proc/<child>/mem` to recover the pathname of each notified `execve`. With that
        // supervisor deleted nothing reads the child's memory any more, so the flag — and the
        // same-uid `ptrace`/`environ`/core-dump exposure it opened for the whole life of every
        // shell subprocess — is simply gone. Do not reintroduce it without a reader that needs it.
        install_seccomp_filter(unix_sockets_allowed)?;

        if matches!(
            tier,
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed
        ) {
            if let Some(fds) = landlock_fds {
                apply_landlock_scope(fds).map_err(io::Error::other)?;
            }
        }

        Ok(())
    }

    /// Marks every file descriptor at or above [`super::FD_HYGIENE_FIRST_FD`] close-on-exec, so
    /// the only descriptors that survive into the exec'd program are the ones the runtime
    /// deliberately set up (stdio) rather than whatever happened to be open.
    ///
    /// `CLOSE_RANGE_CLOEXEC` sets the flag rather than closing the descriptors outright. That is
    /// the required semantics here, not a softer variant of it: several fds in this range are
    /// still needed *inside* this `pre_exec` window — the socketpair end `create_capsule_netns`
    /// sends the namespace's listening sockets over, the diagnostic pipe a failure further down
    /// writes to, and the Landlock grant fds `apply_landlock_scope` builds rules
    /// from. Closing them here would break the very enforcement setup that follows; flagging
    /// them means they keep working until `execve`, which then closes them for us.
    ///
    /// The range starts at 3, never 0 — see [`super::FD_HYGIENE_FIRST_FD`] for the recorded
    /// stdio decision and its reasoning.
    ///
    /// **Kernel range narrowing, stated plainly:** `close_range(2)` landed in Linux 5.9 and
    /// `CLOSE_RANGE_CLOEXEC` in Linux 5.11. On an older kernel this call fails (`ENOSYS`, or
    /// `EINVAL` for the flag on 5.9/5.10), the error propagates, and `Command::spawn()` fails —
    /// so every shell spawn on a `KernelSeccompOnly` *or* `KernelFull` host with such a kernel
    /// now fails rather than silently running without fd hygiene. That is deliberate and it is
    /// this module's established fail-closed discipline (`drop_all_capabilities`,
    /// `install_seccomp_filter` and `apply_landlock_scope` all abort the spawn on an unexpected
    /// error rather than degrade), but it is a real, user-visible narrowing of the supported
    /// kernel range for kernel-enforcement tiers, not a side effect worth burying.
    pub(super) fn mark_inherited_fds_cloexec() -> io::Result<()> {
        // SAFETY: `close_range` takes three scalar arguments and dereferences nothing. It is a
        // single syscall, so it is safe to call in the post-fork/pre-exec window. `c_uint::MAX`
        // as the `last` argument is the documented "to the end of the table" spelling from
        // `close_range(2)`.
        let rc = unsafe {
            libc::close_range(
                super::FD_HYGIENE_FIRST_FD,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Capability-version word for the 64-bit, two-`__u32`-word capability ABI
    /// (`_LINUX_CAPABILITY_VERSION_3`, Linux 2.6.26+). Passing it tells the kernel the
    /// `cap_user_data` argument is a **two element** array. The older version-1 word would cover
    /// only capabilities 0–31 and make the kernel log a deprecation warning.
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

    /// Upper bound for the bounding-set loop when `/proc/sys/kernel/cap_last_cap` cannot be read.
    /// 63 is the architectural ceiling — `capset`'s two 32-bit words address exactly 64
    /// capabilities — so this can never under-cover. Numbers the running kernel does not define
    /// return `EINVAL`, which the loop skips.
    const CAP_LAST_CAP_FALLBACK: u32 = 63;

    /// `cap_user_header_t` from `<linux/capability.h>`. `libc` 0.2 exposes `SYS_capset` but
    /// neither this struct, its data counterpart, nor a safe `capset()` wrapper — so both are
    /// hand-rolled here, for the same reason `send_fd_over_socket` hand-rolls its
    /// `msghdr`/`cmsghdr` handling.
    ///
    /// `dead_code` is allowed because the kernel reads these fields through the raw pointer
    /// handed to `capset(2)`; Rust itself only ever writes them.
    #[repr(C)]
    #[allow(dead_code)]
    struct CapUserHeader {
        version: u32,
        pid: libc::c_int,
    }

    /// One word of `cap_user_data_t` from `<linux/capability.h>`. Under
    /// [`LINUX_CAPABILITY_VERSION_3`] the kernel expects an array of exactly two of these: word 0
    /// carries capabilities 0–31, word 1 carries 32–63. See [`CapUserHeader`] for the `dead_code`
    /// note.
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)]
    struct CapUserData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    /// Strips every Linux capability from the forked child before `execve()`, in three steps
    /// whose **order matters**:
    ///
    ///   1. drop the whole capability **bounding set** (`PR_CAPBSET_DROP`) — while this process
    ///      still holds `CAP_SETPCAP` in its *effective* set, because that is exactly what
    ///      `PR_CAPBSET_DROP` checks (`capabilities(7)`, `prctl(2)`).
    ///   2. clear this process's own permitted/effective/inheritable sets via `capset(2)`.
    ///      Shrinking your own permitted set to any subset — including empty — never requires
    ///      `CAP_SETPCAP`, so running it after step 1 costs nothing. Clearing permitted and
    ///      inheritable also empties the *ambient* set, which the kernel maintains as a subset of
    ///      both — that is what covers a non-root capsule handed an unexpected ambient
    ///      `CAP_MKNOD` (e.g. a systemd unit with `AmbientCapabilities=`).
    ///   3. set `no_new_privs`, so `execve()` cannot regain privilege through a set-user-ID or
    ///      file-capability binary, and so the seccomp filter installed next can load without
    ///      `CAP_SYS_ADMIN` (which step 2 just removed). Setting it here explicitly means this no
    ///      longer depends on libseccomp's default `SCMP_FLTATR_CTL_NNP` attribute having set it
    ///      as a side effect.
    ///
    /// **Why steps 1 and 2 are in this order** (it is the reverse of the obvious "clear my sets,
    /// then clear the bounding set"): `PR_CAPBSET_DROP` requires `CAP_SETPCAP` in the *effective*
    /// set of the caller. Clearing effective first would strip `CAP_SETPCAP`, after which every
    /// `PR_CAPBSET_DROP` returns `EPERM` — and because `EPERM` is (correctly) non-fatal for a
    /// genuinely unprivileged caller, the bounding set would silently survive while this function
    /// still reported success.
    ///
    /// **Why the bounding set is the load-bearing step for a root-uid capsule:** `execve(2)`'s
    /// capability transition treats a file's permitted set as all-ones when the process's real
    /// uid is 0, so the new program's permitted set comes out as `P(bounding)`. Step 2 alone
    /// would be undone by the very next `execve`; emptying the bounding set is what makes the
    /// post-exec permitted (and hence effective) set empty, which is what actually takes
    /// `CAP_MKNOD` away from the shell.
    ///
    /// Note this also removes `CAP_DAC_OVERRIDE` from a root-run capsule's shell subprocess, so
    /// it no longer bypasses ordinary file-permission checks. That is intended — least privilege
    /// — and strictly narrows what the subprocess can reach.
    fn drop_all_capabilities() -> io::Result<()> {
        drop_capability_bounding_set()?;
        clear_capability_sets()?;
        set_no_new_privs()
    }

    /// Step 1 of [`drop_all_capabilities`]. Two errnos are expected and skipped:
    ///
    ///   - `EINVAL` — `cap` is not a capability this kernel defines, so there is nothing to drop.
    ///   - `EPERM` — the caller holds no `CAP_SETPCAP`, i.e. it is a genuinely unprivileged
    ///     process that never held the capability being dropped either. Hard-failing here would
    ///     break every non-root `mur run`.
    ///
    /// Any other errno is unexpected and propagates, aborting the exec — this module's
    /// fail-closed invariant: no silently-unenforced spawn.
    #[allow(unsafe_code)]
    fn drop_capability_bounding_set() -> io::Result<()> {
        let zero: libc::c_ulong = 0;

        for cap in 0..=kernel_last_cap() {
            let cap_arg = libc::c_ulong::from(cap);
            // SAFETY: `prctl(PR_CAPBSET_DROP, ...)` takes an integer capability number and three
            // ignored arguments — no pointers and no borrowed memory are involved.
            let rc = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap_arg, zero, zero, zero) };
            if rc == 0 {
                continue;
            }

            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINVAL) | Some(libc::EPERM) => continue,
                _ => return Err(error),
            }
        }

        Ok(())
    }

    /// Step 2 of [`drop_all_capabilities`]. Always expected to succeed: the kernel's `capset`
    /// checks are all "is the new set a subset of the old one", and the empty set is a subset of
    /// everything. Any error is therefore unexpected and fails closed.
    #[allow(unsafe_code)]
    fn clear_capability_sets() -> io::Result<()> {
        let header = CapUserHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            // 0 means "the calling thread" — the only pid `capset` accepts for a capability
            // change since Linux 2.6.25.
            pid: 0,
        };
        let data = [CapUserData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        }; 2];

        // SAFETY: both pointers refer to stack locals that outlive this single `capset` call, and
        // `data` is exactly the two-element array `LINUX_CAPABILITY_VERSION_3` makes the kernel
        // expect. The kernel only reads through them.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_capset,
                &header as *const CapUserHeader,
                data.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Step 3 of [`drop_all_capabilities`]. Setting `no_new_privs` can only ever *reduce* a
    /// process's privilege and is never refused for a well-formed call, so any error is
    /// unexpected and fails closed. Same call convention and fail-closed style as
    /// `security::harden_process_dumpable`'s `PR_SET_DUMPABLE` `prctl`, but a different call site
    /// and lifetime: this is per-shell-subprocess, inside `pre_exec`, not the once-at-`main()`
    /// whole-process hardening.
    #[allow(unsafe_code)]
    fn set_no_new_privs() -> io::Result<()> {
        #[cfg(test)]
        if super::FORCE_NO_NEW_PRIVS_FAILURE.with(|flag| flag.get()) {
            return Err(io::Error::other(
                "sandbox: no_new_privs (prctl PR_SET_NO_NEW_PRIVS, 1) failed (forced by test seam)",
            ));
        }
        // The kernel rejects `PR_SET_NO_NEW_PRIVS` unless arg2 is exactly 1 and arg3/4/5 are
        // exactly 0, so these are typed as `c_ulong` rather than left as untyped literals whose
        // upper 32 bits would be unspecified in a variadic call on aarch64.
        let enable: libc::c_ulong = 1;
        let zero: libc::c_ulong = 0;

        // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS, ...)` takes four integer arguments — no pointers
        // and no borrowed memory are involved.
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, enable, zero, zero, zero) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Highest capability number this kernel defines, read from `/proc/sys/kernel/cap_last_cap`.
    /// Uses raw `open`/`read` into a stack buffer so it stays allocation-free in the post-`fork()`
    /// window. Any failure — file absent on a pre-2.6.25 kernel, unreadable, contents that do not
    /// parse — falls back to [`CAP_LAST_CAP_FALLBACK`], which over-covers rather than
    /// under-covers, since undefined capability numbers just return the `EINVAL` the caller skips.
    #[allow(unsafe_code)]
    fn kernel_last_cap() -> u32 {
        const PATH: &[u8] = b"/proc/sys/kernel/cap_last_cap\0";

        // SAFETY: `PATH` is a NUL-terminated byte literal with `'static` lifetime, and `open`
        // only reads through the pointer.
        let fd = unsafe {
            libc::open(
                PATH.as_ptr() as *const libc::c_char,
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return CAP_LAST_CAP_FALLBACK;
        }

        let mut buf = [0u8; 16];
        // SAFETY: `fd` was just opened successfully; `buf` is a stack array and its exact length
        // is passed, so `read` cannot write past it.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        // SAFETY: `fd` is open and exclusively owned here; nothing else holds or will reuse it.
        unsafe { libc::close(fd) };

        if read <= 0 {
            return CAP_LAST_CAP_FALLBACK;
        }

        let parsed = std::str::from_utf8(&buf[..read as usize])
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());

        match parsed {
            Some(last) if last <= CAP_LAST_CAP_FALLBACK => last,
            _ => CAP_LAST_CAP_FALLBACK,
        }
    }

    /// The Landlock syscalls the child still has to make after its seccomp filter is loaded, each
    /// with the syscall number to fall back on when this host's libseccomp is too old to resolve
    /// the name (they were added in libseccomp 2.5.4; Ubuntu 22.04 ships 2.5.3 on a kernel that
    /// does support Landlock, so this is a real combination, not a hypothetical one).
    ///
    /// Linux assigned these numbers uniformly across architectures in 5.13 — 444/445/446 on
    /// x86_64, aarch64 and every other arch with Landlock — so a single number per name is
    /// correct for all of them, unlike the legacy syscalls where numbers diverge per arch.
    const LANDLOCK_SYSCALLS: [(&str, i32); 3] = [
        ("landlock_create_ruleset", 444),
        ("landlock_add_rule", 445),
        ("landlock_restrict_self", 446),
    ];

    /// Builds and loads the child's seccomp filter.
    ///
    /// **This filter raises no notifications and needs no supervisor.** It used to: `execve`/
    /// `execveat` carried `SECCOMP_RET_USER_NOTIF` rules so a userspace loop could read the invoked
    /// pathname out of `/proc/<pid>/mem` and answer `CONTINUE`, and `connect`/`sendto` did the same
    /// for a destination `sockaddr`. Both mechanisms are gone, retired in that order: network
    /// enforcement moved into [`crate::network_namespace`] plus [`crate::egress_proxy`], and exec
    /// enforcement moved into the Landlock domain `apply_landlock_scope` installs, where the kernel
    /// evaluates the path it resolved itself. `seccomp_unotify(2)` calls continue-based argument
    /// inspection inherently racy, and this filter no longer does any.
    ///
    /// What is left is one mechanism plus a default:
    ///
    ///   - `socket` gets classic, register-value `Errno` rules, one per denied domain. `domain` is
    ///     a plain integer argument, so the comparison compiles straight into the loaded BPF
    ///     program and is evaluated in-kernel at syscall time: no notification is raised and no
    ///     memory of another task is read. That is what makes it structurally immune to the
    ///     pointer-read TOCTOU class of problem in the first place.
    ///   - the filter's **default action**, which is a deny (`Errno(EPERM)`): a syscall named by
    ///     neither [`super::SECCOMP_SYSCALL_ALLOWLIST`] nor a rule here is refused, without any
    ///     argument being inspected. That is what makes `io_uring_setup`, `bpf`, `userfaultfd`,
    ///     `perf_event_open`, `ptrace` and the rest of [`super::SECCOMP_MUST_STAY_DENIED`]
    ///     unreachable — they are simply absent, not argument-matched.
    fn install_seccomp_filter(unix_sockets_allowed: bool) -> io::Result<()> {
        // Default-deny. `EPERM` matches what the OCI/Docker default profile returns for a syscall
        // outside its allowlist, and is deliberately a *different* errno from the `EACCES` that the
        // `socket()` domain rules (and Landlock) return: `EACCES` means "the sandbox looked at this
        // call's arguments and refused it", `EPERM` means "this syscall is not part of the
        // capsule's syscall surface at all". Keeping them distinct is what lets an operator tell an
        // unmediated-surface problem from a policy decision.
        let mut filter =
            libseccomp::ScmpFilterContext::new(libseccomp::ScmpAction::Errno(libc::EPERM))
                .map_err(to_io_err)?;

        // Ask the kernel to record every action this filter takes that is not `Allow` in the
        // audit trail, so a denial is attributable after the fact to a syscall number, pid and
        // comm. The denied process itself still observes nothing but `EPERM` — `SECCOMP_RET_ERRNO`
        // has no channel for anything richer — so this is the only way an operator debugging a
        // workload that dies on an unexpected denial gets a syscall name to look at.
        //
        // Best-effort on purpose, and the one place in this function where an error is swallowed:
        // `SCMP_FLTATR_CTL_LOG` needs libseccomp API level 3 (libseccomp 2.4.0+), and the crate
        // reports an older runtime library as an error here. That is a *diagnosability* shortfall,
        // not an enforcement one — the default-deny action and every rule below are unaffected —
        // and turning it into a hard failure would take a host with an old libseccomp from
        // "denials are logged less legibly" to "no capsule can spawn a shell at all".
        //
        // Setting the attribute is necessary but not sufficient: the kernel only logs an action
        // whose type also appears in `/proc/sys/kernel/seccomp/actions_logged`, which is host
        // configuration this process does not control.
        // `ESCAPE_CONFORMANCE_HARNESS.md` at the repository root carries the hand-run procedure
        // these denials are verified by.
        let _ = filter.set_ctl_log(true);

        // Deny the dangerous `socket(2)` domains at creation time. `EACCES` (not `EPERM`) is the
        // errno this sandbox uses for "the sandbox looked at this call's arguments and refused it",
        // as opposed to the default action's `EPERM` for "this syscall is not part of the capsule's
        // surface at all". It also matches what Landlock returns for a denied exec, so a capsule
        // sees one consistent errno across both argument-level mechanisms.
        //
        // Untouched by the network-namespace work, and deliberately so: a network namespace does
        // not mediate `AF_UNIX` at all — pathname or abstract — so this register-level rule is
        // still the only thing standing between a capsule and `/var/run/docker.sock`.
        //
        // `add_rule_conditional` (not `..._exact`) on purpose: the non-exact form lets libseccomp
        // adapt the rule to the architectures in the filter — on an arch where `socket` is
        // reached through `socketcall(2)` rather than as its own syscall, the exact form would
        // fail the whole install instead. Fail-closed is preserved either way, since any error
        // here propagates out of `pre_exec` and aborts the spawn.
        //
        // One rule per domain rather than one rule with three comparators: comparators within a
        // single rule are AND-ed, and no `socket()` call has a `domain` that is simultaneously
        // `AF_UNIX` and `AF_NETLINK`, so a combined rule would match nothing.
        let socket_syscall = libseccomp::ScmpSyscall::from_name("socket").map_err(to_io_err)?;
        for domain in super::denied_socket_domains(unix_sockets_allowed) {
            filter
                .add_rule_conditional(
                    libseccomp::ScmpAction::Errno(libc::EACCES),
                    socket_syscall,
                    &[libseccomp::ScmpArgCompare::new(
                        0,
                        libseccomp::ScmpCompareOp::Equal,
                        domain as u64,
                    )],
                )
                .map_err(to_io_err)?;
        }

        // The permitted domains need positive rules of their own now that the filter's default is
        // a deny — without them `socket()` would be refused for every family, IP included.
        //
        // Equality rules per allowed domain, and NOT one unconditional `Allow` rule for `socket`:
        // libseccomp resolves an unconditional rule for a syscall by *discarding* every
        // argument-conditional chain already recorded for it (`db.c`: "syscall exists with chains
        // but the new filter has no chains so we need to clear the existing chains"), so a broad
        // `Allow` here would silently delete the AF_UNIX/AF_NETLINK/AF_PACKET denials directly
        // above and reopen the `/var/run/docker.sock` path they exist to close. containerd's
        // default profile carries the same warning against combining a broad `socket` allow with
        // explicit errno rules, and works around it the same way — with argument-matched rules
        // only. The two domain sets are disjoint by construction (a test pins that), so no
        // `socket()` call can match both an allow and a deny rule.
        for domain in super::allowed_socket_domains(unix_sockets_allowed) {
            filter
                .add_rule_conditional(
                    libseccomp::ScmpAction::Allow,
                    socket_syscall,
                    &[libseccomp::ScmpArgCompare::new(
                        0,
                        libseccomp::ScmpCompareOp::Equal,
                        domain as u64,
                    )],
                )
                .map_err(to_io_err)?;
        }

        // The allowlist proper. Two failure modes are both handled by skipping the name:
        //
        //   - `from_name` fails when this host's libseccomp does not know the syscall (a name
        //     newer than the installed library);
        //   - `add_rule` fails with `EDOM` when the name resolves to a pseudo-syscall that does
        //     not exist on this architecture — every legacy x86_64 spelling in the array
        //     (`open`, `dup2`, `poll`, `stat`, ...) hits this on aarch64.
        //
        // Skipping is fail-closed: a name that gets no rule stays denied by the default action,
        // so the filter can only ever come out *narrower* than intended, never wider. Propagating
        // instead would abort every single spawn on the first arch-specific name, which is why
        // this is the deliberate exception to the module's otherwise-strict error handling.
        for name in super::SECCOMP_SYSCALL_ALLOWLIST {
            if let Ok(syscall) = libseccomp::ScmpSyscall::from_name(name) {
                let _ = filter.add_rule(libseccomp::ScmpAction::Allow, syscall);
            }
        }

        // Landlock's three syscalls, which `child_install_enforcement` calls *after* this filter
        // is loaded (`apply_landlock_scope`, on `KernelFull`). Without these rules the default
        // deny would refuse the child's own filesystem scoping and abort every spawn on that
        // tier. Allowing them costs nothing: a Landlock domain can only ever narrow the process
        // that installs it, never widen it, and Docker's default profile allows them too.
        //
        // Resolved by name where possible, with the syscall number as a fallback, because these
        // names only became known to libseccomp in 2.5.4 — the "skip what does not resolve"
        // policy above would silently turn an older libseccomp into "no capsule can run a shell
        // on a Landlock-capable host". The numbers are the same on every architecture Linux has
        // added Landlock to (they were assigned after the syscall-number unification), so the
        // fallback is not architecture-specific.
        for (name, fallback_nr) in LANDLOCK_SYSCALLS {
            let syscall = libseccomp::ScmpSyscall::from_name(name)
                .unwrap_or_else(|_| libseccomp::ScmpSyscall::from(fallback_nr));
            filter
                .add_rule(libseccomp::ScmpAction::Allow, syscall)
                .map_err(to_io_err)?;
        }

        // Dropping `filter` after `load()` is safe: libseccomp's `seccomp_release()` (invoked on
        // Drop) only frees the userspace filter-building context, while the filter itself is
        // already installed in the kernel and is inherited across the `execve` that follows.
        filter.load().map_err(to_io_err)
    }

    fn to_io_err<E: std::fmt::Display>(error: E) -> io::Error {
        io::Error::other(error.to_string())
    }

    /// Scopes the shell subprocess tree's filesystem access with Landlock. Three kinds of rule:
    ///
    ///   - the workdir gets [`workdir_access_rights`] — read/write plus directory, regular-file,
    ///     FIFO and symlink creation, but **not** `MakeChar`/`MakeBlock`/`MakeSock`, and **not**
    ///     `Execute` unless the capsule declared `capabilities.filesystem.workdir_exec: true` (see
    ///     [`WORKDIR_ACCESS_RIGHTS_NO_EXEC`] for why each right is withheld). Withholding `Execute`
    ///     is what enforces `capabilities.shell.allow` completely: the kernel refuses the exec on
    ///     the path it resolved itself, so nothing the capsule writes into its workdir can run;
    ///   - each [`LandlockGrant`] (the `shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure, any `interpreter_runtime` or `staged_runtime` directory, and on
    ///     `KernelSealed` the fixed [`crate::sealed::SEALED_RUNTIME_PATHS`], all *outside* the
    ///     workdir) gets a **narrow read** grant — never write — with two bits added on top.
    ///     Whether it carries `ReadDir` (i.e. the directory's own entries are enumerable) is
    ///     exactly the grant's `list_dir`: the derived closure files are all `false` (a regular
    ///     file has no meaningful `ReadDir`), while an `interpreter_runtime` directory carries
    ///     whatever its author wrote. `ReadDir` is granted only on the specific inode a rule names
    ///     — never on an ancestor or sibling — so naming one subdirectory never makes `/usr/lib`
    ///     (or any parent) enumerable. Whether it carries `Execute` is the grant's `executable`,
    ///     `true` everywhere except the tier-issued sealed-runtime grant, which is deliberately
    ///     readable and enumerable but not runnable;
    ///   - each entry of the fixed [`CAPSULE_DEVICE_GRANTS`] set gets `ReadFile`, plus `WriteFile`
    ///     if its `writable` bit is set. That bit is set for exactly one path — `/dev/null` — which
    ///     is therefore the *only* writable path outside the workdir in the whole sandbox. This
    ///     list is a compile-time constant, not manifest-derived: no capability declaration widens
    ///     or narrows it. Every device path it does not name stays denied by the ordinary
    ///     no-matching-rule path below.
    ///
    /// Without the second kind of rule, `restrict_self()` denies `Execute`/`ReadFile` on every
    /// path outside the workdir, so every allowlisted binary's `execve` fails with EACCES before
    /// it runs (and even a binary placed *inside* the workdir can't be loaded, because its dynamic
    /// loader must still read `ld-linux`/libc outside it). The narrow grants fix that while keeping
    /// the security story honest: apart from `/dev/null`, nothing outside the workdir is writable,
    /// so a write to any other outside path — including one of these read+execute grant paths — is
    /// still denied.
    ///
    /// `handle_access` must declare the union of every access bit any rule uses, and it stays the
    /// **full** ABI v1 set deliberately: a bit declared there but granted by no rule is denied for
    /// the whole domain, which is exactly how the rights missing from
    /// [`WORKDIR_ACCESS_RIGHTS_NO_EXEC`] become denied rather than merely un-granted, and it is also what
    /// makes every device outside [`CAPSULE_DEVICE_GRANTS`] denied. A grant path — or a device
    /// path, on a host where one of the three is missing — that
    /// fails to *open* is skipped
    /// (shrink-not-fail, matching `resolve_exec_allowlist`) rather than failing the whole scope —
    /// it only narrows what is allowed. A genuine ruleset-construction failure still returns `Err`,
    /// preserving the module's fail-closed invariant.
    fn apply_landlock_scope(fds: &LandlockChildFds) -> Result<(), String> {
        #[cfg(test)]
        if super::FORCE_LANDLOCK_FAILURE.with(|flag| flag.get()) {
            return Err(
                "landlock: ruleset construction failed (forced by test seam)".to_string(),
            );
        }

        use landlock::{
            Access, AccessFs, Compatible, CompatLevel, PathBeneath, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };

        let abi = ABI::V1;
        let access_all = AccessFs::from_all(abi);
        // Enough for the loader to open, read, and map-execute a binary or shared library, and to
        // open a file inside a granted directory by exact name — but no write access, and (without
        // `ReadDir`) no enumeration of the directory's own entries.
        let read_execute = AccessFs::Execute | AccessFs::ReadFile;
        // Adds enumerability (`getdents64`) — only for a grant carrying `list_dir: true`.
        let read_execute_list = read_execute | AccessFs::ReadDir;
        // The same, minus `Execute`. `Execute` is this runtime's exec allowlist (the seccomp
        // `execve` supervisor was retired in its favour), so withholding it is what keeps a
        // whole-tree grant from also being permission to run everything in that tree — see
        // `resolve_sealed_runtime_landlock_grants`, its only user. `dlopen(3)` still works under
        // it: mapping `PROT_EXEC` needs `ReadFile`, not `Execute`.
        let read_list = AccessFs::ReadFile | AccessFs::ReadDir;
        // Device rights. No `Execute` (a character device is never exec'd) and no `ReadDir` (it is
        // not a directory) — only the two data bits, with `WriteFile` reserved for `/dev/null`.
        let device_read: BitFlags<AccessFs> = AccessFs::ReadFile.into();
        let device_read_write = device_read | AccessFs::WriteFile;

        // Every fd here was already opened in the parent (before fork). This function performs no
        // `open()`/`canonicalize()` — only the Landlock ruleset syscalls against already-open fds,
        // so a failure here can only be the kernel call itself, not an unrelated path resolution.
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(access_all)
            .map_err(|error| format!("landlock: handle_access failed: {error}"))?
            .create()
            .map_err(|error| format!("landlock: ruleset create failed: {error}"))?
            .add_rule(PathBeneath::new(
                fds.workdir_fd.as_fd(),
                workdir_access_rights(fds.workdir_exec),
            ))
            .map_err(|error| format!("landlock: add_rule failed: {error}"))?;

        for grant in &fds.grants {
            // Grant paths that failed to open in the parent were already dropped (shrink-not-fail),
            // so every fd reaching here is valid. A rule that still fails to add is a genuine
            // ruleset-construction failure and propagates as `Err` (fail-closed).
            let access = match (grant.executable, grant.list_dir) {
                (true, true) => read_execute_list,
                (true, false) => read_execute,
                // Not executable implies listable today — the only non-executable grant is the
                // whole-tree sealed-runtime one, which exists precisely to enumerate.
                (false, _) => read_list,
            };
            ruleset = ruleset
                .add_rule(PathBeneath::new(grant.fd.as_fd(), access))
                .map_err(|error| format!("landlock: add_rule for grant failed: {error}"))?;
        }

        for device in &fds.devices {
            // Same convention as the grant loop above: a device that failed to open in the parent
            // was already dropped, so a failure here is a genuine ruleset-construction failure and
            // propagates as `Err` (fail-closed).
            let access = if device.writable {
                device_read_write
            } else {
                device_read
            };
            ruleset = ruleset
                .add_rule(PathBeneath::new(device.fd.as_fd(), access))
                .map_err(|error| format!("landlock: add_rule for device failed: {error}"))?;
        }

        ruleset
            .restrict_self()
            .map_err(|error| format!("landlock: restrict_self failed: {error}"))?;

        Ok(())
    }

    /// Opens (in the PARENT, before fork) the workdir fd and every grant fd Landlock needs, using
    /// `O_PATH | O_CLOEXEC` — the exact flags `landlock::PathFd` uses. Moving these `open()` calls
    /// out of the child's `pre_exec` window is the whole point: a workdir that cannot be resolved
    /// now fails here, synchronously, with a message naming the path — before any subprocess is
    /// spawned — instead of collapsing to a bare EINVAL inside `pre_exec`. A grant path that fails
    /// to open is dropped (shrink-not-fail), matching the previous per-grant `continue`.
    ///
    /// `device_grants` — the tier's fixed device set, [`super::CAPSULE_DEVICE_GRANTS`] on
    /// `KernelFull` and the composed root's own OCI default set on `KernelSealed`, chosen by
    /// `super::landlock_device_grants` — is opened here too, by the same rules: the list is a
    /// compile-time constant rather than manifest-derived, and a host where one entry cannot be
    /// opened — unusual, but a broken or minimal `/dev` is possible — loses that one device rather
    /// than failing the launch. Losing `/dev/null` here degrades to exactly the behavior that
    /// existed before this mechanism, which is a strictly narrower scope, never a wider one.
    pub(super) fn open_landlock_fds(
        workdir: &Path,
        landlock_grants: &[LandlockGrant],
        device_grants: &[super::CapsuleDeviceGrant],
        workdir_exec: bool,
    ) -> Result<LandlockChildFds, String> {
        let workdir_fd = open_o_path(workdir).map_err(|error| {
            format!(
                "sandbox: failed to open workdir {} for Landlock scoping: {error}",
                workdir.display()
            )
        })?;

        let mut grants = Vec::with_capacity(landlock_grants.len());
        for grant in landlock_grants {
            match open_o_path(&grant.path) {
                Ok(fd) => grants.push(OpenLandlockGrant {
                    fd,
                    list_dir: grant.list_dir,
                    executable: grant.executable,
                }),
                Err(_) => continue,
            }
        }

        let mut devices = Vec::with_capacity(device_grants.len());
        for device in device_grants {
            match open_o_path(Path::new(device.path)) {
                Ok(fd) => devices.push(OpenDeviceGrant {
                    fd,
                    writable: device.writable,
                }),
                Err(_) => continue,
            }
        }

        Ok(LandlockChildFds {
            workdir_fd,
            workdir_exec,
            grants,
            devices,
        })
    }

    /// Opens `path` with `O_PATH | O_CLOEXEC` (identity/lifetime handle only, never a data fd),
    /// returning an `OwnedFd` suitable for `landlock::PathBeneath`. Same open the vendored
    /// `landlock::PathFd::new` performs, done in the parent so the resulting fd can be moved into
    /// the child's `pre_exec` closure.
    fn open_o_path(path: &Path) -> io::Result<OwnedFd> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)?;
        Ok(OwnedFd::from(file))
    }

    /// Best-effort write of a bounded failure message to the child-diagnostic pipe from inside
    /// `pre_exec`, using a raw `write` loop (allocation for the message itself already happened
    /// upstream — `child_install_enforcement`'s error paths use `format!`). Never fails the caller:
    /// a lost diagnostic only costs legibility, never the fail-closed guarantee (the closure still
    /// returns `Err`, so `execve` is still aborted).
    #[allow(unsafe_code)]
    pub(super) fn write_diagnostic(fd: RawFd, message: &str) {
        let bytes = message.as_bytes();
        let len = bytes.len().min(MAX_DIAG_LEN);
        let mut written = 0;
        while written < len {
            // SAFETY: `fd` is the valid, open write end of the diagnostic pipe; the pointer/len
            // name a live sub-slice of `bytes` for the duration of this single `write` call.
            let n = unsafe {
                libc::write(
                    fd,
                    bytes[written..len].as_ptr() as *const libc::c_void,
                    len - written,
                )
            };
            if n < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            written += n as usize;
        }
    }

    /// Parent-side read of whatever the child wrote to the diagnostic pipe before its `pre_exec`
    /// closure returned `Err`. Blocks until EOF, which arrives once every write end is closed —
    /// the child's (on `_exit`) and the parent's captured copy (which `execute_shell` drops before
    /// calling this). Returns `None` when nothing was written (the success-path EOF, or a `spawn`
    /// failure that never ran `pre_exec`).
    #[allow(unsafe_code)]
    pub(super) fn read_diagnostic_pipe(fd: RawFd) -> Option<String> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            // SAFETY: `fd` is the valid, open read end of the diagnostic pipe; `chunk` is a live
            // stack buffer of exactly the length passed.
            let n = unsafe {
                libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len())
            };
            if n < 0 {
                if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.len() >= MAX_DIAG_LEN {
                break;
            }
        }
        if buf.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&buf).into_owned())
        }
    }

    /// Spawns the background thread that receives the capsule network namespace's listening
    /// sockets from the child and starts serving them through the egress proxy — blocking on that
    /// receive, racing concurrently with the caller's (not-yet-issued) `.spawn()` call rather than
    /// running after it, per [`SupervisorHandle`]'s doc comment.
    ///
    /// The child sends exactly one `SCM_RIGHTS` batch over this socketpair, from
    /// `create_capsule_netns` — the first thing its `pre_exec` window does. It used to send a
    /// second message afterwards (the seccomp notify fd) and this thread used to stay alive
    /// supervising it; both are gone with the exec supervisor, so the thread now finishes as soon
    /// as the proxy is up and hands the live handle back over `proxy_rx` for
    /// [`SupervisorHandle::join_best_effort`] to shut down. What that changes is *when* the proxy
    /// stops: previously when the notify fd closed (i.e. when every process holding the filter had
    /// exited), now when `execute_shell` has finished waiting on the subprocess tree. Those are the
    /// same moment in every ordinary case, because `wait_with_output` reads stdout to EOF and so
    /// waits for every descendant holding the pipe.
    ///
    /// If the sockets never arrive (e.g. `fork()` itself failed, or the child's `pre_exec` closure
    /// errored before reaching the send), there is no live child left unserved:
    /// `Command::spawn()` surfaces that same underlying failure as its own `Err`, via the
    /// close-on-exec error pipe, independently of this thread.
    pub(super) fn start_supervisor(
        parent_sock: OwnedFd,
        egress_policy: crate::egress_proxy::EgressPolicy,
        expected_namespace_sockets: usize,
        diag_read: OwnedFd,
    ) -> SupervisorHandle {
        let (proxy_tx, proxy_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let proxy = start_namespace_proxy(
                parent_sock.as_raw_fd(),
                expected_namespace_sockets,
                egress_policy,
            );
            // A closed receiver means the caller gave up waiting (its bounded `recv_timeout`
            // elapsed). Dropping the handle here then shuts the proxy down on this thread instead,
            // which is the same outcome by a different route — never a proxy left running.
            if let Err(returned) = proxy_tx.send(proxy) {
                if let Some(proxy) = returned.0 {
                    proxy.shutdown();
                }
            }
        });

        SupervisorHandle::Linux {
            proxy_rx,
            diag_read,
        }
    }

    /// Receives the in-namespace listening sockets and starts serving them.
    ///
    /// Returns `None` when the hand-off failed, which is not a silent degradation: the same
    /// `pre_exec` failure that prevented the send also makes the child return `Err`, so
    /// `Command::spawn()` reports it and no subprocess runs. There is no path where the capsule
    /// starts with a namespace whose proxy never came up — that would be a capsule with no
    /// network at all rather than an unbounded one, but it would present as an inexplicable
    /// outage, so it is logged here too.
    fn start_namespace_proxy(
        sock_fd: RawFd,
        expected: usize,
        policy: crate::egress_proxy::EgressPolicy,
    ) -> Option<crate::egress_proxy::EgressProxyHandle> {
        let sockets = match crate::network_namespace::receive_namespace_sockets(sock_fd, expected) {
            Ok(sockets) => sockets,
            Err(error) => {
                eprintln!(
                    "[capsule-runtime] warning: failed to receive the capsule's network \
                     namespace sockets: {error} (the subprocess spawn reports the underlying \
                     pre_exec failure independently)"
                );
                return None;
            }
        };
        match crate::egress_proxy::start_egress_proxy(sockets, policy) {
            Ok(proxy) => Some(proxy),
            Err(error) => {
                eprintln!(
                    "[capsule-runtime] warning: failed to serve the capsule's network namespace \
                     sockets: {error} (the subprocess tree has no route off this host, so its \
                     network calls fail closed)"
                );
                None
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::security_warnings::W_SEC_003;

    /// Content check only, deliberately: this asserts the *constant* the fd-hygiene call is
    /// built from, not the security property itself. A test that opened an extra fd, spawned a
    /// real subprocess and asserted the child could not see it would pass vacuously on every
    /// runner this repo's CI uses (macOS has no `close_range`; Linux CI never resolves to a
    /// kernel-enforcement tier), which would read as evidence while proving nothing. The
    /// property is verified by hand — see
    /// `docs/content/reference/subprocess-fd-hygiene-verification.md`.
    ///
    /// What this does catch is the one silent, catastrophic edit: a `0` here would mark stdio
    /// close-on-exec and break every subprocess invocation on both spawn paths.
    #[test]
    fn fd_hygiene_range_starts_above_stdio() {
        assert_eq!(
            FD_HYGIENE_FIRST_FD, 3,
            "fd hygiene must start at 3 — 0/1/2 are the stdio pipes both spawn paths need to \
             survive execve"
        );
    }

    /// A host that can back the sealed mechanism: AppArmor out of the way, and a namespace probe
    /// that really created one.
    fn sealed_capable() -> crate::sealed::SealedProbe {
        crate::sealed::SealedProbe {
            apparmor_permits_userns: true,
            namespace: crate::sealed::NamespaceProbe::Ok,
        }
    }

    /// A host that cannot: the default, which claims nothing.
    fn sealed_incapable() -> crate::sealed::SealedProbe {
        crate::sealed::SealedProbe::default()
    }

    #[test]
    fn tier_from_probe_non_linux_is_always_environment_only() {
        for landlock in [None, Some(true), Some(false)] {
            for sealed in [sealed_capable(), sealed_incapable()] {
                assert_eq!(
                    tier_from_probe(false, landlock, sealed),
                    EnforcementTier::EnvironmentOnly
                );
            }
        }
    }

    #[test]
    fn tier_from_probe_linux_fully_enforced_is_kernel_full() {
        assert_eq!(
            tier_from_probe(true, Some(true), sealed_incapable()),
            EnforcementTier::KernelFull
        );
    }

    #[test]
    fn tier_from_probe_linux_partially_enforced_is_seccomp_only() {
        assert_eq!(
            tier_from_probe(true, Some(false), sealed_incapable()),
            EnforcementTier::KernelSeccompOnly
        );
    }

    #[test]
    fn tier_from_probe_linux_no_probe_result_is_seccomp_only() {
        assert_eq!(
            tier_from_probe(true, None, sealed_incapable()),
            EnforcementTier::KernelSeccompOnly
        );
    }

    /// The whole conjunction, one element removed at a time: `KernelSealed` needs Linux, a usable
    /// Landlock ABI, AppArmor out of the way, and a namespace probe that succeeded — and drops to
    /// exactly the tier the missing element still supports, never further.
    #[test]
    fn tier_from_probe_reaches_sealed_only_when_every_precondition_holds() {
        use crate::sealed::{NamespaceProbe, SealedProbe};

        assert_eq!(
            tier_from_probe(true, Some(true), sealed_capable()),
            EnforcementTier::KernelSealed
        );

        // No AppArmor profile (and the restriction is on) → the mechanism is unavailable, but
        // Landlock still is: `scoped`, not `advisory`.
        assert_eq!(
            tier_from_probe(
                true,
                Some(true),
                SealedProbe { apparmor_permits_userns: false, namespace: NamespaceProbe::Ok }
            ),
            EnforcementTier::KernelFull
        );

        // The container case, and its three other namespace failure modes.
        for namespace in [
            NamespaceProbe::Denied,
            NamespaceProbe::MapDenied,
            NamespaceProbe::MountDenied,
            NamespaceProbe::Unsupported,
        ] {
            assert_eq!(
                tier_from_probe(
                    true,
                    Some(true),
                    SealedProbe { apparmor_permits_userns: true, namespace }
                ),
                EnforcementTier::KernelFull,
                "namespace probe {namespace:?} must fall back to KernelFull, never below it"
            );
        }

        // Landlock missing: sealed keeps Landlock inside as defence in depth, so a host without it
        // cannot reach sealed no matter how good its namespaces are.
        assert_eq!(
            tier_from_probe(true, Some(false), sealed_capable()),
            EnforcementTier::KernelSeccompOnly
        );
        assert_eq!(
            tier_from_probe(true, None, sealed_capable()),
            EnforcementTier::KernelSeccompOnly
        );

        // Not Linux at all.
        assert_eq!(
            tier_from_probe(false, Some(true), sealed_capable()),
            EnforcementTier::EnvironmentOnly
        );
    }

    /// A capsule that declared the weaker class keeps the weaker mechanism, even where the host
    /// could give it more. Silently installing a composed root under a `scoped` declaration would
    /// delete host paths its `interpreter_runtime` grants legitimately name.
    #[test]
    fn applied_tier_never_installs_a_composed_root_for_a_weaker_declaration() {
        use murmur_artifact::ContainmentClass;

        assert_eq!(
            applied_tier(EnforcementTier::KernelSealed, ContainmentClass::Sealed),
            EnforcementTier::KernelSealed
        );
        for declared in [ContainmentClass::Advisory, ContainmentClass::Scoped] {
            assert_eq!(
                applied_tier(EnforcementTier::KernelSealed, declared),
                EnforcementTier::KernelFull,
                "declaring {declared} must not opt a capsule into the composed root"
            );
        }

        // Every other tier is passed through untouched, whatever was declared — the declared floor
        // can only ever weaken the applied mechanism, never strengthen it past the host.
        for host_tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            for declared in [
                ContainmentClass::Advisory,
                ContainmentClass::Scoped,
                ContainmentClass::Sealed,
            ] {
                assert_eq!(applied_tier(host_tier, declared), host_tier);
            }
        }
    }

    #[test]
    fn sealed_bind_dirs_are_whole_directories_outside_the_fixed_runtime_paths() {
        use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant};

        let policy = CapabilityPolicy {
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![
                    // Already inside /usr, which the fixed list binds wholesale.
                    InterpreterRuntimeDir { path: "/usr/lib/python3.11".to_string(), list_dir: true },
                    InterpreterRuntimeDir { path: "/opt/py/lib".to_string(), list_dir: true },
                    // A duplicate, and a relative path that names nothing absolute.
                    InterpreterRuntimeDir { path: "/opt/py/lib".to_string(), list_dir: false },
                    InterpreterRuntimeDir { path: "relative/lib".to_string(), list_dir: false },
                ],
            }],
            ..CapabilityPolicy::default()
        };
        let exec_allow = vec![
            PathBuf::from("/usr/bin/python3"),
            PathBuf::from("/opt/toolchain/bin/cc"),
        ];

        assert_eq!(
            resolve_sealed_bind_dirs(&exec_allow, &policy),
            vec![PathBuf::from("/opt/toolchain/bin"), PathBuf::from("/opt/py/lib")]
        );
    }

    /// Staged trees are resolved by a different rule than `sealed_bind_dirs` above, and the
    /// differences are the point: a path inside a fixed runtime path is kept (dropping it would
    /// silently make a required grant depend on an unrelated list), duplicates collapse, and a
    /// relative path — which cannot be re-based under the composed root at all — is skipped.
    #[test]
    fn staged_runtime_dirs_keep_fixed_path_overlaps_and_drop_only_duplicates_and_relatives() {
        use murmur_artifact::StagedRuntimeGrant;

        let grant = |binary: &str, source: &str| StagedRuntimeGrant {
            binary: binary.to_string(),
            source_path: source.to_string(),
            pin: "pin-1".to_string(),
        };
        let policy = CapabilityPolicy {
            shell_staged_runtime: vec![
                grant("python3", "/opt/py"),
                // Inside /usr, which the fixed list already binds. Kept anyway — unlike
                // `resolve_sealed_bind_dirs`, which drops exactly this case.
                grant("perl", "/usr/lib/perl5"),
                // A duplicate of the first, and a relative path naming nothing absolute.
                grant("python3.12", "/opt/py"),
                grant("ruby", "relative/ruby"),
            ],
            ..CapabilityPolicy::default()
        };

        assert_eq!(
            resolve_staged_runtime_dirs(&policy),
            vec![PathBuf::from("/opt/py"), PathBuf::from("/usr/lib/perl5")]
        );
    }

    /// The bind is only half of it: `sealed` keeps Landlock installed inside the composed root, so
    /// a staged tree with no Landlock rule is mounted-but-unreadable (`EACCES`). Both resolvers
    /// must therefore cover the same directories.
    #[test]
    fn every_staged_runtime_dir_also_gets_a_listable_landlock_grant() {
        use murmur_artifact::StagedRuntimeGrant;

        let policy = CapabilityPolicy {
            shell_staged_runtime: vec![StagedRuntimeGrant {
                binary: "python3".to_string(),
                source_path: "/opt/py".to_string(),
                pin: "pin-1".to_string(),
            }],
            ..CapabilityPolicy::default()
        };

        let dirs = resolve_staged_runtime_dirs(&policy);
        let grants = resolve_staged_runtime_landlock_grants(&policy);

        assert_eq!(
            grants,
            vec![LandlockGrant {
                path: PathBuf::from("/opt/py"),
                list_dir: true,
                executable: true,
            }],
            "a staged tree is walked by the runtime that uses it, so it must be listable",
        );
        assert_eq!(
            grants.iter().map(|grant| grant.path.clone()).collect::<Vec<_>>(),
            dirs,
            "the bound set and the granted set must not drift apart",
        );
    }

    #[test]
    fn every_sealed_runtime_path_gets_a_listable_non_executable_landlock_grant() {
        let grants = resolve_sealed_runtime_landlock_grants(EnforcementTier::KernelSealed);

        assert_eq!(
            grants,
            crate::sealed::SEALED_RUNTIME_PATHS
                .iter()
                .map(|path| LandlockGrant {
                    path: PathBuf::from(path),
                    list_dir: true,
                    executable: false,
                })
                .collect::<Vec<_>>(),
            "the composed root binds exactly these, so exactly these must be enumerable — \
             the bound set and the granted set must not drift apart",
        );
        assert!(
            grants.iter().all(|grant| grant.list_dir),
            "a runtime tree an interpreter walks is useless without getdents64",
        );
        assert!(
            grants.iter().all(|grant| !grant.executable),
            "Execute is the exec allowlist: granting it over /usr, /bin and /sbin would let a \
             sealed capsule run every binary the host ships, whatever shell.allow says",
        );
    }

    #[test]
    fn sealed_runtime_landlock_grants_are_sealed_tier_only() {
        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            assert!(
                resolve_sealed_runtime_landlock_grants(tier).is_empty(),
                "{tier:?} has no composed root, so granting ReadDir on SEALED_RUNTIME_PATHS \
                 would enumerate the real host filesystem",
            );
        }
    }

    /// The gate as `ShellEnforcement::resolve` actually applies it. `applied_tier` never returns
    /// `KernelSealed` for a capsule declaring `scoped`, on any host, so this half is host-
    /// independent; the `sealed` half asserts against whatever tier this host resolved to, which
    /// is the only honest thing a test can do about a kernel capability it may not have.
    #[test]
    fn shell_enforcement_grants_sealed_runtime_paths_only_when_sealed_applies() {
        let policy = CapabilityPolicy::default();
        let usr = PathBuf::from("/usr");

        let scoped = ShellEnforcement::resolve(&policy, murmur_artifact::ContainmentClass::Scoped)
            .expect("an empty policy resolves");
        assert!(
            !scoped.landlock_grants.iter().any(|grant| grant.path == usr),
            "a scoped capsule runs Landlock over the real host filesystem — /usr must stay \
             unenumerable there",
        );

        let sealed = ShellEnforcement::resolve(&policy, murmur_artifact::ContainmentClass::Sealed)
            .expect("an empty policy resolves");
        let granted: Vec<&LandlockGrant> = sealed
            .landlock_grants
            .iter()
            .filter(|grant| {
                crate::sealed::SEALED_RUNTIME_PATHS.contains(&grant.path.to_str().unwrap_or(""))
            })
            .collect();
        if sealed.tier == EnforcementTier::KernelSealed {
            assert_eq!(
                granted.len(),
                crate::sealed::SEALED_RUNTIME_PATHS.len(),
                "a sealed session must carry one listable grant per bound runtime path",
            );
            assert!(granted
                .iter()
                .all(|grant| grant.list_dir && !grant.executable));
        } else {
            assert!(
                granted.is_empty(),
                "this host fell back to {:?}, where there is no composed root to enumerate",
                sealed.tier,
            );
        }
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

    // ---- invoked-binary reporting (shell-event.binary / trace.jsonl `binary`) ----

    #[test]
    fn resolve_invoked_binary_path_resolves_bare_name_via_path_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        let tool = bin_dir.join("mytool");
        make_executable(&tool, "#!/bin/sh\nexit 0\n");

        let resolved = resolve_invoked_binary_path_in("mytool", &[bin_dir]);
        assert_eq!(
            resolved,
            std::fs::canonicalize(&tool)
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "a resolvable name must report the canonical absolute path, not the bare name"
        );
    }

    #[test]
    fn resolve_invoked_binary_path_falls_back_to_bare_name_when_unresolvable() {
        let temp = tempfile::tempdir().unwrap();
        let path_dirs = [temp.path().to_path_buf()];
        let resolved = resolve_invoked_binary_path_in("definitely-not-a-real-binary", &path_dirs);
        assert_eq!(
            resolved, "definitely-not-a-real-binary",
            "an unresolvable name must fall back to the bare name, never error"
        );
        assert_eq!(resolve_invoked_binary_path_in("", &[]), "");
    }

    #[test]
    fn resolve_invoked_binary_path_takes_the_first_path_match() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        make_executable(&first.join("mytool"), "#!/bin/sh\nexit 0\n");
        make_executable(&second.join("mytool"), "#!/bin/sh\nexit 1\n");

        let resolved = resolve_invoked_binary_path_in("mytool", &[first.clone(), second]);
        assert_eq!(
            resolved,
            std::fs::canonicalize(first.join("mytool"))
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "reporting must match execvp: the first PATH hit is the one that ran"
        );
    }

    #[test]
    fn resolve_invoked_binary_path_ignores_non_executable_path_matches() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("mytool"), "not executable").unwrap();
        assert_eq!(
            resolve_invoked_binary_path_in("mytool", &[temp.path().to_path_buf()]),
            "mytool"
        );
    }

    #[test]
    fn resolve_invoked_binary_path_canonicalizes_a_path_entry_through_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-tool");
        make_executable(&real, "#!/bin/sh\nexit 0\n");
        let link = temp.path().join("link-tool");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            resolve_invoked_binary_path_in(&link.to_string_lossy(), &[]),
            std::fs::canonicalize(&real)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
    }

    // ---- ELF dependency parsing (pure, synthetic fixtures — runs on every OS) ----

    /// Builds a minimal but well-formed ELF64 image exercising exactly the fields
    /// `parse_elf_dependencies` reads: a single identity-mapped `PT_LOAD` (so virtual address ==
    /// file offset), an optional `PT_INTERP`, and — when any dynamic metadata is requested — a
    /// `PT_DYNAMIC` with `DT_NEEDED`/`DT_RUNPATH`/`DT_STRTAB`/`DT_STRSZ`/`DT_NULL` entries.
    fn build_elf64(interp: Option<&str>, needed: &[&str], runpath: Option<&str>) -> Vec<u8> {
        let has_dynamic = !needed.is_empty() || runpath.is_some();

        let mut phnum: u16 = 1; // PT_LOAD
        if interp.is_some() {
            phnum += 1;
        }
        if has_dynamic {
            phnum += 1;
        }

        let ehsize = 64usize;
        let phentsize = 56usize;
        let phoff = ehsize;
        let body_start = phoff + phentsize * phnum as usize;

        // Dynamic string table: index 0 is a NUL, then each soname and the runpath.
        let mut strtab: Vec<u8> = vec![0];
        let mut needed_offsets = Vec::new();
        for name in needed {
            needed_offsets.push(strtab.len() as u64);
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
        }
        let runpath_offset = runpath.map(|r| {
            let off = strtab.len() as u64;
            strtab.extend_from_slice(r.as_bytes());
            strtab.push(0);
            off
        });

        let mut body: Vec<u8> = Vec::new();

        let interp_offset = body_start + body.len();
        if let Some(i) = interp {
            body.extend_from_slice(i.as_bytes());
            body.push(0);
        }

        // Align the dynamic array to 8 bytes.
        while !(body_start + body.len()).is_multiple_of(8) {
            body.push(0);
        }
        let dyn_offset = body_start + body.len();
        let num_dyn = needed.len() + runpath.map_or(0, |_| 1) + 3; // + DT_STRTAB, DT_STRSZ, DT_NULL
        let dyn_size = num_dyn * 16;
        let strtab_offset = dyn_offset + dyn_size;

        if has_dynamic {
            let mut push_dyn = |tag: u64, val: u64| {
                body.extend_from_slice(&tag.to_le_bytes());
                body.extend_from_slice(&val.to_le_bytes());
            };
            for &o in &needed_offsets {
                push_dyn(1, o); // DT_NEEDED
            }
            if let Some(ro) = runpath_offset {
                push_dyn(29, ro); // DT_RUNPATH
            }
            push_dyn(5, strtab_offset as u64); // DT_STRTAB (vaddr == offset, identity PT_LOAD)
            push_dyn(10, strtab.len() as u64); // DT_STRSZ
            push_dyn(0, 0); // DT_NULL
            assert_eq!(body_start + body.len(), strtab_offset);
            body.extend_from_slice(&strtab);
        }

        let total_len = body_start + body.len();
        let mut out = vec![0u8; total_len];
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // little-endian
        out[6] = 1; // EI_VERSION
        out[16..18].copy_from_slice(&3u16.to_le_bytes()); // e_type = ET_DYN
        out[18..20].copy_from_slice(&0x3eu16.to_le_bytes()); // e_machine = x86_64 (irrelevant)
        out[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
        out[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        out[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes()); // e_phentsize
        out[56..58].copy_from_slice(&phnum.to_le_bytes()); // e_phnum

        let mut phdrs: Vec<(u32, u64, u64, u64)> = Vec::new(); // (p_type, p_offset, p_vaddr, p_filesz)
        phdrs.push((1, 0, 0, total_len as u64)); // PT_LOAD identity-maps the whole file
        if let Some(i) = interp {
            phdrs.push((3, interp_offset as u64, interp_offset as u64, i.len() as u64 + 1));
        }
        if has_dynamic {
            phdrs.push((2, dyn_offset as u64, dyn_offset as u64, dyn_size as u64));
        }
        for (idx, (p_type, p_offset, p_vaddr, p_filesz)) in phdrs.iter().enumerate() {
            let base = phoff + idx * phentsize;
            out[base..base + 4].copy_from_slice(&p_type.to_le_bytes());
            out[base + 8..base + 16].copy_from_slice(&p_offset.to_le_bytes());
            out[base + 16..base + 24].copy_from_slice(&p_vaddr.to_le_bytes());
            out[base + 32..base + 40].copy_from_slice(&p_filesz.to_le_bytes());
        }

        out[body_start..total_len].copy_from_slice(&body);
        out
    }

    #[test]
    fn parse_elf_extracts_interp_needed_and_runpath() {
        let bytes = build_elf64(
            Some("/lib64/ld-linux-x86-64.so.2"),
            &["libc.so.6", "libm.so.6"],
            Some("$ORIGIN/../lib:/opt/custom/lib"),
        );
        let deps = parse_elf_dependencies(&bytes).expect("well-formed ELF64 must parse");
        assert_eq!(deps.interp.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
        assert_eq!(deps.needed, vec!["libc.so.6".to_string(), "libm.so.6".to_string()]);
        assert_eq!(
            deps.runpaths,
            vec!["$ORIGIN/../lib".to_string(), "/opt/custom/lib".to_string()],
            "DT_RUNPATH must be split on ':' with empty parts dropped"
        );
    }

    #[test]
    fn parse_elf_static_binary_contributes_only_itself() {
        // No PT_INTERP, no PT_DYNAMIC — a statically linked binary.
        let bytes = build_elf64(None, &[], None);
        let deps = parse_elf_dependencies(&bytes).expect("a static ELF64 is still valid ELF");
        assert_eq!(deps, ElfDependencies::default());
    }

    #[test]
    fn parse_elf_rejects_non_elf_and_non_elf64() {
        assert!(parse_elf_dependencies(b"not an elf at all, way too short").is_none());
        // Correct magic but ELFCLASS32 (byte 4 == 1) must be rejected.
        let mut elf32 = build_elf64(Some("/lib/ld.so"), &["libc.so.6"], None);
        elf32[4] = 1;
        assert!(
            parse_elf_dependencies(&elf32).is_none(),
            "32-bit ELF is not a binary this runtime execs and must not parse"
        );
    }

    // ---- soname resolution (loader search-order, synthetic fixtures) ----

    #[test]
    fn resolve_soname_finds_via_search_dirs_and_runpath_origin() {
        let temp = tempfile::tempdir().unwrap();
        let origin = temp.path().join("app");
        std::fs::create_dir(&origin).unwrap();
        let libs = temp.path().join("libs");
        std::fs::create_dir(&libs).unwrap();
        std::fs::write(libs.join("libc.so.6"), "x").unwrap();

        // Found in a standard search directory.
        assert_eq!(
            resolve_soname("libc.so.6", &origin, &[], std::slice::from_ref(&libs)),
            Some(std::fs::canonicalize(libs.join("libc.so.6")).unwrap())
        );
        // Unresolvable soname → None (shrink-not-fail).
        assert_eq!(
            resolve_soname("libmissing.so", &origin, &[], std::slice::from_ref(&libs)),
            None
        );

        // Found via a `$ORIGIN`-relative DT_RUNPATH, taking precedence over the search dirs.
        let origin_lib = origin.join("lib");
        std::fs::create_dir(&origin_lib).unwrap();
        std::fs::write(origin_lib.join("libfoo.so"), "x").unwrap();
        assert_eq!(
            resolve_soname("libfoo.so", &origin, &["$ORIGIN/lib".to_string()], &[]),
            Some(std::fs::canonicalize(origin_lib.join("libfoo.so")).unwrap())
        );
    }

    // ---- transitive Landlock grant derivation (synthetic ELF + fake libs) ----

    #[test]
    fn resolve_landlock_grants_derives_interp_and_needed_closure() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        let lib_dir = temp.path().join("lib");
        std::fs::create_dir(&bin_dir).unwrap();
        std::fs::create_dir(&lib_dir).unwrap();

        // A fake interpreter and a fake shared library, both outside any "workdir".
        let interp = bin_dir.join("ld-fake.so");
        std::fs::write(&interp, "not really elf").unwrap();
        std::fs::write(lib_dir.join("libfake.so.1"), "not really elf").unwrap();

        // A synthetic dynamically-linked binary naming that interp + that soname.
        let prog = bin_dir.join("prog");
        let elf = build_elf64(
            Some(interp.to_str().unwrap()),
            &["libfake.so.1"],
            None,
        );
        std::fs::write(&prog, &elf).unwrap();

        let grants =
            resolve_landlock_grants_in(std::slice::from_ref(&prog), std::slice::from_ref(&lib_dir));

        let expect = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        assert!(grants.contains(&expect(prog)), "the binary itself must be granted: {grants:?}");
        assert!(grants.contains(&expect(interp)), "the ELF interpreter must be granted: {grants:?}");
        assert!(
            grants.contains(&expect(lib_dir.join("libfake.so.1"))),
            "each resolved DT_NEEDED library must be granted: {grants:?}"
        );
    }

    #[test]
    fn resolve_landlock_grants_skips_unresolvable_deps_and_missing_binaries() {
        let temp = tempfile::tempdir().unwrap();
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();

        // A binary naming a soname that resolves nowhere, plus an interp that doesn't exist.
        let prog = bin_dir.join("prog");
        std::fs::write(
            &prog,
            build_elf64(Some("/definitely/not/here.so"), &["libnope.so"], None),
        )
        .unwrap();
        let missing = bin_dir.join("does-not-exist");

        let grants =
            resolve_landlock_grants_in(&[prog.clone(), missing], std::slice::from_ref(&bin_dir));

        // Only the real binary survives; the missing binary, missing interp, and unresolvable
        // soname each contribute nothing (shrink-not-fail).
        assert_eq!(grants, vec![std::fs::canonicalize(&prog).unwrap()]);
    }

    // ---- interpreter_runtime → LandlockGrant construction (pure, syscall-free, every OS) ----

    fn interp_grant(binary: &str, dirs: &[(&str, bool)]) -> InterpreterRuntimeGrant {
        InterpreterRuntimeGrant {
            binary: binary.to_string(),
            dirs: dirs
                .iter()
                .map(|(path, list_dir)| murmur_artifact::InterpreterRuntimeDir {
                    path: path.to_string(),
                    list_dir: *list_dir,
                })
                .collect(),
        }
    }

    #[test]
    fn resolve_interpreter_runtime_grants_copies_list_dir_verbatim() {
        // Two directories under the same binary, opposite `list_dir` — proving enumerability is
        // per-directory and copied exactly, not inferred or unified.
        let grants = resolve_interpreter_runtime_grants(&[interp_grant(
            "python3",
            &[
                ("/usr/lib/python3.11", true),
                ("/usr/lib/python3.11/lib-dynload", false),
            ],
        )]);

        assert_eq!(
            grants,
            vec![
                LandlockGrant {
                    path: PathBuf::from("/usr/lib/python3.11"),
                    list_dir: true,
                    executable: true,
                },
                LandlockGrant {
                    path: PathBuf::from("/usr/lib/python3.11/lib-dynload"),
                    list_dir: false,
                    executable: true,
                },
            ]
        );
    }

    #[test]
    fn resolve_interpreter_runtime_grants_flattens_multiple_grants_and_needs_no_existing_path() {
        // The declared paths need not exist on this host (this is pure resolution — `apply_landlock_
        // scope` skips any that fail to open), and multiple grants flatten in order.
        let grants = resolve_interpreter_runtime_grants(&[
            interp_grant("python3", &[("/opt/py/nonexistent", false)]),
            interp_grant("ruby", &[("/opt/rb/also-nonexistent", true)]),
        ]);

        assert_eq!(
            grants,
            vec![
                LandlockGrant {
                    path: PathBuf::from("/opt/py/nonexistent"),
                    list_dir: false,
                    executable: true,
                },
                LandlockGrant {
                    path: PathBuf::from("/opt/rb/also-nonexistent"),
                    list_dir: true,
                    executable: true,
                },
            ]
        );
        assert!(resolve_interpreter_runtime_grants(&[]).is_empty());
    }

    #[test]
    fn non_listable_files_marks_every_closure_file_non_enumerable() {
        // The DT_NEEDED closure yields individual files; wrapping them must never set `list_dir`
        // (ReadDir on a regular file was always a no-op — this is the slice's pure correction).
        let wrapped = LandlockGrant::non_listable_files(vec![
            PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6"),
            PathBuf::from("/usr/bin/bash"),
        ]);
        assert!(wrapped.iter().all(|grant| !grant.list_dir), "{wrapped:?}");
        assert_eq!(
            wrapped.iter().map(|g| g.path.clone()).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/lib/x86_64-linux-gnu/libc.so.6"),
                PathBuf::from("/usr/bin/bash"),
            ]
        );
    }

    // ---- `CAPSULE_DEVICE_GRANTS` (fixed device set) ----------------------------------------
    //
    // *Content* checks on a hand-authored constant, in the same category as the
    // `WORKDIR_ACCESS_RIGHTS` bit-membership test and the `denied_socket_domains` tests: they
    // assert what a Rust constant holds, NOT that any kernel grants or refuses anything. Nothing
    // here — and no green CI run — is evidence that `/dev/null` is actually writable inside a
    // capsule, or that `/dev/random` is actually refused. Those are enforcement claims about a
    // real kernel, verified only by the manual procedure in
    // `docs/content/reference/security-warnings.md` ("Manual acceptance procedure — the fixed
    // capsule device set"), on real, uncontainerized Linux hardware. This repo's CI has never
    // resolved to a tier where `apply_landlock_scope` even runs.

    #[test]
    fn capsule_device_grants_are_exactly_three_devices_with_only_dev_null_writable() {
        let listed: Vec<(&str, bool)> = CAPSULE_DEVICE_GRANTS
            .iter()
            .map(|device| (device.path, device.writable))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("/dev/null", true),
                ("/dev/zero", false),
                ("/dev/urandom", false),
            ],
            "the capsule device set is fixed at exactly these three paths and these access \
             levels. /dev/null must stay writable — Python's subprocess.DEVNULL opens it O_RDWR \
             and a shell `2>/dev/null` redirect opens it O_WRONLY, so a read-only grant breaks \
             both. Widening this list is allowed only on demonstrated workload failure, and the \
             failure gets recorded alongside the widening (see the manual acceptance procedure on \
             the security-warnings reference page)."
        );
    }

    #[test]
    fn capsule_device_grants_exclude_the_devices_deliberately_left_out() {
        // `/dev/random` is the one with a reasoning trap attached: a plain write() to it credits
        // *zero* entropy (crediting needs the RNDADDENTROPY ioctl under CAP_SYS_ADMIN, which the
        // shell child has dropped). It is excluded because nothing needs a blocking RNG when
        // /dev/urandom is granted — not because writing to it would poison the kernel pool.
        for excluded in [
            "/dev/random",
            "/dev/full",
            "/dev/tty",
            "/dev/console",
            "/dev/mem",
            "/dev/sda",
            "/dev",
        ] {
            assert!(
                !CAPSULE_DEVICE_GRANTS
                    .iter()
                    .any(|device| device.path == excluded),
                "{excluded} must not be in the capsule device set — it is denied by the ordinary \
                 no-matching-rule path, and adding it here would be the only way to change that"
            );
        }
    }

    #[test]
    fn capsule_device_grants_name_absolute_paths_under_dev() {
        // The list is opened verbatim with `O_PATH` in the parent: a relative path would resolve
        // against whatever cwd the host process happens to have.
        for device in CAPSULE_DEVICE_GRANTS {
            assert!(
                device.path.starts_with("/dev/"),
                "{} must be an absolute path under /dev",
                device.path
            );
        }
    }

    // ---- `denied_socket_domains` ----------------------------------------------------------
    //
    // These are *content* checks on a hand-authored constant list, in the same category as the
    // `WORKDIR_ACCESS_RIGHTS` bit-membership test below: they assert what the function returns,
    // NOT that any kernel refuses anything. Nothing here — and no green CI run — is evidence
    // that `socket(AF_UNIX, ...)` is actually denied on a real host. That claim is verified only
    // by the manual procedure in `docs/content/reference/security-warnings.md`
    // ("Manual acceptance procedure — unmediated AF_UNIX sockets"), on real Linux hardware.

    #[test]
    fn denied_socket_domains_denies_unix_by_default() {
        let denied = denied_socket_domains(false);
        assert!(
            denied.contains(&LINUX_AF_UNIX),
            "AF_UNIX must be denied when the manifest does not declare \
             capabilities.network.unix_sockets — this is the /var/run/docker.sock escape"
        );
    }

    #[test]
    fn denied_socket_domains_omits_unix_only_when_explicitly_allowed() {
        let denied = denied_socket_domains(true);
        assert!(
            !denied.contains(&LINUX_AF_UNIX),
            "a capsule that declared capabilities.network.unix_sockets: true must get no \
             AF_UNIX deny rule at all"
        );
    }

    /// Netlink and packet sockets have no manifest key that widens them: the deny list must be
    /// identical for both values of the one flag that exists.
    #[test]
    fn denied_socket_domains_always_denies_netlink_and_packet() {
        for unix_sockets_allowed in [false, true] {
            let denied = denied_socket_domains(unix_sockets_allowed);
            assert!(
                denied.contains(&LINUX_AF_NETLINK),
                "AF_NETLINK must be denied unconditionally (unix_sockets_allowed = \
                 {unix_sockets_allowed})"
            );
            assert!(
                denied.contains(&LINUX_AF_PACKET),
                "AF_PACKET must be denied unconditionally (unix_sockets_allowed = \
                 {unix_sockets_allowed})"
            );
        }
    }

    /// The rule only ever subtracts: IP sockets stay governed by the capsule's own network
    /// namespace and egress proxy against `capabilities.network.allow`, never by a domain-level
    /// deny.
    #[test]
    fn denied_socket_domains_never_denies_ip_families() {
        for unix_sockets_allowed in [false, true] {
            let denied = denied_socket_domains(unix_sockets_allowed);
            assert!(!denied.contains(&LINUX_AF_INET_DOMAIN));
            assert!(!denied.contains(&LINUX_AF_INET6_DOMAIN));
        }
    }

    /// Pins the exact sets, so widening either one is a deliberate edit to this test rather than
    /// an unnoticed side effect of touching the function.
    #[test]
    fn denied_socket_domains_returns_exactly_the_documented_sets() {
        assert_eq!(
            denied_socket_domains(false),
            vec![LINUX_AF_NETLINK, LINUX_AF_PACKET, LINUX_AF_UNIX]
        );
        assert_eq!(
            denied_socket_domains(true),
            vec![LINUX_AF_NETLINK, LINUX_AF_PACKET]
        );
    }

    /// The domain constants are Linux ABI numbers spelled as literals (see their doc comment),
    /// so nothing checks them against `libc` at compile time. On a Linux build machine, assert
    /// they really do agree with `libc`; on macOS this test compiles away, because
    /// `libc::AF_NETLINK`/`AF_PACKET` do not exist there at all — which is the whole reason the
    /// constants are literals.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_socket_domain_constants_match_libc() {
        assert_eq!(LINUX_AF_UNIX, libc::AF_UNIX);
        assert_eq!(LINUX_AF_NETLINK, libc::AF_NETLINK);
        assert_eq!(LINUX_AF_PACKET, libc::AF_PACKET);
        assert_eq!(LINUX_AF_INET_DOMAIN, libc::AF_INET);
        assert_eq!(LINUX_AF_INET6_DOMAIN, libc::AF_INET6);
    }

    /// The allow and deny domain sets feed two rule loops on the *same* syscall and argument, so
    /// an overlap would mean one `socket()` call matching both an `Allow` and an `Errno` rule —
    /// a filter whose behavior depends on libseccomp's internal rule ordering rather than on
    /// anything stated here.
    #[test]
    fn socket_domain_allow_and_deny_sets_never_overlap() {
        for unix_sockets_allowed in [false, true] {
            let denied = denied_socket_domains(unix_sockets_allowed);
            for domain in allowed_socket_domains(unix_sockets_allowed) {
                assert!(
                    !denied.contains(&domain),
                    "socket domain {domain} is both allowed and denied \
                     (unix_sockets_allowed = {unix_sockets_allowed})"
                );
            }
        }
    }

    /// `capabilities.network.unix_sockets` is the only manifest key either set consults, and it
    /// must move `AF_UNIX` from one set to the other — never leave it in neither, which under a
    /// default-deny filter would silently keep unix sockets refused even after a capsule declared
    /// the capability.
    #[test]
    fn allowed_socket_domains_takes_unix_back_exactly_when_declared() {
        assert_eq!(
            allowed_socket_domains(false),
            vec![LINUX_AF_INET_DOMAIN, LINUX_AF_INET6_DOMAIN]
        );
        assert_eq!(
            allowed_socket_domains(true),
            vec![LINUX_AF_INET_DOMAIN, LINUX_AF_INET6_DOMAIN, LINUX_AF_UNIX]
        );
    }

    // ---- `SECCOMP_SYSCALL_ALLOWLIST` -------------------------------------------------------
    //
    // Same category as the `denied_socket_domains` tests above: *content* checks on a
    // hand-authored constant list. Nothing here — and no green CI run — is evidence that any
    // kernel refuses `io_uring_setup` on a real host; CI never resolves to a kernel enforcement
    // tier, so a green suite has repeatedly meant nothing for this module. The claim that these
    // syscalls are actually denied is verified only by the hand-run escape-conformance harness
    // (`crates/capsule-runtime/escape-conformance/`, cases `syscall-*`), on real bare-metal Linux
    // hardware. What these tests
    // *do* buy is that re-permitting one of the dangerous syscalls cannot happen by accident
    // while reconciling the allowlist against a newer upstream profile.

    #[test]
    fn allowlist_contains_no_syscall_that_must_stay_denied() {
        for name in SECCOMP_MUST_STAY_DENIED {
            assert!(
                !SECCOMP_SYSCALL_ALLOWLIST.contains(name),
                "{name} is in SECCOMP_MUST_STAY_DENIED but was added to \
                 SECCOMP_SYSCALL_ALLOWLIST — if this is deliberate, remove it from the deny list \
                 in the same edit and say why in the commit"
            );
        }
    }

    /// `execve`/`execveat` must be *present*, which is the inversion this slice performed: they
    /// used to be excluded because a `Notify` rule owned them, and a plain `Allow` would have taken
    /// the decision away from the supervisor. With the supervisor deleted, the default-deny action
    /// applies to anything unnamed — so omitting them now would refuse the child's own first
    /// `execve` and no capsule could run a shell tool at all. Exec is decided by the Landlock
    /// domain instead; see `linux_enforce::workdir_access_rights` and `resolve_landlock_grants`.
    #[test]
    fn allowlist_permits_exec_because_landlock_now_decides_it() {
        for name in ["execve", "execveat"] {
            assert!(
                SECCOMP_SYSCALL_ALLOWLIST.contains(&name),
                "{name} must reach the kernel — the default action is a deny, so omitting it \
                 refuses the child's own exec and nothing runs"
            );
        }
    }

    /// `socket` carries argument-conditional rules, and libseccomp resolves an unconditional rule
    /// for a syscall by discarding every conditional chain already recorded for it. A bare
    /// `"socket"` in the allowlist array would therefore delete the AF_UNIX/AF_NETLINK/AF_PACKET
    /// denials at filter-build time, with nothing at runtime to show for it.
    #[test]
    fn allowlist_excludes_socket_which_is_ruled_on_by_domain() {
        assert!(
            !SECCOMP_SYSCALL_ALLOWLIST.contains(&"socket"),
            "socket must be permitted per-domain by allowed_socket_domains, never unconditionally"
        );
    }

    /// A duplicate is harmless to the kernel but means two edits disagree about the same syscall,
    /// which is exactly the state where one of them later gets removed and the other is missed.
    #[test]
    fn allowlist_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for name in SECCOMP_SYSCALL_ALLOWLIST {
            assert!(seen.insert(*name), "{name} appears twice in the allowlist");
        }
    }

    /// The whole design is an allowlist. An empty (or near-empty) array would still compile and
    /// still "pass" every other test here, while denying every syscall a shell needs.
    #[test]
    fn allowlist_covers_the_syscalls_no_process_can_run_without() {
        for name in [
            "read", "write", "openat", "close", "mmap", "munmap", "brk", "exit_group", "futex",
            "rt_sigreturn", "clone", "wait4", "getpid", "getdents64",
        ] {
            assert!(
                SECCOMP_SYSCALL_ALLOWLIST.contains(&name),
                "{name} is missing from the allowlist — no process could run under this filter"
            );
        }
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

    /// The aggregate-bounding gap is warned about exactly where it is real *and* permanent: a
    /// non-Linux host running a capsule that can spawn a subprocess. Never on Linux — there the
    /// same condition refuses the launch outright, so a warning would describe a session that
    /// does not exist — and never for a capsule with no process tree to bound.
    #[test]
    fn aggregate_bounding_warns_only_off_linux_and_only_with_a_subprocess_route() {
        assert_eq!(
            aggregate_bounding_warning(false, true, false),
            Some((W_SEC_010, NO_AGGREGATE_BOUNDING_WARNING))
        );
        assert_eq!(
            aggregate_bounding_warning(false, false, false),
            None,
            "a capsule that cannot spawn a subprocess has nothing to bound"
        );
        assert_eq!(
            aggregate_bounding_warning(true, true, false),
            None,
            "on Linux this case is a refused launch, not a warning"
        );
        assert_eq!(
            aggregate_bounding_warning(true, true, true),
            None,
            "a scope exists, so there is no gap"
        );
    }

    /// The warning must name the per-uid nature of `RLIMIT_NPROC` explicitly: "rlimits are still
    /// applied" reads as reassurance, and the whole point is that the one rlimit an operator
    /// would expect to stop a fork bomb does not.
    #[test]
    fn aggregate_bounding_warning_states_why_rlimit_nproc_is_insufficient() {
        let (_, message) = aggregate_bounding_warning(false, true, false).unwrap();
        assert!(message.contains("per-uid"), "message was: {message}");
        assert!(message.contains("fork bomb"), "message was: {message}");
        assert!(message.contains("pids.max"), "message was: {message}");
    }

    #[test]
    fn warn_for_missing_aggregate_bounding_writes_the_code_and_link_to_bootstrap_log() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_missing_aggregate_bounding(temp.path(), true, false);
        let log = bootstrap_log_contents(temp.path());

        if cfg!(target_os = "linux") {
            assert!(
                log.is_empty(),
                "Linux refuses the launch instead of warning: {log}"
            );
        } else {
            assert!(log.contains(W_SEC_010), "log was: {log}");
            assert!(log.contains(&security_warning_link(W_SEC_010)), "log was: {log}");
        }
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
            "KernelFull must not be silent — it must warn its enforcement is not yet confirmed: {log}"
        );
        assert!(
            log.contains("not yet been verified"),
            "must state the derived-grant mechanism is not yet team-verified on real hardware: {log}"
        );
        assert!(
            log.contains("read+execute") && log.contains("outside the workdir"),
            "must describe what is now actually granted (narrow, derived read+execute outside \
             the workdir), not an unfixed grant bug: {log}"
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
                error.to_string().contains("sandbox"),
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
mod landlock_probe_isolation {
    /// The Landlock probe must leave the calling process exactly as it found it.
    ///
    /// Stated as "the sealed probe gives the same answer before and after" rather than by
    /// inspecting the process's Landlock domain, because there is no API to ask whether a task
    /// is landlocked — and because that framing is what actually broke: the probe restricted
    /// itself, and every later attempt to build a composed root was refused, on every host, in
    /// a way that looked like the host's fault.
    ///
    /// Host-independent by construction. On a machine that cannot do `sealed` at all, both
    /// halves report the same failure and this passes trivially; on a capable one, an
    /// in-process `restrict_self` turns the second answer into a mount denial and fails it.
    #[test]
    fn the_landlock_probe_does_not_restrict_the_calling_process() {
        let before = crate::sealed::probe_sealed_support().namespace;
        let _ = super::linux_enforce::probe_landlock_full_access();
        let after = crate::sealed::probe_sealed_support().namespace;

        assert_eq!(
            before, after,
            "the Landlock probe changed what the sealed probe reports ({before:?} -> {after:?}) — \
             it has restricted the calling process instead of a forked child, which permanently \
             forbids the mount family to this process and everything it spawns",
        );
    }
}

#[cfg(test)]
mod linux_integration_tests {
    use std::path::Path;

    use super::*;

    /// Spread-base supplying the four *host-bounding* fields (`resource_limits`,
    /// `nproc_baseline`, `cgroup_scope`, `workdir_guard`) to the enforcement literals below,
    /// which spell out the policy half by hand because they need to pin `tier` explicitly.
    ///
    /// These tests exercise seccomp, Landlock and the network allowlist — not rlimits or
    /// cgroups — so they take exactly what `ShellEnforcement::resolve` produces before
    /// `with_host_bounding` attaches a live session's handles: the real default ceilings (not
    /// "unbounded" — zeroing them would misrepresent what a real host does, and the rlimit half
    /// of the child `pre_exec` runs on every tier), the real uid baseline, and no cgroup scope
    /// or workdir guard. The `tier` it carries is always overridden by the literal that spreads
    /// it; only the host-bounding tail is ever consumed from here.
    fn host_bounding_base() -> ShellEnforcement {
        ShellEnforcement::environment_only()
    }

    /// Pure content check on the workdir access-right set: no kernel call, no fork, no spawn.
    /// It pins *which* Landlock ABI v1 rights `apply_landlock_scope` hands the workdir, so
    /// re-adding device-node creation becomes a test failure instead of a silent regression.
    ///
    /// It proves **nothing** about whether the kernel enforces that set — that is the manual
    /// acceptance procedure on `docs/content/reference/security-warnings.md`, not this test.
    /// It lives in this `#[cfg(target_os = "linux")]` module rather than the cross-platform
    /// `tests` module only because `landlock::AccessFs` is a Linux-only dependency.
    #[test]
    fn workdir_landlock_grant_withholds_device_and_socket_creation() {
        use landlock::{Access, AccessFs, ABI};

        let all_v1 = AccessFs::from_all(ABI::V1);
        // The `workdir_exec: true` set — the widest the workdir can ever be — so the three
        // unconditionally-withheld rights below are checked against the permissive case rather
        // than passing trivially because `Execute` happened to be absent too.
        let granted = linux_enforce::workdir_access_rights(true);

        for withheld in [AccessFs::MakeChar, AccessFs::MakeBlock, AccessFs::MakeSock] {
            assert!(
                all_v1.contains(withheld),
                "{withheld:?} is expected to be part of Landlock ABI v1 — if it is not, this \
                 test's premise (and the workdir grant's comment) needs revisiting"
            );
            assert!(
                !granted.contains(withheld),
                "the workdir grant must withhold {withheld:?}: with the full ABI v1 set declared \
                 by handle_access, withholding it here is what makes it denied rather than merely \
                 un-granted"
            );
        }

        assert!(
            granted.contains(AccessFs::MakeFifo),
            "MakeFifo stays granted — real build tooling creates named pipes in its working tree"
        );

        let expected = all_v1 & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::MakeSock);
        assert_eq!(
            granted, expected,
            "the workdir_exec grant must be exactly Landlock ABI v1 minus \
             MakeChar/MakeBlock/MakeSock — nothing else may be added or dropped without updating \
             this test and the constant's comment together"
        );
    }

    /// The bit this slice made conditional, pinned in both directions. A *content* check on the
    /// right set only: it proves nothing about whether a kernel refuses an exec, which is the
    /// manual procedure in
    /// `docs/content/reference/workdir-exec-landlock-manual-verification.md`. What it does buy is
    /// that flipping the default back — the exact regression that would silently reopen the
    /// rename-to-an-allowlisted-basename bypass — cannot happen without this test failing.
    #[test]
    fn workdir_execute_right_is_granted_only_when_workdir_exec_is_declared() {
        use landlock::AccessFs;

        assert!(
            !linux_enforce::workdir_access_rights(false).contains(AccessFs::Execute),
            "the default must withhold Execute from the workdir — that withholding is the whole \
             of what enforces capabilities.shell.allow now that the exec supervisor is gone"
        );
        assert!(
            linux_enforce::workdir_access_rights(true).contains(AccessFs::Execute),
            "capabilities.filesystem.workdir_exec: true must actually grant Execute, or the \
             compile-and-run workflows it exists for silently stop working"
        );

        // Nothing else moves with the flag: the two sets differ by exactly `Execute`.
        assert_eq!(
            linux_enforce::workdir_access_rights(true) & !AccessFs::Execute,
            linux_enforce::workdir_access_rights(false),
            "workdir_exec must be a one-bit axis — no other right may ride along with it"
        );
    }

    #[test]
    fn prepare_enforcement_is_noop_for_environment_only_tier_even_on_linux() {
        let mut command = std::process::Command::new("true");
        let enforcement = ShellEnforcement::environment_only();
        let supervisor = prepare_enforcement(&mut command, &enforcement, Path::new("/tmp"))
            .expect("environment_only must never fail");
        supervisor.join_best_effort();
    }

    // There is deliberately no `kernel_tier_denies_exec_outside_shell_allowlist` here any more.
    // It used to run `bash -c 'id'` with only `bash` allowlisted and assert a nonzero exit, which
    // the exec-notify supervisor produced on *every* Linux tier. Exec is a Landlock right now, so
    // the same claim only holds on `KernelFull`/`KernelSealed` — and this repo's CI has never
    // resolved to either, so the test would have run the weaker path and passed while proving
    // nothing. Per this card's roadmap the security property is not asserted in CI at all; it is
    // verified by hand, on real Landlock-capable hardware, following
    // `docs/content/reference/workdir-exec-landlock-manual-verification.md`. The two tests below
    // remain because they assert the *opposite* direction — that allowlisted binaries still run —
    // which a green run does legitimately evidence.

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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
            ..host_bounding_base()
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
            ..host_bounding_base()
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

        // A real listener on the *host*, so the destination port is genuinely open. The failure
        // below is therefore about reachability, not about a closed port: the subprocess runs in
        // its own network namespace, where `127.0.0.1` is that namespace's own loopback and this
        // listener does not exist at all.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement {
            tier,
            // Empty allowlist: nothing is reachable, and the proxy would refuse it even if the
            // capsule found its way to the endpoint.
            network_allow_rules: Vec::new(),
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
            ..host_bounding_base()
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
            "a direct connect() to a destination outside capabilities.network.allow must fail \
             even though the port is really open on the host — the subprocess is in its own \
             network namespace with no route to it"
        );
    }

    /// The no-regression counterpart of the test above: enforcement must add denials for
    /// out-of-policy destinations without breaking permitted ones.
    ///
    /// Same claim as before this slice, reached through the mechanism that replaced the seccomp
    /// connect/sendto supervisor. The allowlist now has to be declared rather than only
    /// pre-resolved, because it
    /// decides two things instead of one: which addresses are permitted *and* which ports the
    /// namespace binds a listener on. A port no allow entry implies has nothing listening and is
    /// refused by the namespace itself.
    ///
    /// Deliberately *not* a test of the security property — the card is explicit that a green
    /// suite proves nothing about the kernel-level claim, and the real check is the hand-run
    /// procedure in
    /// `docs/content/reference/network-namespace-egress-proxy-manual-verification.md`. This
    /// asserts only that the permitted path still functions.
    #[test]
    fn kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy() {
        let tier = detect_enforcement_tier();
        if tier == EnforcementTier::EnvironmentOnly {
            eprintln!(
                "skipping kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy: \
                 resolved to EnvironmentOnly on a Linux host — degrading gracefully"
            );
            return;
        }

        // A real listener on the host, answering one connection, so a success below means bytes
        // really crossed the namespace boundary through the proxy rather than merely connecting.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::Write;
                let _ = stream.write_all(b"pong\n");
            }
        });

        let temp = tempfile::tempdir().unwrap();
        // Naming the port is required, not incidental: it is what puts a listener on that port
        // inside the namespace. See `egress_proxy::egress_listen_ports`.
        let allow = vec![format!("http://127.0.0.1:{port}")];
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            network_allow: allow.clone(),
            ..CapabilityPolicy::default()
        };
        let rules = crate::network_policy::parse_network_allow_rules(&allow).unwrap();
        let enforcement = ShellEnforcement {
            tier,
            egress_tcp_ports: crate::egress_proxy::egress_listen_ports(&rules),
            network_allow_rules: rules,
            network_allow_ips: resolve_network_allowlist_ips(&allow).unwrap(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
            ..host_bounding_base()
        };

        // Builtins only — `/dev/tcp` and `read` are bash itself — so no second binary is exec'd
        // and this measures the network path alone.
        let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && read -r reply <&3 && echo \"$reply\"");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();
        let _ = server.join();

        assert_eq!(
            result.exit_code, 0,
            "a destination inside capabilities.network.allow must stay reachable — enforcement \
             adds denials for out-of-policy destinations, it does not degrade permitted ones \
             (stderr: {})",
            result.stderr
        );
        assert!(
            result.stdout.contains("pong"),
            "the host listener's bytes must come back through the proxy unmodified \
             (stdout: {:?}, stderr: {})",
            result.stdout,
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
            ..host_bounding_base()
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

    // NOTE: the two tests below exercise the derived-Landlock-grant fix for this slice. A green
    // run on THIS repo's CI/dev machine is NOT evidence the fix works: per this card's roadmap,
    // CI has never actually resolved to `KernelFull` (it silently runs the `KernelSeccompOnly`
    // path instead), and the dev machine is macOS. Both tests print an unmistakable skip line and
    // return when the host is not `KernelFull`. Real acceptance is a manual run on a real
    // Landlock-capable Linux host, done by the team after this card lands.

    #[test]
    fn kernel_full_runs_nontrivial_shell_allowlist_but_a_pass_here_does_not_prove_the_landlock_fix()
    {
        let tier = detect_enforcement_tier();
        if tier != EnforcementTier::KernelFull {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_runs_nontrivial_shell_allowlist_... requires \
                 EnforcementTier::KernelFull (Landlock ABI, kernel 5.13+); detected {tier:?}. \
                 This run does NOT execute the derived-grant Landlock code path at all, so a \
                 green result here is not evidence the workdir-scope exec bug is fixed — only a \
                 real KernelFull Linux host can prove that."
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        // A non-trivial allowlist: an interpreter (`bash`) plus a second, genuinely nested,
        // dynamically-linked binary (`cat`). Both are guaranteed present on the target dev/CI
        // Linux hosts and both live outside the workdir, so this only succeeds if the derived
        // Landlock grants cover each binary, its ELF interpreter, and its shared libraries.
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "cat".to_string()],
            ..CapabilityPolicy::default()
        };
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            ..host_bounding_base()
        };

        // `echo` is a bash builtin; piping it into `cat` forces bash to fork+exec `cat`, a real
        // nested, dynamically-linked exec — the case the whole fix exists for.
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", "echo landlock-grant-ok | cat"],
            &[],
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok, not Err");

        assert_eq!(
            result.exit_code, 0,
            "bash + cat are both allowlisted and dynamically linked; the derived Landlock grants \
             must let each exec and dynamic-link outside the workdir (stderr: {})",
            result.stderr
        );
        assert!(
            result.stdout.contains("landlock-grant-ok"),
            "the nested `cat` must actually run and pass its stdin through (stdout: {}, stderr: {})",
            result.stdout,
            result.stderr
        );
    }

    #[test]
    fn kernel_full_denies_write_outside_workdir_but_a_pass_here_does_not_prove_the_landlock_fix() {
        let tier = detect_enforcement_tier();
        if tier != EnforcementTier::KernelFull {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_denies_write_outside_workdir_... requires \
                 EnforcementTier::KernelFull (Landlock ABI, kernel 5.13+); detected {tier:?}. \
                 This run does NOT execute the Landlock code path, so a green result here is not \
                 evidence writes outside the workdir are actually denied — only a real KernelFull \
                 Linux host can prove that."
            );
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        // A target OUTSIDE the workdir (a separate temp dir). Its parent exists, so the only
        // reason a write can fail is the Landlock scope — not a missing directory.
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("should-not-be-written.txt");

        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            ..host_bounding_base()
        };

        // `echo >file` uses only a bash builtin + an open-for-write of an outside path — the
        // write itself is what Landlock must refuse. The derived read+execute grants never widen
        // to write, so nothing outside the workdir (grant paths included) is writable.
        let script = format!("echo pwned > '{}'", target.display());
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok with a nonzero exit code, not Err");

        assert!(
            !target.exists(),
            "a write to a path outside the Landlock-scoped workdir must be denied under KernelFull"
        );
        assert_ne!(
            result.exit_code, 0,
            "bash's redirection to an outside path must fail (Landlock denies the open-for-write)"
        );
    }

    /// Builds a KernelFull enforcement for `policy`, combining the derived `DT_NEEDED`-closure
    /// file grants with the policy's `interpreter_runtime` directory grants — exactly what
    /// `ShellEnforcement::resolve` does, spelled out so the test controls the tier explicitly.
    fn kernel_full_enforcement(tier: EnforcementTier, policy: &CapabilityPolicy) -> ShellEnforcement {
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let mut landlock_grants =
            LandlockGrant::non_listable_files(resolve_landlock_grants(&exec_allow_paths));
        landlock_grants
            .extend(resolve_interpreter_runtime_grants(&policy.shell_interpreter_runtime));
        ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants,
            ..host_bounding_base()
        }
    }

    // NOTE (as above): a green run on THIS repo's CI/dev machine proves nothing — CI has never
    // resolved to `KernelFull` and the dev machine is macOS. Both tests print an unmistakable skip
    // line and return when the host is not `KernelFull`. Real acceptance is a manual run on a real
    // Landlock-capable Linux host.

    #[test]
    fn kernel_full_interpreter_runtime_list_dir_false_opens_file_but_denies_listing() {
        let tier = detect_enforcement_tier();
        if tier != EnforcementTier::KernelFull {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_interpreter_runtime_list_dir_false_... requires \
                 EnforcementTier::KernelFull (Landlock ABI, kernel 5.13+); detected {tier:?}. \
                 This run does NOT execute the interpreter_runtime Landlock code path, so a green \
                 result here is not evidence list_dir:false behaves correctly — only a real \
                 KernelFull Linux host can prove that."
            );
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        // A "stdlib"-shaped directory OUTSIDE the workdir, holding one known file.
        let stdlib = tempfile::tempdir().unwrap();
        let module = stdlib.path().join("mod.py");
        std::fs::write(&module, "x = 1\n").unwrap();

        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "cat".to_string(), "ls".to_string()],
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "bash".to_string(),
                dirs: vec![murmur_artifact::InterpreterRuntimeDir {
                    path: stdlib.path().to_string_lossy().into_owned(),
                    list_dir: false,
                }],
            }],
            ..CapabilityPolicy::default()
        };
        let enforcement = kernel_full_enforcement(tier, &policy);

        // Opening the file by its exact known name must succeed — Landlock's read rights apply to
        // the subtree beneath the granted directory.
        let read = crate::shell::execute_shell(
            "bash",
            &["-c", &format!("cat '{}'", module.display())],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok");
        assert_eq!(
            read.exit_code, 0,
            "list_dir:false must still permit opening a known file inside the grant (stderr: {})",
            read.stderr
        );
        assert!(read.stdout.contains("x = 1"), "stdout: {}", read.stdout);

        // Listing the directory's own entries must fail — ReadDir was not granted.
        let list = crate::shell::execute_shell(
            "bash",
            &["-c", &format!("ls '{}'", stdlib.path().display())],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok with a nonzero exit code");
        assert_ne!(
            list.exit_code, 0,
            "list_dir:false must deny enumerating the granted directory's own entries"
        );
    }

    #[test]
    fn kernel_full_interpreter_runtime_list_dir_true_lists_dir_but_not_its_parent() {
        let tier = detect_enforcement_tier();
        if tier != EnforcementTier::KernelFull {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_interpreter_runtime_list_dir_true_... requires \
                 EnforcementTier::KernelFull (Landlock ABI, kernel 5.13+); detected {tier:?}. \
                 This run does NOT execute the interpreter_runtime Landlock code path, so a green \
                 result here is not evidence list_dir:true (and its non-widening to the parent) \
                 behaves correctly — only a real KernelFull Linux host can prove that."
            );
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        // parent/ (NOT granted) contains stdlib/ (granted list_dir:true) contains one file.
        let parent = tempfile::tempdir().unwrap();
        let stdlib = parent.path().join("stdlib");
        std::fs::create_dir(&stdlib).unwrap();
        std::fs::write(stdlib.join("mod.py"), "x = 1\n").unwrap();

        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "ls".to_string()],
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "bash".to_string(),
                dirs: vec![murmur_artifact::InterpreterRuntimeDir {
                    path: stdlib.to_string_lossy().into_owned(),
                    list_dir: true,
                }],
            }],
            ..CapabilityPolicy::default()
        };
        let enforcement = kernel_full_enforcement(tier, &policy);

        // Listing the granted directory must succeed.
        let list = crate::shell::execute_shell(
            "bash",
            &["-c", &format!("ls '{}'", stdlib.display())],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok");
        assert_eq!(
            list.exit_code, 0,
            "list_dir:true must permit enumerating the granted directory (stderr: {})",
            list.stderr
        );
        assert!(list.stdout.contains("mod.py"), "stdout: {}", list.stdout);

        // Listing the PARENT (never named in any grant) must fail — naming one subdirectory does
        // not widen enumerability to its ancestors. This is the `/usr/lib` ceiling case.
        let list_parent = crate::shell::execute_shell(
            "bash",
            &["-c", &format!("ls '{}'", parent.path().display())],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok with a nonzero exit code");
        assert_ne!(
            list_parent.exit_code, 0,
            "a grant on stdlib/ must not make its (ungranted) parent enumerable"
        );
    }

    /// The `executable` axis, measured against the real kernel rather than reasoned about.
    ///
    /// This is the property the `sealed` runtime-tree grant depends on: a whole tree can be made
    /// readable and enumerable without becoming a place to run programs from. It is asserted here
    /// on the plain Landlock path (`KernelFull`) because the access shape is tier-independent —
    /// `apply_landlock_scope` builds the same rule either way — and because a `KernelSealed` host
    /// would have to be driven through a composed root, which no unit test can reach.
    ///
    /// Runs on any host with a usable Landlock ABI, `KernelSealed` ones included: the tier is
    /// pinned in the enforcement literal, so only the ABI has to be real. Skips loudly elsewhere.
    #[test]
    fn kernel_full_non_executable_grant_lists_its_tree_but_refuses_to_run_from_it() {
        let host_tier = detect_enforcement_tier();
        if !matches!(
            host_tier,
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed
        ) {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_non_executable_grant_... needs a usable Landlock ABI (kernel 5.13+); \
                 detected {host_tier:?}. This run does NOT install a Landlock domain, so a green \
                 result is not evidence that withholding Execute denies execve."
            );
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        // A runtime-tree stand-in: one readable file plus one runnable binary, outside the workdir.
        let tree = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join("entry.txt"), "x\n").unwrap();
        let runme = tree.path().join("runme");
        std::fs::copy("/bin/echo", &runme).unwrap();

        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "ls".to_string()],
            ..CapabilityPolicy::default()
        };
        let with_tree_grant = |executable: bool| {
            let mut enforcement = kernel_full_enforcement(EnforcementTier::KernelFull, &policy);
            enforcement.landlock_grants.push(LandlockGrant {
                path: tree.path().to_path_buf(),
                list_dir: true,
                executable,
            });
            enforcement
        };
        let run = |enforcement: &ShellEnforcement, command: String| {
            crate::shell::execute_shell(
                "bash",
                &["-c", &command],
                &[],
                workdir.path(),
                &policy,
                enforcement,
            )
            .expect("execute_shell should return Ok even when the command itself fails")
        };

        let denied = with_tree_grant(false);
        let list = run(&denied, format!("ls '{}'", tree.path().display()));
        assert_eq!(
            list.exit_code, 0,
            "a non-executable grant must still be enumerable — that is its entire purpose \
             (stderr: {})",
            list.stderr
        );
        assert!(list.stdout.contains("entry.txt"), "stdout: {}", list.stdout);

        let exec_denied = run(&denied, format!("'{}' ran", runme.display()));
        assert_ne!(
            exec_denied.exit_code, 0,
            "withholding Execute must deny execve inside the granted tree; a program ran instead \
             (stdout: {})",
            exec_denied.stdout
        );
        assert!(
            !exec_denied.stdout.contains("ran"),
            "the binary produced output, so it executed: {}",
            exec_denied.stdout
        );

        // The control: the identical grant with `executable: true` does run it. Without this the
        // assertion above could be passing for an unrelated reason (a bad path, a missing library).
        let allowed = with_tree_grant(true);
        let exec_allowed = run(&allowed, format!("'{}' ran", runme.display()));
        assert_eq!(
            exec_allowed.exit_code, 0,
            "the same tree granted Execute must run (stderr: {})",
            exec_allowed.stderr
        );
        assert!(
            exec_allowed.stdout.contains("ran"),
            "stdout: {}",
            exec_allowed.stdout
        );
    }

    // ---- slice ebcc5f51: every distinct pre_exec setup failure is legible + fail-closed ----
    //
    // These force each of the three distinct sandbox setup failures and assert each produces its
    // OWN legible message (not the undifferentiated bare EINVAL), and that the target binary never
    // executes. One failure is real (an unresolvable workdir, now caught before fork()); two use
    // the child-side (`pre_exec`) test seams below. All are safe on any Linux host: the workdir
    // and no_new_privs failures happen before any real kernel enforcement, and the Landlock seam
    // short-circuits before any real Landlock syscall.

    struct ForceNoNewPrivsFailureGuard;

    impl ForceNoNewPrivsFailureGuard {
        fn new() -> Self {
            FORCE_NO_NEW_PRIVS_FAILURE.with(|flag| flag.set(true));
            Self
        }
    }

    impl Drop for ForceNoNewPrivsFailureGuard {
        fn drop(&mut self) {
            FORCE_NO_NEW_PRIVS_FAILURE.with(|flag| flag.set(false));
        }
    }

    struct ForceLandlockFailureGuard;

    impl ForceLandlockFailureGuard {
        fn new() -> Self {
            FORCE_LANDLOCK_FAILURE.with(|flag| flag.set(true));
            Self
        }
    }

    impl Drop for ForceLandlockFailureGuard {
        fn drop(&mut self) {
            FORCE_LANDLOCK_FAILURE.with(|flag| flag.set(false));
        }
    }

    /// A `KernelFull` enforcement over an empty Landlock grant set. The forced-failure tests do not
    /// depend on any specific grant — they force the failure directly — so this exercises the
    /// child-side `pre_exec` steps on any Linux host regardless of its real Landlock support.
    fn kernel_full_empty_grants() -> ShellEnforcement {
        ShellEnforcement {
            tier: EnforcementTier::KernelFull,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: false,
            landlock_grants: Vec::new(),
            ..host_bounding_base()
        }
    }

    /// Scenario 1 (real): `prepare_enforcement` with a `KernelFull` tier and a workdir that does
    /// not exist. Because this slice opens the workdir's Landlock fd in the PARENT, this fails
    /// synchronously before fork() — `.spawn()` is never called. Returns the error string.
    fn workdir_resolution_error() -> String {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist-workdir");
        let marker = temp.path().join("never-spawned.marker");
        let mut command = std::process::Command::new("bash");
        command
            .arg("-c")
            .arg(format!("echo ran > '{}'", marker.display()));

        let error = prepare_enforcement(&mut command, &kernel_full_empty_grants(), &missing)
            .expect_err("an unresolvable workdir must fail before fork()");
        assert!(
            !marker.exists(),
            "prepare_enforcement must never spawn a subprocess: {error}"
        );
        error
    }

    /// Runs `execute_shell` for `bash -c 'echo ran > marker'` under `KernelFull` with `arm`'s
    /// child-side failure active, asserts the target script never ran (marker absent), and returns
    /// the resulting error string. The RAII guard `arm` returns stays alive across `.spawn()`.
    fn child_setup_failure_error<G>(arm: impl FnOnce() -> G) -> String {
        child_setup_failure_typed(&kernel_full_empty_grants(), arm).to_string()
    }

    /// [`child_setup_failure_error`] without the lossy `.to_string()`, and over a caller-chosen
    /// enforcement. Tests that care which *kind* of failure happened — not merely what it reads
    /// like — use this: the whole point of the sealed composed-root failure is that it keeps its
    /// identity out to the CLI instead of arriving as one more error string.
    fn child_setup_failure_typed<G>(
        enforcement: &ShellEnforcement,
        arm: impl FnOnce() -> G,
    ) -> crate::shell::ShellExecError {
        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let marker = temp.path().join("spawned-anyway.marker");
        let script = format!("echo ran > '{}'", marker.display());

        let _guard = arm();
        let error = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            temp.path(),
            &policy,
            enforcement,
        )
        .expect_err("a forced child-side setup failure must make execute_shell return Err");
        assert!(
            !marker.exists(),
            "no subprocess may run when pre_exec setup fails: {error}"
        );
        error
    }

    #[test]
    fn pre_exec_workdir_resolution_failure_is_caught_before_fork() {
        let error = workdir_resolution_error();
        assert!(
            error.contains("workdir") && error.contains("does-not-exist-workdir"),
            "the error must name the workdir path that failed to resolve: {error}"
        );
        assert!(
            !error.contains("os error 22"),
            "must not collapse to the bare EINVAL string: {error}"
        );
    }

    #[test]
    fn pre_exec_no_new_privs_failure_is_distinct_and_fails_closed() {
        let error = child_setup_failure_error(ForceNoNewPrivsFailureGuard::new);
        assert!(
            error.contains("no_new_privs"),
            "the error must name the no_new_privs step specifically: {error}"
        );
        assert!(
            !error.contains("os error 22"),
            "must not collapse to the bare EINVAL string: {error}"
        );
    }

    #[test]
    fn pre_exec_landlock_failure_is_distinct_and_fails_closed() {
        let error = child_setup_failure_error(ForceLandlockFailureGuard::new);
        assert!(
            error.contains("landlock"),
            "the error must name the Landlock construction step specifically: {error}"
        );
        assert!(
            !error.contains("os error 22"),
            "must not collapse to the bare EINVAL string: {error}"
        );
    }

    #[test]
    fn pre_exec_setup_failures_produce_pairwise_distinct_messages() {
        let workdir_msg = workdir_resolution_error();
        let no_new_privs_msg = child_setup_failure_error(ForceNoNewPrivsFailureGuard::new);
        let landlock_msg = child_setup_failure_error(ForceLandlockFailureGuard::new);

        let messages = [
            ("workdir", &workdir_msg),
            ("no_new_privs", &no_new_privs_msg),
            ("landlock", &landlock_msg),
        ];
        for (i, (left_name, left)) in messages.iter().enumerate() {
            for (right_name, right) in &messages[i + 1..] {
                assert_ne!(left, right, "{left_name} vs {right_name} must differ");
            }
        }

        for message in [&workdir_msg, &no_new_privs_msg, &landlock_msg] {
            assert!(!message.is_empty(), "a distinct message must not be empty");
            assert!(
                !message.contains("os error 22"),
                "no failure may read as a bare EINVAL: {message}"
            );
        }
    }

    /// The first half of the composed-root failure path this slice promises: a `pre_exec`
    /// composed-root failure comes back out of `execute_shell` as the *typed*
    /// `SealedRootConstructionFailed`, carrying the child's diagnostic, and reports itself as
    /// session-fatal. `runtime::tests` covers the second half (what the dispatch layer then
    /// does with it) and `murmur-cli`'s error test covers the third (`E-RUN-014`).
    #[test]
    fn a_composed_root_failure_is_typed_and_session_fatal_not_just_another_message() {
        let error =
            child_setup_failure_typed(&sealed_test_enforcement(), ForceSealedRootFailureGuard::new);

        assert!(
            matches!(
                error,
                crate::shell::ShellExecError::SealedRootConstructionFailed { .. }
            ),
            "a sealed-root diagnostic must survive as its own variant, not collapse into \
             ShellExecError::Failed: {error}"
        );
        let fatal = error
            .session_fatal()
            .expect("a composed-root failure must end the session, not just the tool call");
        assert!(
            matches!(
                fatal,
                crate::errors::RuntimeError::SealedRootConstructionFailed { .. }
            ),
            "must map to the RuntimeError the CLI renders as E-RUN-014: {fatal}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("composed root") && rendered.contains("unshare"),
            "the message must name both the mechanism and the step that failed: {rendered}"
        );
        assert!(
            !rendered.contains("os error 22"),
            "must not collapse to the bare EINVAL string: {rendered}"
        );
    }

    /// The contrast that makes the assertion above mean something: an equally fatal-looking
    /// `pre_exec` failure that is *not* the composed root stays an ordinary failure, so nothing
    /// in this path ends a session for a Landlock or `no_new_privs` problem.
    #[test]
    fn an_ordinary_pre_exec_failure_is_not_session_fatal() {
        let landlock =
            child_setup_failure_typed(&kernel_full_empty_grants(), ForceLandlockFailureGuard::new);
        assert!(
            landlock.session_fatal().is_none(),
            "a Landlock setup failure is the tool call's problem, not the session's: {landlock}"
        );

        let no_new_privs = child_setup_failure_typed(
            &kernel_full_empty_grants(),
            ForceNoNewPrivsFailureGuard::new,
        );
        assert!(
            no_new_privs.session_fatal().is_none(),
            "a no_new_privs failure is the tool call's problem, not the session's: {no_new_privs}"
        );
    }

}
