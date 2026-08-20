//! The capsule's own network namespace: the structural half of the mechanism that replaced the
//! seccomp-notify `connect`/`sendto` supervisor.
//!
//! [`crate::egress_proxy`] decides *which* destinations a native subprocess may reach. This module
//! is what makes that decision the only way out — it puts the subprocess tree somewhere with no
//! route off the host, so an unlisted destination is not "denied" by a filter that has to be
//! consulted, it is simply unreachable.
//!
//! ## The syscall sequence
//!
//! Everything happens in the forked child's `pre_exec` window, before the seccomp filter (which
//! denies `unshare`, `setns` and `socket(AF_NETLINK)` outright) and before [`crate::sealed`]'s
//! composed root:
//!
//!   1. `unshare(CLONE_NEWUSER | CLONE_NEWNET)`, then the identity `uid_map`/`gid_map` writes that
//!      make the child the owner of both. The user namespace is not incidental: `CLONE_NEWNET`
//!      alone needs `CAP_NET_ADMIN`, which an unprivileged process does not have, and creating it
//!      inside a user namespace it owns is what supplies that capability *there and nowhere else*.
//!      This is the same primitive `sealed` uses and the same AppArmor restriction governs it —
//!      see [`EgressNamespaceBlocker`].
//!   2. `lo` is brought up ([`bring_loopback_up`]). It is the namespace's only interface: there is
//!      no veth pair, no bridge and no NAT, so nothing in the namespace has a path to the host's
//!      network or to the outside world.
//!   3. One route is installed over netlink ([`add_local_default_route`]): `local default dev lo`.
//!      A `local`-type default route makes every address *locally deliverable*, so a `connect()`
//!      to any address is looped back to a socket in this namespace with the original destination
//!      intact — recoverable with `getsockname(2)`. That is the entire interception mechanism,
//!      and it is why no netfilter rule is needed anywhere. It also means a lookup reaches the
//!      resolver whatever address `/etc/resolv.conf` names, and a connection to a port nothing is
//!      bound to is refused immediately rather than stalling through the kernel's full SYN-retry
//!      schedule against a blackhole.
//!   4. The listening sockets are created here, *inside* the namespace — one TCP listener per
//!      port the allowlist implies, then one wildcard UDP :53 resolver socket — and handed back
//!      to the parent in a single `SCM_RIGHTS` message. A socket belongs to the namespace it was
//!      **created** in, not the one it is used from, which is the whole reason this inversion
//!      works: the runtime serves them while staying in the host's namespace, with real internet
//!      access for the upstream half.
//!
//! ## Why this shape rather than the obvious ones
//!
//! *No veth pair.* Wiring a namespace to the host with veth requires `CAP_NET_ADMIN` in the
//! **host's** network namespace, which means real root. This design needs no host privilege at
//! all, which is what lets `mur` install per-user.
//!
//! *No proxy process inside the namespace.* `setns(2)` into the namespace from a thread of the
//! runtime is not possible: it requires `CAP_SYS_ADMIN` in the caller's *own* user namespace, and
//! an unprivileged multithreaded process cannot get it (`unshare(CLONE_NEWUSER)` requires a
//! single-threaded process). Passing the listening sockets *out* inverts the problem and needs no
//! privilege whatsoever. This was established empirically, not assumed — the `setns` route was
//! tried first and returned `EPERM`.
//!
//! *No `iptables`/`nft`/`TPROXY`.* Nothing here needs a netfilter rule, so there is no dependency
//! on netfilter modules being present, no `iptables` binary to shell out to, and no rule set that
//! can drift out of step with the manifest.
//!
//! ## What this does not isolate
//!
//! `AF_UNIX` is untouched. `CLONE_NEWNET` does not mediate unix sockets in any way that matters
//! here — a pathname socket is reached through the filesystem, not the network stack. That is
//! exactly why the register-level `socket(2)` domain filter (`denied_socket_domains` in
//! [`crate::sandbox`]) is untouched here and remains the thing that refuses
//! `/var/run/docker.sock`.

/// One TCP listener per allowlisted port, plus the single wildcard UDP :53 resolver socket.
///
/// Bounds the one `SCM_RIGHTS` message the child sends, and therefore the fixed stack buffers the
/// `pre_exec` window builds it in.
pub(crate) const MAX_NAMESPACE_SOCKETS: usize = crate::egress_proxy::MAX_EGRESS_TCP_PORTS + 1;

// ---------------------------------------------------------------- blockers

/// Why this host cannot give a capsule its own network namespace.
///
/// Exactly two variants, because they are the two things an operator can act on and their
/// remediations do not overlap: one is a permission this host withholds from the `mur` binary,
/// the other is a kernel that does not implement the mechanism at all. Collapsing them would send
/// half the readers to the wrong fix.
///
/// Deliberately **not** an `EnforcementTier` variant and deliberately not tied to
/// `ContainmentClass`: a network namespace is required for every Linux capsule that can spawn a
/// subprocess, at every containment class, so hanging it off the class ladder would make
/// `advisory` capsules silently exempt from this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressNamespaceBlocker {
    /// The kernel implements unprivileged user namespaces, but this host withholds them from this
    /// binary: AppArmor's `restrict_unprivileged_userns` is on and the shipped profile is not
    /// confining `mur`, or `unshare` was refused outright (the container case), or the namespace
    /// was created and could not then be owned or configured.
    CapabilityGrantMissing,
    /// The kernel does not provide the mechanism at all — `CONFIG_USER_NS=n`, or
    /// `user.max_user_namespaces=0`.
    KernelSupportMissing,
}

