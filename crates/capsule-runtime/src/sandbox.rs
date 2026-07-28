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
//!   - **classic seccomp-bpf argument matching** (`SECCOMP_RET_ERRNO`) on `socket(2)`'s `domain`,
//!     to refuse whole address families outright — see `denied_socket_domains`. Unlike a
//!     destination `sockaddr`, `domain` is an integer in a register, so the kernel's own BPF can
//!     compare it with no userspace round-trip and no notification at all. This is what stops a
//!     capsule reaching `/var/run/docker.sock` (host root) over `AF_UNIX`, which the notify path
//!     above never covered: its `sockaddr` parser returns `None` for non-IP families and the
//!     supervisor treats that as allow. `AF_NETLINK`/`AF_PACKET` are denied unconditionally;
//!     `AF_UNIX` is denied unless `capabilities.network.unix_sockets` says otherwise.
//!   - **a default-deny syscall allowlist** modelled on the OCI/Docker default seccomp profile —
//!     see `SECCOMP_SYSCALL_ALLOWLIST`. The filter's default action is `SECCOMP_RET_ERRNO(EPERM)`,
//!     so a syscall named by neither that array nor one of the rules above is refused outright,
//!     with no argument inspection at all. This is what puts `io_uring_*` (historically an
//!     LSM-path-hook bypass, and so a candidate route around Landlock itself), `bpf`,
//!     `userfaultfd`, `perf_event_open`, `ptrace` and the rest of `SECCOMP_MUST_STAY_DENIED` out
//!     of a capsule's reach. The filter also sets `SCMP_FLTATR_CTL_LOG`, so each denial reaches
//!     the kernel audit trail with a syscall number, pid and comm.
//!   - **Landlock LSM** to scope filesystem access to the capsule's `workdir`. The workdir's own
//!     grant deliberately withholds `MakeChar`/`MakeBlock`/`MakeSock` — see
//!     `linux_enforce::WORKDIR_ACCESS_RIGHTS`.
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
//! ## Three-tier model
//!
//! - `KernelFull` (Linux, Landlock ABI available — kernel 5.13+): seccomp syscall allowlist +
//!   exec/network allowlisting + socket-domain denial + Landlock filesystem scoping.
//! - `KernelSeccompOnly` (Linux, Landlock unavailable — kernel <5.13): seccomp syscall allowlist +
//!   exec/network allowlisting + socket-domain denial only. Filesystem scope stays convention-only
//!   (`current_dir`) — a documented gap, not a bug. The syscall allowlist and the socket-domain
//!   denial are identical on both tiers: they are seccomp rules, so they do not depend on Landlock
//!   in any way.
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
use murmur_artifact::InterpreterRuntimeGrant;

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

/// Resolves the program name a shell tool invoked to the canonical filesystem path of the
/// binary that `execvp` would actually run, for reporting purposes: `shell-event.binary`
/// on the hook side and the `binary` key of `trace.jsonl`'s `shell` record.
///
/// Unlike [`resolve_exec_allowlist`] — which collects *every* `PATH` match because the
/// kernel exec supervisor needs the full identity set — this returns the **first** match,
/// which is the one `execvp` picks and therefore the one that ran. Resolution rules are
/// otherwise identical (same `PATH` source, same [`is_executable_file`] test, same
/// `canonicalize`), so a reported path is always a member of the allowlisted identity set
/// the supervisor matches against.
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

/// One resolved Landlock filesystem grant outside the workdir: a canonical path plus whether the
/// directory's own entries may be enumerated (`ReadDir`).
///
/// `list_dir` carries the whole difference between the two grant kinds this runtime issues:
///
///   - `false` → `Execute + ReadFile`. Enough for the dynamic loader to open, read, and
///     map-execute a file; and (Landlock's read rights apply to the whole subtree beneath a
///     granted directory) enough to open a file *inside* a granted directory by its exact name.
///     But the directory's own listing (`getdents64`) is denied. This is what every derived
///     `DT_NEEDED`-closure grant gets — they are individual files, where `ReadDir` was always a
///     no-op anyway (Landlock's `ReadDir` only has meaning on a directory inode).
///   - `true` → `Execute + ReadFile + ReadDir`. Adds enumerability, which a path-based
///     interpreter's import machinery needs on each `sys.path` entry (CPython's `FileFinder`
///     `listdir`-caches each one). Only ever set by an author writing `list_dir: true` next to a
///     specific `interpreter_runtime` directory — never inferred, and never applied to an
///     ancestor or sibling of a granted directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandlockGrant {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) path: PathBuf,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) list_dir: bool,
}

