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
//! ## Whether a host can create one
//!
//! Answered by creating one: [`crate::namespace_probe`] runs the same creator a session does, with
//! an empty allowlist, and drops what it hands over. A staged session's staging and launch read
//! the answer off that session's one [`crate::sandbox::HostProbe`], and a session's first command
//! gets the same refusal from its own creation ([`SessionNetns::establish`]). AppArmor's unprivileged-userns restriction and the
//! profile confining `mur` are read only to explain a failure: a container whose own profile
//! permits the namespace is not refused because the host's restriction reads `1`.
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
//!      This is the same primitive `sealed` uses, and AppArmor's unprivileged-userns
//!      restriction can refuse it the same way.
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

// The creation steps and the namespace plan stay defined everywhere so the decisions over them are
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

// ---------------------------------------------------------------- the creation's steps

/// One step of the namespace's construction, as the creator reports it when that step fails.
///
/// Defined outside the Linux module so [`egress_namespace_failure`], the decision over a failed
/// step, compiles and is tested on every OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreationStep {
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
    const ALL: [CreationStep; 8] = [
        Self::Unshare,
        Self::IdentityMaps,
        Self::Loopback,
        Self::LocalRoute,
        Self::TcpListener,
        Self::Resolver,
        Self::NamespaceHandles,
        Self::HandOff,
    ];

    /// The step a failure record's code names, or `None` for a code no step carries.
    pub(crate) fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|step| *step as u32 == code)
    }

    /// The call or operation, as a refusal names it.
    #[must_use]
    pub fn describe(self) -> &'static str {
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

/// Why an attempt to create a session's network namespace produced no namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamespaceAttemptError {
    /// The creator ran, and `step` failed with `errno`.
    Step { step: CreationStep, errno: i32 },
    /// The creator could not run, or exited without handing anything over; the runtime's own
    /// account of it (`fork() for the namespace creator failed: …`).
    Creator(String),
}

// ---------------------------------------------------------------- the failure

/// Which remediation an [`EgressNamespaceFailure`] calls for.
///
/// Not an `EnforcementTier` variant and not tied to `ContainmentClass`: a network
/// namespace is required for every Linux capsule that can spawn a subprocess, at every
/// containment class, so hanging it off the class ladder would make `advisory` capsules silently
/// exempt from this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressNamespaceBlocker {
    /// The kernel implements unprivileged user namespaces and something on this host refused one
    /// to `mur`: AppArmor's unprivileged-userns restriction, another AppArmor profile confining
    /// `mur`, a container runtime's seccomp filter or capabilities, or another security module.
    /// The refusal came at `unshare`, or after it while the namespace was owned or configured.
    CapabilityGrantMissing,
    /// The kernel does not provide the mechanism: `unshare` failed with `ENOSPC`
    /// (`user.max_user_namespaces`), `EINVAL` or `ENOSYS` (`CONFIG_USER_NS=n`), or `EUSERS` (the
    /// nesting limit).
    KernelSupportMissing,
    /// The runtime could not run the namespace creator, or the creator stopped without reporting
    /// a step — a process or descriptor limit, memory, or a policy that killed it — so the host's
    /// answer is unknown. Refused like the other two: a namespace nobody could create is a
    /// namespace the capsule does not get.
    CreatorUnavailable,
}

/// A creation of the capsule network namespace that failed, with what its refusal names.
///
/// Every refusal of `E-CAP-005` — at staging, at launch, at a session's first command, in
/// `mur doctor` — renders [`Self::reason`], so the places that ask cannot explain one host two
/// ways.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressNamespaceFailure {
    /// Decided by the failed call and its `errno` alone; the readings below never change it.
    pub blocker: EgressNamespaceBlocker,
    /// The call that failed, in [`CreationStep::describe`]'s words (`unshare(CLONE_NEWUSER |
    /// CLONE_NEWNET)`, `writing uid_map/gid_map`, …), or, for
    /// [`EgressNamespaceBlocker::CreatorUnavailable`], the runtime's account of why the creator
    /// could not run.
    pub failed_call: String,
    /// The `errno` the failed call left. `None` exactly for
    /// [`EgressNamespaceBlocker::CreatorUnavailable`], where no creation step ran.
    pub errno: Option<i32>,
    /// AppArmor's unprivileged-userns restriction as this process read it when the failure was
    /// observed. Explains the failure; never decides whether there is one.
    pub userns_grant: crate::sealed::UsernsGrant,
    /// The profile the kernel reports as confining this process, read at the same moment.
    pub attachment: crate::sealed::ProfileAttachment,
}