impl EgressNamespaceBlocker {
    /// Every variant, so a caller reasoning about refusal text cannot silently miss one. Same
    /// convention as `SealedBlocker::ALL`, which was added there after a hand-maintained list
    /// drifted the moment a variant appeared.
    pub const ALL: &'static [EgressNamespaceBlocker] = &[
        EgressNamespaceBlocker::CapabilityGrantMissing,
        EgressNamespaceBlocker::KernelSupportMissing,
    ];

    /// One sentence naming the missing mechanism, plus the exact remediation. Rendered into
    /// [`crate::errors::RuntimeError::EgressNamespaceUnavailable`], so this is the text an
    /// operator sees under `E-CAP-005`.
    #[must_use]
    pub fn reason(self) -> String {
        match self {
            EgressNamespaceBlocker::CapabilityGrantMissing => format!(
                "this host refused unshare(CLONE_NEWUSER | CLONE_NEWNET) to the mur binary. On an \
                 AppArmor host (Ubuntu 23.10+ and derivatives) this is the unprivileged-userns \
                 restriction: install and load the profile shipped with mur, `sudo install -m 644 \
                 packaging/apparmor/{name} {path} && sudo apparmor_parser -r {path}` (or re-run \
                 the mur installer as root), then re-run. Inside a container it is a missing \
                 capability: add `--cap-add SYS_ADMIN` to the container invocation, or create the \
                 network namespace outside the container and run mur inside it. The runtime will \
                 not fall back to the retired seccomp connect/sendto interception — that \
                 mechanism was removed as unsound, not demoted to a fallback.",
                name = crate::sealed::SEALED_APPARMOR_PROFILE_NAME,
                path = crate::sealed::SEALED_APPARMOR_PROFILE_PATH,
            ),
            EgressNamespaceBlocker::KernelSupportMissing => {
                "this kernel does not provide unprivileged user namespaces, which a capsule's \
                 network namespace has to be created inside (CONFIG_USER_NS=n, or \
                 user.max_user_namespaces=0). Raise `sudo sysctl -w \
                 user.max_user_namespaces=10000` if the sysctl is merely zeroed, otherwise run on \
                 a kernel built with CONFIG_USER_NS=y. The runtime will not fall back to the \
                 retired seccomp connect/sendto interception — that mechanism was removed as \
                 unsound, not demoted to a fallback."
                    .to_string()
            }
        }
    }
}

// ---------------------------------------------------------------- probe

/// What a real `unshare(CLONE_NEWUSER | CLONE_NEWNET)` attempt in a forked child did.
///
/// Richer than the two-valued [`EgressNamespaceBlocker`] on purpose: the probe records the step
/// that failed, and the mapping to a blocker is then an explicit, reviewable, unit-tested
/// decision rather than something the probe pre-decided in the dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EgressNamespaceProbe {
    /// The namespace was created, owned, and configured — `lo` came up and the local route
    /// installed. Everything the real launch does, rehearsed.
    Ok,
    /// `unshare` was refused (`EPERM`).
    Denied,
    /// The namespace was created but writing the identity `uid_map`/`gid_map` was refused.
    MapDenied,
    /// Created and owned, but configuring it failed. Kept apart from `Denied` because it is the
    /// signature of a confinement that permits namespace creation and then withholds
    /// `CAP_NET_ADMIN` inside it.
    ConfigDenied,
    /// The kernel does not implement it, or the probe could not run at all.
    #[default]
    Unsupported,
}

/// Everything the host probe learned, kept separate from the decision that uses it so
/// [`egress_namespace_blocker`] stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct EgressNamespaceSupport {
    /// AppArmor is not standing between this binary and an unprivileged user namespace.
    ///
    /// Answered by `sealed`'s own probe of exactly the same question: the restriction is on
    /// `CLONE_NEWUSER`, which both mechanisms need, so asking it twice with two implementations
    /// could only ever produce two answers that disagree about one host.
    pub(crate) apparmor_permits_userns: bool,
    pub(crate) namespace: EgressNamespaceProbe,
}

/// Which of the two blockers to name, given the probe. Pure, and unit-testable on any OS.
///
/// `None` means this host can give a capsule its own network namespace.
pub(crate) fn egress_namespace_blocker(
    is_linux: bool,
    support: EgressNamespaceSupport,
) -> Option<EgressNamespaceBlocker> {
    // Not Linux: there is no network namespace here, and there was no seccomp interception
    // either. A non-Linux host resolves to `EnforcementTier::EnvironmentOnly`, installs no kernel
    // subprocess sandbox at all, and says so loudly (`W_SEC_001`). Refusing here would turn every
    // macOS run into an error about a mechanism macOS has never had.
    if !is_linux {
        return None;
    }
    // AppArmor before the namespace outcome, for the reason `sealed_blocker` documents: when the
    // restriction is on and our profile is absent, the `unshare` failure is a *consequence*, and
    // blaming the kernel would send an Ubuntu desktop user somewhere useless.
    if !support.apparmor_permits_userns {
        return Some(EgressNamespaceBlocker::CapabilityGrantMissing);
    }
    match support.namespace {
        EgressNamespaceProbe::Ok => None,
        EgressNamespaceProbe::Denied
        | EgressNamespaceProbe::MapDenied
        | EgressNamespaceProbe::ConfigDenied => Some(EgressNamespaceBlocker::CapabilityGrantMissing),
        EgressNamespaceProbe::Unsupported => Some(EgressNamespaceBlocker::KernelSupportMissing),
    }
}