impl LandlockGrant {
    /// Wraps each derived `DT_NEEDED`-closure file path as a non-listable grant. A regular file
    /// has no meaningful `ReadDir`, so `list_dir: false` is both correct and a pure simplification
    /// of b3220cb5's old uniform `Execute|ReadFile|ReadDir` — it never changes *which* files are
    /// granted.
    fn non_listable_files(paths: Vec<PathBuf>) -> Vec<LandlockGrant> {
        paths
            .into_iter()
            .map(|path| LandlockGrant { path, list_dir: false })
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
            })
        })
        .collect()
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
/// symlink in that window can still race it.
///
/// Neither exec nor symlinks are what bounds that race — it is the whole class of decision
/// this supervisor makes by dereferencing a pointer into another task's memory. The
/// `connect`/`sendto` path reads a `sockaddr` out of the notifying task via the same
/// `/proc/<pid>/mem` read (`linux_enforce::read_sockaddr_ip_from_child`) and answers with the
/// same `CONTINUE`, so a second thread can overwrite that buffer between the supervisor's read
/// and the kernel's post-continue re-use of it exactly as it can retarget a symlink here.
/// Closing either fully requires fd-substitution (`SECCOMP_IOCTL_NOTIF_ADDFD`-style) rather
/// than continue semantics; the non-racing rename/copy bypass is what this closes. Audited,
/// with citations and a hand-runnable race probe, in
/// `docs/content/reference/seccomp-notify-toctou-audit.md`.
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

// `socket(2)`'s `domain` argument values, as Linux's ABI defines them. Spelled out as literals
// rather than `libc::AF_*` for the same reason `LINUX_AF_INET` below is: the filter these feed is
// always compiled *for a Linux child*, so the numbers must be Linux's regardless of what host the
// build runs on — and `denied_socket_domains` has to stay compilable and unit-testable on a macOS
// dev machine, where `libc::AF_NETLINK` and `libc::AF_PACKET` do not exist at all (the `libc`
// crate defines them only under `linux_like`). `libc::AF_UNIX` does exist on macOS, but taking one
// of the three from `libc` and two from literals would be worse than taking all three the same
// way. Values are stable kernel ABI: `include/linux/socket.h`.
const LINUX_AF_UNIX: i32 = 1;
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
/// pointer BPF cannot dereference, which is the entire reason `connect`/`sendto` need the
/// notify+supervisor machinery (see this module's header). Denying the domain at *creation* time
/// therefore needs no userspace round-trip, and is structurally immune to the TOCTOU class of
/// problem that reading a pointed-to argument out of another task's memory invites.
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
/// `AF_INET`/`AF_INET6` are never denied here: they stay governed exactly as before by the
/// `connect`/`sendto` notify path against `capabilities.network.allow`. This function only ever
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

/// `socket(2)` `domain` values for the two IP families, as `i32` to match
/// [`allowed_socket_domains`]'s rule-argument type. Derived from the `sockaddr`-parser constants
/// below rather than re-typed as fresh literals: `AF_INET`'s number is one ABI value, whether it
/// is being read out of a `sockaddr` or compared against `socket()`'s first argument.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET_DOMAIN: i32 = LINUX_AF_INET as i32;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_AF_INET6_DOMAIN: i32 = LINUX_AF_INET6 as i32;