impl EgressNamespaceFailure {
    /// `<failed call> failed with <ERRNO>`, or the creator's account: the one-line answer
    /// `mur doctor` prints.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.errno {
            Some(errno) => format!("{} failed with {}", self.failed_call, errno_label(errno)),
            None => self.failed_call.clone(),
        }
    }

    /// The text an operator sees under `E-CAP-005`: what failed, with its `errno` symbolically
    /// and in words; the likely cause and its remedy; and, last, the host readings the cause was
    /// chosen from.
    #[must_use]
    pub fn reason(&self) -> String {
        let what = match self.errno {
            Some(errno) => format!(
                "{} failed with {} ({})",
                self.failed_call,
                errno_label(errno),
                errno_words(errno)
            ),
            None => self.failed_call.clone(),
        };
        format!("{what}. {} {}", self.cause(), self.readings())
    }

    /// The step `failed_call` names, when it names one.
    fn step(&self) -> Option<CreationStep> {
        self.errno?;
        CreationStep::ALL
            .into_iter()
            .find(|step| step.describe() == self.failed_call)
    }

    fn cause(&self) -> String {
        match self.blocker {
            EgressNamespaceBlocker::KernelSupportMissing => kernel_cause(self.errno).to_string(),
            EgressNamespaceBlocker::CreatorUnavailable => {
                "Likely cause: the namespace creator could not run or stopped before reporting — \
                 a process or descriptor limit, or memory, ran out, or a policy killed it (its \
                 account then reads `killed by signal`) — so whether this host can create \
                 the namespace is unknown, and the capsule is refused rather than run without \
                 it. Re-run once the limit is relieved, or allow the creator's syscalls."
                    .to_string()
            }
            EgressNamespaceBlocker::CapabilityGrantMissing => {
                let cause = grant_cause(self.userns_grant, &self.attachment);
                match self.step().and_then(step_context) {
                    Some(context) => format!("{context} {cause}"),
                    None => cause,
                }
            }
        }
    }

    fn readings(&self) -> String {
        host_readings(self.userns_grant, &self.attachment)
    }
}

/// The readings line every namespace refusal ends with — `E-CAP-005`'s, and `E-CAP-003`'s for
/// `sealed`. AppArmor's restriction knob reads `1` for both grants decided with it on, and `0` for
/// the one decided with it off.
pub(crate) fn host_readings(
    userns_grant: crate::sealed::UsernsGrant,
    attachment: &crate::sealed::ProfileAttachment,
) -> String {
    use crate::sealed::{ProfileAttachment, UsernsGrant};
    let restriction = match userns_grant {
        UsernsGrant::ApparmorAbsent => "AppArmor not enabled",
        UsernsGrant::RestrictionDisabledHostWide => {
            "kernel.apparmor_restrict_unprivileged_userns=0"
        }
        UsernsGrant::ProfileConfining | UsernsGrant::Withheld => {
            "kernel.apparmor_restrict_unprivileged_userns=1"
        }
    };
    let profile = match attachment {
        ProfileAttachment::Sealed { profile } | ProfileAttachment::Other { profile } => {
            profile.as_str()
        }
        ProfileAttachment::Unconfined => "unconfined",
        ProfileAttachment::NotReported => "not reported",
    };
    format!("Host readings: {restriction}; AppArmor profile confining mur: {profile}.")
}

/// What a step after `unshare` failing says about the namespace, ahead of the cause.
fn step_context(step: CreationStep) -> Option<&'static str> {
    match step {
        CreationStep::Unshare => None,
        CreationStep::IdentityMaps => Some(
            "The namespace was created and could not be owned: writing its identity map was \
             refused.",
        ),
        CreationStep::Loopback | CreationStep::LocalRoute => Some(
            "The namespace was created and owned, and configuring it was refused — the mark of a \
             confinement that withholds CAP_NET_ADMIN inside the namespace.",
        ),
        CreationStep::TcpListener
        | CreationStep::Resolver
        | CreationStep::NamespaceHandles
        | CreationStep::HandOff => {
            Some("The namespace was created and configured, and handing it to the runtime failed.")
        }
    }
}