/// Which blocker, if any, stands between this host and a capsule network namespace.
///
/// Reads a process-cached probe, so the refusal an operator sees at staging time and the
/// mechanism a launch actually installs can never disagree about the host.
#[cfg(target_os = "linux")]
#[must_use]
pub fn detect_egress_namespace_blocker() -> Option<EgressNamespaceBlocker> {
    static SUPPORT: std::sync::OnceLock<EgressNamespaceSupport> = std::sync::OnceLock::new();
    let support = *SUPPORT.get_or_init(linux::probe_egress_namespace);
    egress_namespace_blocker(true, support)
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn detect_egress_namespace_blocker() -> Option<EgressNamespaceBlocker> {
    egress_namespace_blocker(false, EgressNamespaceSupport::default())
}

/// Whether a test that needs a capsule network namespace must stand down on this host, printing
/// the blocker when it must.
///
/// Test support, not a runtime code path. Asked through [`detect_egress_namespace_blocker`], the
/// same probe `stage_session` consults, so the gate and the staging refusal cannot reach different
/// conclusions about the host.
///
/// Narrower than [`crate::skip_without_host_support`], which also requires a delegated cgroup
/// scope: a test driving `execute_shell` directly needs the namespace and nothing else, and
/// standing down on the cgroup half would skip it on hosts where it runs perfectly well.
pub fn skip_without_egress_namespace(test_name: &str) -> bool {
    match detect_egress_namespace_blocker() {
        Some(blocker) => {
            eprintln!("[SKIP-HOST] {test_name}: {}", blocker.reason());
            true
        }
        None => false,
    }
}

/// Refuses the launch when a capsule that can spawn native subprocesses is on a host that cannot
/// give them a network namespace.
///
/// Pure — the host answer is passed in — so the decision is testable on any OS at any tier.
///
/// `can_spawn_subprocess` gates the whole check rather than
/// `capabilities.network.allow` being non-empty, and the asymmetry is deliberate: an empty
/// allowlist means *no* egress, and without the namespace to deliver that, the capsule would get
/// unrestricted egress instead. Refusing only capsules that asked for some network would let
/// exactly the capsules that asked for none run with the most.
pub fn check_egress_namespace(
    can_spawn_subprocess: bool,
    blocker: Option<EgressNamespaceBlocker>,
) -> Result<(), crate::errors::RuntimeError> {
    if !can_spawn_subprocess {
        return Ok(());
    }
    match blocker {
        None => Ok(()),
        Some(blocker) => Err(crate::errors::RuntimeError::EgressNamespaceUnavailable {
            blocker,
            reason: blocker.reason(),
        }),
    }
}

// ---------------------------------------------------------------- the plan

/// The fully-resolved, allocation-free description of the namespace the forked child will build.
///
/// Precomputed in the **parent**, before `fork()`, for the reason [`crate::sealed`] documents at
/// length: the `pre_exec` window permits only async-signal-safe work, and allocating there can
/// deadlock on an allocator lock a different thread of the parent held at `fork()` time. The
/// child reads this and issues syscalls; it computes nothing and allocates nothing.
#[derive(Debug, Clone)]
pub(crate) struct CapsuleNetnsPlan {
    /// TCP ports to bind and listen on inside the namespace, ascending — exactly
    /// `egress_proxy::egress_listen_ports` for this capsule's allowlist.
    ///
    /// May be empty, which is a capsule with no TCP egress at all. The UDP :53 socket is bound
    /// regardless, so even that capsule gets `REFUSED` *answers* to its lookups rather than the
    /// multi-second resolver stall a dropped packet produces.
    pub(crate) tcp_ports: Vec<u16>,
    /// The real uid/gid, read in the parent.
    ///
    /// Load-bearing ordering, exactly as in `sealed::probe_namespace`: inside a fresh user
    /// namespace with no map written yet, `getuid()` reports the overflow id (65534), and writing
    /// that into `uid_map` is refused on every host.
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl CapsuleNetnsPlan {
    /// Resolves the plan for one subprocess launch.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)]
    pub(crate) fn resolve(tcp_ports: Vec<u16>) -> Self {
        // SAFETY: `getuid`/`getgid` take no arguments, dereference nothing and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Self {
            tcp_ports,
            uid,
            gid,
        }
    }

    /// How many descriptors the child will send: one per TCP port, plus the resolver socket.
    pub(crate) fn socket_count(&self) -> usize {
        self.tcp_ports.len() + 1
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::{create_capsule_netns, receive_namespace_sockets};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::io;
    use std::os::fd::{FromRawFd, OwnedFd, RawFd};

    use super::{CapsuleNetnsPlan, EgressNamespaceProbe, EgressNamespaceSupport};

    // ------------------------------------------------------------ netlink constants
    //
    // Spelled out here rather than taken from `libc`, whose rtnetlink coverage varies by target
    // and libc flavour. These are uapi ABI constants — fixed for the lifetime of the interface —
    // and a wrong one fails loudly at the netlink ACK rather than silently.

    const RTM_NEWROUTE: u16 = 24;
    const NLM_F_REQUEST: u16 = 0x001;
    const NLM_F_ACK: u16 = 0x004;
    const NLM_F_EXCL: u16 = 0x200;
    const NLM_F_CREATE: u16 = 0x400;
    const NLMSG_ERROR: u16 = 2;
    /// Table 255, `local` — the table whose routes cause *input* delivery. A default route here
    /// is what makes every address locally deliverable.
    const RT_TABLE_LOCAL: u8 = 255;
    const RTPROT_BOOT: u8 = 3;
    const RT_SCOPE_HOST: u8 = 254;
    const RTN_LOCAL: u8 = 2;
    const RTA_OIF: u16 = 4;
    /// `lo` is always interface index 1 in a freshly created network namespace: it is the only
    /// interface there, and the kernel creates it first.
    const LOOPBACK_IFINDEX: u32 = 1;

    const NLMSGHDR_LEN: usize = 16;
    const RTMSG_LEN: usize = 12;
    const RTATTR_OIF_LEN: usize = 8;
    const ROUTE_REQUEST_LEN: usize = NLMSGHDR_LEN + RTMSG_LEN + RTATTR_OIF_LEN;

    // ------------------------------------------------------------ probe

    /// Rehearses, in a forked child, exactly what a real launch does: create the namespace, own
    /// it, bring `lo` up and install the local route.
    ///
    /// Forking rather than testing in-process is required, not defensive — `unshare(CLONE_NEWUSER)`
    /// is irreversible for the calling process, and putting the whole runtime into a user
    /// namespace as a side effect of a capability check is the "probe that changes the thing it
    /// measures" mistake `sealed::probe_namespace` documents avoiding.
    pub(super) fn probe_egress_namespace() -> EgressNamespaceSupport {
        EgressNamespaceSupport {
            apparmor_permits_userns: crate::sealed::apparmor_permits_userns(),
            namespace: probe_namespace(),
        }
    }

    // Exit codes the probe child reports, so the parent learns the failing *step* with no IPC
    // beyond `waitpid`. Same convention as `sealed::probe_namespace`.
    const PROBE_OK: i32 = 0;
    const PROBE_UNSHARE_DENIED: i32 = 1;
    const PROBE_UNSHARE_UNSUPPORTED: i32 = 2;
    const PROBE_MAP_DENIED: i32 = 3;
    const PROBE_CONFIG_DENIED: i32 = 4;

    fn probe_namespace() -> EgressNamespaceProbe {
        // Read before the fork, and therefore before `unshare`. See `CapsuleNetnsPlan::uid`.
        // SAFETY: `getuid`/`getgid` take no arguments and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

        // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the child
        // touches nothing but async-signal-safe primitives. The child below calls `unshare`,
        // `prctl`, the `open`/`write`/`close` triples that write the identity maps, `socket`,
        // `ioctl`, `sendto`/`recv` and `_exit` — every one async-signal-safe, with no allocation,
        // no locks and no stdio.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return EgressNamespaceProbe::Unsupported;
        }
        if pid == 0 {
            // SAFETY: forked-child context, syscalls only; every branch ends in `_exit`.
            unsafe {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                    let errno = *libc::__errno_location();
                    libc::_exit(match errno {
                        libc::EPERM => PROBE_UNSHARE_DENIED,
                        _ => PROBE_UNSHARE_UNSUPPORTED,
                    });
                }
                if write_identity_maps(uid, gid).is_err() {
                    libc::_exit(PROBE_MAP_DENIED);
                }
                if bring_loopback_up().is_err() || add_local_default_route().is_err() {
                    libc::_exit(PROBE_CONFIG_DENIED);
                }
                libc::_exit(PROBE_OK);
            }
        }

        let mut status: libc::c_int = 0;
        // SAFETY: `pid` is the child just forked; `status` is a live local.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited != pid || !libc::WIFEXITED(status) {
            return EgressNamespaceProbe::Unsupported;
        }
        match libc::WEXITSTATUS(status) {
            PROBE_OK => EgressNamespaceProbe::Ok,
            PROBE_UNSHARE_DENIED => EgressNamespaceProbe::Denied,
            PROBE_MAP_DENIED => EgressNamespaceProbe::MapDenied,
            PROBE_CONFIG_DENIED => EgressNamespaceProbe::ConfigDenied,
            _ => EgressNamespaceProbe::Unsupported,
        }
    }

    // ------------------------------------------------------------ child-side construction

    /// Builds the capsule's network namespace and hands its listening sockets to the parent.
    ///
    /// Runs in the forked child's `pre_exec` window, **before** the seccomp filter (which denies
    /// `unshare` and `socket(AF_NETLINK)`) and before `sealed`'s composed root. The ordering
    /// against `sealed` is load-bearing in a second way worth stating outright: `sealed` goes on
    /// to `unshare(CLONE_NEWUSER | CLONE_NEWNS)` a *nested* user namespace, and a process in a
    /// descendant user namespace holds no capabilities in the ancestor that owns this network
    /// namespace. A `sealed` capsule therefore cannot reconfigure the namespace confining it —
    /// verified by observing `SIOCSIFFLAGS` return `EPERM` from inside the nested namespace,
    /// rather than assumed from the capability rules.
    ///
    /// # Safety
    /// Post-`fork()`, pre-`execve` child context only. Every call below is async-signal-safe and
    /// nothing here allocates: `plan` was fully resolved in the parent.
    pub(crate) unsafe fn create_capsule_netns(
        plan: &CapsuleNetnsPlan,
        sock_fd: RawFd,
    ) -> io::Result<()> {
        // SAFETY: forked-child context; the caller's `unsafe` contract covers this whole body.
        unsafe {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return Err(io::Error::other(
                    "egress-netns: unshare(CLONE_NEWUSER | CLONE_NEWNET) failed",
                ));
            }
            write_identity_maps(plan.uid, plan.gid)
                .map_err(|()| io::Error::other("egress-netns: writing uid_map/gid_map failed"))?;
            bring_loopback_up()
                .map_err(|()| io::Error::other("egress-netns: bringing lo up failed"))?;
            add_local_default_route().map_err(|()| {
                io::Error::other("egress-netns: installing the local default route failed")
            })?;

            // One TCP listener per allowlisted port, then the resolver socket last — the order
            // the parent's `start_egress_proxy` splits them back apart in.
            let mut fds = [-1 as RawFd; super::MAX_NAMESPACE_SOCKETS];
            let mut count = 0usize;

            for port in &plan.tcp_ports {
                let fd = bind_tcp_listener(*port);
                if fd < 0 {
                    close_all(&fds[..count]);
                    return Err(io::Error::other(
                        "egress-netns: binding a TCP listener inside the namespace failed",
                    ));
                }
                fds[count] = fd;
                count += 1;
            }

            let resolver = bind_dns_socket();
            if resolver < 0 {
                close_all(&fds[..count]);
                return Err(io::Error::other(
                    "egress-netns: binding UDP 53 inside the namespace failed",
                ));
            }
            fds[count] = resolver;
            count += 1;

            let sent = send_fds(sock_fd, &fds[..count]);
            close_all(&fds[..count]);
            sent.map_err(|()| {
                io::Error::other("egress-netns: handing the namespace sockets to the runtime failed")
            })
        }
    }

    /// Writes `setgroups=deny` and the identity `uid_map`/`gid_map`, reopening the dumpable
    /// window around them.
    ///
    /// The dumpable dance is not optional and is `sealed`'s hardest-won lesson, reused here
    /// rather than rediscovered: `mur` marks itself non-dumpable at startup, the flag is inherited
    /// across `fork()`, and the kernel reassigns every `/proc/<pid>/*` entry of a non-dumpable
    /// task to root — including `uid_map`. Without this the namespace is created and can never be
    /// owned, and the failure reads as a host id-mapping policy problem on a host whose policy is
    /// perfectly fine.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only.
    unsafe fn write_identity_maps(uid: u32, gid: u32) -> Result<(), ()> {
        // SAFETY: caller's contract; every call below is async-signal-safe.
        unsafe {
            let previous = crate::sealed::make_dumpable_for_map_writes();
            let _ = crate::sealed::write_decimal_map(c"/proc/self/setgroups", None, 0);
            let mapped =
                crate::sealed::write_decimal_map(c"/proc/self/uid_map", Some(uid), uid).is_ok()
                    && crate::sealed::write_decimal_map(c"/proc/self/gid_map", Some(gid), gid)
                        .is_ok();
            crate::sealed::restore_dumpable(previous);
            if mapped {
                Ok(())
            } else {
                Err(())
            }
        }
    }

    /// `ip link set lo up`, via the legacy `ifreq` ioctl rather than netlink.
    ///
    /// The ioctl is a fixed-size struct with no message framing, so it is markedly simpler to do
    /// allocation-free inside `pre_exec` than the equivalent `RTM_NEWLINK`. It is not deprecated
    /// for this purpose, and every distribution's `ifconfig` still uses it.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only.
    unsafe fn bring_loopback_up() -> Result<(), ()> {
        // SAFETY: `ifr` is a live, zeroed `ifreq` whose name field is set to "lo"; both ioctls
        // read and write only within it, and the socket is closed on every path.
        unsafe {
            let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if sock < 0 {
                return Err(());
            }
            let mut ifr: libc::ifreq = std::mem::zeroed();
            ifr.ifr_name[0] = b'l' as libc::c_char;
            ifr.ifr_name[1] = b'o' as libc::c_char;
            if libc::ioctl(sock, libc::SIOCGIFFLAGS, &mut ifr) < 0 {
                libc::close(sock);
                return Err(());
            }
            ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            let rc = libc::ioctl(sock, libc::SIOCSIFFLAGS, &ifr);
            libc::close(sock);
            if rc < 0 {
                return Err(());
            }
            Ok(())
        }
    }

    /// `ip route add local default dev lo`, over raw netlink.
    ///
    /// A route of type `RTN_LOCAL` in table `local` tells the kernel that the destination is one
    /// of *its own* addresses, so a `connect()` to any address is looped back and delivered to a
    /// listening socket in this namespace — with the original destination preserved and
    /// recoverable by `getsockname(2)`. That is the whole interception mechanism: no netfilter, no
    /// `iptables` binary, and no rule set that can drift out of step with the manifest. A port
    /// nothing is bound to is refused immediately by the namespace instead of stalling through the
    /// kernel's full SYN-retry schedule against a blackhole.
    ///
    /// IPv4 only, deliberately. A capsule's IPv6 destinations have no route at all and fail with
    /// `ENETUNREACH`, and the proxy reaches every upstream over the host's own stack, so nothing
    /// a capsule can legitimately ask for depends on IPv6 inside the namespace.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only. The request is built in a fixed stack buffer.
    unsafe fn add_local_default_route() -> Result<(), ()> {
        // SAFETY: `request` and `address` are live, correctly-sized stack locals; the socket is
        // closed on every path.
        unsafe {
            let mut request = [0u8; ROUTE_REQUEST_LEN];
            request[0..4].copy_from_slice(&(ROUTE_REQUEST_LEN as u32).to_ne_bytes());
            request[4..6].copy_from_slice(&RTM_NEWROUTE.to_ne_bytes());
            request[6..8].copy_from_slice(
                &(NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL).to_ne_bytes(),
            );
            request[8..12].copy_from_slice(&1u32.to_ne_bytes()); // sequence
            request[12..16].copy_from_slice(&0u32.to_ne_bytes()); // pid: the kernel assigns

            let rtm = NLMSGHDR_LEN;
            request[rtm] = libc::AF_INET as u8;
            request[rtm + 1] = 0; // dst_len 0 — a default route
            request[rtm + 2] = 0; // src_len
            request[rtm + 3] = 0; // tos
            request[rtm + 4] = RT_TABLE_LOCAL;
            request[rtm + 5] = RTPROT_BOOT;
            request[rtm + 6] = RT_SCOPE_HOST;
            request[rtm + 7] = RTN_LOCAL;
            request[rtm + 8..rtm + 12].copy_from_slice(&0u32.to_ne_bytes()); // rtm_flags

            let attr = rtm + RTMSG_LEN;
            request[attr..attr + 2].copy_from_slice(&(RTATTR_OIF_LEN as u16).to_ne_bytes());
            request[attr + 2..attr + 4].copy_from_slice(&RTA_OIF.to_ne_bytes());
            request[attr + 4..attr + 8].copy_from_slice(&LOOPBACK_IFINDEX.to_ne_bytes());

            let sock = libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                libc::NETLINK_ROUTE,
            );
            if sock < 0 {
                return Err(());
            }
            let mut address: libc::sockaddr_nl = std::mem::zeroed();
            address.nl_family = libc::AF_NETLINK as u16;
            let sent = libc::sendto(
                sock,
                request.as_ptr().cast(),
                ROUTE_REQUEST_LEN,
                0,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            );
            if sent < 0 {
                libc::close(sock);
                return Err(());
            }

            // Reading the ACK is not optional: netlink reports the real outcome here, and a
            // request that was accepted for delivery but rejected by the kernel would otherwise
            // look like success and leave the capsule with a namespace it cannot resolve through.
            let mut response = [0u8; 256];
            let received = libc::recv(sock, response.as_mut_ptr().cast(), response.len(), 0);
            libc::close(sock);
            if received < (NLMSGHDR_LEN + 4) as isize {
                return Err(());
            }
            if u16::from_ne_bytes([response[4], response[5]]) == NLMSG_ERROR {
                let code = i32::from_ne_bytes([
                    response[NLMSGHDR_LEN],
                    response[NLMSGHDR_LEN + 1],
                    response[NLMSGHDR_LEN + 2],
                    response[NLMSGHDR_LEN + 3],
                ]);
                // Netlink spells a plain ACK as an error message carrying code 0.
                if code != 0 {
                    return Err(());
                }
            }
            Ok(())
        }
    }

    /// A listening TCP socket on `0.0.0.0:port` inside the namespace, or `-1`.
    ///
    /// The wildcard address is what pairs with the local default route: every address is locally
    /// deliverable, so this one socket receives connections addressed to *any* host on this port,
    /// and `getsockname(2)` on each accepted connection recovers which one the capsule dialled.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only.
    unsafe fn bind_tcp_listener(port: u16) -> RawFd {
        // SAFETY: `address` is a live, zeroed `sockaddr_in`; every failure path closes the fd.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return -1;
            }
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = port.to_be();
            address.sin_addr.s_addr = libc::INADDR_ANY.to_be();
            if libc::bind(
                fd,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) != 0
                || libc::listen(fd, 128) != 0
            {
                libc::close(fd);
                return -1;
            }
            fd
        }
    }

    /// A UDP socket bound to `0.0.0.0:53` inside the namespace with `IP_PKTINFO` enabled, or
    /// `-1`.
    ///
    /// The wildcard bind pairs with the local default route: whatever address `/etc/resolv.conf`
    /// names, the query is delivered to this one socket. `IP_PKTINFO` is then required rather
    /// than a nicety — it is how the proxy recovers the address the query was *sent to*, so its
    /// reply can carry that same address as the source. A resolver discards an answer that
    /// appears to come from a server it never asked, and the symptom of getting this wrong is
    /// "DNS silently does not work".
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only.
    unsafe fn bind_dns_socket() -> RawFd {
        // SAFETY: as `bind_tcp_listener`.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return -1;
            }
            let enable: libc::c_int = 1;
            if libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_PKTINFO,
                std::ptr::addr_of!(enable).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) != 0
            {
                libc::close(fd);
                return -1;
            }
            let mut address: libc::sockaddr_in = std::mem::zeroed();
            address.sin_family = libc::AF_INET as libc::sa_family_t;
            address.sin_port = super::DNS_PORT_BE;
            address.sin_addr.s_addr = libc::INADDR_ANY.to_be();
            if libc::bind(
                fd,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ) != 0
            {
                libc::close(fd);
                return -1;
            }
            fd
        }
    }

    /// # Safety
    /// Post-`fork()`, pre-exec child context only.
    unsafe fn close_all(fds: &[RawFd]) {
        for fd in fds {
            if *fd >= 0 {
                // SAFETY: each entry was returned by a successful `socket` call above.
                unsafe { libc::close(*fd) };
            }
        }
    }

    /// Enough room for [`super::MAX_NAMESPACE_SOCKETS`] descriptors plus the `cmsghdr`.
    const CMSG_BUFFER_LEN: usize = 128;

    // Not just a comment: `send_fds`/`receive_namespace_sockets` size `msg_controllen` from
    // `libc::CMSG_SPACE(descriptor_bytes)`, independently of `CMSG_BUFFER_LEN`, so nothing before
    // this diff stopped a future bump of `MAX_NAMESPACE_SOCKETS` (via
    // `egress_proxy::MAX_EGRESS_TCP_PORTS`) from silently making that computed length exceed the
    // fixed-size `CmsgBuffer` below — the CMSG macros would then write past its end. Pinned here
    // at compile time instead of trusted to stay true.
    const _: () = assert!(
        // SAFETY: `CMSG_SPACE` is pure arithmetic on its argument (see libc's definition); it is
        // marked `unsafe` only for macro-generation consistency with pointer-taking neighbours
        // like `CMSG_DATA`, not because this call touches memory.
        unsafe {
            libc::CMSG_SPACE((super::MAX_NAMESPACE_SOCKETS * std::mem::size_of::<RawFd>()) as u32)
                as usize
        } <= CMSG_BUFFER_LEN,
        "CMSG_BUFFER_LEN is too small to hold MAX_NAMESPACE_SOCKETS descriptors plus the cmsghdr; \
         raise it to match"
    );

    /// A `cmsghdr` buffer with the alignment the CMSG macros require.
    ///
    /// A bare `[u8; N]` is only 1-byte aligned and `CMSG_FIRSTHDR` casts it straight to
    /// `*mut cmsghdr`: on a debug build that is a misaligned-pointer panic, and on a release build
    /// it is undefined behaviour. Found the hard way while prototyping this hand-off.
    #[repr(C, align(8))]
    struct CmsgBuffer([u8; CMSG_BUFFER_LEN]);

    /// Sends every namespace socket to the parent in one `SCM_RIGHTS` message.
    ///
    /// One message rather than one per fd, so the parent's receive is a single unambiguous read:
    /// a partial hand-off would leave the proxy serving the endpoint but not the resolver, which
    /// presents as "DNS mysteriously does not work" rather than as a failure.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only; `sendmsg` is async-signal-safe and every
    /// buffer is a stack local.
    unsafe fn send_fds(sock_fd: RawFd, fds: &[RawFd]) -> Result<(), ()> {
        // SAFETY: all buffers are live locals outliving the single `sendmsg`; the control buffer
        // is 8-byte aligned, which the CMSG macros require.
        unsafe {
            let mut payload = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut control = CmsgBuffer([0u8; CMSG_BUFFER_LEN]);
            let bytes = std::mem::size_of_val(fds) as u32;

            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(bytes) as _;

            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(());
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                bytes as usize,
            );

            if libc::sendmsg(sock_fd, &msg, 0) < 0 {
                return Err(());
            }
            Ok(())
        }
    }

    /// Parent-side counterpart of [`send_fds`]: receives the namespace's listening sockets.
    ///
    /// Returns them in the order the child sent: one TCP listener per allowlisted port, then the
    /// resolver socket. A count that does not match the plan is an error rather than a
    /// best-effort start, because a proxy missing one of its sockets is a capsule whose network
    /// half-works in a way nothing downstream would report.
    pub(crate) fn receive_namespace_sockets(
        sock_fd: RawFd,
        expected: usize,
    ) -> io::Result<Vec<OwnedFd>> {
        // SAFETY: `sock_fd` is a valid connected unix socket; all buffers are live locals sized
        // for the largest message the child can send, and the control buffer is 8-byte aligned.
        unsafe {
            let mut payload = [0u8; 1];
            let mut iov = libc::iovec {
                iov_base: payload.as_mut_ptr().cast(),
                iov_len: payload.len(),
            };
            let mut control = CmsgBuffer([0u8; CMSG_BUFFER_LEN]);
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.0.as_mut_ptr().cast();
            msg.msg_controllen = control.0.len() as _;

            if libc::recvmsg(sock_fd, &mut msg, libc::MSG_CMSG_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null()
                || (*cmsg).cmsg_level != libc::SOL_SOCKET
                || (*cmsg).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(io::Error::other(
                    "egress-netns: the namespace sockets did not arrive as an SCM_RIGHTS message",
                ));
            }
            let bytes = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
            let received = bytes / std::mem::size_of::<RawFd>();
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            // Wrapped in `OwnedFd` before the count is checked, so a mismatched message closes
            // its descriptors on the error path instead of leaking them.
            let mut sockets = Vec::with_capacity(received);
            for index in 0..received {
                sockets.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                    data.add(index),
                )));
            }
            if received != expected {
                return Err(io::Error::other(
                    "egress-netns: the runtime received a different number of namespace sockets \
                     than the launch plan asked for",
                ));
            }
            Ok(sockets)
        }
    }
}

