//! The `sealed` containment mechanism: a private mount namespace pivoted onto a composed root.
//!
//! `scoped` (Landlock + seccomp, see [`crate::sandbox`]) can deliver "nothing outside the workdir
//! is readable or writable". It can never deliver "nothing outside the workdir *exists*", because
//! Landlock is an access filter — it does not change what a process can see, name or traverse. A
//! mount namespace can, and that is the whole of what this module adds.
//!
//! ## What the mechanism is
//!
//! Inside the forked child's `pre_exec` window — before fd hygiene, before the capability drop,
//! before the seccomp filter, and before the Landlock ruleset — the child:
//!
//!   1. `unshare(CLONE_NEWUSER | CLONE_NEWNS)`, then maps its own uid/gid one-to-one into the new
//!      user namespace (`/proc/self/uid_map`), which is what gives it `CAP_SYS_ADMIN` *inside that
//!      namespace* and nowhere else. Nothing here needs the host's root.
//!   2. Makes mount propagation private (`MS_REC | MS_PRIVATE` on `/`), so nothing it does leaks
//!      back into the host's mount table.
//!   3. Mounts a fresh `tmpfs` over a base directory ([`choose_root_base`]) and composes the new
//!      root inside it: the host runtime directories ([`SEALED_RUNTIME_PATHS`]) and a fixed,
//!      narrow `/etc` set ([`SEALED_ETC_PATHS`]) bind-mounted **read-only**, the session workdir
//!      bind-mounted **read-write at its own absolute path**, a private `/dev` tmpfs carrying the
//!      OCI default device set ([`SEALED_DEVICE_NODES`]), and `/proc` mounted with `hidepid`.
//!   4. `pivot_root`s onto it, detaches the old root, remounts the new root read-only, and
//!      `chdir`s back into the workdir.
//!
//! Everything outside that root is then not merely access-denied — it is *absent*. There is no
//! pathname for `/etc/shadow`, for a Docker socket, or for `/dev/sda`, so the escape classes that
//! `scoped` has to enumerate and refuse (pathname unix sockets, device nodes, metadata visibility,
//! stdlib directory enumeration) stop being reachable rather than being individually denied.
//!
//! ## What it does *not* replace
//!
//! Landlock and seccomp still install afterwards, now scoped to the private root. `sealed` is
//! defence in depth over `scoped`'s mechanism, not a substitute for it: the syscall allowlist is
//! still the only thing standing between a capsule and `io_uring`, `bpf` or `ptrace`, none of
//! which a mount namespace has an opinion about.
//!
//! ## Why the plan is precomputed
//!
//! Everything the child does is driven by [`SealedRootSpec`], a fully-resolved list of
//! NUL-terminated paths built in the **parent**, before `fork()`. The `pre_exec` window permits
//! only async-signal-safe work: allocating there (a `format!`, a `PathBuf::join`) can deadlock on
//! the allocator lock a *different* thread of the parent held at fork time. Splitting the decision
//! ([`plan_composed_root`], pure and unit-testable against a fake host layout) from the execution
//! ([`construct_composed_root`], raw syscalls over already-built `CString`s) is what keeps the
//! child side allocation-free on its success path — and it is what makes the composed-root layout
//! testable at all on a host that cannot create a namespace.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- AppArmor

/// Name of the AppArmor profile this runtime ships, as it appears in
/// `/sys/kernel/security/apparmor/profiles` and in `/proc/self/attr/current`.
///
/// On a host where AppArmor's `restrict_unprivileged_userns` knob is on (Ubuntu 23.10+ and
/// derivatives), an unconfined process's `unshare(CLONE_NEWUSER)` is transitioned into the
/// restricted `unprivileged_userns` profile, which denies `CAP_SYS_ADMIN` and therefore every
/// `mount(2)` that follows. Loading this profile — which attaches to the `mur` binary and carries
/// `userns,` — is what lifts that, exactly as Ubuntu's own shipped profiles do for Chrome and
/// Firefox.
pub const SEALED_APPARMOR_PROFILE_NAME: &str = "mur-sealed";

/// Where [`scripts/install.sh`](../../../scripts/install.sh) installs the shipped profile, and the
/// path the refusal message tells an operator to load.
pub const SEALED_APPARMOR_PROFILE_PATH: &str = "/etc/apparmor.d/mur-sealed";

// ---------------------------------------------------------------- blockers

/// The one mechanism that stands between this host and `sealed`, named specifically enough that
/// the refusal carries a command the operator can run.
///
/// Deliberately a small closed enum rather than a free-form string: `containment.rs` renders it,
/// `sandbox.rs` derives it from the probe, and the manual-verification document asserts the exact
/// text of each rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SealedBlocker {
    /// Not Linux. Mount namespaces do not exist here and never will — the same permanence
    /// `EnforcementTier::EnvironmentOnly` carries.
    NotLinux,
    /// AppArmor's unprivileged-user-namespace restriction is active and this binary is not
    /// confined by the shipped [`SEALED_APPARMOR_PROFILE_NAME`] profile.
    AppArmorProfileMissing,
    /// `unshare(CLONE_NEWUSER | CLONE_NEWNS)` was refused outright — the container case: no
    /// `CAP_SYS_ADMIN`, or the container's own seccomp filter blocking the syscall.
    NamespaceCreationDenied,
    /// The namespace was created, but `mount(2)` inside it was refused. The signature of a
    /// confinement that permits `userns_create` and then denies `CAP_SYS_ADMIN`.
    MountDenied,
    /// The kernel has no unprivileged user namespace support at all (`CONFIG_USER_NS=n`, or
    /// `user.max_user_namespaces=0`).
    KernelUnsupported,
    /// Namespaces are available but Landlock is not, so the defence-in-depth layer `sealed`
    /// is required to keep would be missing. `sealed` is strictly stronger than `scoped`; a host
    /// that cannot back `scoped` cannot back `sealed` either.
    LandlockUnavailable,
}

impl SealedBlocker {
    /// One sentence naming the missing mechanism, plus the exact remediation. Rendered into
    /// `RuntimeError::ContainmentFloorUnmet`'s `reason`, so this is the text an operator sees
    /// under `E-CAP-003`.
    #[must_use]
    pub fn reason(self) -> String {
        match self {
            SealedBlocker::NotLinux => "sealed requires a Linux mount namespace + pivot_root; this \
                 platform has no such primitive and never will (macOS and every non-Linux target \
                 stay at advisory permanently)"
                .to_string(),
            SealedBlocker::AppArmorProfileMissing => format!(
                "sealed requires an unprivileged user+mount namespace, and AppArmor's \
                 unprivileged-userns restriction is active on this host while the '{name}' profile \
                 is not confining this binary. Install and load the profile shipped with mur: \
                 `sudo install -m 644 packaging/apparmor/{name} {path} && sudo apparmor_parser -r \
                 {path}` (or re-run the mur installer as root), then re-run. To turn the \
                 restriction off host-wide instead: `sudo sysctl -w \
                 kernel.apparmor_restrict_unprivileged_userns=0`.",
                name = SEALED_APPARMOR_PROFILE_NAME,
                path = SEALED_APPARMOR_PROFILE_PATH,
            ),
            SealedBlocker::NamespaceCreationDenied => {
                "sealed requires unshare(CLONE_NEWUSER | CLONE_NEWNS), which this host refused. \
                 This is the usual answer inside a container: CAP_SYS_ADMIN is absent, or the \
                 container's own seccomp filter blocks unshare(2). Either add `--cap-add \
                 SYS_ADMIN` to the container invocation, or establish the mount namespace outside \
                 the container and run mur inside it. The runtime will not fall back to a weaker \
                 class."
                    .to_string()
            }
            SealedBlocker::MountDenied => format!(
                "sealed created a user+mount namespace but mount(2) inside it was refused, which \
                 is what a confinement that permits userns_create and then denies CAP_SYS_ADMIN \
                 looks like. On an AppArmor host, load the shipped profile: `sudo apparmor_parser \
                 -r {path}`. Inside a container, add `--cap-add SYS_ADMIN` to the container \
                 invocation, or establish the mount namespace outside the container.",
                path = SEALED_APPARMOR_PROFILE_PATH,
            ),
            SealedBlocker::KernelUnsupported => {
                "sealed requires unprivileged user namespaces, which this kernel does not provide \
                 (CONFIG_USER_NS=n, or user.max_user_namespaces=0). Raise \
                 `sudo sysctl -w user.max_user_namespaces=10000` if the sysctl is merely zeroed, \
                 otherwise run on a kernel built with CONFIG_USER_NS=y."
                    .to_string()
            }
            SealedBlocker::LandlockUnavailable => {
                "sealed keeps Landlock and seccomp inside the composed root as defence in depth, \
                 and this host provides no usable Landlock ABI (Linux 5.13+ required). A host that \
                 cannot back scoped cannot back sealed either."
                    .to_string()
            }
        }
    }
}

