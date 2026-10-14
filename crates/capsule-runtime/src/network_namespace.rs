//! The capsule's own network namespace: the structural half of the mechanism that replaced the
//! seccomp-notify `connect`/`sendto` supervisor.
//!
//! [`crate::egress_proxy`] decides *which* destinations a native subprocess may reach. This module
//! is what makes that decision the only way out — it puts the subprocess tree somewhere with no
//! route off the host, so an unlisted destination is not "denied" by a filter that has to be
//! consulted, it is simply unreachable.
//!
//! ## One namespace per session
//!
//! A session has exactly one network namespace, [`SessionNetns`], created the first time one of
//! its commands is spawned and shared by every command after that. A server one shell call starts
//! on `127.0.0.1:8080` is therefore reachable from the session's next call, and from nothing
//! outside the session: two sessions are two `mur` processes, each with its own `SessionNetns`,
//! and no interface hands one across a process boundary. A plan's shell steps run in the
//! namespace of the session that submitted the plan.
//!
//! The namespace is built once by a dedicated *creator* child, forked for nothing else, which
//! hands the parent the namespace's listening sockets and a descriptor for each of its two
//! namespaces in one `SCM_RIGHTS` message and then exits. The namespace outlives the creator
//! because the runtime holds those descriptors. Every command then enters it with
//! [`join_session_netns`] — `setns(2)` into the user namespace, then into the network namespace —
//! in its own `pre_exec` window, where it is single-threaded and still unfiltered. The runtime's
//! own threads never call `setns` or `unshare`. The namespace is released when the session ends:
//! [`SessionNetns::end`] stops the session's egress proxy and closes the descriptors, after
//! `ShellEnforcement::end_session` has killed every process the session started.
//!
//! ## The syscall sequence
//!
//! Everything happens in the creator child, before any capsule code runs, and the join happens in
//! each command's forked child before the seccomp filter (which denies `unshare`, `setns` and
//! `socket(AF_NETLINK)` outright) and before [`crate::sealed`]'s composed root:
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
//!      to the parent in a single `SCM_RIGHTS` message, followed by the `/proc/self/ns/user` and
//!      `/proc/self/ns/net` descriptors later commands join through. A socket belongs to the
//!      namespace it was **created** in, not the one it is used from, which is the whole reason
//!      this inversion works: the runtime serves them while staying in the host's namespace, with
//!      real internet access for the upstream half. The listeners are held for the whole session,
//!      so a server a command starts on one of those ports gets `EADDRINUSE`.
//!
//! ## Why this shape rather than the obvious ones
//!
//! *No veth pair.* Wiring a namespace to the host with veth requires `CAP_NET_ADMIN` in the
//! **host's** network namespace, which means real root. This design needs no host privilege at
//! all, which is what lets `mur` install per-user.
//!
//! *No proxy process inside the namespace, and no holder process.* A multithreaded process cannot
//! enter a user namespace: `setns(user_fd, CLONE_NEWUSER)` from the runtime fails (`EINVAL`, or
//! `EPERM` from one of its threads). A freshly forked child is single-threaded, and the euid that
//! owns the session's user namespace holds every capability in it, so the same `setns` from a
//! command's `pre_exec` window succeeds, followed by `setns(net_fd, CLONE_NEWNET)`. Passing the
//! listening sockets *out* is what lets the proxy serve the namespace from the host's side with no
//! privilege at all.
//!
//! A long-lived single-threaded holder that forked each command on request was the alternative.
//! It would make every command its child rather than the runtime's, so every `Child` the shell
//! path keys on would need a proxy layer, and it would re-implement `std::process::Command`'s spawn
//! over a request protocol. Joining in `pre_exec` changes one step of the existing spawn instead.
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

// The probe outcome and the namespace plan stay defined everywhere so the decisions over them are
// unit-tested on every OS, but outside tests only Linux code builds or reads them, so off Linux
// they are dead by construction.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

/// One TCP listener per allowlisted port, plus the single wildcard UDP :53 resolver socket.
pub(crate) const MAX_NAMESPACE_SOCKETS: usize = crate::egress_proxy::MAX_EGRESS_TCP_PORTS + 1;