/// [`crate::egress_proxy::EGRESS_DNS_PORT`] in network byte order, so the `pre_exec` window does
/// no arithmetic the parent could have done for it.
#[cfg(target_os = "linux")]
const DNS_PORT_BE: u16 = crate::egress_proxy::EGRESS_DNS_PORT.to_be();

#[cfg(test)]
mod tests {
    use super::*;

    fn support(apparmor: bool, namespace: EgressNamespaceProbe) -> EgressNamespaceSupport {
        EgressNamespaceSupport {
            apparmor_permits_userns: apparmor,
            namespace,
        }
    }

    // ---- `egress_namespace_blocker` ------------------------------------------------------

    #[test]
    fn a_working_namespace_blocks_nothing() {
        assert_eq!(
            egress_namespace_blocker(true, support(true, EgressNamespaceProbe::Ok)),
            None
        );
    }

    #[test]
    fn apparmor_is_blamed_before_the_namespace_outcome() {
        // The `unshare` failure is a *consequence* of the restriction, so naming the kernel here
        // would send an Ubuntu desktop user to entirely the wrong fix.
        assert_eq!(
            egress_namespace_blocker(true, support(false, EgressNamespaceProbe::Unsupported)),
            Some(EgressNamespaceBlocker::CapabilityGrantMissing)
        );
    }