// ---------------------------------------------------------------- host probe result

/// Outcome of really trying to create a user+mount namespace in a forked child.
///
/// The four states are not interchangeable: `Denied` and `MountDenied` both mean "no sealed here"
/// but point at completely different remediations (container capability vs. AppArmor profile),
/// which is the entire reason this is not a `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum NamespaceProbe {
    /// `unshare` succeeded and a `mount(2)` inside the new namespace succeeded.
    Ok,
    /// `unshare` itself was refused (`EPERM`).
    Denied,
    /// `unshare` succeeded, `mount(2)` inside the namespace did not.
    MountDenied,
    /// The kernel does not implement it (`ENOSYS`/`EINVAL`), or the probe could not run at all.
    #[default]
    Unsupported,
}

/// Everything the host probe learned about `sealed`, kept separate from the decision that uses it
/// so `sandbox::tier_from_probe` stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SealedProbe {
    /// AppArmor does not stand between this binary and an unprivileged user namespace: either its
    /// `restrict_unprivileged_userns` knob is off (or AppArmor is absent entirely), or the shipped
    /// [`SEALED_APPARMOR_PROFILE_NAME`] profile is confining this process.
    ///
    /// Named for the question that actually matters rather than for "is the profile loaded",
    /// because a Fedora or Arch host with no AppArmor at all needs no profile and must not be
    /// refused for lacking one.
    pub(crate) apparmor_permits_userns: bool,
    /// What a real `unshare` + `mount` attempt in a forked child did.
    pub(crate) namespace: NamespaceProbe,
}

/// Which single mechanism to blame, given the probe. Pure; the ordering is the order an operator
/// has to fix things in.
///
/// `None` means every precondition holds and this host can back `sealed`.
pub(crate) fn sealed_blocker(
    is_linux: bool,
    landlock_fully_enforced: bool,
    probe: SealedProbe,
) -> Option<SealedBlocker> {
    if !is_linux {
        return Some(SealedBlocker::NotLinux);
    }
    // Blame AppArmor before the namespace outcome: when the restriction is on and our profile is
    // not loaded, the namespace failure is a *consequence*, and pointing at `--cap-add SYS_ADMIN`
    // on a bare Ubuntu desktop would send the operator somewhere useless.
    if !probe.apparmor_permits_userns {
        return Some(SealedBlocker::AppArmorProfileMissing);
    }
    match probe.namespace {
        NamespaceProbe::Ok => {
            if landlock_fully_enforced {
                None
            } else {
                Some(SealedBlocker::LandlockUnavailable)
            }
        }
        NamespaceProbe::Denied => Some(SealedBlocker::NamespaceCreationDenied),
        NamespaceProbe::MountDenied => Some(SealedBlocker::MountDenied),
        NamespaceProbe::Unsupported => Some(SealedBlocker::KernelUnsupported),
    }
}

// ---------------------------------------------------------------- the composed root, declared

/// Host directories bind-mounted read-only into every composed root, in order.
///
/// Fixed, not derived: this is the roadmap's validated recipe (`bind-mount /usr`) plus the
/// non-usrmerge spellings of the same tree. On a usrmerge distro `/bin`, `/sbin`, `/lib`,
/// `/lib64`… are *symlinks* into `/usr`; [`plan_composed_root`] recreates them as symlinks rather
/// than bind-mounting through them, so the composed root has the same shape as the host and a
/// hashbang like `#!/bin/sh` resolves.
///
/// Deliberately a whole-directory bind rather than a curated per-file set: the point of `sealed`
/// is that it *deletes* the ELF-closure derivation `scoped` needs, along with its lock-time
/// pinning problem. A later slice (`staged-runtime-bind-mount`) replaces this list with a staged
/// interpreter tree; until then, anything a `shell.allow` binary needs at runtime must be under
/// one of these or under a directory derived from the manifest — see `extra_read_only` in
/// [`plan_composed_root`].
pub const SEALED_RUNTIME_PATHS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32",
];

/// The only `/etc` entries that exist inside a composed root, each bind-mounted read-only and each
/// silently skipped when the host does not have it.
///
/// An allowlist, never `/etc` wholesale, and never a denylist of sensitive names: `/etc/shadow`,
/// `/etc/sudoers`, `/etc/ssh`, cloud-init credentials and every future addition to `/etc` are
/// absent by construction rather than by enumeration. The entries here are the ones without which
/// ordinary tooling misreports rather than fails cleanly — the dynamic loader's cache, the TLS
/// trust store, DNS configuration, the timezone, and the passwd/group databases `getpwuid(3)`
/// needs.
///
/// Residual, recorded rather than buried: `/etc/passwd` and `/etc/group` are world-readable on
/// every distribution, so binding them leaks the host's account names into the capsule. They are
/// bound rather than synthesised because synthesising them means writing files from inside the
/// `pre_exec` window, and this module's discipline is that that window performs no work the parent
/// could have done for it. Synthesising a two-line passwd/group in the parent is the obvious
/// follow-up.
pub const SEALED_ETC_PATHS: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/ca-certificates.conf",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/nsswitch.conf",
    "/etc/localtime",
    "/etc/timezone",
    "/etc/terminfo",
    "/etc/passwd",
    "/etc/group",
];

/// One entry of the private `/dev` tmpfs.
///
/// `major`/`minor`/`mode` are recorded for the record and for the manual-verification document to
/// assert against — they are **not** used to create the node. Device nodes cannot be `mknod`ed
/// from inside a non-initial user namespace at all (the kernel refuses regardless of
/// `CAP_MKNOD`), so each entry is created as an empty file in the tmpfs and the host's node is
/// bind-mounted over it. This is what bubblewrap and rootless podman do, and it means the numbers
/// a capsule actually sees are the host's — which on every mainstream distribution are exactly the
/// ones below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealedDevice {
    /// Name under `/dev`, without the directory.
    pub name: &'static str,
    /// The host path bind-mounted over the tmpfs entry — `/dev/` + [`Self::name`], spelled out
    /// because it is also what the composed root's Landlock rule set is built from.
    pub path: &'static str,
    pub major: u32,
    pub minor: u32,
    pub mode: u32,
    /// Whether the composed root's Landlock rule for this device carries `WriteFile` on top of
    /// `ReadFile`.
    ///
    /// Landlock still mediates inside a composed root — that is the whole point of keeping it as
    /// defence in depth — so a device present in `/dev` but absent from the rule set would exist
    /// and be unopenable, which is worse than not being there. The read-only pair is the two
    /// entropy sources, where nothing legitimate writes; the rest carry write because their write
    /// side is defined to discard (`null`, `zero`), to report `ENOSPC` (`full`), or to be a
    /// terminal (`tty`).
    pub writable: bool,
}