/// The likely cause of a refused namespace on a kernel that provides one, chosen from the
/// readings.
///
/// AppArmor absent comes first because `/proc/self/attr/current` then reports another security
/// module's label, which is no AppArmor profile. A process another profile confines is next: the
/// host's restriction knob does not govern it, whatever it reads.
fn grant_cause(
    userns_grant: crate::sealed::UsernsGrant,
    attachment: &crate::sealed::ProfileAttachment,
) -> String {
    use crate::sealed::{
        ProfileAttachment, UsernsGrant, SEALED_APPARMOR_PROFILE_NAME, SEALED_APPARMOR_PROFILE_PATH,
    };
    const CONTAINER_REMEDY: &str = "Inside a container, run it with a seccomp policy that \
         permits unshare(CLONE_NEWUSER), or with `--cap-add SYS_ADMIN`, or create the network \
         namespace outside the container and run mur inside it.";
    match (userns_grant, attachment) {
        (UsernsGrant::ApparmorAbsent, _) => format!(
            "Likely cause: not AppArmor, which is not enabled on this host — a container \
             runtime's seccomp filter or a missing CAP_SYS_ADMIN, or another security module's \
             policy. {CONTAINER_REMEDY} On a bare host, check the policy of the security module \
             in use."
        ),
        (_, ProfileAttachment::Other { profile }) => format!(
            "Likely cause: the AppArmor profile `{profile}` confines mur. \
             kernel.apparmor_restrict_unprivileged_userns does not govern a process another \
             profile confines: that profile, and a container runtime's seccomp filter and \
             capabilities, decide whether it may create a user namespace. Run the container with \
             an AppArmor profile and seccomp policy that permit unshare(CLONE_NEWUSER), or with \
             `--cap-add SYS_ADMIN`, or create the network namespace outside the container and run \
             mur inside it."
        ),
        (UsernsGrant::ProfileConfining, _) | (_, ProfileAttachment::Sealed { .. }) => format!(
            "Likely cause: the {SEALED_APPARMOR_PROFILE_NAME} AppArmor profile confines mur and \
             the creation still failed, so the profile the kernel has loaded may be stale. Reload \
             it with `sudo apparmor_parser -r {SEALED_APPARMOR_PROFILE_PATH}` (for a checkout \
             build, re-run `scripts/install-dev-apparmor.sh`), and check `mur doctor`'s \
             comparison of the installed profile with the one this build ships."
        ),
        (UsernsGrant::RestrictionDisabledHostWide, _) => format!(
            "Likely cause: not AppArmor, whose unprivileged-userns restriction is off on this host \
             — a container runtime's seccomp filter or a missing CAP_SYS_ADMIN, or another \
             security module's policy. {CONTAINER_REMEDY}"
        ),
        (UsernsGrant::Withheld, _) => format!(
            "Likely cause: AppArmor's unprivileged-userns restriction \
             (kernel.apparmor_restrict_unprivileged_userns=1) applies to this binary, which no \
             {SEALED_APPARMOR_PROFILE_NAME} profile confines. Install and load the profile shipped \
             with mur, `sudo install -m 644 packaging/apparmor/{SEALED_APPARMOR_PROFILE_NAME} \
             {SEALED_APPARMOR_PROFILE_PATH} && sudo apparmor_parser -r \
             {SEALED_APPARMOR_PROFILE_PATH}` (or re-run the mur installer as root), then re-run. \
             Building out of a checkout, where the binary sits at ./target/{{debug,release}}/mur \
             and no shipped profile attaches to it: run `scripts/install-dev-apparmor.sh`, which \
             generates and loads the same grant for those two paths. LAST RESORT, only where a \
             profile genuinely cannot be loaded: `sudo sysctl -w \
             kernel.apparmor_restrict_unprivileged_userns=0` — this removes unprivileged-userns \
             hardening from every program on the machine, not just from mur, and is not the \
             configuration murmur ships."
        ),
    }
}

/// The likely cause of an `unshare` the kernel could not honour, by its `errno`.
fn kernel_cause(errno: Option<i32>) -> &'static str {
    match errno {
        Some(libc::ENOSPC) => {
            "Likely cause: the kernel's limit on user namespaces — user.max_user_namespaces is 0, \
             or every namespace it allows is in use. Raise it where it is set, on the host or in \
             the user namespace mur runs in (`sudo sysctl -w user.max_user_namespaces=10000`), \
             then re-run."
        }
        Some(libc::EUSERS) => {
            "Likely cause: the kernel's nesting limit for user namespaces — mur already runs \
             inside as many nested user namespaces as the kernel allows. Run it fewer levels \
             deep."
        }
        _ => {
            "Likely cause: this kernel does not provide user namespaces (built with \
             CONFIG_USER_NS=n). Run mur on a kernel built with CONFIG_USER_NS=y."
        }
    }
}

/// The symbolic name of the `errno` values a namespace creation can leave.
pub(crate) fn errno_name(errno: i32) -> Option<&'static str> {
    Some(match errno {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EACCES => "EACCES",
        libc::EINVAL => "EINVAL",
        libc::ENOSPC => "ENOSPC",
        libc::ENOSYS => "ENOSYS",
        libc::EUSERS => "EUSERS",
        libc::ENOMEM => "ENOMEM",
        libc::EAGAIN => "EAGAIN",
        libc::EADDRINUSE => "EADDRINUSE",
        libc::EMFILE => "EMFILE",
        libc::ENFILE => "ENFILE",
        _ => return None,
    })
}

/// [`errno_name`], or `errno <n>` for a value outside it.
pub(crate) fn errno_label(errno: i32) -> String {
    errno_name(errno).map_or_else(|| format!("errno {errno}"), str::to_string)
}

/// The C library's description of `errno`, without the `(os error <n>)` suffix `std` appends.
pub(crate) fn errno_words(errno: i32) -> String {
    let rendered = std::io::Error::from_raw_os_error(errno).to_string();
    rendered
        .strip_suffix(&format!(" (os error {errno})"))
        .map_or_else(|| rendered.clone(), str::to_string)
}