    #[test]
    fn every_refusal_of_a_supported_mechanism_is_a_capability_grant_problem() {
        for probe in [
            EgressNamespaceProbe::Denied,
            EgressNamespaceProbe::MapDenied,
            EgressNamespaceProbe::ConfigDenied,
        ] {
            assert_eq!(
                egress_namespace_blocker(true, support(true, probe)),
                Some(EgressNamespaceBlocker::CapabilityGrantMissing),
                "{probe:?}"
            );
        }
    }

    #[test]
    fn a_kernel_without_the_mechanism_is_named_separately() {
        assert_eq!(
            egress_namespace_blocker(true, support(true, EgressNamespaceProbe::Unsupported)),
            Some(EgressNamespaceBlocker::KernelSupportMissing)
        );
    }

    #[test]
    fn non_linux_is_never_refused_for_a_mechanism_it_never_had() {
        for probe in [
            EgressNamespaceProbe::Ok,
            EgressNamespaceProbe::Denied,
            EgressNamespaceProbe::Unsupported,
        ] {
            assert_eq!(egress_namespace_blocker(false, support(false, probe)), None);
        }
    }

    // ---- refusal text --------------------------------------------------------------------

    #[test]
    fn every_blocker_refuses_the_retired_fallback_in_so_many_words() {
        for blocker in EgressNamespaceBlocker::ALL {
            let reason = blocker.reason();
            assert!(
                reason.contains("will not fall back"),
                "{blocker:?} must state that the retired seccomp path is not a fallback"
            );
        }
    }