/// The OCI runtime-spec default device set, which is what a `sealed` capsule's `/dev` contains and
/// all it contains.
///
/// This is deliberately *independent* of `sandbox::CAPSULE_DEVICE_GRANTS`, the three-device
/// Landlock grant set that services `scoped`. That set is a rule list layered over the host's real
/// `/dev`; this one is the entire contents of a private tmpfs. They answer different questions and
/// neither should be derived from the other.
///
/// `/dev/console` is omitted (the OCI spec creates it only for a terminal-attached container, and
/// a capsule subprocess has pipes, not a controlling terminal). `/dev/ptmx` and `/dev/pts` come
/// from a `devpts` mount rather than from this list, and `/dev/shm` is omitted deliberately: it is
/// writable, and the session workdir is the only writable path in a composed root.
pub const SEALED_DEVICE_NODES: &[SealedDevice] = &[
    SealedDevice { name: "null", path: "/dev/null", major: 1, minor: 3, mode: 0o666, writable: true },
    SealedDevice { name: "zero", path: "/dev/zero", major: 1, minor: 5, mode: 0o666, writable: true },
    SealedDevice { name: "full", path: "/dev/full", major: 1, minor: 7, mode: 0o666, writable: true },
    SealedDevice { name: "random", path: "/dev/random", major: 1, minor: 8, mode: 0o666, writable: false },
    SealedDevice { name: "urandom", path: "/dev/urandom", major: 1, minor: 9, mode: 0o666, writable: false },
    SealedDevice { name: "tty", path: "/dev/tty", major: 5, minor: 0, mode: 0o666, writable: true },
];

/// The OCI default `/dev` symlinks, `(link name under /dev, target)`.
pub const SEALED_DEVICE_SYMLINKS: &[(&str, &str)] = &[
    ("fd", "/proc/self/fd"),
    ("stdin", "/proc/self/fd/0"),
    ("stdout", "/proc/self/fd/1"),
    ("stderr", "/proc/self/fd/2"),
    ("ptmx", "pts/ptmx"),
];

/// Directory created inside the session workdir and bind-mounted at `/tmp` in the composed root.
///
/// `/tmp` has to exist — bash heredocs, `mktemp`, compilers and package managers all fail without
/// it in ways that read as runtime bugs rather than as policy. Backing it with a directory *inside
/// the workdir* is what keeps "the session workdir is the only writable path" literally true: the
/// bytes written to `/tmp` land in the workdir, are counted by the existing workdir-size guard
/// (`capabilities.resources.workdir_max_bytes`), and are discarded with the session.
pub const SEALED_TMP_DIR_NAME: &str = ".mur-tmp";

/// Directory the old root is parked in between `pivot_root(2)` and `umount2(MNT_DETACH)`.
/// Removed immediately afterwards, so it does not exist for any process the capsule can run.
const OLD_ROOT_NAME: &str = ".mur-oldroot";

/// Base directories the new-root `tmpfs` may be mounted over, most preferred first.
///
/// The base is overmounted inside the private namespace, so whatever the host has there becomes
/// unreachable for the rest of the setup — which is why the chosen base must not be an ancestor of
/// the session workdir (a workdir under `/tmp` is entirely normal) and must not be one of the
/// directories the composed root goes on to bind-mount from.
pub const SEALED_ROOT_BASE_CANDIDATES: &[&str] = &["/tmp", "/run", "/var/tmp", "/mnt", "/media"];

/// `hidepid` spellings tried, in order, when mounting the composed root's `/proc`.
///
/// The numeric form is the legacy parser's; `invisible` is the Linux 5.8+ spelling. A kernel that
/// accepts neither still gets a `/proc`, because a `/proc` that fails to mount breaks far more
/// than an unmasked one leaks — but the fallback is recorded here rather than hidden, and the
/// manual-verification document checks which one a given host landed on.
const PROC_HIDEPID_OPTIONS: &[&str] = &["hidepid=2", "hidepid=invisible", ""];

// ---------------------------------------------------------------- host layout (test seam)

/// What a path is on the host, as far as composing a root cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PathKind {
    Dir,
    /// Any non-directory: a regular file, a device node, a socket. All are bind-mount targets that
    /// need an empty *file* created for them rather than a directory.
    File,
    /// A symlink, with its literal (un-resolved) target.
    Symlink(PathBuf),
}

/// The host filesystem facts [`plan_composed_root`] reads. A trait purely so the planner can be
/// unit-tested against a synthetic usrmerge / non-usrmerge layout on any machine — production has
/// exactly one implementation, [`RealHostLayout`].
pub(crate) trait HostLayout {
    fn kind(&self, path: &Path) -> Option<PathKind>;
}

/// The real filesystem, read with `symlink_metadata` so a usrmerge `/bin -> usr/bin` reports as a
/// symlink rather than as the directory it points at.
pub(crate) struct RealHostLayout;

impl HostLayout for RealHostLayout {
    fn kind(&self, path: &Path) -> Option<PathKind> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        if meta.file_type().is_symlink() {
            return Some(PathKind::Symlink(std::fs::read_link(path).ok()?));
        }
        if meta.is_dir() {
            return Some(PathKind::Dir);
        }
        Some(PathKind::File)
    }
}

// ---------------------------------------------------------------- the plan

/// One filesystem operation in the composed-root plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootOp {
    /// `mkdir(path, 0755)`. `EEXIST` is success.
    MkDir(PathBuf),
    /// Create an empty regular file, to be bind-mounted over.
    MkFile(PathBuf),
    /// `symlink(target, link)` — used to reproduce the host's usrmerge shape and the OCI `/dev`
    /// symlinks.
    Symlink { target: PathBuf, link: PathBuf },
    /// `mount(source, target, MS_BIND | MS_REC)`, followed by a second
    /// `MS_REMOUNT | MS_BIND | MS_RDONLY` call when `read_only` — a single `MS_BIND | MS_RDONLY`
    /// mount does *not* produce a read-only bind, which is a kernel behaviour worth stating
    /// rather than rediscovering.
    Bind { source: PathBuf, target: PathBuf, read_only: bool },
    /// A fresh `tmpfs`.
    Tmpfs { target: PathBuf, options: &'static str },
    /// A fresh `procfs`, masked with `hidepid`.
    Proc { target: PathBuf },
    /// A fresh `devpts` instance, so the capsule can allocate its own ptys without seeing the
    /// host's.
    DevPts { target: PathBuf },
    /// `mount(NULL, target, NULL, MS_REMOUNT | MS_BIND | MS_RDONLY)` — used to seal `/dev` after
    /// its device nodes have been bound into it.
    RemountReadOnly(PathBuf),
}

/// One planned operation plus whether failing it aborts the launch.
///
/// `required: false` is the "shrink, do not fail" convention this crate already uses for optional
/// device grants in `sandbox::open_landlock_fds`: losing an optional bind makes the composed root
/// *narrower*, never wider, so refusing the launch over it would trade a working capsule for no
/// security gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootStep {
    pub(crate) op: RootOp,
    pub(crate) required: bool,
}

/// The full, host-resolved recipe for one composed root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposedRootPlan {
    /// Directory the new-root `tmpfs` is mounted over, and the directory `pivot_root` is handed.
    pub(crate) base: PathBuf,
    pub(crate) steps: Vec<RootStep>,
    /// The workdir's path *inside* the composed root. Identical to its host path by design: env
    /// vars, `PWD`, log paths and anything a previous turn recorded keep working, and the capsule
    /// never has to be told it moved.
    pub(crate) workdir_in_root: PathBuf,
}

/// Picks the directory the new-root `tmpfs` is mounted over.
///
/// Rejects any candidate that is the workdir or an ancestor of it — overmounting an ancestor of
/// the workdir would hide the very directory the plan then has to bind-mount — and any candidate
/// the host does not have. Pure apart from the injected `exists` predicate.
pub(crate) fn choose_root_base(
    workdir: &Path,
    candidates: &[&str],
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .iter()
        .map(Path::new)
        .find(|candidate| !workdir.starts_with(candidate) && exists(candidate))
        .map(Path::to_path_buf)
}

