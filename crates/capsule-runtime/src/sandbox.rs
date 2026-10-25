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
//!     rename-to-an-allowlisted-basename bypass has no path left. `execve`/`execveat` are
//!     ordinary allowed syscalls, decided entirely by the Landlock domain they run in.
//!   - **classic seccomp-bpf argument matching** (`SECCOMP_RET_ERRNO`) on `socket(2)`'s `domain`,
//!     to refuse whole address families outright — see `denied_socket_domains`. Unlike a
//!     destination `sockaddr`, `domain` is an integer in a register, so the kernel's own BPF can
//!     compare it with no userspace round-trip and no notification at all. This is what stops a
//!     capsule reaching `/var/run/docker.sock` (host root) over `AF_UNIX`, which no other
//!     mechanism here covers: a network namespace does not mediate `AF_UNIX` at all, and no
//!     Landlock ABI the ruleset is built at mediates an abstract-namespace socket.
//!     `AF_NETLINK`/`AF_PACKET` are denied unconditionally; `AF_UNIX` is denied unless
//!     `capabilities.network.unix_sockets` says otherwise.
//!   - **a default-deny syscall allowlist** modelled on the OCI/Docker default seccomp profile —
//!     see `SECCOMP_SYSCALL_ALLOWLIST`. The filter's default action is `SECCOMP_RET_ERRNO(EPERM)`,
//!     so a syscall named by neither that array nor one of the rules above is refused outright,
//!     with no argument inspection at all. This is what puts `io_uring_*` (historically an
//!     LSM-path-hook bypass, and so a candidate route around Landlock itself), `bpf`,
//!     `userfaultfd`, `perf_event_open`, `ptrace` and the rest of `SECCOMP_MUST_STAY_DENIED` out
//!     of a capsule's reach. The filter also sets `SCMP_FLTATR_CTL_LOG`, so each denial reaches
//!     the kernel audit trail with a syscall number, pid and comm.
//!   - **Landlock LSM** to scope filesystem access to the capsule's `workdir`. The ruleset is
//!     built at the kernel's Landlock ABI, capped at `LANDLOCK_TARGET_ABI` (v3), so on a 5.19+
//!     kernel files move and hard-link between folders inside the workdir (`Refer`), and on 6.2+
//!     truncation outside it is refused (`Truncate`). The workdir's own
//!     grant deliberately withholds `MakeChar`/`MakeBlock`/`MakeSock`, and withholds `Execute`
//!     unless `capabilities.filesystem.workdir_exec` is declared — see
//!     `linux_enforce::WORKDIR_ACCESS_RIGHTS_NO_EXEC` and `linux_enforce::workdir_access_rights`.
//!     Outside the workdir, alongside the derived
//!     read+execute grants, a fixed three-device set is granted unconditionally — see
//!     `CAPSULE_DEVICE_GRANTS`: `/dev/null` read+write (with the session's temporary and home
//!     folders, the only writable paths outside the workdir), `/dev/zero` and `/dev/urandom`
//!     read-only, and no other device at all.
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
//! - `KernelSealed` (Linux, everything `KernelFull` has, plus a composed root the namespace probe
//!   really built inside the capsule's network namespace — see [`crate::namespace_probe`]): the
//!   subprocess tree additionally runs inside a private mount namespace pivoted onto a composed
//!   root, so paths outside it are *absent* rather than denied. Landlock and seccomp still install,
//!   inside the new root, as defence in depth — see [`crate::sealed`]. Only capsules that declare
//!   `capabilities.containment: sealed` get this; see `applied_tier`.
//! - `KernelFull` (Linux, Landlock ABI available — kernel 5.13+): seccomp syscall allowlist +
//!   socket-domain denial + Landlock filesystem scoping, which is also what makes
//!   `capabilities.shell.allow` enforceable (exec is a Landlock right, not a syscall filter).
//! - `KernelSeccompOnly` (Linux, Landlock unavailable — kernel <5.13): seccomp syscall allowlist +
//!   socket-domain denial only. Filesystem scope stays convention-only (`current_dir`) — a
//!   documented gap, not a bug — and, since the exec supervisor was retired, so does
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
//! file. Each reading is taken live, and a staged session takes exactly one for every consumer
//! of that session to read; see [`HostProbe`].
//!
//! ## Fail-closed invariant
//!
//! If kernel enforcement setup fails unexpectedly on a Linux host (not the expected
//! "Landlock unsupported, degrade to `KernelSeccompOnly`" signal, but something like seccomp
//! filter install failing, or the session's network namespace failing to be created before
//! spawn) — `prepare_enforcement` returns `Err` and `execute_shell` must not call `.spawn()` at
//! all.
//! There is no code path where a Linux host silently runs a shell subprocess with zero
//! enforcement because setup failed.

use std::net::IpAddr;
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
/// ruleset-restriction probe, and what [`crate::namespace_probe`] created, decide the tier. No
/// syscalls here — fully unit-testable on any OS.
///
/// `KernelSealed` requires the whole conjunction — Linux, a fully enforced Landlock ruleset, and a
/// sealed-root rehearsal that really built the composed root inside the capsule's network
/// namespace. Any missing element falls back to the tier that element still supports; nothing
/// here ever reports a mechanism the host did not demonstrate, and nothing here reads AppArmor.
pub(crate) fn tier_from_probe(
    is_linux: bool,
    landlock_fully_enforced: Option<bool>,
    namespaces: &crate::namespace_probe::NamespaceReading,
) -> EnforcementTier {
    if !is_linux {
        return EnforcementTier::EnvironmentOnly;
    }
    match landlock_fully_enforced {
        Some(true) if namespaces.sealed_root_created() => EnforcementTier::KernelSealed,
        Some(true) => EnforcementTier::KernelFull,
        Some(false) | None => EnforcementTier::KernelSeccompOnly,
    }
}

/// The tier a session applies on a host whose tier is `host_tier`: never stronger than the host
/// can back and never stronger than the capsule asked for.
///
/// A capsule declaring less than `sealed` on a sealed-capable host runs on
/// [`EnforcementTier::KernelFull`]: the composed root would otherwise delete host paths its
/// `interpreter_runtime` grants legitimately point at — a narrowing of `scoped`'s behaviour, not
/// added security. The declared floor is honoured, not merely met.
///
/// Called only by [`crate::containment::ContainmentDecision::decide`], which reads the session's
/// achieved class off the tier this returns, so the class a report names and the tier the
/// session's commands run on are one decision.
pub(crate) fn applied_tier(
    host_tier: EnforcementTier,
    declared: murmur_artifact::ContainmentClass,
) -> EnforcementTier {
    match host_tier {
        EnforcementTier::KernelSealed if declared < murmur_artifact::ContainmentClass::Sealed => {
            EnforcementTier::KernelFull
        }
        other => other,
    }
}

/// The host facts the tier decision is made from, taken together as one reading.
///
/// A staged session takes exactly one of these and every consumer of that session reads it — the
/// containment-floor refusal, the `ScopeReport` written to the trace, the `/tmp` mapping, the
/// `E-CAP-005` check at staging and at launch, and the tier [`ShellEnforcement::resolve`]
/// installs. Two readings of a host that changed between them would let the trace describe a
/// session that never ran under what it claims.
#[derive(Debug, Clone)]
pub struct HostProbe {
    landlock_fully_enforced: Option<bool>,
    /// The kernel's Landlock ABI, from `linux_enforce::kernel_landlock_abi`. `None` off Linux and
    /// on a Linux kernel without Landlock.
    landlock_kernel_abi: Option<u8>,
    namespaces: crate::namespace_probe::NamespaceReading,
}

/// Probes taken since this process started. See [`HostProbe::probes_taken`].
static PROBES_TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl HostProbe {
    /// Reads the host as it is right now: on Linux a real Landlock ruleset restriction attempt
    /// (`RulesetStatus::FullyEnforced` → `Some(true)`, `PartiallyEnforced`/`NotEnforced` or any
    /// construction error → `Some(false)`, no Landlock ABI → `None`) plus
    /// [`crate::namespace_probe`]'s three attempts: the session's network namespace, a command's
    /// private mount namespace inside it, and the composed root's nested namespace and pivot. Off
    /// Linux it probes nothing and claims nothing.
    ///
    /// Taken once per staged session, not once per process. That is affordable because both
    /// halves do their work in a forked child and leave the calling process exactly as they found
    /// it — `linux_enforce::probe_landlock_full_access` forks so the runtime is never placed
    /// into a Landlock domain by a capability check, and the namespace probe forks because
    /// `unshare(CLONE_NEWUSER)` is irreversible for the task that calls it. Neither is
    /// idempotency-sensitive, so a reading costs no more than the syscalls it makes: four
    /// `fork`/`waitpid` pairs and three small `/proc` reads.
    ///
    /// The window this leaves open is one session wide. Between `runtime::stage_session` and the
    /// last subprocess that session spawns the host is not re-read; a change inside that window is
    /// caught by the mechanism installation itself failing closed, not by a second probe. A
    /// process that stages repeatedly — `mur eval`, which stages once per dataset case — judges
    /// each session against the host as it was when that session was staged.
    pub fn probe() -> HostProbe {
        PROBES_TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        let probe = HostProbe {
            landlock_fully_enforced: linux_enforce::probe_landlock_full_access(),
            landlock_kernel_abi: linux_enforce::kernel_landlock_abi(),
            namespaces: crate::namespace_probe::probe_namespaces(),
        };
        #[cfg(not(target_os = "linux"))]
        let probe = HostProbe {
            landlock_fully_enforced: None,
            landlock_kernel_abi: None,
            namespaces: crate::namespace_probe::probe_namespaces(),
        };
        probe
    }

    /// A probe value assembled from known halves, without touching the host. The seam the
    /// fail-closed tests use to assert what an all-negative reading resolves to on any OS.
    #[cfg(test)]
    pub(crate) fn from_parts(
        landlock_fully_enforced: Option<bool>,
        namespaces: crate::namespace_probe::NamespaceReading,
    ) -> HostProbe {
        HostProbe {
            landlock_fully_enforced,
            landlock_kernel_abi: None,
            namespaces,
        }
    }

    /// This probe with its kernel Landlock ABI reading replaced. The seam the ABI tests use to
    /// resolve a session as a kernel reporting `abi` would, on a host that reports another.
    #[cfg(test)]
    pub(crate) fn with_landlock_kernel_abi(self, abi: Option<u8>) -> HostProbe {
        HostProbe {
            landlock_kernel_abi: abi,
            ..self
        }
    }

    /// The tier this reading supports. `pub(crate)` because [`EnforcementTier`] is.
    pub(crate) fn tier(&self) -> EnforcementTier {
        tier_from_probe(
            cfg!(target_os = "linux"),
            self.landlock_fully_enforced,
            &self.namespaces,
        )
    }

    /// What the namespace probe attempted, and the readings it took with the attempts.
    pub fn namespaces(&self) -> &crate::namespace_probe::NamespaceReading {
        &self.namespaces
    }

    /// The `E-CAP-005` failure this reading's network attempt is, or `None` when it created the
    /// namespace (and always off Linux).
    pub fn egress_namespace_failure(
        &self,
    ) -> Option<crate::network_namespace::EgressNamespaceFailure> {
        self.namespaces.egress_namespace_failure()
    }

    /// Whether this host can give a command a private mount namespace inside the session's user
    /// namespace: the probe's mount attempt created one.
    pub(crate) fn mount_namespace_usable(&self) -> bool {
        cfg!(target_os = "linux") && self.namespaces.mount_namespace_created()
    }

    /// Why this reading cannot back `sealed`, or `None` when nothing keeps it below. `None`
    /// exactly when [`Self::tier`] is `KernelSealed`.
    pub fn sealed_shortfall(&self) -> Option<crate::sealed::SealedShortfall> {
        crate::sealed::sealed_shortfall(
            cfg!(target_os = "linux"),
            self.landlock_fully_enforced == Some(true),
            &self.namespaces,
        )
    }

    /// Where this host's permission to create an unprivileged user namespace comes from, or
    /// `None` off Linux, where the question has no answer rather than a negative one.
    pub fn userns_grant(&self) -> Option<crate::sealed::UsernsGrant> {
        if cfg!(target_os = "linux") {
            Some(self.namespaces.userns_grant)
        } else {
            None
        }
    }

    /// The kernel's Landlock ABI version, or `None` when the kernel has no Landlock (off Linux
    /// always). The ABI the session's ruleset is built at is [`ruleset_landlock_abi`] of this.
    pub fn landlock_kernel_abi(&self) -> Option<u8> {
        self.landlock_kernel_abi
    }

    /// How many probes this process has taken, counting from zero at startup and never
    /// decreasing.
    ///
    /// The observability hook the freshness tests assert against — that staging twice advances it
    /// by two, and that one `stage_session` advances it by one — and nothing else reads it. No
    /// runtime decision depends on the count.
    pub fn probes_taken() -> u64 {
        PROBES_TAKEN.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The tier this host can back, from a reading taken here. The seam the kernel tests resolve the
/// host they run on through; a session reads its own [`HostProbe`].
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn detect_enforcement_tier() -> EnforcementTier {
    HostProbe::probe().tier()
}

/// Why this host cannot back `sealed`, or `None` when nothing keeps it below, from a reading
/// taken here.
///
/// Takes its own probe, as its neighbours do. What a session needs — that the refusal an operator
/// sees and the tier the runtime installs cannot disagree about the host — is upheld by
/// `runtime::stage_session` passing one [`HostProbe`] to both, not by this function.
pub fn detect_sealed_shortfall() -> Option<crate::sealed::SealedShortfall> {
    HostProbe::probe().sealed_shortfall()
}

/// Where this host's unprivileged-user-namespace permission comes from, or `None` off Linux, where
/// the question has no answer rather than a negative one.
///
/// Takes its own probe, as its neighbours do; a session reads the provenance off its own
/// [`HostProbe`] instead, so what it is shown and the tier it installs describe one host at one
/// moment.
pub fn detect_userns_grant() -> Option<crate::sealed::UsernsGrant> {
    HostProbe::probe().userns_grant()
}

/// The kernel's Landlock ABI version, or `None` when there is no Landlock (off Linux always), from
/// a reading taken here.
///
/// Takes its own probe, as its neighbours do; a session reads it off its own [`HostProbe`].
pub fn detect_landlock_abi() -> Option<u8> {
    HostProbe::probe().landlock_kernel_abi()
}

/// The highest Landlock ABI a capsule's ruleset is built at.
///
/// v2 adds `Refer` (moving and hard-linking between folders) and v3 adds `Truncate`, both granted
/// on the workdir — see `linux_enforce::WORKDIR_ACCESS_RIGHTS_NO_EXEC`. v4 is not taken: it
/// handles TCP `bind`/`connect`, and declaring those rights would deny every TCP connection the
/// rules do not re-grant, which is the network namespace and egress proxy's boundary to draw. v5
/// (`IoctlDev`) and v6 (abstract unix sockets and signals) each move a different boundary again.
pub(crate) const LANDLOCK_TARGET_ABI: u8 = 3;

/// The Landlock ABI this session's ruleset is built at: the kernel's, capped at
/// [`LANDLOCK_TARGET_ABI`], on the two tiers that install a Landlock domain, and `None` on every
/// other tier or when the kernel reports no Landlock.
///
/// Pure, so the ABI the trace records and the ABI the child builds at are the same function of
/// the same [`HostProbe`] reading.
pub(crate) fn ruleset_landlock_abi(kernel_abi: Option<u8>, tier: EnforcementTier) -> Option<u8> {
    match tier {
        EnforcementTier::KernelFull | EnforcementTier::KernelSealed => kernel_abi
            .filter(|abi| *abi >= 1)
            .map(|abi| abi.min(LANDLOCK_TARGET_ABI)),
        EnforcementTier::KernelSeccompOnly | EnforcementTier::EnvironmentOnly => None,
    }
}

/// The lines `mur doctor` prints under its `Landlock` heading for a kernel reporting
/// `kernel_abi`: what the kernel offers, what the runtime builds at, and what a capsule loses on
/// this host below [`LANDLOCK_TARGET_ABI`].
///
/// Pure and cross-platform, so a test can check the exact words for a kernel this host is not.
pub fn landlock_doctor_lines(kernel_abi: Option<u8>) -> Vec<String> {
    if !cfg!(target_os = "linux") {
        return vec!["kernel ABI: n/a (Landlock is a Linux mechanism)".to_string()];
    }
    let Some(kernel) = kernel_abi.filter(|abi| *abi >= 1) else {
        return vec![
            "kernel ABI: none (no Landlock on this host)".to_string(),
            "ruleset ABI: n/a (no Landlock domain is installed)".to_string(),
        ];
    };
    let built = kernel.min(LANDLOCK_TARGET_ABI);
    let mut lines = vec![
        format!("kernel ABI: {kernel}"),
        format!("ruleset ABI: {built} (the kernel's, capped at {LANDLOCK_TARGET_ABI})"),
    ];
    if built < 2 {
        lines.push(
            "moving or hard-linking a file between folders inside the work folder fails with \
             EXDEV (Invalid cross-device link) on this host; Linux 5.19 (Landlock ABI v2) \
             permits it. Tools that fall back to copy-and-delete (mv, pip, git) still work; uv \
             and other tools that rename atomically between folders do not."
                .to_string(),
        );
    }
    if built < 3 {
        lines.push(
            "truncate(2) on a file outside the work folder is not mediated below Landlock ABI v3 \
             (Linux 6.2)."
                .to_string(),
        );
    }
    lines
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
pub(crate) fn resolve_network_allowlist_ips(
    network_allow: &[String],
) -> Result<Vec<IpAddr>, String> {
    let rules = crate::network_policy::parse_network_allow_rules(network_allow)
        .map_err(|error| error.to_string())?;

    let mut ips = std::collections::BTreeSet::new();
    for rule in &rules {
        let host = rule.host.as_str();
        match crate::dns_resolver::resolve(host) {
            crate::dns_resolver::Resolution::Resolved(addresses) => ips.extend(addresses),
            crate::dns_resolver::Resolution::DoesNotExist => {}
            // Skipped like any other host that contributed no address, but said out loud: a
            // resolver that did not answer is a transient condition an operator can act on,
            // where a name that does not exist is a manifest to correct.
            crate::dns_resolver::Resolution::DidNotAnswer(reason) => crate::runtime_err!(
                "[capsule-runtime] warning: the network allowlist host '{host}' could not be \
                 resolved at launch: {reason} (a subprocess reaching it by literal address is \
                 denied for this run)"
            ),
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

/// Testable core of `resolve_exec_allowlist`, with the `PATH` directory list injected. Every
/// entry is a bare binary name (the manifest refuses any other, `E-MAN-003`), and every
/// executable `PATH` match is included (not just the first): all of them are binaries the
/// operator's launch-time `PATH` exposes under that allowlisted name, and on merged-`/usr` distros
/// `/bin/x` and `/usr/bin/x` canonicalize to the same entry anyway.
pub(crate) fn resolve_exec_allowlist_in(
    shell_allow: &[String],
    path_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut resolved = std::collections::BTreeSet::new();
    for entry in shell_allow {
        if entry.is_empty() {
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
/// injected. `pub(crate)` because [`crate::reachability`] needs the same first-match resolution
/// against an *injected* directory list — its own unit tests point `PATH` at a temp directory of
/// synthetic script fixtures rather than at whatever the machine running the suite has installed.
pub(crate) fn resolve_invoked_binary_path_in(binary: &str, path_dirs: &[PathBuf]) -> String {
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

/// The first executable named `binary` on this process's `PATH`, or `None` when there is none.
///
/// First match wins, which is the one `execvp` would run — the same rule
/// [`resolve_invoked_binary_path_in`] reports under, so a caller naming a binary and a caller
/// reporting one cannot disagree about which file `PATH` means. The path is returned as found,
/// not canonicalized: callers here name a binary to a person rather than derive a grant from it.
pub fn find_on_path(binary: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable_file(candidate))
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ---- derived Landlock read+execute grant set ----
//
// On `KernelFull`, Landlock scopes the shell subprocess tree to the workdir. The workdir rule
// grants `linux_enforce::workdir_access_rights` (every right of the ABI the ruleset is built at
// except device-node and unix-socket creation, and `Execute` unless `workdir_exec`), but — once
// `restrict_self()` lands — `Execute`/`ReadFile` are then denied on every path *outside* the
// workdir, including the `shell.allow` binaries themselves, their ELF interpreter (dynamic loader),
// and every shared library they pull in. That silently defeats the seccomp exec-allowlist: each
// allowlisted `execve` fails with EACCES before it runs.
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
/// Covers both supported platform targets (`linux/x86_64` and `linux/aarch64`), including the
/// Debian/Ubuntu multiarch-triplet subdirectories. Extend here for a platform this list does not
/// cover — it is the single source of truth.
pub(crate) const DEFAULT_LIBRARY_SEARCH_DIRS: &[&str] = &[
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
    Some(if le {
        u16::from_le_bytes(raw)
    } else {
        u16::from_be_bytes(raw)
    })
}

fn read_u32(bytes: &[u8], off: usize, le: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(off..off + 4)?.try_into().ok()?;
    Some(if le {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    })
}

fn read_u64(bytes: &[u8], off: usize, le: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(off..off + 8)?.try_into().ok()?;
    Some(if le {
        u64::from_le_bytes(raw)
    } else {
        u64::from_be_bytes(raw)
    })
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
/// Only ELF64 is parsed: both supported Linux targets (`x86_64`, `aarch64`) are 64-bit, so a
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
    // Each Elf64_Phdr is 56 bytes; a smaller entsize is not this layout.
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
        return Some(ElfDependencies {
            interp,
            needed: Vec::new(),
            runpaths: Vec::new(),
        });
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

    Some(ElfDependencies {
        interp,
        needed,
        runpaths,
    })
}

/// Whether `bytes` begins an ELF64 shared object (`e_type` `ET_DYN`), in either byte order. Needs
/// only the first 18 bytes, so a caller sorting a directory reads no more of each file than that.
/// A position-independent executable is `ET_DYN` too and answers `true`.
#[cfg(target_os = "linux")]
pub(crate) fn elf_is_shared_object(bytes: &[u8]) -> bool {
    if bytes.len() < 18 || &bytes[0..4] != b"\x7fELF" || bytes[4] != 2 {
        return false;
    }
    let le = match bytes[5] {
        1 => true,
        2 => false,
        _ => return false,
    };
    read_u16(bytes, 16, le) == Some(3)
}

/// The name of every entry in an ELF64 image's dynamic symbol table (`.dynsym`), defined and
/// undefined alike, in table order. `None` for anything that is not a well-formed ELF64 image or
/// has no `.dynsym` section.
///
/// Reads the section headers: the `SHT_DYNSYM` section and the string table its `sh_link` names.
/// A symbol the binary imports from a shared library is listed here as well as one it defines, so
/// an interpreter built against `libpython` or `libperl` names its entry symbol the same way a
/// statically linked one does. A binary stripped of its section headers names nothing.
#[cfg(target_os = "linux")]
pub(crate) fn elf_dynamic_symbol_names(bytes: &[u8]) -> Option<Vec<String>> {
    if bytes.len() < 64 || &bytes[0..4] != b"\x7fELF" || bytes[4] != 2 {
        return None;
    }
    let le = match bytes[5] {
        1 => true,
        2 => false,
        _ => return None,
    };
    let e_shoff = read_u64(bytes, 40, le)? as usize;
    let e_shentsize = read_u16(bytes, 58, le)? as usize;
    let e_shnum = read_u16(bytes, 60, le)? as usize;
    // Each Elf64_Shdr is 64 bytes; a smaller entsize is not this layout.
    if e_shentsize < 64 {
        return None;
    }
    // (sh_type, sh_offset, sh_size, sh_link, sh_entsize) of section `index`.
    let section = |index: usize| -> Option<(u32, usize, usize, usize, usize)> {
        let base = e_shoff.checked_add(index.checked_mul(e_shentsize)?)?;
        Some((
            read_u32(bytes, base + 4, le)?,
            read_u64(bytes, base + 24, le)? as usize,
            read_u64(bytes, base + 32, le)? as usize,
            read_u32(bytes, base + 40, le)? as usize,
            read_u64(bytes, base + 56, le)? as usize,
        ))
    };
    // SHT_DYNSYM == 11.
    let (_, sym_off, sym_size, strtab_index, sym_entsize) = (0..e_shnum)
        .filter_map(section)
        .find(|(sh_type, ..)| *sh_type == 11)?;
    // Each Elf64_Sym is 24 bytes.
    if sym_entsize < 24 {
        return None;
    }
    let (_, str_off, str_size, _, _) = section(strtab_index)?;
    let strtab = bytes.get(str_off..str_off.checked_add(str_size)?)?;
    let symbols = bytes.get(sym_off..sym_off.checked_add(sym_size)?)?;
    let mut names = Vec::new();
    for symbol in symbols.chunks_exact(sym_entsize) {
        let st_name = read_u32(symbol, 0, le)? as usize;
        if st_name == 0 {
            continue;
        }
        if let Some(region) = strtab.get(st_name..) {
            names.push(cstr_at_start(region));
        }
    }
    Some(names)
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
    resolve_landlock_grants_outside(exec_allow_paths, search_dirs, &[])
}

/// [`resolve_landlock_grants_in`], skipping every path under one of the canonical `excluded`
/// roots: such a path is neither granted nor read for further dependencies. For a closure that
/// must not be decided by files the capsule can write.
pub(crate) fn resolve_landlock_grants_outside(
    exec_allow_paths: &[PathBuf],
    search_dirs: &[PathBuf],
    excluded: &[PathBuf],
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
        if excluded.iter().any(|root| path.starts_with(root)) {
            continue;
        }
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
            if let Some(resolved) = resolve_soname(soname, origin_dir, &deps.runpaths, search_dirs)
            {
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
///     writing `list_dir: true` next to a specific `interpreter_runtime` directory, on each
///     directory a recognised interpreter reports on its own import path
///     ([`resolve_derived_interpreter_grants`]), and by the whole-tree resolvers
///     ([`resolve_staged_runtime_landlock_grants`], [`resolve_sealed_runtime_landlock_grants`]
///     and, per entry, the directory half of [`resolve_sealed_etc_landlock_grants`]) where a tree
///     is walked by definition — never inferred from anything else, and never applied to an
///     ancestor or sibling of a granted directory.
///
/// `ReadDir` is meaningful only on a directory, and the kernel enforces that: `landlock_add_rule`
/// returns `EINVAL` for a directory-only right on a non-directory. `open_landlock_fds` therefore
/// narrows `list_dir` to `false` for any grant whose fd does not `fstat` as a directory, so this
/// field is a request rather than a promise.
///
/// `executable` decides `Execute`, which is this runtime's exec allowlist: a path without an
/// `Execute` rule cannot be `execve`d, and a path with one can. This is the canonical statement of
/// that rule; the grant resolvers below cross-reference it rather than restating it.
///
///   - `true` → `Execute + ReadFile`, for a path the manifest asked to *run*: a `shell.allow`
///     binary, its `DT_NEEDED` closure, and the interpreter/staged trees an author named.
///   - `false` → `ReadFile` only (readable, and with `list_dir` enumerable, but not runnable).
///     For a grant the *tier* issues rather than the manifest: see
///     [`resolve_sealed_runtime_landlock_grants`], where granting `Execute` over `/usr`, `/bin`
///     and `/sbin` wholesale would turn `shell.allow` into a no-op on `sealed`, and
///     [`resolve_sealed_etc_landlock_grants`], where `/etc/alternatives` is a directory of
///     symlinks into exactly those trees.
///
/// Affects `execve` only: a shared object mapped `PROT_EXEC` by `dlopen(3)` needs `ReadFile`, not
/// `Execute`, so an interpreter still loads its C extensions out of an `executable: false` tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LandlockGrant {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) path: PathBuf,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) list_dir: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) executable: bool,
    /// When set, the rule is added only if `path` still opens to this inode, and is otherwise
    /// dropped (shrink-not-fail). For a grant on a capsule-writable path: the capsule can replace
    /// the file, or a directory above it, with a symlink to any host binary, and an unpinned grant
    /// would follow it and put `Execute` on that binary. A pin tells one live file from another,
    /// not new content from old: an inode number freed by a delete can be reused by the next file.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) pinned: Option<FileIdentity>,
}

/// A file's `(st_dev, st_ino)`: which inode a path named when the runtime recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    dev: u64,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ino: u64,
}

impl FileIdentity {
    #[cfg(unix)]
    pub(crate) fn of(metadata: &std::fs::Metadata) -> FileIdentity {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

impl LandlockGrant {
    /// Wraps each derived `DT_NEEDED`-closure file path as a non-listable grant. A regular file
    /// has no meaningful `ReadDir`, so `list_dir: false` costs nothing. `executable: true`: this
    /// closure is exactly the set of paths `shell.allow` asked to run — see [`LandlockGrant`].
    fn non_listable_files(paths: Vec<PathBuf>) -> Vec<LandlockGrant> {
        paths
            .into_iter()
            .map(|path| LandlockGrant {
                path,
                list_dir: false,
                executable: true,
                pinned: None,
            })
            .collect()
    }
}

/// Turns each `capabilities.shell.interpreter_runtime` directory into a [`LandlockGrant`] carrying
/// exactly the `list_dir` its author wrote. Pure and syscall-free (the declared paths need not
/// exist here — `apply_landlock_scope` skips any that fail to open), so it is unit-testable on
/// every platform, including non-Linux targets.
///
/// There is deliberately no path here that could widen a grant to a whole install prefix: it emits
/// one grant per explicitly declared directory and copies the author's `list_dir` verbatim,
/// inferring enumerability from nothing. A recognised interpreter's own import path is granted
/// separately, by [`resolve_derived_interpreter_grants`]; this is the route for an interpreter the
/// runtime does not recognise.
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
                pinned: None,
            })
        })
        .collect()
}

/// One [`LandlockGrant`] per entry staging derived from a recognised interpreter (see
/// [`crate::interpreter_runtime`]): each directory on its import path readable and listable, and
/// each zip archive on it and each extension-module library readable only.
///
/// `executable: false` on every one (see [`LandlockGrant`]): the manifest asked to run the
/// interpreter, which its own ELF closure grants, not the files it imports. A C extension module
/// still loads, because `dlopen(3)` needs `ReadFile` alone. The derivation never names `/` or an
/// ancestor of the interpreter's directory, so no grant here widens to an install prefix.
pub(crate) fn resolve_derived_interpreter_grants(
    derived: &[crate::interpreter_runtime::DerivedInterpreterRuntime],
) -> Vec<LandlockGrant> {
    let read_only = |path: &Path, list_dir: bool| LandlockGrant {
        path: path.to_path_buf(),
        list_dir,
        executable: false,
        pinned: None,
    };
    derived
        .iter()
        .flat_map(|runtime| {
            runtime
                .entries()
                .map(|(path, is_dir)| read_only(path, is_dir))
                .chain(runtime.libraries.iter().map(|path| read_only(path, false)))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// One [`LandlockGrant`] per host trust-store path staging found (see [`crate::trust_store`]): a
/// directory readable and listable, a CA bundle file readable.
///
/// `executable: false` on every one (see [`LandlockGrant`]): a certificate is read, never run. No
/// write right, as on every [`LandlockGrant`]. The network gate (a non-empty `shell.allow` and a
/// non-empty `network.allow`) lives in [`crate::trust_store::derive_trust_store`], so a policy
/// that fails it reaches here with nothing to grant; the derivation never names `/` or an
/// ancestor of a trust-store path, so no grant here reaches `/etc/ssl/private`.
///
/// Issued on every tier and consulted only on `KernelFull` and `KernelSealed`. On `KernelSealed`
/// a path the composed root does not bind stays unreachable: this grant adds no bind.
pub(crate) fn resolve_trust_store_grants(
    paths: &[crate::trust_store::TrustStorePath],
) -> Vec<LandlockGrant> {
    paths
        .iter()
        .map(|entry| LandlockGrant {
            path: entry.path.clone(),
            list_dir: entry.directory,
            executable: false,
            pinned: None,
        })
        .collect()
}

/// The host directories a composed root must carry *beyond* [`crate::sealed::SEALED_RUNTIME_PATHS`]
/// for this capsule's `shell.allow` to keep working: the containing directory of each resolved
/// allowlisted binary, each declared `interpreter_runtime` directory, and each directory a
/// recognised interpreter reported on its import path with the parent directory of each library
/// its extension modules need.
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
    let mut dirs: Vec<PathBuf> = Vec::new();
    let candidates = exec_allow_paths
        .iter()
        .filter_map(|binary| binary.parent().map(Path::to_path_buf))
        .chain(
            policy
                .shell_interpreter_runtime
                .iter()
                .flat_map(|grant| grant.dirs.iter().map(|dir| PathBuf::from(&dir.path))),
        )
        .chain(
            policy
                .shell_derived_interpreter_runtime
                .iter()
                .flat_map(|runtime| {
                    runtime
                        .entries()
                        .filter(|(_, is_dir)| *is_dir)
                        .map(|(path, _)| path.to_path_buf())
                        .chain(
                            runtime
                                .libraries
                                .iter()
                                .filter_map(|library| library.parent().map(Path::to_path_buf)),
                        )
                        .collect::<Vec<_>>()
                }),
        );

    for candidate in candidates {
        push_sealed_bind_dir(&mut dirs, candidate);
    }
    dirs
}

/// Adds `candidate` to `dirs` as an optional composed-root bind unless it is relative, already
/// inside a fixed [`crate::sealed::SEALED_RUNTIME_PATHS`] entry, or already listed. The one filter
/// [`resolve_sealed_bind_dirs`] and [`ShellEnforcement::for_native_entrypoint`] share.
fn push_sealed_bind_dir(dirs: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidate.is_absolute() {
        return;
    }
    if crate::sealed::SEALED_RUNTIME_PATHS
        .iter()
        .any(|root| candidate.starts_with(root))
    {
        return;
    }
    if dirs.contains(&candidate) {
        return;
    }
    dirs.push(candidate);
}

/// The most bytes [`shebang_interpreter`] reads from the head of a native entrypoint. The
/// kernel's own `#!` buffer (`BINPRM_BUF_SIZE`) is 256 bytes, so a longer line names nothing the
/// kernel would run.
const SHEBANG_READ_LIMIT: u64 = 256;

/// The interpreter the kernel runs when it execs the script `image`: the first token of a leading
/// `#!` line, when that token is an absolute path. `None` for an ELF binary, a relative
/// interpreter and a line with no token.
///
/// Looks at no more than the first [`SHEBANG_READ_LIMIT`] bytes. Only the interpreter itself is
/// named: for `#!/usr/bin/env python3` that is `env`, and `python3` is whatever `env` then finds,
/// which needs its own `capabilities.shell.allow` entry like any other program a tool runs.
pub(crate) fn shebang_interpreter(image: &[u8]) -> Option<PathBuf> {
    let head = &image[..image.len().min(SHEBANG_READ_LIMIT as usize)];
    let line = crate::reachability::shebang_line_of(head)?;
    let interpreter = PathBuf::from(line.split_whitespace().next()?);
    interpreter.is_absolute().then_some(interpreter)
}

/// What the kernel executes to start one installed native tool, derived once, when the runtime
/// writes the tool into the session workdir, from the image bytes it writes.
///
/// The workdir is capsule-writable, so nothing here is read back from it: the `#!` interpreter and
/// the `PT_INTERP`/`DT_NEEDED` entries come from the runtime's own copy of the image, the
/// dependency walk searches only absolute locations and neither grants nor reads a file under the
/// accessible workdir, and the entrypoint's own grant is pinned to the inode the runtime wrote. A
/// symlink or any other existing file put in the entrypoint's place loses its `Execute` rule. A
/// capsule that rewrites the entrypoint runs its own code under these grants, including when it
/// deletes the file and the filesystem hands a new one the freed inode number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeEntrypoint {
    path: PathBuf,
    pinned: Option<FileIdentity>,
    interpreter: Option<PathBuf>,
    closure: Vec<PathBuf>,
}

impl NativeEntrypoint {
    /// Derives the grants for the tool the runtime has just written at `path` from `image`, the
    /// bytes it wrote. `accessible_workdir` is the capsule-writable tree the walk stays out of.
    pub(crate) fn derive(path: &Path, image: &[u8], accessible_workdir: &Path) -> NativeEntrypoint {
        let search_dirs: Vec<PathBuf> = DEFAULT_LIBRARY_SEARCH_DIRS
            .iter()
            .map(PathBuf::from)
            .collect();
        Self::derive_in(path, image, accessible_workdir, &search_dirs)
    }

    fn derive_in(
        path: &Path,
        image: &[u8],
        accessible_workdir: &Path,
        search_dirs: &[PathBuf],
    ) -> NativeEntrypoint {
        let interpreter = shebang_interpreter(image);
        let mut seeds: Vec<PathBuf> = interpreter.iter().cloned().collect();
        if let Some(deps) = parse_elf_dependencies(image) {
            // Only absolute locations are searched. `$ORIGIN` is the entrypoint's own directory in
            // the capsule-writable workdir, and a relative entry resolves against this process's
            // working directory, which may be the workdir too: a symlink planted there would
            // carry the lookup to any host file, and canonicalising it hides that from the
            // exclusion below. A library the tool ships beside itself is readable through the
            // workdir's own rule and needs no grant.
            let absolute = |entry: &&String| Path::new(entry.as_str()).is_absolute();
            seeds.extend(deps.interp.iter().filter(absolute).map(PathBuf::from));
            let runpaths: Vec<String> = deps
                .runpaths
                .iter()
                .filter(|entry| absolute(entry) && !entry.contains("ORIGIN"))
                .cloned()
                .collect();
            seeds.extend(
                deps.needed
                    .iter()
                    .filter(|soname| !soname.contains('/') || absolute(soname))
                    .filter_map(|soname| {
                        resolve_soname(soname, Path::new("/"), &runpaths, search_dirs)
                    }),
            );
        }
        let excluded = std::fs::canonicalize(accessible_workdir)
            .unwrap_or_else(|_| accessible_workdir.to_path_buf());
        NativeEntrypoint {
            path: path.to_path_buf(),
            pinned: pinned_identity(path),
            interpreter,
            closure: resolve_landlock_grants_outside(&seeds, search_dirs, &[excluded]),
        }
    }

    /// The installed entrypoint, the path the tool is spawned from.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// The identity of the regular file at `path`, not following a final symlink. `None` when there is
/// none, which leaves the entrypoint without a grant rather than with an unpinned one.
#[cfg(unix)]
fn pinned_identity(path: &Path) -> Option<FileIdentity> {
    std::fs::symlink_metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|meta| FileIdentity::of(&meta))
}

#[cfg(not(unix))]
fn pinned_identity(_path: &Path) -> Option<FileIdentity> {
    None
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
            pinned: None,
        })
        .collect()
}

/// One listable [`LandlockGrant`] per [`crate::sealed::SEALED_RUNTIME_PATHS`] entry, and only on
/// `KernelSealed`.
///
/// `plan_composed_root` bind-mounts `/usr`, `/bin`, `/sbin`, `/lib`… read-only into every composed
/// root, but the Landlock ruleset installed inside that root denies any path with no matching rule.
/// The `shell.allow` ELF closure grants those files one at a time and non-listably, so a binary
/// opens by exact name while the directory holding it cannot be enumerated — enough to start an
/// interpreter, not enough to run it: CPython dies in `init_fs_encoding` because it cannot
/// `getdents64` `/usr/lib/python3.N` to find `encodings`. This grant makes the bound tree
/// enumerable.
///
/// `list_dir: true` is not configurable: a whole runtime tree a program walks is not a
/// case-by-case author choice. Writes are moot — the binds are `MS_RDONLY`.
///
/// `executable: false` (see [`LandlockGrant`]): this is the only grant covering whole host trees
/// the manifest never named, so it must enumerate them without becoming permission to run them.
///
/// Sealed-tier only: on `KernelFull` (where `scoped` runs) Landlock applies over the real host
/// filesystem, so `ReadDir` on `/usr` would expose host directory shape; under `KernelSealed` the
/// tree is a private read-only bind holding only what was staged, so enumerating it reveals
/// nothing. Empty on every other tier.
///
/// Pure and syscall-free (paths need not exist here — `apply_landlock_scope` skips any that fail
/// to open), so unit-testable on every platform.
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
            pinned: None,
        })
        .collect()
}

/// One read [`LandlockGrant`] per [`crate::sealed::SEALED_ETC_PATHS`] entry, and only on
/// `KernelSealed`.
///
/// Exactly the same shape of bug as [`resolve_sealed_runtime_landlock_grants`], one directory over:
/// `plan_composed_root` bind-mounts the fixed `/etc` allowlist read-only into every composed root,
/// and the Landlock ruleset installed inside that root then denies every one of them, because a
/// path with no matching rule is denied. The mount is present and unopenable — the worst of both
/// answers. Most visibly this is the TLS trust store: `load_verify_locations` on
/// `/etc/ssl/certs/ca-certificates.crt` returns `EACCES`, so `pip`, `curl`, `wget` and `git` all
/// fail certificate verification over an otherwise-working TCP connection.
///
/// `list_dir` is per entry, taken from the `directory` bit each [`crate::sealed::SEALED_EtcPath`]
/// carries (see [`crate::sealed::SealedEtcPath`]) — unlike the runtime-path list, this one mixes
/// directories, regular files and symlinks.
/// Directories get `ReadDir` because enumeration is how the trust store is actually consumed
/// (`c_rehash` symlink lookup, `os.listdir('/etc/ssl/certs')`, `SSL_CERT_DIR`); files and symlinks
/// get `ReadFile` only, the same convention as [`LandlockGrant::non_listable_files`].
///
/// `executable: false` throughout (see [`LandlockGrant`] for what `Execute` decides). Nothing in
/// `/etc` is a program a manifest asked to run, and `/etc/alternatives` in particular is a
/// directory of symlinks *into* `/usr/bin`, so granting `Execute` here would be a second,
/// undeclared route to the exec bypass `resolve_sealed_runtime_landlock_grants` withholds it to
/// prevent.
///
/// No write right, ever: the binds are `MS_RDONLY`, and `/etc/resolv.conf` or `/etc/hosts` becoming
/// writable inside a capsule would be a name-resolution hijack of the capsule's own egress.
///
/// Sealed-tier only, for the same reason as the runtime-path grant: on `KernelFull` — the tier a
/// `scoped` capsule runs on — Landlock applies over the real host `/etc`, where these rules would
/// hand out the host's own trust store, `resolv.conf` and account databases. Under `KernelSealed`
/// the paths resolve inside a private read-only bind of an allowlist that was curated precisely so
/// reading it reveals nothing sensitive. Empty on every other tier.
/// The trust store alone is granted on every Landlock tier, under the network gate, by
/// [`resolve_trust_store_grants`].
///
/// Pure and syscall-free (paths need not exist here — `apply_landlock_scope` skips any that fail to
/// open), so unit-testable on every platform.
pub(crate) fn resolve_sealed_etc_landlock_grants(tier: EnforcementTier) -> Vec<LandlockGrant> {
    if tier != EnforcementTier::KernelSealed {
        return Vec::new();
    }
    crate::sealed::SEALED_ETC_PATHS
        .iter()
        .map(|entry| LandlockGrant {
            path: PathBuf::from(entry.path),
            list_dir: entry.directory,
            executable: false,
            pinned: None,
        })
        .collect()
}

// ---- fixed capsule device set ------------------------------------------------------------
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
///   - `true` → `ReadFile | WriteFile`. Granted to exactly one path, `/dev/null`, which with the
///     session's temporary folder is one of the two writable paths outside the workdir.
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
/// `handle_access` declaring the full right-set of the ABI the ruleset is built at, "no rule" means
/// "denied", not "ungoverned".
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
/// place this list is documented (`docs/content/reference/capsule-device-set-manual-verification.md`).
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

/// Network decision for one destination IP.
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
// non-Linux targets, where `libc::AF_NETLINK` and `libc::AF_PACKET` do not exist at all (the `libc`
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
/// pointer BPF cannot dereference, so deciding on it needs a userspace round-trip that reads the
/// calling task's memory (see this module's header). Denying the domain at *creation* time needs
/// no round-trip, and is structurally immune to the TOCTOU class of problem that reading a
/// pointed-to argument out of another task's memory invites.
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
/// Literals from the same `include/linux/socket.h` list as their neighbours above, for the same
/// reason: the filter is always compiled for a Linux child.
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
/// `execve`/`execveat` are ordinary allowed syscalls here: they permit only *reaching the kernel*,
/// where the Landlock domain decides the exec on the path it resolved itself (see this module's
/// header for why they no longer carry `Notify` rules). Permitting them is not optional — the
/// child's very first act after this filter loads is the `execve` that turns it into the tool
/// binary, so a denial here refuses every capsule outright.
///
/// Everything else is refused by the default action. Without this list `io_uring_setup`, `bpf`,
/// `userfaultfd`, `perf_event_open`, `open_by_handle_at`, `keyctl` and `ptrace` are all reachable
/// from a capsule shell on bare metal, while the same probe under Docker gets `EPERM` for each.
/// `io_uring` is the one that
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
    // `clone`. Reproducing that faithfully is possible for `clone` but pointless in isolation:
    // `unshare`/`setns`/`mount`/`pivot_root`/`chroot` all stay denied, so a new namespace stays
    // largely inert.
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
    // `connect`/`sendto` ARE here: allowing them permits only *reaching the kernel*, where the
    // capsule's own network namespace has no route to anything except the egress proxy. The
    // destination check lives there, not in this filter — see `crate::network_namespace`.
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
///   1. **Demonstrated reachable** — a probe reached these on bare metal while Docker refused
///      them: `io_uring_setup`/`io_uring_enter`/
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
// The list is a specification, not an input to the filter build: its one consumer is the
// regression test asserting no entry ever reappears in the allowlist. Deleting it would delete
// the thing that test checks against.
#[allow(dead_code)]
pub(crate) const SECCOMP_MUST_STAY_DENIED: &[&str] = &[
    // 1. demonstrated reachable on bare metal
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

/// The largest BPF program `linux_enforce::build_seccomp_filter` may compile to before this
/// project treats the filter as having outgrown its headroom. Held against the *measured* export
/// of the real filter by a test, not against an estimate: growing
/// [`SECCOMP_SYSCALL_ALLOWLIST`] past this fails `cargo test` with the measured count in the
/// message, instead of surfacing as a spawn that dies inside `pre_exec` on some host.
///
/// Set well below [`BPF_MAXINSNS_CEILING`] on purpose. It is a project budget, not a kernel one:
/// crossing it is a prompt to re-measure and decide, not evidence that the kernel would refuse.
/// `docs/content/reference/seccomp-filter-instruction-budget.md` records the measurement this
/// number was chosen against.
#[allow(dead_code)]
pub(crate) const SECCOMP_FILTER_INSTRUCTION_BUDGET: usize = 512;

/// The kernel's `BPF_MAXINSNS` — the most instructions a *single* seccomp filter may contain.
/// A longer program is refused by `seccomp(SECCOMP_SET_MODE_FILTER)` with `EINVAL`, before the
/// filter is attached to anything. Measured rather than quoted: a test brackets it by loading
/// two filters that straddle it in forked children.
#[allow(dead_code)]
pub(crate) const BPF_MAXINSNS_CEILING: usize = 4096;

/// The kernel's `MAX_INSNS_PER_PATH` — the most instructions the *whole chain* of filters
/// attached to one task may total, counting every filter it already carries plus a small
/// per-filter accounting overhead. An attach that would cross it is refused with `ENOMEM`,
/// which is the only seccomp path in the kernel that returns that errno. Measured rather than
/// quoted: a test stacks filters in a forked child until the refusal arrives.
#[allow(dead_code)]
pub(crate) const SECCOMP_MAX_INSNS_PER_PATH: usize = 32768;

/// Bundles the resolved, host-independent enforcement inputs for one capsule session.
#[derive(Debug, Clone)]
pub(crate) struct ShellEnforcement {
    pub(crate) tier: EnforcementTier,
    /// Every `capabilities.network.allow` host resolved to concrete addresses once, at launch,
    /// by [`resolve_network_allowlist_ips`].
    ///
    /// The narrower of the egress proxy's two checks — the one applied to a destination the
    /// capsule reached without ever resolving a name for it (a hardcoded literal). See
    /// `egress_proxy::EgressPolicy::allows_connection`, which consults exactly this list through
    /// [`network_ip_allowed`].
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
    /// The Landlock ABI the child's ruleset is built at: [`ruleset_landlock_abi`] of the session's
    /// [`HostProbe`] and [`Self::tier`], decided here so the ABI `session_start` records is the one
    /// installed. `None` on the tiers with no Landlock domain, and off Linux always.
    ///
    /// On `KernelFull`/`KernelSealed` a `None` builds at ABI v1, the narrowest set, so a missing
    /// reading can only narrow what the capsule may do. `Some(1)` likewise reproduces exactly
    /// what a 5.13–5.18 kernel installs, which is how the tests force that ruleset on a newer one.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) landlock_abi: Option<u8>,
    /// Narrow read (never write) Landlock grants *outside* the workdir so the allowlisted
    /// binaries can actually exec, dynamically link, and (for a path-based interpreter) reach their
    /// stdlib. Seven origins, combined here:
    ///
    ///   - the `DT_NEEDED`-closure files (`shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure), from `resolve_landlock_grants` — each wrapped `list_dir: false`
    ///     (they are individual files, where `ReadDir` was always a no-op);
    ///   - one grant per `capabilities.shell.interpreter_runtime` directory, from
    ///     `resolve_interpreter_runtime_grants` — each carrying exactly the `list_dir` its author
    ///     declared;
    ///   - one non-executable grant per entry a recognised interpreter reported on its import
    ///     path at staging, and per library its extension modules need, from
    ///     `resolve_derived_interpreter_grants` — listable for a directory;
    ///   - one non-executable read grant per host trust-store path staging found for a capsule
    ///     with a non-empty `shell.allow` and `network.allow`, from `resolve_trust_store_grants` —
    ///     listable for a directory;
    ///   - one listable grant per `capabilities.shell.staged_runtime` tree, from
    ///     `resolve_staged_runtime_landlock_grants`;
    ///   - on `KernelSealed` **only**, one listable but *non-executable* grant per fixed
    ///     [`crate::sealed::SEALED_RUNTIME_PATHS`] entry, from
    ///     `resolve_sealed_runtime_landlock_grants` — the composed root's own bind-mounted runtime
    ///     tree, which nothing else grants;
    ///   - on `KernelSealed` **only**, one non-executable read grant per fixed
    ///     [`crate::sealed::SEALED_ETC_PATHS`] entry, from `resolve_sealed_etc_landlock_grants`
    ///     (`list_dir` per entry, since that list mixes directories and files) — the composed
    ///     root's `/etc` allowlist, likewise granted by nothing else.
    ///
    /// The closure, `interpreter_runtime` and `staged_runtime` grants carry `executable: true`;
    /// the derived interpreter grants, the trust-store grants and the two tier-issued ones do not,
    /// and that asymmetry is load-bearing — see [`LandlockGrant`].
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
    /// Whole directories, never files: a bind carries a directory's whole contents, so this must
    /// not grow into a second ELF-closure derivation. Consumed only on `KernelSealed`; resolved on
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
    /// so macOS gets the per-process bounds even though it can never get the aggregate (cgroup)
    /// half. See [`crate::resources`].
    pub(crate) resource_limits: crate::resources::HostResourceLimits,
    /// The session's cgroup v2 scope, when the host could delegate one (Linux only, and only
    /// for capsules that can actually spawn a native subprocess). `None` on macOS always, and on
    /// Linux only for capsules with no subprocess capability at all — a Linux capsule that *can*
    /// spawn one and could not be given a scope never reaches here, because the launch is
    /// refused first with `RuntimeError::CgroupDelegationUnavailable`.
    ///
    /// Its presence decides each spawn's `RLIMIT_NPROC` ([`Self::nproc_bound`]): a spawn that
    /// joins it is bounded by `pids.max` alone. A native tool can still be dispatched on a
    /// session whose policy required no scope, so a spawn path never assumes one is here.
    pub(crate) cgroup_scope: Option<Arc<crate::cgroup::CgroupScope>>,
    /// The session's periodic workdir-size check. Consulted before every subprocess spawn, so a
    /// disk filler stops writing at the first spawn after the ceiling is crossed rather than
    /// only at the next agent turn.
    pub(crate) workdir_guard: Option<Arc<crate::resources::WorkdirGuard>>,
    /// The session's one network namespace, which every command on a kernel tier joins.
    ///
    /// Shared by every clone of this enforcement, so a listener one command starts is reachable
    /// from the next. A fresh, not-yet-created namespace from [`Self::resolve`] and
    /// [`Self::environment_only`]; a plan inside a session takes its session's through
    /// [`Self::with_session_netns`]. Released by [`Self::end_session`].
    pub(crate) session_netns: Arc<crate::network_namespace::SessionNetns>,
    /// The session's runtime directory and its `sealed-etc/` sibling: `$MURMUR_RUNTIME_DIR` in
    /// every command's environment, a read-only Landlock rule on the Landlock tiers, a read-only
    /// bind on `KernelSealed`, and the source of the composed root's `/etc/passwd` and
    /// `/etc/group`. `None` outside a session, where a command is told of no runtime directory
    /// and a sealed root cannot be composed.
    pub(crate) runtime_dir: Option<crate::runtime_dir::RuntimeDirRoots>,
    /// The session's runtime directory itself, removed by [`Self::remove_runtime_dir`] on the
    /// process exits that run no destructor. Set only on the session's own enforcement.
    pub(crate) runtime_dir_owner: Option<crate::runtime_dir::SharedRuntimeDir>,
    /// How a literal `/tmp` reaches the session's temporary folder (`runtime_dir`'s `temp`):
    /// the session's [`crate::temp_dir::temp_mapping`] answer, through [`Self::with_temp_mapping`],
    /// or [`crate::temp_dir::tier_temp_mapping`] of [`Self::tier`] until one is handed in. Read
    /// only while the enforcement holds a folder.
    pub(crate) temp_mapping: crate::temp_dir::TempMapping,
}

impl ShellEnforcement {
    /// Resolves tier + network allowlist + canonical exec allowlist once, at launch time.
    ///
    /// The rlimit half of [`crate::resources`] is resolved here too (it needs nothing but the
    /// policy), while the cgroup scope and workdir guard are session-scoped live handles the
    /// caller creates and attaches with [`Self::with_host_bounding`].
    ///
    /// `decision` is the session's [`crate::containment::ContainmentDecision`], and its applied
    /// tier is the one installed: whether a sealed-capable host builds a composed root is decided
    /// there, from the declared floor, so the tier installed here is the one the session's trace
    /// names.
    ///
    /// `probe` is the reading `decision` was made from — for a session, the [`HostProbe`]
    /// `runtime::stage_session` took and carried on the `StagedSession` — and supplies the
    /// kernel's Landlock ABI.
    pub(crate) fn resolve(
        policy: &CapabilityPolicy,
        decision: &crate::containment::ContainmentDecision,
        probe: HostProbe,
    ) -> Result<Self, String> {
        let tier = decision.applied_tier();
        let network_allow_ips = resolve_network_allowlist_ips(&policy.network_allow)?;
        let network_allow_rules =
            crate::network_policy::parse_network_allow_rules(&policy.network_allow)
                .map_err(|error| error.to_string())?;
        let egress_tcp_ports = crate::egress_proxy::egress_listen_ports(&network_allow_rules);
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        // The `DT_NEEDED`-closure files (individual files → non-listable), one grant per
        // author-declared `interpreter_runtime` directory (each with its own `list_dir`), and one
        // read-only grant per entry a recognised interpreter reported on its own import path at
        // staging. A directory neither the manifest names nor an allowlisted interpreter reports
        // never receives a rule, regardless of what it holds.
        let mut landlock_grants =
            LandlockGrant::non_listable_files(resolve_landlock_grants(&exec_allow_paths));
        landlock_grants.extend(resolve_interpreter_runtime_grants(
            &policy.shell_interpreter_runtime,
        ));
        landlock_grants.extend(resolve_derived_interpreter_grants(
            &policy.shell_derived_interpreter_runtime,
        ));
        // Plus one read-only grant per host trust-store path staging found. Not tier-gated: the
        // network gate lives in the derivation, which leaves the field empty for a capsule whose
        // commands cannot reach the network.
        landlock_grants.extend(resolve_trust_store_grants(&policy.shell_trust_store));
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
        // And, on `KernelSealed` only, one read grant per fixed `SEALED_ETC_PATHS` entry — the same
        // mounted-but-denied bug one directory over, and the reason TLS certificate verification
        // failed inside a composed root. Tier-gated by the resolver itself, exactly as above: on
        // `KernelFull` these rules would apply to the *host's* /etc.
        landlock_grants.extend(resolve_sealed_etc_landlock_grants(tier));
        let sealed_bind_dirs = resolve_sealed_bind_dirs(&exec_allow_paths, policy);
        let staged_runtime_dirs = resolve_staged_runtime_dirs(policy);
        Ok(Self {
            tier,
            network_allow_ips,
            network_allow_rules,
            egress_tcp_ports,
            unix_sockets_allowed: policy.unix_sockets_allowed,
            workdir_exec: policy.workdir_exec_allowed,
            landlock_abi: ruleset_landlock_abi(probe.landlock_kernel_abi(), tier),
            landlock_grants,
            sealed_bind_dirs,
            staged_runtime_dirs,
            resource_limits: policy.resources,
            cgroup_scope: None,
            workdir_guard: None,
            session_netns: Arc::new(crate::network_namespace::SessionNetns::new()),
            runtime_dir: None,
            runtime_dir_owner: None,
            temp_mapping: crate::temp_dir::tier_temp_mapping(tier),
        })
    }

    /// [`Self::resolve`] for a capsule declaring `declared`, decided from `probe` with the
    /// runtime directory outside the workdir: the seam tests resolve an enforcement through.
    #[cfg(test)]
    pub(crate) fn resolve_declared(
        policy: &CapabilityPolicy,
        declared: murmur_artifact::ContainmentClass,
        probe: HostProbe,
    ) -> Result<Self, String> {
        let decision = crate::containment::ContainmentDecision::decide(
            probe.tier(),
            declared,
            policy.workdir_exec_allowed,
            false,
        );
        Self::resolve(policy, &decision, probe)
    }

    /// Attach the session's runtime directory, and with `owner` the directory itself, so a
    /// process exit that runs no destructor removes it ([`Self::remove_runtime_dir`]).
    pub(crate) fn with_runtime_dir(
        mut self,
        roots: crate::runtime_dir::RuntimeDirRoots,
        owner: Option<crate::runtime_dir::SharedRuntimeDir>,
    ) -> Self {
        self.runtime_dir = Some(roots);
        self.runtime_dir_owner = owner;
        self
    }

    /// The runtime directory's absolute path, the value of `$MURMUR_RUNTIME_DIR`.
    pub(crate) fn runtime_dir_path(&self) -> Option<&Path> {
        self.runtime_dir.as_ref().map(|roots| roots.runtime.path())
    }

    /// The session's records directory, held. `None` for an enforcement held outside a session.
    pub(crate) fn records_root(&self) -> Option<&crate::workdir_fs::WorkdirRoot> {
        self.runtime_dir
            .as_ref()
            .and_then(|roots| roots.records.as_ref())
    }

    /// The records directory's absolute path, the value of `$MURMUR_RECORDS_DIR`. `None` for an
    /// enforcement held outside a session.
    pub(crate) fn records_dir_path(&self) -> Option<&Path> {
        self.records_root()
            .map(crate::workdir_fs::WorkdirRoot::path)
    }

    /// The session's temporary folder's absolute path. `None` for an enforcement held outside a
    /// session.
    pub(crate) fn temp_dir_path(&self) -> Option<&Path> {
        self.runtime_dir
            .as_ref()
            .and_then(|roots| roots.temp.as_ref())
            .map(crate::workdir_fs::WorkdirRoot::path)
    }

    /// The session's home folder, `home/` in its records directory, held. `None` for an
    /// enforcement held outside a session, which runs no command.
    pub(crate) fn home_root(&self) -> Option<&crate::workdir_fs::WorkdirRoot> {
        self.runtime_dir
            .as_ref()
            .and_then(|roots| roots.home.as_ref())
    }

    /// The directory the session's delegated children keep their records beneath, when the
    /// session can delegate and a child has staged there; read-only to every command. Linux only:
    /// no other platform grants it.
    #[cfg(target_os = "linux")]
    pub(crate) fn children_records_path(&self) -> Option<&Path> {
        self.runtime_dir
            .as_ref()
            .and_then(|roots| roots.children_records.as_deref())
            .filter(|path| path.is_dir())
    }

    /// The home folder's absolute path, the value of `$HOME` and the synthetic passwd's home
    /// directory on every tier. `None` for an enforcement held outside a session.
    pub(crate) fn home_dir_path(&self) -> Option<&Path> {
        self.home_root().map(crate::workdir_fs::WorkdirRoot::path)
    }

    /// Hand in the session's [`crate::temp_dir::temp_mapping`] answer, decided for this
    /// enforcement's tier.
    pub(crate) fn with_temp_mapping(mut self, mapping: crate::temp_dir::TempMapping) -> Self {
        self.temp_mapping = mapping;
        self
    }

    /// What this enforcement installs for `/tmp` while it holds a folder: the composed root's bind
    /// on `KernelSealed`, a private mount namespace on `KernelFull` and `KernelSeccompOnly` when
    /// [`Self::temp_mapping`] says so, and nothing otherwise. Keyed on [`Self::tier`] as well, so
    /// no tier is ever told of a mechanism it does not install.
    pub(crate) fn installed_temp_mapping(&self) -> crate::temp_dir::TempMapping {
        use crate::temp_dir::TempMapping;
        match (self.tier, self.temp_mapping) {
            (
                EnforcementTier::KernelFull | EnforcementTier::KernelSeccompOnly,
                TempMapping::MountNamespace,
            ) => TempMapping::MountNamespace,
            (
                EnforcementTier::KernelFull | EnforcementTier::KernelSeccompOnly,
                TempMapping::NotMapped(reason),
            ) => TempMapping::NotMapped(reason),
            (tier, _) => crate::temp_dir::tier_temp_mapping(tier),
        }
    }

    /// The value of `TMPDIR`, `TMP` and `TEMP` in every command this enforcement spawns: `/tmp`
    /// when a literal `/tmp` is the session's temporary folder, the folder's absolute path when
    /// not, and `None` when the enforcement holds no folder.
    pub(crate) fn tmpdir_value(&self) -> Option<String> {
        self.temp_dir_path()
            .map(|folder| self.installed_temp_mapping().tmpdir_value(folder))
    }

    /// Removes the session's runtime directory now, ahead of a process exit that runs no
    /// destructor. Does nothing on an enforcement that does not own one.
    pub(crate) fn remove_runtime_dir(&self) {
        if let Some(owner) = &self.runtime_dir_owner {
            owner.remove();
        }
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

    /// The `RLIMIT_NPROC` decision for one spawn, made in the parent before its `pre_exec`
    /// closure is installed.
    ///
    /// [`crate::resources::NprocBound::CgroupPidsMax`] when the spawn will join a scope that
    /// places it, because both closures that receive this run the join fatally before `execve`.
    /// Otherwise [`crate::resources::NprocBound::Headroom`], with a baseline measured by
    /// `crate::resources::uid_task_count` on this call — a `/proc` walk on Linux — so call it
    /// once per spawn and never from inside `pre_exec`.
    pub(crate) fn nproc_bound(&self) -> crate::resources::NprocBound {
        if self
            .cgroup_scope
            .as_ref()
            .is_some_and(|scope| scope.places_subprocesses())
        {
            crate::resources::NprocBound::CgroupPidsMax
        } else {
            crate::resources::NprocBound::Headroom {
                baseline: crate::resources::uid_task_count().unwrap_or(0),
            }
        }
    }

    /// The latched workdir-size breach, if the guard has seen one.
    pub(crate) fn workdir_breach(&self) -> Option<crate::resources::WorkdirBreach> {
        self.workdir_guard.as_ref().and_then(|guard| guard.breach())
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
            // No Landlock domain on this tier. A test that spreads this over a Landlock tier
            // builds at ABI v1, the narrowest set.
            landlock_abi: None,
            landlock_grants: Vec::new(),
            sealed_bind_dirs: Vec::new(),
            staged_runtime_dirs: Vec::new(),
            // Defaults, not "no limits": the rlimit ceilings apply unchanged on this tier, so
            // zeroing them out here would misrepresent what a real macOS host does.
            resource_limits: crate::resources::HostResourceLimits::default(),
            cgroup_scope: None,
            workdir_guard: None,
            session_netns: Arc::new(crate::network_namespace::SessionNetns::new()),
            runtime_dir: None,
            runtime_dir_owner: None,
            temp_mapping: crate::temp_dir::tier_temp_mapping(EnforcementTier::EnvironmentOnly),
        }
    }

    /// This enforcement holding a session's runtime directory roots, home folder included, in a
    /// fresh directory kept past the test: a clone may outlive the value. For a test whose
    /// commands need only to run.
    #[cfg(test)]
    pub(crate) fn with_test_session_dirs(self) -> Self {
        let base = tempfile::tempdir().unwrap().keep();
        self.with_runtime_dir(test_runtime_dir(&base), None)
    }

    /// Run this enforcement's commands in `session_netns` rather than in a namespace of its own.
    ///
    /// For a plan submitted inside a session: its shell steps are that session's commands, so
    /// they share its loopback and its egress proxy.
    pub(crate) fn with_session_netns(
        mut self,
        session_netns: Arc<crate::network_namespace::SessionNetns>,
    ) -> Self {
        self.session_netns = session_netns;
        self
    }

    /// This enforcement for one native tool spawn: a clone whose exec allowlist also holds what
    /// the kernel itself executes to start `entrypoint`.
    ///
    /// That is the entrypoint, pinned to the inode the runtime installed, and the closure
    /// [`NativeEntrypoint`] derived from the installed image: the absolute interpreter on its `#!`
    /// line, and the `PT_INTERP`/`DT_NEEDED` closure — the native-tool counterpart of what
    /// `capabilities.shell.allow` derives for a shell binary. The entrypoint needs its own
    /// `Execute` rule because it sits in the runtime directory, whose rule grants reading and
    /// nothing else. On `KernelSealed` the interpreter's directory is also bound into the composed
    /// root when no fixed runtime path carries it; the entrypoint needs no bind, because the root
    /// binds the runtime directory at its own path.
    ///
    /// Nothing else is added, and nothing is read from the workdir here. Whatever the tool reads
    /// or runs once started is granted by the capsule, exactly as for a shell command. Every clone
    /// shares `session_netns`, so the tool joins the session's one namespace and egress proxy.
    pub(crate) fn for_native_entrypoint(&self, entrypoint: &NativeEntrypoint) -> ShellEnforcement {
        let mut enforcement = self.clone();
        if let Some(pinned) = entrypoint.pinned {
            enforcement.landlock_grants.push(LandlockGrant {
                path: entrypoint.path.clone(),
                list_dir: false,
                executable: true,
                pinned: Some(pinned),
            });
        }
        enforcement
            .landlock_grants
            .extend(LandlockGrant::non_listable_files(
                entrypoint.closure.clone(),
            ));
        if self.tier == EnforcementTier::KernelSealed {
            if let Some(dir) = entrypoint.interpreter.as_deref().and_then(Path::parent) {
                push_sealed_bind_dir(&mut enforcement.sealed_bind_dirs, dir.to_path_buf());
            }
        }
        enforcement
    }

    /// End the session: kill every process it started, then release its network namespace.
    ///
    /// Killing goes through the cgroup scope, which every spawn joins before `execve` and no fork
    /// escapes, so a server backgrounded with `&`, a `setsid` double fork and a demoted command
    /// still running are all in it. The namespace is released only once the scope is empty (or
    /// [`SESSION_END_KILL_WAIT`] has passed), so its proxy never stops under a process still
    /// using it. Idempotent; every spawn on this enforcement afterwards is refused.
    pub(crate) fn end_session(&self) {
        self.kill_session_processes();
        self.session_netns.end();
    }

    /// Kill every process in this enforcement's cgroup scope and wait, bounded by
    /// [`SESSION_END_KILL_WAIT`], for the scope to empty. Does nothing without a scope.
    ///
    /// The half of [`Self::end_session`] a plan runs at its own end: a plan's scope is its own,
    /// and the namespace may be its session's.
    pub(crate) fn kill_session_processes(&self) {
        let Some(scope) = self.cgroup_scope.as_ref() else {
            return;
        };
        if let Err(error) = scope.kill_all() {
            crate::runtime_err!(
                "[capsule-runtime] warning: could not kill the processes in cgroup scope {}: \
                 {error}",
                scope.path().display()
            );
        }
        if !scope.wait_until_empty(SESSION_END_KILL_WAIT) {
            crate::runtime_err!(
                "[capsule-runtime] warning: cgroup scope {} still held processes {} ms after \
                 they were killed",
                scope.path().display(),
                SESSION_END_KILL_WAIT.as_millis()
            );
        }
    }
}

/// How long [`ShellEnforcement::end_session`] waits for a killed scope's `cgroup.procs` to empty.
pub(crate) const SESSION_END_KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Calls [`ShellEnforcement::end_session`] when dropped.
///
/// Held by a launch for its whole length, so a session that ends by an error return still kills
/// what it started. Every path that ends a session normally calls `end_session` itself first, at
/// the point its teardown needs it; the call this makes then finds nothing left to do.
pub(crate) struct EndSessionOnDrop(ShellEnforcement);

impl EndSessionOnDrop {
    pub(crate) fn new(enforcement: ShellEnforcement) -> Self {
        Self(enforcement)
    }
}

impl Drop for EndSessionOnDrop {
    fn drop(&mut self) {
        self.0.end_session();
    }
}

/// Core messages shared between the stderr line and `logs/bootstrap.log` line for each tier
/// warning, matching the shared-const convention used by `runtime::BASH_NETWORK_BYPASS_WARNING`.
/// Full detail lives on the diagnostics doc page (`security_warning_link`); keep each to
/// one or two concise sentences.
///
/// The Landlock filesystem scope now derives a narrow read+execute grant (the `shell.allow`
/// binaries, their ELF interpreter, and their shared-library closure) *outside* the workdir, in
/// addition to the workdir's own grant (which withholds device-node and unix-socket creation, see
/// `linux_enforce::WORKDIR_ACCESS_RIGHTS`) — so allowlisted programs can actually exec and
/// dynamically link, and the only writable paths outside the workdir are the session's temporary
/// and home folders and `/dev/null`, one of the three fixed devices in `CAPSULE_DEVICE_GRANTS`. Seccomp
/// additionally refuses the `socket(2)` domains in `denied_socket_domains` — closing the
/// `/var/run/docker.sock` path, which no Landlock ABI mediates for pathname sockets. The forked
/// shell child also drops every Linux capability before `execve` (see
/// `linux_enforce::drop_all_capabilities`).
/// `KernelFull` warns (`W_SEC_005`) rather than staying silent: the tier is a filesystem and
/// syscall scope, not a namespace, so what it does and does not cover has to reach the operator.
const KERNEL_ENFORCEMENT_NOTICE: &str = "capabilities.shell.allow is non-empty and this host \
resolved to a Linux kernel-enforcement tier (Landlock/seccomp). Landlock now grants a narrow, \
derived read+execute scope outside the workdir (the allowlisted binaries, their loader, and their \
shared libraries — nothing writable), read-only access to the directories an allowlisted \
CPython or Perl interpreter reports on its own import path, read-only access to the host's TLS \
trust store when capabilities.network.allow is also non-empty, and no other directory granted \
wholesale, plus a fixed device set \
every capsule gets unconditionally: /dev/null read+write (with the session's temporary and home \
folders, the only writable paths outside the workdir), \
/dev/zero and /dev/urandom read-only, and no other device — /dev/random, /dev/full, /dev/tty and \
raw block devices stay denied. The workdir's own grant also \
withholds device-node and unix-socket creation; seccomp refuses socket(AF_UNIX) outright unless \
capabilities.network.unix_sockets is declared, and always refuses AF_NETLINK/AF_PACKET, so a \
capsule cannot reach a host daemon socket such as /var/run/docker.sock; the seccomp filter now \
defaults to deny and permits only a fixed syscall allowlist modelled on the OCI/Docker default \
profile, so io_uring, bpf, userfaultfd, perf_event_open, ptrace and similar are refused outright; \
and the forked shell child \
drops every Linux capability and sets no_new_privs before execve. Paths outside the workdir are \
denied, not absent: stat, access and readlink on an ungranted path still succeed.";

/// `KernelSealed`'s counterpart to [`KERNEL_ENFORCEMENT_NOTICE`], carrying the same `W_SEC_005`
/// code: a silent strongest tier would imply the boundary has no documented exceptions, and it
/// has one. Names what `sealed` adds over `scoped` so the warning is not a louder copy of it.
const KERNEL_SEALED_ENFORCEMENT_NOTICE: &str = "capabilities.shell.allow is non-empty and this \
host resolved to the sealed kernel-enforcement tier: the subprocess tree runs in a private mount \
namespace pivoted onto a composed root (host runtime bind-mounted read-only, the session workdir, \
its temporary folder at /tmp and its home folder at $HOME the only writable paths, a private /dev \
tmpfs carrying the OCI default device set), with Landlock \
and seccomp still installed inside it as defence in depth. Paths outside that root are absent \
rather than denied — with one documented exception: /proc is a bind of the host's, not a masked \
private procfs, because mounting one unprivileged needs a PID namespace this tier does not create, \
so host process metadata stays visible there exactly as it is under scoped.";

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
        EnforcementTier::KernelSealed => Some((W_SEC_005, KERNEL_SEALED_ENFORCEMENT_NOTICE)),
        EnforcementTier::KernelFull => Some((W_SEC_005, KERNEL_ENFORCEMENT_NOTICE)),
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
    workdir: &crate::workdir_fs::WorkdirRoot,
    requires_bounding: bool,
    has_scope: bool,
) {
    let is_linux = cfg!(target_os = "linux");
    if let Some((code, message)) =
        aggregate_bounding_warning(is_linux, requires_bounding, has_scope)
    {
        let link = security_warning_link(code);
        crate::runtime_err!("[capsule-runtime] warning[{code}]: {message} ({link})");
        crate::agent::append_bootstrap_log(
            workdir,
            &format!("[capability-policy] warning[{code}]: {message} ({link})"),
        );
    }
}

/// Fires at every launch, not just once.
pub(crate) fn warn_for_enforcement_tier(
    tier: EnforcementTier,
    workdir: &crate::workdir_fs::WorkdirRoot,
    policy: &CapabilityPolicy,
) {
    if let Some((code, message)) = tier_warning(tier, policy.shell_allow.is_empty()) {
        let link = security_warning_link(code);
        crate::runtime_err!("[capsule-runtime] warning[{code}]: {message} ({link})");
        crate::agent::append_bootstrap_log(
            workdir,
            &format!("[capability-policy] warning[{code}]: {message} ({link})"),
        );
    }
    if tier == EnforcementTier::EnvironmentOnly {
        crate::runtime::warn_if_bash_network_bypass(workdir, policy);
    }
}

/// What `prepare_enforcement` hands back alongside the configured `Command`: on a kernel tier, the
/// read end of the child's diagnostic pipe.
///
/// The session's egress proxy is not here. It belongs to the session's
/// [`crate::network_namespace::SessionNetns`] and outlives every command, so a process a command
/// backgrounds keeps its allowlisted egress after the command returns.
#[derive(Debug)]
pub(crate) enum SupervisorHandle {
    Noop,
    #[cfg(target_os = "linux")]
    Linux {
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

// Forced-failure seam for the fail-closed contract: inducing a real kernel-level setup failure
// requires already-hostile conditions, so tests simulate one here instead. Only compiled into
// test builds; production `prepare_enforcement` has no bypass or injection point.
#[cfg(test)]
thread_local! {
    pub(crate) static FORCE_PREPARE_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn forced_prepare_failure() -> Result<(), String> {
    if FORCE_PREPARE_FAILURE.with(|flag| flag.get()) {
        return Err("sandbox: kernel enforcement setup failed (forced by test seam)".to_string());
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
    /// Fails the `/tmp` mapping of a `MountNamespace` spawn at its first step, before any real
    /// namespace syscall, so a failed mapping's path is testable on any Linux host.
    pub(crate) static FORCE_TMP_MAP_FAILURE: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// RAII arm for [`FORCE_TMP_MAP_FAILURE`].
#[cfg(all(test, target_os = "linux"))]
pub(crate) struct ForceTmpMapFailureGuard;

#[cfg(all(test, target_os = "linux"))]
impl ForceTmpMapFailureGuard {
    pub(crate) fn new() -> Self {
        FORCE_TMP_MAP_FAILURE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for ForceTmpMapFailureGuard {
    fn drop(&mut self) {
        FORCE_TMP_MAP_FAILURE.with(|flag| flag.set(false));
    }
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
    // Kept past the test: the enforcement may be cloned into a thread that outlives the value.
    let base = tempfile::tempdir().unwrap().keep();
    ShellEnforcement {
        tier: EnforcementTier::KernelSealed,
        ..ShellEnforcement::environment_only().with_test_session_dirs()
    }
    .with_runtime_dir(test_runtime_dir(&base), None)
}

/// `runtime/`, `sealed-etc/`, `harness/` and a records directory holding a temporary folder and
/// a home folder, created under `base`, held as a session's runtime directory roots.
#[cfg(test)]
pub(crate) fn test_runtime_dir(base: &Path) -> crate::runtime_dir::RuntimeDirRoots {
    let open = |sub: &str| {
        let path = base.join(sub);
        std::fs::create_dir_all(&path).unwrap();
        crate::workdir_fs::WorkdirRoot::open(&path).unwrap()
    };
    crate::runtime_dir::RuntimeDirRoots::new(
        open(crate::runtime_dir::RUNTIME_SUBDIR),
        open(crate::runtime_dir::SEALED_ETC_SUBDIR),
        open(crate::runtime_dir::HARNESS_SUBDIR),
    )
    .with_records(open(crate::records_dir::RECORDS_DIR_NAME))
    .with_temp(open(&format!(
        "{}/{}",
        crate::records_dir::RECORDS_DIR_NAME,
        crate::temp_dir::SESSION_TEMP_DIR_NAME
    )))
    .with_home(open(&format!(
        "{}/{}",
        crate::records_dir::RECORDS_DIR_NAME,
        crate::records_dir::HOME_SUBDIR
    )))
}

/// Attach the OS-level *process* bounds — hard rlimits, and cgroup membership where a scope
/// exists — to `command`, so they apply to the spawned process and everything it forks.
///
/// Deliberately independent of the tier machinery above. Landlock and seccomp are Linux kernel
/// primitives that exist on some hosts; `setrlimit` is POSIX and exists on all of them, and the
/// cgroup self-move is a single `write(2)` to a descriptor the parent already opened. So this is
/// the whole of `prepare_enforcement` on `EnvironmentOnly` and off Linux, through which every
/// shell command, plan shell step and native tool is spawned.
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
    let nproc = enforcement.nproc_bound();
    let cgroup_scope = enforcement.cgroup_scope.clone();

    // SAFETY: the closure runs in the forked child before `execve`, where only async-signal-safe
    // operations are permitted. It performs `getrlimit`/`setrlimit` pairs and, when a cgroup
    // scope exists, a single `write` to a descriptor opened by the parent before the fork —
    // no allocation, no locks, no path lookups.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            crate::resources::apply_hard_rlimits(&limits, nproc)?;
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
/// **Stdio is NOT marked close-on-exec, and is NOT reopened.** The `Stdio::piped()` handles a
/// spawn installs *before* `pre_exec` runs exist so the exec'd program inherits them across its
/// own `execve` and talks to the parent over them — `execute_shell` reads the subprocess's
/// stdout/stderr, and `dispatch_native_tool` additionally writes its `ToolInput` JSON to the
/// subprocess's stdin and parses its `ToolResult` JSON back off stdout. Marking 0/1/2
/// close-on-exec would leave the exec'd program with those descriptors closed, breaking every
/// invocation, and there is nothing meaningful to reopen them onto: the pipes themselves are the
/// intended destination. The gap this constant closes is everything *above*
/// stdio, where inheritance was never intended and was only ever prevented by the empirical
/// accident of nothing else happening to be open at spawn time.
///
/// Its only non-test reader is Linux-only (`linux_enforce::mark_inherited_fds_cloexec`), so this
/// carries the same cross-platform `dead_code` exception the other Linux-consumed items in this
/// module carry.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) const FD_HYGIENE_FIRST_FD: u32 = 3;

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
    _accessible_workdir: &Path,
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
/// submodule for the mechanics: the session's network namespace is established in the parent,
/// and the `pre_exec` closure sets `RLIMIT_NPROC` or joins the cgroup scope, then joins the
/// namespace, then maps `/tmp` or installs the composed root, then seccomp and Landlock, and
/// applies the per-process rlimits last.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(crate) fn prepare_enforcement(
    command: &mut std::process::Command,
    enforcement: &ShellEnforcement,
    accessible_workdir: &Path,
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

    // The session's temporary folder, reopened from its root's held descriptor: its Landlock
    // rule's fd on both Landlock tiers, and the identity the sealed bind and the mount-namespace
    // bind both verify, so a link swapped onto its path redirects neither.
    let temp_dir_fd = enforcement
        .runtime_dir
        .as_ref()
        .and_then(|roots| roots.temp.as_ref())
        .map(|temp| temp.path_fd().map(Arc::new))
        .transpose()
        .map_err(|error| {
            format!("sandbox: failed to open the session's temporary folder: {error}")
        })?;
    // The session's home folder, on exactly the temporary folder's terms: its Landlock rule's fd
    // on both Landlock tiers and the identity its sealed bind verifies.
    let home_dir_fd = enforcement
        .home_root()
        .map(|home| home.path_fd().map(Arc::new))
        .transpose()
        .map_err(|error| format!("sandbox: failed to open the session's home folder: {error}"))?;

    // The delegated children's records, opened only once a child has staged there, with no link
    // at the last component followed: a read-only rule on both Landlock tiers and a read-only
    // bind on `KernelSealed`, so a parent's agent reads the result an outcome names.
    let children_records_fd = enforcement
        .children_records_path()
        .map(|path| open_directory_no_follow(path).map(|fd| (Arc::new(fd), path.to_path_buf())))
        .transpose()?;

    // The descriptors a rule and a bind share on `KernelSealed`: the temporary folder and the
    // runtime and records directories, each taken once, with no link followed, from a held
    // descriptor. The Landlock rule is taken on the descriptor, and the bind is refused unless
    // the directory the child reaches is the one it names, so a link swapped in afterwards
    // changes neither.
    let sealed_sources = if enforcement.tier == EnforcementTier::KernelSealed {
        Some(SealedSourceFds::open(
            enforcement,
            temp_dir_fd.as_ref(),
            home_dir_fd.as_ref(),
            children_records_fd.clone(),
        )?)
    } else {
        None
    };
    // On `KernelFull` the runtime directory needs the Landlock rule alone.
    let runtime_dir_fd = match &sealed_sources {
        Some(sources) => Some(Arc::clone(&sources.runtime_dir)),
        None if enforcement.tier == EnforcementTier::KernelFull => enforcement
            .runtime_dir
            .as_ref()
            .map(|roots| roots.runtime.path_fd().map(Arc::new))
            .transpose()
            .map_err(|error| {
                format!(
                    "sandbox: failed to open the runtime directory for Landlock scoping: {error}"
                )
            })?,
        None => None,
    };
    // The records directory takes the runtime directory's rule on both Landlock tiers, from the
    // same descriptor its sealed bind verifies.
    let records_dir_fd = match &sealed_sources {
        Some(sources) => sources.records_dir.as_ref().map(|(fd, _)| Arc::clone(fd)),
        None if enforcement.tier == EnforcementTier::KernelFull => enforcement
            .runtime_dir
            .as_ref()
            .and_then(|roots| roots.records.as_ref())
            .map(|records| records.path_fd().map(Arc::new))
            .transpose()
            .map_err(|error| {
                format!(
                    "sandbox: failed to open the records directory for Landlock scoping: {error}"
                )
            })?,
        None => None,
    };

    // The private `/tmp` of a `MountNamespace` spawn, every path and buffer of it built here.
    let tmp_map_spec = match (&temp_dir_fd, enforcement.installed_temp_mapping()) {
        (Some(folder_fd), crate::temp_dir::TempMapping::MountNamespace) => Some(
            build_tmp_map_spec(enforcement, folder_fd, accessible_workdir)?,
        ),
        _ => None,
    };

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
            linux_enforce::LandlockFdRequest {
                accessible_workdir,
                landlock_grants: &enforcement.landlock_grants,
                device_grants: &landlock_device_grants(enforcement.tier),
                workdir_exec: enforcement.workdir_exec,
                abi: enforcement.landlock_abi,
                sealed: sealed_sources.as_ref(),
                temp_dir_fd,
                home_dir_fd,
                runtime_dir_fd,
                records_dir_fd,
                children_records_fd: children_records_fd.map(|(fd, _)| fd),
            },
        )?)
    } else {
        None
    };

    // Resolve the whole composed root in the PARENT for the same reason the Landlock fds are
    // opened here: every path lookup, every `PathBuf::join` and every allocation the child would
    // otherwise perform inside `pre_exec` — the window where the allocator lock may be held by a
    // thread that no longer exists — happens now instead, synchronously, where a failure names
    // itself. See `crate::sealed` for the plan/execute split.
    let sealed_spec = match &sealed_sources {
        Some(sources) => Some(build_sealed_root(
            accessible_workdir,
            sources,
            &enforcement.sealed_bind_dirs,
            &enforcement.staged_runtime_dirs,
        )?),
        None => None,
    };

    // Dedicated CLOEXEC pipe carrying failure detail out of the child. The write end is moved
    // into the `pre_exec` closure; because it is CLOEXEC, a successful `execve` closes it with
    // nothing written (the parent's read then returns immediate EOF — zero cost on the success
    // path), while any `pre_exec` setup failure writes its real message here before the closure
    // returns `Err`.
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

    // The session's network namespace, created by the first command that needs it and joined by
    // every one after. Established here, in the parent and before the fork, so the egress proxy
    // is already serving the namespace's listeners when the command's first connection arrives,
    // and so a namespace that cannot be created fails this spawn with a legible error instead of
    // running the command anywhere else. The join handle moves into the closure, which keeps both
    // descriptors open until the `Command` is dropped.
    let netns = enforcement.session_netns.establish(
        &enforcement.egress_tcp_ports,
        crate::egress_proxy::EgressPolicy::new(
            enforcement.network_allow_rules.clone(),
            enforcement.network_allow_ips.clone(),
        ),
    )?;

    let tier = enforcement.tier;
    // Copied out before the `move` closure below takes ownership of everything it touches, the
    // same clone-before-move shape `tier` and the Landlock fds already use.
    let unix_sockets_allowed = enforcement.unix_sockets_allowed;
    // Same clone-before-move shape. `HostResourceLimits` and `NprocBound` are `Copy`, and the
    // `RLIMIT_NPROC` decision is made here because its baseline is a `/proc` walk; the cgroup scope
    // is an `Arc` whose `cgroup.procs` descriptor was opened in the parent at scope-creation time,
    // so the child performs no path lookup to join it.
    let resource_limits = enforcement.resource_limits;
    let nproc = enforcement.nproc_bound();
    let cgroup_scope = enforcement.cgroup_scope.clone();

    // SAFETY: this closure runs in the forked child, after fork() but before execve() — the
    // narrow pre_exec window where only async-signal-safe operations are permitted. In order, it
    // performs: without a cgroup scope, the `RLIMIT_NPROC` `getrlimit`/`setrlimit` pair of
    // `resources::apply_nproc_bound`; with one, one `write` to the `cgroup.procs` descriptor the
    // parent opened; two `setns` calls into the session's namespaces; on a `MountNamespace` spawn
    // the `unshare`/`mount`/`open`/`mkdirat`/`chdir` sequence of `temp_dir::map_private_tmp`, and
    // on `KernelSealed` the `unshare`/`open`/`write`/`mount`/`pivot_root` sequence of
    // `sealed::construct_composed_root`, both over paths and buffers built here; the
    // `close_range` fd-hygiene call; the capability-dropping `prctl`/`capset` sequence
    // (bounding-set drop, capset clear, and the explicit `no_new_privs` `prctl`) plus one
    // allocation-free `open`/`read`/`close` of `/proc/sys/kernel/cap_last_cap`; libseccomp filter
    // construction/load (kernel syscalls); on `KernelFull`/`KernelSealed` Landlock ruleset
    // construction/`restrict_self` against already-open fds (also kernel syscalls); and last, the
    // `getrlimit`/`setrlimit` pairs of `resources::apply_per_process_rlimits`. No `canonicalize()`, no
    // path lookup beyond those prepared paths and `cap_last_cap`, no locks beyond what those
    // syscalls need. On any failure it writes the real error message to the CLOEXEC diagnostic
    // pipe (best-effort, bounded, raw `write` loop) before returning `Err`. `diag_write`, the
    // namespace join handle and the Landlock fds are moved in, and are `CLOEXEC`, so none of them
    // reaches the exec'd program.
    unsafe {
        command.pre_exec(move || {
            // `RLIMIT_NPROC` first: the composed root's `unshare(CLONE_NEWUSER)` below fixes the
            // session namespace's task bound at this child's limit as of that call, and a later
            // `setrlimit` would bound only the new namespace's own tasks. It is set only without
            // a cgroup scope (`NprocBound::Headroom`) and creates no descriptor.
            if let Err(error) = crate::resources::apply_nproc_bound(&resource_limits, nproc) {
                linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                return Err(error);
            }
            if let Some(scope) = cgroup_scope.as_ref() {
                if let Err(error) = scope.join_current_process() {
                    linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                    return Err(error);
                }
            }

            let (user_ns_fd, net_ns_fd) = netns.raw_fds();
            if let Err(error) = linux_enforce::child_install_enforcement(
                tier,
                user_ns_fd,
                net_ns_fd,
                tmp_map_spec.as_ref(),
                sealed_spec.as_ref(),
                landlock_fds.as_ref(),
                unix_sockets_allowed,
            ) {
                linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                return Err(error);
            }

            // The per-process rlimits come last, as the final call before `execve`. They cannot
            // come first: every descriptor the steps above create (the `/tmp` mapping's `O_PATH`
            // fds, the composed root's `/proc/self/*_map` writes and `MkFile` steps,
            // `cap_last_cap`, the Landlock ruleset) must be numbered below `RLIMIT_NOFILE`, and
            // this child holds every descriptor `mur` had open at the fork, so the lowest free
            // number can be above a modest `max_open_files`. Last still bounds the exec'd program
            // from its first instruction: a step above that fails returns `Err`, which aborts the
            // exec, so no capsule code runs in this child; a failing `setrlimit` aborts it the
            // same way; and `prlimit64`/`getrlimit`/`setrlimit` are in
            // `SECCOMP_SYSCALL_ALLOWLIST`, and lowering a limit needs no capability, so neither
            // the filter nor the capability drop stops them.
            if let Err(error) = crate::resources::apply_per_process_rlimits(&resource_limits) {
                linux_enforce::write_diagnostic(diag_write.as_raw_fd(), &error.to_string());
                return Err(error);
            }
            Ok(())
        });
    }

    Ok(SupervisorHandle::Linux { diag_read })
}

/// Which fixed device set Landlock grants, keyed on the tier.
///
/// Landlock keeps mediating inside a composed root, so the two device *lists* have to agree: a
/// device present in the sealed `/dev` but absent from the Landlock rules would exist and be
/// unopenable, which reads as a runtime bug rather than as policy. `KernelFull` keeps
/// [`CAPSULE_DEVICE_GRANTS`], which services `scoped`, while `KernelSealed` derives its own from
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

/// The `/tmp` mapping of one `MountNamespace` spawn: the folder held by `folder_fd` bound over
/// canonical `/tmp`, and every path beneath the host's `/tmp` the command still needs re-exposed
/// at its own path.
///
/// Those paths are the accessible workdir and the session's home folder, bound read-write, and the runtime and
/// records directories and every Landlock grant path, which already carries the `shell.allow`
/// closure, the interpreter and staged runtimes and a native tool's entrypoint closure, bound
/// read-only. Landlock adds the folder's rights to everything bound beneath it, which are the
/// workdir's own; a read-only bind is what keeps them from writing the rest. See
/// [`crate::temp_dir::reexposed_paths`].
#[cfg(target_os = "linux")]
fn build_tmp_map_spec(
    enforcement: &ShellEnforcement,
    folder_fd: &std::os::fd::OwnedFd,
    accessible_workdir: &Path,
) -> Result<crate::temp_dir::TmpMapSpec, String> {
    let folder = enforcement.temp_dir_path().ok_or_else(|| {
        format!(
            "{} this enforcement holds none",
            crate::temp_dir::TMP_MAP_FAILURE_PREFIX
        )
    })?;
    let identity = crate::workdir_fs::fd_identity(folder_fd).map_err(|errno| {
        format!(
            "{} fstat of the temporary folder failed: {errno}",
            crate::temp_dir::TMP_MAP_FAILURE_PREFIX
        )
    })?;
    let canonical_tmp = crate::temp_dir::canonical_tmp();
    let read_only = [
        enforcement.runtime_dir_path(),
        enforcement.records_dir_path(),
        enforcement.children_records_path(),
    ]
    .into_iter()
    .flatten()
    .chain(
        enforcement
            .landlock_grants
            .iter()
            .map(|grant| grant.path.as_path()),
    );
    let reexposed = crate::temp_dir::reexposed_paths(
        &canonical_tmp,
        std::iter::once(accessible_workdir).chain(enforcement.home_dir_path()),
        read_only,
    );
    crate::temp_dir::TmpMapSpec::new(
        folder,
        identity,
        &canonical_tmp,
        &reexposed,
        accessible_workdir,
    )
}

/// `path`, a directory, opened `O_PATH` with no link followed at its last component: the
/// descriptor a rule is taken on and a bind is verified against.
#[cfg(target_os = "linux")]
fn open_directory_no_follow(path: &Path) -> Result<std::os::fd::OwnedFd, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .map(std::os::fd::OwnedFd::from)
        .map_err(|error| format!("sandbox: failed to open {}: {error}", path.display()))
}

/// Writes (idempotently) the synthetic `/etc/passwd`/`/etc/group` the composed root binds in place
/// of the host's into the session's `sealed-etc/` directory, returning their host paths in
/// [`crate::sealed::SEALED_ETC_PATHS`] order.
///
/// `sealed-etc/` sits beside the runtime directory, outside every workdir, and no capsule command
/// is granted it. The account the files name is the home folder's owner, the identity the
/// capsule's subprocesses run as, with the session's home folder as its home directory.
///
/// Runs in the parent, never in `pre_exec`: composing a root is the child's job, but *producing
/// content* is work the parent can do, and this module's rule is that the `pre_exec` window does
/// nothing the parent could have done for it.
///
/// The rewrite is in place ([`crate::workdir_fs::WorkdirRoot::overwrite_in_place`] truncates; it
/// does not replace): Landlock grants the file by inode and the composed root binds it by path, so
/// replacing the file would leave `/etc/passwd` bound but denied (`EACCES`).
#[cfg(target_os = "linux")]
fn ensure_sealed_identity_files(
    sealed_etc_root: &crate::workdir_fs::WorkdirRoot,
    home_root: &crate::workdir_fs::WorkdirRoot,
) -> Result<Vec<PathBuf>, String> {
    // The home folder was created by this process, so its owner is the identity the capsule's
    // subprocesses run as — no `getuid(2)` FFI needed under `#![deny(unsafe_code)]`.
    let (uid, gid) = home_root.owner().map_err(|error| {
        format!(
            "sealed: failed to stat the session's home folder for the capsule's uid/gid: {error}"
        )
    })?;
    // The same string `build_shell_env` puts in `$HOME`: the home folder's own absolute path,
    // which the composed root binds at that same path.
    let home = home_root.path().to_string_lossy().into_owned();
    let identity = crate::sealed::SealedAccountIdentity {
        uid,
        gid,
        home: &home,
    };

    let mut written = Vec::new();
    for entry in crate::sealed::SEALED_ETC_PATHS {
        let Some(file) = entry.synthetic else {
            continue;
        };
        sealed_etc_root
            .overwrite_in_place(
                file.file_name(),
                file.render(&identity)?.as_bytes(),
                crate::workdir_fs::DEFAULT_FILE_MODE,
            )
            .map_err(|error| {
                format!(
                    "sealed: failed to write the synthetic {}: {error}",
                    entry.path
                )
            })?;
        written.push(crate::sealed::synthetic_etc_source(
            sealed_etc_root.path(),
            file,
        ));
    }
    Ok(written)
}

/// The descriptors a composed root's runtime-provided binds are verified against and its Landlock
/// rules are taken on, opened in the parent before the fork.
#[cfg(target_os = "linux")]
pub(crate) struct SealedSourceFds {
    /// The session's temporary folder, reopened from its root's held descriptor: the `/tmp`
    /// rule's fd.
    tmp_store: Arc<std::os::fd::OwnedFd>,
    /// The folder's host path, which the child opens and verifies against [`Self::tmp_store`].
    tmp_store_path: PathBuf,
    /// The session's home folder, reopened from its root's held descriptor, and its absolute
    /// path: its rule's fd and its read-write bind at that same path.
    home_dir: (Arc<std::os::fd::OwnedFd>, PathBuf),
    /// The runtime directory, reopened from its root's held descriptor: its rule's fd.
    runtime_dir: Arc<std::os::fd::OwnedFd>,
    /// The runtime directory's absolute path, which the bind reproduces inside the root and the
    /// child verifies against [`Self::runtime_dir`].
    runtime_dir_path: PathBuf,
    /// The records directory, reopened from its root's held descriptor, and its absolute path:
    /// its rule's fd and its read-only bind, on the runtime directory's terms. `None` for an
    /// enforcement held outside a session.
    records_dir: Option<(Arc<std::os::fd::OwnedFd>, PathBuf)>,
    /// The delegated children's records, and its absolute path: its rule's fd and its read-only
    /// bind. `None` until a child has staged there.
    children_records: Option<(Arc<std::os::fd::OwnedFd>, PathBuf)>,
    /// `sealed-etc/`'s host path.
    etc_dir: PathBuf,
    /// The synthetic `/etc` files, in [`crate::sealed::SEALED_ETC_PATHS`] order.
    identity_files: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl SealedSourceFds {
    /// Writes the synthetic `/etc` files and takes every descriptor, with `temp_dir_fd` and
    /// `home_dir_fd` the descriptors `prepare_enforcement` took on the session's temporary and
    /// home folders. A sealed root cannot be composed for an enforcement that holds no runtime
    /// directory, no temporary folder or no home folder.
    fn open(
        enforcement: &ShellEnforcement,
        temp_dir_fd: Option<&Arc<std::os::fd::OwnedFd>>,
        home_dir_fd: Option<&Arc<std::os::fd::OwnedFd>>,
        children_records: Option<(Arc<std::os::fd::OwnedFd>, PathBuf)>,
    ) -> Result<Self, String> {
        let roots = enforcement.runtime_dir.as_ref().ok_or_else(|| {
            "sealed: a composed root needs the session's runtime directory, and this \
             enforcement holds none"
                .to_string()
        })?;
        let (Some(temp), Some(tmp_store)) = (roots.temp.as_ref(), temp_dir_fd) else {
            return Err(
                "sealed: a composed root binds the session's temporary folder at /tmp, \
                        and this enforcement holds none"
                    .to_string(),
            );
        };
        let tmp_store = Arc::clone(tmp_store);
        let (Some(home), Some(home_fd)) = (roots.home.as_ref(), home_dir_fd) else {
            return Err(crate::shell::NO_HOME_FOLDER_REFUSAL.to_string());
        };
        let home_dir = (Arc::clone(home_fd), home.path().to_path_buf());
        let identity_files = ensure_sealed_identity_files(&roots.sealed_etc, home)?;
        let runtime_dir = Arc::new(roots.runtime.path_fd().map_err(|error| {
            format!("sealed: failed to open the runtime directory for its bind: {error}")
        })?);
        let records_dir = roots
            .records
            .as_ref()
            .map(|records| {
                records
                    .path_fd()
                    .map(|fd| (Arc::new(fd), records.path().to_path_buf()))
                    .map_err(|error| {
                        format!(
                            "sealed: failed to open the records directory for its bind: {error}"
                        )
                    })
            })
            .transpose()?;
        Ok(Self {
            tmp_store,
            tmp_store_path: temp.path().to_path_buf(),
            home_dir,
            runtime_dir,
            runtime_dir_path: roots.runtime.path().to_path_buf(),
            records_dir,
            children_records,
            etc_dir: roots.sealed_etc.path().to_path_buf(),
            identity_files,
        })
    }

    /// `(st_dev, st_ino)` of `fd`, which a bind's source must still have in the child.
    fn identity_of(fd: &std::os::fd::OwnedFd) -> Result<(u64, u64), String> {
        crate::workdir_fs::fd_identity(fd)
            .map_err(|errno| format!("sealed: fstat of a bind source failed: {errno}"))
    }
}

/// Parent-side half of the sealed mechanism: pick a base, plan the composed root against the real
/// host layout, and lower the plan into the C-string form the forked child executes.
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
    accessible_workdir: &Path,
    sources: &SealedSourceFds,
    extra_read_only: &[PathBuf],
    staged_runtime_read_only: &[PathBuf],
) -> Result<crate::sealed::SealedRootSpec, String> {
    use crate::sealed;

    // The workdir must be absolute for its path to mean the same thing inside the composed root,
    // and `launch_session` always creates it as one — but the composed root's whole design rests
    // on it, so it is checked rather than assumed.
    if !accessible_workdir.is_absolute() {
        return Err(format!(
            "sealed: capsule workdir {} is not absolute; a composed root reproduces the workdir at \
             its own absolute path",
            accessible_workdir.display()
        ));
    }

    let base = sealed::choose_root_base(
        accessible_workdir,
        sealed::SEALED_ROOT_BASE_CANDIDATES,
        |path| path.is_dir(),
    )
    .ok_or_else(|| {
        format!(
            "sealed: no usable base directory for the composed root; none of {:?} exists outside \
             the session workdir's own path",
            sealed::SEALED_ROOT_BASE_CANDIDATES
        )
    })?;

    let plan = sealed::plan_composed_root(
        accessible_workdir,
        &sealed::SealedBindSources {
            tmp_store: (
                &sources.tmp_store_path,
                SealedSourceFds::identity_of(&sources.tmp_store)?,
            ),
            etc_dir: &sources.etc_dir,
            runtime_dir: Some((
                &sources.runtime_dir_path,
                SealedSourceFds::identity_of(&sources.runtime_dir)?,
            )),
            records_dir: sources
                .records_dir
                .as_ref()
                .map(|(fd, path)| {
                    SealedSourceFds::identity_of(fd).map(|identity| (path.as_path(), identity))
                })
                .transpose()?,
            home_dir: Some((
                &sources.home_dir.1,
                SealedSourceFds::identity_of(&sources.home_dir.0)?,
            )),
            children_records: sources
                .children_records
                .as_ref()
                .map(|(fd, path)| {
                    SealedSourceFds::identity_of(fd).map(|identity| (path.as_path(), identity))
                })
                .transpose()?,
        },
        &base,
        extra_read_only,
        staged_runtime_read_only,
        &sealed::RealHostLayout,
    );
    sealed::build_sealed_root_spec(&plan)
}

/// Linux-only mechanics: the `pre_exec` enforcement sequence, seccomp filter construction and
/// Landlock application.
///
/// Nothing here uses `libseccomp`'s notify facility. `connect`/`sendto` are enforced by the
/// network namespace and `execve`/`execveat` by Landlock `Execute` rights, both of which decide
/// from kernel-held state. A `Notify` rule would reintroduce a `/proc/<pid>/mem` read on the hot
/// path, where the value read can change between the decision and the kernel's own dereference.
#[cfg_attr(target_os = "linux", allow(unsafe_code))]
#[cfg(target_os = "linux")]
mod linux_enforce {
    use std::io;
    use std::os::fd::{AsFd, OwnedFd, RawFd};
    use std::path::Path;

    use landlock::{make_bitflags, AccessFs, BitFlags};

    use super::{EnforcementTier, LandlockGrant};

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
    /// workdir's fd and the session's temporary folder's (both scoped by
    /// [`workdir_access_rights`]), each successfully-opened grant fd, and each
    /// successfully-opened fixed-device fd.
    /// Handed by reference into the child's `pre_exec`, where `apply_landlock_scope` builds rules
    /// against these fds without performing a single `open()`.
    pub(super) struct LandlockChildFds {
        workdir_fd: OwnedFd,
        /// The capsule's `capabilities.filesystem.workdir_exec`, carried alongside the fd it
        /// applies to so `apply_landlock_scope` — which runs in `pre_exec` and must read nothing
        /// but its argument — has the whole workdir rule decided for it before fork.
        workdir_exec: bool,
        /// The Landlock ABI the ruleset is built at, decided in the parent from the session's
        /// [`super::HostProbe`] (see [`super::ShellEnforcement::landlock_abi`]) so the child reads
        /// no host state of its own. `None` builds at ABI v1, the narrowest set.
        abi: Option<u8>,
        /// The session's temporary folder, `tmp/` in its records directory, on both Landlock
        /// tiers whenever the enforcement holds one, mapped onto `/tmp` or not. `None` outside a
        /// session. On `KernelSealed` the same descriptor is the composed root's `/tmp` bind
        /// source.
        ///
        /// It needs a rule of its own on every mapped tier: Landlock resolves a path by walking
        /// dentries up to each mount root, so an access to `/tmp/x` is matched against the folder
        /// (the bind's root) and then against the mountpoint `/tmp` — never against the records
        /// directory or the workdir. It carries the workdir's own right-set, `Execute` axis
        /// included, so `workdir_exec: false` forbids running anything written there.
        temp_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The session's home folder, `home/` in its records directory, on exactly
        /// [`Self::temp_dir_fd`]'s terms: the workdir's rights, on both Landlock tiers, whenever the
        /// enforcement holds one. On `KernelSealed` the same descriptor is the read-write bind's
        /// source.
        home_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The synthetic `/etc/passwd`/`/etc/group` in the session's `sealed-etc/`, opened on
        /// `KernelSealed` and empty on every other tier.
        ///
        /// Exactly the [`Self::temp_dir_fd`] problem, one directory over. The grant list already
        /// carries `/etc/passwd`, but that rule is taken on the fd opened at the *host's*
        /// `/etc/passwd` — a different inode from the file the composed root actually binds there.
        /// Landlock matches inodes, not path strings, and the walk from a bind-mounted file starts
        /// at the bound file's own inode, so without these fds both account databases would be
        /// mounted and `EACCES`: `getpwuid(3)` fails, and a shell cannot expand `~`.
        ///
        /// Read-only, like every other `SEALED_ETC_PATHS` rule — the mount is `MS_RDONLY` too, and
        /// nothing inside a capsule has any business editing an account database.
        sealed_identity_fds: Vec<OwnedFd>,
        /// The session's runtime directory, reopened from its root's held descriptor: `ReadFile`
        /// and `ReadDir` and nothing else, so every command reads `MURMUR.md`, `task.md`, plans
        /// and installed tools and changes none of them. `None` outside a session. On
        /// `KernelSealed` the same descriptor is the read-only bind's source.
        runtime_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The session's records directory, reopened from its root's held descriptor, granted on
        /// [`Self::runtime_dir_fd`]'s terms: every command reads the trace and logs and changes
        /// none of them. `None` outside a session. On `KernelSealed` the same descriptor is the
        /// read-only bind's source.
        records_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The delegated children's records, granted on [`Self::records_dir_fd`]'s terms, once a
        /// child has staged there.
        children_records_fd: Option<std::sync::Arc<OwnedFd>>,
        grants: Vec<OpenLandlockGrant>,
        devices: Vec<OpenDeviceGrant>,
    }

    impl LandlockChildFds {
        /// The temporary folder's fd, for the tier-gate test. Keeps the struct's fields private
        /// like every other one: nothing outside `apply_landlock_scope` needs the fd itself.
        #[cfg(test)]
        pub(super) fn temp_dir_fd(&self) -> Option<&OwnedFd> {
            self.temp_dir_fd.as_deref()
        }

        /// The home folder's fd, for the tier-gate test, on the same terms.
        #[cfg(test)]
        pub(super) fn home_dir_fd(&self) -> Option<&OwnedFd> {
            self.home_dir_fd.as_deref()
        }

        /// The runtime directory's fd, for the tier-gate test, on the same terms.
        #[cfg(test)]
        pub(super) fn runtime_dir_fd(&self) -> Option<&OwnedFd> {
            self.runtime_dir_fd.as_deref()
        }

        /// The records directory's fd, for the tier-gate test, on the same terms.
        #[cfg(test)]
        pub(super) fn records_dir_fd(&self) -> Option<&OwnedFd> {
            self.records_dir_fd.as_deref()
        }

        /// The delegated children's records' fd, for the tier-gate test, on the same terms.
        #[cfg(test)]
        pub(super) fn children_records_fd(&self) -> Option<&OwnedFd> {
            self.children_records_fd.as_deref()
        }

        /// How many synthetic-`/etc` fds were opened, for the tier-gate test. A count, not the fds:
        /// the same "nothing outside `apply_landlock_scope` needs the fd itself" rule as above.
        #[cfg(test)]
        pub(super) fn sealed_identity_fd_count(&self) -> usize {
            self.sealed_identity_fds.len()
        }

        /// How many grant fds were opened, for the pinned-grant test. A count, for the same reason.
        #[cfg(test)]
        pub(super) fn grant_count(&self) -> usize {
            self.grants.len()
        }
    }

    /// The Landlock rights granted on the capsule **workdir's own** `PathBeneath` rule,
    /// *excluding* `Execute` — see [`workdir_access_rights`], which adds it back for a capsule that
    /// declared `capabilities.filesystem.workdir_exec: true`.
    ///
    /// Spelled out variant by variant rather than `AccessFs::from_all` for two reasons: `from_all`
    /// hides what is actually handed to the capsule, and four rights are withheld from this
    /// constant. It is the set for [`super::LANDLOCK_TARGET_ABI`]; `apply_landlock_scope` masks it
    /// to the ABI the ruleset is built at, so on an older kernel the rights that ABI does not know
    /// simply drop out.
    ///
    /// Two rights are granted from an ABI later than v1:
    ///
    ///   - `Refer` (ABI v2, Linux 5.19) — moving and hard-linking a file between two folders.
    ///     Landlock refuses every cross-folder `rename(2)`/`link(2)` with `EXDEV` unless the ruleset
    ///     handles `Refer` and both folders grant it, so without it every tool that renames a
    ///     finished file into place from a sibling folder fails in the workdir. It reaches nothing
    ///     outside: a move or link needs `Refer` on both the source and the destination folder, no
    ///     rule outside the workdir and the session's temporary folder carries it, and Landlock
    ///     also refuses any move that would leave the file with more rights at its destination
    ///     than its source.
    ///   - `Truncate` (ABI v3, Linux 6.2) — `truncate(2)`, `ftruncate(2)` and `open(O_TRUNC)`.
    ///     Below v3 Landlock does not mediate truncation at all, so a capsule could zero any host
    ///     file its uid can write. Handling it denies that everywhere except the rules that grant
    ///     it, which are exactly the ones already carrying `WriteFile`: this one and the writable
    ///     devices.
    ///
    ///   - `Execute` — the one this constant withholds *conditionally*. Withholding it is what
    ///     makes `capabilities.shell.allow` a complete, sound statement: Landlock refuses the exec
    ///     on the path the kernel itself resolved, so a binary the capsule wrote into its workdir
    ///     cannot run under any name — including an allowlisted basename, the exact bypass a prior
    ///     release shipped. The cost: a binary the capsule legitimately compiled in its workdir
    ///     cannot run either, which is what `workdir_exec: true` buys back.
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
    ///     that breaks those is not a fix. The manual acceptance procedure in
    ///     `docs/content/reference/workdir-device-node-manual-verification.md` carries an
    ///     explicit unix-socket scenario for exactly this; if the team's
    ///     real-hardware run finds a workload that needs it, add `MakeSock` back to this list.
    ///
    /// `MakeFifo` stays granted: real build tooling does create named pipes in its working tree,
    /// and a FIFO carries none of the raw-device risk above.
    ///
    /// Because `apply_landlock_scope`'s `handle_access` declares the *full* `from_all` set of the
    /// ABI it builds at, a right this rule withholds is not merely "not extra-granted" — it
    /// is **denied on every path this rule covers**, workdir included, once `restrict_self()` takes
    /// effect — so withholding `Execute` here is enforcement, not a mere omission. The three
    /// `Make*` rights are denied domain-wide because no other rule grants them either; `Execute`
    /// is denied *on the workdir specifically*, while the narrow read+execute grants below still
    /// carry it for the allowlisted binaries outside it: exec where the operator named a binary,
    /// nowhere the capsule writes.
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
            | Refer
            | Truncate
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

    /// The kernel's Landlock ABI version, or `None` when the kernel has no Landlock.
    ///
    /// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` returns the version and
    /// creates nothing — no ruleset fd and no domain — so unlike
    /// [`probe_landlock_full_access`] it is safe to call in-process. A kernel built without
    /// Landlock, or with it disabled at boot, fails it with `ENOSYS`/`EOPNOTSUPP`.
    pub(super) fn kernel_landlock_abi() -> Option<u8> {
        const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
        // SAFETY: with a null attribute pointer, a zero size and only the VERSION flag set, the
        // kernel reads no memory and allocates no fd; the call has no side effect to undo.
        let version = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if version <= 0 {
            return None;
        }
        Some(u8::try_from(version).unwrap_or(u8::MAX))
    }

    /// Probes real Landlock support by building a ruleset granting full access to `/` and
    /// calling `restrict_self()` **in a forked child**, so the host process is never placed
    /// into a Landlock domain by a mere capability check.
    ///
    /// Forking is required, not defensive. The rights the ruleset grants are beside the point:
    /// entering *any* Landlock domain — even one permitting everything — permanently forbids the
    /// mount family to the task and every descendant it will ever have, which is how Landlock
    /// stops a sandbox being escaped through a nested namespace. [`HostProbe::probe`] runs this
    /// before the sealed probe, and a session's probe is taken before any fork that would
    /// build a composed root, so restricting in-process poisons `sealed` on every host and every
    /// invocation: the namespace is created and then `mount` is refused, with nothing in the error
    /// to suggest the runtime did it to itself.
    ///
    /// The ruleset is built in the parent because creating one and adding rules restricts
    /// nothing; only `restrict_self` applies it. That keeps everything the child does after
    /// the fork down to a `prctl` and one syscall.
    pub(super) fn probe_landlock_full_access() -> Option<bool> {
        use landlock::{
            Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
            RulesetCreatedAttr, RulesetStatus, ABI,
        };

        // The child reports which of the three outcomes it reached through its exit status;
        // nothing else crosses the fork.
        const PROBE_FULLY_ENFORCED: i32 = 0;
        const PROBE_PARTIAL: i32 = 1;
        const PROBE_FAILED: i32 = 2;

        // ABI v1 on purpose: this decides whether a Landlock tier is available at all, which v1
        // answers on every Landlock kernel. The ABI a capsule's ruleset is built at is read
        // separately, by `kernel_landlock_abi`.
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

    /// Runs inside the forked child, pre-exec: joins the session's network namespace through
    /// `user_ns_fd`/`net_ns_fd`, maps `/tmp` onto the session's temporary folder when
    /// `tmp_map_spec` is given, strips the child's Linux capabilities, installs the seccomp filter
    /// (the `socket()` domain denials keyed on `unix_sockets_allowed`, over a default-deny syscall
    /// allowlist), and (on `KernelFull`/`KernelSealed`) applies the Landlock filesystem scope —
    /// which is also what enforces `capabilities.shell.allow`. Returning `Err` here aborts the
    /// exec (std's `Command` machinery propagates it back to the parent's `.spawn()` call as an
    /// `io::Error`) — the fail-closed path for setup failures that happen after fork.
    pub(super) fn child_install_enforcement(
        tier: EnforcementTier,
        user_ns_fd: RawFd,
        net_ns_fd: RawFd,
        tmp_map_spec: Option<&crate::temp_dir::TmpMapSpec>,
        sealed_spec: Option<&crate::sealed::SealedRootSpec>,
        landlock_fds: Option<&LandlockChildFds>,
        unix_sockets_allowed: bool,
    ) -> io::Result<()> {
        // FIRST, on every kernel tier: join the session's network namespace. It has to precede
        // everything below it for three separate reasons:
        //
        //   * `SECCOMP_MUST_STAY_DENIED` denies `setns`, so the filter installed further down
        //     closes the only window in which the join can happen — the same argument the
        //     composed root makes for `unshare`.
        //   * joining the user namespace hands this child every capability *inside* it, and
        //     `drop_all_capabilities` (below) strips them again before `execve`. Joining after
        //     that drop would fail: entering the network namespace needs `CAP_SYS_ADMIN` there.
        //   * on `KernelSealed`, the composed root's own `unshare(CLONE_NEWUSER)` nests its user
        //     namespace inside the session's. A task in a descendant user namespace holds no
        //     capability in the ancestor, so the capsule cannot reconfigure the network namespace
        //     it is confined to — which only holds if the join comes first.
        //
        // There is no path that skips it and falls back: a host that cannot create the namespace
        // was refused at launch (`RuntimeError::EgressNamespaceUnavailable`, `E-CAP-005`), and a
        // spawn whose namespace could not be established failed before the fork.
        //
        // SAFETY: post-`fork()`, pre-exec child context — exactly the window this function
        // documents itself as running in, and the one `join_session_netns` requires. Both
        // descriptors are held open by the join handle the `pre_exec` closure owns.
        unsafe { crate::network_namespace::join_session_netns(user_ns_fd, net_ns_fd) }.map_err(
            |error| {
                io::Error::other(format!(
                    "sandbox: joining this session's network namespace failed: {error}"
                ))
            },
        )?;

        // NEXT, and only on a `MountNamespace` spawn: a private mount namespace, owned by the
        // session's user namespace, in which the session's temporary folder is bound over `/tmp`.
        // Here for the composed root's reason: the capabilities the join above handed out are
        // still held, and the seccomp filter below denies `unshare` and `mount`. A mapping that
        // cannot be built fails the spawn; nothing runs with an unmapped `/tmp` instead.
        if let Some(spec) = tmp_map_spec {
            #[cfg(test)]
            if super::FORCE_TMP_MAP_FAILURE.with(|flag| flag.get()) {
                return Err(io::Error::other(format!(
                    "{} (forced by test seam)",
                    spec.describe(crate::temp_dir::TmpMapFailure::forced(libc::EPERM))
                )));
            }
            // SAFETY: post-`fork()`, pre-exec child context, after `join_session_netns` handed
            // this child every capability in the session's user namespace and before the seccomp
            // filter — the window `map_private_tmp` requires. Every path and buffer it reads was
            // built in the parent and is borrowed from the `pre_exec` closure.
            unsafe { crate::temp_dir::map_private_tmp(spec) }
                .map_err(|failure| io::Error::other(spec.describe(failure)))?;
        }

        // NEXT, and only on `KernelSealed`: build the private mount namespace and pivot onto the
        // composed root. This has to precede every *remaining* step in this closure, because
        // `SECCOMP_MUST_STAY_DENIED` denies `unshare`, `mount` and `pivot_root` to every process
        // the filter below covers, permanently and deliberately.
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
        // Running it here is also what makes it cover this function's *own* descriptors: the
        // diagnostic pipe and the Landlock grant fds are used below but must never survive the
        // `execve`, and neither may the session's namespace handles, already used above.
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
        // (`security::harden_process_dumpable`). Nothing reads the child's memory, so nothing
        // needs it dumpable. Marking it dumpable opens same-uid `ptrace`, `environ` and core-dump
        // access for the whole life of every shell subprocess: do not do so without a reader that
        // requires it.
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
    /// still needed *inside* this `pre_exec` window — the diagnostic pipe a failure further down
    /// writes to, and the Landlock grant fds `apply_landlock_scope` builds rules from. Closing
    /// them here would break the very enforcement setup that follows; flagging them means they
    /// keep working until `execve`, which then closes them for us.
    ///
    /// The range starts at 3, never 0 — see [`super::FD_HYGIENE_FIRST_FD`] for the recorded
    /// stdio decision and its reasoning.
    ///
    /// **Kernel range narrowing:** `close_range(2)` landed in Linux 5.9 and
    /// `CLOSE_RANGE_CLOEXEC` in Linux 5.11. On an older kernel this call fails (`ENOSYS`, or
    /// `EINVAL` for the flag on 5.9/5.10), the error propagates, and `Command::spawn()` fails —
    /// so every spawn on a `KernelSeccompOnly` *or* `KernelFull` host with such a kernel
    /// now fails rather than silently running without fd hygiene. That is deliberate and it is
    /// this module's established fail-closed discipline (`drop_all_capabilities`,
    /// `install_seccomp_filter` and `apply_landlock_scope` all abort the spawn on an unexpected
    /// error rather than degrade), but it is a real, user-visible narrowing of the supported
    /// kernel range for kernel-enforcement tiers.
    pub(super) fn mark_inherited_fds_cloexec() -> io::Result<()> {
        // SAFETY: this issues the `close_range` syscall directly through `libc::syscall`. The
        // three variadic arguments are all `c_uint` scalars and the kernel dereferences none of
        // them. It is a single syscall, so it is safe to make in the post-fork/pre-exec window.
        // `c_uint::MAX` as the `last` argument is the documented "to the end of the table"
        // spelling from `close_range(2)`.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                super::FD_HYGIENE_FIRST_FD,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
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

    /// Builds the child's seccomp filter and returns it unloaded, so that the same construction
    /// the spawn path uses can also be measured. [`install_seccomp_filter`] is the only caller
    /// that loads it; the measurement of what it compiles to, the kernel ceilings that number is
    /// held against and the headroom between them are recorded in
    /// `docs/content/reference/seccomp-filter-instruction-budget.md`.
    ///
    /// **This filter raises no notifications and needs no supervisor.** Network enforcement lives
    /// in [`crate::network_namespace`] plus [`crate::egress_proxy`], and exec enforcement in the
    /// Landlock domain `apply_landlock_scope` installs — see this module's own header for why no
    /// `Notify` rule may come back.
    ///
    /// One mechanism plus a default:
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
    fn build_seccomp_filter(
        unix_sockets_allowed: bool,
    ) -> io::Result<libseccomp::ScmpFilterContext> {
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
        // but the new filter has no chains so the existing chains must be cleared"), so a broad
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

        Ok(filter)
    }

    /// Loads the filter [`build_seccomp_filter`] produces into the calling task. Irreversible:
    /// the filter stays in force for the rest of this task's life and is inherited across the
    /// `execve` that follows, which is why nothing outside a forked child ever calls this.
    fn install_seccomp_filter(unix_sockets_allowed: bool) -> io::Result<()> {
        // Dropping the context after `load()` is safe: libseccomp's `seccomp_release()` (invoked
        // on Drop) only frees the userspace filter-building context, while the filter itself is
        // already installed in the kernel and is inherited across the `execve` that follows.
        build_seccomp_filter(unix_sockets_allowed)?
            .load()
            .map_err(to_io_err)
    }

    fn to_io_err<E: std::fmt::Display>(error: E) -> io::Error {
        io::Error::other(error.to_string())
    }

    /// Size of one `struct sock_filter`, the record `seccomp_export_bpf` writes per BPF
    /// instruction. Dividing an export's length by it is what turns bytes into an instruction
    /// count.
    #[cfg(test)]
    const SOCK_FILTER_BYTES: usize = 8;

    /// Writes `export` — one of libseccomp's two exporters — into an anonymous in-memory file and
    /// returns what it wrote.
    ///
    /// Both exporters take a descriptor and nothing else, so the buffer has to come from
    /// somewhere. A `memfd` rather than a temporary file because it touches no filesystem the
    /// sandbox might already have scoped away, and because `tempfile` is a dev-dependency of this
    /// crate and so is unavailable to anything outside a test build.
    #[cfg(test)]
    fn export_to_memfd<F>(export: F) -> io::Result<Vec<u8>>
    where
        F: FnOnce(&OwnedFd) -> Result<(), libseccomp::error::SeccompError>,
    {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::fd::FromRawFd;

        // SAFETY: `memfd_create` reads a NUL-terminated name (a `c"..."` literal, static
        // lifetime) and returns a fresh descriptor or -1. No flags, so no cloexec and no sealing:
        // the descriptor lives and dies inside this function.
        let raw = unsafe { libc::memfd_create(c"murmur-seccomp-export".as_ptr(), 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor `memfd_create` just returned and nothing else owns, which
        // is exactly the precondition for handing it to `OwnedFd`.
        let memfd = unsafe { OwnedFd::from_raw_fd(raw) };

        export(&memfd).map_err(to_io_err)?;

        let mut file = std::fs::File::from(memfd);
        file.seek(SeekFrom::Start(0))?;
        let mut exported = Vec::new();
        file.read_to_end(&mut exported)?;
        Ok(exported)
    }

    /// The number of BPF instructions the filter [`build_seccomp_filter`] produces compiles to on
    /// this host, for the given `unix_sockets_allowed`.
    ///
    /// Measured, never assumed: `seccomp_export_bpf` writes the generated program as a sequence of
    /// 8-byte `struct sock_filter` records, so the instruction count is the exported length
    /// divided by that record size. The number depends on the host — how many
    /// [`super::SECCOMP_SYSCALL_ALLOWLIST`] names this architecture and this libseccomp resolve —
    /// which is why it is measured here rather than written down.
    ///
    /// `export_bpf` and not `export_bpf_mem`: the latter binds `seccomp_export_bpf_mem`, which
    /// first appeared in libseccomp 2.6.0, and a host on 2.5.x would fail to resolve the symbol.
    #[cfg(test)]
    fn seccomp_filter_instruction_count(unix_sockets_allowed: bool) -> io::Result<usize> {
        let filter = build_seccomp_filter(unix_sockets_allowed)?;
        let exported = export_to_memfd(|fd| filter.export_bpf(fd))?;
        Ok(exported.len() / SOCK_FILTER_BYTES)
    }

    /// The filter [`build_seccomp_filter`] produces, rendered as libseccomp's pseudo filter code:
    /// one block per syscall carrying a rule, each block naming the argument comparisons and the
    /// action. It is the only view of a built context that shows the rule *shape* rather than the
    /// compiled program's size, which is what a test pinning the rule sequence needs.
    #[cfg(test)]
    fn seccomp_filter_pfc(unix_sockets_allowed: bool) -> io::Result<String> {
        let filter = build_seccomp_filter(unix_sockets_allowed)?;
        let exported = export_to_memfd(|fd| filter.export_pfc(fd))?;
        String::from_utf8(exported).map_err(to_io_err)
    }

    /// Scopes the shell subprocess tree's filesystem access with Landlock. Four kinds of rule:
    ///
    ///   - the workdir gets [`workdir_access_rights`] — read/write plus directory, regular-file,
    ///     FIFO and symlink creation, but **not** `MakeChar`/`MakeBlock`/`MakeSock`, and **not**
    ///     `Execute` unless the capsule declared `capabilities.filesystem.workdir_exec: true` (see
    ///     [`WORKDIR_ACCESS_RIGHTS_NO_EXEC`] for why each right is withheld). Withholding `Execute`
    ///     is what enforces `capabilities.shell.allow` completely: the kernel refuses the exec on
    ///     the path it resolved itself, so nothing the capsule writes into its workdir can run;
    ///   - the session's temporary folder gets that **same** right-set from the same local
    ///     binding, whenever the enforcement holds one. The composed root and the mount-namespace
    ///     mapping both bind it at `/tmp` read-write, but Landlock matches an access to `/tmp/x`
    ///     against the bind's own root inode rather than the workdir's, so without this rule the
    ///     mount is writable and every access to it is nevertheless `EACCES` — bash heredocs,
    ///     `mktemp`, a compiler's intermediate objects and a package manager's build scratch all
    ///     fail. Identical rights are the honest ones: it is counted by the same
    ///     `workdir_max_bytes` guard, so it must be neither more nor less runnable than the
    ///     workdir itself;
    ///   - each [`LandlockGrant`] (the `shell.allow` binaries, their ELF interpreter, their
    ///     shared-library closure, any `interpreter_runtime` or `staged_runtime` directory, and on
    ///     `KernelSealed` the fixed [`crate::sealed::SEALED_RUNTIME_PATHS`] and
    ///     [`crate::sealed::SEALED_ETC_PATHS`], all *outside* the workdir) gets a **narrow read**
    ///     grant — never write — with two bits added on top.
    ///     Whether it carries `ReadDir` (i.e. the directory's own entries are enumerable) is
    ///     exactly the grant's `list_dir`: the derived closure files are all `false` (a regular
    ///     file has no meaningful `ReadDir`), while an `interpreter_runtime` directory carries
    ///     whatever its author wrote. `ReadDir` is granted only on the specific inode a rule names
    ///     — never on an ancestor or sibling — so naming one subdirectory never makes `/usr/lib`
    ///     (or any parent) enumerable. Whether it carries `Execute` is the grant's `executable`,
    ///     `true` everywhere except the two tier-issued sealed grants, which are deliberately
    ///     readable (and, on a directory, enumerable) but not runnable;
    ///   - each entry of the fixed [`CAPSULE_DEVICE_GRANTS`] set gets `ReadFile`, plus `WriteFile`
    ///     and `Truncate` if its `writable` bit is set. That bit is set for exactly one path —
    ///     `/dev/null` — which is therefore the *only* writable path outside the workdir in the
    ///     whole sandbox. This list is a compile-time constant, not manifest-derived: no capability
    ///     declaration widens or narrows it. Every device path it does not name stays denied by the
    ///     ordinary no-matching-rule path below.
    ///
    /// Without the second kind of rule, `restrict_self()` denies `Execute`/`ReadFile` on every
    /// path outside the workdir, so every allowlisted binary's `execve` fails with EACCES before
    /// it runs (and even a binary placed *inside* the workdir can't be loaded, because its dynamic
    /// loader must still read `ld-linux`/libc outside it). The narrow grants fix that while keeping
    /// the security story honest: apart from `/dev/null`, nothing outside the workdir is writable,
    /// so a write to any other outside path — including one of these read+execute grant paths — is
    /// still denied.
    ///
    /// The ruleset is built at `fds.abi` — the kernel's ABI capped at
    /// [`super::LANDLOCK_TARGET_ABI`], decided in the parent — or at ABI v1 when that is `None`.
    /// `handle_access` declares the **full** `from_all` set of that ABI, under `CompatLevel::HardRequirement`, and every rule's
    /// rights are masked to it first: a right the ABI does not know would otherwise fail
    /// `add_rule`, and masking it out is what a kernel of that ABI would enforce anyway. Built at
    /// v1, the ruleset is bit for bit the one a 5.13–5.18 kernel gets.
    ///
    /// `handle_access` must declare the union of every access bit any rule uses, and it is the
    /// full set deliberately: a bit declared there but granted by no rule is denied for
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
            return Err("landlock: ruleset construction failed (forced by test seam)".to_string());
        }

        use landlock::{
            Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
            RulesetCreatedAttr, ABI,
        };

        let abi = fds.abi.map_or(ABI::V1, |abi| ABI::from(i32::from(abi)));
        let handled = AccessFs::from_all(abi);
        // Enough for the loader to open, read, and map-execute a binary or shared library, and to
        // open a file inside a granted directory by exact name — but no write access, and (without
        // `ReadDir`) no enumeration of the directory's own entries.
        let read_execute = AccessFs::Execute | AccessFs::ReadFile;
        // Adds enumerability (`getdents64`) — only for a grant carrying `list_dir: true`.
        let read_execute_list = read_execute | AccessFs::ReadDir;
        // `ReadFile + ReadDir`, no `Execute` — readable and enumerable but not runnable. See
        // `resolve_sealed_runtime_landlock_grants` and `resolve_sealed_etc_landlock_grants` for why
        // `Execute` is withheld from a tier-issued grant over trees the manifest never named.
        let read_list = AccessFs::ReadFile | AccessFs::ReadDir;
        // `ReadFile` alone: the same tier-issued grant on an entry that is a *file* rather than a
        // directory (most of `SEALED_ETC_PATHS`). Not cosmetic — `landlock_append_fs_rule` rejects
        // `ReadDir` on a non-directory with `EINVAL`, and a rejected rule fails the launch below.
        let read_only: BitFlags<AccessFs> = AccessFs::ReadFile.into();
        // Device rights. No `Execute` (a character device is never exec'd) and no `ReadDir` (it is
        // not a directory) — only the data bits, with `WriteFile` reserved for the writable
        // devices. `Truncate` rides with `WriteFile` because a shell's `>/dev/null` opens with
        // `O_TRUNC`, and handling `Truncate` from ABI v3 would otherwise refuse it.
        let device_read: BitFlags<AccessFs> = AccessFs::ReadFile.into();
        let device_read_write = device_read | AccessFs::WriteFile | AccessFs::Truncate;

        // The one writable right-set in this function, resolved once and used by both rules that
        // carry it. The session's temporary folder is counted by the workdir's size guard and is
        // the capsule's other scratch space, so it gets the same rights, `Execute` axis included:
        // `workdir_exec: false` must no more permit running a binary from `/tmp` than from the
        // workdir. Sharing the binding is what keeps that true; two separate calls could drift
        // apart.
        let writable_rights = workdir_access_rights(fds.workdir_exec) & handled;

        // Every fd here was already opened in the parent (before fork). This function performs no
        // `open()`/`canonicalize()` — only the Landlock ruleset syscalls against already-open fds,
        // so a failure here can only be the kernel call itself, not an unrelated path resolution.
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(handled)
            .map_err(|error| format!("landlock: handle_access failed: {error}"))?
            .create()
            .map_err(|error| format!("landlock: ruleset create failed: {error}"))?
            .add_rule(PathBeneath::new(fds.workdir_fd.as_fd(), writable_rights))
            .map_err(|error| format!("landlock: add_rule failed: {error}"))?;

        // The session's temporary folder, whenever the enforcement holds one. A rule on the
        // workdir does not reach a bind mounted over `/tmp`; see [`LandlockChildFds::temp_dir_fd`].
        if let Some(temp_dir_fd) = &fds.temp_dir_fd {
            ruleset = ruleset
                .add_rule(PathBeneath::new(temp_dir_fd.as_fd(), writable_rights))
                .map_err(|error| {
                    format!("landlock: add_rule for the session's temporary folder failed: {error}")
                })?;
        }

        // The session's home folder, on exactly the temporary folder's terms: the workdir's
        // rights beneath the records directory's read-only rule.
        if let Some(home_dir_fd) = &fds.home_dir_fd {
            ruleset = ruleset
                .add_rule(PathBeneath::new(home_dir_fd.as_fd(), writable_rights))
                .map_err(|error| {
                    format!("landlock: add_rule for the session's home folder failed: {error}")
                })?;
        }

        // The runtime directory: readable and listable, never writable or executable. A native
        // tool's entrypoint inside it runs through its own pinned `Execute` grant.
        if let Some(runtime_dir_fd) = &fds.runtime_dir_fd {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    runtime_dir_fd.as_fd(),
                    read_list & handled,
                ))
                .map_err(|error| {
                    format!("landlock: add_rule for the runtime directory failed: {error}")
                })?;
        }

        // The records directory, on the same terms: the runtime appends to the trace from
        // outside the domain, and no command inside it can.
        if let Some(records_dir_fd) = &fds.records_dir_fd {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    records_dir_fd.as_fd(),
                    read_list & handled,
                ))
                .map_err(|error| {
                    format!("landlock: add_rule for the records directory failed: {error}")
                })?;
        }

        // The delegated children's records, on the records directory's terms.
        if let Some(children_records_fd) = &fds.children_records_fd {
            ruleset = ruleset
                .add_rule(PathBeneath::new(
                    children_records_fd.as_fd(),
                    read_list & handled,
                ))
                .map_err(|error| {
                    format!("landlock: add_rule for the children's records failed: {error}")
                })?;
        }

        // `KernelSealed` only — empty on every other tier. `ReadFile` alone: these are the two
        // synthetic account databases the root binds read-only at `/etc/passwd`/`/etc/group`, and
        // the grant loop's rules for those two paths are taken on the host's files, not on these.
        for identity_fd in &fds.sealed_identity_fds {
            ruleset = ruleset
                .add_rule(PathBeneath::new(identity_fd.as_fd(), read_only & handled))
                .map_err(|error| {
                    format!("landlock: add_rule for a synthetic /etc file failed: {error}")
                })?;
        }

        for grant in &fds.grants {
            // Grant paths that failed to open in the parent were already dropped (shrink-not-fail),
            // so every fd reaching here is valid. A rule that still fails to add is a genuine
            // ruleset-construction failure and propagates as `Err` (fail-closed).
            let access = match (grant.executable, grant.list_dir) {
                (true, true) => read_execute_list,
                (true, false) => read_execute,
                (false, true) => read_list,
                (false, false) => read_only,
            };
            ruleset = ruleset
                .add_rule(PathBeneath::new(grant.fd.as_fd(), access & handled))
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
                .add_rule(PathBeneath::new(device.fd.as_fd(), access & handled))
                .map_err(|error| format!("landlock: add_rule for device failed: {error}"))?;
        }

        ruleset
            .restrict_self()
            .map_err(|error| format!("landlock: restrict_self failed: {error}"))?;

        Ok(())
    }

    /// What [`open_landlock_fds`] opens, or carries through, for one launch.
    pub(super) struct LandlockFdRequest<'a> {
        /// The workdir the capsule's commands run in. Fail-closed: a workdir that cannot be
        /// opened is an `Err` naming the path.
        pub(super) accessible_workdir: &'a Path,
        /// The manifest-derived grants. A grant path that fails to open is dropped
        /// (shrink-not-fail).
        pub(super) landlock_grants: &'a [LandlockGrant],
        /// The tier's fixed device set, [`super::CAPSULE_DEVICE_GRANTS`] on `KernelFull` and the
        /// composed root's own OCI default set on `KernelSealed`, chosen by
        /// `super::landlock_device_grants`. Opened by the grants' rules: the list is a
        /// compile-time constant rather than manifest-derived, and a host where one entry cannot
        /// be opened — unusual, but a broken or minimal `/dev` is possible — loses that one device
        /// rather than failing the launch. A lost device, `/dev/null` included, narrows the
        /// capsule's scope and never widens it.
        pub(super) device_grants: &'a [super::CapsuleDeviceGrant],
        /// Carried into [`LandlockChildFds::workdir_exec`].
        pub(super) workdir_exec: bool,
        /// Carried into [`LandlockChildFds::abi`].
        pub(super) abi: Option<u8>,
        /// What only `KernelSealed` opens: the synthetic `/etc` files. Fail-closed.
        pub(super) sealed: Option<&'a super::SealedSourceFds>,
        /// The session's temporary folder's descriptor on both Landlock tiers, which
        /// `prepare_enforcement` already took and shares with the sealed binds. Fail-closed — an
        /// `Err` naming the path, exactly like the workdir's own open, never the grant loops'
        /// shrink-not-fail `continue`.
        pub(super) temp_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The session's home folder's descriptor, on the same terms as [`Self::temp_dir_fd`].
        pub(super) home_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The runtime directory's descriptor, on the same terms as [`Self::temp_dir_fd`].
        pub(super) runtime_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The records directory's descriptor, on the same terms as [`Self::temp_dir_fd`].
        pub(super) records_dir_fd: Option<std::sync::Arc<OwnedFd>>,
        /// The delegated children's records' descriptor, on the same terms.
        pub(super) children_records_fd: Option<std::sync::Arc<OwnedFd>>,
    }

    impl<'a> LandlockFdRequest<'a> {
        /// `accessible_workdir` and nothing else: no grants, no devices, no sealed sources and
        /// no directory fds.
        #[cfg(test)]
        pub(super) fn workdir_only(accessible_workdir: &'a Path) -> Self {
            Self {
                accessible_workdir,
                landlock_grants: &[],
                device_grants: &[],
                workdir_exec: false,
                abi: None,
                sealed: None,
                temp_dir_fd: None,
                home_dir_fd: None,
                runtime_dir_fd: None,
                records_dir_fd: None,
                children_records_fd: None,
            }
        }
    }

    /// Opens (in the PARENT, before fork) the workdir fd and every grant fd Landlock needs, using
    /// `O_PATH | O_CLOEXEC` — the exact flags `landlock::PathFd` uses. Moving these `open()` calls
    /// out of the child's `pre_exec` window is the whole point: a workdir that cannot be resolved
    /// now fails here, synchronously, with a message naming the path — before any subprocess is
    /// spawned — instead of collapsing to a bare EINVAL inside `pre_exec`. A grant path that fails
    /// to open is dropped (shrink-not-fail), matching the previous per-grant `continue`.
    ///
    /// It is also where each grant's `list_dir` is reconciled against the fd that was actually
    /// opened — see the comment on the grant loop. Same discipline as the drop above: narrowing
    /// only, never widening.
    ///
    /// The request's devices are opened by the same rules as its grants, and its directory fds
    /// move into the returned [`LandlockChildFds`]. See [`LandlockFdRequest`] for each field.
    pub(super) fn open_landlock_fds(
        request: LandlockFdRequest<'_>,
    ) -> Result<LandlockChildFds, String> {
        let LandlockFdRequest {
            accessible_workdir,
            landlock_grants,
            device_grants,
            workdir_exec,
            abi,
            sealed,
            temp_dir_fd,
            home_dir_fd,
            runtime_dir_fd,
            records_dir_fd,
            children_records_fd,
        } = request;
        let workdir_fd = open_o_path(accessible_workdir)
            .map(OwnedFd::from)
            .map_err(|error| {
                format!(
                    "sandbox: failed to open workdir {} for Landlock scoping: {error}",
                    accessible_workdir.display()
                )
            })?;

        // Same tier gate, same fail-closed handling: the composed root binds these two files at
        // `/etc/passwd`/`/etc/group`, and the grant loop below opens the *host's* files of those
        // names, whose inodes the capsule never sees. See `LandlockChildFds`. `sealed-etc/` is
        // written by the runtime alone.
        let mut sealed_identity_fds = Vec::new();
        for path in sealed
            .map(|sources| sources.identity_files.as_slice())
            .unwrap_or_default()
        {
            let file = open_o_path(path).map_err(|error| {
                format!(
                    "sandbox: failed to open the synthetic /etc file {} for Landlock scoping: \
                     {error}",
                    path.display()
                )
            })?;
            sealed_identity_fds.push(OwnedFd::from(file));
        }

        let mut grants = Vec::with_capacity(landlock_grants.len());
        for grant in landlock_grants {
            match open_o_path(&grant.path) {
                Ok(file)
                    if grant.pinned.is_some_and(|pinned| {
                        file.metadata()
                            .map(|meta| super::FileIdentity::of(&meta) != pinned)
                            .unwrap_or(true)
                    }) =>
                {
                    continue
                }
                Ok(file) => grants.push(OpenLandlockGrant {
                    // `ReadDir` is meaningful only on a directory, and the kernel does not merely
                    // ignore it elsewhere: `landlock_append_fs_rule` returns `EINVAL` for a
                    // directory-only right on a non-directory, and the rule loop in
                    // `apply_landlock_scope` is fail-closed, so one such rule would refuse the
                    // whole launch. Every `list_dir: true` reaching here is a *claim* — an author's
                    // `interpreter_runtime` entry, or the static `directory` bit on a
                    // `SEALED_ETC_PATHS` entry — so it is checked against the fd actually opened
                    // and narrowed rather than trusted. Narrowing only: this can drop `ReadDir`,
                    // never add it.
                    list_dir: grant.list_dir
                        && file.metadata().map(|meta| meta.is_dir()).unwrap_or(false),
                    fd: OwnedFd::from(file),
                    executable: grant.executable,
                }),
                Err(_) => continue,
            }
        }

        let mut devices = Vec::with_capacity(device_grants.len());
        for device in device_grants {
            match open_o_path(Path::new(device.path)) {
                Ok(file) => devices.push(OpenDeviceGrant {
                    fd: OwnedFd::from(file),
                    writable: device.writable,
                }),
                Err(_) => continue,
            }
        }

        Ok(LandlockChildFds {
            workdir_fd,
            workdir_exec,
            abi,
            temp_dir_fd,
            home_dir_fd,
            sealed_identity_fds,
            runtime_dir_fd,
            records_dir_fd,
            children_records_fd,
            grants,
            devices,
        })
    }

    /// Opens `path` with `O_PATH | O_CLOEXEC` (identity/lifetime handle only, never a data fd),
    /// suitable for `landlock::PathBeneath`. Same open the vendored `landlock::PathFd::new`
    /// performs, done in the parent so the resulting fd can be moved into the child's `pre_exec`
    /// closure.
    ///
    /// Returns the `File` rather than a bare `OwnedFd` so the caller can `fstat` it — see the
    /// `list_dir` narrowing in [`open_landlock_fds`], which needs the kind of *this* fd and must
    /// not perform a second path lookup to get it.
    fn open_o_path(path: &Path) -> io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
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
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
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

    /// What the child's seccomp filter costs, and the two kernel ceilings that cost is held
    /// against — all four numbers produced by running code on the host under test, never quoted.
    ///
    /// The written record these tests feed is
    /// `docs/content/reference/seccomp-filter-instruction-budget.md`.
    ///
    /// Nothing here ever loads a filter into the test process. A loaded seccomp filter is
    /// irreversible for the life of the task, so an in-process load would silently apply to the
    /// rest of the test binary and to everything it spawns. Every load happens in a forked child
    /// that does nothing else and then `_exit`s.
    #[cfg(test)]
    #[allow(clippy::print_stdout, clippy::print_stderr)]
    mod seccomp_budget {
        use std::io;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // `seccomp_filter_instruction_count` is absent from this list because the test that
        // reports it carries the same name; it is called as `super::` throughout.
        use super::{
            build_seccomp_filter, export_to_memfd, seccomp_filter_pfc, to_io_err,
            LANDLOCK_SYSCALLS, SOCK_FILTER_BYTES,
        };
        use crate::sandbox::{
            allowed_socket_domains, denied_socket_domains, detect_enforcement_tier,
            EnforcementTier, BPF_MAXINSNS_CEILING, SECCOMP_FILTER_INSTRUCTION_BUDGET,
            SECCOMP_MAX_INSNS_PER_PATH, SECCOMP_SYSCALL_ALLOWLIST,
        };

        /// The kernel charges each filter already attached to a task four instructions on top of
        /// its own length when it decides whether the next attach fits under
        /// [`SECCOMP_MAX_INSNS_PER_PATH`].
        ///
        /// It is only part of the arithmetic. The length the kernel adds up is that of the
        /// *converted* program — `seccomp_prepare_filter` translates the classic BPF that
        /// `export_bpf` writes into eBPF before the chain is measured, and the translation comes
        /// out slightly longer. So a load count predicted from the exported classic length is an
        /// upper bound on the load count the kernel actually allows, never an equality, which is
        /// why the cumulative probe brackets it rather than pinning it.
        const KERNEL_PER_FILTER_OVERHEAD_INSNS: usize = 4;

        /// The exit code a load probe's child reports on a refusal libseccomp gave no errno for.
        /// Every real errno is a small positive number, and 0 is reserved for a load that
        /// succeeded, so this cannot collide with either.
        const PROBE_ERRNO_UNKNOWN: i32 = 254;

        /// A filter built only to be measured or refused: default `Allow`, and `rules`
        /// argument-conditional `Errno` rules on a syscall no probe child ever makes.
        ///
        /// Default `Allow` rather than the real filter's default deny, for two reasons that both
        /// matter. A child has to survive its own load long enough to `_exit` and, in the
        /// cumulative probe, to call `seccomp(2)` again — the real filter denies `seccomp` on
        /// purpose, so it cannot be stacked on itself. And libseccomp drops a rule whose action
        /// equals the default action, so the rules have to carry an action the default does not:
        /// `Errno` against a default of `Allow` is the inverse of the shipped filter and keeps
        /// every rule in the program.
        ///
        /// `set_api_sysrawrc` is what makes the errno observable at all: without it libseccomp
        /// collapses every `seccomp(2)` attach failure to `ECANCELED`, which would hide the very
        /// `EINVAL`/`ENOMEM` distinction these probes exist to establish.
        fn probe_filter(rules: usize) -> io::Result<(libseccomp::ScmpFilterContext, usize)> {
            let mut filter = libseccomp::ScmpFilterContext::new(libseccomp::ScmpAction::Allow)
                .map_err(to_io_err)?;
            filter.set_api_sysrawrc(true).map_err(to_io_err)?;

            let syscall = libseccomp::ScmpSyscall::from_name("ptrace").map_err(to_io_err)?;
            for value in 0..rules {
                filter
                    .add_rule_conditional(
                        libseccomp::ScmpAction::Errno(libc::EPERM),
                        syscall,
                        &[libseccomp::ScmpArgCompare::new(
                            0,
                            libseccomp::ScmpCompareOp::Equal,
                            value as u64,
                        )],
                    )
                    .map_err(to_io_err)?;
            }

            let exported = export_to_memfd(|fd| filter.export_bpf(fd))?;
            Ok((filter, exported.len() / SOCK_FILTER_BYTES))
        }

        /// The smallest probe-filter rule count whose compiled program is longer than
        /// `instructions`. Doubling to bracket, then bisecting: the instruction count grows
        /// monotonically with the rule count, so both halves are sound and the whole search
        /// costs a couple of dozen exports rather than thousands.
        fn first_rule_count_above(instructions: usize) -> io::Result<usize> {
            let mut under = 1;
            let mut over = 8;
            while probe_filter(over)?.1 <= instructions {
                under = over;
                over *= 2;
                assert!(
                    over <= 1 << 20,
                    "no probe filter of up to {over} rules exceeded {instructions} instructions — \
                     the probe is not growing the program the way this search assumes"
                );
            }
            while over - under > 1 {
                let mid = under + (over - under) / 2;
                if probe_filter(mid)?.1 <= instructions {
                    under = mid;
                } else {
                    over = mid;
                }
            }
            Ok(over)
        }

        /// The raw errno behind a libseccomp failure, for a context built by [`probe_filter`] and
        /// therefore carrying `SCMP_FLTATR_API_SYSRAWRC`. Known errnos come back as
        /// `ErrorKind::Errno`, anything else as the raw code; both are mapped here so the caller
        /// only ever sees a plain `libc` value.
        fn refusal_errno(error: &libseccomp::error::SeccompError) -> i32 {
            use libseccomp::error::SeccompErrno;

            if let Some(raw) = error.sysrawrc() {
                return -raw;
            }
            match error.errno() {
                Some(SeccompErrno::EACCES) => libc::EACCES,
                Some(SeccompErrno::ECANCELED) => libc::ECANCELED,
                Some(SeccompErrno::EINVAL) => libc::EINVAL,
                Some(SeccompErrno::ENOMEM) => libc::ENOMEM,
                _ => PROBE_ERRNO_UNKNOWN,
            }
        }

        /// Loads `filter` in a forked child and reports what the kernel said: `Ok(())` for a load
        /// that took, `Err(errno)` for one the kernel refused.
        ///
        /// The filter is built by the caller, before the fork, so the child's whole post-fork
        /// window is `load()` and `_exit`.
        fn load_in_child(filter: &libseccomp::ScmpFilterContext) -> Result<(), i32> {
            // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the
            // child confines itself to work that does not depend on another thread's state. This
            // child calls `load()` and then `_exit`. `load()` allocates — libseccomp generates
            // the BPF program on every load and does not cache the one `export_bpf` produced —
            // so this probe is not async-signal-safe in the strict sense; it is bounded to a
            // child that touches nothing else and dies immediately.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());

            if pid == 0 {
                let code = match filter.load() {
                    Ok(()) => 0,
                    Err(error) => refusal_errno(&error),
                };
                // SAFETY: forked-child context; `_exit` skips every destructor and atexit hook,
                // which is what keeps libtest's state in the parent untouched.
                unsafe { libc::_exit(code) }
            }

            let mut status: libc::c_int = 0;
            // SAFETY: `pid` is the child just forked; `status` is a live local.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            assert!(waited == pid, "waitpid: {}", io::Error::last_os_error());
            assert!(
                libc::WIFEXITED(status),
                "the load probe child did not exit normally (status {status})"
            );
            match libc::WEXITSTATUS(status) {
                0 => Ok(()),
                errno => Err(errno),
            }
        }

        /// The number of distinct syscalls the allowlist actually contributes a rule for on this
        /// host, counted the way `build_seccomp_filter` contributes them — resolve the name, try
        /// the rule, keep what libseccomp accepted — but against a scratch context, so the count
        /// is an independent measurement rather than a restatement of the filter under test.
        fn resolvable_allowlist_syscalls() -> io::Result<usize> {
            use std::collections::HashSet;

            let mut scratch =
                libseccomp::ScmpFilterContext::new(libseccomp::ScmpAction::Errno(libc::EPERM))
                    .map_err(to_io_err)?;
            let mut accepted = HashSet::new();
            for name in SECCOMP_SYSCALL_ALLOWLIST {
                if let Ok(syscall) = libseccomp::ScmpSyscall::from_name(name) {
                    if scratch
                        .add_rule(libseccomp::ScmpAction::Allow, syscall)
                        .is_ok()
                    {
                        accepted.insert(i32::from(syscall));
                    }
                }
            }
            Ok(accepted.len())
        }

        /// The line both ceiling probes print instead of running, on a host where no seccomp
        /// filter can be loaded at all.
        fn skip_without_seccomp(what: &str) -> bool {
            let tier = detect_enforcement_tier();
            if tier == EnforcementTier::EnvironmentOnly {
                eprintln!(
                    "SKIP — PROVES NOTHING ON THIS HOST: measuring {what} requires a host that \
                     can load a seccomp filter at all, which is every tier above \
                     EnforcementTier::EnvironmentOnly; detected {tier:?}"
                );
                return true;
            }
            false
        }

        /// What the real filter compiles to, against the budget this project holds itself to and
        /// the ceiling the kernel holds it to.
        ///
        /// Both `unix_sockets_allowed` settings, because they build different rule sets — one
        /// moves `AF_UNIX` from the denied domains to the allowed ones — and the budget has to
        /// cover the larger of the two.
        #[test]
        fn seccomp_filter_instruction_count() {
            for unix_sockets_allowed in [false, true] {
                let count = super::seccomp_filter_instruction_count(unix_sockets_allowed)
                    .expect("export the built seccomp filter");

                eprintln!(
                    "seccomp filter instruction count: unix_sockets_allowed={unix_sockets_allowed} \
                     -> {count} instructions; budget {SECCOMP_FILTER_INSTRUCTION_BUDGET}; kernel \
                     per-filter ceiling {BPF_MAXINSNS_CEILING} ({:.1}x headroom); kernel \
                     cumulative ceiling {SECCOMP_MAX_INSNS_PER_PATH} ({:.1}x headroom)",
                    BPF_MAXINSNS_CEILING as f64 / count as f64,
                    SECCOMP_MAX_INSNS_PER_PATH as f64 / count as f64,
                );

                assert!(
                    count > 0,
                    "the exported filter is empty, so nothing was measured — \
                     `export_bpf` wrote no `struct sock_filter` records at all"
                );
                assert!(
                    count <= SECCOMP_FILTER_INSTRUCTION_BUDGET,
                    "the child's seccomp filter compiles to {count} instructions with \
                     unix_sockets_allowed={unix_sockets_allowed}, over this project's budget of \
                     {SECCOMP_FILTER_INSTRUCTION_BUDGET} (the kernel refuses a single filter over \
                     {BPF_MAXINSNS_CEILING} with EINVAL). Re-measure, decide whether the rule set \
                     still earns its size, and raise SECCOMP_FILTER_INSTRUCTION_BUDGET \
                     deliberately — updating \
                     docs/content/reference/seccomp-filter-instruction-budget.md with the new \
                     numbers — rather than letting the filter grow into a spawn failure"
                );
            }
        }

        /// The rule sequence `build_seccomp_filter` lays down, pinned against the filter's own
        /// pseudo-filter-code export so a change to the shape is a test failure rather than a
        /// silent change of policy.
        ///
        /// The `EACCES` count is the load-bearing one. An unconditional `Allow` on `socket`
        /// anywhere in this function would make libseccomp discard every argument-conditional
        /// chain already recorded for that syscall — reopening `AF_UNIX`, `AF_NETLINK` and
        /// `AF_PACKET` — and it would show up here as zero `EACCES` actions.
        #[test]
        fn seccomp_filter_rule_shape() {
            let allowlist_syscalls =
                resolvable_allowlist_syscalls().expect("count the resolvable allowlist syscalls");
            assert!(
                allowlist_syscalls > 0,
                "no SECCOMP_SYSCALL_ALLOWLIST name resolved on this host, so this test would pass \
                 for the wrong reason"
            );

            for unix_sockets_allowed in [false, true] {
                let filter =
                    build_seccomp_filter(unix_sockets_allowed).expect("build the seccomp filter");
                assert_eq!(
                    filter.get_act_default().expect("read the default action"),
                    libseccomp::ScmpAction::Errno(libc::EPERM),
                    "the filter's default action is no longer a deny with EPERM, which is what \
                     makes every syscall outside the allowlist unreachable"
                );

                let pfc =
                    seccomp_filter_pfc(unix_sockets_allowed).expect("export the filter's pfc");
                let denied = denied_socket_domains(unix_sockets_allowed);
                let allowed = allowed_socket_domains(unix_sockets_allowed);

                // One block per syscall carrying at least one rule: `socket`, every allowlist
                // name libseccomp accepted, and the three Landlock syscalls.
                let syscall_blocks = pfc.matches("# filter for syscall ").count();
                assert_eq!(
                    syscall_blocks,
                    1 + allowlist_syscalls + LANDLOCK_SYSCALLS.len(),
                    "the built filter carries rules for {syscall_blocks} syscalls, not the \
                     socket rule plus {allowlist_syscalls} allowlist syscalls plus {} Landlock \
                     syscalls this code adds",
                    LANDLOCK_SYSCALLS.len()
                );

                let deny_actions = pfc
                    .matches(&format!("action ERRNO({});", libc::EACCES))
                    .count();
                assert_eq!(
                    deny_actions,
                    denied.len(),
                    "the built filter carries {deny_actions} EACCES rules, not one per denied \
                     socket domain ({denied:?}) — an unconditional Allow on `socket` would look \
                     exactly like this, and would have discarded every domain denial with it"
                );

                // Every non-`socket` block contributes exactly one `Allow`; the `socket` block
                // contributes one per allowed domain.
                let allow_actions = pfc.matches("action ALLOW;").count();
                assert_eq!(
                    allow_actions,
                    allowed.len() + allowlist_syscalls + LANDLOCK_SYSCALLS.len(),
                    "the built filter carries {allow_actions} Allow rules, not one per allowed \
                     socket domain ({allowed:?}) plus one per allowlist syscall plus one per \
                     Landlock syscall"
                );

                for (name, _) in LANDLOCK_SYSCALLS {
                    assert!(
                        pfc.contains(&format!("\"{name}\"")),
                        "{name} has no rule in the built filter, so `apply_landlock_scope` would \
                         be refused by the filter installed just before it"
                    );
                }

                eprintln!(
                    "seccomp filter rule shape: unix_sockets_allowed={unix_sockets_allowed} -> \
                     default ERRNO(EPERM); {} socket EACCES rules; {} socket ALLOW rules; \
                     {allowlist_syscalls} allowlist syscalls; {} Landlock syscalls",
                    denied.len(),
                    allowed.len(),
                    LANDLOCK_SYSCALLS.len(),
                );
            }
        }

        /// Brackets `BPF_MAXINSNS`, the kernel's limit on a *single* filter's length, by loading
        /// two filters that straddle it and observing that the kernel takes one and refuses the
        /// other with `EINVAL`.
        ///
        /// `EINVAL`, not `ENOMEM`: an `ENOMEM` out of a seccomp attach never came from this
        /// ceiling, whatever the filter's length.
        #[test]
        fn seccomp_per_filter_ceiling() {
            if skip_without_seccomp("the kernel's per-filter instruction ceiling") {
                return;
            }

            let over_rules = first_rule_count_above(BPF_MAXINSNS_CEILING)
                .expect("bracket the per-filter ceiling");
            let (under_filter, under_count) =
                probe_filter(over_rules - 1).expect("build the under-ceiling probe filter");
            let (over_filter, over_count) =
                probe_filter(over_rules).expect("build the over-ceiling probe filter");

            assert!(
                under_count <= BPF_MAXINSNS_CEILING && over_count > BPF_MAXINSNS_CEILING,
                "the two probe filters ({under_count} and {over_count} instructions) do not \
                 straddle BPF_MAXINSNS_CEILING ({BPF_MAXINSNS_CEILING}), so this test would prove \
                 nothing about where the ceiling is"
            );

            let under_outcome = load_in_child(&under_filter);
            let over_outcome = load_in_child(&over_filter);

            eprintln!(
                "seccomp per-filter ceiling: largest filter that loaded = {under_count} \
                 instructions; smallest that did not = {over_count} instructions, refused with \
                 errno {} (EINVAL is {}); recorded BPF_MAXINSNS_CEILING = {BPF_MAXINSNS_CEILING}",
                over_outcome.err().unwrap_or(0),
                libc::EINVAL,
            );

            assert_eq!(
                under_outcome,
                Ok(()),
                "a {under_count}-instruction filter was refused, below the \
                 {BPF_MAXINSNS_CEILING}-instruction ceiling this test expects the kernel to hold"
            );
            assert_eq!(
                over_outcome,
                Err(libc::EINVAL),
                "a {over_count}-instruction filter was not refused with EINVAL, so \
                 BPF_MAXINSNS_CEILING ({BPF_MAXINSNS_CEILING}) is not where this kernel puts its \
                 per-filter limit"
            );
        }

        /// Measures `MAX_INSNS_PER_PATH`, the kernel's limit on the *whole chain* of filters one
        /// task carries, by stacking filters in a forked child until the kernel refuses.
        ///
        /// `ENOMEM` is what this ceiling returns, and it is the only seccomp attach path in the
        /// kernel that returns it — so an `ENOMEM` from a runtime that installs exactly one
        /// filter of a few hundred instructions, and that had not yet reached `load()` when it
        /// failed, did not come from the kernel's seccomp code.
        #[test]
        fn seccomp_cumulative_ceiling() {
            if skip_without_seccomp("the kernel's cumulative filter-chain ceiling") {
                return;
            }

            // A probe of roughly the shipped filter's size, so the number of stacked loads reads
            // directly as "how many filters like ours one task could carry".
            let (filter, count) = probe_filter(120).expect("build the cumulative probe filter");
            let per_filter_cost = count + KERNEL_PER_FILTER_OVERHEAD_INSNS;
            // Enough headroom past the ceiling that a kernel which never refuses is caught by the
            // cap rather than by the test hanging.
            let max_loads = SECCOMP_MAX_INSNS_PER_PATH / per_filter_cost + 16;

            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` is a live two-element array, which is what `pipe2` writes into.
            let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
            assert!(rc == 0, "pipe2: {}", io::Error::last_os_error());
            // SAFETY: both descriptors are fresh out of `pipe2` and owned by nothing else.
            let (read_fd, write_fd) =
                unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

            // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the
            // child confines itself to work that does not depend on another thread's state. This
            // child calls `load()` in a loop, writes eight bytes and `_exit`s.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork failed: {}", io::Error::last_os_error());

            if pid == 0 {
                let mut loads: u32 = 0;
                let mut refusal: i32 = 0;
                while loads < max_loads as u32 {
                    match filter.load() {
                        Ok(()) => loads += 1,
                        Err(error) => {
                            refusal = refusal_errno(&error);
                            break;
                        }
                    }
                }

                let mut report = [0u8; 8];
                report[..4].copy_from_slice(&loads.to_ne_bytes());
                report[4..].copy_from_slice(&refusal.to_ne_bytes());
                // SAFETY: forked-child context; `write` is async-signal-safe, `report` is a live
                // local of exactly the length passed, and the descriptor is the pipe's write end.
                unsafe { libc::write(write_fd.as_raw_fd(), report.as_ptr().cast(), report.len()) };
                // SAFETY: forked-child context; `_exit` skips every destructor and atexit hook.
                unsafe { libc::_exit(0) }
            }

            drop(write_fd);
            let mut report = [0u8; 8];
            std::io::Read::read_exact(&mut std::fs::File::from(read_fd), &mut report)
                .expect("read the cumulative probe child's report");
            let loads = u32::from_ne_bytes(report[..4].try_into().unwrap()) as usize;
            let refusal = i32::from_ne_bytes(report[4..].try_into().unwrap());

            let mut status: libc::c_int = 0;
            // SAFETY: `pid` is the child just forked; `status` is a live local.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            assert!(waited == pid, "waitpid: {}", io::Error::last_os_error());

            let stacked = loads * count;
            let cumulative_at_refusal = count + loads * per_filter_cost;
            let loads_if_counted_classic = SECCOMP_MAX_INSNS_PER_PATH / per_filter_cost;
            let shipped = super::seccomp_filter_instruction_count(false)
                .expect("measure the shipped filter for comparison");

            eprintln!(
                "seccomp cumulative ceiling: {loads} loads of a {count}-instruction filter \
                 succeeded ({stacked} instructions stacked, {cumulative_at_refusal} counted the \
                 way the kernel counts once the {KERNEL_PER_FILTER_OVERHEAD_INSNS}-instruction \
                 per-filter overhead is added); load {} was refused with errno {refusal} (ENOMEM \
                 is {}), against the recorded SECCOMP_MAX_INSNS_PER_PATH of \
                 {SECCOMP_MAX_INSNS_PER_PATH} — which the exported classic length puts at at most \
                 {loads_if_counted_classic} loads. The runtime installs ONE filter of {shipped} \
                 instructions, so reaching this ceiling would take {} of them in a single task.",
                loads + 1,
                libc::ENOMEM,
                SECCOMP_MAX_INSNS_PER_PATH / shipped,
            );

            assert!(
                loads >= 2,
                "the kernel refused the filter chain after only {loads} load(s), so this is not \
                 a cumulative limit and the ENOMEM it returns is not the one being ruled out"
            );
            assert_eq!(
                refusal,
                libc::ENOMEM,
                "stacking filters past SECCOMP_MAX_INSNS_PER_PATH was refused with errno \
                 {refusal}, not ENOMEM ({}) — the rule-out in \
                 docs/content/reference/seccomp-filter-instruction-budget.md rests on ENOMEM \
                 being what this ceiling returns",
                libc::ENOMEM
            );
            assert!(
                loads <= loads_if_counted_classic,
                "the kernel took {loads} loads of a {count}-instruction filter — more than the \
                 {loads_if_counted_classic} that fit under SECCOMP_MAX_INSNS_PER_PATH \
                 ({SECCOMP_MAX_INSNS_PER_PATH}) even before the conversion to eBPF lengthens \
                 each one, so this kernel's ceiling is higher than the recorded value"
            );
            // Loose on purpose: the exported classic length under-counts what the kernel adds
            // up, so the bound below tolerates any plausible conversion expansion while still
            // refusing a ceiling of a different order of magnitude — a kernel that stopped at,
            // say, BPF_MAXINSNS would fail here.
            assert!(
                loads * 2 > loads_if_counted_classic,
                "the kernel refused after {loads} loads of a {count}-instruction filter, far \
                 short of the {loads_if_counted_classic} that SECCOMP_MAX_INSNS_PER_PATH \
                 ({SECCOMP_MAX_INSNS_PER_PATH}) implies — the ceiling being measured is not the \
                 one recorded"
            );
            assert!(
                shipped * 2 < SECCOMP_MAX_INSNS_PER_PATH,
                "the shipped filter ({shipped} instructions) is within a factor of two of the \
                 cumulative ceiling, which would make the rule-out this test supports untrue"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::security_warnings::W_SEC_003;

    /// Content check only: this asserts the *constant*, not the security property. Spawning a
    /// subprocess and asserting it cannot see an inherited fd passes vacuously without kernel
    /// enforcement, so it would read as evidence while proving nothing.
    ///
    /// It does catch the one silent, catastrophic edit: a `0` here marks stdio close-on-exec and
    /// breaks every subprocess invocation.
    #[test]
    fn fd_hygiene_range_starts_above_stdio() {
        assert_eq!(
            FD_HYGIENE_FIRST_FD, 3,
            "fd hygiene must start at 3 — 0/1/2 are the stdio pipes every spawn needs to \
             survive execve"
        );
    }

    /// `mark_inherited_fds_cloexec` reaches `close_range` through `libc::syscall`, so its failure
    /// convention is the raw one: `-1` with `errno` set. That convention is what turns a kernel
    /// that rejects the call into a failed `Command::spawn()` rather than a spawn that silently
    /// runs without fd hygiene.
    ///
    /// The rejection is provoked with an all-bits-set flag word, which the kernel refuses during
    /// argument validation before it touches any descriptor — so this exercises the errno path
    /// without closing or flagging a single fd of the test process. A kernel older than 5.11
    /// rejects the production call with a different errno and the identical convention — see the
    /// **Kernel range narrowing** paragraph on `mark_inherited_fds_cloexec`.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    #[test]
    fn close_range_syscall_reports_failure_as_minus_one_and_errno() {
        // SAFETY: `close_range` takes three scalar arguments and dereferences nothing. Every flag
        // bit is set here, so the kernel rejects the call on argument validation and no descriptor
        // of this process is closed or marked close-on-exec.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                FD_HYGIENE_FIRST_FD,
                libc::c_uint::MAX,
                libc::c_uint::MAX,
            )
        };

        assert_eq!(rc, -1, "an invalid flag word must not be accepted");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EINVAL),
            "the two statements the production path runs on failure must carry the kernel's errno"
        );
    }

    /// A host whose probe created all three namespaces — read under the AppArmor restriction with
    /// `mur` unconfined, because the readings never decide anything.
    fn sealed_capable() -> crate::namespace_probe::NamespaceReading {
        crate::namespace_probe::NamespaceReading {
            network: Ok(()),
            mount: crate::namespace_probe::MountAttempt::Created,
            sealed_root: crate::namespace_probe::MountAttempt::Created,
            userns_grant: crate::sealed::UsernsGrant::Withheld,
            attachment: crate::sealed::ProfileAttachment::Unconfined,
        }
    }

    /// A host whose probe could not run at all, which claims nothing.
    fn sealed_incapable() -> crate::namespace_probe::NamespaceReading {
        crate::namespace_probe::NamespaceReading {
            network: Err(crate::network_namespace::NamespaceAttemptError::Creator(
                "the namespace creator did not run".to_string(),
            )),
            mount: crate::namespace_probe::MountAttempt::NotAttempted,
            sealed_root: crate::namespace_probe::MountAttempt::NotAttempted,
            userns_grant: crate::sealed::UsernsGrant::Withheld,
            attachment: crate::sealed::ProfileAttachment::NotReported,
        }
    }

    /// `mur doctor`'s native-tool mechanism is read from the decision on the reading it is handed:
    /// on a sealed-capable reading a capsule declaring `scoped` runs its native tools on
    /// `landlock+seccomp` and one declaring `sealed` on the composed root.
    #[test]
    fn doctor_reads_the_decision_from_the_probe_it_is_handed() {
        if !cfg!(target_os = "linux") {
            return;
        }
        let sealed_host = HostProbe::from_parts(Some(true), sealed_capable());
        assert_eq!(sealed_host.tier(), EnforcementTier::KernelSealed);
        assert_eq!(
            crate::containment::applied_mechanism(
                &sealed_host,
                murmur_artifact::ContainmentClass::Scoped
            ),
            "landlock+seccomp"
        );
        assert_eq!(
            crate::containment::applied_mechanism(
                &sealed_host,
                murmur_artifact::ContainmentClass::Sealed
            ),
            "mountns+pivot_root+landlock+seccomp"
        );
    }

    /// A probe that learned nothing: no Landlock answer and a namespace probe that could not run.
    /// The reading a host gives when neither half of the probe could run.
    fn unprobed() -> HostProbe {
        HostProbe::from_parts(None, sealed_incapable())
    }

    /// A probe that could not run must land on the weakest Linux tier, not on either kernel tier
    /// it failed to demonstrate. This is the fail-closed direction, asserted at the one place the
    /// tier is decided from a reading.
    #[test]
    fn an_unprobed_host_resolves_to_seccomp_only() {
        let tier = unprobed().tier();
        if cfg!(target_os = "linux") {
            assert_eq!(tier, EnforcementTier::KernelSeccompOnly);
        } else {
            assert_eq!(tier, EnforcementTier::EnvironmentOnly);
        }
        assert_ne!(tier, EnforcementTier::KernelSealed);
        assert_ne!(tier, EnforcementTier::KernelFull);
    }

    /// A reading that demonstrated nothing must still name what is missing: a refusal with no
    /// blocker is a refusal an operator cannot act on.
    #[test]
    fn an_unprobed_host_names_a_blocker() {
        assert!(unprobed().sealed_shortfall().is_some());
    }

    /// The achieved class the trace records for an unprobed host stays strictly below `sealed`,
    /// so nothing downstream can read a failed probe as a met floor.
    #[test]
    fn an_unprobed_host_achieves_less_than_sealed() {
        let achieved = crate::containment::achieved_class_for_tier(unprobed().tier());
        assert!(achieved < murmur_artifact::ContainmentClass::Sealed);
    }

    /// The whole point of the direction: a capsule declaring `sealed` against a reading that
    /// demonstrated nothing is refused, with the shortfall naming the mechanism.
    #[test]
    fn an_unprobed_host_refuses_a_declared_sealed_floor() {
        let probe = unprobed();
        let achieved = crate::containment::achieved_class_for_tier(probe.tier());
        let error = crate::containment::check_containment_floor(
            murmur_artifact::ContainmentClass::Sealed,
            achieved,
            probe.sealed_shortfall().as_ref(),
            false,
            None,
        )
        .expect_err("an unprobed host cannot back a declared sealed floor");
        assert!(matches!(
            error,
            crate::errors::RuntimeError::ContainmentFloorUnmet { .. }
        ));
    }

    /// The probe succeeding while the AppArmor setting reads `1`: all three attempts created,
    /// `mur` unconfined or confined by a container's profile. The host backs `sealed`, maps
    /// `/tmp`, and is refused nothing.
    #[test]
    fn a_probe_that_created_every_namespace_backs_sealed_while_the_setting_reads_one() {
        for attachment in [
            crate::sealed::ProfileAttachment::Unconfined,
            crate::sealed::ProfileAttachment::Other {
                profile: "docker-default".to_string(),
            },
        ] {
            let probe = HostProbe::from_parts(
                Some(true),
                crate::namespace_probe::NamespaceReading {
                    attachment,
                    ..sealed_capable()
                },
            );
            if !cfg!(target_os = "linux") {
                assert_eq!(probe.tier(), EnforcementTier::EnvironmentOnly);
                continue;
            }
            assert_eq!(probe.tier(), EnforcementTier::KernelSealed);
            assert_eq!(probe.sealed_shortfall(), None);
            assert!(probe.mount_namespace_usable());
            assert_eq!(probe.egress_namespace_failure(), None);
            assert_eq!(
                crate::temp_dir::temp_mapping(
                    EnforcementTier::KernelFull,
                    probe.mount_namespace_usable(),
                    Path::new("/home/u/w"),
                ),
                crate::temp_dir::TempMapping::MountNamespace
            );
        }
    }

    /// No decision reads the grant: over every reading of the restriction, the tier, the
    /// shortfall, the `/tmp` question and the network failure are the same — for a probe that
    /// created everything, and for one that created only the network and mount namespaces.
    #[test]
    fn every_decision_is_identical_across_every_userns_grant() {
        use crate::namespace_probe::{
            MountAttempt, MountAttemptError, MountStep, NamespaceReading,
        };
        let root_refused = NamespaceReading {
            sealed_root: MountAttempt::Failed(MountAttemptError::Step {
                step: MountStep::PivotRoot,
                errno: libc::EPERM,
            }),
            ..sealed_capable()
        };
        for base in [sealed_capable(), root_refused, sealed_incapable()] {
            let decide = |grant: crate::sealed::UsernsGrant| {
                let probe = HostProbe::from_parts(
                    Some(true),
                    NamespaceReading {
                        userns_grant: grant,
                        ..base.clone()
                    },
                );
                (
                    probe.tier(),
                    probe.sealed_shortfall().map(|shortfall| shortfall.blocker),
                    probe.mount_namespace_usable(),
                    probe
                        .egress_namespace_failure()
                        .map(|failure| failure.blocker),
                )
            };
            let expected = decide(crate::sealed::UsernsGrant::Withheld);
            for grant in crate::sealed::UsernsGrant::ALL {
                assert_eq!(decide(*grant), expected, "{grant:?} over {base:?}");
            }
        }
    }

    /// On whatever host this runs, one live reading is `KernelSealed` exactly when it names no
    /// `sealed` shortfall, maps `/tmp` exactly when the network and mount attempts created their
    /// namespaces, and reports `E-CAP-005` exactly when the network attempt failed.
    #[test]
    fn a_live_reading_answers_every_question_from_its_own_attempts() {
        let probe = HostProbe::probe();
        let namespaces = probe.namespaces().clone();
        let linux = cfg!(target_os = "linux");
        assert_eq!(
            probe.tier() == EnforcementTier::KernelSealed,
            probe.sealed_shortfall().is_none(),
            "{namespaces:?}"
        );
        assert_eq!(
            probe.mount_namespace_usable(),
            linux
                && namespaces.network.is_ok()
                && namespaces.mount == crate::namespace_probe::MountAttempt::Created
        );
        assert_eq!(
            probe.egress_namespace_failure().is_some(),
            linux && namespaces.network.is_err()
        );
    }

    #[test]
    fn tier_from_probe_non_linux_is_always_environment_only() {
        for landlock in [None, Some(true), Some(false)] {
            for namespaces in [sealed_capable(), sealed_incapable()] {
                assert_eq!(
                    tier_from_probe(false, landlock, &namespaces),
                    EnforcementTier::EnvironmentOnly
                );
            }
        }
    }

    #[test]
    fn tier_from_probe_linux_fully_enforced_is_kernel_full() {
        assert_eq!(
            tier_from_probe(true, Some(true), &sealed_incapable()),
            EnforcementTier::KernelFull
        );
    }

    #[test]
    fn tier_from_probe_linux_partially_enforced_is_seccomp_only() {
        assert_eq!(
            tier_from_probe(true, Some(false), &sealed_incapable()),
            EnforcementTier::KernelSeccompOnly
        );
    }

    #[test]
    fn tier_from_probe_linux_no_probe_result_is_seccomp_only() {
        assert_eq!(
            tier_from_probe(true, None, &sealed_incapable()),
            EnforcementTier::KernelSeccompOnly
        );
    }

    /// The whole conjunction, one element removed at a time: `KernelSealed` needs Linux, a fully
    /// enforced Landlock ruleset and a sealed-root rehearsal that built the composed root — and
    /// drops to exactly the tier the missing element still supports, never further.
    #[test]
    fn tier_from_probe_reaches_sealed_only_when_every_precondition_holds() {
        use crate::namespace_probe::{
            MountAttempt, MountAttemptError, MountStep, NamespaceReading,
        };

        assert_eq!(
            tier_from_probe(true, Some(true), &sealed_capable()),
            EnforcementTier::KernelSealed
        );

        // A refused network namespace attempts nothing after it: `scoped`, not `advisory`.
        let network_refused = NamespaceReading {
            network: Err(crate::network_namespace::NamespaceAttemptError::Step {
                step: crate::network_namespace::CreationStep::IdentityMaps,
                errno: libc::EPERM,
            }),
            mount: MountAttempt::NotAttempted,
            sealed_root: MountAttempt::NotAttempted,
            ..sealed_capable()
        };
        assert_eq!(
            tier_from_probe(true, Some(true), &network_refused),
            EnforcementTier::KernelFull
        );

        // Every way the sealed-root rehearsal can fail falls back to KernelFull, never below.
        for step in [
            MountStep::JoinSession,
            MountStep::UnshareUserMount,
            MountStep::IdentityMaps,
            MountStep::MakePrivateNested,
            MountStep::Tmpfs,
            MountStep::PivotPrepare,
            MountStep::PivotRoot,
            MountStep::DetachOldRoot,
        ] {
            let reading = NamespaceReading {
                sealed_root: MountAttempt::Failed(MountAttemptError::Step {
                    step,
                    errno: libc::EPERM,
                }),
                ..sealed_capable()
            };
            assert_eq!(
                tier_from_probe(true, Some(true), &reading),
                EnforcementTier::KernelFull,
                "a sealed root refused at {step:?} must fall back to KernelFull"
            );
        }

        // The `/tmp` attempt is not the composed root's: refusing it leaves `sealed` reachable.
        let tmp_refused = NamespaceReading {
            mount: MountAttempt::Failed(MountAttemptError::Step {
                step: MountStep::MakePrivate,
                errno: libc::EACCES,
            }),
            ..sealed_capable()
        };
        assert_eq!(
            tier_from_probe(true, Some(true), &tmp_refused),
            EnforcementTier::KernelSealed
        );

        // Landlock missing: sealed keeps Landlock inside as defence in depth, so a host without it
        // cannot reach sealed no matter how good its namespaces are.
        assert_eq!(
            tier_from_probe(true, Some(false), &sealed_capable()),
            EnforcementTier::KernelSeccompOnly
        );
        assert_eq!(
            tier_from_probe(true, None, &sealed_capable()),
            EnforcementTier::KernelSeccompOnly
        );

        // Not Linux at all.
        assert_eq!(
            tier_from_probe(false, Some(true), &sealed_capable()),
            EnforcementTier::EnvironmentOnly
        );
    }

    /// `/tmp` follows the probe's mount attempt: refused at `MS_REC|MS_PRIVATE` it is not mapped;
    /// created with the sealed root refused it is mapped at `KernelFull`; with the network
    /// namespace refused there is no mount namespace to map it in.
    #[test]
    fn the_tmp_mapping_follows_the_mount_attempt() {
        use crate::namespace_probe::{
            MountAttempt, MountAttemptError, MountStep, NamespaceReading,
        };
        use crate::temp_dir::{temp_mapping, NotMappedReason, TempMapping};
        if !cfg!(target_os = "linux") {
            return;
        }
        let workdir = Path::new("/home/u/w");
        let refused = |step, errno| MountAttempt::Failed(MountAttemptError::Step { step, errno });

        let mount_refused = HostProbe::from_parts(
            Some(true),
            NamespaceReading {
                mount: refused(MountStep::MakePrivate, libc::EACCES),
                sealed_root: refused(MountStep::MakePrivateNested, libc::EACCES),
                ..sealed_capable()
            },
        );
        assert_eq!(mount_refused.tier(), EnforcementTier::KernelFull);
        assert_eq!(
            temp_mapping(
                mount_refused.tier(),
                mount_refused.mount_namespace_usable(),
                workdir
            ),
            TempMapping::NotMapped(NotMappedReason::NoMountNamespace)
        );

        let root_refused = HostProbe::from_parts(
            Some(true),
            NamespaceReading {
                sealed_root: refused(MountStep::PivotRoot, libc::EPERM),
                ..sealed_capable()
            },
        );
        assert_eq!(root_refused.tier(), EnforcementTier::KernelFull);
        assert_eq!(
            temp_mapping(
                root_refused.tier(),
                root_refused.mount_namespace_usable(),
                workdir
            ),
            TempMapping::MountNamespace
        );

        let network_refused = HostProbe::from_parts(
            Some(true),
            NamespaceReading {
                network: Err(crate::network_namespace::NamespaceAttemptError::Step {
                    step: crate::network_namespace::CreationStep::Unshare,
                    errno: libc::EPERM,
                }),
                mount: MountAttempt::NotAttempted,
                sealed_root: MountAttempt::NotAttempted,
                ..sealed_capable()
            },
        );
        assert!(!network_refused.mount_namespace_usable());
        assert!(matches!(
            temp_mapping(
                network_refused.tier(),
                network_refused.mount_namespace_usable(),
                workdir
            ),
            TempMapping::NotMapped(_)
        ));
        assert!(network_refused.egress_namespace_failure().is_some());
    }

    /// The ruleset ABI is the kernel's, capped at v3, on the two Landlock tiers only.
    #[test]
    fn ruleset_landlock_abi_is_the_kernel_abi_capped_at_the_target_on_landlock_tiers() {
        for tier in [EnforcementTier::KernelFull, EnforcementTier::KernelSealed] {
            for (kernel, built) in [
                (Some(1), Some(1)),
                (Some(2), Some(2)),
                (Some(3), Some(3)),
                (Some(8), Some(3)),
                (Some(u8::MAX), Some(3)),
            ] {
                assert_eq!(
                    ruleset_landlock_abi(kernel, tier),
                    built,
                    "kernel ABI {kernel:?} on {tier:?}"
                );
            }
        }
    }

    /// No Landlock on the kernel, or a tier that installs no Landlock domain, builds at no ABI.
    #[test]
    fn ruleset_landlock_abi_is_none_without_a_kernel_abi_or_a_landlock_tier() {
        for tier in [EnforcementTier::KernelFull, EnforcementTier::KernelSealed] {
            assert_eq!(ruleset_landlock_abi(None, tier), None);
            assert_eq!(ruleset_landlock_abi(Some(0), tier), None);
        }
        for tier in [
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            for kernel in [None, Some(0), Some(1), Some(3), Some(8)] {
                assert_eq!(
                    ruleset_landlock_abi(kernel, tier),
                    None,
                    "kernel ABI {kernel:?} on {tier:?}"
                );
            }
        }
    }

    /// `ShellEnforcement::resolve` carries the ruleset ABI of the probe it is handed, keyed on the
    /// applied tier, and an all-negative probe carries none.
    #[test]
    fn ruleset_landlock_abi_reaches_shell_enforcement_from_the_probe() {
        let negative = unprobed();
        for abi in [None, Some(1), Some(8)] {
            let enforcement = ShellEnforcement::resolve_declared(
                &CapabilityPolicy::default(),
                murmur_artifact::ContainmentClass::Advisory,
                negative.clone().with_landlock_kernel_abi(abi),
            )
            .unwrap()
            .with_test_session_dirs();
            assert_eq!(
                enforcement.landlock_abi,
                ruleset_landlock_abi(abi, enforcement.tier),
                "kernel ABI {abi:?}"
            );
            // An all-negative probe resolves below the Landlock tiers on every OS.
            assert_eq!(enforcement.landlock_abi, None);
        }
        if cfg!(target_os = "linux") {
            let full = HostProbe::from_parts(Some(true), sealed_incapable());
            let enforcement = ShellEnforcement::resolve_declared(
                &CapabilityPolicy::default(),
                murmur_artifact::ContainmentClass::Scoped,
                full.with_landlock_kernel_abi(Some(8)),
            )
            .unwrap()
            .with_test_session_dirs();
            assert_eq!(enforcement.tier, EnforcementTier::KernelFull);
            assert_eq!(enforcement.landlock_abi, Some(LANDLOCK_TARGET_ABI));
        }
    }

    /// The `mur doctor` Landlock block for each kernel ABI that changes what it says.
    #[test]
    fn landlock_doctor_lines_name_the_kernel_and_ruleset_abi() {
        if !cfg!(target_os = "linux") {
            for abi in [None, Some(1), Some(8)] {
                assert_eq!(
                    landlock_doctor_lines(abi),
                    vec!["kernel ABI: n/a (Landlock is a Linux mechanism)".to_string()]
                );
            }
            return;
        }

        let none = landlock_doctor_lines(None).join("\n");
        assert!(none.contains("no Landlock on this host"), "{none}");
        assert_eq!(landlock_doctor_lines(Some(0)), landlock_doctor_lines(None));

        let v1 = landlock_doctor_lines(Some(1)).join("\n");
        assert!(v1.contains("kernel ABI: 1"), "{v1}");
        assert!(v1.contains("ruleset ABI: 1"), "{v1}");
        assert!(v1.contains("EXDEV"), "{v1}");
        assert!(v1.contains("Invalid cross-device link"), "{v1}");
        assert!(v1.contains("Linux 5.19 (Landlock ABI v2)"), "{v1}");
        assert!(v1.contains("uv"), "{v1}");

        let v2 = landlock_doctor_lines(Some(2)).join("\n");
        assert!(v2.contains("kernel ABI: 2"), "{v2}");
        assert!(v2.contains("ruleset ABI: 2"), "{v2}");
        assert!(!v2.contains("EXDEV"), "{v2}");
        assert!(
            v2.contains("not mediated below Landlock ABI v3 (Linux 6.2)"),
            "{v2}"
        );

        for kernel in [3, 8] {
            let lines = landlock_doctor_lines(Some(kernel));
            assert_eq!(
                lines,
                vec![
                    format!("kernel ABI: {kernel}"),
                    "ruleset ABI: 3 (the kernel's, capped at 3)".to_string(),
                ]
            );
        }
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
                    InterpreterRuntimeDir {
                        path: "/usr/lib/python3.11".to_string(),
                        list_dir: true,
                    },
                    InterpreterRuntimeDir {
                        path: "/opt/py/lib".to_string(),
                        list_dir: true,
                    },
                    // A duplicate, and a relative path that names nothing absolute.
                    InterpreterRuntimeDir {
                        path: "/opt/py/lib".to_string(),
                        list_dir: false,
                    },
                    InterpreterRuntimeDir {
                        path: "relative/lib".to_string(),
                        list_dir: false,
                    },
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
            vec![
                PathBuf::from("/opt/toolchain/bin"),
                PathBuf::from("/opt/py/lib")
            ]
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
                pinned: None,
            }],
            "a staged tree is walked by the runtime that uses it, so it must be listable",
        );
        assert_eq!(
            grants
                .iter()
                .map(|grant| grant.path.clone())
                .collect::<Vec<_>>(),
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
                    pinned: None,
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

        let scoped = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            HostProbe::probe(),
        )
        .expect("an empty policy resolves")
        .with_test_session_dirs();
        assert!(
            !scoped.landlock_grants.iter().any(|grant| grant.path == usr),
            "a scoped capsule runs Landlock over the real host filesystem — /usr must stay \
             unenumerable there",
        );

        let sealed = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Sealed,
            HostProbe::probe(),
        )
        .expect("an empty policy resolves")
        .with_test_session_dirs();
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
    fn every_sealed_etc_path_gets_a_non_executable_landlock_grant() {
        let grants = resolve_sealed_etc_landlock_grants(EnforcementTier::KernelSealed);

        assert_eq!(
            grants,
            crate::sealed::SEALED_ETC_PATHS
                .iter()
                .map(|entry| LandlockGrant {
                    path: PathBuf::from(entry.path),
                    list_dir: entry.directory,
                    executable: false,
                    pinned: None,
                })
                .collect::<Vec<_>>(),
            "the composed root binds exactly these /etc entries, so exactly these must be \
             readable — the bound set and the granted set must not drift apart",
        );
        assert!(
            grants.iter().all(|grant| !grant.executable),
            "Execute is the exec allowlist, and nothing in /etc is a program a manifest asked to \
             run — /etc/alternatives is a directory of symlinks into /usr/bin",
        );

        // The `directory` classification itself, spelled out independently of the constant so a
        // silent reclassification (which would cost an entry its ReadDir, or claim ReadDir on a
        // regular file) shows up as a test failure rather than as a TLS bug on a real host.
        let listable: Vec<&str> = grants
            .iter()
            .filter(|grant| grant.list_dir)
            .map(|grant| grant.path.to_str().unwrap())
            .collect();
        assert_eq!(
            listable,
            vec![
                "/etc/ld.so.conf.d",
                "/etc/alternatives",
                "/etc/ssl",
                "/etc/pki",
                "/etc/ca-certificates",
                "/etc/terminfo",
            ],
            "ReadDir belongs to exactly the directory entries: os.listdir('/etc/ssl/certs') and \
             the c_rehash symlink lookup need it, and a regular file has no meaningful ReadDir",
        );
    }

    #[test]
    fn sealed_etc_landlock_grants_are_sealed_tier_only() {
        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            assert!(
                resolve_sealed_etc_landlock_grants(tier).is_empty(),
                "{tier:?} has no composed root, so these rules would hand out the real host's \
                 /etc — its trust store, resolv.conf and account databases",
            );
        }
    }

    /// The gate as `ShellEnforcement::resolve` actually applies it, modelled on
    /// `shell_enforcement_grants_sealed_runtime_paths_only_when_sealed_applies`: the `scoped` half
    /// is host-independent (`applied_tier` never returns `KernelSealed` for a `scoped`
    /// declaration), and the `sealed` half asserts against whatever tier this host resolved to.
    #[test]
    fn shell_enforcement_grants_sealed_etc_paths_only_when_sealed_applies() {
        let policy = CapabilityPolicy::default();
        let is_etc_grant = |grant: &LandlockGrant| {
            crate::sealed::SEALED_ETC_PATHS
                .iter()
                .any(|entry| Path::new(entry.path) == grant.path)
        };

        let scoped = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            HostProbe::probe(),
        )
        .expect("an empty policy resolves")
        .with_test_session_dirs();
        assert!(
            !scoped.landlock_grants.iter().any(is_etc_grant),
            "a scoped capsule runs Landlock over the real host filesystem — /etc/shadow's \
             neighbours must not become readable there",
        );

        let sealed = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Sealed,
            HostProbe::probe(),
        )
        .expect("an empty policy resolves")
        .with_test_session_dirs();
        let granted: Vec<&LandlockGrant> = sealed
            .landlock_grants
            .iter()
            .filter(|grant| is_etc_grant(grant))
            .collect();
        if sealed.tier == EnforcementTier::KernelSealed {
            assert_eq!(
                granted.len(),
                crate::sealed::SEALED_ETC_PATHS.len(),
                "a sealed session must carry one read grant per bound /etc entry",
            );
            assert!(granted.iter().all(|grant| !grant.executable));
        } else {
            assert!(
                granted.is_empty(),
                "this host fell back to {:?}, where there is no composed root and /etc is the \
                 host's own",
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
        assert!(
            !ips.is_empty(),
            "localhost should resolve to at least one address"
        );
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
        let ips =
            resolve_network_allowlist_ips(&["definitely-not-a-real-host.invalid".to_string()])
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
        let ips =
            resolve_network_allowlist_ips(&["localhost".to_string(), "localhost:8080".to_string()])
                .unwrap();
        let mut sorted = ips.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            ips.len(),
            sorted.len(),
            "resolved IPs should already be deduplicated"
        );
    }

    #[test]
    fn environment_only_has_no_resolved_ips_and_environment_only_tier() {
        let enforcement = ShellEnforcement::environment_only();
        assert_eq!(enforcement.tier, EnforcementTier::EnvironmentOnly);
        assert!(enforcement.network_allow_ips.is_empty());
    }

    /// With no scope a spawn's `RLIMIT_NPROC` is headroom over a real count. A `0` baseline
    /// would turn `max_processes` into an absolute per-uid ceiling that an interactive account
    /// is already past, failing the subprocess's first `fork()`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn nproc_bound_without_a_scope_is_headroom_over_a_live_count() {
        match ShellEnforcement::environment_only()
            .with_test_session_dirs()
            .nproc_bound()
        {
            crate::resources::NprocBound::Headroom { baseline } => assert!(
                baseline > 0,
                "the test process is owned by this uid, so the count cannot be zero"
            ),
            other => panic!("no scope must give headroom, got {other:?}"),
        }
    }

    /// The baseline is taken on each call, not when the enforcement was built: threads the host
    /// starts in between are counted by the next spawn.
    #[cfg(target_os = "linux")]
    #[test]
    fn nproc_bound_without_a_scope_is_measured_on_each_call() {
        let _lock = crate::resources::test_support::host_threads_lock();
        let enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        let baseline = |bound| match bound {
            crate::resources::NprocBound::Headroom { baseline } => baseline,
            other => panic!("no scope must give headroom, got {other:?}"),
        };

        let first = baseline(enforcement.nproc_bound());
        let holders = crate::resources::test_support::HeldThreads::start(200);
        let second = baseline(enforcement.nproc_bound());
        drop(holders);

        assert!(
            second > first,
            "200 threads started between the calls must raise the baseline: {first} then {second}"
        );
    }

    /// A spawn that will join a live scope leaves `RLIMIT_NPROC` to `pids.max`.
    #[cfg(target_os = "linux")]
    #[test]
    fn nproc_bound_with_a_scope_leaves_rlimit_nproc_to_pids_max() {
        if !crate::cgroup::cgroup_delegation_available() {
            crate::runtime_err!(
                "[SKIP-HOST] nproc_bound_with_a_scope_leaves_rlimit_nproc_to_pids_max: this host \
                 cannot delegate a cgroup v2 scope"
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let limits = crate::resources::HostResourceLimits::default();
        let scope = crate::cgroup::prepare_scope(true, &limits, "ses_nproc_bound", temp.path())
            .expect("this host delegates a cgroup scope")
            .scope;
        let enforcement = ShellEnforcement::environment_only()
            .with_test_session_dirs()
            .with_host_bounding(scope, None);

        assert_eq!(
            enforcement.nproc_bound(),
            crate::resources::NprocBound::CgroupPidsMax
        );
    }

    /// A Linux capsule with a cgroup scope gets no `RLIMIT_NPROC` from the runtime, so threads the
    /// rest of the host starts after the enforcement was resolved cannot spend its fork headroom.
    /// The 400 holders put the uid well past any `RLIMIT_NPROC` derived from a count taken before
    /// they started, which fails the first `fork()` in the subprocess with `EAGAIN`. The 40
    /// concurrent `sleep`s and the concurrent `node` children each need headroom well past the
    /// declared `max_processes` of 32.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_scoped_capsule_keeps_spawning_after_the_host_adds_threads() {
        const TEST: &str = "a_scoped_capsule_keeps_spawning_after_the_host_adds_threads";
        if !crate::cgroup::cgroup_delegation_available() {
            crate::runtime_err!("[SKIP-HOST] {TEST}: this host cannot delegate a cgroup v2 scope");
            return;
        }
        let _lock = crate::resources::test_support::host_threads_lock();

        let temp = tempfile::tempdir().unwrap();
        let limits = crate::resources::HostResourceLimits {
            max_processes: 32,
            max_open_files: 256,
            ..crate::resources::HostResourceLimits::default()
        };
        let scope = crate::cgroup::prepare_scope(true, &limits, "ses_nproc_race", temp.path())
            .expect("this host delegates a cgroup scope")
            .scope;
        assert!(
            scope.is_some(),
            "a required scope on Linux is always created"
        );
        let node_available = crate::sandbox::resolve_invoked_binary_path("node") != "node";
        let mut shell_allow = vec!["bash".to_string()];
        if node_available {
            shell_allow.push("node".to_string());
        }
        let policy = CapabilityPolicy {
            shell_allow,
            resources: limits,
            ..CapabilityPolicy::default()
        };
        let mut enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        enforcement.resource_limits = limits;
        let enforcement = enforcement.with_host_bounding(scope, None);

        let holders = crate::resources::test_support::HeldThreads::start(400);

        let result = crate::shell::execute_shell(
            "bash",
            &[
                "-c",
                "for i in $(seq 1 40); do sleep 0.2 & done; wait; ulimit -Hu; ulimit -Hn",
            ],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("bash must run");
        assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        assert_eq!(result.resource_limit_hit, None);
        let (_, own_hard) = crate::resources::test_support::own_rlimit_nproc();
        let lines: Vec<&str> = result.stdout.lines().collect();
        assert_eq!(
            lines,
            [
                crate::resources::test_support::rendered_rlimit(own_hard).as_str(),
                "256"
            ],
            "RLIMIT_NPROC is inherited unchanged and RLIMIT_NOFILE is the declared ceiling",
        );

        if node_available {
            let script = "const { spawn } = require('child_process');\n\
                const runs = Array.from({ length: 8 }, () => new Promise((resolve) => {\n\
                  const child = spawn(process.execPath, ['-e', 'process.exit(0)']);\n\
                  child.once('error', (error) => resolve(error.message));\n\
                  child.once('exit', (code) => resolve(code === 0 ? null : `exit ${code}`));\n\
                }));\n\
                Promise.all(runs).then((outcomes) => {\n\
                  const failures = outcomes.filter((outcome) => outcome !== null);\n\
                  if (failures.length > 0) { console.error(failures.join('\\n')); process.exit(1); }\n\
                });\n";
            let result = crate::shell::execute_shell(
                "node",
                &["-e", script],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
                &policy,
                &enforcement,
            )
            .expect("node must run");
            assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
        } else {
            crate::runtime_err!("[SKIP-HOST] {TEST}: no node on PATH, skipping the node half");
        }

        drop(holders);
    }

    /// Without a scope `RLIMIT_NPROC` is the only process bound, so its baseline has to follow the
    /// host: 300 threads started after the enforcement was resolved raise the next spawn's
    /// ceiling, and releasing them lowers the one after.
    #[cfg(target_os = "linux")]
    #[test]
    fn without_a_scope_the_nproc_baseline_is_measured_per_spawn() {
        const TEST: &str = "without_a_scope_the_nproc_baseline_is_measured_per_spawn";
        let _lock = crate::resources::test_support::host_threads_lock();

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            resources: crate::resources::HostResourceLimits {
                max_processes: 64,
                ..crate::resources::HostResourceLimits::default()
            },
            ..CapabilityPolicy::default()
        };
        let mut enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        enforcement.resource_limits = policy.resources;

        let before = crate::resources::uid_task_count().expect("Linux can count the uid's tasks");
        let (_, own_hard) = crate::resources::test_support::own_rlimit_nproc();
        if own_hard != libc::RLIM_INFINITY && own_hard < before + 664 {
            crate::runtime_err!(
                "[SKIP-HOST] {TEST}: the inherited hard RLIMIT_NPROC is {own_hard}, too close to \
                 the uid's {before} tasks for 300 more threads to show in a child's ceiling"
            );
            return;
        }

        let hard_nproc = || -> u64 {
            let result = crate::shell::execute_shell(
                "bash",
                &["-c", "ulimit -Hu"],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
                &policy,
                &enforcement,
            )
            .expect("bash must run");
            assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
            result.stdout.trim().parse().expect("a finite ceiling")
        };

        let holders = crate::resources::test_support::HeldThreads::start(300);
        let held = hard_nproc();
        assert!(
            held >= before + 64 + 150,
            "the ceiling must count the holder threads started after the enforcement was \
             resolved: {held} with {before} tasks before them"
        );

        drop(holders);
        let released = hard_nproc();
        assert!(
            released < held,
            "the ceiling must drop once the holders exit: {released} after, {held} while held"
        );
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
    fn resolve_exec_allowlist_canonicalizes_path_matches_through_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-tool");
        make_executable(&real, "#!/bin/sh\nexit 0\n");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir(&bin_dir).unwrap();
        std::os::unix::fs::symlink(&real, bin_dir.join("link-tool")).unwrap();

        let resolved = resolve_exec_allowlist_in(&["link-tool".to_string()], &[bin_dir]);
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
            phdrs.push((
                3,
                interp_offset as u64,
                interp_offset as u64,
                i.len() as u64 + 1,
            ));
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
        assert_eq!(
            deps.needed,
            vec!["libc.so.6".to_string(), "libm.so.6".to_string()]
        );
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
        let elf = build_elf64(Some(interp.to_str().unwrap()), &["libfake.so.1"], None);
        std::fs::write(&prog, &elf).unwrap();

        let grants =
            resolve_landlock_grants_in(std::slice::from_ref(&prog), std::slice::from_ref(&lib_dir));

        let expect = |p: PathBuf| std::fs::canonicalize(p).unwrap();
        assert!(
            grants.contains(&expect(prog)),
            "the binary itself must be granted: {grants:?}"
        );
        assert!(
            grants.contains(&expect(interp)),
            "the ELF interpreter must be granted: {grants:?}"
        );
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
                    pinned: None,
                },
                LandlockGrant {
                    path: PathBuf::from("/usr/lib/python3.11/lib-dynload"),
                    list_dir: false,
                    executable: true,
                    pinned: None,
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
                    pinned: None,
                },
                LandlockGrant {
                    path: PathBuf::from("/opt/rb/also-nonexistent"),
                    list_dir: true,
                    executable: true,
                    pinned: None,
                },
            ]
        );
        assert!(resolve_interpreter_runtime_grants(&[]).is_empty());
    }

    #[test]
    fn non_listable_files_marks_every_closure_file_non_enumerable() {
        // The DT_NEEDED closure yields individual files; wrapping them must never set `list_dir`
        // (ReadDir on a regular file is a no-op anyway).
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

    // ---- native tool entrypoints ------------------------------------------------------------

    fn write_entrypoint(dir: &Path, name: &str, head: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, head).unwrap();
        path
    }

    /// `path` as the runtime records a native tool it has just written there, inside `workdir`.
    fn installed(path: &Path, workdir: &Path) -> NativeEntrypoint {
        NativeEntrypoint::derive(path, &std::fs::read(path).unwrap(), workdir)
    }

    #[test]
    fn shebang_interpreter_reads_the_absolute_interpreter_and_nothing_else() {
        let cases: &[(&[u8], Option<&str>)] = &[
            (b"#!/bin/sh\necho hi\n", Some("/bin/sh")),
            (b"#! /usr/bin/python3 -u\n", Some("/usr/bin/python3")),
            // `env` is what the kernel runs; `python3` is found by `env` and needs its own grant.
            (b"#!/usr/bin/env python3\n", Some("/usr/bin/env")),
            (b"#!sh\n", None),
            (b"#!\n", None),
            (b"\x7fELF\x02\x01\x01", None),
            (b"", None),
        ];
        for (head, expected) in cases {
            assert_eq!(
                shebang_interpreter(head),
                expected.map(PathBuf::from),
                "{:?}",
                String::from_utf8_lossy(head)
            );
        }

        // A `#!` line longer than the kernel's own buffer names nothing past it.
        let mut long = b"#!".to_vec();
        long.extend(std::iter::repeat_n(b' ', SHEBANG_READ_LIMIT as usize));
        long.extend(b"/bin/sh\n");
        assert_eq!(shebang_interpreter(&long), None);
    }

    /// The entrypoint, pinned to its inode, and its interpreter are added as `Execute + ReadFile`
    /// file grants, and nothing is taken away from the enforcement they extend.
    #[test]
    fn for_native_entrypoint_adds_the_pinned_entrypoint_and_its_interpreter_only() {
        let dir = tempfile::tempdir().unwrap();
        let entrypoint = write_entrypoint(dir.path(), "tool", b"#!/bin/sh\ncat\n");
        let existing = LandlockGrant {
            path: PathBuf::from("/usr/bin/git"),
            list_dir: false,
            executable: true,
            pinned: None,
        };
        let base = ShellEnforcement {
            tier: EnforcementTier::KernelFull,
            landlock_grants: vec![existing.clone()],
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        };

        let native = base.for_native_entrypoint(&installed(&entrypoint, dir.path()));

        assert_eq!(native.landlock_grants[0], existing);
        let added: Vec<&LandlockGrant> = native.landlock_grants[1..].iter().collect();
        let identity = FileIdentity::of(&std::fs::metadata(&entrypoint).unwrap());
        assert!(
            added
                .iter()
                .any(|grant| grant.path == entrypoint && grant.pinned == Some(identity)),
            "the entrypoint must be granted, pinned to its inode: {added:?}"
        );
        let canonical_sh = std::fs::canonicalize("/bin/sh").unwrap();
        assert!(
            added.iter().any(|grant| grant.path == canonical_sh),
            "/bin/sh must be granted: {added:?}"
        );
        assert!(
            added
                .iter()
                .all(|grant| grant.executable && !grant.list_dir),
            "{added:?}"
        );
        // The closure is files, never a directory such as the workdir or /etc.
        assert!(added.iter().all(|grant| !grant.path.is_dir()), "{added:?}");
        assert!(native.sealed_bind_dirs.is_empty(), "not sealed: no binds");
        assert!(
            Arc::ptr_eq(&native.session_netns, &base.session_netns),
            "a native spawn joins the session's own namespace"
        );
        assert_eq!(
            base.landlock_grants.len(),
            1,
            "the session's grants are untouched"
        );
    }

    /// On `KernelSealed`, an interpreter outside the fixed runtime paths has its directory bound
    /// into the composed root as an optional bind; one inside them adds nothing.
    #[test]
    fn for_native_entrypoint_binds_an_interpreter_outside_the_runtime_paths_when_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let sealed = ShellEnforcement {
            tier: EnforcementTier::KernelSealed,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        };

        let system = installed(
            &write_entrypoint(dir.path(), "system", b"#!/bin/sh\n"),
            dir.path(),
        );
        assert!(sealed
            .for_native_entrypoint(&system)
            .sealed_bind_dirs
            .is_empty());

        let custom = installed(
            &write_entrypoint(dir.path(), "custom", b"#!/opt/tools/bin/run\n"),
            dir.path(),
        );
        assert_eq!(
            sealed.for_native_entrypoint(&custom).sealed_bind_dirs,
            vec![PathBuf::from("/opt/tools/bin")]
        );

        let scoped = ShellEnforcement {
            tier: EnforcementTier::KernelFull,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        };
        assert!(scoped
            .for_native_entrypoint(&custom)
            .sealed_bind_dirs
            .is_empty());
    }

    /// The capsule can rewrite the installed entrypoint between calls. Whatever it writes, the
    /// next spawn is granted exactly what the installed image was: a new `#!` interpreter, on any
    /// tier, adds no exec grant and no composed-root bind.
    #[test]
    fn a_rewritten_entrypoint_gains_no_grant() {
        let dir = tempfile::tempdir().unwrap();
        let entrypoint = write_entrypoint(dir.path(), "tool", b"#!/bin/sh\ncat\n");
        let native = installed(&entrypoint, dir.path());

        for tier in [EnforcementTier::KernelFull, EnforcementTier::KernelSealed] {
            let base = ShellEnforcement {
                tier,
                ..ShellEnforcement::environment_only().with_test_session_dirs()
            };
            let before = base.for_native_entrypoint(&native);

            std::fs::write(&entrypoint, b"#!/opt/elsewhere/bin/run\n").unwrap();
            let after = base.for_native_entrypoint(&native);
            std::fs::write(&entrypoint, b"#!/bin/sh\ncat\n").unwrap();

            assert_eq!(after.landlock_grants, before.landlock_grants, "{tier:?}");
            assert_eq!(after.sealed_bind_dirs, before.sealed_bind_dirs, "{tier:?}");
            assert!(
                after
                    .landlock_grants
                    .iter()
                    .all(|grant| !grant.path.starts_with("/opt/elsewhere")),
                "{tier:?}: {:?}",
                after.landlock_grants
            );
        }
    }

    /// The dependency walk neither grants nor reads a file under the accessible workdir, where the
    /// capsule could plant a library whose own entries widen the closure.
    #[test]
    fn the_entrypoint_closure_does_not_reach_into_the_workdir() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path().join("workdir");
        let tool_dir = workdir.join("tools").join("tool");
        std::fs::create_dir_all(&tool_dir).unwrap();
        let system = temp.path().join("system");
        std::fs::create_dir(&system).unwrap();
        std::fs::write(system.join("libsys.so"), "x").unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("libpicked.so"), "x").unwrap();

        // Planted next to the entrypoint, found through `$ORIGIN`, and naming a file outside.
        std::fs::write(
            tool_dir.join("libplanted.so"),
            build_elf64(
                None,
                &[outside.join("libpicked.so").to_str().unwrap()],
                None,
            ),
        )
        .unwrap();
        // A symlink next to the entrypoint: canonicalising it lands outside the workdir, so only
        // refusing the `$ORIGIN` lookup itself keeps the host file out of the closure.
        std::fs::write(outside.join("host-binary"), "x").unwrap();
        std::os::unix::fs::symlink(outside.join("host-binary"), tool_dir.join("liblinked.so"))
            .unwrap();
        let image = build_elf64(
            None,
            &["libplanted.so", "liblinked.so", "libsys.so"],
            Some("$ORIGIN"),
        );
        let entrypoint = tool_dir.join("tool");
        std::fs::write(&entrypoint, &image).unwrap();

        let native = NativeEntrypoint::derive_in(
            &entrypoint,
            &image,
            &workdir,
            std::slice::from_ref(&system),
        );

        let sys = std::fs::canonicalize(system.join("libsys.so")).unwrap();
        assert_eq!(native.closure, vec![sys]);
    }

    /// A pinned grant whose path no longer opens to the pinned inode is dropped: replacing the
    /// entrypoint, here with a symlink to a host binary, leaves it with no `Execute` rule.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_entrypoint_loses_its_pinned_grant() {
        let accessible = tempfile::tempdir().unwrap();
        let session = accessible
            .path()
            .join(".murmur")
            .join("ses_native_entrypoint");
        std::fs::create_dir_all(&session).unwrap();
        let entrypoint = write_entrypoint(&session, "tool", b"#!/bin/sh\n");
        let native = installed(&entrypoint, accessible.path());
        let grants = ShellEnforcement {
            tier: EnforcementTier::KernelFull,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        }
        .for_native_entrypoint(&native)
        .landlock_grants;
        let open = |grants: &[LandlockGrant]| {
            linux_enforce::open_landlock_fds(linux_enforce::LandlockFdRequest {
                landlock_grants: grants,
                ..linux_enforce::LandlockFdRequest::workdir_only(accessible.path())
            })
            .unwrap()
            .grant_count()
        };
        let installed_count = open(&grants);
        // Holds the installed inode for the rest of the test. Freed, its number may be handed to
        // the next file created on this filesystem, which would then match the pin.
        std::fs::hard_link(&entrypoint, session.join("installed")).unwrap();

        std::fs::remove_file(&entrypoint).unwrap();
        std::os::unix::fs::symlink("/bin/sh", &entrypoint).unwrap();
        assert_eq!(open(&grants), installed_count - 1, "symlink swapped in");

        std::fs::remove_file(&entrypoint).unwrap();
        std::fs::write(&entrypoint, b"#!/bin/sh\n").unwrap();
        assert_eq!(open(&grants), installed_count - 1, "new file, new inode");
    }

    // ---- `CAPSULE_DEVICE_GRANTS` (fixed device set) ----------------------------------------
    //
    // The canonical statement of what the *content* checks in this file are worth, referenced by
    // the `denied_socket_domains` and `SECCOMP_SYSCALL_ALLOWLIST` sections below: they assert what
    // a Rust constant holds, NOT that any kernel grants or refuses anything. A green run is not
    // evidence that `/dev/null` is writable inside a capsule or that `/dev/random` is refused —
    // on a host below `KernelFull`, `apply_landlock_scope` never runs at all. A content check
    // buys one thing: changing a constant cannot happen by accident.
    //
    // The enforcement claim for this constant is verified only by the manual procedure in
    // `docs/content/reference/capsule-device-set-manual-verification.md`, on real,
    // uncontainerized Linux hardware.

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
             the diagnostics reference page)."
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
    // Content checks — see the `CAPSULE_DEVICE_GRANTS` section above for what that is and is not
    // worth. That `socket(AF_UNIX, ...)` is actually denied on a real host is verified only by the
    // manual procedure in `docs/content/reference/af-unix-sockets-manual-verification.md`, on
    // real Linux hardware.

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
    // Content checks — see the `CAPSULE_DEVICE_GRANTS` section above for what that is and is not
    // worth. These assert the allowlist's contents, not that the kernel denies anything. They buy
    // one thing: re-permitting a dangerous syscall cannot happen by accident while reconciling
    // the allowlist against a newer upstream profile.

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

    /// `execve`/`execveat` must be *present*: the default-deny action applies to anything unnamed,
    /// so omitting them would refuse the child's own first `execve` and no capsule could run a
    /// shell tool at all. Exec is decided by the Landlock domain instead; see
    /// `linux_enforce::workdir_access_rights` and `resolve_landlock_grants`.
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
            "read",
            "write",
            "openat",
            "close",
            "mmap",
            "munmap",
            "brk",
            "exit_group",
            "futex",
            "rt_sigreturn",
            "clone",
            "wait4",
            "getpid",
            "getdents64",
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
    fn tier_warning_kernel_full_warns_when_a_shell_is_declared() {
        // The Linux "full" tier must NOT be silent: it is a filesystem and syscall scope, not a
        // namespace, and a silent tier would imply it has no documented limits. It warns when a
        // shell is declared, and stays silent only when there is no subprocess to enforce on.
        assert_eq!(
            tier_warning(EnforcementTier::KernelFull, false),
            Some((W_SEC_005, KERNEL_ENFORCEMENT_NOTICE))
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
        warn_for_missing_aggregate_bounding(
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            true,
            false,
        );
        let log = bootstrap_log_contents(temp.path());

        if cfg!(target_os = "linux") {
            assert!(
                log.is_empty(),
                "Linux refuses the launch instead of warning: {log}"
            );
        } else {
            assert!(log.contains(W_SEC_010), "log was: {log}");
            assert!(
                log.contains(&security_warning_link(W_SEC_010)),
                "log was: {log}"
            );
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
    fn warn_for_enforcement_tier_kernel_full_logs_scope_when_shell_declared() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::KernelFull,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &warning_test_policy(),
        );
        let log = bootstrap_log_contents(temp.path());
        assert!(
            log.contains(W_SEC_005),
            "KernelFull must not be silent — it must state what the tier does and does not cover: {log}"
        );
        assert!(
            log.contains("denied, not absent"),
            "must state the scope's documented limit: paths outside the workdir are denied, not \
             absent: {log}"
        );
        assert!(
            log.contains("read+execute") && log.contains("outside the workdir"),
            "must describe what is now actually granted (narrow, derived read+execute outside \
             the workdir), not an unfixed grant bug: {log}"
        );
        assert!(
            log.contains(&security_warning_link(W_SEC_005)),
            "must link to the diagnostics doc page: {log}"
        );
    }

    #[test]
    fn warn_for_enforcement_tier_kernel_full_silent_without_shell_allow() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::KernelFull,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &warning_test_policy(),
        );
        let log = bootstrap_log_contents(temp.path());
        assert!(
            log.contains("Landlock"),
            "must name the missing primitive: {log}"
        );
        assert!(
            log.contains("filesystem"),
            "must name what is not enforced: {log}"
        );
        assert!(
            log.contains(W_SEC_002),
            "must carry its warning code: {log}"
        );
        assert!(
            log.contains(&security_warning_link(W_SEC_002)),
            "must link to the diagnostics doc page: {log}"
        );
    }

    #[test]
    fn warn_for_enforcement_tier_environment_only_logs_permanent_and_bypass_warnings() {
        let temp = tempfile::tempdir().unwrap();
        warn_for_enforcement_tier(
            EnforcementTier::EnvironmentOnly,
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        assert!(
            log.contains(W_SEC_001),
            "must carry the tier warning code: {log}"
        );
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
                &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
                &policy,
                &ShellEnforcement::environment_only().with_test_session_dirs(),
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &ShellEnforcement::environment_only().with_test_session_dirs(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(marker.exists());
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
#[allow(clippy::print_stdout, clippy::print_stderr)]
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
        let before = crate::namespace_probe::probe_namespaces().sealed_root;
        let _ = super::linux_enforce::probe_landlock_full_access();
        let after = crate::namespace_probe::probe_namespaces().sealed_root;

        assert_eq!(
            before, after,
            "the Landlock probe changed what the sealed probe reports ({before:?} -> {after:?}) — \
             it has restricted the calling process instead of a forked child, which permanently \
             forbids the mount family to this process and everything it spawns",
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::print_stdout, clippy::print_stderr)]
mod linux_integration_tests {
    use std::path::Path;

    use super::*;

    /// Spread-base supplying the three *host-bounding* fields (`resource_limits`,
    /// `cgroup_scope`, `workdir_guard`) to the enforcement literals below, which spell out the
    /// policy half by hand because they need to pin `tier` explicitly.
    ///
    /// These tests exercise seccomp, Landlock and the network allowlist — not rlimits or
    /// cgroups — so they take exactly what `ShellEnforcement::resolve` produces before
    /// `with_host_bounding` attaches a live session's handles: the real default ceilings (not
    /// "unbounded" — zeroing them would misrepresent what a real host does, and the rlimit half
    /// of the child `pre_exec` runs on every tier), and no cgroup scope or workdir guard, so each
    /// spawn gets `RLIMIT_NPROC` headroom over the uid's count at that spawn. The `tier` it
    /// carries is always overridden by the literal that spreads it; only the host-bounding tail
    /// is ever consumed from here.
    fn host_bounding_base() -> ShellEnforcement {
        // Every session holds a runtime directory, and a sealed one cannot compose a root
        // without it. Kept past the test: a clone may outlive the value.
        let base = tempfile::tempdir().unwrap().keep();
        ShellEnforcement::environment_only().with_runtime_dir(test_runtime_dir(&base), None)
    }

    /// Pure content check on the workdir access-right set: no kernel call, no fork, no spawn.
    /// It pins *which* rights of [`LANDLOCK_TARGET_ABI`] `apply_landlock_scope` hands the workdir,
    /// so re-adding device-node creation, or losing the move and truncate rights, becomes a test
    /// failure instead of a silent regression.
    ///
    /// It proves **nothing** about whether the kernel enforces that set — that is the manual
    /// acceptance procedure in
    /// `docs/content/reference/workdir-device-node-manual-verification.md` and the
    /// `kernel_tier_*` tests below, not this test.
    /// It lives in this `#[cfg(target_os = "linux")]` module rather than the cross-platform
    /// `tests` module only because `landlock::AccessFs` is a Linux-only dependency.
    #[test]
    fn workdir_landlock_grant_withholds_device_and_socket_creation() {
        use landlock::{Access, AccessFs, ABI};

        assert_eq!(
            LANDLOCK_TARGET_ABI, 3,
            "this test pins the ABI v3 set; raising the cap needs this test revisited with it"
        );
        let all_v3 = AccessFs::from_all(ABI::V3);
        // The `workdir_exec: true` set — the widest the workdir can ever be — so the three
        // unconditionally-withheld rights below are checked against the permissive case rather
        // than passing trivially because `Execute` happened to be absent too.
        let granted = linux_enforce::workdir_access_rights(true);

        for withheld in [AccessFs::MakeChar, AccessFs::MakeBlock, AccessFs::MakeSock] {
            assert!(
                all_v3.contains(withheld),
                "{withheld:?} is expected to be part of Landlock ABI v3 — if it is not, this \
                 test's premise (and the workdir grant's comment) needs revisiting"
            );
            assert!(
                !granted.contains(withheld),
                "the workdir grant must withhold {withheld:?}: with the full ABI set declared \
                 by handle_access, withholding it here is what makes it denied rather than merely \
                 un-granted"
            );
        }

        for added in [AccessFs::Refer, AccessFs::Truncate] {
            assert!(
                granted.contains(added),
                "the workdir grant must carry {added:?}: without Refer a move between workdir \
                 folders is EXDEV, and without Truncate a v3 ruleset refuses truncating the \
                 capsule's own files"
            );
        }

        assert!(
            granted.contains(AccessFs::MakeFifo),
            "MakeFifo stays granted — real build tooling creates named pipes in its working tree"
        );

        let expected = all_v3 & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::MakeSock);
        assert_eq!(
            granted, expected,
            "the workdir_exec grant must be exactly Landlock ABI v3 minus \
             MakeChar/MakeBlock/MakeSock — nothing else may be added or dropped without updating \
             this test and the constant's comment together"
        );
    }

    /// The workdir right set as a ruleset built at ABI v1 sees it, after `apply_landlock_scope`
    /// masks it to the handled set: exactly ABI v1 minus the three `Make*` rights, with neither
    /// `Refer` nor `Truncate`, so a 5.13–5.18 kernel is handed no right it does not know.
    #[test]
    fn workdir_rights_masked_to_abi_v1_equal_the_pre_refer_set() {
        use landlock::{Access, AccessFs, ABI};

        let all_v1 = AccessFs::from_all(ABI::V1);
        let masked = linux_enforce::workdir_access_rights(true) & all_v1;

        assert_eq!(
            masked,
            all_v1 & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::MakeSock),
            "masked to ABI v1, the workdir grant must be the v1 set minus the three Make* rights"
        );
        assert!(!masked.contains(AccessFs::Refer));
        assert!(!masked.contains(AccessFs::Truncate));
        assert_eq!(
            linux_enforce::workdir_access_rights(false) & all_v1,
            masked & !AccessFs::Execute,
            "masking must not move the workdir_exec axis"
        );
    }

    /// The `workdir_exec` bit, pinned in both directions. A *content* check on the right set
    /// only: it proves nothing about whether a kernel refuses an exec, which is the manual
    /// procedure in
    /// `docs/content/reference/workdir-exec-landlock-manual-verification.md`. It catches the one
    /// regression that matters here: flipping the default back — which would silently reopen the
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

    /// A sealed enforcement over a fresh runtime directory beside `accessible`.
    fn sealed_with_runtime_dir(base: &Path) -> ShellEnforcement {
        ShellEnforcement {
            tier: EnforcementTier::KernelSealed,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        }
        .with_runtime_dir(test_runtime_dir(base), None)
    }

    /// The session's temporary folder's descriptor, as `prepare_enforcement` takes it from the
    /// held root.
    fn temp_dir_fd_of(enforcement: &ShellEnforcement) -> Option<Arc<std::os::fd::OwnedFd>> {
        enforcement
            .runtime_dir
            .as_ref()
            .and_then(|roots| roots.temp.as_ref())
            .map(|temp| Arc::new(temp.path_fd().unwrap()))
    }

    /// The session's home folder's descriptor, as `prepare_enforcement` takes it from the held
    /// root.
    fn home_dir_fd_of(enforcement: &ShellEnforcement) -> Option<Arc<std::os::fd::OwnedFd>> {
        enforcement
            .home_root()
            .map(|home| Arc::new(home.path_fd().unwrap()))
    }

    /// The sealed sources take the session's temporary folder from the runtime directory roots,
    /// bind it from its own path, and add nothing to either workdir; the Landlock fds carry the
    /// folder's rule whenever one is handed in. Opening an fd is an ordinary `open()` — no
    /// Landlock call, no namespace, no privilege — so this runs on any Linux host.
    ///
    /// It proves **nothing** about whether the kernel then honors the rule built from that fd;
    /// the live tests below and the hand-run procedure in
    /// `docs/content/reference/sealed-containment-manual-verification.md` do.
    #[test]
    fn the_temp_folder_and_runtime_dir_are_opened_for_landlock_and_the_sealed_bind() {
        let accessible = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let enforcement = sealed_with_runtime_dir(runtime.path());
        let temp_dir_fd = temp_dir_fd_of(&enforcement);
        let sources = SealedSourceFds::open(
            &enforcement,
            temp_dir_fd.as_ref(),
            home_dir_fd_of(&enforcement).as_ref(),
            None,
        )
        .expect("a real, writable workdir must yield the sealed sources");
        assert_eq!(
            sources.tmp_store_path,
            enforcement.temp_dir_path().unwrap(),
            "the composed root binds the session's temporary folder at /tmp"
        );
        assert_eq!(
            sources.home_dir.1,
            enforcement.home_dir_path().unwrap(),
            "the composed root binds the session's home folder at its own path"
        );
        let fds = linux_enforce::open_landlock_fds(linux_enforce::LandlockFdRequest {
            sealed: Some(&sources),
            temp_dir_fd,
            home_dir_fd: Some(Arc::clone(&sources.home_dir.0)),
            runtime_dir_fd: Some(Arc::clone(&sources.runtime_dir)),
            records_dir_fd: sources.records_dir.as_ref().map(|(fd, _)| Arc::clone(fd)),
            ..linux_enforce::LandlockFdRequest::workdir_only(accessible.path())
        })
        .expect("a real, writable workdir must yield sealed Landlock fds");

        assert!(fds.temp_dir_fd().is_some());
        assert!(
            fds.home_dir_fd().is_some(),
            "the home folder takes the temporary folder's rule"
        );
        assert!(fds.runtime_dir_fd().is_some());
        assert!(
            fds.records_dir_fd().is_some(),
            "the records directory takes the runtime directory's read-only rule"
        );
        assert_eq!(
            std::fs::read_dir(accessible.path()).unwrap().count(),
            0,
            "enabling sealed must add nothing to the work folder"
        );

        let fds = linux_enforce::open_landlock_fds(linux_enforce::LandlockFdRequest::workdir_only(
            accessible.path(),
        ))
        .expect("a real, writable workdir must yield Landlock fds on any tier");
        assert!(
            fds.temp_dir_fd().is_none(),
            "an enforcement holding no temporary folder grants none"
        );
        assert!(fds.home_dir_fd().is_none());
        assert!(fds.children_records_fd().is_none());
    }

    /// A sealed root cannot be composed without the session's temporary folder: the refusal is
    /// the parent's, names the folder, and comes before anything is spawned.
    #[test]
    fn a_sealed_root_needs_the_temp_folder() {
        let runtime = tempfile::tempdir().unwrap();
        let mut roots = test_runtime_dir(runtime.path());
        roots.temp = None;
        let enforcement = ShellEnforcement {
            tier: EnforcementTier::KernelSealed,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        }
        .with_runtime_dir(roots, None);
        let error = SealedSourceFds::open(
            &enforcement,
            None,
            home_dir_fd_of(&enforcement).as_ref(),
            None,
        )
        .err()
        .expect("refused");
        assert!(error.contains("temporary folder"), "{error}");
    }

    /// Nor without its home folder: the refusal names the missing home folder, and comes before
    /// anything is spawned or written.
    #[test]
    fn no_home_folder_refuses_a_sealed_root() {
        let runtime = tempfile::tempdir().unwrap();
        let roots = crate::runtime_dir::RuntimeDirRoots {
            home: None,
            ..test_runtime_dir(runtime.path())
        };
        let enforcement = ShellEnforcement {
            tier: EnforcementTier::KernelSealed,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        }
        .with_runtime_dir(roots, None);
        let error = SealedSourceFds::open(
            &enforcement,
            temp_dir_fd_of(&enforcement).as_ref(),
            None,
            None,
        )
        .err()
        .expect("refused");
        assert!(error.contains("no home folder"), "{error}");
        assert!(!runtime
            .path()
            .join(crate::runtime_dir::SEALED_ETC_SUBDIR)
            .join("passwd")
            .exists());
    }

    /// The synthetic account databases, written into `sealed-etc/` beside the runtime directory
    /// and opened by the same parent-side call.
    ///
    /// Like the temporary-folder test above it proves nothing about kernel enforcement — that is
    /// the hand-run in `docs/content/reference/sealed-containment-manual-verification.md`. It
    /// catches that the files exist before `build_sealed_root` binds them, that there is an fd to
    /// build each rule from, and that nothing is written into either workdir for them.
    #[test]
    fn the_synthetic_etc_files_are_written_into_sealed_etc_and_opened() {
        let accessible = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let enforcement = sealed_with_runtime_dir(runtime.path());
        let sources = SealedSourceFds::open(
            &enforcement,
            temp_dir_fd_of(&enforcement).as_ref(),
            home_dir_fd_of(&enforcement).as_ref(),
            None,
        )
        .unwrap();
        let fds = linux_enforce::open_landlock_fds(linux_enforce::LandlockFdRequest {
            sealed: Some(&sources),
            ..linux_enforce::LandlockFdRequest::workdir_only(accessible.path())
        })
        .expect("a real, writable workdir must yield sealed Landlock fds");

        let synthetic: Vec<&crate::sealed::SealedEtcPath> = crate::sealed::SEALED_ETC_PATHS
            .iter()
            .filter(|entry| entry.synthetic.is_some())
            .collect();
        assert_eq!(
            fds.sealed_identity_fd_count(),
            synthetic.len(),
            "every synthetic /etc entry the composed root binds needs a rule of its own: {synthetic:?}",
        );
        let etc_dir = runtime.path().join(crate::runtime_dir::SEALED_ETC_SUBDIR);
        for entry in &synthetic {
            let path = crate::sealed::synthetic_etc_source(&etc_dir, entry.synthetic.unwrap());
            assert!(
                path.is_file(),
                "{} must be written into sealed-etc/",
                entry.path
            );
        }
        assert!(
            !accessible.path().join(".mur-etc").exists(),
            "the work folder gained .mur-etc"
        );

        let fds = linux_enforce::open_landlock_fds(linux_enforce::LandlockFdRequest::workdir_only(
            accessible.path(),
        ))
        .unwrap();
        assert_eq!(fds.sealed_identity_fd_count(), 0);
    }

    /// A sealed root cannot be composed without the session's runtime directory: the refusal is
    /// the parent's, before anything is spawned.
    #[test]
    fn a_sealed_root_needs_the_runtime_directory() {
        let enforcement = ShellEnforcement {
            tier: EnforcementTier::KernelSealed,
            ..ShellEnforcement::environment_only()
        };
        let error = SealedSourceFds::open(&enforcement, None, None, None)
            .err()
            .expect("refused");
        assert!(error.contains("runtime directory"), "{error}");
    }

    /// The one field that has to agree across two files: the synthetic `pw_dir` and `$HOME`.
    ///
    /// Both are the session's home folder's absolute path, `<records>/home`, and this asserts the
    /// end result — a capsule sees `pwd.getpwuid(os.getuid()).pw_dir` and `os.environ["HOME"]`,
    /// and a disagreement between them is silent until a tool picks the wrong one. The uid/gid
    /// are checked against the home folder's own owner, which is where
    /// `ensure_sealed_identity_files` reads the capsule's identity from.
    #[test]
    fn home_folder_is_the_synthetic_passwd_entrys_home_and_home() {
        use std::os::unix::fs::MetadataExt;

        let runtime = tempfile::tempdir().unwrap();
        let roots = test_runtime_dir(runtime.path());
        let home = roots.home.clone().unwrap();
        let written = ensure_sealed_identity_files(&roots.sealed_etc, &home).unwrap();
        let env = crate::shell::build_shell_env(
            &crate::types::CapabilityPolicy::default(),
            &[],
            Some(home.path()),
            None,
            None,
            None,
        )
        .unwrap();

        let passwd = std::fs::read_to_string(&written[0]).unwrap();
        let owner = std::fs::metadata(home.path()).unwrap();
        let entry: Vec<&str> = passwd
            .lines()
            .last()
            .expect("the capsule's own entry is the last line")
            .split(':')
            .collect();

        assert_eq!(entry[2], owner.uid().to_string(), "uid field");
        assert_eq!(entry[3], owner.gid().to_string(), "gid field");
        assert_eq!(
            Some(&entry[5].to_string()),
            env.get("HOME"),
            "pw_dir and $HOME are the same string or the entry is worse than useless",
        );
        assert_eq!(
            entry[5],
            runtime
                .path()
                .join(crate::records_dir::RECORDS_DIR_NAME)
                .join(crate::records_dir::HOME_SUBDIR)
                .to_str()
                .unwrap()
        );

        let group = std::fs::read_to_string(&written[1]).unwrap();
        assert_eq!(
            group.lines().last().unwrap().split(':').nth(2),
            Some(owner.gid().to_string().as_str()),
            "gid field of the capsule's own group",
        );
    }

    /// A workdir root over `work/` beside an `outside/` directory holding one sentinel file.
    fn sealed_planted() -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        crate::workdir_fs::WorkdirRoot,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let accessible = dir.path().join("work");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&accessible).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel.txt"), b"operator data").unwrap();
        let root = crate::workdir_fs::WorkdirRoot::open(&accessible).unwrap();
        (dir, root.path().to_path_buf(), outside, root)
    }

    /// Staging twice rewrites `sealed-etc/passwd` in place: the inode a Landlock rule was bound to
    /// on the first call is the file the second call leaves.
    #[test]
    fn sealed_identity_files_keep_their_inode() {
        use std::os::unix::fs::MetadataExt;

        let (_dir, _session, _outside, root) = sealed_planted();
        let runtime = tempfile::tempdir().unwrap();
        let roots = test_runtime_dir(runtime.path());
        let passwd = roots.sealed_etc.path().join("passwd");
        ensure_sealed_identity_files(&roots.sealed_etc, &root).unwrap();
        let first = std::fs::metadata(&passwd).unwrap().ino();
        ensure_sealed_identity_files(&roots.sealed_etc, &root).unwrap();
        assert_eq!(std::fs::metadata(&passwd).unwrap().ino(), first);
        assert!(root.take_refusals().is_empty());
    }

    /// Pure content check on the right-set the composed root's `/tmp` rule carries: no kernel call,
    /// no fork, no spawn.
    ///
    /// `apply_landlock_scope` resolves `workdir_access_rights(fds.workdir_exec)` into a single
    /// local and adds it to *both* the workdir rule and the `/tmp` rule, so the two sets are equal
    /// by construction; what this test pins is the content of that shared set as `/tmp`'s users
    /// need it — a compiler writing intermediates, `mktemp` creating and removing files, a package
    /// manager creating build directories — and the `Execute` axis, which must track
    /// `workdir_exec` on `/tmp` exactly as it does on the workdir, because both paths resolve to
    /// the same storage and a binary staged in one is the same bytes as a binary staged in the
    /// other.
    #[test]
    fn the_composed_root_tmp_rule_carries_exactly_the_workdir_right_set() {
        use landlock::AccessFs;

        for workdir_exec in [false, true] {
            let rights = linux_enforce::workdir_access_rights(workdir_exec);

            for needed in [
                AccessFs::WriteFile,
                AccessFs::ReadFile,
                AccessFs::ReadDir,
                AccessFs::MakeReg,
                AccessFs::MakeDir,
                AccessFs::RemoveFile,
                AccessFs::RemoveDir,
            ] {
                assert!(
                    rights.contains(needed),
                    "/tmp carries the workdir's rights, so it must carry {needed:?}: without it \
                     `cc`, `mktemp` and `pip` fail on a mount that is genuinely writable",
                );
            }

            assert_eq!(
                rights.contains(AccessFs::Execute),
                workdir_exec,
                "/tmp is backed by a directory inside the workdir, so Execute there must follow \
                 capabilities.filesystem.workdir_exec exactly — granting it unconditionally would \
                 make /tmp a hole through which capsule-written bytes run",
            );
        }

        assert_eq!(
            linux_enforce::workdir_access_rights(true) & !AccessFs::Execute,
            linux_enforce::workdir_access_rights(false),
            "the /tmp rule and the workdir rule share one right-set binding in \
             apply_landlock_scope; if that set ever grows a second conditional axis, this test and \
             that binding must be revisited together",
        );
    }

    #[test]
    fn prepare_enforcement_is_noop_for_environment_only_tier_even_on_linux() {
        let mut command = std::process::Command::new("true");
        let enforcement = ShellEnforcement::environment_only().with_test_session_dirs();
        let supervisor = prepare_enforcement(&mut command, &enforcement, Path::new("/tmp"))
            .expect("environment_only must never fail");
        assert!(matches!(supervisor, SupervisorHandle::Noop));
        assert_eq!(
            enforcement.session_netns.creations(),
            0,
            "a tier with no kernel sandbox must not create a network namespace"
        );
    }

    // Exec denial outside the allowlist is deliberately not asserted here. Exec is a Landlock
    // right, so the claim holds only on `KernelFull`/`KernelSealed`; on any weaker tier such a
    // test passes while exercising a path that enforces nothing. The two tests below assert the
    // opposite direction — that allowlisted binaries still run — which any tier does evidence.

    #[test]
    fn kernel_tier_allows_exec_within_shell_allowlist() {
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_tier_allows_exec_within_shell_allowlist",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_tier_allows_nested_exec_of_second_allowlisted_binary",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_tier_denies_network_connect_outside_allowlist",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
    /// The allowlist has to be declared rather than only pre-resolved, because it decides two
    /// things: which addresses are permitted *and* which ports the namespace binds a listener on.
    /// A port no allow entry implies has nothing listening and is refused by the namespace itself.
    ///
    /// Deliberately *not* a test of the security property — a green suite proves nothing about
    /// the kernel-level claim, and the real check is the hand-run procedure in
    /// `docs/content/reference/network-namespace-egress-proxy-manual-verification.md`. This
    /// asserts only that the permitted path still functions.
    #[test]
    fn kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy() {
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_tier_reaches_an_allowlisted_destination_through_the_egress_proxy",
        ) {
            return;
        }
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
        let script =
            format!("exec 3<>/dev/tcp/127.0.0.1/{port} && read -r reply <&3 && echo \"$reply\"");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_full_denies_filesystem_access_outside_workdir",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .unwrap();

        assert_ne!(
            result.exit_code, 0,
            "reading a file outside the Landlock-scoped workdir must fail under KernelFull"
        );
    }

    // ---- moves, links and truncation under the ruleset ABI ----
    //
    // The tests below resolve their enforcement the way a session does — `ShellEnforcement::resolve`
    // over a live `HostProbe` at `scoped` — so the ruleset ABI they run under is the one the probe
    // fills. Each one that needs a newer ABI than this host offers prints a skip line and returns.

    /// A `scoped` session's enforcement for `policy`, from a live probe, or `None` (after a skip
    /// line naming `test`) when this host cannot install a Landlock domain at `min_abi` or above.
    fn scoped_landlock_enforcement(
        test: &str,
        policy: &CapabilityPolicy,
        min_abi: u8,
    ) -> Option<ShellEnforcement> {
        if crate::network_namespace::skip_without_egress_namespace(test) {
            return None;
        }
        let probe = HostProbe::probe();
        let enforcement = ShellEnforcement::resolve_declared(
            policy,
            murmur_artifact::ContainmentClass::Scoped,
            probe.clone(),
        )
        .unwrap()
        .with_test_session_dirs();
        if enforcement.tier != EnforcementTier::KernelFull
            || enforcement.landlock_abi < Some(min_abi)
        {
            eprintln!(
                "skipping {test}: needs EnforcementTier::KernelFull with a Landlock ruleset at ABI \
                 {min_abi} or later; resolved {:?} at ABI {:?} (kernel reports {:?})",
                enforcement.tier,
                enforcement.landlock_abi,
                probe.landlock_kernel_abi()
            );
            return None;
        }
        Some(enforcement)
    }

    fn coreutils_policy(extra: &[&str]) -> CapabilityPolicy {
        let mut shell_allow: Vec<String> = ["bash", "mkdir", "mv", "ln", "stat", "cat"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        shell_allow.extend(extra.iter().map(|name| name.to_string()));
        CapabilityPolicy {
            shell_allow,
            ..CapabilityPolicy::default()
        }
    }

    fn run_bash_in(
        workdir: &Path,
        script: &str,
        policy: &CapabilityPolicy,
        enforcement: &ShellEnforcement,
    ) -> crate::shell::ShellResult {
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            policy,
            enforcement,
        )
        .unwrap();
        println!(
            "exit {}\n--- stdout\n{}--- stderr\n{}---",
            result.exit_code, result.stdout, result.stderr
        );
        result
    }

    /// A `<base>/work` workdir with a sibling `<base>/outside` on the same filesystem, so a
    /// refusal between them is Landlock's and never the VFS's cross-mount `EXDEV`.
    fn work_and_outside() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let work = base.path().join("work");
        let outside = base.path().join("outside");
        std::fs::create_dir(&work).unwrap();
        std::fs::create_dir(&outside).unwrap();
        (base, work, outside)
    }

    /// With `Refer` granted on the workdir, `mv` between two of its folders is a real `rename(2)`
    /// (the inode survives), and `ln` and a directory reparent across folders both succeed.
    #[test]
    fn kernel_tier_moves_files_between_workdir_folders() {
        let test = "kernel_tier_moves_files_between_workdir_folders";
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 3) else {
            return;
        };
        assert_eq!(enforcement.tier, EnforcementTier::KernelFull);
        assert_eq!(enforcement.landlock_abi, Some(3));

        let temp = tempfile::tempdir().unwrap();
        let result = run_bash_in(
            temp.path(),
            r#"set -e
mkdir a b
echo payload > a/x
before=$(stat -c %i a/x)
mv a/x b/x
after=$(stat -c %i b/x)
echo "mv a/x b/x: inode $before -> $after"
[ "$before" = "$after" ] || { echo "inode changed, mv copied instead of renaming" >&2; exit 10; }
ln b/x a/y
[ "$(cat a/y)" = payload ] || exit 11
echo "ln b/x a/y: ok"
mkdir a/d
mv a/d b/d
[ -d b/d ] && [ ! -e a/d ] || exit 12
echo "mv a/d b/d: ok"
"#,
            &policy,
            &enforcement,
        );

        assert_eq!(
            result.exit_code, 0,
            "a move, a hard link and a directory reparent between workdir folders must all \
             succeed under Refer"
        );
    }

    /// On a Landlock tier the session's home folder is `$HOME` and carries the work folder's
    /// rights: a command writes, renames within and removes files under it. The records directory
    /// around it stays read-only: a command cannot write `out/result.txt`, create `children/x`, or
    /// move the home folder itself aside. Both directories sit under the host's `/tmp` here, so
    /// where `/tmp` is mapped this also proves the home folder is re-exposed writable over the
    /// read-only records directory.
    #[test]
    fn home_folder_is_writable_like_tmp_and_the_records_around_it_are_not() {
        let test = "home_folder_is_writable_like_tmp_and_the_records_around_it_are_not";
        let policy = coreutils_policy(&["rm", "rmdir"]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };
        eprintln!(
            "{test}: tier {:?}, Landlock ruleset ABI {:?} (kernel {:?}), /tmp {:?}",
            enforcement.tier,
            enforcement.landlock_abi,
            HostProbe::probe().landlock_kernel_abi(),
            enforcement.installed_temp_mapping()
        );
        let records = enforcement.records_root().unwrap().clone();
        records
            .create_dir_all(crate::records_dir::RESULTS_SUBDIR, 0o700)
            .unwrap();
        let home = enforcement.home_dir_path().unwrap().to_path_buf();

        let workdir = tempfile::tempdir().unwrap();
        let result = run_bash_in(
            workdir.path(),
            &format!(
                r#"[ "$HOME" = '{home}' ] && echo home-is-the-folder
echo a > "$HOME/a" && mv "$HOME/a" "$HOME/b" && mkdir "$HOME/d" && mv "$HOME/b" "$HOME/d/c" \
  && rm "$HOME/d/c" && rmdir "$HOME/d" && echo home-writable
if echo x > "$MURMUR_RECORDS_DIR/out/result.txt"; then echo result-written; else echo result-refused; fi
if mkdir -p "$MURMUR_RECORDS_DIR/children/x"; then echo child-created; else echo child-refused; fi
if mv "$HOME" "$HOME.moved"; then echo home-moved; else echo home-move-refused; fi
"#,
                home = home.display()
            ),
            &policy,
            &enforcement,
        );

        assert_eq!(
            result.stdout,
            "home-is-the-folder\nhome-writable\nresult-refused\nchild-refused\nhome-move-refused\n",
            "stderr: {}",
            result.stderr
        );
        assert!(home.is_dir());
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
        assert!(!records.host_path("out/result.txt").exists());
        assert!(!records.host_path("children").exists());
        assert!(!home.with_extension("moved").exists());
        assert_eq!(std::fs::read_dir(workdir.path()).unwrap().count(), 0);
    }

    /// A delegated child's records, beneath the session's children's records directory, are
    /// readable by a parent's command and never writable: the result an outcome names can be read,
    /// and nothing of the child's can be changed. Before any child has staged there is nothing to
    /// grant, and nothing is.
    #[test]
    fn children_records_are_readable_and_never_writable_by_a_parents_commands() {
        let test = "children_records_are_readable_and_never_writable_by_a_parents_commands";
        let policy = coreutils_policy(&[]);
        let Some(mut enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };
        let base = tempfile::tempdir().unwrap();
        let children_records = base.path().join("children");
        enforcement.runtime_dir.as_mut().unwrap().children_records = Some(children_records.clone());
        assert_eq!(
            enforcement.children_records_path(),
            None,
            "no child has staged"
        );

        let result_file = children_records.join("worker-0123456789abcdef/ses_child/out/result.txt");
        std::fs::create_dir_all(result_file.parent().unwrap()).unwrap();
        std::fs::write(&result_file, "the child's answer").unwrap();
        assert_eq!(
            enforcement.children_records_path(),
            Some(children_records.as_path())
        );

        let workdir = tempfile::tempdir().unwrap();
        let result = run_bash_in(
            workdir.path(),
            &format!(
                r#"cat '{file}'; echo
if echo forged > '{file}'; then echo written; else echo write-refused; fi
if mkdir '{dir}/x'; then echo created; else echo create-refused; fi
"#,
                file = result_file.display(),
                dir = children_records.display()
            ),
            &policy,
            &enforcement,
        );

        assert_eq!(
            result.stdout, "the child's answer\nwrite-refused\ncreate-refused\n",
            "stderr: {}",
            result.stderr
        );
        assert_eq!(
            std::fs::read_to_string(&result_file).unwrap(),
            "the child's answer"
        );
        assert!(!children_records.join("x").exists());
    }

    /// The ruleset a 5.13–5.18 kernel gets, forced through the same `landlock_abi` field the probe
    /// fills: the capsule still launches, `ln` across folders is `EXDEV`, and `mv` survives only
    /// by copying.
    #[test]
    fn kernel_tier_at_abi_v1_refuses_cross_folder_links_with_exdev() {
        let test = "kernel_tier_at_abi_v1_refuses_cross_folder_links_with_exdev";
        let policy = coreutils_policy(&[]);
        let Some(mut enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };
        enforcement.landlock_abi = Some(1);

        let temp = tempfile::tempdir().unwrap();
        let result = run_bash_in(
            temp.path(),
            r#"mkdir a b || exit 20
echo payload > a/x || exit 21
before=$(stat -c %i a/x)
mv a/x b/x || exit 22
[ -f b/x ] && [ ! -e a/x ] || exit 23
after=$(stat -c %i b/x)
echo "mv a/x b/x: inode $before -> $after"
[ "$before" != "$after" ] || exit 24
if ln b/x a/y; then exit 25; fi
exit 42
"#,
            &policy,
            &enforcement,
        );

        assert_eq!(
            result.exit_code, 42,
            "the capsule must launch at ABI v1 and run the script to its own exit code: mv falls \
             back to copy-and-delete (a new inode) and ln fails"
        );
        assert!(
            result.stderr.contains("Invalid cross-device link"),
            "ln across workdir folders at ABI v1 must fail with EXDEV; stderr: {}",
            result.stderr
        );
    }

    /// Nothing is moved or linked out of the workdir into a sibling folder on the same filesystem.
    #[test]
    fn kernel_tier_refuses_rename_out_of_the_workdir() {
        let test = "kernel_tier_refuses_rename_out_of_the_workdir";
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 2) else {
            return;
        };

        let (_base, work, outside) = work_and_outside();
        let script = format!(
            r#"echo payload > x || exit 20
if mv x '{outside}/x'; then exit 21; fi
if ln x '{outside}/x2'; then exit 22; fi
exit 0
"#,
            outside = outside.display()
        );
        let result = run_bash_in(&work, &script, &policy, &enforcement);

        assert_eq!(
            result.exit_code, 0,
            "mv and ln out of the workdir must both fail"
        );
        assert!(
            work.join("x").exists(),
            "the source must stay in the workdir"
        );
        assert!(!outside.join("x").exists(), "nothing may be moved out");
        assert!(!outside.join("x2").exists(), "nothing may be linked out");
    }

    /// Nothing is moved or linked into the workdir from a sibling folder on the same filesystem.
    #[test]
    fn kernel_tier_refuses_rename_into_the_workdir() {
        let test = "kernel_tier_refuses_rename_into_the_workdir";
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 2) else {
            return;
        };

        let (_base, work, outside) = work_and_outside();
        std::fs::write(outside.join("y"), "original\n").unwrap();
        let script = format!(
            r#"if mv '{outside}/y' ./y; then exit 21; fi
if ln '{outside}/y' ./y2; then exit 22; fi
exit 0
"#,
            outside = outside.display()
        );
        let result = run_bash_in(&work, &script, &policy, &enforcement);

        assert_eq!(
            result.exit_code, 0,
            "mv and ln into the workdir must both fail"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("y")).unwrap(),
            "original\n",
            "the outside file must be untouched"
        );
        assert!(!work.join("y").exists(), "nothing may be moved in");
        assert!(!work.join("y2").exists(), "nothing may be linked in");
    }

    /// A by-path `truncate(2)` on a host file outside the workdir: refused once the ruleset
    /// handles `Truncate` (ABI v3), and not mediated at all at ABI v1 — the gap v3 closes.
    #[test]
    fn kernel_tier_truncate_outside_the_workdir_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let test = "kernel_tier_truncate_outside_the_workdir_is_refused";
        let policy = coreutils_policy(&["perl"]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 3) else {
            return;
        };

        let (_base, work, outside) = work_and_outside();
        let target = outside.join("t");
        std::fs::write(&target, "keep\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let script = format!(
            r#"perl -e 'truncate($ARGV[0], 0) or die "$!\n"' '{}'"#,
            target.display()
        );

        let refused = run_bash_in(&work, &script, &policy, &enforcement);
        assert_ne!(
            refused.exit_code, 0,
            "truncate(2) outside the workdir must be refused at ABI 3"
        );
        assert!(
            refused.stderr.contains("Permission denied"),
            "the refusal must be EACCES; stderr: {}",
            refused.stderr
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep\n");

        let mut at_v1 = enforcement.clone();
        at_v1.landlock_abi = Some(1);
        let unmediated = run_bash_in(&work, &script, &policy, &at_v1);
        assert_eq!(
            unmediated.exit_code, 0,
            "at ABI v1 truncate(2) is not mediated, which is the gap this test pins as closed above"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "");
    }

    /// Handling `Truncate` must not refuse what a capsule truncates legitimately: its own files,
    /// with `truncate` and with a `>` redirect over an existing file, and `/dev/null`.
    #[test]
    fn kernel_tier_truncate_inside_the_workdir_and_dev_null_still_work() {
        let test = "kernel_tier_truncate_inside_the_workdir_and_dev_null_still_work";
        let policy = coreutils_policy(&["truncate"]);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 3) else {
            return;
        };

        let temp = tempfile::tempdir().unwrap();
        let result = run_bash_in(
            temp.path(),
            r#"set -e
echo hello > f && truncate -s 0 f && [ ! -s f ]
echo again > f && echo over > f && [ "$(cat f)" = over ]
: > g
echo discard > /dev/null
echo ok
"#,
            &policy,
            &enforcement,
        );

        assert_eq!(
            result.exit_code, 0,
            "truncating inside the workdir and writing to /dev/null must keep working"
        );
    }

    /// `uv venv` then `uv pip install <wheel>` in the workdir, offline: uv unpacks the wheel into
    /// a temporary folder in its cache and renames it into place. Passes at the probed ABI, and
    /// fails with `EXDEV` at ABI v1, so the pass is `Refer`'s.
    ///
    /// Ignored because it needs two host-side inputs a stock host does not have:
    /// `MURMUR_TEST_UV`, an absolute path to a `uv` binary, and `MURMUR_TEST_WHEEL`, an absolute
    /// path to `six-1.16.0-py2.py3-none-any.whl`, whose module and version the script checks after
    /// installing it. Run without them it
    /// panics naming the missing variable. `uv`, `python3` and its stdlib are granted by this
    /// test's policy only.
    #[test]
    #[ignore = "needs MURMUR_TEST_UV and MURMUR_TEST_WHEEL; see the doc comment"]
    fn kernel_tier_uv_installs_a_wheel_in_the_workdir() {
        let test = "kernel_tier_uv_installs_a_wheel_in_the_workdir";
        let input = |name: &str| {
            let value = std::env::var(name)
                .unwrap_or_else(|_| panic!("{test} needs {name} set to an absolute path"));
            let path = PathBuf::from(value);
            assert!(
                path.is_absolute() && path.is_file(),
                "{name}={} is not a file",
                path.display()
            );
            path
        };
        let uv = input("MURMUR_TEST_UV");
        let wheel = input("MURMUR_TEST_WHEEL");
        let python = std::fs::canonicalize("/usr/bin/python3").expect("host python3");
        let stdlib = std::process::Command::new(&python)
            .args([
                "-I",
                "-c",
                "import sysconfig; print(sysconfig.get_paths()['stdlib'])",
            ])
            .output()
            .expect("host python3 runs");
        let stdlib = String::from_utf8(stdlib.stdout).unwrap().trim().to_string();

        let policy = CapabilityPolicy {
            shell_allow: vec![
                "bash".to_string(),
                uv.display().to_string(),
                python.display().to_string(),
            ],
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![murmur_artifact::InterpreterRuntimeDir {
                    path: stdlib,
                    list_dir: true,
                }],
            }],
            ..CapabilityPolicy::default()
        };
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 2) else {
            return;
        };

        let wheel_name = wheel.file_name().unwrap().to_str().unwrap().to_string();
        let script = format!(
            r#"set -e
export UV_CACHE_DIR="$PWD/.uv-cache" UV_PYTHON_DOWNLOADS=never
'{uv}' venv --python '{python}' .venv
'{uv}' pip install --offline --no-index --python .venv/bin/python './{wheel_name}'
.venv/bin/python -c 'import six; print(six.__version__)'
"#,
            uv = uv.display(),
            python = python.display(),
        );
        let install = |enforcement: &ShellEnforcement| {
            let temp = tempfile::tempdir().unwrap();
            std::fs::copy(&wheel, temp.path().join(&wheel_name)).unwrap();
            run_bash_in(temp.path(), &script, &policy, enforcement)
        };

        let at_probed = install(&enforcement);
        assert_eq!(
            at_probed.exit_code, 0,
            "uv must install the wheel at ABI {:?}",
            enforcement.landlock_abi
        );
        assert!(at_probed.stdout.contains("1.16.0"), "{}", at_probed.stdout);

        let mut at_v1 = enforcement.clone();
        at_v1.landlock_abi = Some(1);
        let refused = install(&at_v1);
        let output = format!("{}{}", refused.stdout, refused.stderr);
        assert_ne!(refused.exit_code, 0, "at ABI v1 the install must fail");
        assert!(
            output.contains("os error 18") || output.contains("Invalid cross-device link"),
            "at ABI v1 the install must fail with EXDEV: {output}"
        );
    }

    // The two tests below exercise the derived Landlock grants. A green run is not evidence they
    // work: below `KernelFull` the weaker `KernelSeccompOnly` path runs instead, and on non-Linux
    // no Landlock domain is installed at all. Both tests print a skip line and return when the
    // host is not `KernelFull`.

    #[test]
    fn kernel_full_runs_nontrivial_shell_allowlist_but_a_pass_here_does_not_prove_the_landlock_fix()
    {
        if crate::network_namespace::skip_without_egress_namespace("kernel_full_runs_nontrivial_shell_allowlist_but_a_pass_here_does_not_prove_the_landlock_fix") {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace("kernel_full_denies_write_outside_workdir_but_a_pass_here_does_not_prove_the_landlock_fix") {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
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
    fn kernel_full_enforcement(
        tier: EnforcementTier,
        policy: &CapabilityPolicy,
    ) -> ShellEnforcement {
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let mut landlock_grants =
            LandlockGrant::non_listable_files(resolve_landlock_grants(&exec_allow_paths));
        landlock_grants.extend(resolve_interpreter_runtime_grants(
            &policy.shell_interpreter_runtime,
        ));
        ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants,
            ..host_bounding_base()
        }
    }

    /// A real sealed run: the composed root's `/tmp` is the session's temporary folder in its
    /// records directory, `$HOME` and the synthetic `/etc/passwd`'s home are both its home folder,
    /// writable inside the root (`cd ~ && touch f`), and the work folder gains nothing.
    ///
    /// This drives a composed root end to end, so it needs a host that can actually reach
    /// `KernelSealed` and skips loudly on one that cannot; the parent-side halves are covered
    /// unconditionally by the `open_landlock_fds` tests above. `mur doctor` reports the same host
    /// answer this checks.
    #[test]
    fn home_folder_and_tmp_in_the_records_dir_on_a_sealed_root() {
        if crate::network_namespace::skip_without_egress_namespace(
            "home_folder_and_tmp_in_the_records_dir_on_a_sealed_root",
        ) {
            return;
        }
        let probe = HostProbe::probe();
        if probe.tier() != EnforcementTier::KernelSealed {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE SEALED ROOT ON THIS HOST: \
                 home_folder_and_tmp_in_the_records_dir_on_a_sealed_root needs a host that reaches \
                 KernelSealed (unprivileged user namespaces plus a usable Landlock ABI); detected \
                 {:?}. No composed root is built in this run.",
                probe.tier()
            );
            return;
        }

        let accessible = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec![
                "bash".to_string(),
                "cat".to_string(),
                "id".to_string(),
                "touch".to_string(),
                "getent".to_string(),
                "cut".to_string(),
            ],
            ..CapabilityPolicy::default()
        };
        let runtime = tempfile::tempdir().unwrap();
        let enforcement = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Sealed,
            probe.clone(),
        )
        .expect("a sealed-capable host resolves sealed enforcement")
        .with_runtime_dir(
            test_runtime_dir(runtime.path()).with_children_records(runtime.path().join("children")),
            None,
        );
        // A delegated child's result, as a child that has staged leaves it.
        let child_result = runtime
            .path()
            .join("children/worker-0123456789abcdef/out/result.txt");
        std::fs::create_dir_all(child_result.parent().unwrap()).unwrap();
        std::fs::write(&child_result, "the child's answer").unwrap();
        assert_eq!(enforcement.tier, EnforcementTier::KernelSealed);
        eprintln!(
            "home_folder_and_tmp_in_the_records_dir_on_a_sealed_root: tier {:?}, Landlock \
             ruleset ABI {:?} (kernel {:?})",
            enforcement.tier,
            enforcement.landlock_abi,
            probe.landlock_kernel_abi()
        );
        let probe_name = format!("sealed-probe-{}", std::process::id());

        let result = crate::shell::execute_shell(
            "bash",
            &[
                "-c",
                &format!(
                    "echo t > /tmp/{probe_name} && cat /tmp/{probe_name} && id -un && \
                     printf '%s\\n' \"$TMPDIR\" && printf '%s\\n' \"$HOME\" && \
                     getent passwd \"$(id -u)\" | cut -d: -f6 && cd ~ && touch f && echo touched && \
                     cat '{child}' && echo && \
                     {{ echo forged > '{child}' && echo forged || echo child-read-only; }}",
                    child = child_result.display()
                ),
            ],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(accessible.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell returns Ok even when the command itself fails");

        assert_eq!(
            result.exit_code, 0,
            "stdout: {} stderr: {}",
            result.stdout, result.stderr
        );
        let mut lines = result.stdout.lines();
        assert_eq!(
            lines.next(),
            Some("t"),
            "the capsule's /tmp must be writable"
        );
        assert!(
            lines.next().is_some_and(|name| !name.is_empty()),
            "`id -un` resolves only through the synthetic /etc/passwd the composed root binds: \
             {}",
            result.stdout
        );
        assert_eq!(
            lines.next(),
            Some("/tmp"),
            "TMPDIR names /tmp inside the root"
        );
        let home = enforcement.home_dir_path().unwrap().to_str().unwrap();
        assert_eq!(lines.next(), Some(home), "$HOME is the home folder");
        assert_eq!(lines.next(), Some(home), "passwd's home is the home folder");
        assert_eq!(
            lines.next(),
            Some("touched"),
            "~ is writable inside the root"
        );
        assert_eq!(
            lines.next(),
            Some("the child's answer"),
            "a child's result reads"
        );
        assert_eq!(
            lines.next(),
            Some("child-read-only"),
            "and cannot be changed"
        );
        assert!(Path::new(home).join("f").is_file());

        let folder = enforcement.temp_dir_path().unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join(&probe_name)).unwrap(),
            "t\n",
            "the bytes written to /tmp land in the session's temporary folder"
        );
        assert!(
            !Path::new("/tmp").join(&probe_name).exists(),
            "nothing reaches the host's /tmp"
        );
        assert!(runtime
            .path()
            .join(crate::runtime_dir::SEALED_ETC_SUBDIR)
            .join("passwd")
            .is_file());
        assert_eq!(
            std::fs::read_dir(accessible.path()).unwrap().count(),
            0,
            "enabling sealed must add nothing to the work folder"
        );
    }

    // As above: a green run proves nothing below `KernelFull`. Both tests print a skip line and
    // return when the host is not `KernelFull`.

    #[test]
    fn kernel_full_interpreter_runtime_list_dir_false_opens_file_but_denies_listing() {
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_full_interpreter_runtime_list_dir_false_opens_file_but_denies_listing",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_full_interpreter_runtime_list_dir_true_lists_dir_but_not_its_parent",
        ) {
            return;
        }
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
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
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
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell should return Ok with a nonzero exit code");
        assert_ne!(
            list_parent.exit_code, 0,
            "a grant on stdlib/ must not make its (ungranted) parent enumerable"
        );
    }

    /// The `executable` axis, asserted against a real Landlock kernel.
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
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_full_non_executable_grant_lists_its_tree_but_refuses_to_run_from_it",
        ) {
            return;
        }
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
                pinned: None,
            });
            enforcement
        };
        let run = |enforcement: &ShellEnforcement, command: String| {
            crate::shell::execute_shell(
                "bash",
                &["-c", &command],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
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

    /// The access shape most of `SEALED_ETC_PATHS` needs: a non-executable grant on a *regular
    /// file* (`/etc/ssl/certs/ca-certificates.crt`'s stand-in) must install and be readable.
    ///
    /// Two kernel facts are pinned here. First, `ReadFile` alone —
    /// `apply_landlock_scope`'s `(false, false)` arm — is enough to `open()` the file. Second,
    /// `landlock_append_fs_rule` rejects `ReadDir` on a non-directory with `EINVAL`, and the rule
    /// loop is fail-closed, so a `list_dir: true` *claim* on a file would refuse the whole launch
    /// if `open_landlock_fds` did not narrow it against the fd it opened. The second half of this
    /// test is that narrowing: the same grant asserted `list_dir: true` must still launch and still
    /// read, not die in `pre_exec`.
    ///
    /// Runs on any host with a usable Landlock ABI; skips loudly elsewhere.
    #[test]
    fn kernel_full_non_executable_file_grant_is_readable_and_survives_a_bad_list_dir_claim() {
        if crate::network_namespace::skip_without_egress_namespace(
            "kernel_full_non_executable_file_grant_is_readable_and_survives_a_bad_list_dir_claim",
        ) {
            return;
        }
        let host_tier = detect_enforcement_tier();
        if !matches!(
            host_tier,
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed
        ) {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE LANDLOCK FIX ON THIS HOST: \
                 kernel_full_non_executable_file_grant_... needs a usable Landlock ABI (kernel \
                 5.13+); detected {host_tier:?}. This run does NOT install a Landlock domain, so a \
                 green result is not evidence that a file grant reads, nor that a ReadDir claim on \
                 a file is narrowed instead of refusing the launch."
            );
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        // A trust-store stand-in: a regular file outside the workdir, granted on its own.
        let store = tempfile::tempdir().unwrap();
        let bundle = store.path().join("ca-certificates.crt");
        std::fs::write(&bundle, "-----BEGIN CERTIFICATE-----\n").unwrap();

        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "cat".to_string()],
            ..CapabilityPolicy::default()
        };
        let with_file_grant = |list_dir: bool| {
            let mut enforcement = kernel_full_enforcement(EnforcementTier::KernelFull, &policy);
            enforcement.landlock_grants.push(LandlockGrant {
                path: bundle.clone(),
                list_dir,
                executable: false,
                pinned: None,
            });
            enforcement
        };
        let read = |enforcement: &ShellEnforcement| {
            crate::shell::execute_shell(
                "bash",
                &["-c", &format!("cat '{}'", bundle.display())],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
                &policy,
                enforcement,
            )
            .expect("execute_shell should return Ok even when the command itself fails")
        };

        let honest = read(&with_file_grant(false));
        assert_eq!(
            honest.exit_code, 0,
            "ReadFile alone must open a granted regular file — this is what the /etc allowlist's \
             file entries rely on (stderr: {})",
            honest.stderr
        );
        assert!(
            honest.stdout.contains("BEGIN CERTIFICATE"),
            "stdout: {}",
            honest.stdout
        );

        let overclaimed = read(&with_file_grant(true));
        assert_eq!(
            overclaimed.exit_code, 0,
            "a list_dir:true claim on a regular file must be narrowed to ReadFile, not turned \
             into an EINVAL that refuses the launch (stderr: {})",
            overclaimed.stderr
        );
        assert!(
            overclaimed.stdout.contains("BEGIN CERTIFICATE"),
            "stdout: {}",
            overclaimed.stdout
        );
    }

    // ---- every distinct pre_exec setup failure is legible + fail-closed ----
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
    /// not exist. Because the workdir's Landlock fd is opened in the PARENT, this fails
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
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
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
        if crate::network_namespace::skip_without_egress_namespace(
            "pre_exec_no_new_privs_failure_is_distinct_and_fails_closed",
        ) {
            return;
        }
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
        if crate::network_namespace::skip_without_egress_namespace(
            "pre_exec_landlock_failure_is_distinct_and_fails_closed",
        ) {
            return;
        }
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
        if crate::network_namespace::skip_without_egress_namespace(
            "pre_exec_setup_failures_produce_pairwise_distinct_messages",
        ) {
            return;
        }
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

    /// The first half of the composed-root failure path: a `pre_exec`
    /// composed-root failure comes back out of `execute_shell` as the *typed*
    /// `SealedRootConstructionFailed`, carrying the child's diagnostic, and reports itself as
    /// session-fatal. `runtime::tests` covers the second half (what the dispatch layer then
    /// does with it) and `murmur-cli`'s error test covers the third (`E-RUN-014`).
    #[test]
    fn a_composed_root_failure_is_typed_and_session_fatal_not_just_another_message() {
        if crate::network_namespace::skip_without_egress_namespace(
            "a_composed_root_failure_is_typed_and_session_fatal_not_just_another_message",
        ) {
            return;
        }
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

    // ---- one network namespace per session -------------------------------------------------
    //
    // Every test below drives real commands through `execute_shell`/`run_shell` against an
    // enforcement resolved the way `stage_session` resolves one, and reads what a later command of
    // the same session — or of another session — can reach.

    /// `bash`, `nc` and `sleep`, plus `extra`: what a model starting and probing a server uses.
    fn server_policy(extra: &[&str]) -> CapabilityPolicy {
        CapabilityPolicy {
            shell_allow: ["bash", "nc", "sleep"]
                .iter()
                .chain(extra)
                .map(|binary| binary.to_string())
                .collect(),
            ..CapabilityPolicy::default()
        }
    }

    /// A session's enforcement, resolved as `stage_session` resolves a `scoped` capsule's.
    fn session_enforcement(policy: &CapabilityPolicy) -> ShellEnforcement {
        let enforcement = ShellEnforcement::resolve_declared(
            policy,
            murmur_artifact::ContainmentClass::Scoped,
            HostProbe::probe(),
        )
        .expect("the policy resolves")
        .with_test_session_dirs();
        assert_eq!(
            enforcement.tier,
            EnforcementTier::KernelFull,
            "a scoped capsule on this host runs on KernelFull"
        );
        enforcement
    }

    /// A one-shot server on `127.0.0.1:<port>` that answers its one connection with `reply`,
    /// backgrounded with its output redirected so the command that starts it returns at once.
    fn one_shot_server(reply: &str, port: u16) -> String {
        format!(
            "(printf \"{reply}\\n\" | nc -N -l 127.0.0.1 {port}) >/dev/null 2>&1 & echo $! > server.pid"
        )
    }

    /// Connects to `127.0.0.1:<port>`, retrying for up to five seconds while the server comes up,
    /// and prints the first line it reads.
    fn retrying_client(port: u16) -> String {
        format!(
            "for i in {{1..50}}; do exec 3<>/dev/tcp/127.0.0.1/{port} 2>/dev/null && break; \
             sleep 0.1; done; read -r reply <&3; echo \"$reply\""
        )
    }

    fn bash(
        script: &str,
        workdir: &Path,
        policy: &CapabilityPolicy,
        enforcement: &ShellEnforcement,
    ) -> Result<crate::shell::ShellResult, crate::shell::ShellExecError> {
        crate::shell::execute_shell(
            "bash",
            &["-c", script],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            policy,
            enforcement,
        )
    }

    fn pid_in(workdir: &Path, file: &str) -> i32 {
        std::fs::read_to_string(workdir.join(file))
            .unwrap_or_else(|error| panic!("{file} was not written: {error}"))
            .trim()
            .parse()
            .unwrap_or_else(|error| panic!("{file} holds no pid: {error}"))
    }

    fn process_exists(pid: i32) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Whether `/proc/<pid>` is gone within `timeout`. A killed process stays visible until its
    /// parent — `init` for an orphan — reaps it, which takes a moment after the kill itself.
    fn process_gone_within(pid: i32, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while process_exists(pid) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        true
    }

    /// A server one command backgrounds answers a later, separate command of the same session.
    #[test]
    #[allow(clippy::print_stdout)]
    fn a_listener_started_by_one_command_answers_the_next() {
        if crate::network_namespace::skip_without_egress_namespace(
            "a_listener_started_by_one_command_answers_the_next",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let policy = server_policy(&[]);
        let enforcement = session_enforcement(&policy);

        let started = std::time::Instant::now();
        let server = bash(
            &one_shot_server("pong", 8080),
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("the server command runs");
        let server_ms = started.elapsed().as_millis();
        assert_eq!(server.exit_code, 0, "stderr: {}", server.stderr);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a backgrounded server with its output redirected must not hold the command open"
        );

        let client = bash(&retrying_client(8080), temp.path(), &policy, &enforcement)
            .expect("the client command runs");
        println!(
            "tier {:?}; command 1 exit {} in {server_ms} ms; command 2 exit {} stdout {:?}; \
             namespaces created: {}",
            enforcement.tier,
            server.exit_code,
            client.exit_code,
            client.stdout.trim(),
            enforcement.session_netns.creations()
        );
        assert_eq!(client.exit_code, 0, "stderr: {}", client.stderr);
        assert_eq!(client.stdout.trim(), "pong");
        assert_eq!(enforcement.session_netns.creations(), 1);

        enforcement.end_session();
    }

    /// A foreground server demoted after its grace period keeps listening, and a later command
    /// reaches it; the demoted command then completes with what it received.
    #[test]
    #[allow(clippy::print_stdout)]
    fn a_demoted_server_stays_reachable_from_later_commands() {
        use crate::detached::{DetachPolicy, DetachedRegistry, DetachedReport};

        if crate::network_namespace::skip_without_egress_namespace(
            "a_demoted_server_stays_reachable_from_later_commands",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let policy = server_policy(&[]);
        let enforcement =
            session_enforcement(&policy).with_runtime_dir(test_runtime_dir(base.path()), None);
        let (registry, mut reports) = DetachedRegistry::new();

        let outcome = crate::shell::run_shell(
            "nc",
            &["-l", "127.0.0.1", "8080"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap(),
            &policy,
            &enforcement,
            Some(DetachPolicy {
                grace: std::time::Duration::ZERO,
                delivered: true,
                registry: Arc::clone(&registry),
                command: "-l 127.0.0.1 8080".to_string(),
                context_id: "ctx_demoted_server".to_string(),
                provenance: None,
                abandoned: None,
                canceled: None,
            }),
        )
        .expect("the server command runs");
        let crate::shell::ShellOutcome::Detached(info) = outcome else {
            panic!("a zero grace period demotes the server, got {outcome:?}");
        };

        let client = bash(
            "for i in {1..50}; do exec 3<>/dev/tcp/127.0.0.1/8080 2>/dev/null && break; \
             sleep 0.1; done; echo ping >&3",
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("the client command runs");
        assert_eq!(client.exit_code, 0, "stderr: {}", client.stderr);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let completion = loop {
            match reports.try_recv() {
                Ok(DetachedReport::Completed(completion)) => break completion,
                Ok(DetachedReport::Lost(_)) => {}
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => panic!("the demoted server never completed: {error}"),
            }
        };
        let log = std::fs::read_to_string(&completion.output_path)
            .expect("the demoted command's log is written");
        println!(
            "demoted as {}; client exit {}; completion {} exit {}; log has ping: {}",
            info.work_id,
            client.exit_code,
            completion.work_id,
            completion.exit_code,
            log.contains("ping")
        );
        assert_eq!(completion.work_id, info.work_id);
        assert_eq!(completion.exit_code, 0, "log: {log}");
        assert!(log.contains("ping"), "log: {log}");

        enforcement.end_session();
    }

    /// A truncated command's log, named by its absolute path in the session's records directory,
    /// reads back under the session's own enforcement: through the shell interpreter and through
    /// a binary allowed on its own that gets the path as an argument. A write to it is refused,
    /// on any tier that installs Landlock.
    #[test]
    #[allow(clippy::print_stdout)]
    fn a_truncated_log_reads_back_through_the_shell_tool_and_refuses_a_write() {
        // A host that cannot give the session its network namespace refuses the session at
        // staging, so there is no session enforcement to run under.
        if crate::network_namespace::skip_without_egress_namespace(
            "a_truncated_log_reads_back_through_the_shell_tool_and_refuses_a_write",
        ) {
            return;
        }
        let base = home_tempdir();
        let workdir = home_tempdir();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "grep".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            HostProbe::probe(),
        )
        .expect("the policy resolves")
        .with_runtime_dir(test_runtime_dir(base.path()), None);
        let landlock = matches!(
            enforcement.tier,
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed
        );
        println!(
            "tier {:?}; landlock ABI {:?}",
            enforcement.tier,
            detect_landlock_abi()
        );

        // A builtin loop: only `bash` and `grep` are executable under Landlock.
        let printed = run_bash_in(
            workdir.path(),
            "for ((i = 1; i <= 20000; i++)); do echo $i; done",
            &policy,
            &enforcement,
        );
        assert_eq!(printed.exit_code, 0, "{}", printed.stderr);
        assert!(printed.truncated);
        let path = printed
            .full_output_path
            .clone()
            .expect("a truncated command names its log");
        assert!(path.starts_with(enforcement.records_dir_path().unwrap().to_str().unwrap()));
        let before = std::fs::read(&path).unwrap();

        let through_bash = run_bash_in(
            workdir.path(),
            &format!("grep -x 20000 {path}"),
            &policy,
            &enforcement,
        );
        assert_eq!(through_bash.exit_code, 0, "{}", through_bash.stderr);
        assert_eq!(through_bash.stdout, "20000\n");

        let through_grep = crate::shell::execute_shell(
            "grep",
            &["-x", "20000", &path],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .unwrap();
        assert_eq!(through_grep.exit_code, 0, "{}", through_grep.stderr);
        assert_eq!(through_grep.stdout, "20000\n");

        if landlock {
            let write = run_bash_in(
                workdir.path(),
                &format!("echo x >> {path}"),
                &policy,
                &enforcement,
            );
            assert_ne!(write.exit_code, 0, "a command appended to its own log");
            assert_eq!(std::fs::read(&path).unwrap(), before);
        } else {
            println!(
                "[SKIP-HOST] a_truncated_log_reads_back_through_the_shell_tool_and_refuses_a_write: \
                 the write half needs Landlock; detected {:?}",
                enforcement.tier
            );
        }
        assert!(!workdir.path().join("logs").exists());
        enforcement.end_session();
    }

    /// Two sessions each bind the same port on their own loopback, each reaches only its own
    /// server, and the host reaches neither.
    #[test]
    #[allow(clippy::print_stdout)]
    fn two_sessions_do_not_share_loopback() {
        if crate::network_namespace::skip_without_egress_namespace(
            "two_sessions_do_not_share_loopback",
        ) {
            return;
        }
        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap().port()
        };
        let policy = server_policy(&[]);
        let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let session_a = session_enforcement(&policy);
        let session_b = session_enforcement(&policy);

        for (dir, session, reply) in [
            (&dir_a, &session_a, "pong-a"),
            (&dir_b, &session_b, "pong-b"),
        ] {
            let server = bash(&one_shot_server(reply, port), dir.path(), &policy, session)
                .expect("the server command runs");
            assert_eq!(server.exit_code, 0, "stderr: {}", server.stderr);
        }
        // Both servers bound the same port: each is still running, waiting for its one client.
        std::thread::sleep(std::time::Duration::from_millis(300));
        for dir in [&dir_a, &dir_b] {
            let pid = pid_in(dir.path(), "server.pid");
            assert!(process_exists(pid), "a server's bind on {port} failed");
        }

        let host = std::net::TcpStream::connect(("127.0.0.1", port));
        let host_error = host.expect_err("the host must not reach a session's loopback server");
        assert_eq!(host_error.kind(), std::io::ErrorKind::ConnectionRefused);

        let client_b = bash(&retrying_client(port), dir_b.path(), &policy, &session_b)
            .expect("B's client runs");
        assert_eq!(client_b.exit_code, 0, "stderr: {}", client_b.stderr);

        // B's one-shot server is spent; A's still listens on the same port in A's namespace.
        let second_b = bash(
            &format!("exec 3<>/dev/tcp/127.0.0.1/{port}"),
            dir_b.path(),
            &policy,
            &session_b,
        )
        .expect("B's second client runs");

        let client_a = bash(&retrying_client(port), dir_a.path(), &policy, &session_a)
            .expect("A's client runs");
        println!(
            "port {port}; host connect: {host_error}; B read {:?}; B's second client exit {} \
             stderr {:?}; A read {:?}",
            client_b.stdout.trim(),
            second_b.exit_code,
            second_b.stderr.trim(),
            client_a.stdout.trim()
        );
        assert_eq!(client_b.stdout.trim(), "pong-b");
        assert_ne!(second_b.exit_code, 0);
        assert!(
            second_b.stderr.contains("Connection refused"),
            "stderr: {}",
            second_b.stderr
        );
        assert_eq!(client_a.exit_code, 0, "stderr: {}", client_a.stderr);
        assert_eq!(client_a.stdout.trim(), "pong-a");

        session_a.end_session();
        session_b.end_session();
    }

    /// The session's one egress proxy serves every command, refuses what is not allowlisted,
    /// and is created once.
    #[test]
    #[allow(clippy::print_stdout)]
    fn the_session_proxy_serves_every_command() {
        if crate::network_namespace::skip_without_egress_namespace(
            "the_session_proxy_serves_every_command",
        ) {
            return;
        }
        // Answers every connection, so the first and third commands both get a reply.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                use std::io::Write;
                if let Ok(mut stream) = stream {
                    let _ = stream.write_all(b"pong\n");
                }
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            network_allow: vec![format!("http://127.0.0.1:{port}")],
            ..server_policy(&[])
        };
        let enforcement = session_enforcement(&policy);
        let through_proxy =
            format!("exec 3<>/dev/tcp/127.0.0.1/{port} && read -r reply <&3 && echo \"$reply\"");

        let first = bash(&through_proxy, temp.path(), &policy, &enforcement).unwrap();
        let unlisted = bash(
            "exec 3<>/dev/tcp/10.255.255.1/9",
            temp.path(),
            &policy,
            &enforcement,
        )
        .unwrap();
        let third = bash(&through_proxy, temp.path(), &policy, &enforcement).unwrap();
        println!(
            "first {:?} exit {}; unlisted exit {} stderr {:?}; third {:?} exit {}; namespaces \
             created: {}",
            first.stdout.trim(),
            first.exit_code,
            unlisted.exit_code,
            unlisted.stderr.trim(),
            third.stdout.trim(),
            third.exit_code,
            enforcement.session_netns.creations()
        );
        assert_eq!(first.exit_code, 0, "stderr: {}", first.stderr);
        assert_eq!(first.stdout.trim(), "pong");
        assert_ne!(unlisted.exit_code, 0, "an unlisted destination must fail");
        assert_eq!(third.exit_code, 0, "stderr: {}", third.stderr);
        assert_eq!(third.stdout.trim(), "pong");
        assert_eq!(enforcement.session_netns.creations(), 1);

        enforcement.end_session();
    }

    /// Commands whose first spawns race each other still create exactly one namespace, and
    /// all of them land in it.
    #[test]
    #[allow(clippy::print_stdout)]
    fn concurrent_first_commands_share_one_namespace() {
        if crate::network_namespace::skip_without_egress_namespace(
            "concurrent_first_commands_share_one_namespace",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let policy = server_policy(&[]);
        let enforcement = session_enforcement(&policy);
        // One one-shot server per client, on its own port: a second client queued on a one-shot
        // `nc -l` would sit in its listen backlog and be closed unanswered when it exits.
        let servers = (8081..=8083)
            .map(|port| one_shot_server("pong", port).replace("server.pid", &format!("{port}.pid")))
            .collect::<Vec<_>>()
            .join("; ");
        let scripts = [
            servers,
            retrying_client(8081),
            retrying_client(8082),
            retrying_client(8083),
        ];

        let barrier = std::sync::Barrier::new(scripts.len());
        let results: Vec<crate::shell::ShellResult> = std::thread::scope(|scope| {
            let handles: Vec<_> = scripts
                .iter()
                .map(|script| {
                    let enforcement = enforcement.clone();
                    let (barrier, policy, workdir) = (&barrier, &policy, temp.path());
                    scope.spawn(move || {
                        barrier.wait();
                        bash(script, workdir, policy, &enforcement).expect("the command runs")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        for (index, result) in results.iter().enumerate() {
            println!(
                "command {index}: exit {} stdout {:?}",
                result.exit_code,
                result.stdout.trim()
            );
            assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr);
        }
        for client in &results[1..] {
            assert_eq!(client.stdout.trim(), "pong");
        }
        println!(
            "namespaces created: {}",
            enforcement.session_netns.creations()
        );
        assert_eq!(enforcement.session_netns.creations(), 1);

        enforcement.end_session();
    }

    /// Ending the session kills everything it started — a `setsid` process and a background
    /// server included — releases the namespace, and refuses every command after.
    #[test]
    #[allow(clippy::print_stdout)]
    fn ending_the_session_kills_everything_it_started() {
        if crate::cgroup::skip_without_host_support(
            "ending_the_session_kills_everything_it_started",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let policy = server_policy(&["setsid"]);
        let scope =
            crate::cgroup::prepare_scope(true, &policy.resources, "ses_end_kills", temp.path())
                .expect("a delegated scope")
                .scope
                .expect("a required scope on Linux is never None");
        let enforcement =
            session_enforcement(&policy).with_host_bounding(Some(Arc::clone(&scope)), None);

        let started = bash(
            "setsid sleep 300 >/dev/null 2>&1 & echo $! > sleeper.pid; \
             nc -l 127.0.0.1 8080 >/dev/null 2>&1 & echo $! > server.pid",
            temp.path(),
            &policy,
            &enforcement,
        )
        .expect("the command runs");
        assert_eq!(started.exit_code, 0, "stderr: {}", started.stderr);
        let sleeper = pid_in(temp.path(), "sleeper.pid");
        let server = pid_in(temp.path(), "server.pid");
        assert!(process_exists(sleeper), "the setsid sleeper is running");
        assert!(process_exists(server), "the background server is running");

        let ending = std::time::Instant::now();
        enforcement.end_session();
        let took = ending.elapsed();

        let sleeper_gone = process_gone_within(sleeper, std::time::Duration::from_secs(2));
        let server_gone = process_gone_within(server, std::time::Duration::from_secs(2));
        let procs = std::fs::read_to_string(scope.path().join("cgroup.procs")).unwrap();
        let after = bash("true", temp.path(), &policy, &enforcement);
        println!(
            "end_session took {} ms; sleeper {sleeper} gone: {sleeper_gone}; server {server} \
             gone: {server_gone}; cgroup.procs after: {:?}; next command: {:?}",
            took.as_millis(),
            procs.trim(),
            after.as_ref().map(|result| result.exit_code)
        );
        assert!(took < std::time::Duration::from_secs(3), "took {took:?}");
        assert!(sleeper_gone, "the setsid sleeper outlived its session");
        assert!(server_gone, "the background server outlived its session");
        assert!(procs.trim().is_empty(), "cgroup.procs: {procs}");
        let refusal = after
            .expect_err("a command after the session ended must be refused")
            .to_string();
        assert!(refusal.contains("session has ended"), "{refusal}");
    }

    // ---- the session's temporary folder ----
    //
    // Each test below resolves its enforcement the way a session does, over a live `HostProbe`,
    // and maps `/tmp` with the answer staging would give. One that needs a private mount
    // namespace prints a `[SKIP-HOST]` line and returns on a host that cannot give one.

    /// A directory under `HOME`, outside `/tmp`, removed when the value drops.
    fn home_tempdir() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::var_os("HOME").expect("HOME is set")).unwrap()
    }

    /// A session's enforcement at `class` from a live probe, holding a runtime directory, records
    /// directory and temporary folder under `base`, with the `/tmp` mapping staging decides for
    /// `workdir`; `None` after a skip line naming `test` when that mapping is not
    /// [`crate::temp_dir::TempMapping::MountNamespace`].
    fn mapped_enforcement(
        test: &str,
        policy: &CapabilityPolicy,
        class: murmur_artifact::ContainmentClass,
        base: &Path,
        workdir: &Path,
    ) -> Option<ShellEnforcement> {
        if crate::network_namespace::skip_without_egress_namespace(test) {
            return None;
        }
        let probe = HostProbe::probe();
        let mount_namespace_usable = probe.mount_namespace_usable();
        let enforcement = ShellEnforcement::resolve_declared(policy, class, probe)
            .unwrap()
            .with_runtime_dir(test_runtime_dir(base), None);
        let mapping =
            crate::temp_dir::temp_mapping(enforcement.tier, mount_namespace_usable, workdir);
        if mapping != crate::temp_dir::TempMapping::MountNamespace {
            crate::runtime_err!(
                "[SKIP-HOST] {test}: this host maps no private /tmp at {class:?} ({mapping:?} on \
                 {:?})",
                enforcement.tier
            );
            return None;
        }
        Some(enforcement.with_temp_mapping(mapping))
    }

    /// The same session enforcement with the mount namespace probed as unusable: `KernelFull`,
    /// `/tmp` not mapped. `None` after a skip line on a host that has no `KernelFull` at all.
    fn unmapped_kernel_full_enforcement(
        test: &str,
        policy: &CapabilityPolicy,
        base: &Path,
        workdir: &Path,
    ) -> Option<ShellEnforcement> {
        if crate::network_namespace::skip_without_egress_namespace(test) {
            return None;
        }
        let live = HostProbe::probe();
        if !matches!(
            live.tier(),
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed
        ) {
            crate::runtime_err!(
                "[SKIP-HOST] {test}: needs Landlock; detected {:?}",
                live.tier()
            );
            return None;
        }
        let refused = crate::namespace_probe::MountAttempt::Failed(
            crate::namespace_probe::MountAttemptError::Step {
                step: crate::namespace_probe::MountStep::UnshareMount,
                errno: libc::EPERM,
            },
        );
        let probe = HostProbe::from_parts(
            Some(true),
            crate::namespace_probe::NamespaceReading {
                mount: refused.clone(),
                sealed_root: refused,
                ..live.namespaces().clone()
            },
        )
        .with_landlock_kernel_abi(live.landlock_kernel_abi());
        assert!(!probe.mount_namespace_usable());
        let enforcement = ShellEnforcement::resolve_declared(
            policy,
            murmur_artifact::ContainmentClass::Scoped,
            probe,
        )
        .unwrap()
        .with_runtime_dir(test_runtime_dir(base), None);
        assert_eq!(enforcement.tier, EnforcementTier::KernelFull);
        let mapping = crate::temp_dir::temp_mapping(enforcement.tier, false, workdir);
        assert_eq!(
            mapping,
            crate::temp_dir::TempMapping::NotMapped(
                crate::temp_dir::NotMappedReason::NoMountNamespace
            )
        );
        Some(enforcement.with_temp_mapping(mapping))
    }

    /// A name no other test or run uses beneath `/tmp`.
    fn unique_tmp_name(label: &str) -> String {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!(
            "murmur-probe-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    /// The parent-side spec re-exposes the work folder and the home folder read-write and the
    /// runtime and records directories and every grant path read-only, each at its exact path
    /// beneath `/tmp`, the home folder over the records directory it lies in.
    #[test]
    fn the_tmp_map_spec_reexposes_exact_paths_beneath_tmp_read_only_but_the_workdir() {
        let base = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        let grant_dir = tempfile::tempdir().unwrap();
        let grant = grant_dir.path().join("bin");
        std::fs::write(&grant, b"#!/bin/sh\n").unwrap();
        let mut enforcement = ShellEnforcement {
            tier: EnforcementTier::KernelFull,
            ..ShellEnforcement::environment_only().with_test_session_dirs()
        }
        .with_runtime_dir(test_runtime_dir(base.path()), None)
        .with_temp_mapping(crate::temp_dir::TempMapping::MountNamespace);
        enforcement.landlock_grants =
            LandlockGrant::non_listable_files(vec![grant.clone(), PathBuf::from("/usr/bin/env")]);
        let folder = enforcement
            .runtime_dir
            .as_ref()
            .and_then(|roots| roots.temp.as_ref())
            .unwrap()
            .path_fd()
            .unwrap();

        let spec = build_tmp_map_spec(&enforcement, &folder, workdir.path()).unwrap();

        let canonical = |path: &Path| {
            std::fs::canonicalize(path)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let mut expected = vec![
            (canonical(workdir.path()), false),
            (
                canonical(&base.path().join(crate::runtime_dir::RUNTIME_SUBDIR)),
                true,
            ),
            (
                canonical(&base.path().join(crate::records_dir::RECORDS_DIR_NAME)),
                true,
            ),
            (
                canonical(
                    &base
                        .path()
                        .join(crate::records_dir::RECORDS_DIR_NAME)
                        .join(crate::records_dir::HOME_SUBDIR),
                ),
                false,
            ),
            (canonical(&grant), true),
        ];
        expected.sort();
        assert_eq!(spec.reexposed_sources(), expected);
    }

    /// At `scoped` and at `advisory`, a literal `/tmp` path lands in the session's temporary
    /// folder, `TMPDIR` is `/tmp`, and the host's `/tmp` gains nothing.
    #[test]
    fn a_literal_tmp_path_reaches_the_session_temp_folder() {
        const TEST: &str = "a_literal_tmp_path_reaches_the_session_temp_folder";
        for class in [
            murmur_artifact::ContainmentClass::Scoped,
            murmur_artifact::ContainmentClass::Advisory,
        ] {
            let base = home_tempdir();
            let workdir = home_tempdir();
            let policy = coreutils_policy(&[]);
            let Some(enforcement) =
                mapped_enforcement(TEST, &policy, class, base.path(), workdir.path())
            else {
                return;
            };
            let name = unique_tmp_name("literal");
            let result = run_bash_in(
                workdir.path(),
                &format!("echo x > /tmp/{name} && cat /tmp/{name} && printf '%s\\n' \"$TMPDIR\""),
                &policy,
                &enforcement,
            );
            assert_eq!(result.exit_code, 0, "{class:?}: {}", result.stderr);
            assert_eq!(result.stdout, "x\n/tmp\n", "{class:?}");
            let folder = enforcement.temp_dir_path().unwrap();
            assert_eq!(std::fs::read_to_string(folder.join(&name)).unwrap(), "x\n");
            assert!(
                !Path::new("/tmp").join(&name).exists(),
                "{class:?}: the host's /tmp gained {name}"
            );
        }
    }

    /// `KernelSeccompOnly` maps `/tmp` through the same per-command mount namespace: the session
    /// enforcement with its tier set to seccomp-only, so no Landlock domain is installed.
    #[test]
    fn a_literal_tmp_path_reaches_the_session_temp_folder_on_the_seccomp_only_tier() {
        const TEST: &str =
            "a_literal_tmp_path_reaches_the_session_temp_folder_on_the_seccomp_only_tier";
        let base = home_tempdir();
        let workdir = home_tempdir();
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = mapped_enforcement(
            TEST,
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            base.path(),
            workdir.path(),
        ) else {
            return;
        };
        let enforcement = ShellEnforcement {
            tier: EnforcementTier::KernelSeccompOnly,
            ..enforcement
        };
        assert_eq!(
            enforcement.installed_temp_mapping(),
            crate::temp_dir::TempMapping::MountNamespace
        );
        let name = unique_tmp_name("seccomp-only");
        let result = run_bash_in(
            workdir.path(),
            &format!("echo x > /tmp/{name} && cat /tmp/{name} && printf '%s\\n' \"$TMPDIR\""),
            &policy,
            &enforcement,
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "x\n/tmp\n");
        let folder = enforcement.temp_dir_path().unwrap();
        assert_eq!(std::fs::read_to_string(folder.join(&name)).unwrap(), "x\n");
        assert!(
            !Path::new("/tmp").join(&name).exists(),
            "the host's /tmp gained {name}"
        );
    }

    /// Two sessions' commands each see their own folder at `/tmp`.
    #[test]
    fn two_sessions_do_not_share_tmp() {
        const TEST: &str = "two_sessions_do_not_share_tmp";
        let policy = coreutils_policy(&[]);
        let (base_a, base_b) = (home_tempdir(), home_tempdir());
        let (work_a, work_b) = (home_tempdir(), home_tempdir());
        let class = murmur_artifact::ContainmentClass::Scoped;
        let Some(a) = mapped_enforcement(TEST, &policy, class, base_a.path(), work_a.path()) else {
            return;
        };
        let Some(b) = mapped_enforcement(TEST, &policy, class, base_b.path(), work_b.path()) else {
            return;
        };
        let name = unique_tmp_name("shared");

        let first = run_bash_in(work_a.path(), &format!("echo a > /tmp/{name}"), &policy, &a);
        assert_eq!(first.exit_code, 0, "{}", first.stderr);
        let second = run_bash_in(
            work_b.path(),
            &format!("test ! -e /tmp/{name} && echo b > /tmp/{name} && cat /tmp/{name}"),
            &policy,
            &b,
        );
        assert_eq!(second.exit_code, 0, "{}", second.stderr);
        assert_eq!(second.stdout, "b\n");
        let again = run_bash_in(work_a.path(), &format!("cat /tmp/{name}"), &policy, &a);
        assert_eq!(again.stdout, "a\n", "{}", again.stderr);
        assert_ne!(a.temp_dir_path(), b.temp_dir_path());
        assert!(!Path::new("/tmp").join(&name).exists());
    }

    /// With the workdir, the runtime and records directories and `HOME` all beneath the host's
    /// `/tmp`, every one of them is still reached at its own path while `/tmp` is the folder.
    #[test]
    fn a_workdir_under_tmp_stays_reachable_when_tmp_is_mapped() {
        const TEST: &str = "a_workdir_under_tmp_stays_reachable_when_tmp_is_mapped";
        let base = tempfile::tempdir().unwrap();
        let workdir = tempfile::tempdir().unwrap();
        assert!(workdir.path().starts_with(crate::temp_dir::canonical_tmp()));
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = mapped_enforcement(
            TEST,
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            base.path(),
            workdir.path(),
        ) else {
            return;
        };
        std::fs::write(
            enforcement.runtime_dir_path().unwrap().join("probe"),
            b"runtime\n",
        )
        .unwrap();
        let name = unique_tmp_name("under");
        let result = run_bash_in(
            workdir.path(),
            &format!(
                "echo a > \"$(pwd)/f\" && cat \"$(pwd)/f\" && cat \"$MURMUR_RUNTIME_DIR/probe\" && \
                 echo h > \"$HOME/h\" && cat \"$HOME/h\" && echo t > /tmp/{name} && \
                 cat /tmp/{name}"
            ),
            &policy,
            &enforcement,
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout, "a\nruntime\nh\nt\n");
        assert_eq!(
            std::fs::read_to_string(workdir.path().join("f")).unwrap(),
            "a\n"
        );

        // Only the paths the command needs are re-exposed, and only the workdir writably: a
        // sibling of the records directory is absent, and the records directory refuses a write
        // although it now lies beneath the folder's writable rule.
        let sibling = base.path().join("sibling");
        std::fs::write(&sibling, b"host\n").unwrap();
        let record = enforcement.records_dir_path().unwrap().join("trace.jsonl");
        std::fs::write(&record, b"{}\n").unwrap();
        let refused = run_bash_in(
            workdir.path(),
            &format!(
                "test ! -e {sibling} && echo absent; echo z >> \"$MURMUR_RECORDS_DIR/trace.jsonl\"",
                sibling = sibling.display()
            ),
            &policy,
            &enforcement,
        );
        assert!(refused.stdout.starts_with("absent\n"), "{}", refused.stderr);
        assert_ne!(refused.exit_code, 0);
        assert!(
            refused.stderr.contains("Read-only file system"),
            "{}",
            refused.stderr
        );
        assert_eq!(std::fs::read(&record).unwrap(), b"{}\n");
        let folder = enforcement.temp_dir_path().unwrap();
        assert_eq!(std::fs::read_to_string(folder.join(&name)).unwrap(), "t\n");
        assert!(!Path::new("/tmp").join(&name).exists());
    }

    /// Without a usable mount namespace, `KernelFull` maps nothing: a literal `/tmp` write is
    /// refused by Landlock, and `$TMPDIR` is the folder's absolute path and works.
    #[test]
    fn without_a_mount_namespace_tmp_is_refused_and_tmpdir_works() {
        const TEST: &str = "without_a_mount_namespace_tmp_is_refused_and_tmpdir_works";
        let base = home_tempdir();
        let workdir = home_tempdir();
        let policy = coreutils_policy(&[]);
        let Some(enforcement) =
            unmapped_kernel_full_enforcement(TEST, &policy, base.path(), workdir.path())
        else {
            return;
        };
        let name = unique_tmp_name("refused");
        let refused = run_bash_in(
            workdir.path(),
            &format!("echo y > /tmp/{name}"),
            &policy,
            &enforcement,
        );
        assert_ne!(refused.exit_code, 0);
        assert!(
            refused.stderr.contains("Permission denied"),
            "{}",
            refused.stderr
        );
        assert!(!Path::new("/tmp").join(&name).exists());

        let folder = enforcement.temp_dir_path().unwrap().to_path_buf();
        let works = run_bash_in(
            workdir.path(),
            "echo z > \"$TMPDIR/m\" && cat \"$TMPDIR/m\" && printf '%s\\n' \"$TMPDIR\" \"$TMP\" \
             \"$TEMP\"",
            &policy,
            &enforcement,
        );
        assert_eq!(works.exit_code, 0, "{}", works.stderr);
        let folder_text = folder.to_string_lossy();
        assert_eq!(
            works.stdout,
            format!("z\n{folder_text}\n{folder_text}\n{folder_text}\n")
        );
        assert_eq!(std::fs::read_to_string(folder.join("m")).unwrap(), "z\n");
    }

    /// The records directory stays read-only apart from `tmp/`: no record can be linked, moved
    /// or appended into the folder, and the folder itself cannot be renamed — mapped and not.
    #[test]
    fn a_record_cannot_be_moved_or_linked_into_tmp() {
        const TEST: &str = "a_record_cannot_be_moved_or_linked_into_tmp";
        let policy = coreutils_policy(&[]);
        for mapped in [true, false] {
            let base = home_tempdir();
            let workdir = home_tempdir();
            let enforcement = if mapped {
                mapped_enforcement(
                    TEST,
                    &policy,
                    murmur_artifact::ContainmentClass::Scoped,
                    base.path(),
                    workdir.path(),
                )
            } else {
                unmapped_kernel_full_enforcement(TEST, &policy, base.path(), workdir.path())
            };
            let Some(enforcement) = enforcement else {
                return;
            };
            let records = enforcement.records_dir_path().unwrap().to_path_buf();
            let record = records.join("trace.jsonl");
            std::fs::write(&record, b"{\"event\":\"session_start\"}\n").unwrap();
            let before = std::fs::read(&record).unwrap();
            for script in [
                "ln \"$MURMUR_RECORDS_DIR/trace.jsonl\" /tmp/l",
                "ln \"$MURMUR_RECORDS_DIR/trace.jsonl\" \"$TMPDIR/l2\"",
                "ln \"$MURMUR_RECORDS_DIR/trace.jsonl\" \"$MURMUR_RECORDS_DIR/tmp/l3\"",
                "mv \"$MURMUR_RECORDS_DIR/trace.jsonl\" \"$TMPDIR/m\"",
                "mv \"$MURMUR_RECORDS_DIR/trace.jsonl\" \"$MURMUR_RECORDS_DIR/tmp/m2\"",
                "echo z >> \"$MURMUR_RECORDS_DIR/trace.jsonl\"",
                "mv \"$MURMUR_RECORDS_DIR/tmp\" \"$MURMUR_RECORDS_DIR/tmp-moved\"",
            ] {
                let result = run_bash_in(workdir.path(), script, &policy, &enforcement);
                assert_ne!(
                    result.exit_code, 0,
                    "mapped: {mapped}: `{script}` succeeded"
                );
            }
            assert_eq!(std::fs::read(&record).unwrap(), before, "mapped: {mapped}");
            assert!(records
                .join(crate::temp_dir::SESSION_TEMP_DIR_NAME)
                .is_dir());
            assert!(!records.join("tmp-moved").exists());
        }
    }

    /// The folder carries the workdir's right-set, `Execute` axis included: a binary copied into
    /// `/tmp` runs only with `workdir_exec: true`.
    #[test]
    fn tmp_follows_workdir_exec() {
        const TEST: &str = "tmp_follows_workdir_exec";
        for (workdir_exec, expected) in [(false, 126), (true, 0)] {
            let base = home_tempdir();
            let workdir = home_tempdir();
            let policy = CapabilityPolicy {
                workdir_exec_allowed: workdir_exec,
                ..coreutils_policy(&["cp", "true"])
            };
            let Some(enforcement) = mapped_enforcement(
                TEST,
                &policy,
                murmur_artifact::ContainmentClass::Advisory,
                base.path(),
                workdir.path(),
            ) else {
                return;
            };
            let result = run_bash_in(
                workdir.path(),
                "cp /bin/true /tmp/t && /tmp/t",
                &policy,
                &enforcement,
            );
            assert_eq!(
                result.exit_code, expected,
                "workdir_exec: {workdir_exec}: {}",
                result.stderr
            );
            if !workdir_exec {
                assert!(
                    result.stderr.contains("Permission denied"),
                    "{}",
                    result.stderr
                );
            }
        }
    }

    /// A mapping that cannot be built fails the spawn with its own error and runs nothing.
    #[test]
    fn a_failed_tmp_mapping_spawns_nothing() {
        const TEST: &str = "a_failed_tmp_mapping_spawns_nothing";
        let base = home_tempdir();
        let workdir = home_tempdir();
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = mapped_enforcement(
            TEST,
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            base.path(),
            workdir.path(),
        ) else {
            return;
        };
        let error = {
            let _forced = ForceTmpMapFailureGuard::new();
            crate::shell::execute_shell(
                "bash",
                &["-c", "echo ran > \"$(pwd)/marker\""],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
                &policy,
                &enforcement,
            )
            .expect_err("a failed mapping fails the spawn")
        };
        let message = error.to_string();
        println!("{message}");
        assert!(
            message.contains("could not map /tmp onto this session's temporary folder"),
            "{message}"
        );
        assert!(
            matches!(error, crate::shell::ShellExecError::Failed(_)),
            "an ordinary spawn failure, not a session-fatal one: {error:?}"
        );
        assert!(!workdir.path().join("marker").exists(), "the command ran");
    }

    /// A link swapped onto the folder's path after the session holds it redirects no bind: the
    /// child's open reaches another directory, and the spawn fails naming the step.
    #[test]
    fn a_link_swapped_onto_the_temp_folder_redirects_no_bind() {
        const TEST: &str = "a_link_swapped_onto_the_temp_folder_redirects_no_bind";
        let base = home_tempdir();
        let workdir = home_tempdir();
        let policy = coreutils_policy(&[]);
        let Some(enforcement) = mapped_enforcement(
            TEST,
            &policy,
            murmur_artifact::ContainmentClass::Scoped,
            base.path(),
            workdir.path(),
        ) else {
            return;
        };
        let folder = enforcement.temp_dir_path().unwrap().to_path_buf();
        let decoy = base.path().join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        std::fs::rename(&folder, base.path().join("folder-moved")).unwrap();
        std::os::unix::fs::symlink(&decoy, &folder).unwrap();

        let error = crate::shell::execute_shell(
            "bash",
            &["-c", "echo x > /tmp/swapped"],
            &[],
            &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
            &policy,
            &enforcement,
        )
        .expect_err("a swapped folder fails the spawn")
        .to_string();
        println!("{error}");
        assert!(
            error.contains("could not map /tmp onto this session's temporary folder"),
            "{error}"
        );
        assert!(error.contains("opening the temporary folder"), "{error}");
        assert!(
            error.contains(&std::io::Error::from_raw_os_error(libc::ESTALE).to_string()),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(&decoy).unwrap().count(),
            0,
            "the link's target was written"
        );
    }

    /// A policy allowlisting `binary` with its interpreter runtime derived the way
    /// `stage_session` derives it, `work` excluded as the capsule-writable root.
    fn derived_interpreter_policy(binary: &str, work: &Path) -> CapabilityPolicy {
        let mut policy = CapabilityPolicy {
            shell_allow: vec![binary.to_string()],
            ..CapabilityPolicy::default()
        };
        policy.shell_derived_interpreter_runtime =
            crate::interpreter_runtime::derive_interpreter_runtimes(
                &policy.shell_allow,
                &[],
                &[work],
            );
        policy
    }

    /// What `program` prints when this host's `python3` runs it outside any sandbox.
    fn unsandboxed_python(program: &str) -> String {
        let output = std::process::Command::new("python3")
            .args(["-I", "-c", program])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Asserts the derived runtime holds the directories `encodings` and `_sqlite3` load from on
    /// this host, as the interpreter itself reports them, and returns the first: the stdlib.
    fn assert_derives_stdlib_and_sqlite(policy: &CapabilityPolicy) -> PathBuf {
        let stdlib = PathBuf::from(unsandboxed_python(
            "import encodings, os; print(os.path.dirname(os.path.dirname(encodings.__file__)))",
        ));
        let dynload = PathBuf::from(unsandboxed_python(
            "import _sqlite3, os; print(os.path.dirname(os.path.realpath(_sqlite3.__file__)))",
        ));
        let derived = &policy.shell_derived_interpreter_runtime;
        println!("derived: {derived:#?}");
        let dirs: Vec<&PathBuf> = derived.iter().flat_map(|runtime| &runtime.dirs).collect();
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();
        assert!(
            dirs.contains(&&canonical(&stdlib)),
            "{stdlib:?} not in {dirs:?}"
        );
        assert!(
            dirs.contains(&&canonical(&dynload)),
            "{dynload:?} not in {dirs:?}"
        );
        canonical(&stdlib)
    }

    /// The program that opens `secret` and lists `parent`, each in its own `try`, printing the
    /// exception class name or `allowed`.
    fn python_probe_outside(secret: &Path, parent: &Path) -> String {
        format!(
            "import os\n\
             for label, act in (('open', lambda: open({secret:?}).read()), \
             ('listdir', lambda: os.listdir({parent:?}))):\n\
             \x20   try:\n\
             \x20       act(); print(label, 'allowed')\n\
             \x20   except Exception as error:\n\
             \x20       print(label, type(error).__name__)\n",
            secret = secret.to_string_lossy(),
            parent = parent.to_string_lossy(),
        )
    }

    fn execute(
        binary: &str,
        args: &[&str],
        accessible: &Path,
        policy: &CapabilityPolicy,
        enforcement: &ShellEnforcement,
    ) -> crate::shell::ShellResult {
        let result = crate::shell::execute_shell(
            binary,
            args,
            &[],
            &crate::workdir_fs::WorkdirRoot::open(accessible).unwrap(),
            policy,
            enforcement,
        )
        .unwrap();
        println!(
            "exit {}\n--- stdout\n{}--- stderr\n{}---",
            result.exit_code, result.stdout, result.stderr
        );
        result
    }

    /// A policy allowlisting `shell_allow` and reaching `network_allow`, with the trust store
    /// derived the way `stage_session` derives it, `work` excluded as the capsule-writable root.
    fn trust_store_policy(
        shell_allow: &[&str],
        network_allow: &[&str],
        work: &Path,
    ) -> CapabilityPolicy {
        let mut policy = CapabilityPolicy {
            shell_allow: shell_allow.iter().map(|name| name.to_string()).collect(),
            network_allow: network_allow.iter().map(|rule| rule.to_string()).collect(),
            ..CapabilityPolicy::default()
        };
        policy.shell_trust_store = crate::trust_store::derive_trust_store(
            &policy.shell_allow,
            &policy.network_allow,
            &[work],
        );
        println!("derived trust store: {:#?}", policy.shell_trust_store);
        policy
    }

    /// The first CA bundle file of the candidate table this host has.
    fn host_ca_bundle() -> Option<PathBuf> {
        crate::trust_store::TRUST_STORE_CANDIDATES
            .iter()
            .filter(|candidate| !candidate.directory)
            .map(|candidate| PathBuf::from(candidate.path))
            .find(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file()))
    }

    /// An allowlisted `curl` in a capsule with a network allowlist and no `interpreter_runtime`
    /// block verifies a server certificate under `scoped`.
    #[test]
    fn a_scoped_curl_with_network_allow_completes_an_https_request() {
        let test = "a_scoped_curl_with_network_allow_completes_an_https_request";
        if find_on_path("curl").is_none() {
            eprintln!("SKIP — PROVES NOTHING: {test} needs curl on PATH");
            return;
        }
        let unsandboxed = std::process::Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "15",
            ])
            .arg("https://example.com/")
            .output()
            .unwrap();
        if String::from_utf8_lossy(&unsandboxed.stdout) != "200" {
            eprintln!(
                "SKIP — PROVES NOTHING: {test} needs https://example.com/ to answer 200 outside \
                 the sandbox; it gave {unsandboxed:?}"
            );
            return;
        }
        let (_base, work, _outside) = work_and_outside();
        let policy = trust_store_policy(&["curl"], &["example.com"], &work);
        assert!(policy.shell_interpreter_runtime.is_empty());
        assert!(
            !policy.shell_trust_store.is_empty(),
            "this host has a trust store the candidate table names"
        );
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };

        let fetched = execute(
            "curl",
            &[
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "30",
                "https://example.com/",
            ],
            &work,
            &policy,
            &enforcement,
        );
        enforcement.end_session();
        assert_eq!(fetched.exit_code, 0, "{}", fetched.stderr);
        assert_eq!(fetched.stdout, "200");
    }

    /// The trust store reaches a command only when the capsule declares a network allowlist.
    #[test]
    fn a_scoped_command_reads_the_trust_store_only_with_network_allow() {
        let test = "a_scoped_command_reads_the_trust_store_only_with_network_allow";
        assert!(
            crate::trust_store::derive_trust_store(&[], &["example.com".into()], &[]).is_empty()
        );
        let Some(bundle) = host_ca_bundle() else {
            eprintln!("SKIP — PROVES NOTHING: {test} needs a CA bundle the candidate table names");
            return;
        };
        let bundle_arg = bundle.to_str().unwrap();
        let (_base, work, _outside) = work_and_outside();

        let offline = trust_store_policy(&["cat"], &[], &work);
        assert!(offline.shell_trust_store.is_empty());
        let online = trust_store_policy(&["cat"], &["127.0.0.1"], &work);
        assert!(!online.shell_trust_store.is_empty());
        let Some(enforcement) = scoped_landlock_enforcement(test, &offline, 1) else {
            return;
        };
        for derived in &online.shell_trust_store {
            assert!(
                !enforcement
                    .landlock_grants
                    .iter()
                    .any(|grant| grant.path == derived.path),
                "{derived:?} granted without network.allow"
            );
        }
        let denied = execute("cat", &[bundle_arg], &work, &offline, &enforcement);
        enforcement.end_session();
        assert_ne!(denied.exit_code, 0);
        assert!(
            denied.stderr.contains("Permission denied"),
            "{}",
            denied.stderr
        );

        let Some(enforcement) = scoped_landlock_enforcement(test, &online, 1) else {
            return;
        };
        let read = execute("cat", &[bundle_arg], &work, &online, &enforcement);
        enforcement.end_session();
        assert_eq!(read.exit_code, 0, "{}", read.stderr);
        assert!(
            read.stdout.starts_with("-----BEGIN CERTIFICATE-----"),
            "{}",
            &read.stdout[..read.stdout.len().min(200)]
        );
    }

    /// The trust-store grant reaches the certificates and nothing beside them: not the directory
    /// above, its OpenSSL configuration or its private keys, and nothing is writable.
    #[test]
    fn the_trust_store_grant_reaches_nothing_beyond_the_trust_store() {
        let test = "the_trust_store_grant_reaches_nothing_beyond_the_trust_store";
        let (_base, work, _outside) = work_and_outside();
        let policy = trust_store_policy(&["cat", "ls", "bash"], &["127.0.0.1"], &work);
        for entry in &policy.shell_trust_store {
            for broad in ["/", "/etc", "/etc/ssl", "/etc/pki", "/etc/pki/tls"] {
                assert_ne!(entry.path, Path::new(broad));
            }
            assert!(
                !entry.path.components().any(|c| c.as_os_str() == "private"),
                "{entry:?}"
            );
        }
        assert!(resolve_trust_store_grants(&policy.shell_trust_store)
            .iter()
            .all(|grant| !grant.executable));
        if std::fs::metadata("/etc/ssl/certs").map_or(true, |m| !m.is_dir()) {
            eprintln!("SKIP — PROVES NOTHING: {test} needs /etc/ssl/certs on this host");
            return;
        }
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };

        let listed = execute("ls", &["/etc/ssl/certs"], &work, &policy, &enforcement);
        assert_eq!(listed.exit_code, 0, "{}", listed.stderr);
        let above = execute("ls", &["/etc/ssl"], &work, &policy, &enforcement);
        assert_ne!(above.exit_code, 0);
        assert!(
            above.stderr.contains("Permission denied"),
            "{}",
            above.stderr
        );
        if std::fs::metadata("/etc/ssl/openssl.cnf").is_ok() {
            let config = execute(
                "cat",
                &["/etc/ssl/openssl.cnf"],
                &work,
                &policy,
                &enforcement,
            );
            assert_ne!(config.exit_code, 0);
            assert!(
                config.stderr.contains("Permission denied"),
                "{}",
                config.stderr
            );
        }
        let private = execute("ls", &["/etc/ssl/private"], &work, &policy, &enforcement);
        assert_ne!(private.exit_code, 0);
        let probe = Path::new("/etc/ssl/certs/murmur-probe");
        let written = execute(
            "bash",
            &["-c", "echo x > /etc/ssl/certs/murmur-probe"],
            &work,
            &policy,
            &enforcement,
        );
        enforcement.end_session();
        assert_ne!(written.exit_code, 0);
        assert!(std::fs::symlink_metadata(probe).is_err());
    }

    #[test]
    fn resolve_trust_store_grants_reads_and_lists_but_never_executes() {
        let paths = [
            crate::trust_store::TrustStorePath {
                path: PathBuf::from("/etc/ssl/certs"),
                directory: true,
            },
            crate::trust_store::TrustStorePath {
                path: PathBuf::from("/etc/pki/tls/cacert.pem"),
                directory: false,
            },
        ];
        assert_eq!(
            resolve_trust_store_grants(&paths),
            vec![
                LandlockGrant {
                    path: PathBuf::from("/etc/ssl/certs"),
                    list_dir: true,
                    executable: false,
                    pinned: None,
                },
                LandlockGrant {
                    path: PathBuf::from("/etc/pki/tls/cacert.pem"),
                    list_dir: false,
                    executable: false,
                    pinned: None,
                },
            ]
        );
        assert!(resolve_trust_store_grants(&[]).is_empty());
    }

    /// `ShellEnforcement::resolve` adds exactly the policy's trust-store grants on both Landlock
    /// tiers, whatever the host, and leaves the sealed `/etc` grants in place.
    #[test]
    fn shell_enforcement_resolve_adds_the_trust_store() {
        use murmur_artifact::ContainmentClass;
        let store = vec![
            crate::trust_store::TrustStorePath {
                path: PathBuf::from("/murmur-test/certs"),
                directory: true,
            },
            crate::trust_store::TrustStorePath {
                path: PathBuf::from("/murmur-test/bundle.pem"),
                directory: false,
            },
        ];
        let expected = resolve_trust_store_grants(&store);
        let with_store = CapabilityPolicy {
            shell_trust_store: store.clone(),
            ..CapabilityPolicy::default()
        };
        let without = CapabilityPolicy::default();
        for (tier, declared) in [
            (EnforcementTier::KernelFull, ContainmentClass::Scoped),
            (EnforcementTier::KernelSealed, ContainmentClass::Sealed),
        ] {
            let decision =
                crate::containment::ContainmentDecision::decide(tier, declared, false, false);
            let granted = |policy: &CapabilityPolicy| {
                let enforcement =
                    ShellEnforcement::resolve(policy, &decision, HostProbe::probe()).unwrap();
                assert_eq!(enforcement.tier, tier);
                enforcement.landlock_grants
            };
            let with_grants = granted(&with_store);
            let without_grants = granted(&without);
            for grant in &expected {
                assert_eq!(
                    with_grants.iter().filter(|g| *g == grant).count(),
                    1,
                    "{tier:?}: {grant:?}"
                );
            }
            assert!(without_grants
                .iter()
                .all(|grant| !grant.path.starts_with("/murmur-test")));
            assert_eq!(with_grants.len(), without_grants.len() + expected.len());
            let sealed_etc = resolve_sealed_etc_landlock_grants(tier);
            assert_eq!(sealed_etc.is_empty(), tier != EnforcementTier::KernelSealed);
            for grant in &sealed_etc {
                assert!(with_grants.contains(grant), "{tier:?}: {grant:?}");
            }
        }
    }

    /// An allowlisted `python3` with no `interpreter_runtime` block imports its standard library
    /// and a C extension needing a library outside its own closure under `scoped`, while a file
    /// outside its runtime and the directory above its stdlib stay denied.
    #[test]
    fn a_scoped_python3_imports_its_stdlib_and_nothing_outside_it() {
        let test = "a_scoped_python3_imports_its_stdlib_and_nothing_outside_it";
        if find_on_path("python3").is_none() {
            println!("[SKIP] {test}: no python3 on PATH");
            return;
        }
        let (_base, work, outside) = work_and_outside();
        let policy = derived_interpreter_policy("python3", &work);
        let stdlib = assert_derives_stdlib_and_sqlite(&policy);
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };

        let imported = execute(
            "python3",
            &["-c", "import json, sqlite3, venv; print('imported')"],
            &work,
            &policy,
            &enforcement,
        );
        assert_eq!(imported.exit_code, 0, "{}", imported.stderr);
        assert_eq!(imported.stdout.trim(), "imported");

        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        let probe = python_probe_outside(&secret, stdlib.parent().unwrap());
        let denied = execute("python3", &["-c", &probe], &work, &policy, &enforcement);
        assert_eq!(denied.exit_code, 0, "{}", denied.stderr);
        assert_eq!(
            denied.stdout.lines().collect::<Vec<_>>(),
            vec!["open PermissionError", "listdir PermissionError"]
        );
    }

    /// The same interpreter under `sealed`: it imports what it imported under `scoped`, and the
    /// file outside its runtime is neither readable nor present in the composed root.
    #[test]
    fn a_sealed_python3_imports_its_stdlib_and_nothing_outside_it() {
        let test = "a_sealed_python3_imports_its_stdlib_and_nothing_outside_it";
        if crate::network_namespace::skip_without_egress_namespace(test) {
            return;
        }
        if find_on_path("python3").is_none() {
            println!("[SKIP] {test}: no python3 on PATH");
            return;
        }
        let probe = HostProbe::probe();
        if probe.tier() != EnforcementTier::KernelSealed {
            eprintln!(
                "SKIP — PROVES NOTHING ABOUT THE SEALED ROOT ON THIS HOST: {test} needs a host \
                 that reaches KernelSealed; detected {:?}. No composed root is built in this run.",
                probe.tier()
            );
            return;
        }
        let (_base, work, outside) = work_and_outside();
        let policy = derived_interpreter_policy("python3", &work);
        assert_derives_stdlib_and_sqlite(&policy);
        let runtime = tempfile::tempdir().unwrap();
        let enforcement = ShellEnforcement::resolve_declared(
            &policy,
            murmur_artifact::ContainmentClass::Sealed,
            probe,
        )
        .expect("a sealed-capable host resolves sealed enforcement")
        .with_runtime_dir(test_runtime_dir(runtime.path()), None);
        assert_eq!(enforcement.tier, EnforcementTier::KernelSealed);

        let imported = execute(
            "python3",
            &["-c", "import json, sqlite3, venv; print('imported')"],
            &work,
            &policy,
            &enforcement,
        );
        assert_eq!(imported.exit_code, 0, "{}", imported.stderr);
        assert_eq!(imported.stdout.trim(), "imported");

        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "secret").unwrap();
        let program = format!(
            "try:\n    open({:?}).read(); print('allowed')\nexcept Exception as error:\n    \
             print(type(error).__name__)\n",
            secret.to_string_lossy()
        );
        let denied = execute("python3", &["-c", &program], &work, &policy, &enforcement);
        assert_eq!(denied.exit_code, 0, "{}", denied.stderr);
        let outcome = denied.stdout.trim();
        assert!(
            outcome == "PermissionError" || outcome == "FileNotFoundError",
            "{outcome}"
        );
    }

    /// An allowlisted `perl` with no `interpreter_runtime` block loads `POSIX`, an XS module, out
    /// of its own `@INC` under `scoped`.
    #[test]
    fn a_scoped_perl_loads_posix_from_its_inc() {
        let test = "a_scoped_perl_loads_posix_from_its_inc";
        if find_on_path("perl").is_none() {
            println!("[SKIP] {test}: no perl on PATH");
            return;
        }
        let (_base, work, _outside) = work_and_outside();
        let policy = derived_interpreter_policy("perl", &work);
        assert!(
            !policy.shell_derived_interpreter_runtime.is_empty(),
            "perl was not recognised or reported nothing"
        );
        let Some(enforcement) = scoped_landlock_enforcement(test, &policy, 1) else {
            return;
        };
        let result = execute(
            "perl",
            &["-MPOSIX", "-e", "print qq{loaded\n}"],
            &work,
            &policy,
            &enforcement,
        );
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        assert_eq!(result.stdout.trim(), "loaded");
    }
}