    #[test]
    fn the_capability_grant_refusal_names_both_remediations() {
        let reason = EgressNamespaceBlocker::CapabilityGrantMissing.reason();
        assert!(reason.contains("apparmor_parser"), "names the AppArmor fix");
        assert!(
            reason.contains("--cap-add SYS_ADMIN"),
            "names the container fix"
        );
    }

    #[test]
    fn the_kernel_refusal_names_the_sysctl_and_the_config_symbol() {
        let reason = EgressNamespaceBlocker::KernelSupportMissing.reason();
        assert!(reason.contains("user.max_user_namespaces"));
        assert!(reason.contains("CONFIG_USER_NS"));
    }

    // ---- the socket plan --------------------------------------------------------------------

    #[test]
    fn the_socket_count_is_one_per_port_plus_the_resolver() {
        let plan = CapsuleNetnsPlan {
            tcp_ports: vec![80, 443],
            uid: 1000,
            gid: 1000,
        };
        assert_eq!(plan.socket_count(), 3);
    }

    #[test]
    fn a_capsule_with_no_tcp_egress_still_binds_the_resolver() {
        // Load-bearing: with nothing bound on 53 a lookup would be a dropped packet, and the
        // capsule would stall through glibc's whole retry schedule instead of being told no.
        let plan = CapsuleNetnsPlan {
            tcp_ports: Vec::new(),
            uid: 1000,
            gid: 1000,
        };
        assert_eq!(plan.socket_count(), 1);
    }

    #[test]
    fn the_widest_allowlist_still_fits_one_scm_rights_message() {
        let plan = CapsuleNetnsPlan {
            tcp_ports: (0..crate::egress_proxy::MAX_EGRESS_TCP_PORTS as u16).collect(),
            uid: 1000,
            gid: 1000,
        };
        assert!(plan.socket_count() <= MAX_NAMESPACE_SOCKETS);
    }
}