/// Builds the ordered operation list for one composed root. Pure with respect to `host`.
///
/// `extra_read_only` carries the host directories this *particular* capsule needs beyond
/// [`SEALED_RUNTIME_PATHS`] — the containing directory of each resolved `shell.allow` binary and
/// each `capabilities.shell.interpreter_runtime` directory. They are whole-directory binds, not a
/// per-file grant set: this slice must not extend the ELF-closure mechanism it exists to make
/// unnecessary. Entries already covered by a fixed path are dropped.
pub(crate) fn plan_composed_root(
    workdir: &Path,
    base: &Path,
    extra_read_only: &[PathBuf],
    host: &dyn HostLayout,
) -> ComposedRootPlan {
    let mut builder = PlanBuilder::new(base);

    // 1. The host runtime tree, read-only. A usrmerge symlink is reproduced as a symlink so the
    //    composed root has the host's shape; a real directory is bind-mounted.
    for path in SEALED_RUNTIME_PATHS
        .iter()
        .map(Path::new)
        .chain(extra_read_only.iter().map(PathBuf::as_path))
    {
        builder.mirror(path, host, /* required */ false);
    }

    // 2. The narrow /etc allowlist, read-only. Everything else under /etc simply does not exist.
    for path in SEALED_ETC_PATHS.iter().map(Path::new) {
        builder.mirror(path, host, /* required */ false);
    }

    // 3. A private /dev tmpfs carrying the OCI default device set, sealed read-only once it is
    //    populated. Device nodes are bind-mounted from the host because `mknod` of a device is
    //    refused inside a user namespace — see `SealedDevice`.
    let dev = base.join("dev");
    builder.mkdir_p(&dev);
    builder.push(
        RootOp::Tmpfs { target: dev.clone(), options: "mode=0755,size=1m" },
        true,
    );
    for device in SEALED_DEVICE_NODES {
        let source = PathBuf::from(device.path);
        if host.kind(&source).is_none() {
            continue;
        }
        let target = dev.join(device.name);
        builder.push(RootOp::MkFile(target.clone()), false);
        builder.push(RootOp::Bind { source, target, read_only: false }, false);
    }
    let pts = dev.join("pts");
    builder.push(RootOp::MkDir(pts.clone()), false);
    builder.push(RootOp::DevPts { target: pts }, false);
    for (link, target) in SEALED_DEVICE_SYMLINKS {
        builder.push(
            RootOp::Symlink { target: PathBuf::from(*target), link: dev.join(link) },
            false,
        );
    }
    builder.push(RootOp::RemountReadOnly(dev), false);

    // 4. /proc, masked with hidepid.
    let proc = base.join("proc");
    builder.push(RootOp::MkDir(proc.clone()), true);
    builder.push(RootOp::Proc { target: proc }, true);

    // 5. The session workdir, at its own absolute path, read-write — the only writable path in the
    //    composed root, and the backing store for /tmp below.
    let workdir_target = rebase(base, workdir);
    builder.mkdir_p(&workdir_target);
    builder.push(
        RootOp::Bind {
            source: workdir.to_path_buf(),
            target: workdir_target,
            read_only: false,
        },
        true,
    );

    // 6. /tmp, backed by a directory inside the workdir so it stays inside the one writable path
    //    and inside the workdir size budget.
    let tmp = base.join("tmp");
    builder.push(RootOp::MkDir(tmp.clone()), true);
    builder.push(
        RootOp::Bind {
            source: workdir.join(SEALED_TMP_DIR_NAME),
            target: tmp,
            read_only: false,
        },
        true,
    );

    ComposedRootPlan {
        base: base.to_path_buf(),
        steps: builder.steps,
        workdir_in_root: workdir.to_path_buf(),
    }
}

/// `base` + `path`, where `path` is absolute: `/tmp` + `/usr/lib` → `/tmp/usr/lib`.
fn rebase(base: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix("/").unwrap_or(path);
    base.join(relative)
}

struct PlanBuilder {
    base: PathBuf,
    steps: Vec<RootStep>,
    /// Directories already scheduled for creation, so a shared prefix (`/usr`, `/etc`) is not
    /// `mkdir`ed once per entry under it.
    made: std::collections::HashSet<PathBuf>,
}

impl PlanBuilder {
    fn new(base: &Path) -> Self {
        let mut made = std::collections::HashSet::new();
        made.insert(base.to_path_buf());
        Self { base: base.to_path_buf(), steps: Vec::new(), made }
    }

    fn push(&mut self, op: RootOp, required: bool) {
        self.steps.push(RootStep { op, required });
    }

    /// Schedules `mkdir` for every not-yet-scheduled component of `target` under the base.
    fn mkdir_p(&mut self, target: &Path) {
        let mut current = self.base.clone();
        let Ok(relative) = target.strip_prefix(&self.base) else {
            return;
        };
        for component in relative.components() {
            current = current.join(component);
            if self.made.insert(current.clone()) {
                self.steps.push(RootStep { op: RootOp::MkDir(current.clone()), required: true });
            }
        }
    }

    /// Reproduces one host path inside the composed root: a symlink as a symlink, a directory or
    /// file as a read-only bind mount. A path the host does not have contributes nothing.
    fn mirror(&mut self, source: &Path, host: &dyn HostLayout, required: bool) {
        let Some(kind) = host.kind(source) else {
            return;
        };
        let target = rebase(&self.base, source);
        if self.made.contains(&target) {
            return;
        }
        match kind {
            PathKind::Symlink(link_target) => {
                if let Some(parent) = target.parent() {
                    self.mkdir_p(parent);
                }
                self.made.insert(target.clone());
                self.push(RootOp::Symlink { target: link_target, link: target }, required);
            }
            PathKind::Dir => {
                self.mkdir_p(&target);
                self.push(
                    RootOp::Bind { source: source.to_path_buf(), target, read_only: true },
                    required,
                );
            }
            PathKind::File => {
                if let Some(parent) = target.parent() {
                    self.mkdir_p(parent);
                }
                self.made.insert(target.clone());
                self.push(RootOp::MkFile(target.clone()), required);
                self.push(
                    RootOp::Bind { source: source.to_path_buf(), target, read_only: true },
                    required,
                );
            }
        }
    }
}

// ---------------------------------------------------------------- Linux: probe + execution

/// Marker every composed-root failure message carries, so `shell.rs` can tell a `pre_exec`
/// sealed-root failure apart from a Landlock or seccomp one after reading it back off the
/// diagnostic pipe — the pipe carries one flat string, so the classification has to live in the
/// text. Declared here rather than inside the Linux submodule because the reader is
/// cross-platform code.
pub(crate) const SEALED_ROOT_FAILURE_PREFIX: &str = "sealed-root:";

#[cfg(target_os = "linux")]
pub(crate) use linux::{
    build_sealed_root_spec, construct_composed_root, probe_sealed_support, SealedRootSpec,
};