/// Every descriptor the creator child sends: the namespace sockets, then the user and network
/// namespace handles.
///
/// Bounds the one `SCM_RIGHTS` message, and therefore the fixed stack buffers the creator builds
/// it in.
pub(crate) const MAX_SESSION_NETNS_FDS: usize = MAX_NAMESPACE_SOCKETS + 2;

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
                 the mur installer as root), then re-run. Building out of a checkout, where the \
                 binary sits at ./target/{{debug,release}}/mur and no shipped profile attaches to \
                 it: run `scripts/install-dev-apparmor.sh`, which generates and loads the same \
                 grant for those two paths. Inside a container it is a missing capability \
                 instead: add `--cap-add SYS_ADMIN` to the container invocation, or create the \
                 network namespace outside the container and run mur inside it. LAST RESORT, only \
                 where a profile genuinely cannot be loaded: `sudo sysctl -w \
                 kernel.apparmor_restrict_unprivileged_userns=0` — this removes \
                 unprivileged-userns hardening from every program on the machine, not just from \
                 mur, and is not the configuration murmur ships.",
                name = crate::sealed::SEALED_APPARMOR_PROFILE_NAME,
                path = crate::sealed::SEALED_APPARMOR_PROFILE_PATH,
            ),
            EgressNamespaceBlocker::KernelSupportMissing => {
                "this kernel does not provide unprivileged user namespaces, which a capsule's \
                 network namespace has to be created inside (CONFIG_USER_NS=n, or \
                 user.max_user_namespaces=0). Raise `sudo sysctl -w \
                 user.max_user_namespaces=10000` if the sysctl is merely zeroed, otherwise run on \
                 a kernel built with CONFIG_USER_NS=y."
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
    /// Where this host's permission for an unprivileged user namespace comes from.
    ///
    /// Answered by `sealed`'s own probe of exactly the same question: the restriction is on
    /// `CLONE_NEWUSER`, which both mechanisms need, so asking it twice with two implementations
    /// could only ever produce two answers that disagree about one host.
    pub(crate) userns_grant: crate::sealed::UsernsGrant,
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
    if !support.userns_grant.permits_userns() {
        return Some(EgressNamespaceBlocker::CapabilityGrantMissing);
    }
    match support.namespace {
        EgressNamespaceProbe::Ok => None,
        EgressNamespaceProbe::Denied
        | EgressNamespaceProbe::MapDenied
        | EgressNamespaceProbe::ConfigDenied => {
            Some(EgressNamespaceBlocker::CapabilityGrantMissing)
        }
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
            crate::runtime_err!("[SKIP-HOST] {test_name}: {}", blocker.reason());
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
    /// Resolves the plan for one session's namespace.
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

// ---------------------------------------------------------------- the session's namespace

/// One session's network namespace, created on first use and shared by every command after it.
///
/// Always held as `Arc<SessionNetns>` on [`crate::sandbox::ShellEnforcement`], so every clone of a
/// session's enforcement — each tool call's, each demoted command's, a plan's — reaches the same
/// namespace. Its state moves one way: not yet created, established, ended. An ended namespace is
/// never recreated, so a command spawned after its session ended is refused rather than given a
/// fresh namespace nobody will tear down.
///
/// Off Linux there is no namespace to create: the type exists so the session-end path needs no
/// `cfg`, and it only ever moves from not yet created to ended.
#[derive(Debug, Default)]
pub(crate) struct SessionNetns {
    state: std::sync::Mutex<SessionNetnsState>,
    /// Successful creations, which a test compares against the commands it ran.
    #[cfg(test)]
    creations: std::sync::atomic::AtomicUsize,
}

#[derive(Debug, Default)]
enum SessionNetnsState {
    #[default]
    NotYetCreated,
    #[cfg(target_os = "linux")]
    Established(linux::EstablishedNetns),
    Ended,
}

/// What a command's `pre_exec` window needs to join its session's namespace: the user and network
/// namespace descriptors.
///
/// Shares the session's descriptors rather than duplicating them, so they stay open for as long as
/// the `Command` holding this does, even if the session ends between `establish` and the spawn: a
/// join then enters a namespace whose processes are being killed, never a reused descriptor number.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub(crate) struct SessionNetnsJoin {
    user_ns: std::sync::Arc<std::os::fd::OwnedFd>,
    net_ns: std::sync::Arc<std::os::fd::OwnedFd>,
}

#[cfg(target_os = "linux")]
impl SessionNetnsJoin {
    /// `(user_ns, net_ns)`, for [`join_session_netns`].
    pub(crate) fn raw_fds(&self) -> (std::os::fd::RawFd, std::os::fd::RawFd) {
        use std::os::fd::AsRawFd;
        (self.user_ns.as_raw_fd(), self.net_ns.as_raw_fd())
    }
}

impl SessionNetns {
    /// A session whose namespace does not exist yet. Creates nothing.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The handles a command joins the session's namespace through, creating the namespace and
    /// starting the session's egress proxy on the first call.
    ///
    /// Serialised on one mutex, so concurrent first commands create exactly one namespace. A
    /// failed creation leaves the namespace not yet created and leaks nothing, so that command's
    /// spawn fails and the next one tries again. `tcp_ports` and `policy` are read on the call
    /// that creates the namespace; every later call is handed the namespace that call created.
    #[cfg(target_os = "linux")]
    pub(crate) fn establish(
        &self,
        tcp_ports: &[u16],
        policy: crate::egress_proxy::EgressPolicy,
    ) -> Result<SessionNetnsJoin, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            SessionNetnsState::Established(established) => return Ok(established.join()),
            SessionNetnsState::Ended => {
                return Err(
                    "sandbox: this session has ended, and its network namespace was released \
                     with it; no further command runs in it"
                        .to_string(),
                )
            }
            SessionNetnsState::NotYetCreated => {}
        }
        let plan = CapsuleNetnsPlan::resolve(tcp_ports.to_vec());
        let established = linux::create_session_netns(&plan)
            .and_then(|created| created.serve(policy))
            .map_err(|detail| {
                format!("sandbox: could not create this session's network namespace: {detail}")
            })?;
        let join = established.join();
        *state = SessionNetnsState::Established(established);
        #[cfg(test)]
        self.creations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(join)
    }

    /// Releases the namespace: stops the session's egress proxy and closes the namespace
    /// descriptors. Idempotent, and final — [`Self::establish`] refuses every call after it.
    ///
    /// The namespace itself is gone once its last process is: the caller kills the session's
    /// processes first (`ShellEnforcement::end_session`).
    pub(crate) fn end(&self) {
        let previous = std::mem::replace(
            &mut *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            SessionNetnsState::Ended,
        );
        #[cfg(target_os = "linux")]
        if let SessionNetnsState::Established(established) = previous {
            established.release();
        }
        #[cfg(not(target_os = "linux"))]
        drop(previous);
    }

    /// How many namespaces this session has created: `0` before its first command, `1` after.
    #[cfg(test)]
    pub(crate) fn creations(&self) -> usize {
        self.creations.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for SessionNetns {
    /// The backstop for a session that never reached `end_session`: the proxy stops and the
    /// descriptors close when the last enforcement holding this goes.
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::join_session_netns;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;

    use super::{
        CapsuleNetnsPlan, EgressNamespaceProbe, EgressNamespaceSupport, SessionNetnsJoin,
        MAX_NAMESPACE_SOCKETS, MAX_SESSION_NETNS_FDS,
    };

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
            userns_grant: crate::sealed::userns_grant(),
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

    // ------------------------------------------------------------ the creator child

    /// The step of the namespace's construction that failed, as the creator reports it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum CreationStep {
        Unshare = 1,
        IdentityMaps = 2,
        Loopback = 3,
        LocalRoute = 4,
        TcpListener = 5,
        Resolver = 6,
        NamespaceHandles = 7,
        HandOff = 8,
    }

    impl CreationStep {
        fn from_code(code: u32) -> Option<Self> {
            Some(match code {
                1 => Self::Unshare,
                2 => Self::IdentityMaps,
                3 => Self::Loopback,
                4 => Self::LocalRoute,
                5 => Self::TcpListener,
                6 => Self::Resolver,
                7 => Self::NamespaceHandles,
                8 => Self::HandOff,
                _ => return None,
            })
        }

        fn describe(self) -> &'static str {
            match self {
                Self::Unshare => "unshare(CLONE_NEWUSER | CLONE_NEWNET)",
                Self::IdentityMaps => "writing uid_map/gid_map",
                Self::Loopback => "bringing lo up",
                Self::LocalRoute => "installing the local default route",
                Self::TcpListener => "binding a TCP listener inside the namespace",
                Self::Resolver => "binding UDP 53 inside the namespace",
                Self::NamespaceHandles => "opening /proc/self/ns/user and /proc/self/ns/net",
                Self::HandOff => "handing the namespace to the runtime",
            }
        }
    }

    /// A failed step and the `errno` it left, carried out of the creator as a fixed-size record.
    ///
    /// `Copy` and built from integers only, so the creator reports a failure without allocating.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct CreationFailure {
        step: CreationStep,
        errno: i32,
    }

    /// Bytes in an encoded [`CreationFailure`]: the step's code, then the `errno`.
    const FAILURE_RECORD_LEN: usize = 8;

    impl CreationFailure {
        /// The failure of `step`, with the `errno` the failing call just left.
        ///
        /// # Safety
        /// Reads the calling thread's `errno`; async-signal-safe.
        unsafe fn at(step: CreationStep) -> Self {
            // SAFETY: `__errno_location` returns the calling thread's own errno slot, which is
            // always valid to read.
            let errno = unsafe { *libc::__errno_location() };
            Self { step, errno }
        }

        fn encode(self) -> [u8; FAILURE_RECORD_LEN] {
            let mut record = [0u8; FAILURE_RECORD_LEN];
            record[..4].copy_from_slice(&(self.step as u32).to_ne_bytes());
            record[4..].copy_from_slice(&self.errno.to_ne_bytes());
            record
        }

        fn decode(record: &[u8; FAILURE_RECORD_LEN]) -> Option<Self> {
            let step = CreationStep::from_code(u32::from_ne_bytes([
                record[0], record[1], record[2], record[3],
            ]))?;
            let errno = i32::from_ne_bytes([record[4], record[5], record[6], record[7]]);
            Some(Self { step, errno })
        }
    }

    impl std::fmt::Display for CreationFailure {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{} failed: {}",
                self.step.describe(),
                io::Error::from_raw_os_error(self.errno)
            )
        }
    }

    /// Builds the session's network namespace and hands it to the parent: its listening sockets,
    /// then descriptors for its user and network namespaces, in one `SCM_RIGHTS` message ordered
    /// `[tcp listeners…, udp53, user_ns, net_ns]`.
    ///
    /// Runs only in the creator child [`create_session_netns`] forks, which runs no capsule code,
    /// installs no seccomp filter or Landlock domain and joins no cgroup scope; it `_exit`s as
    /// soon as this returns. Commands join what this builds through [`join_session_netns`]. A
    /// `sealed` command goes on to `unshare(CLONE_NEWUSER | CLONE_NEWNS)` a *nested* user
    /// namespace, and a process in a descendant user namespace holds no capabilities in the
    /// ancestor that owns this network namespace, so a `sealed` capsule cannot reconfigure the
    /// namespace confining it: `SIOCSIFFLAGS` returns `EPERM` from inside the nested namespace.
    ///
    /// # Safety
    /// Post-`fork()` child context only. Every call below is async-signal-safe and nothing here
    /// allocates: `plan` was fully resolved in the parent, and a failure is a `Copy` record.
    unsafe fn create_capsule_netns(
        plan: &CapsuleNetnsPlan,
        sock_fd: RawFd,
    ) -> Result<(), CreationFailure> {
        // SAFETY: forked-child context; the caller's `unsafe` contract covers this whole body.
        unsafe {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
                return Err(CreationFailure::at(CreationStep::Unshare));
            }
            if write_identity_maps(plan.uid, plan.gid).is_err() {
                return Err(CreationFailure::at(CreationStep::IdentityMaps));
            }
            if bring_loopback_up().is_err() {
                return Err(CreationFailure::at(CreationStep::Loopback));
            }
            if add_local_default_route().is_err() {
                return Err(CreationFailure::at(CreationStep::LocalRoute));
            }

            // One TCP listener per allowlisted port, then the resolver socket — the order the
            // parent's `start_egress_proxy` splits them back apart in — then the two namespace
            // handles, which the parent takes off the end.
            let mut fds = [-1 as RawFd; MAX_SESSION_NETNS_FDS];
            let mut count = 0usize;

            for port in &plan.tcp_ports {
                let fd = bind_tcp_listener(*port);
                if fd < 0 {
                    let failure = CreationFailure::at(CreationStep::TcpListener);
                    close_all(&fds[..count]);
                    return Err(failure);
                }
                fds[count] = fd;
                count += 1;
            }

            let resolver = bind_dns_socket();
            if resolver < 0 {
                let failure = CreationFailure::at(CreationStep::Resolver);
                close_all(&fds[..count]);
                return Err(failure);
            }
            fds[count] = resolver;
            count += 1;

            // Opened after the maps are written, so both name the namespaces this child now owns.
            for namespace in [c"/proc/self/ns/user", c"/proc/self/ns/net"] {
                let fd = libc::open(namespace.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    let failure = CreationFailure::at(CreationStep::NamespaceHandles);
                    close_all(&fds[..count]);
                    return Err(failure);
                }
                fds[count] = fd;
                count += 1;
            }

            let sent = send_fds(sock_fd, &fds[..count]);
            let failure = CreationFailure::at(CreationStep::HandOff);
            close_all(&fds[..count]);
            sent.map_err(|()| failure)
        }
    }

    /// Enters the session's namespaces: `setns(user_fd, CLONE_NEWUSER)`, then
    /// `setns(net_fd, CLONE_NEWNET)`. Returns the `errno` of whichever failed.
    ///
    /// The order is required. Joining a network namespace needs `CAP_SYS_ADMIN` in the user
    /// namespace that owns it, which the caller holds only once it is inside that user namespace;
    /// joining the user namespace needs the caller to be single-threaded and its euid to own the
    /// namespace, which a command's forked child is and does.
    ///
    /// # Safety
    /// Post-`fork()`, pre-exec child context only. Both descriptors must be open; the
    /// [`SessionNetnsJoin`] they came from keeps them so.
    pub(crate) unsafe fn join_session_netns(user_fd: RawFd, net_fd: RawFd) -> io::Result<()> {
        // SAFETY: `setns` takes two integers and dereferences nothing; `last_os_error` reads
        // errno and does not allocate.
        unsafe {
            if libc::setns(user_fd, libc::CLONE_NEWUSER) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setns(net_fd, libc::CLONE_NEWNET) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// What the creator handed over: the namespace sockets, ready for the egress proxy, and the
    /// two namespace handles.
    pub(super) struct CreatedNetns {
        sockets: Vec<OwnedFd>,
        user_ns: OwnedFd,
        net_ns: OwnedFd,
    }

    impl CreatedNetns {
        /// Starts the session's egress proxy on the namespace sockets.
        ///
        /// On failure the sockets and the namespace handles close here, which leaves no process
        /// and no descriptor holding the namespace.
        pub(super) fn serve(
            self,
            policy: crate::egress_proxy::EgressPolicy,
        ) -> Result<EstablishedNetns, String> {
            let proxy = crate::egress_proxy::start_egress_proxy(self.sockets, policy)
                .map_err(|error| format!("starting the session's egress proxy failed: {error}"))?;
            Ok(EstablishedNetns {
                user_ns: Arc::new(self.user_ns),
                net_ns: Arc::new(self.net_ns),
                proxy,
            })
        }
    }

    /// An established session namespace: the two handles commands join through, and the proxy
    /// serving its listeners.
    #[derive(Debug)]
    pub(super) struct EstablishedNetns {
        user_ns: Arc<OwnedFd>,
        net_ns: Arc<OwnedFd>,
        proxy: crate::egress_proxy::EgressProxyHandle,
    }

    impl EstablishedNetns {
        pub(super) fn join(&self) -> SessionNetnsJoin {
            SessionNetnsJoin {
                user_ns: Arc::clone(&self.user_ns),
                net_ns: Arc::clone(&self.net_ns),
            }
        }

        /// Stops the proxy and drops this session's hold on the namespace handles.
        pub(super) fn release(self) {
            self.proxy.shutdown();
        }
    }

    /// Forks the creator child, waits for it, and takes what it handed over.
    ///
    /// The creator is a child of its own rather than a command's `pre_exec` step, so no command is
    /// special: a command that fails cannot take the session's namespace with it, and creation
    /// never depends on what any command is.
    pub(super) fn create_session_netns(plan: &CapsuleNetnsPlan) -> Result<CreatedNetns, String> {
        // Checked here because the creator indexes a fixed buffer of this size and must not be the
        // place an oversized plan is discovered.
        if plan.socket_count() > MAX_NAMESPACE_SOCKETS {
            return Err(format!(
                "{} namespace sockets requested, more than the {MAX_NAMESPACE_SOCKETS} one \
                 hand-off carries",
                plan.socket_count()
            ));
        }

        let mut pair = [0i32; 2];
        // SAFETY: `pair` is a 2-element stack array, exactly the size `socketpair` writes into.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(format!(
                "socketpair() for the namespace hand-off failed: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: both fds were just returned by the successful `socketpair` above and nothing
        // else owns them.
        let (parent_end, creator_end) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
        let creator_fd = creator_end.as_raw_fd();

        // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the child
        // touches nothing but async-signal-safe primitives. The child runs only
        // `create_capsule_netns` — `unshare`, `prctl`, `open`/`write`/`close`, `socket`, `ioctl`,
        // `bind`, `listen`, `setsockopt`, `sendto`/`recv`/`sendmsg` — then `send` and `_exit`,
        // with no allocation, no locks and no stdio.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!(
                "fork() for the namespace creator failed: {}",
                io::Error::last_os_error()
            ));
        }
        if pid == 0 {
            // SAFETY: forked-child context, syscalls only; both branches end in `_exit`, so no
            // destructor of the parent's state runs here.
            unsafe {
                match create_capsule_netns(plan, creator_fd) {
                    Ok(()) => libc::_exit(0),
                    Err(failure) => {
                        let record = failure.encode();
                        libc::send(creator_fd, record.as_ptr().cast(), record.len(), 0);
                        libc::_exit(1);
                    }
                }
            }
        }
        drop(creator_end);

        let status = wait_for_creator(pid)?;
        match receive_session_netns(parent_end.as_raw_fd(), plan.socket_count())? {
            Received::Handles(mut fds) => {
                let net_ns = fds.pop().expect("the count was checked on receipt");
                let user_ns = fds.pop().expect("the count was checked on receipt");
                Ok(CreatedNetns {
                    sockets: fds,
                    user_ns,
                    net_ns,
                })
            }
            Received::Failure(failure) => Err(failure.to_string()),
            Received::Nothing => Err(format!(
                "the namespace creator exited without handing anything over ({status})"
            )),
        }
    }

    /// Reaps the creator. The hand-off is read only afterwards: a `SOCK_DGRAM` pair reports no
    /// end-of-file when its peer closes, so reading first would block forever on a creator that
    /// died before sending, while the message a creator did send stays queued after it exits.
    fn wait_for_creator(pid: libc::pid_t) -> Result<String, String> {
        let mut status: libc::c_int = 0;
        loop {
            // SAFETY: `pid` is the creator just forked; `status` is a live local.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited == pid {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(format!("waiting for the namespace creator failed: {error}"));
            }
        }
        Ok(if libc::WIFEXITED(status) {
            format!("exit status {}", libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            format!("killed by signal {}", libc::WTERMSIG(status))
        } else {
            format!("wait status {status}")
        })
    }

    /// Encodes and decodes a failure record as the creator and the parent do, rendered as the
    /// parent renders it.
    #[cfg(test)]
    pub(super) fn describe_failure_record(step: u32, errno: i32) -> Option<String> {
        let mut record = [0u8; FAILURE_RECORD_LEN];
        record[..4].copy_from_slice(&step.to_ne_bytes());
        record[4..].copy_from_slice(&errno.to_ne_bytes());
        CreationFailure::decode(&record).map(|failure| {
            assert_eq!(failure.encode(), record);
            failure.to_string()
        })
    }

    /// The creator's one message.
    enum Received {
        /// `[tcp listeners…, udp53, user_ns, net_ns]`, every one `CLOEXEC`.
        Handles(Vec<OwnedFd>),
        Failure(CreationFailure),
        Nothing,
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
            let mapped = crate::sealed::write_decimal_map(c"/proc/self/uid_map", Some(uid), uid)
                .is_ok()
                && crate::sealed::write_decimal_map(c"/proc/self/gid_map", Some(gid), gid).is_ok();
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
                // SAFETY: each entry was returned by a successful `socket` or `open` call above.
                unsafe { libc::close(*fd) };
            }
        }
    }

    /// Enough room for [`MAX_SESSION_NETNS_FDS`] descriptors plus the `cmsghdr`.
    const CMSG_BUFFER_LEN: usize = 128;

    // `send_fds`/`receive_session_netns` size `msg_controllen` from
    // `libc::CMSG_SPACE(descriptor_bytes)`, independently of `CMSG_BUFFER_LEN`, so a bump of
    // `MAX_SESSION_NETNS_FDS` (via `egress_proxy::MAX_EGRESS_TCP_PORTS`) could otherwise make that
    // computed length exceed the fixed-size `CmsgBuffer` below, and the CMSG macros would then
    // write past its end. Pinned at compile time instead.
    const _: () = assert!(
        // SAFETY: `CMSG_SPACE` is pure arithmetic on its argument (see libc's definition); it is
        // marked `unsafe` only for macro-generation consistency with pointer-taking neighbours
        // like `CMSG_DATA`, not because this call touches memory.
        unsafe {
            libc::CMSG_SPACE((MAX_SESSION_NETNS_FDS * std::mem::size_of::<RawFd>()) as u32) as usize
        } <= CMSG_BUFFER_LEN,
        "CMSG_BUFFER_LEN is too small to hold MAX_SESSION_NETNS_FDS descriptors plus the \
         cmsghdr; raise it to match"
    );

    /// A `cmsghdr` buffer with the alignment the CMSG macros require.
    ///
    /// A bare `[u8; N]` is only 1-byte aligned and `CMSG_FIRSTHDR` casts it straight to
    /// `*mut cmsghdr`: on a debug build that is a misaligned-pointer panic, and on a release build
    /// it is undefined behaviour. Found the hard way while prototyping this hand-off.
    #[repr(C, align(8))]
    struct CmsgBuffer([u8; CMSG_BUFFER_LEN]);

    /// Sends every namespace socket and both namespace handles to the parent in one
    /// `SCM_RIGHTS` message.
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

    /// Parent-side counterpart of [`send_fds`]: takes the creator's one message off the queue.
    ///
    /// Non-blocking, because the creator has already been reaped by the time this runs: whatever
    /// it sent is queued, and an empty queue means it sent nothing. A descriptor count other than
    /// `expected_sockets + 2` is an error rather than a best-effort start, because a proxy missing
    /// one of its sockets is a capsule whose network half-works in a way nothing downstream would
    /// report.
    fn receive_session_netns(sock_fd: RawFd, expected_sockets: usize) -> Result<Received, String> {
        // SAFETY: `sock_fd` is a valid unix socket; all buffers are live locals sized for the
        // largest message the creator can send, and the control buffer is 8-byte aligned.
        unsafe {
            let mut payload = [0u8; FAILURE_RECORD_LEN];
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

            let received = libc::recvmsg(
                sock_fd,
                &mut msg,
                libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
            );
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(Received::Nothing);
                }
                return Err(format!("receiving the namespace hand-off failed: {error}"));
            }

            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                if received as usize == FAILURE_RECORD_LEN {
                    if let Some(failure) = CreationFailure::decode(&payload) {
                        return Ok(Received::Failure(failure));
                    }
                }
                return Ok(Received::Nothing);
            }
            if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
                return Err(
                    "the namespace hand-off did not arrive as an SCM_RIGHTS message".to_string(),
                );
            }
            let bytes = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
            let count = bytes / std::mem::size_of::<RawFd>();
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            // Wrapped in `OwnedFd` before anything is checked, so a mismatched or truncated
            // message closes its descriptors on the error path instead of leaking them.
            let mut fds = Vec::with_capacity(count);
            for index in 0..count {
                fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(
                    data.add(index),
                )));
            }
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                return Err("the namespace hand-off was truncated".to_string());
            }
            if count != expected_sockets + 2 {
                return Err(format!(
                    "the namespace creator handed over {count} descriptors; {} were expected \
                     ({expected_sockets} sockets and 2 namespace handles)",
                    expected_sockets + 2
                ));
            }
            Ok(Received::Handles(fds))
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
            userns_grant: if apparmor {
                crate::sealed::UsernsGrant::ProfileConfining
            } else {
                crate::sealed::UsernsGrant::Withheld
            },
            namespace,
        }
    }

    /// The blocker decision reads only `UsernsGrant::permits_userns()`. Every grant that permits
    /// the namespace must therefore reach the same verdict — naming the provenance reports a host
    /// fact and never changes one.
    #[test]
    fn every_permitting_userns_grant_reaches_the_same_verdict() {
        for grant in crate::sealed::UsernsGrant::ALL {
            let support = EgressNamespaceSupport {
                userns_grant: *grant,
                namespace: EgressNamespaceProbe::Ok,
            };
            let expected = if grant.permits_userns() {
                None
            } else {
                Some(EgressNamespaceBlocker::CapabilityGrantMissing)
            };
            assert_eq!(
                egress_namespace_blocker(true, support),
                expected,
                "grant {grant:?} must decide only through permits_userns()"
            );
        }
    }

    /// Both refusal texts put the profile first and label the sysctl as what it costs. The sysctl
    /// is a legitimate answer on a host where no profile can be loaded, so the text still carries
    /// it — but a reader must not be able to take it for a peer alternative to the profile.
    #[test]
    fn the_capability_grant_reason_offers_the_profile_before_the_sysctl() {
        let reason = EgressNamespaceBlocker::CapabilityGrantMissing.reason();
        let profile = reason
            .find("apparmor_parser -r")
            .expect("the profile install command must be named");
        let sysctl = reason
            .find("kernel.apparmor_restrict_unprivileged_userns=0")
            .expect("the fallback must still be named for hosts that cannot load a profile");
        assert!(
            profile < sysctl,
            "the profile install must come first: {reason}"
        );
        assert!(
            reason.contains("LAST RESORT"),
            "the sysctl must be labelled a fallback, not offered as a peer: {reason}"
        );
        assert!(
            reason.contains("every program on the machine"),
            "the sysctl's host-wide cost must be stated: {reason}"
        );
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
        // The two namespace handles ride in the same message.
        assert_eq!(MAX_SESSION_NETNS_FDS, MAX_NAMESPACE_SOCKETS + 2);
    }

    // ---- the session's namespace ---------------------------------------------------------

    /// An ended session is never given a namespace, including one that never created one.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_ended_session_refuses_to_create_a_namespace() {
        let netns = SessionNetns::new();
        netns.end();
        let refusal = netns
            .establish(
                &[],
                crate::egress_proxy::EgressPolicy::new(Vec::new(), Vec::new()),
            )
            .expect_err("an ended session creates nothing");
        assert!(refusal.contains("session has ended"), "{refusal}");
        assert_eq!(netns.creations(), 0);
    }

    /// One creation serves every later call, and the namespace it made is the one each call
    /// is handed; ending it is final and repeatable.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_creates_its_namespace_once_and_ends_it_for_good() {
        if skip_without_egress_namespace(
            "a_session_creates_its_namespace_once_and_ends_it_for_good",
        ) {
            return;
        }
        let policy = || crate::egress_proxy::EgressPolicy::new(Vec::new(), Vec::new());
        let netns = SessionNetns::new();
        assert_eq!(
            netns.creations(),
            0,
            "nothing is created before it is needed"
        );

        let first = netns
            .establish(&[], policy())
            .expect("the namespace is created");
        let second = netns
            .establish(&[], policy())
            .expect("the namespace is reused");
        assert_eq!(netns.creations(), 1);
        let inode = |fd: std::os::fd::RawFd| {
            std::fs::metadata(format!("/proc/self/fd/{fd}"))
                .map(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata))
                .unwrap()
        };
        assert_eq!(inode(first.raw_fds().1), inode(second.raw_fds().1));
        let host_netns = std::fs::metadata("/proc/self/ns/net")
            .map(|metadata| std::os::unix::fs::MetadataExt::ino(&metadata))
            .unwrap();
        assert_ne!(
            inode(first.raw_fds().1),
            host_netns,
            "the session's namespace is not the runtime's own"
        );

        netns.end();
        netns.end();
        assert!(netns.establish(&[], policy()).is_err());
        assert_eq!(
            netns.creations(),
            1,
            "an ended namespace is never recreated"
        );
    }

    /// The creator's failure record survives the round trip, so the parent names the step and
    /// the errno rather than a bare exit status.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_creation_failure_names_its_step_and_errno() {
        let text = linux::describe_failure_record(4, libc::EPERM).expect("a known step decodes");
        assert!(
            text.starts_with("installing the local default route failed:"),
            "{text}"
        );
        assert!(text.contains("Operation not permitted"), "{text}");
        assert_eq!(linux::describe_failure_record(99, 0), None);
    }
}