/// Whether a namespace attempt is an `E-CAP-005` failure, and which. Pure, and unit-testable on
/// any OS.
///
/// The attempt alone decides: `None` exactly when it succeeded. The readings only choose the
/// words of the refusal.
pub(crate) fn egress_namespace_failure(
    is_linux: bool,
    attempt: Result<(), NamespaceAttemptError>,
    userns_grant: crate::sealed::UsernsGrant,
    attachment: crate::sealed::ProfileAttachment,
) -> Option<EgressNamespaceFailure> {
    // Not Linux: there is no network namespace here. A non-Linux host resolves to
    // `EnforcementTier::EnvironmentOnly`, installs no kernel subprocess sandbox at all, and says
    // so loudly (`W_SEC_001`). Refusing here would turn every macOS run into an error about a
    // mechanism macOS has never had.
    if !is_linux {
        return None;
    }
    attempt
        .err()
        .map(|error| failure_of(error, userns_grant, attachment))
}

/// The failure a failed attempt is, classified by its step and `errno`.
fn failure_of(
    error: NamespaceAttemptError,
    userns_grant: crate::sealed::UsernsGrant,
    attachment: crate::sealed::ProfileAttachment,
) -> EgressNamespaceFailure {
    let (blocker, failed_call, errno) = match error {
        NamespaceAttemptError::Step { step, errno } => {
            let blocker = match (step, errno) {
                (
                    CreationStep::Unshare,
                    libc::ENOSPC | libc::EINVAL | libc::ENOSYS | libc::EUSERS,
                ) => EgressNamespaceBlocker::KernelSupportMissing,
                _ => EgressNamespaceBlocker::CapabilityGrantMissing,
            };
            (blocker, step.describe().to_string(), Some(errno))
        }
        NamespaceAttemptError::Creator(detail) => {
            (EgressNamespaceBlocker::CreatorUnavailable, detail, None)
        }
    };
    EgressNamespaceFailure {
        blocker,
        failed_call,
        errno,
        userns_grant,
        attachment,
    }
}

/// Whether this host can give a capsule its own network namespace, answered by creating one.
///
/// A fresh [`crate::sandbox::HostProbe`] each call, so it re-reads the host: the attempt is the
/// launch's own construction ([`linux::create_session_netns`] with an empty allowlist, the
/// namespace it hands over dropped at once), and it cannot pass where a launch would fail, or fail
/// where a launch would pass. A session does not call this: it reads the failure off its own
/// `HostProbe`, so staging and launch judge one reading.
#[must_use]
pub fn detect_egress_namespace_failure() -> Option<EgressNamespaceFailure> {
    crate::sandbox::HostProbe::probe().egress_namespace_failure()
}