/// Non-Linux stub: nothing here can be probed, so nothing is claimed.
#[cfg(not(target_os = "linux"))]
pub(crate) fn probe_sealed_support() -> SealedProbe {
    SealedProbe::default()
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{
        ComposedRootPlan, NamespaceProbe, RootOp, RootStep, SealedProbe, OLD_ROOT_NAME,
        PROC_HIDEPID_OPTIONS, SEALED_APPARMOR_PROFILE_NAME, SEALED_ROOT_FAILURE_PREFIX,
    };

    // ------------------------------------------------------------ probe

    /// Reads AppArmor's unprivileged-userns restriction and, if it is not in the way, really
    /// creates a user+mount namespace in a forked child.
    ///
    /// Forking rather than testing in-process is required, not defensive: `unshare(CLONE_NEWUSER)`
    /// is irreversible for the calling process, and putting the whole runtime into a user
    /// namespace as a side effect of a capability check would be exactly the "probe that changes
    /// the thing it measures" mistake `probe_landlock_full_access` documents avoiding.
    pub(crate) fn probe_sealed_support() -> SealedProbe {
        SealedProbe {
            apparmor_permits_userns: apparmor_permits_userns(),
            namespace: probe_namespace(),
        }
    }

    /// `true` when AppArmor is not standing between this binary and an unprivileged user
    /// namespace.
    ///
    /// Three questions in order: is AppArmor even enabled; is its `restrict_unprivileged_userns`
    /// knob on; and — only if both — is this process confined by the shipped `mur-sealed` profile.
    /// Reading `/proc/self/attr/current` rather than the loaded-profile list is deliberate: a
    /// profile that is loaded but does not *attach* to the path `mur` was installed at helps
    /// nobody, and this asks the question that decides the outcome.
    fn apparmor_permits_userns() -> bool {
        let enabled = read_trimmed("/sys/module/apparmor/parameters/enabled");
        if !matches!(enabled.as_deref(), Some("Y") | Some("1")) {
            return true;
        }

        let restricted = read_trimmed("/sys/module/apparmor/parameters/restrict_unprivileged_userns")
            .or_else(|| read_trimmed("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"));
        if !matches!(restricted.as_deref(), Some("Y") | Some("1")) {
            return true;
        }

        read_trimmed("/proc/self/attr/current")
            .is_some_and(|current| current.starts_with(SEALED_APPARMOR_PROFILE_NAME))
    }

    fn read_trimmed(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    // Exit codes the probe child reports. Kept as small distinct integers so the parent learns the
    // *step* that failed without any IPC beyond `waitpid`.
    const PROBE_OK: i32 = 0;
    const PROBE_UNSHARE_DENIED: i32 = 1;
    const PROBE_UNSHARE_UNSUPPORTED: i32 = 2;
    const PROBE_MOUNT_DENIED: i32 = 3;
    const PROBE_MAP_DENIED: i32 = 4;

    fn probe_namespace() -> NamespaceProbe {
        // SAFETY: `fork()` from a possibly-multithreaded process is sound as long as the child
        // touches nothing but async-signal-safe primitives. The child below calls `unshare`,
        // `mount` and `_exit` and nothing else — no allocation, no locks, no stdio.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return NamespaceProbe::Unsupported;
        }
        if pid == 0 {
            // SAFETY: forked-child context, syscalls only; every branch ends in `_exit`.
            unsafe {
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    let errno = *libc::__errno_location();
                    libc::_exit(match errno {
                        libc::EPERM => PROBE_UNSHARE_DENIED,
                        _ => PROBE_UNSHARE_UNSUPPORTED,
                    });
                }
                // Rehearse the identity uid/gid mapping too, rather than stopping at
                // `unshare`. A probe that skips it is not measuring what the real construction
                // does: a host can allow the namespace and still refuse to let the process own
                // it, and finding that out at spawn time instead of at launch time is exactly
                // the surprise this probe exists to prevent.
                let uid = libc::getuid();
                let gid = libc::getgid();
                let _ = write_decimal_map(c"/proc/self/setgroups", None, 0);
                if write_decimal_map(c"/proc/self/uid_map", Some(uid), uid).is_err()
                    || write_decimal_map(c"/proc/self/gid_map", Some(gid), gid).is_err()
                {
                    libc::_exit(PROBE_MAP_DENIED);
                }

                // The first mount any composed root performs, and the one AppArmor's
                // `unprivileged_userns` profile denies: if this works, `CAP_SYS_ADMIN` is real
                // inside the new namespace.
                let rc = libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                );
                libc::_exit(if rc == 0 { PROBE_OK } else { PROBE_MOUNT_DENIED });
            }
        }

        let mut status: libc::c_int = 0;
        // SAFETY: `pid` is the child just forked; `status` is a live local.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited != pid {
            return NamespaceProbe::Unsupported;
        }
        if !libc::WIFEXITED(status) {
            return NamespaceProbe::Unsupported;
        }
        match libc::WEXITSTATUS(status) {
            PROBE_OK => NamespaceProbe::Ok,
            // A namespace this process cannot map itself into is a namespace it cannot use, and
            // the remediation is the same one `Denied` names.
            PROBE_UNSHARE_DENIED | PROBE_MAP_DENIED => NamespaceProbe::Denied,
            PROBE_MOUNT_DENIED => NamespaceProbe::MountDenied,
            _ => NamespaceProbe::Unsupported,
        }
    }

    /// Writes either the literal `deny` (when `id` is `None`) or an identity map line
    /// `"<id> <id> 1\n"`, formatting the number into a stack buffer.
    ///
    /// Hand-rolled rather than `format!` because this runs in a forked child: the parent may have
    /// been mid-allocation on another thread when `fork()` returned, so touching the allocator
    /// here can deadlock. `id` never exceeds `u32::MAX`, so ten digits is the whole range.
    unsafe fn write_decimal_map(
        path: &std::ffi::CStr,
        id: Option<libc::uid_t>,
        value: libc::uid_t,
    ) -> Result<(), ()> {
        let Some(_) = id else {
            return write_file(path, b"deny");
        };

        let mut buf = [0u8; 32];
        let mut len = 0usize;
        let write_number = |buf: &mut [u8; 32], len: &mut usize, mut n: u32| {
            let mut digits = [0u8; 10];
            let mut count = 0;
            loop {
                digits[count] = b'0' + (n % 10) as u8;
                count += 1;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            while count > 0 {
                count -= 1;
                buf[*len] = digits[count];
                *len += 1;
            }
        };

        write_number(&mut buf, &mut len, value);
        buf[len] = b' ';
        len += 1;
        write_number(&mut buf, &mut len, value);
        buf[len] = b' ';
        len += 1;
        buf[len] = b'1';
        len += 1;
        buf[len] = b'\n';
        len += 1;

        write_file(path, &buf[..len])
    }

    // ------------------------------------------------------------ spec (parent side)

    /// One planned operation, lowered to NUL-terminated C strings plus a pre-rendered label.
    ///
    /// The label is built here, in the parent, precisely so the child never has to: a failure
    /// message that names its step costs one `String` per step at spawn time and zero allocation
    /// inside `pre_exec`.
    struct CStep {
        kind: CStepKind,
        required: bool,
        label: String,
    }

    enum CStepKind {
        MkDir(CString),
        MkFile(CString),
        Symlink { target: CString, link: CString },
        Bind { source: CString, target: CString, read_only: bool },
        Tmpfs { target: CString, options: CString },
        Proc { target: CString },
        DevPts { target: CString },
        RemountReadOnly(CString),
    }

    /// Everything the child needs to build a composed root, fully resolved before `fork()`.
    pub(crate) struct SealedRootSpec {
        uid_map: Vec<u8>,
        gid_map: Vec<u8>,
        base: CString,
        /// `<base>/.mur-oldroot` — created before the pivot.
        old_root_full: CString,
        /// `.mur-oldroot`, relative, as `pivot_root(2)` wants it with `cwd == base`.
        old_root_relative: CString,
        /// `/.mur-oldroot`, as it is named after the pivot, for the detach and the rmdir.
        old_root_pivoted: CString,
        steps: Vec<CStep>,
        workdir_in_root: CString,
    }

    /// Where a composed root's construction failed. `Copy` and allocation-free by construction —
    /// it is produced inside `pre_exec`; the message is rendered by the caller, afterwards.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SealedRootFailure {
        /// Index into the spec's step list, or `usize::MAX` for one of the fixed
        /// unshare/pivot/chdir stages.
        pub(crate) step: usize,
        /// A fixed stage name when `step` is `usize::MAX`; otherwise unused.
        pub(crate) stage: &'static str,
        pub(crate) errno: i32,
    }

    impl SealedRootFailure {
        fn stage(stage: &'static str) -> Self {
            Self {
                step: usize::MAX,
                stage,
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            }
        }

        fn step(index: usize) -> Self {
            Self {
                step: index,
                stage: "",
                errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            }
        }
    }

    impl SealedRootSpec {
        /// Renders a failure into the message that reaches the operator, naming both the step and
        /// the errno. Runs in the *parent's* error path (via the diagnostic pipe) or in the
        /// child's, where an allocation is already unavoidable because the launch is over.
        pub(crate) fn describe(&self, failure: SealedRootFailure) -> String {
            let what = if failure.step == usize::MAX {
                failure.stage.to_string()
            } else {
                self.steps
                    .get(failure.step)
                    .map(|step| step.label.clone())
                    .unwrap_or_else(|| format!("step {}", failure.step))
            };
            format!(
                "{SEALED_ROOT_FAILURE_PREFIX} {what} failed: {}",
                std::io::Error::from_raw_os_error(failure.errno)
            )
        }
    }

    /// Lowers a [`ComposedRootPlan`] into the C-string form the child executes. Fails only on a
    /// path containing an interior NUL, which no real path does.
    pub(crate) fn build_sealed_root_spec(
        plan: &ComposedRootPlan,
    ) -> Result<SealedRootSpec, String> {
        // SAFETY: `getuid`/`getgid` take no arguments, dereference nothing and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

        let mut steps = Vec::with_capacity(plan.steps.len());
        for RootStep { op, required } in &plan.steps {
            let (kind, label) = match op {
                RootOp::MkDir(path) => (
                    CStepKind::MkDir(cstr(path)?),
                    format!("mkdir {}", path.display()),
                ),
                RootOp::MkFile(path) => (
                    CStepKind::MkFile(cstr(path)?),
                    format!("create {}", path.display()),
                ),
                RootOp::Symlink { target, link } => (
                    CStepKind::Symlink { target: cstr(target)?, link: cstr(link)? },
                    format!("symlink {} -> {}", link.display(), target.display()),
                ),
                RootOp::Bind { source, target, read_only } => (
                    CStepKind::Bind {
                        source: cstr(source)?,
                        target: cstr(target)?,
                        read_only: *read_only,
                    },
                    format!(
                        "bind{} {} -> {}",
                        if *read_only { " (ro)" } else { "" },
                        source.display(),
                        target.display()
                    ),
                ),
                RootOp::Tmpfs { target, options } => (
                    CStepKind::Tmpfs {
                        target: cstr(target)?,
                        options: CString::new(*options).map_err(|error| error.to_string())?,
                    },
                    format!("tmpfs on {}", target.display()),
                ),
                RootOp::Proc { target } => (
                    CStepKind::Proc { target: cstr(target)? },
                    format!("proc on {}", target.display()),
                ),
                RootOp::DevPts { target } => (
                    CStepKind::DevPts { target: cstr(target)? },
                    format!("devpts on {}", target.display()),
                ),
                RootOp::RemountReadOnly(path) => (
                    CStepKind::RemountReadOnly(cstr(path)?),
                    format!("remount read-only {}", path.display()),
                ),
            };
            steps.push(CStep { kind, required: *required, label });
        }

        Ok(SealedRootSpec {
            // Identity maps: the capsule keeps the uid it already had, so files it writes into the
            // bind-mounted workdir are owned by the real user rather than by `nobody`.
            uid_map: format!("{uid} {uid} 1\n").into_bytes(),
            gid_map: format!("{gid} {gid} 1\n").into_bytes(),
            base: cstr(&plan.base)?,
            old_root_full: cstr(&plan.base.join(OLD_ROOT_NAME))?,
            old_root_relative: CString::new(OLD_ROOT_NAME).map_err(|e| e.to_string())?,
            old_root_pivoted: CString::new(format!("/{OLD_ROOT_NAME}"))
                .map_err(|e| e.to_string())?,
            steps,
            workdir_in_root: cstr(&plan.workdir_in_root)?,
        })
    }

    fn cstr(path: &Path) -> Result<CString, String> {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("path contains an interior NUL: {}", path.display()))
    }

    // ------------------------------------------------------------ execution (child side)

    /// Builds the composed root and pivots onto it. Runs inside the forked child's `pre_exec`
    /// window, before fd hygiene, the capability drop, seccomp and Landlock — which is the only
    /// window in which `unshare`/`mount`/`pivot_root` are available at all, since
    /// `sandbox::SECCOMP_MUST_STAY_DENIED` denies all three to every process the filter covers.
    ///
    /// Allocation-free on the success path: every path was turned into a `CString` by
    /// [`build_sealed_root_spec`] before `fork()`.
    pub(crate) fn construct_composed_root(
        spec: &SealedRootSpec,
    ) -> Result<(), SealedRootFailure> {
        // SAFETY: every call below is a bare syscall over pointers into `spec`, which outlives
        // this function. No allocation, no locks, no reentrancy — the constraints of the
        // post-fork/pre-exec window.
        unsafe {
            // 1. The namespaces. `CLONE_NEWUSER` is what makes `CLONE_NEWNS` available without
            //    host root; asking for both in one call means the mount namespace is created with
            //    the new user namespace's credentials already in force.
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(SealedRootFailure::stage("unshare(CLONE_NEWUSER|CLONE_NEWNS)"));
            }

            // 2. Identity uid/gid maps. `setgroups=deny` first, which the kernel requires before
            //    an unprivileged process may write `gid_map`. A host without `setgroups` (pre-3.19)
            //    is tolerated; a failing `uid_map` is not, because running as the overflow uid
            //    would leave the capsule unable to write its own workdir.
            let _ = write_file(c"/proc/self/setgroups", b"deny");
            if write_file(c"/proc/self/uid_map", &spec.uid_map).is_err() {
                return Err(SealedRootFailure::stage("write /proc/self/uid_map"));
            }
            if write_file(c"/proc/self/gid_map", &spec.gid_map).is_err() {
                return Err(SealedRootFailure::stage("write /proc/self/gid_map"));
            }

            // 3. Private propagation, so none of what follows escapes back to the host's mounts.
            if libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                return Err(SealedRootFailure::stage("mount --make-rprivate /"));
            }

            // 4. The new root: an empty tmpfs over the chosen base. It is a mount point, which is
            //    what `pivot_root(2)` requires of its new root.
            if libc::mount(
                c"tmpfs".as_ptr(),
                spec.base.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV,
                c"mode=0755".as_ptr() as *const libc::c_void,
            ) != 0
            {
                return Err(SealedRootFailure::stage("mount tmpfs for the composed root"));
            }

            // 5. The plan.
            for (index, step) in spec.steps.iter().enumerate() {
                if execute_step(step).is_err() && step.required {
                    return Err(SealedRootFailure::step(index));
                }
            }

            // 6. Pivot. `chdir` into the new root first so `pivot_root` can be handed the pair of
            //    relative paths the syscall is happiest with.
            if libc::mkdir(spec.old_root_full.as_ptr(), 0o700) != 0 {
                return Err(SealedRootFailure::stage("mkdir the old-root parking directory"));
            }
            if libc::chdir(spec.base.as_ptr()) != 0 {
                return Err(SealedRootFailure::stage("chdir into the composed root"));
            }
            if libc::syscall(
                libc::SYS_pivot_root,
                c".".as_ptr(),
                spec.old_root_relative.as_ptr(),
            ) != 0
            {
                return Err(SealedRootFailure::stage("pivot_root"));
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(SealedRootFailure::stage("chdir / after pivot_root"));
            }

            // 7. Detach and remove the old root. After this there is no pathname anywhere in this
            //    namespace that reaches the host filesystem.
            if libc::umount2(spec.old_root_pivoted.as_ptr(), libc::MNT_DETACH) != 0 {
                return Err(SealedRootFailure::stage("umount2 the old root"));
            }
            // Best-effort, deliberately: `MNT_DETACH` already removed the old root from this
            // namespace's tree, so a leftover empty directory here is cosmetic. Failing a launch
            // over it would trade a working capsule for nothing.
            libc::rmdir(spec.old_root_pivoted.as_ptr());

            // 8. Seal the root tmpfs itself. `MS_REMOUNT | MS_BIND` changes per-mount flags only,
            //    which is the well-defined way to make an existing mount read-only; the workdir
            //    bind underneath it keeps its own (writable) flags.
            if libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID
                    | libc::MS_NODEV,
                std::ptr::null(),
            ) != 0
            {
                return Err(SealedRootFailure::stage("remount the composed root read-only"));
            }

            // 9. Back into the workdir, at the same absolute path it had on the host. `Command`
            //    already `chdir`ed here before this closure ran, but that was in the old root.
            if libc::chdir(spec.workdir_in_root.as_ptr()) != 0 {
                return Err(SealedRootFailure::stage("chdir into the workdir inside the root"));
            }
        }

        Ok(())
    }

    /// Executes one planned step. `Err(())` carries no detail — the caller pairs the index with
    /// the spec's pre-rendered label and the live errno.
    unsafe fn execute_step(step: &CStep) -> Result<(), ()> {
        match &step.kind {
            CStepKind::MkDir(path) => {
                if libc::mkdir(path.as_ptr(), 0o755) != 0
                    && *libc::__errno_location() != libc::EEXIST
                {
                    return Err(());
                }
            }
            CStepKind::MkFile(path) => {
                let fd = libc::open(
                    path.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                    libc::mode_t::from(0o644u16),
                );
                if fd < 0 {
                    return Err(());
                }
                libc::close(fd);
            }
            CStepKind::Symlink { target, link } => {
                if libc::symlink(target.as_ptr(), link.as_ptr()) != 0 {
                    return Err(());
                }
            }
            CStepKind::Bind { source, target, read_only } => {
                if libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND | libc::MS_REC,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(());
                }
                if *read_only
                    && libc::mount(
                        std::ptr::null(),
                        target.as_ptr(),
                        std::ptr::null(),
                        libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID,
                        std::ptr::null(),
                    ) != 0
                {
                    return Err(());
                }
            }
            CStepKind::Tmpfs { target, options } => {
                if libc::mount(
                    c"tmpfs".as_ptr(),
                    target.as_ptr(),
                    c"tmpfs".as_ptr(),
                    libc::MS_NOSUID,
                    options.as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(());
                }
            }
            CStepKind::Proc { target } => {
                // Try each `hidepid` spelling in turn. The unmasked fallback is the last entry,
                // and it is a fallback rather than a failure because a capsule with no `/proc` at
                // all breaks in far more ways than one with an unmasked `/proc` leaks.
                for option in PROC_HIDEPID_OPTIONS {
                    let mut buffer = [0u8; 32];
                    let bytes = option.as_bytes();
                    if bytes.len() >= buffer.len() {
                        continue;
                    }
                    buffer[..bytes.len()].copy_from_slice(bytes);
                    let data = if bytes.is_empty() {
                        std::ptr::null()
                    } else {
                        buffer.as_ptr() as *const libc::c_void
                    };
                    if libc::mount(
                        c"proc".as_ptr(),
                        target.as_ptr(),
                        c"proc".as_ptr(),
                        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                        data,
                    ) == 0
                    {
                        return Ok(());
                    }
                }
                return Err(());
            }
            CStepKind::DevPts { target } => {
                if libc::mount(
                    c"devpts".as_ptr(),
                    target.as_ptr(),
                    c"devpts".as_ptr(),
                    libc::MS_NOSUID | libc::MS_NOEXEC,
                    c"newinstance,ptmxmode=0666,mode=0620".as_ptr() as *const libc::c_void,
                ) != 0
                {
                    return Err(());
                }
            }
            CStepKind::RemountReadOnly(target) => {
                if libc::mount(
                    std::ptr::null(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID,
                    std::ptr::null(),
                ) != 0
                {
                    return Err(());
                }
            }
        }
        Ok(())
    }

    /// `open`/`write`/`close` over a static path, with no allocation — the `/proc/self/*_map`
    /// writes are the only file I/O the composed-root construction performs.
    unsafe fn write_file(path: &std::ffi::CStr, data: &[u8]) -> Result<(), ()> {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(());
        }
        let mut written = 0usize;
        while written < data.len() {
            let n = libc::write(
                fd,
                data[written..].as_ptr() as *const libc::c_void,
                data.len() - written,
            );
            if n < 0 {
                if *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                libc::close(fd);
                return Err(());
            }
            if n == 0 {
                break;
            }
            written += n as usize;
        }
        libc::close(fd);
        if written == data.len() {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeHost(HashMap<PathBuf, PathKind>);

    impl FakeHost {
        fn usrmerge() -> Self {
            let mut map = HashMap::new();
            map.insert(PathBuf::from("/usr"), PathKind::Dir);
            map.insert(PathBuf::from("/bin"), PathKind::Symlink(PathBuf::from("usr/bin")));
            map.insert(PathBuf::from("/sbin"), PathKind::Symlink(PathBuf::from("usr/sbin")));
            map.insert(PathBuf::from("/lib"), PathKind::Symlink(PathBuf::from("usr/lib")));
            map.insert(PathBuf::from("/lib64"), PathKind::Symlink(PathBuf::from("usr/lib64")));
            map.insert(PathBuf::from("/etc/ld.so.cache"), PathKind::File);
            map.insert(PathBuf::from("/etc/ssl"), PathKind::Dir);
            map.insert(PathBuf::from("/etc/passwd"), PathKind::File);
            for device in SEALED_DEVICE_NODES {
                map.insert(PathBuf::from(device.path), PathKind::File);
            }
            Self(map)
        }
    }

    impl HostLayout for FakeHost {
        fn kind(&self, path: &Path) -> Option<PathKind> {
            self.0.get(path).cloned()
        }
    }

    fn plan_for(workdir: &str) -> ComposedRootPlan {
        plan_composed_root(
            Path::new(workdir),
            Path::new("/tmp"),
            &[],
            &FakeHost::usrmerge(),
        )
    }

    fn ops(plan: &ComposedRootPlan) -> Vec<RootOp> {
        plan.steps.iter().map(|step| step.op.clone()).collect()
    }

    #[test]
    fn base_selection_skips_any_ancestor_of_the_workdir() {
        // The default case: a workdir under the user's home, so `/tmp` is free.
        assert_eq!(
            choose_root_base(Path::new("/home/u/w"), SEALED_ROOT_BASE_CANDIDATES, |_| true),
            Some(PathBuf::from("/tmp"))
        );
        // A workdir under /tmp — overmounting /tmp would hide the workdir before it can be bound.
        assert_eq!(
            choose_root_base(Path::new("/tmp/session/w"), SEALED_ROOT_BASE_CANDIDATES, |_| true),
            Some(PathBuf::from("/run"))
        );
        // A host missing the first two candidates falls through to the third.
        assert_eq!(
            choose_root_base(Path::new("/home/u/w"), SEALED_ROOT_BASE_CANDIDATES, |path| {
                path != Path::new("/tmp") && path != Path::new("/run")
            }),
            Some(PathBuf::from("/var/tmp"))
        );
        assert_eq!(
            choose_root_base(Path::new("/home/u/w"), SEALED_ROOT_BASE_CANDIDATES, |_| false),
            None
        );
    }

    #[test]
    fn usrmerge_symlinks_are_reproduced_rather_than_bound_through() {
        let plan = plan_for("/home/u/w");
        assert!(ops(&plan).contains(&RootOp::Symlink {
            target: PathBuf::from("usr/bin"),
            link: PathBuf::from("/tmp/bin"),
        }));
        assert!(ops(&plan).contains(&RootOp::Bind {
            source: PathBuf::from("/usr"),
            target: PathBuf::from("/tmp/usr"),
            read_only: true,
        }));
        // `/lib32` and `/libx32` are absent from this fake host, so they contribute nothing.
        assert!(!ops(&plan)
            .iter()
            .any(|op| matches!(op, RootOp::Bind { source, .. } if source == Path::new("/lib32"))));
    }

    #[test]
    fn every_runtime_and_etc_bind_is_read_only_and_only_the_workdir_is_not() {
        let plan = plan_for("/home/u/w");
        let writable: Vec<&PathBuf> = plan
            .steps
            .iter()
            .filter_map(|step| match &step.op {
                RootOp::Bind { source, read_only: false, .. } => Some(source),
                _ => None,
            })
            .collect();

        // The workdir, /tmp's workdir-backed store, and the /dev nodes (whose write side is the
        // device driver's, not the filesystem's) — and nothing else.
        for source in &writable {
            assert!(
                source.starts_with("/home/u/w") || source.starts_with("/dev/"),
                "unexpected writable bind of {}",
                source.display()
            );
        }
        assert!(writable.contains(&&PathBuf::from("/home/u/w")));
    }

    #[test]
    fn etc_is_an_allowlist_so_shadow_is_absent_by_construction() {
        let plan = plan_for("/home/u/w");
        let bound: Vec<String> = plan
            .steps
            .iter()
            .filter_map(|step| match &step.op {
                RootOp::Bind { source, .. } => Some(source.display().to_string()),
                _ => None,
            })
            .collect();

        assert!(bound.iter().any(|path| path == "/etc/ld.so.cache"));
        assert!(!bound.iter().any(|path| path == "/etc"));
        assert!(!bound.iter().any(|path| path.starts_with("/etc/shadow")));
        assert!(!bound.iter().any(|path| path.starts_with("/root")));
        assert!(!bound.iter().any(|path| path.starts_with("/home/u/.ssh")));
    }

    #[test]
    fn the_workdir_keeps_its_absolute_path_inside_the_root() {
        let plan = plan_for("/home/u/.murmur/sessions/abc");
        assert_eq!(plan.workdir_in_root, PathBuf::from("/home/u/.murmur/sessions/abc"));
        assert!(ops(&plan).contains(&RootOp::Bind {
            source: PathBuf::from("/home/u/.murmur/sessions/abc"),
            target: PathBuf::from("/tmp/home/u/.murmur/sessions/abc"),
            read_only: false,
        }));
        // Every parent component is created before the bind, exactly once.
        let mkdirs: Vec<&RootOp> = plan
            .steps
            .iter()
            .map(|step| &step.op)
            .filter(|op| matches!(op, RootOp::MkDir(path) if path.starts_with("/tmp/home")))
            .collect();
        assert_eq!(mkdirs.len(), 5, "got {mkdirs:?}");
    }

    #[test]
    fn tmp_is_backed_by_a_directory_inside_the_workdir() {
        let plan = plan_for("/home/u/w");
        assert!(ops(&plan).contains(&RootOp::Bind {
            source: PathBuf::from("/home/u/w").join(SEALED_TMP_DIR_NAME),
            target: PathBuf::from("/tmp/tmp"),
            read_only: false,
        }));
    }

    #[test]
    fn dev_carries_the_oci_default_set_and_is_sealed_afterwards() {
        let plan = plan_for("/home/u/w");
        let operations = ops(&plan);

        for device in SEALED_DEVICE_NODES {
            assert!(
                operations.contains(&RootOp::Bind {
                    source: PathBuf::from("/dev").join(device.name),
                    target: PathBuf::from("/tmp/dev").join(device.name),
                    read_only: false,
                }),
                "missing /dev/{}",
                device.name
            );
        }
        // No block device, and nothing outside the OCI default set.
        assert!(!operations
            .iter()
            .any(|op| matches!(op, RootOp::Bind { source, .. } if source == Path::new("/dev/sda"))));

        let tmpfs_index = operations
            .iter()
            .position(|op| matches!(op, RootOp::Tmpfs { target, .. } if target == Path::new("/tmp/dev")))
            .expect("/dev tmpfs");
        let sealed_index = operations
            .iter()
            .position(|op| matches!(op, RootOp::RemountReadOnly(path) if path == Path::new("/tmp/dev")))
            .expect("/dev sealed read-only");
        assert!(tmpfs_index < sealed_index, "the device nodes must be bound before /dev is sealed");
    }

    #[test]
    fn proc_is_mounted_and_masked() {
        let plan = plan_for("/home/u/w");
        assert!(ops(&plan).contains(&RootOp::Proc { target: PathBuf::from("/tmp/proc") }));
        assert_eq!(PROC_HIDEPID_OPTIONS[0], "hidepid=2");
    }

    #[test]
    fn manifest_derived_directories_are_whole_directory_binds_and_deduped() {
        let mut host = FakeHost::usrmerge();
        host.0.insert(PathBuf::from("/opt/python3.12"), PathKind::Dir);
        let plan = plan_composed_root(
            Path::new("/home/u/w"),
            Path::new("/tmp"),
            &[PathBuf::from("/opt/python3.12"), PathBuf::from("/usr")],
            &host,
        );
        let operations = ops(&plan);

        assert!(operations.contains(&RootOp::Bind {
            source: PathBuf::from("/opt/python3.12"),
            target: PathBuf::from("/tmp/opt/python3.12"),
            read_only: true,
        }));
        // `/usr` was already bound by the fixed list; naming it again adds nothing.
        assert_eq!(
            operations
                .iter()
                .filter(|op| matches!(op, RootOp::Bind { source, .. } if source == Path::new("/usr")))
                .count(),
            1
        );
    }

    #[test]
    fn blocker_blames_apparmor_before_the_namespace_outcome_it_causes() {
        let probe = SealedProbe {
            apparmor_permits_userns: false,
            namespace: NamespaceProbe::Denied,
        };
        assert_eq!(
            sealed_blocker(true, true, probe),
            Some(SealedBlocker::AppArmorProfileMissing)
        );
        let reason = SealedBlocker::AppArmorProfileMissing.reason();
        assert!(reason.contains("mur-sealed"));
        assert!(reason.contains("apparmor_parser -r"));
    }

    #[test]
    fn blocker_names_the_container_remediation_verbatim() {
        let probe = SealedProbe {
            apparmor_permits_userns: true,
            namespace: NamespaceProbe::Denied,
        };
        assert_eq!(
            sealed_blocker(true, true, probe),
            Some(SealedBlocker::NamespaceCreationDenied)
        );
        let reason = SealedBlocker::NamespaceCreationDenied.reason();
        assert!(reason.contains("--cap-add SYS_ADMIN"));
        assert!(reason.contains("outside the container"));
    }

    #[test]
    fn blocker_is_none_only_when_every_precondition_holds() {
        let ok = SealedProbe {
            apparmor_permits_userns: true,
            namespace: NamespaceProbe::Ok,
        };
        assert_eq!(sealed_blocker(true, true, ok), None);
        assert_eq!(sealed_blocker(false, true, ok), Some(SealedBlocker::NotLinux));
        assert_eq!(
            sealed_blocker(true, false, ok),
            Some(SealedBlocker::LandlockUnavailable)
        );
        assert_eq!(
            sealed_blocker(
                true,
                true,
                SealedProbe { apparmor_permits_userns: true, namespace: NamespaceProbe::MountDenied }
            ),
            Some(SealedBlocker::MountDenied)
        );
        assert_eq!(
            sealed_blocker(
                true,
                true,
                SealedProbe { apparmor_permits_userns: true, namespace: NamespaceProbe::Unsupported }
            ),
            Some(SealedBlocker::KernelUnsupported)
        );
    }

    #[test]
    fn every_blocker_reason_names_a_mechanism_and_stays_one_paragraph() {
        for blocker in [
            SealedBlocker::NotLinux,
            SealedBlocker::AppArmorProfileMissing,
            SealedBlocker::NamespaceCreationDenied,
            SealedBlocker::MountDenied,
            SealedBlocker::KernelUnsupported,
            SealedBlocker::LandlockUnavailable,
        ] {
            let reason = blocker.reason();
            assert!(reason.starts_with("sealed "), "got: {reason}");
            assert!(!reason.contains('\n'), "got: {reason}");
        }
    }

    #[test]
    fn the_device_set_is_exactly_the_oci_default_and_excludes_shm() {
        let names: Vec<&str> = SEALED_DEVICE_NODES.iter().map(|d| d.name).collect();
        assert_eq!(names, ["null", "zero", "full", "random", "urandom", "tty"]);
        assert!(!names.contains(&"shm"));
        assert!(!names.contains(&"console"));
        // Every entry records the canonical major/minor the OCI spec names.
        assert_eq!(
            SEALED_DEVICE_NODES
                .iter()
                .map(|d| (d.path, d.major, d.minor, d.mode, d.writable))
                .collect::<Vec<_>>(),
            [
                ("/dev/null", 1, 3, 0o666, true),
                ("/dev/zero", 1, 5, 0o666, true),
                ("/dev/full", 1, 7, 0o666, true),
                ("/dev/random", 1, 8, 0o666, false),
                ("/dev/urandom", 1, 9, 0o666, false),
                ("/dev/tty", 5, 0, 0o666, true),
            ]
        );
    }
}