/// The syscalls the child's seccomp filter permits outright, modelled on the OCI/Docker default
/// seccomp profile (reconciled against `containerd`'s `contrib/seccomp/seccomp_default.go`, which
/// is the maintained source of the same list Docker ships).
///
/// The filter's **default action is a deny** (`Errno(EPERM)`), so this array is the whole of what
/// a shell subprocess may call, minus two carve-outs handled by rules rather than by name here:
///
///   - `execve`/`execveat`/`connect`/`sendto` are `Notify` rules (argument-content decisions made
///     by the supervisor). They are deliberately **absent** from this array — adding them would
///     give them a second, plain `Allow` rule and take them away from the supervisor.
///   - `socket` is absent for the same structural reason: it carries argument-conditional rules
///     (see [`denied_socket_domains`] / [`allowed_socket_domains`]), and an unconditional `Allow`
///     rule would discard them.
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
    // `socket` is NOT here (argument-conditional rules — see this array's doc comment), and
    // neither are `connect`/`sendto` (notify rules).
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
///      otherwise have to be routed through the notify supervisor (out of scope for this slice).
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
    /// `capabilities.network.unix_sockets`, threaded through to the seccomp `socket()` rule.
    /// `false` (the default) means the forked child cannot create an `AF_UNIX` socket at all, so
    /// a local daemon socket — `/var/run/docker.sock` above all — is unreachable regardless of
    /// what any Landlock ABI does or does not mediate for pathname sockets. Consumed only by
    /// `linux_enforce::install_seccomp_filter` (via `denied_socket_domains`); resolved on every
    /// platform for parity but never read off Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) unix_sockets_allowed: bool,
    /// Canonical filesystem paths of the `shell.allow` binaries, resolved once at launch by
    /// `resolve_exec_allowlist` — the identity set the seccomp exec supervisor matches
    /// against (never the raw name strings; see `decide_exec_allowed`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) exec_allow_paths: Vec<PathBuf>,
    /// Narrow read+execute (never write) Landlock grants *outside* the workdir so the allowlisted
    /// binaries can actually exec, dynamically link, and (for a path-based interpreter) reach their
    /// stdlib. Two origins, combined here:
    ///
    ///   - the `DT_NEEDED`-closure files (`shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure), from `resolve_landlock_grants` — each wrapped `list_dir: false`
    ///     (they are individual files, where `ReadDir` was always a no-op);
    ///   - one grant per `capabilities.shell.interpreter_runtime` directory, from
    ///     `resolve_interpreter_runtime_grants` — each carrying exactly the `list_dir` its author
    ///     declared.
    ///
    /// Resolved once at launch (in the parent) and threaded into the forked child's `pre_exec`,
    /// where `apply_landlock_scope` turns each into a per-path `PathBeneath` rule with an access
    /// set that depends on `list_dir`. Only consulted on `KernelFull`; resolved on every platform
    /// for parity but never read off Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) landlock_grants: Vec<LandlockGrant>,
}