/// Whether a test that needs a capsule network namespace must stand down on this host, printing
/// the failure when it must.
///
/// Test support, not a runtime code path. Asked through [`detect_egress_namespace_failure`], the
/// same probe `stage_session` consults, so the gate and the staging refusal cannot reach different
/// conclusions about the host.
///
/// Narrower than [`crate::skip_without_host_support`], which also requires a delegated cgroup
/// scope: a test driving `execute_shell` directly needs the namespace and nothing else, and
/// standing down on the cgroup half would skip it on hosts where it runs perfectly well.
pub fn skip_without_egress_namespace(test_name: &str) -> bool {
    match detect_egress_namespace_failure() {
        Some(failure) => {
            crate::runtime_err!("[SKIP-HOST] {test_name}: {}", failure.reason());
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
    failure: Option<&EgressNamespaceFailure>,
) -> Result<(), crate::errors::RuntimeError> {
    match failure {
        Some(failure) if can_spawn_subprocess => Err(unavailable(failure)),
        _ => Ok(()),
    }
}

/// The `E-CAP-005` refusal of `failure`.
fn unavailable(failure: &EgressNamespaceFailure) -> crate::errors::RuntimeError {
    crate::errors::RuntimeError::EgressNamespaceUnavailable {
        blocker: failure.blocker,
        reason: failure.reason(),
    }
}

/// What [`SessionNetns::establish`] reports when the session's namespace cannot be created.
///
/// A failed step is the `E-CAP-005` refusal staging would have rendered for it, with the readings
/// passed in. A creator that could not run is the runtime's own account: that spawn fails, and
/// the next one tries again.
fn session_netns_creation_error(
    error: NamespaceAttemptError,
    userns_grant: crate::sealed::UsernsGrant,
    attachment: crate::sealed::ProfileAttachment,
) -> String {
    match error {
        NamespaceAttemptError::Creator(detail) => {
            format!("sandbox: could not create this session's network namespace: {detail}")
        }
        step => format!(
            "sandbox: E-CAP-005: {}",
            unavailable(&failure_of(step, userns_grant, attachment))
        ),
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
    /// Load-bearing ordering, exactly as in `namespace_probe`'s rehearsals: inside a fresh user
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
    /// A creation step that fails is refused with `E-CAP-005` in the words staging uses, so a
    /// native tool pulled mid-session, whose first call is the first time anything asks, is
    /// refused the way a launch would have been.
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
        // The readings are taken here, in the runtime, when a creation has failed: the creator
        // child exits as soon as it reports.
        let created = linux::create_session_netns(&plan).map_err(|error| {
            session_netns_creation_error(
                error,
                crate::sealed::userns_grant(),
                crate::sealed::inspect_profile_attachment(),
            )
        })?;
        let established = created.serve(policy).map_err(|detail| {
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
pub(crate) use linux::{create_session_netns, join_session_netns};

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::sync::Arc;

    use super::{
        CapsuleNetnsPlan, CreationStep, NamespaceAttemptError, SessionNetnsJoin,
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

    // ------------------------------------------------------------ the creator child

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

    impl From<CreationFailure> for NamespaceAttemptError {
        fn from(failure: CreationFailure) -> Self {
            NamespaceAttemptError::Step {
                step: failure.step,
                errno: failure.errno,
            }
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
    pub(crate) struct CreatedNetns {
        sockets: Vec<OwnedFd>,
        user_ns: OwnedFd,
        net_ns: OwnedFd,
    }

    impl CreatedNetns {
        /// `(user_ns, net_ns)`, for [`join_session_netns`] in a child forked while this is held.
        pub(crate) fn namespace_handles(&self) -> (RawFd, RawFd) {
            (self.user_ns.as_raw_fd(), self.net_ns.as_raw_fd())
        }

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
    ///
    /// A failed step is [`NamespaceAttemptError::Step`] with the step and the `errno` the creator
    /// reported; every way the creator itself could not run or report is
    /// [`NamespaceAttemptError::Creator`].
    pub(crate) fn create_session_netns(
        plan: &CapsuleNetnsPlan,
    ) -> Result<CreatedNetns, NamespaceAttemptError> {
        // Checked here because the creator indexes a fixed buffer of this size and must not be the
        // place an oversized plan is discovered.
        if plan.socket_count() > MAX_NAMESPACE_SOCKETS {
            return Err(NamespaceAttemptError::Creator(format!(
                "{} namespace sockets requested, more than the {MAX_NAMESPACE_SOCKETS} one \
                 hand-off carries",
                plan.socket_count()
            )));
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
            return Err(NamespaceAttemptError::Creator(format!(
                "socketpair() for the namespace hand-off failed: {}",
                io::Error::last_os_error()
            )));
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
            return Err(NamespaceAttemptError::Creator(format!(
                "fork() for the namespace creator failed: {}",
                io::Error::last_os_error()
            )));
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

        let status = wait_for_creator(pid).map_err(NamespaceAttemptError::Creator)?;
        match receive_session_netns(parent_end.as_raw_fd(), plan.socket_count())
            .map_err(NamespaceAttemptError::Creator)?
        {
            Received::Handles(mut fds) => {
                let net_ns = fds.pop().expect("the count was checked on receipt");
                let user_ns = fds.pop().expect("the count was checked on receipt");
                Ok(CreatedNetns {
                    sockets: fds,
                    user_ns,
                    net_ns,
                })
            }
            Received::Failure(failure) => Err(failure.into()),
            Received::Nothing => Err(NamespaceAttemptError::Creator(format!(
                "the namespace creator exited without handing anything over ({status})"
            ))),
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

    /// Encodes and decodes a failure record as the creator and the parent do, as the error the
    /// parent returns for it.
    #[cfg(test)]
    pub(super) fn decode_failure_record(step: u32, errno: i32) -> Option<NamespaceAttemptError> {
        let mut record = [0u8; FAILURE_RECORD_LEN];
        record[..4].copy_from_slice(&step.to_ne_bytes());
        record[4..].copy_from_slice(&errno.to_ne_bytes());
        CreationFailure::decode(&record).map(|failure| {
            assert_eq!(failure.encode(), record);
            failure.into()
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
    use crate::sealed::{ProfileAttachment, UsernsGrant};

    fn attachments() -> [ProfileAttachment; 4] {
        [
            ProfileAttachment::Sealed {
                profile: "mur-sealed".to_string(),
            },
            ProfileAttachment::Other {
                profile: "docker-default".to_string(),
            },
            ProfileAttachment::Unconfined,
            ProfileAttachment::NotReported,
        ]
    }

    fn step(step: CreationStep, errno: i32) -> Result<(), NamespaceAttemptError> {
        Err(NamespaceAttemptError::Step { step, errno })
    }

    fn failure(
        attempt: Result<(), NamespaceAttemptError>,
        grant: UsernsGrant,
        attachment: ProfileAttachment,
    ) -> EgressNamespaceFailure {
        egress_namespace_failure(true, attempt, grant, attachment)
            .expect("a failed attempt is a failure")
    }

    // ---- the decision ----------------------------------------------------------------------

    /// A namespace that was created is never refused, whatever AppArmor reads: a container whose
    /// own profile permits it, on a host whose restriction reads `1`, gets its namespace.
    #[test]
    fn a_created_namespace_is_never_refused_whatever_the_readings() {
        for grant in UsernsGrant::ALL {
            for attachment in attachments() {
                assert_eq!(
                    egress_namespace_failure(true, Ok(()), *grant, attachment.clone()),
                    None,
                    "{grant:?} / {attachment:?}"
                );
            }
        }
    }

    /// A namespace that was not created is refused, whatever AppArmor reads, and the blocker is
    /// the same for every reading.
    #[test]
    fn a_failed_creation_is_refused_whatever_the_readings() {
        let attempts = [
            (
                step(CreationStep::Unshare, libc::EPERM),
                EgressNamespaceBlocker::CapabilityGrantMissing,
            ),
            (
                step(CreationStep::Unshare, libc::ENOSPC),
                EgressNamespaceBlocker::KernelSupportMissing,
            ),
            (
                Err(NamespaceAttemptError::Creator("fork() failed".to_string())),
                EgressNamespaceBlocker::CreatorUnavailable,
            ),
        ];
        for (attempt, blocker) in attempts {
            for grant in UsernsGrant::ALL {
                for attachment in attachments() {
                    let failure =
                        egress_namespace_failure(true, attempt.clone(), *grant, attachment.clone())
                            .unwrap_or_else(|| {
                                panic!("{attempt:?} under {grant:?} / {attachment:?}")
                            });
                    assert_eq!(failure.blocker, blocker, "{grant:?} / {attachment:?}");
                }
            }
        }
    }

    #[test]
    fn only_an_unshare_the_kernel_cannot_honour_is_missing_kernel_support() {
        for errno in [libc::ENOSPC, libc::EINVAL, libc::ENOSYS, libc::EUSERS] {
            assert_eq!(
                failure(
                    step(CreationStep::Unshare, errno),
                    UsernsGrant::Withheld,
                    ProfileAttachment::Unconfined
                )
                .blocker,
                EgressNamespaceBlocker::KernelSupportMissing,
                "errno {errno}"
            );
        }
        for (failed, errno) in [
            (CreationStep::Unshare, libc::EPERM),
            (CreationStep::Unshare, libc::EACCES),
            (CreationStep::IdentityMaps, libc::EPERM),
            (CreationStep::Loopback, libc::EPERM),
            (CreationStep::LocalRoute, libc::EINVAL),
            (CreationStep::Resolver, libc::ENOSPC),
        ] {
            assert_eq!(
                failure(
                    step(failed, errno),
                    UsernsGrant::Withheld,
                    ProfileAttachment::Unconfined
                )
                .blocker,
                EgressNamespaceBlocker::CapabilityGrantMissing,
                "{failed:?} errno {errno}"
            );
        }
    }

    #[test]
    fn a_creator_that_could_not_run_is_unavailable_and_keeps_its_detail() {
        let detail = "fork() for the namespace creator failed: Resource temporarily unavailable \
                      (os error 11)";
        let failure = failure(
            Err(NamespaceAttemptError::Creator(detail.to_string())),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        assert_eq!(failure.blocker, EgressNamespaceBlocker::CreatorUnavailable);
        assert_eq!(failure.errno, None);
        let reason = failure.reason();
        assert!(reason.starts_with(detail), "{reason}");
        assert!(!reason.contains("apparmor_parser"), "{reason}");
        assert!(!reason.contains("CONFIG_USER_NS"), "{reason}");
        assert!(!reason.contains("user.max_user_namespaces"), "{reason}");
    }

    #[test]
    fn non_linux_is_never_refused_for_a_mechanism_it_never_had() {
        for attempt in [
            Ok(()),
            step(CreationStep::Unshare, libc::EPERM),
            Err(NamespaceAttemptError::Creator("fork() failed".to_string())),
        ] {
            assert_eq!(
                egress_namespace_failure(
                    false,
                    attempt,
                    UsernsGrant::Withheld,
                    ProfileAttachment::NotReported
                ),
                None
            );
        }
        #[cfg(not(target_os = "linux"))]
        assert_eq!(detect_egress_namespace_failure(), None);
    }

    #[test]
    fn the_check_refuses_exactly_a_capsule_that_can_spawn_on_a_failing_host() {
        let failure = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        match check_egress_namespace(true, Some(&failure)) {
            Err(crate::errors::RuntimeError::EgressNamespaceUnavailable { blocker, reason }) => {
                assert_eq!(blocker, failure.blocker);
                assert_eq!(reason, failure.reason());
            }
            other => panic!("expected the E-CAP-005 refusal, got {other:?}"),
        }
        assert!(check_egress_namespace(false, Some(&failure)).is_ok());
        assert!(check_egress_namespace(true, None).is_ok());
        assert!(check_egress_namespace(false, None).is_ok());
    }

    // ---- the reason ------------------------------------------------------------------------

    #[test]
    fn an_unconfined_binary_on_a_restricted_host_is_told_the_profile_before_the_sysctl() {
        let reason = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        )
        .reason();
        assert!(
            reason.starts_with(
                "unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with EPERM (Operation not \
                 permitted)."
            ),
            "{reason}"
        );
        assert!(
            reason.contains("kernel.apparmor_restrict_unprivileged_userns=1"),
            "{reason}"
        );
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
        assert!(reason.contains("LAST RESORT"), "{reason}");
        assert!(reason.contains("every program on the machine"), "{reason}");
        assert!(
            reason.contains("scripts/install-dev-apparmor.sh"),
            "{reason}"
        );
        assert!(
            reason.ends_with(
                "Host readings: kernel.apparmor_restrict_unprivileged_userns=1; AppArmor profile \
                 confining mur: unconfined."
            ),
            "{reason}"
        );
    }

    #[test]
    fn a_binary_another_profile_confines_is_given_the_container_remedy_not_the_sysctl() {
        let reason = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::Withheld,
            ProfileAttachment::Other {
                profile: "docker-default".to_string(),
            },
        )
        .reason();
        assert!(reason.contains("`docker-default`"), "{reason}");
        assert!(reason.contains("--cap-add SYS_ADMIN"), "{reason}");
        assert!(reason.contains("seccomp"), "{reason}");
        assert!(
            !reason.contains("kernel.apparmor_restrict_unprivileged_userns=0"),
            "{reason}"
        );
        assert!(!reason.contains("apparmor_parser"), "{reason}");
        assert!(
            reason.ends_with("AppArmor profile confining mur: docker-default."),
            "{reason}"
        );
    }

    #[test]
    fn a_failure_under_the_sealed_profile_points_at_a_stale_load() {
        let reason = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::ProfileConfining,
            ProfileAttachment::Sealed {
                profile: "mur-sealed".to_string(),
            },
        )
        .reason();
        assert!(
            reason.contains("sudo apparmor_parser -r /etc/apparmor.d/mur-sealed"),
            "{reason}"
        );
        assert!(reason.contains("stale"), "{reason}");
        assert!(!reason.contains("LAST RESORT"), "{reason}");
    }

    #[test]
    fn without_the_restriction_the_cause_is_not_apparmor() {
        for (grant, readings) in [
            (
                UsernsGrant::RestrictionDisabledHostWide,
                "kernel.apparmor_restrict_unprivileged_userns=0",
            ),
            (UsernsGrant::ApparmorAbsent, "AppArmor not enabled"),
        ] {
            for attachment in [
                ProfileAttachment::Unconfined,
                ProfileAttachment::NotReported,
            ] {
                let reason =
                    failure(step(CreationStep::Unshare, libc::EPERM), grant, attachment).reason();
                assert!(
                    !reason.contains("apparmor_restrict_unprivileged_userns=1"),
                    "{reason}"
                );
                assert!(!reason.contains("apparmor_parser"), "{reason}");
                assert!(reason.contains("--cap-add SYS_ADMIN"), "{reason}");
                assert!(
                    reason.contains(&format!("Host readings: {readings};")),
                    "{reason}"
                );
            }
        }
    }

    /// On a host without AppArmor `/proc/self/attr/current` carries another module's label, which
    /// is no AppArmor profile to blame.
    #[test]
    fn another_security_modules_label_is_not_blamed_as_an_apparmor_profile() {
        let reason = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::ApparmorAbsent,
            ProfileAttachment::Other {
                profile: "unconfined_u:unconfined_r:unconfined_t:s0".to_string(),
            },
        )
        .reason();
        assert!(reason.contains("not AppArmor"), "{reason}");
        assert!(!reason.contains("the AppArmor profile `"), "{reason}");
    }

    #[test]
    fn a_namespace_created_and_then_refused_says_which_half_failed() {
        let configure = failure(
            step(CreationStep::Loopback, libc::EPERM),
            UsernsGrant::RestrictionDisabledHostWide,
            ProfileAttachment::Unconfined,
        )
        .reason();
        assert!(
            configure.starts_with("bringing lo up failed with EPERM"),
            "{configure}"
        );
        assert!(configure.contains("CAP_NET_ADMIN"), "{configure}");

        let owned = failure(
            step(CreationStep::IdentityMaps, libc::EACCES),
            UsernsGrant::RestrictionDisabledHostWide,
            ProfileAttachment::Unconfined,
        )
        .reason();
        assert!(
            owned.starts_with("writing uid_map/gid_map failed with EACCES (Permission denied)."),
            "{owned}"
        );
        assert!(owned.contains("could not be owned"), "{owned}");
    }

    #[test]
    fn missing_kernel_support_names_the_limit_or_the_config_symbol_and_no_apparmor() {
        for (errno, named) in [
            (libc::ENOSPC, "user.max_user_namespaces"),
            (libc::EINVAL, "CONFIG_USER_NS"),
            (libc::ENOSYS, "CONFIG_USER_NS"),
            (libc::EUSERS, "nesting limit"),
        ] {
            let reason = failure(
                step(CreationStep::Unshare, errno),
                UsernsGrant::Withheld,
                ProfileAttachment::Unconfined,
            )
            .reason();
            assert!(reason.contains(named), "{reason}");
            assert!(
                reason.contains(errno_name(errno).expect("a named errno")),
                "{reason}"
            );
            assert!(!reason.contains("apparmor_parser"), "{reason}");
            assert!(!reason.contains("LAST RESORT"), "{reason}");
        }
    }

    #[test]
    fn an_unnamed_errno_is_rendered_by_number() {
        assert_eq!(errno_name(libc::EPERM), Some("EPERM"));
        assert_eq!(errno_name(-7), None);
        let failure = failure(
            step(CreationStep::HandOff, 4095),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        assert_eq!(
            failure.summary(),
            "handing the namespace to the runtime failed with errno 4095"
        );
    }

    #[test]
    fn the_summary_names_the_call_and_the_errno() {
        let failed = failure(
            step(CreationStep::Unshare, libc::EPERM),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        assert_eq!(
            failed.summary(),
            "unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with EPERM"
        );
        let creator = failure(
            Err(NamespaceAttemptError::Creator("fork() failed".to_string())),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        assert_eq!(creator.summary(), "fork() failed");
    }

    #[test]
    fn every_step_round_trips_through_its_code() {
        for step in CreationStep::ALL {
            assert_eq!(CreationStep::from_code(step as u32), Some(step));
        }
        assert_eq!(CreationStep::from_code(0), None);
        assert_eq!(CreationStep::from_code(99), None);
    }

    // ---- one probe, one reason -------------------------------------------------------------

    /// A step that fails in a session's own creation is refused with exactly the text staging
    /// renders for the same step, `errno` and readings.
    #[test]
    fn a_session_creation_failure_is_refused_in_the_staging_words() {
        for (failed, errno, grant, attachment) in [
            (
                CreationStep::Unshare,
                libc::EPERM,
                UsernsGrant::Withheld,
                ProfileAttachment::Unconfined,
            ),
            (
                CreationStep::LocalRoute,
                libc::EPERM,
                UsernsGrant::Withheld,
                ProfileAttachment::Other {
                    profile: "docker-default".to_string(),
                },
            ),
            (
                CreationStep::Unshare,
                libc::ENOSPC,
                UsernsGrant::RestrictionDisabledHostWide,
                ProfileAttachment::Unconfined,
            ),
        ] {
            let staging =
                egress_namespace_failure(true, step(failed, errno), grant, attachment.clone());
            let staging_refusal = check_egress_namespace(true, staging.as_ref())
                .expect_err("staging refuses")
                .to_string();
            let establish = session_netns_creation_error(
                NamespaceAttemptError::Step {
                    step: failed,
                    errno,
                },
                grant,
                attachment,
            );
            assert_eq!(establish, format!("sandbox: E-CAP-005: {staging_refusal}"));
            assert!(
                establish.contains(&staging.expect("a failure").reason()),
                "{establish}"
            );
        }
    }

    #[test]
    fn a_session_creator_that_could_not_run_keeps_the_runtime_account() {
        let error = session_netns_creation_error(
            NamespaceAttemptError::Creator("fork() for the namespace creator failed".to_string()),
            UsernsGrant::Withheld,
            ProfileAttachment::Unconfined,
        );
        assert_eq!(
            error,
            "sandbox: could not create this session's network namespace: fork() for the \
             namespace creator failed"
        );
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
        assert_eq!(
            linux::decode_failure_record(4, libc::EPERM),
            Some(NamespaceAttemptError::Step {
                step: CreationStep::LocalRoute,
                errno: libc::EPERM,
            })
        );
        assert_eq!(linux::decode_failure_record(99, 0), None);
    }

    /// The probe is the launch's own construction, so on a host where it reports no failure a
    /// session creates its namespace, and on one where it reports a failure the session's
    /// creation fails at the same step.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_probe_and_a_session_creation_agree_about_this_host() {
        let probe = detect_egress_namespace_failure();
        let session = linux::create_session_netns(&CapsuleNetnsPlan::resolve(Vec::new())).map(drop);
        match (&probe, &session) {
            (None, Ok(())) => {}
            (Some(failure), Err(NamespaceAttemptError::Step { step, errno })) => {
                assert_eq!(failure.failed_call, step.describe());
                assert_eq!(failure.errno, Some(*errno));
            }
            other => panic!("the probe and a session creation disagree: {other:?}"),
        }
    }
}