impl ShellEnforcement {
    /// Resolves tier + network allowlist + canonical exec allowlist once, at launch time.
    pub(crate) fn resolve(policy: &CapabilityPolicy) -> Result<Self, String> {
        let tier = detect_enforcement_tier();
        let network_allow_ips = resolve_network_allowlist_ips(&policy.network_allow)?;
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        // The b3220cb5 `DT_NEEDED`-closure files (individual files → non-listable) plus one grant
        // per author-declared `interpreter_runtime` directory (each with its own `list_dir`). A
        // directory not named in the manifest never receives a rule, regardless of what it holds.
        let mut landlock_grants =
            LandlockGrant::non_listable_files(resolve_landlock_grants(&exec_allow_paths));
        landlock_grants
            .extend(resolve_interpreter_runtime_grants(&policy.shell_interpreter_runtime));
        Ok(Self {
            tier,
            network_allow_ips,
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths,
            landlock_grants,
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
            // Denied, matching every other grant this constructor zeroes out. Inert on this tier
            // (no seccomp filter is installed at all), but it must not read as "allowed".
            unix_sockets_allowed: false,
            exec_allow_paths: Vec::new(),
            landlock_grants: Vec::new(),
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
/// dynamically link, and nothing outside the workdir is writable. Seccomp additionally refuses the
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
shared libraries — nothing writable, no directory granted wholesale). The workdir's own grant also \
withholds device-node and unix-socket creation; seccomp refuses socket(AF_UNIX) outright unless \
capabilities.network.unix_sockets is declared, and always refuses AF_NETLINK/AF_PACKET, so a \
capsule cannot reach a host daemon socket such as /var/run/docker.sock; the seccomp filter now \
defaults to deny and permits only a fixed syscall allowlist modelled on the OCI/Docker default \
profile, so io_uring, bpf, userfaultfd, perf_event_open, ptrace and similar are refused outright; \
and the forked shell child \
drops every Linux capability and sets no_new_privs before execve — but this mechanism has not yet \
been verified by the team on real Landlock-capable Linux hardware — treat shell-subprocess \
isolation as not-yet-confirmed and do not rely on it as a hardened boundary until it is.";

const SECCOMP_ONLY_WARNING: &str = "capabilities.shell.allow is non-empty and this Linux kernel \
lacks Landlock (kernel <5.13) — filesystem access outside the capsule workdir is not \
kernel-enforced at all, and the seccomp exec/network enforcement that would apply has not been \
verified on real Linux hardware. Seccomp also refuses socket(AF_UNIX) unless \
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
#[derive(Debug)]
pub(crate) enum SupervisorHandle {
    Noop,
    #[cfg(target_os = "linux")]
    Linux {
        done_rx: std::sync::mpsc::Receiver<()>,
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
    /// Best-effort join with a short bound so `execute_shell` never hangs waiting for the
    /// supervisor thread. In practice it returns immediately: the notify fd closes (which
    /// makes the thread return) once every process holding the seccomp filter — the child and
    /// all its descendants — has exited, which `wait_with_output` (called by `execute_shell`
    /// right before this) has already waited for.
    pub(crate) fn join_best_effort(self) {
        match self {
            SupervisorHandle::Noop => {}
            #[cfg(target_os = "linux")]
            SupervisorHandle::Linux { done_rx, .. } => {
                let _ = done_rx.recv_timeout(std::time::Duration::from_secs(5));
                // Deliberately not joining the underlying `JoinHandle`: dropping it without
                // joining leaves the thread detached (it keeps running to completion in the
                // background, reclaimed by the OS on exit), which is fine here since the
                // `done_rx` signal above already tells us the loop returned.
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

// Child-side (`pre_exec`) forced-failure seams, one per distinct setup step this slice can fail
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

    // Resolve/open every Landlock path in the PARENT, before fork(). Failing to open the workdir
    // is a normal synchronous error here — it names the path and returns before any subprocess is
    // spawned — rather than an `open()` that, buried inside `pre_exec`, would collapse to a bare
    // EINVAL at the `.spawn()` call site. Grant paths that fail to open are silently dropped
    // (shrink-not-fail), exactly as before; only the *where* of the open moved. Landlock rules
    // only apply on `KernelFull`, so `KernelSeccompOnly` opens nothing.
    let landlock_fds = if enforcement.tier == EnforcementTier::KernelFull {
        Some(linux_enforce::open_landlock_fds(
            workdir,
            &enforcement.landlock_grants,
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

    let tier = enforcement.tier;
    // Copied out before the `move` closure below takes ownership of everything it touches, the
    // same clone-before-move shape `tier` and the Landlock fds already use. Note the notify-fd
    // socketpair above is created in the *parent*, before fork, so the child's own `sendmsg` over
    // it is unaffected by a filter that denies `socket(AF_UNIX, ...)`.
    let unix_sockets_allowed = enforcement.unix_sockets_allowed;

    // SAFETY: this closure runs in the forked child, after fork() but before execve() — the
    // narrow pre_exec window where only async-signal-safe operations are permitted. It performs
    // the capability-dropping `prctl`/`capset` sequence (bounding-set drop, capset clear, and the
    // explicit `no_new_privs` `prctl`) plus one allocation-free `open`/`read`/`close` of
    // `/proc/sys/kernel/cap_last_cap`, libseccomp filter construction/load (kernel syscalls), one
    // `sendmsg` call to hand the notify fd to the parent over `child_sock`, and (KernelFull only)
    // Landlock ruleset construction/`restrict_self` against already-open fds (also kernel
    // syscalls) — no `open()`/`canonicalize()` beyond that one `cap_last_cap` read, no locks
    // beyond what those syscalls need. On any failure it writes the real error message to the
    // CLOEXEC diagnostic pipe (best-effort, bounded, raw `write` loop) before returning `Err`.
    // `child_sock`, `diag_write`, and the Landlock fds are moved in and close automatically when
    // the closure body finishes.
    unsafe {
        command.pre_exec(move || {
            let sock_fd = child_sock.as_raw_fd();
            match linux_enforce::child_install_enforcement(
                tier,
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

    // Start the receiver+supervisor thread now — BEFORE the caller calls `.spawn()`. See
    // `SupervisorHandle`'s doc comment for why this ordering (not "after spawn") is required.
    Ok(linux_enforce::start_supervisor(
        parent_sock,
        enforcement.exec_allow_paths.clone(),
        enforcement.network_allow_ips.clone(),
        diag_read,
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
    use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
    use std::os::unix::fs::FileExt;
    use std::path::{Path, PathBuf};

    use landlock::{make_bitflags, AccessFs, BitFlags};

    use super::{decide_exec_allowed, network_ip_allowed, parse_sockaddr_ip};
    use super::{EnforcementTier, LandlockGrant, SupervisorHandle};

    /// Longest diagnostic message written to (or read from) the child-failure pipe. A message
    /// naming the failed step is far shorter than this; the bound just keeps the best-effort
    /// `write`/`read` loops trivially terminating.
    const MAX_DIAG_LEN: usize = 1024;

    /// One Landlock grant path, opened (`O_PATH | O_CLOEXEC`) in the PARENT before fork(), paired
    /// with the `list_dir` bit that decides whether its rule also carries `ReadDir`. Only the
    /// already-open fd crosses into the child's `pre_exec` — never a path to re-open there.
    pub(super) struct OpenLandlockGrant {
        fd: OwnedFd,
        list_dir: bool,
    }

    /// All Landlock file descriptors resolved in the parent for one shell subprocess: the
    /// workdir's fd (scoped by [`WORKDIR_ACCESS_RIGHTS`]) plus each successfully-opened grant fd.
    /// Handed by reference into the child's `pre_exec`, where `apply_landlock_scope` builds rules
    /// against these fds without performing a single `open()`.
    pub(super) struct LandlockChildFds {
        workdir_fd: OwnedFd,
        grants: Vec<OpenLandlockGrant>,
    }

    /// The Landlock ABI v1 rights granted on the capsule **workdir's own** `PathBeneath` rule.
    ///
    /// Spelled out variant by variant rather than `AccessFs::from_all(ABI::V1)` for two reasons:
    /// `from_all` hides what is actually handed to the capsule, and three of ABI v1's thirteen
    /// rights are deliberately withheld.
    ///
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
    /// `from_all(ABI::V1)` set, the three withheld rights are not merely "not extra-granted" —
    /// they become **denied everywhere** in this domain, workdir included, once `restrict_self()`
    /// takes effect.
    pub(super) const WORKDIR_ACCESS_RIGHTS: BitFlags<AccessFs> = make_bitflags!(AccessFs::{
        Execute
            | WriteFile
            | ReadFile
            | ReadDir
            | RemoveDir
            | RemoveFile
            | MakeDir
            | MakeReg
            | MakeFifo
            | MakeSym
    });

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

    /// Runs inside the forked child, pre-exec: strips the child's Linux capabilities, installs
    /// the seccomp filter (exec/network notify rules plus the `socket()` domain denials keyed on
    /// `unix_sockets_allowed`), hands its notify fd to the parent over
    /// `child_sock_fd`, and (on `KernelFull`) applies the Landlock filesystem scope. Returning
    /// `Err` here aborts the exec (std's `Command` machinery propagates it back to the parent's
    /// `.spawn()` call as an `io::Error`) — the fail-closed path for setup failures that happen
    /// after fork.
    pub(super) fn child_install_enforcement(
        tier: EnforcementTier,
        landlock_fds: Option<&LandlockChildFds>,
        child_sock_fd: RawFd,
        unix_sockets_allowed: bool,
    ) -> io::Result<()> {
        // First, and on every kernel tier (not just `KernelFull`): Landlock and the Linux
        // capability model are independent, both-must-allow gates, so narrowing the Landlock
        // workdir grant does not by itself take `CAP_MKNOD` away from a root-uid capsule. This
        // must also run *before* `install_seccomp_filter`, because it is what sets
        // `no_new_privs` — which `seccomp(2)` requires once `CAP_SYS_ADMIN` is gone. Setting it
        // here (rather than depending on libseccomp's `SCMP_FLTATR_CTL_NNP` default) also means a
        // failure is its own distinct, fail-closed error path before any filter is installed.
        drop_all_capabilities()?;

        install_seccomp_filter(child_sock_fd, unix_sockets_allowed)?;

        if tier == EnforcementTier::KernelFull {
            if let Some(fds) = landlock_fds {
                apply_landlock_scope(fds).map_err(io::Error::other)?;
            }
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

    /// Builds and loads the child's seccomp filter, then hands its notify fd to the parent.
    ///
    /// Two independent mechanisms live in this one filter, and they are deliberately not the
    /// same mechanism:
    ///
    ///   - `execve`/`execveat`/`connect`/`sendto` get `Notify` rules, because deciding them needs
    ///     the *contents* of a pointed-to argument (a pathname, a `sockaddr`) that in-kernel BPF
    ///     cannot dereference — so the userspace supervisor (`classify_and_decide`) resolves each
    ///     one. Unchanged by the unix-socket work.
    ///   - `socket` gets classic, register-value `Errno` rules, one per denied domain. `domain` is
    ///     a plain integer argument, so the comparison compiles straight into the loaded BPF
    ///     program and is evaluated in-kernel at syscall time: no notification is raised, no
    ///     supervisor round-trip happens, and no memory of another task is read. That is what
    ///     makes it structurally immune to the pointer-read TOCTOU concerns that apply to the
    ///     notify rules above, and why it is a hard deny rather than a supervisor decision.
    ///
    /// Around both of those sits the filter's **default action**, which is a deny
    /// (`Errno(EPERM)`): a syscall named by neither [`super::SECCOMP_SYSCALL_ALLOWLIST`] nor a
    /// rule here is refused, without any argument being inspected. That is what makes
    /// `io_uring_setup`, `bpf`, `userfaultfd`, `perf_event_open`, `ptrace` and the rest of
    /// [`super::SECCOMP_MUST_STAY_DENIED`] unreachable — they are simply absent, not
    /// argument-matched.
    fn install_seccomp_filter(child_sock_fd: RawFd, unix_sockets_allowed: bool) -> io::Result<()> {
        // Default-deny. `EPERM` matches what the OCI/Docker default profile returns for a syscall
        // outside its allowlist, and is deliberately a *different* errno from the `EACCES` that
        // `classify_and_decide` and the `socket()` domain rules return: `EACCES` means "the
        // sandbox looked at this call's arguments and refused it", `EPERM` means "this syscall is
        // not part of the capsule's syscall surface at all". Keeping them distinct is what lets
        // an operator tell an unmediated-surface problem from a policy decision.
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
        // configuration this process does not control. `SECCOMP_ALLOWLIST_VERIFICATION.md` at the
        // workspace root carries the check and what to do when `errno` is absent from that list.
        let _ = filter.set_ctl_log(true);

        for name in ["execve", "execveat", "connect", "sendto"] {
            let syscall = libseccomp::ScmpSyscall::from_name(name).map_err(to_io_err)?;
            filter
                .add_rule(libseccomp::ScmpAction::Notify, syscall)
                .map_err(to_io_err)?;
        }

        // Deny the dangerous `socket(2)` domains at creation time. `EACCES` (not `EPERM`) matches
        // what `classify_and_decide`'s `Decision::Deny` already returns for a blocked TCP/UDP
        // destination, so a capsule sees one consistent errno for "the sandbox refused this
        // socket" whichever of the two mechanisms refused it.
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

    /// Scopes the shell subprocess tree's filesystem access with Landlock. Two kinds of rule:
    ///
    ///   - the workdir gets [`WORKDIR_ACCESS_RIGHTS`] — read/write/execute plus directory,
    ///     regular-file, FIFO and symlink creation, but **not** `MakeChar`/`MakeBlock`/`MakeSock`
    ///     (see that constant for why each of the three is withheld);
    ///   - each [`LandlockGrant`] (the `shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure, and any `interpreter_runtime` directory, all *outside* the
    ///     workdir) gets a **narrow read+execute** grant — never write. Whether that grant also
    ///     carries `ReadDir` (i.e. the directory's own entries are enumerable) is exactly the
    ///     grant's `list_dir`: the derived closure files are all `false` (a regular file has no
    ///     meaningful `ReadDir`), while an `interpreter_runtime` directory carries whatever its
    ///     author wrote. `ReadDir` is granted only on the specific inode a rule names — never on
    ///     an ancestor or sibling — so naming one subdirectory never makes `/usr/lib` (or any
    ///     parent) enumerable.
    ///
    /// Without the second kind of rule, `restrict_self()` denies `Execute`/`ReadFile` on every
    /// path outside the workdir, so every allowlisted binary's `execve` fails with EACCES before
    /// it runs (and even a binary placed *inside* the workdir can't be loaded, because its dynamic
    /// loader must still read `ld-linux`/libc outside it). The narrow grants fix that while keeping
    /// the security story honest: nothing outside the workdir is writable, so a write to any
    /// outside path — including one of these read+execute grant paths — is still denied.
    ///
    /// `handle_access` must declare the union of every access bit any rule uses, and it stays the
    /// **full** ABI v1 set deliberately: a bit declared there but granted by no rule is denied for
    /// the whole domain, which is exactly how the three rights missing from
    /// [`WORKDIR_ACCESS_RIGHTS`] become denied rather than merely un-granted. A grant path that
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
        // Adds enumerability (`getdents64`) — only for a grant whose author set `list_dir: true`.
        let read_execute_list = read_execute | AccessFs::ReadDir;

        // Every fd here was already opened in the parent (before fork). This function performs no
        // `open()`/`canonicalize()` — only the Landlock ruleset syscalls against already-open fds,
        // so a failure here can only be the kernel call itself, not an unrelated path resolution.
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(access_all)
            .map_err(|error| format!("landlock: handle_access failed: {error}"))?
            .create()
            .map_err(|error| format!("landlock: ruleset create failed: {error}"))?
            .add_rule(PathBeneath::new(fds.workdir_fd.as_fd(), WORKDIR_ACCESS_RIGHTS))
            .map_err(|error| format!("landlock: add_rule failed: {error}"))?;

        for grant in &fds.grants {
            // Grant paths that failed to open in the parent were already dropped (shrink-not-fail),
            // so every fd reaching here is valid. A rule that still fails to add is a genuine
            // ruleset-construction failure and propagates as `Err` (fail-closed).
            let access = if grant.list_dir {
                read_execute_list
            } else {
                read_execute
            };
            ruleset = ruleset
                .add_rule(PathBeneath::new(grant.fd.as_fd(), access))
                .map_err(|error| format!("landlock: add_rule for grant failed: {error}"))?;
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
    pub(super) fn open_landlock_fds(
        workdir: &Path,
        landlock_grants: &[LandlockGrant],
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
                }),
                Err(_) => continue,
            }
        }

        Ok(LandlockChildFds { workdir_fd, grants })
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
        diag_read: OwnedFd,
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

        SupervisorHandle::Linux { done_rx, diag_read }
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

            // Read memory, THEN validate the notification id is still valid, THEN respond. If
            // the id went stale (the target thread was resumed/killed via another path between
            // our read and now), skip it rather than responding to a dead notification.
            //
            // This is a *liveness* check, and explicitly NOT a TOCTOU-safe pattern.
            // `SECCOMP_IOCTL_NOTIF_ID_VALID` answers "does this notification still exist"; it
            // does not re-validate that the bytes `classify_and_decide` just read out of the
            // child are the bytes the kernel will act on when the syscall resumes. Because the
            // `Decision::Allow` arm below answers with `SECCOMP_USER_NOTIF_FLAG_CONTINUE`
            // (`new_continue`), the kernel re-dereferences the pathname/`sockaddr` pointer
            // itself after we respond — so a hostile *multithreaded* child that rewrites that
            // buffer in the window between our read and the syscall's resumption gets the
            // rewritten value enforced against a decision computed from the original one.
            // `seccomp_unotify(2)` names this directly and calls continue-based argument
            // inspection inherently racy. Audited in
            // `docs/content/reference/seccomp-notify-toctou-audit.md`; a fix is a full
            // replacement of this architecture (Landlock `Execute` for exec, a network
            // namespace + egress proxy for network), not a patch here.
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
                },
                LandlockGrant {
                    path: PathBuf::from("/usr/lib/python3.11/lib-dynload"),
                    list_dir: false,
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
                },
                LandlockGrant {
                    path: PathBuf::from("/opt/rb/also-nonexistent"),
                    list_dir: true,
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

    /// The rule only ever subtracts: IP sockets stay governed by the `connect`/`sendto` notify
    /// path against `capabilities.network.allow`, never by a domain-level deny.
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
    // syscalls are actually denied is verified only by the manual procedure in
    // `SECCOMP_ALLOWLIST_VERIFICATION.md`, on real bare-metal Linux hardware. What these tests
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

    /// The four notify syscalls are decided by `classify_and_decide` on their argument *contents*.
    /// A plain `Allow` rule for any of them would take that decision away from the supervisor
    /// entirely — exec identity matching and the network allowlist would both stop applying.
    #[test]
    fn allowlist_excludes_the_notify_syscalls() {
        for name in ["execve", "execveat", "connect", "sendto"] {
            assert!(
                !SECCOMP_SYSCALL_ALLOWLIST.contains(&name),
                "{name} is decided by the notify supervisor; a plain Allow rule would bypass it"
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
        let granted = linux_enforce::WORKDIR_ACCESS_RIGHTS;

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
            "the workdir grant must be exactly Landlock ABI v1 minus MakeChar/MakeBlock/MakeSock \
             — nothing else may be added or dropped without updating this test and the constant's \
             comment together"
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            unix_sockets_allowed: policy.unix_sockets_allowed,
            exec_allow_paths: resolve_exec_allowlist(&policy.shell_allow),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &resolve_exec_allowlist(&policy.shell_allow),
            )),
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
            exec_allow_paths,
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
            exec_allow_paths,
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
            exec_allow_paths,
            landlock_grants,
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
            exec_allow_paths: Vec::new(),
            landlock_grants: Vec::new(),
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
            &kernel_full_empty_grants(),
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

        assert_ne!(
            workdir_msg, no_new_privs_msg,
            "workdir vs no_new_privs must differ"
        );
        assert_ne!(
            workdir_msg, landlock_msg,
            "workdir vs landlock must differ"
        );
        assert_ne!(
            no_new_privs_msg, landlock_msg,
            "no_new_privs vs landlock must differ"
        );

        for message in [&workdir_msg, &no_new_privs_msg, &landlock_msg] {
            assert!(!message.is_empty(), "a distinct message must not be empty");
            assert!(
                !message.contains("os error 22"),
                "no failure may read as a bare EINVAL: {message}"
            );
        }
    }
}
