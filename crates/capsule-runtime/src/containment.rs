//! Containment classes: what an operator *declares* versus what a session actually *achieves*.
//!
//! The two directions are deliberately kept apart:
//!
//! * The **declared floor** ([`murmur_artifact::ContainmentClass`]) is a requirement, combined
//!   from the manifest, the workspace config and `--containment` by taking the strongest (see
//!   [`murmur_artifact::effective_containment_floor`]).
//! * The **achieved class** is the class of the tier the session's commands run on. That tier,
//!   and the class read off it, are one [`ContainmentDecision`], made once per session from one
//!   [`HostProbe`] reading and the declared floor. The host's tier is a ceiling the declaration
//!   can only lower: a capsule declaring less than `sealed` on a sealed-capable host runs on
//!   [`EnforcementTier::KernelFull`] and achieves `scoped`, and no declaration raises a tier
//!   above what the host backs. [`achieved_class_for_tier`] takes nothing but a tier, so no
//!   declaration can talk a tier into reporting a class it does not back. Two capsule-side facts
//!   then cap the class at [`ContainmentClass::Advisory`] without changing the tier:
//!   `capabilities.filesystem.workdir_exec: true` and a runtime directory inside the workdir (see
//!   [`achieved_containment_class`]).
//!
//! What the host could back for a capsule declaring `sealed` is reported separately, as
//! [`ContainmentDecision::host_containment`], and decides nothing.
//!
//! [`ContainmentClass::Sealed`] is reachable exactly when [`EnforcementTier::KernelSealed`] is
//! applied: a Linux host with a usable Landlock ABI and a namespace probe that really built the
//! composed root inside the capsule's network namespace, and a capsule declaring `sealed`. See
//! [`crate::sealed`] for the mechanism and for the [`crate::sealed::SealedBlocker`] taxonomy that
//! turns "not sealed here" into a specific command an operator can run.
//!
//! The shortfall reason for `sealed` is therefore *mechanism-specific*, not a fixed string: a
//! host missing the AppArmor profile and a container missing `CAP_SYS_ADMIN` both fail to reach
//! `sealed`, but they are fixed in completely different places, and a refusal that cannot tell
//! them apart is a refusal an operator cannot act on.

use murmur_artifact::{ContainmentClass, ExportMode, Exports, FileExport, PeerFilesExport};
use serde::Serialize;

use crate::{
    cgroup::{CgroupScopeReport, CgroupScopeStatus, IoMaxReport, IoMaxStatus},
    errors::RuntimeError,
    runtime_dir::RuntimeDirPlacement,
    sandbox::{EnforcementTier, HostProbe},
    sealed::{SealedShortfall, UsernsGrant},
    types::CapabilityPolicy,
};

pub use crate::sandbox::{detect_sealed_shortfall, detect_userns_grant};

/// The class a session whose commands run on `tier` achieves, before any capsule-side cap.
///
/// Pure and tier-only, so this mapping stays unit-testable on any OS and no declaration can make a
/// tier report a class it does not back.
pub(crate) fn achieved_class_for_tier(tier: EnforcementTier) -> ContainmentClass {
    match tier {
        // A private mount namespace pivoted onto a composed root: paths outside it are absent,
        // not merely denied — exactly the mechanism `sealed` names. Landlock and seccomp still
        // install inside it, so this arm is strictly stronger than `KernelFull`'s, never an
        // alternative to it.
        EnforcementTier::KernelSealed => ContainmentClass::Sealed,
        // Landlock mediates the filesystem and seccomp mediates exec/network: exactly the
        // mechanism `scoped` names.
        EnforcementTier::KernelFull => ContainmentClass::Scoped,
        // Seccomp alone leaves the filesystem scope convention-only, and `EnvironmentOnly` has
        // no kernel primitive at all. Neither can back a filesystem claim, so neither clears
        // `scoped`.
        EnforcementTier::KernelSeccompOnly | EnforcementTier::EnvironmentOnly => {
            ContainmentClass::Advisory
        }
        // The operator's own stance: nothing is installed, and the class says so.
        EnforcementTier::Unconfined => ContainmentClass::Unconfined,
    }
}

/// The class a session achieves: the class of the tier its commands run on, from
/// [`achieved_class_for_tier`], capped by what the capsule's own
/// `capabilities.filesystem.workdir_exec` leaves standing and by where its runtime directory lies.
///
/// Both caps lower the class without changing the tier: the commands still run under the same
/// mechanism, which no longer backs the claim the higher class makes.
///
/// Why `workdir_exec` caps at [`ContainmentClass::Advisory`]: `scoped` (and `sealed` above it) both
/// claim that what runs inside the capsule is bounded by what the operator declared. With the
/// workdir's `Execute` right granted, a binary the agent compiles, downloads or renames inside its
/// own workdir runs whatever `capabilities.shell.allow` says — the allowlist stops being an
/// enforceable property. A tier that backs `scoped` still cannot back a claim the capsule itself
/// has given up, so the honest report is `advisory`, on every tier.
///
/// Why `runtime_dir_inside_workdir` caps at [`ContainmentClass::Advisory`] too: with
/// `~/.murmur/sessions` inside the workdir the capsule's commands write, they can rewrite the
/// task, `MURMUR.md`, a submitted plan or an installed tool's manifest, which every class above
/// `advisory` claims they cannot.
fn achieved_containment_class(
    tier: EnforcementTier,
    workdir_exec: bool,
    runtime_dir_inside_workdir: bool,
) -> ContainmentClass {
    let class = achieved_class_for_tier(tier);
    if workdir_exec || runtime_dir_inside_workdir {
        class.min(ContainmentClass::Advisory)
    } else {
        class
    }
}

/// A session's containment, decided once: the tier its commands run on and the class that tier
/// achieves for this capsule.
///
/// Every reader of a session's class or tier takes it from here — the floor refusal, the
/// [`ScopeReport`] (and so `--explain-scope` and `session_start`), the `/tmp` mapping, the
/// `MURMUR.md` exec facts, the tier `ShellEnforcement` installs, the resource plane's and peer
/// handoff's symlink rule, `mur doctor` and the escape-conformance harness — so no report can name
/// a class or tier the session's commands do not run under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainmentDecision {
    declared: ContainmentClass,
    host_tier: EnforcementTier,
    applied_tier: EnforcementTier,
    achieved: ContainmentClass,
}

impl ContainmentDecision {
    /// Decides from one host tier reading, the combined declared floor, the capsule's
    /// `capabilities.filesystem.workdir_exec` and whether its runtime directory lies inside the
    /// workdir. The last two cap the class and never change the tier.
    pub(crate) fn decide(
        host_tier: EnforcementTier,
        declared: ContainmentClass,
        workdir_exec: bool,
        runtime_dir_inside_workdir: bool,
    ) -> Self {
        let applied_tier = crate::sandbox::applied_tier(host_tier, declared);
        Self {
            declared,
            host_tier,
            applied_tier,
            achieved: achieved_containment_class(
                applied_tier,
                workdir_exec,
                runtime_dir_inside_workdir,
            ),
        }
    }

    /// The combined declared floor the decision was made for.
    pub fn declared(&self) -> ContainmentClass {
        self.declared
    }

    /// The class this session's commands run under.
    pub fn achieved(&self) -> ContainmentClass {
        self.achieved
    }

    /// The class the host could back for a capsule declaring `sealed`, with no capsule-side cap.
    /// Reported only; nothing decides from it.
    pub fn host_containment(&self) -> ContainmentClass {
        achieved_class_for_tier(self.host_tier)
    }

    /// The wire name of the tier this session installs, as [`ScopeReport::enforcement_tier`]
    /// spells it.
    pub fn enforcement_tier(&self) -> &'static str {
        enforcement_tier_name(self.applied_tier)
    }

    /// The tier this session installs. `pub(crate)` because [`EnforcementTier`] is.
    pub(crate) fn applied_tier(&self) -> EnforcementTier {
        self.applied_tier
    }
}

/// Probes this host once and decides for a capsule declaring `declared`, with neither capsule-side
/// cap, returning the probe's sealed shortfall beside it.
///
/// What `mur doctor`'s callers and the escape-conformance harness ask: the class and tier a
/// session declaring `declared` would run under here, and why `sealed` is out of reach when it is.
/// A *session* takes its own [`HostProbe`] in `runtime::stage_session` and decides with its real
/// `workdir_exec` and runtime-directory placement instead.
pub fn detect_containment_decision(
    declared: ContainmentClass,
) -> (ContainmentDecision, Option<SealedShortfall>) {
    let probe = HostProbe::probe();
    (
        ContainmentDecision::decide(probe.tier(), declared, false, false),
        probe.sealed_shortfall(),
    )
}

/// Why `achieved` falls short of `declared`, naming the missing *mechanism* rather than the
/// missing class. `None` when the floor is met.
///
/// Pure and host-independent: the caller supplies both classes *and* — for the `sealed` arm — the
/// already-probed [`SealedShortfall`], so this function still probes nothing and stays testable
/// without a kernel. `sealed_shortfall` is `None` when the caller has not probed, in which case
/// the arm falls back to naming the mechanism generically.
///
/// `workdir_exec` is the capsule's own `capabilities.filesystem.workdir_exec`. It is checked before
/// every host arm below, because it is the one shortfall an operator cannot fix by changing hosts:
/// no kernel backs `scoped` for a capsule that has declared its workdir executable. Naming a
/// missing Landlock ABI to someone whose real problem is a line in their own manifest sends them to
/// the wrong machine.
///
/// `runtime_dir_inside` is the placement when the session's runtime directory lies inside the
/// workdir, checked right after `workdir_exec` and for the same reason: the fix is a different
/// `--workdir`, not a different host.
pub fn containment_shortfall_reason(
    declared: ContainmentClass,
    achieved: ContainmentClass,
    sealed_shortfall: Option<&SealedShortfall>,
    workdir_exec: bool,
    runtime_dir_inside: Option<&RuntimeDirPlacement>,
) -> Option<String> {
    containment_shortfall(
        declared,
        achieved,
        sealed_shortfall,
        workdir_exec,
        runtime_dir_inside,
    )
    .map(|(reason, _)| reason)
}

/// Why a containment floor is unmet, which decides what an operator is told to change: a host
/// that cannot back the class, or one of the two capsule-side causes no host can fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortfallCause {
    /// This host lacks the mechanism the declared class needs.
    Host,
    /// `capabilities.filesystem.workdir_exec: true` caps the class at `advisory`.
    WorkdirExec,
    /// The runtime or records directory lies inside the workdir, which caps the class at
    /// `advisory`.
    InsideWorkdir,
}

/// [`containment_shortfall_reason`], with the [`ShortfallCause`] it was decided by.
fn containment_shortfall(
    declared: ContainmentClass,
    achieved: ContainmentClass,
    sealed_shortfall: Option<&SealedShortfall>,
    workdir_exec: bool,
    runtime_dir_inside: Option<&RuntimeDirPlacement>,
) -> Option<(String, ShortfallCause)> {
    if achieved >= declared {
        return None;
    }

    if workdir_exec && declared > ContainmentClass::Advisory {
        return Some((
            WORKDIR_EXEC_SHORTFALL.to_string(),
            ShortfallCause::WorkdirExec,
        ));
    }

    if let Some(placement) = runtime_dir_inside.filter(|_| declared > ContainmentClass::Advisory) {
        return Some((placement.inside_shortfall(), ShortfallCause::InsideWorkdir));
    }

    let reason = match declared {
        // Unreachable: every class meets an unconfined floor, so `achieved >= declared` above
        // has already returned.
        ContainmentClass::Unconfined => {
            "unconfined declares no floor and is met by every session".to_string()
        }
        // Unreachable in practice — `Advisory` is the weakest class, so nothing is below it —
        // but stated rather than `unreachable!()` so a future variant reordering cannot panic
        // a launch path.
        ContainmentClass::Advisory => {
            "advisory is the weakest containment class and is satisfied by every host".to_string()
        }
        ContainmentClass::Scoped => {
            "scoped requires Landlock filesystem mediation (Linux 5.13+ with a usable Landlock \
             ABI); this host provides no kernel filesystem mediation, so paths outside the \
             workdir are constrained by convention only"
                .to_string()
        }
        // The mechanism exists now, so the reason names *which part of it* this host is missing
        // and how to fix that part — never the old blanket "no host can provide it today".
        ContainmentClass::Sealed => match sealed_shortfall {
            Some(shortfall) => shortfall.reason(),
            None => "sealed requires a private mount namespace pivoted onto a composed root \
                     (unshare(CLONE_NEWUSER|CLONE_NEWNS) + pivot_root), which this host did not \
                     provide"
                .to_string(),
        },
    };

    Some((reason, ShortfallCause::Host))
}

/// The reason a capsule that declared `capabilities.filesystem.workdir_exec: true` falls short of
/// any floor above `advisory`. Fixed text rather than a `match` arm per declared class: the
/// mechanism, the consequence and the remedy are identical for `scoped` and `sealed`, because both
/// rest on the claim this declaration gives up.
const WORKDIR_EXEC_SHORTFALL: &str =
    "capabilities.filesystem.workdir_exec: true keeps the Landlock Execute right on the session \
     workdir, so a binary the capsule compiles, downloads or renames inside it runs regardless of \
     capabilities.shell.allow — the allowlist stops being an enforceable property of this capsule. \
     No host can back a class above advisory for it. Either remove workdir_exec (the allowlist is \
     then enforced by the kernel on the resolved path) or lower the declared containment floor to \
     advisory";

/// The refusal gate: `Ok(())` when the session's `achieved` class
/// ([`ContainmentDecision::achieved`]) meets or exceeds the `declared` floor,
/// [`RuntimeError::ContainmentFloorUnmet`] otherwise.
///
/// Pure — takes every input as an argument and probes nothing, so the whole decision is
/// testable without a kernel.
pub fn check_containment_floor(
    declared: ContainmentClass,
    achieved: ContainmentClass,
    sealed_shortfall: Option<&SealedShortfall>,
    workdir_exec: bool,
    runtime_dir_inside: Option<&RuntimeDirPlacement>,
) -> Result<(), RuntimeError> {
    match containment_shortfall(
        declared,
        achieved,
        sealed_shortfall,
        workdir_exec,
        runtime_dir_inside,
    ) {
        None => Ok(()),
        Some((reason, cause)) => Err(RuntimeError::ContainmentFloorUnmet {
            declared,
            achieved,
            reason,
            cause,
            required_by: Vec::new(),
        }),
    }
}

/// The declared `exports.files` block as it appears in a [`ScopeReport`] — and therefore in
/// `mur run --explain-scope --json` and in `trace.jsonl`'s `session_start.effective_grants`.
///
/// A flattened copy rather than [`FileExport`] itself: this is a wire shape with a stable field
/// order and a `mode` that serializes as `"read-only"`, and it must not move whenever the
/// manifest type gains a field the report has no business publishing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportsFilesReport {
    /// `exports.files.root` verbatim, as declared.
    pub root: String,
    pub mode: ExportMode,
    /// The effective per-file read ceiling, with the default already applied.
    pub max_bytes: u64,
}

impl From<&FileExport> for ExportsFilesReport {
    fn from(export: &FileExport) -> Self {
        Self {
            root: export.root.clone(),
            mode: export.mode,
            max_bytes: export.max_bytes,
        }
    }
}

/// The declared `exports.peer_files` block as it appears in a [`ScopeReport`].
///
/// A flattened copy rather than [`PeerFilesExport`] itself, on the same terms as
/// [`ExportsFilesReport`]: a wire shape with a stable field order, which must not move whenever
/// the manifest type gains a field the report has no business publishing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerFilesReport {
    /// `exports.peer_files.root` verbatim, as declared.
    pub root: String,
    /// The effective handle-lifetime ceiling in seconds, with the ephemeral default already
    /// applied — so an operator reads the value the runtime will actually clamp to, not the
    /// absence of a declaration.
    pub max_ttl_secs: u64,
    /// The effective per-file ceiling on a redeemed read, with the default already applied.
    pub max_bytes: u64,
}

impl From<&PeerFilesExport> for PeerFilesReport {
    fn from(export: &PeerFilesExport) -> Self {
        Self {
            root: export.root.clone(),
            max_ttl_secs: export.effective_max_ttl_secs(),
            max_bytes: export.max_bytes,
        }
    }
}

/// One artifact's durable state grant as it appears in a [`ScopeReport`].
///
/// A grant rather than a disclosure, unlike [`ExportsFilesReport`] beside it — but reported on
/// exactly the same terms, and for the same reason: it opens a host path outside the workdir that
/// an operator reading `filesystem scope` alone would never see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateStoreReport {
    /// The artifact whose entry declared `capabilities.state`.
    pub artifact: String,
    /// The store directory name, with the capsule-name default already applied — so an operator
    /// reads the directory that will actually be opened, not the absence of a declaration.
    pub store: String,
    /// Absolute host path of the store, `~/.murmur/state/<store>` with the home already resolved.
    /// Reported as a string rather than a path so the JSON shape does not vary by platform.
    pub host_path: String,
}

/// The filesystem surface one artifact's guest is preopened into, as a stable wire name.
///
/// The three values are the whole space: a WASI guest either holds a directory descriptor for the
/// accessible workdir, holds one for a subtree of it, or holds none at all. Which one an entry
/// resolves to is decided by its role and its `capabilities.filesystem.scope` in
/// [`crate::network_policy::preopen_reports`]; the baselines the two roles start from are
/// [`crate::network_policy::ToolCapabilityGrant`] and
/// [`crate::network_policy::HookCapabilityGrant`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreopenSurface {
    /// The entire accessible workdir, preopened as `"."` with `FsPerms::ReadWrite`. The wide
    /// default a `runtime: tool` or `runtime: driver` entry with no
    /// `capabilities.filesystem.scope` resolves to — see
    /// [`crate::network_policy::ToolCapabilityGrant`] for the threat that default is chosen
    /// against and the threat it is not.
    WholeWorkdir,
    /// `<accessible workdir>/<scope>` and nothing above it, preopened as `"."` with the same
    /// rights. What a declared `capabilities.filesystem.scope` resolves to, whatever the role.
    ScopedSubtree,
    /// No preopened directory of any kind, so the guest holds no `wasi:filesystem` descriptor. The
    /// default-deny baseline a `runtime: hook` entry with no `capabilities.filesystem.scope`
    /// resolves to — see [`crate::network_policy::HookCapabilityGrant`].
    Nothing,
    /// The session's records directory's `hooks/` and nothing else, preopened as `"."`
    /// read-write: what a `runtime: hook` entry declaring `capabilities.filesystem.records: true`
    /// resolves to, in place of a workdir scope.
    RecordsHooks,
}

impl PreopenSurface {
    /// The phrase [`PreopenReport::render`] names this surface with. Distinct per variant rather
    /// than a formatted path, because the point of the line is that an operator can tell the three
    /// apart without knowing the resolved workdir.
    fn summary(self) -> &'static str {
        match self {
            PreopenSurface::WholeWorkdir => "the whole accessible workdir",
            PreopenSurface::ScopedSubtree => "one subtree of the accessible workdir",
            PreopenSurface::Nothing => "nothing preopened",
            PreopenSurface::RecordsHooks => "the records directory's hooks/",
        }
    }
}

/// One artifact's resolved WASI preopen as it appears in a [`ScopeReport`] — and therefore in
/// `mur run --explain-scope`, in `mur doctor`, and in `trace.jsonl`'s
/// `session_start.effective_grants`.
///
/// Per-artifact, like [`StateStoreReport`] beside it, and reported for the same reason: the
/// capsule-wide [`ScopeReport::filesystem_scope`] is joined into no guest preopen root, so an
/// operator reading that field alone learns nothing about what any tool, driver or hook can
/// actually see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreopenReport {
    /// The artifact whose entry in the capsule operator's own manifest this describes.
    pub artifact: String,
    /// [`murmur_artifact::ArtifactRuntime::as_str`] verbatim, so the report and the manifest name
    /// a role with one vocabulary.
    pub role: String,
    /// The entry's declared `capabilities.filesystem.scope`, verbatim. `None` = undeclared, which
    /// is what [`Self::surface`] has already resolved against the role's own baseline.
    pub scope: Option<String>,
    /// What the guest is preopened into once the role's baseline and [`Self::scope`] are both
    /// applied.
    pub surface: PreopenSurface,
    /// How the artifact reaches the session's runtime directory, read-only: at `murmur-runtime/`
    /// for a `runtime: tool` with no `capabilities.filesystem.scope`. `None` — serialized
    /// `null` — for every other entry, which has no reach.
    pub runtime_dir_reach: Option<RuntimeDirReach>,
    /// The host folders the guest is also preopened into, under `murmur-host`
    /// ([`crate::guest_roots`]), in preopen order. Filled for exactly the entries that reach the
    /// runtime directory, from the session's unconfined stance or `host_paths`. Always
    /// serialized: `[]` for every other entry.
    pub extra_roots: Vec<ExtraRootReport>,
}

/// One extra root as [`PreopenReport::extra_roots`] carries it. Every key is always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtraRootReport {
    /// Where the guest opens it: `murmur-host` followed by [`Self::path`].
    pub guest_path: String,
    /// The canonical host directory.
    pub path: String,
    /// `read` or `read-write`.
    pub access: crate::host_paths::HostPathAccess,
    /// `unconfined` or `host_paths`.
    pub source: crate::guest_roots::GuestRootSource,
}

impl From<&crate::guest_roots::GuestRoot> for ExtraRootReport {
    fn from(root: &crate::guest_roots::GuestRoot) -> Self {
        Self {
            guest_path: root.guest_path.clone(),
            path: root.host_path.display().to_string(),
            access: root.access,
            source: root.source,
        }
    }
}

/// Every report that reaches the runtime directory given `roots`, the session's extra roots
/// ([`crate::guest_roots::extra_guest_roots`]). `stage_session`, `mur run --explain-scope` and
/// `mur doctor` all call it, so the three agree.
pub fn attach_extra_roots(reports: &mut [PreopenReport], roots: &[crate::guest_roots::GuestRoot]) {
    for report in reports {
        report.extra_roots = if report.runtime_dir_reach.is_some() {
            roots.iter().map(ExtraRootReport::from).collect()
        } else {
            Vec::new()
        };
    }
}

/// How one artifact reaches the runtime directory, as [`PreopenReport::runtime_dir_reach`]
/// carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeDirReach {
    /// A tool's read-only preopen at `murmur-runtime/`.
    MurmurRuntime,
}

impl PreopenReport {
    /// The single line this preopen renders as, shared by [`ScopeReport::render`] and by
    /// `mur doctor` so the two cannot word the same grant differently.
    pub fn render(&self) -> String {
        let artifact = &self.artifact;
        let role = &self.role;
        let surface = self.surface.summary();
        let guest = crate::runtime_dir::RUNTIME_DIR_GUEST_PATH;
        let runtime_dir = match self.runtime_dir_reach {
            None => String::new(),
            Some(RuntimeDirReach::MurmurRuntime) => {
                format!("; runtime directory read-only at {guest}/")
            }
        };
        let extra_roots = self.render_extra_roots();
        match &self.scope {
            Some(scope) => format!(
                "{artifact} ({role}): {surface} — capabilities.filesystem.scope: \
                 {scope}{runtime_dir}{extra_roots}"
            ),
            None => format!(
                "{artifact} ({role}): {surface} — no capabilities.filesystem.scope \
                 declared{runtime_dir}{extra_roots}"
            ),
        }
    }

    /// The `; …` fragment naming [`Self::extra_roots`], empty when there are none.
    fn render_extra_roots(&self) -> String {
        if self.extra_roots.is_empty() {
            return String::new();
        }
        let guest = crate::guest_roots::HOST_ROOT_GUEST_PATH;
        let whole_host = self
            .extra_roots
            .iter()
            .any(|root| root.source == crate::guest_roots::GuestRootSource::Unconfined);
        if whole_host {
            return format!("; whole host read-write under {guest}/");
        }
        let folders: Vec<String> = self
            .extra_roots
            .iter()
            .map(|root| format!("{} ({})", root.path, root.access))
            .collect();
        format!("; host folders under {guest}/: {}", folders.join(", "))
    }
}

/// What the filesystem mechanism this session installs restricts, as a stable wire name.
///
/// Keyed on [`ContainmentDecision::applied_tier`], the tier
/// [`crate::sandbox::ShellEnforcement::resolve`] installs the session's mechanisms from and the
/// one [`ScopeReport::enforcement_tier`] names. Not on the achieved class, which `workdir_exec`
/// and the runtime directory's placement can cap at `advisory` without changing what the
/// mechanism restricts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilesystemRestriction {
    /// No kernel mechanism mediates the filesystem, so a path outside the workdir is reachable.
    Advisory,
    /// Landlock refuses the open, and the path outside the granted set still exists.
    Enforced,
    /// A composed root, where the path outside the capsule is not there to open.
    Absent,
    /// The unconfined stance: commands run as the operator and reach the host filesystem as the
    /// operator does.
    None,
}

/// What the filesystem mechanism this session installs does *not* protect, in words an operator
/// can act on.
///
/// The counterpart to every grant beside it in a [`ScopeReport`]: those say what the capsule may
/// reach, and this says which of the restrictions an operator would read into them are not there.
///
/// [`Self::not_protected`] is non-empty below [`FilesystemRestriction::Absent`], and empty at it
/// unless the runtime directory lies inside the workdir. It is serialized either way — an absent
/// key identifies a runtime that predates the field, never a capsule with nothing to disclaim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilesystemBoundaryReport {
    /// What the session's filesystem mechanism restricts.
    pub restriction: FilesystemRestriction,
    /// One statement per protection an operator would otherwise assume, the runtime directory's
    /// [`RuntimeDirReport::not_protected_because`] among them when it is set. Rendered verbatim
    /// under `Not protected here`, so the JSON and the human report cannot word the same
    /// disclaimer differently.
    pub not_protected: Vec<String>,
}

/// What a session with no kernel filesystem mechanism leaves unrestricted.
///
/// Names the mechanism rather than the class, because "advisory" is the word an operator has
/// already read on the `achieved:` line and taken for a protection.
const ADVISORY_FILESYSTEM_NOT_PROTECTED: &str =
    "Filesystem restriction is advisory on this host: no kernel mechanism denies a read or a write \
     outside the work folder, so capabilities.filesystem.scope bounds what the runtime hands a \
     subprocess rather than what that subprocess can reach. A Linux host with a usable Landlock \
     ABI installs a Landlock domain instead, where the kernel refuses the open.";

/// What a Landlock-scoped session leaves unrestricted.
///
/// Not the `advisory` sentence: Landlock really does mediate the filesystem here, and an operator
/// told otherwise goes and hardens something the kernel already holds. What Landlock does not do is
/// make a path absent — it mediates the operations that touch a file, not path resolution.
const ENFORCED_FILESYSTEM_NOT_PROTECTED: &str =
    "A path outside the granted set is denied rather than absent: Landlock refuses the open, and \
     stat, access and readlink on that path still succeed and still report the real file's \
     metadata, because Landlock mediates the operations that touch a file and not path resolution. \
     A probe that only asks whether a path exists therefore learns nothing about what was denied — \
     read the file to see the refusal. Declaring capabilities.containment: sealed on a host that \
     reaches the sealed tier composes a root the path is not in at all.";

/// What `HOME` is on a tier with no kernel filesystem mechanism: moved, and nothing more.
///
/// Stated beside [`ADVISORY_FILESYSTEM_NOT_PROTECTED`] because the relocation is the one thing an
/// operator sees working on such a tier, and it is easy to read as a boundary.
const ADVISORY_HOME_NOT_PROTECTED: &str =
    "HOME is relocated, not protected: it points at the session's home folder, and the real home \
     directory stays readable and writable by absolute path, because no kernel mechanism mediates \
     the filesystem on this tier.";

/// What no mechanism protects, and the reason `--explain-scope` carries this block at all.
///
/// Identical wherever it prints, because the rewrite it describes is identical: `HOME` is replaced
/// with the session's home folder on every tier and every platform (see `crate::shell`), so the
/// obvious `~`-based containment probe answers *no such file* on a host that denies nothing.
const PATH_REWRITING_NOT_PROTECTED: &str =
    "Path rewriting is a convenience, not a boundary: HOME is set to the session's home folder, \
     home/ in its records directory, on every tier, so `cat ~/.ssh/id_rsa` answers \"No such file \
     or directory\" whether or not anything was denied — the tilde resolved into that folder and \
     the host file was never looked for. A ~-based probe is therefore never evidence of containment; test with an absolute path \
     to a real file outside the workdir instead.";

/// What an unconfined session leaves unrestricted: everything, which is the one statement made.
pub(crate) const UNCONFINED_NOT_PROTECTED: &str =
    "Nothing is contained: this session runs unconfined, so its shell commands, plan shell steps \
     and process harness run as the operator, with the operator's HOME, environment \
     (credential variables included), filesystem and network, and sudo works wherever the \
     operator's sudoers allows it. The trace and the records directory are written as always but \
     are not protected from those commands.";

/// The filesystem boundary the tier a session applies actually provides, and the protections it
/// does not.
///
/// Pure and derived from the applied tier alone, so `--explain-scope` and
/// `session_start.effective_grants` cannot word the same disclaimer differently: both go through
/// [`scope_report_for_tier`], which calls this once with the tier it has already resolved.
///
/// The argument is the *applied* tier, never the host tier and never the achieved class. A composed
/// root is what makes a host path absent, and a session composes one only when the host reaches
/// `KernelSealed` **and** the capsule declared a `sealed` floor; a Landlock domain is installed
/// whenever the applied tier is `KernelFull` or `KernelSealed`, including for a capsule whose
/// `capabilities.filesystem.workdir_exec` caps its achieved class at `advisory`.
pub(crate) fn filesystem_boundary(applied: EnforcementTier) -> FilesystemBoundaryReport {
    // The path-rewriting statement rides along with the mechanism-specific one rather than standing
    // on its own, so a composed root — where the paths outside it are absent, and the `~` rewrite
    // lands inside the workdir — discloses nothing at all.
    if applied == EnforcementTier::Unconfined {
        // `HOME` is not rewritten on this tier, so the path-rewriting statement does not apply.
        return FilesystemBoundaryReport {
            restriction: FilesystemRestriction::None,
            not_protected: vec![UNCONFINED_NOT_PROTECTED.to_string()],
        };
    }
    // The `HOME` statement follows the path-rewriting one it qualifies, so that statement sits at
    // the same place on every tier that carries it.
    let not_protected: &[&str] = match applied {
        EnforcementTier::KernelSealed => &[],
        EnforcementTier::Unconfined => unreachable!("returned above"),
        EnforcementTier::KernelFull => &[
            ENFORCED_FILESYSTEM_NOT_PROTECTED,
            PATH_REWRITING_NOT_PROTECTED,
        ],
        EnforcementTier::KernelSeccompOnly | EnforcementTier::EnvironmentOnly => &[
            ADVISORY_FILESYSTEM_NOT_PROTECTED,
            PATH_REWRITING_NOT_PROTECTED,
            ADVISORY_HOME_NOT_PROTECTED,
        ],
    };
    let restriction = match applied {
        EnforcementTier::KernelSealed => FilesystemRestriction::Absent,
        EnforcementTier::Unconfined => unreachable!("returned above"),
        EnforcementTier::KernelFull => FilesystemRestriction::Enforced,
        EnforcementTier::KernelSeccompOnly | EnforcementTier::EnvironmentOnly => {
            FilesystemRestriction::Advisory
        }
    };
    let not_protected = not_protected
        .iter()
        .map(|statement| (*statement).to_string())
        .collect();

    FilesystemBoundaryReport {
        restriction,
        not_protected,
    }
}

/// What the session's runtime directory, or its records directory, is to the capsule's commands —
/// `--explain-scope`'s `Runtime directory` and `Records directory` blocks,
/// `session_start.effective_grants.runtime_dir` and `.records_dir` and `mur doctor`'s sections,
/// all from [`runtime_dir_report`] and [`records_dir_report`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeDirReport {
    /// `<home>/.murmur/sessions/<session-id>/runtime`, or
    /// `<home>/.murmur/records/<workdir root>/<session-id>`, with the literal `<session-id>`
    /// segment `runtime_writes` carries.
    pub path: String,
    /// Whether no capsule command can change a file in it.
    pub read_only_to_commands: bool,
    /// The mechanism that makes it read-only: `mountns+landlock` on the sealed tier, `landlock`
    /// on the Landlock tier, `null` when nothing does.
    pub enforced_by: Option<&'static str>,
    /// Why a command can change it, in words an operator can act on; `null` when none can.
    pub not_protected_because: Option<String>,
}

/// The runtime directory on the sealed tier: a read-only bind plus a Landlock rule.
const RUNTIME_DIR_ENFORCED_SEALED: &str = "mountns+landlock";

/// The runtime directory on the Landlock tier: a read-only Landlock rule.
const RUNTIME_DIR_ENFORCED_LANDLOCK: &str = "landlock";

/// The lowest Landlock ABI that mediates `truncate(2)`.
const LANDLOCK_TRUNCATE_ABI: u8 = 3;

/// One of the two directories outside the workdir the runtime keeps out of the commands' reach,
/// as [`protected_dir_report`] words its claims about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtectedDir {
    Runtime,
    Records,
}

impl ProtectedDir {
    fn noun(self) -> &'static str {
        match self {
            Self::Runtime => "runtime directory",
            Self::Records => "records directory",
        }
    }

    /// What a command could empty there on a Landlock ABI that does not mediate `truncate(2)`.
    fn file(self) -> &'static str {
        match self {
            Self::Runtime => "runtime file",
            Self::Records => "record",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Runtime => "Runtime directory",
            Self::Records => "Records directory",
        }
    }
}

/// What the session's runtime directory is to its commands, from the tier the session applies,
/// the Landlock ABI its ruleset is built at and where the directory lies.
///
/// Pure and the one source of the claim: `--explain-scope`, `session_start.effective_grants` and
/// `mur doctor` all print what this returns. It never claims protection on the seccomp-only tier,
/// with no kernel containment, or when the directory lies inside the workdir, and on the Landlock
/// tier below ABI 3 it says `truncate(2)` is not mediated.
#[must_use]
pub(crate) fn runtime_dir_report(
    applied: EnforcementTier,
    landlock_abi: Option<u8>,
    placement: &RuntimeDirPlacement,
) -> RuntimeDirReport {
    protected_dir_report(
        ProtectedDir::Runtime,
        applied,
        landlock_abi,
        placement.report_path(),
        placement.sessions_inside_workdir,
        &placement.workdir,
    )
}

/// [`runtime_dir_report`] for the session's records directory, which the same rule and bind
/// protect on the same tiers.
#[must_use]
pub(crate) fn records_dir_report(
    applied: EnforcementTier,
    landlock_abi: Option<u8>,
    placement: &RuntimeDirPlacement,
) -> RuntimeDirReport {
    protected_dir_report(
        ProtectedDir::Records,
        applied,
        landlock_abi,
        placement.records_report_path(),
        placement.records_inside_workdir,
        &placement.workdir,
    )
}

fn protected_dir_report(
    dir: ProtectedDir,
    applied: EnforcementTier,
    landlock_abi: Option<u8>,
    path: String,
    inside_workdir: bool,
    workdir: &std::path::Path,
) -> RuntimeDirReport {
    let noun = dir.noun();
    let not_protected = |reason: String| RuntimeDirReport {
        path: path.clone(),
        read_only_to_commands: false,
        enforced_by: None,
        not_protected_because: Some(reason),
    };
    match applied {
        EnforcementTier::Unconfined => not_protected(format!(
            "the capsule runs unconfined, so its commands run as the operator and can change the \
             {noun}"
        )),
        EnforcementTier::EnvironmentOnly => not_protected(format!(
            "this host has no kernel containment for capsule commands — macOS and every \
             non-Linux host run them as the operator's own processes, so a command can change \
             the {noun}"
        )),
        EnforcementTier::KernelSeccompOnly => not_protected(format!(
            "this host has no Landlock, so no kernel mechanism keeps a command from changing the \
             {noun}"
        )),
        _ if inside_workdir => not_protected(format!(
            "the {noun} {path} lies inside the workdir {}, so the capsule's commands can write \
             it; run with a --workdir that does not contain ~/.murmur",
            workdir.display()
        )),
        EnforcementTier::KernelSealed => RuntimeDirReport {
            path,
            read_only_to_commands: true,
            enforced_by: Some(RUNTIME_DIR_ENFORCED_SEALED),
            not_protected_because: None,
        },
        EnforcementTier::KernelFull => {
            let abi = landlock_abi.unwrap_or(1);
            if abi < LANDLOCK_TRUNCATE_ABI {
                not_protected(format!(
                    "Landlock ABI {abi} does not mediate truncate(2), so a command can empty a \
                     {} though not replace it; kernel 6.2 or later refuses it",
                    dir.file()
                ))
            } else {
                RuntimeDirReport {
                    path,
                    read_only_to_commands: true,
                    enforced_by: Some(RUNTIME_DIR_ENFORCED_LANDLOCK),
                    not_protected_because: None,
                }
            }
        }
    }
}

/// The `Runtime directory` block of `--explain-scope` and `mur doctor`: its path, and whether
/// the capsule's commands can change it. Ends with a newline.
#[must_use]
pub fn render_runtime_dir(report: &RuntimeDirReport) -> String {
    render_protected_dir(ProtectedDir::Runtime, report)
}

/// The `Records directory` block of `--explain-scope` and `mur doctor`, on
/// [`render_runtime_dir`]'s terms. Ends with a newline.
#[must_use]
pub fn render_records_dir(report: &RuntimeDirReport) -> String {
    render_protected_dir(ProtectedDir::Records, report)
}

fn render_protected_dir(dir: ProtectedDir, report: &RuntimeDirReport) -> String {
    let commands = match (&report.not_protected_because, report.enforced_by) {
        (Some(reason), _) => format!("can change it — {reason}"),
        (None, Some(mechanism)) => format!("read only ({mechanism})"),
        (None, None) => "read only".to_string(),
    };
    format!(
        "{}\n  path:      {}\n  commands:  {commands}\n",
        dir.heading(),
        report.path
    )
}

/// The session's temporary folder, and whether a literal `/tmp` reaches it —
/// `--explain-scope`'s `Temporary folder` block, `session_start.effective_grants.temp_dir` and
/// `mur doctor`'s section, all from [`temp_dir_report`]. Field names are the JSON keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TempDirReport {
    /// `<home>/.murmur/records/<workdir root>/<session-id>/tmp`, with the literal `<session-id>`
    /// segment the records directory's path carries.
    pub path: String,
    /// The value of `TMPDIR`, `TMP` and `TEMP` in every command: `/tmp` when mapped, [`Self::path`]
    /// when not.
    pub tmpdir: String,
    /// Whether a literal `/tmp` is the folder.
    pub tmp_mapped: bool,
    /// The mechanism that maps it: `composed-root` on the sealed tier, `mount-namespace` on the
    /// tiers below it, `null` when not mapped.
    pub mapped_by: Option<&'static str>,
    /// Why `/tmp` is not mapped and what a literal `/tmp` path reaches instead; `null` when
    /// mapped.
    pub not_mapped_because: Option<String>,
}

/// What the session's temporary folder is to its commands under `mapping`, with the folder's path
/// from `placement`. Pure.
#[must_use]
pub(crate) fn temp_dir_report(
    mapping: crate::temp_dir::TempMapping,
    placement: &RuntimeDirPlacement,
) -> TempDirReport {
    let path = std::path::Path::new(&placement.records_report_path())
        .join(crate::temp_dir::SESSION_TEMP_DIR_NAME)
        .display()
        .to_string();
    TempDirReport {
        tmpdir: mapping.tmpdir_value(std::path::Path::new(&path)),
        path,
        tmp_mapped: mapping.is_mapped(),
        mapped_by: mapping.mapped_by(),
        not_mapped_because: match mapping {
            crate::temp_dir::TempMapping::NotMapped(reason) => Some(reason.describe().to_string()),
            _ => None,
        },
    }
}

/// The `Temporary folder` block of `--explain-scope` and `mur doctor`: its path, the value of
/// `TMPDIR`, and whether a literal `/tmp` reaches it. Ends with a newline.
#[must_use]
pub fn render_temp_dir(report: &TempDirReport) -> String {
    let tmp = match (report.mapped_by, &report.not_mapped_because) {
        (Some(mechanism), _) => format!("mapped ({mechanism})"),
        (None, Some(reason)) => format!("not mapped — {reason}"),
        (None, None) => "not mapped".to_string(),
    };
    format!(
        "Temporary folder\n  path:      {}\n  TMPDIR:    {}\n  /tmp:      {tmp}\n",
        report.path, report.tmpdir
    )
}

/// One element of [`ScopeReport::declared_needs`]: what one artifact's needs for one program
/// resolved to. Every key is always serialized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeclaredNeedsReport {
    /// The artifact as the capsule's manifest names it, scope included.
    pub artifact: String,
    /// The program key, as `capabilities.shell.allow` names it.
    pub program: String,
    pub granted: Vec<GrantedNeedReport>,
    pub missing: Vec<MissingNeedReport>,
}

/// A granted need: its label (`library libm.so.6`, `program_folder .`) and each path, rendered
/// `<path>` for a file and `<path> (list_dir)` for a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrantedNeedReport {
    pub need: String,
    pub paths: Vec<String>,
}

/// A missing need: its label and why it is not granted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingNeedReport {
    pub need: String,
    pub reason: String,
}

impl From<&crate::declared_needs::ResolvedProgramNeeds> for DeclaredNeedsReport {
    fn from(resolved: &crate::declared_needs::ResolvedProgramNeeds) -> Self {
        Self {
            artifact: resolved.artifact.clone(),
            program: resolved.program.clone(),
            granted: resolved
                .granted
                .iter()
                .map(|need| GrantedNeedReport {
                    need: need.need.form.to_string(),
                    paths: need
                        .paths
                        .iter()
                        .map(crate::declared_needs::NeedPath::render)
                        .collect(),
                })
                .collect(),
            missing: resolved
                .missing
                .iter()
                .map(|need| MissingNeedReport {
                    need: need.need.form.to_string(),
                    reason: need.reason.clone(),
                })
                .collect(),
        }
    }
}

impl DeclaredNeedsReport {
    /// One line per need, as `--explain-scope` prints them under `declared needs:`:
    /// `<artifact> (<program>): <need> -> <path>[, <path>…]` for a granted need and
    /// `<artifact> (<program>): <need> -> missing (<reason>)` for a missing one.
    #[must_use]
    pub fn render_lines(&self) -> Vec<String> {
        let owner = format!("{} ({})", self.artifact, self.program);
        self.granted
            .iter()
            .map(|need| format!("{owner}: {} -> {}", need.need, need.paths.join(", ")))
            .chain(
                self.missing
                    .iter()
                    .map(|need| format!("{owner}: {} -> missing ({})", need.need, need.reason)),
            )
            .collect()
    }
}

/// One element of [`ScopeReport::pinned_programs`]: which copy of a program a tool pins a
/// download of the session runs. `path` is absent for `missing`, `url` and `sha256` are present
/// only for `pinned`, and `reason` only for `missing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PinnedProgramReport {
    /// The artifact as the capsule's manifest names it, scope included.
    pub artifact: String,
    /// The program key, as `capabilities.shell.allow` names it.
    pub program: String,
    /// This host's platform, as the download is pinned for it (`linux-x86_64`).
    pub platform: String,
    /// `pinned` (the program store's copy), `host` (a program of that name on `PATH`) or
    /// `missing`.
    pub source: String,
    /// The file that runs, canonical.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&crate::pinned_programs::ResolvedPinnedProgram> for PinnedProgramReport {
    fn from(resolved: &crate::pinned_programs::ResolvedPinnedProgram) -> Self {
        use crate::pinned_programs::PinnedSource;
        let mut report = Self {
            artifact: resolved.artifact.clone(),
            program: resolved.program.clone(),
            platform: resolved.platform.clone(),
            source: resolved.source.label().to_string(),
            path: None,
            url: None,
            sha256: None,
            reason: None,
        };
        match &resolved.source {
            PinnedSource::Pinned {
                path, url, sha256, ..
            } => {
                report.path = Some(path.display().to_string());
                report.url = Some(url.clone());
                report.sha256 = Some(sha256.clone());
            }
            PinnedSource::Host { path } => report.path = Some(path.display().to_string()),
            PinnedSource::Missing { reason } => report.reason = Some(reason.clone()),
        }
        report
    }
}

impl PinnedProgramReport {
    /// The line `--explain-scope` prints under `pinned programs:`:
    /// `<artifact> (<program>): pinned <path> (sha256 <sha>, from <url>)`,
    /// `<artifact> (<program>): host <path>` or `<artifact> (<program>): missing (<reason>)`.
    #[must_use]
    pub fn render_line(&self) -> String {
        let owner = format!("{} ({})", self.artifact, self.program);
        let path = self.path.as_deref().unwrap_or_default();
        match self.source.as_str() {
            "pinned" => format!(
                "{owner}: pinned {path} (sha256 {}, from {})",
                self.sha256.as_deref().unwrap_or_default(),
                self.url.as_deref().unwrap_or_default()
            ),
            "host" => format!("{owner}: host {path}"),
            _ => format!(
                "{owner}: missing ({})",
                self.reason.as_deref().unwrap_or_default()
            ),
        }
    }
}

/// Read-only answer to "what would this capsule actually be allowed to do on this host?".
///
/// Built from the already-parsed [`CapabilityPolicy`] plus one [`ContainmentDecision`]. Nothing
/// here stages artifacts, touches a registry, creates a workdir or resolves DNS — it is a
/// diagnostic, so it reports even when [`Self::floor_met`] is `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeReport {
    /// Strongest class any source asked for (manifest / workspace config / `--containment`).
    pub declared_containment: ContainmentClass,
    /// The class this session's commands run under: the class of [`Self::enforcement_tier`],
    /// capped at `advisory` by `workdir_exec` or a runtime directory inside the workdir. From
    /// [`ContainmentDecision::achieved`].
    pub achieved_containment: ContainmentClass,
    /// The class this host could back for a capsule declaring `sealed`, never what this session
    /// runs on: a capsule declaring less runs on a weaker tier, and [`Self::achieved_containment`]
    /// says which. From [`ContainmentDecision::host_containment`]. Always serialized.
    pub host_containment: ContainmentClass,
    /// `achieved_containment >= declared_containment`. `false` means `mur run` would refuse.
    pub floor_met: bool,
    /// The missing mechanism when `floor_met` is `false`; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortfall_reason: Option<String>,
    /// The tier this session installs, as a stable wire name: the host's tier capped by the
    /// declared floor, from [`ContainmentDecision::enforcement_tier`].
    pub enforcement_tier: &'static str,
    /// Every route this capsule could run code on, and what holds each: one [`RouteReport`] row per
    /// [`Route::ALL`] entry, in that order, from [`route_table`]. A route the session's class
    /// covers names [`Self::enforcement_tier`]; one it does not names [`HOST_PROCESS_MECHANISM`]
    /// and says what it does run under.
    ///
    /// Always serialized, every row, on every tier and platform: an absent key identifies a
    /// runtime that predates it. Set again through [`Self::with_routes`] by the caller that knows
    /// the inference transport.
    pub routes: Vec<RouteReport>,
    /// Where this host's permission to create an unprivileged user namespace comes from, as
    /// [`UsernsGrant::wire_name`], or `null` off Linux, where AppArmor does not exist and the
    /// question has no answer.
    ///
    /// Always serialized, on the same terms as [`Self::workdir_exec`]: the key's absence
    /// identifies a runtime that predates it, not a host that was not asked. Reported because
    /// three very different hosts — no AppArmor, hardening switched off host-wide, and the shipped
    /// profile confining `mur` — otherwise produce byte-identical reports while differing enormously
    /// in what else on the machine can create a user namespace.
    pub userns_grant: Option<UsernsGrant>,
    /// The Landlock ABI this session's ruleset is built at: the kernel's, capped at
    /// [`crate::sandbox::LANDLOCK_TARGET_ABI`], or `null` when no Landlock domain is installed —
    /// a tier below `landlock+seccomp`, or a kernel without Landlock.
    ///
    /// Keyed on the session's applied tier, like [`Self::filesystem_boundary`], and computed
    /// by the same [`crate::sandbox::ruleset_landlock_abi`] the shell child builds at, so the
    /// trace records the ruleset that was installed. Always serialized, on the same terms as
    /// [`Self::userns_grant`]. Below 2 a file cannot be moved or hard-linked between two folders
    /// of the workdir; below 3 truncating a host file outside it is not refused.
    pub landlock_abi: Option<u8>,
    /// `capabilities.filesystem.scope`, verbatim.
    pub filesystem_scope: Option<String>,
    /// `capabilities.filesystem.workdir_exec`. Always present (never skipped when `false`), so a
    /// consumer can tell "declared false" from "this runtime predates the key" by the field's
    /// presence rather than by its value.
    ///
    /// `true` is the only manifest declaration that lowers [`Self::achieved_containment`], which is
    /// why it is reported next to the grants rather than buried: reading `achieved: advisory` on a
    /// Landlock-capable host makes sense only alongside it.
    pub workdir_exec: bool,
    /// `capabilities.filesystem.read_only`, verbatim — the workdir subtrees the capsule may read
    /// but must not write. Empty when the capsule declares none, which is the whole workdir
    /// writable. Always serialized as an array (never skipped), on the same terms as
    /// [`Self::state_stores`]: an absent key identifies a runtime that predates the field, not a
    /// capsule that declared nothing.
    ///
    /// The one field here that subtracts rather than grants, which is why it is reported beside
    /// [`Self::read_only_advisory_for`] rather than alone: a protection stated without its
    /// qualification reads as a boundary the capsule does not have.
    pub read_only_paths: Vec<String>,
    /// The entries of [`Self::shell_allow`] that [`Self::read_only_paths`] is only advisory
    /// against — the same set `W-SEC-017` fires for, resolved by the same
    /// [`read_only_advisory_for`] both this report and `mur doctor` call.
    ///
    /// Empty means the declaration is enforced for every readable call. Non-empty names the
    /// binaries whose own file I/O the dispatch-time write-intent analyser cannot read, so the
    /// report cannot claim more than `W-SEC-017` already qualifies. Always serialized as an array
    /// (never skipped), on the same terms as [`Self::read_only_paths`], and always empty when
    /// that list is: nothing is advisory against a protection that was never declared.
    pub read_only_advisory_for: Vec<String>,
    /// The filesystem surface every guest-bearing artifact in this capsule is preopened into, one
    /// entry per `runtime: tool`, `runtime: driver` and `runtime: hook` entry, in manifest order.
    /// Empty when the capsule declares only skills. Always serialized as an array (never skipped),
    /// on the same terms as [`Self::state_stores`]: an absent key identifies a runtime that
    /// predates the field, not a capsule with no guest artifacts.
    ///
    /// This is the field that answers "what can this tool read and write?".
    /// [`Self::filesystem_scope`] above does not: it is the capsule-wide declaration, and it is
    /// joined into no preopen root.
    ///
    /// Copied through untouched, exactly as the reports beside it are — resolving a preopen
    /// changes no class, no floor and no tier.
    pub preopens: Vec<PreopenReport>,
    /// `capabilities.network.allow`, verbatim — declared destinations, not resolved IPs.
    pub network_allow: Vec<String>,
    /// `capabilities.network.unix_sockets`.
    pub unix_sockets: bool,
    /// `capabilities.shell.allow`, verbatim.
    pub shell_allow: Vec<String>,
    /// `capabilities.spawn.allow`, verbatim.
    pub spawn_allow: Vec<String>,
    /// `capabilities.spawn.workdir_snapshot`, verbatim: the entries of [`Self::spawn_allow`] whose
    /// every delegation starts in a copy of this session's work folder. Always serialized, `[]`
    /// when undeclared.
    pub spawn_workdir_snapshot: Vec<String>,
    /// What this session's work folder started with, when a delegating parent copied its own
    /// into it, and what keeps this session's commands off the parent's real folder on its tier:
    /// `null` for every session but such a child. Always serialized.
    ///
    /// A report of what the launching parent did, from `mur run --parent-workdir-snapshot`, never
    /// a grant: it opens nothing and changes no class, floor or tier. Its `restriction` is
    /// [`Self::filesystem_boundary`]'s, keyed on the same applied tier.
    pub parent_workdir: Option<crate::workdir_snapshot::ParentWorkdirReport>,
    /// The `context.images` limits this session applies, and whether `keep` applies at all. Set
    /// by the caller that knows the transport, through [`Self::with_images`]; `null` until then.
    /// Always serialized.
    pub images: Option<ImagesReport>,
    /// `capabilities.env.allow`, verbatim.
    pub env_allow: Vec<String>,
    /// `capabilities.plan.submit` — whether the model is offered the `submit-plan` tool. Always
    /// serialized (never skipped when `false`), on the same terms as [`Self::workdir_exec`]: an
    /// absent key identifies a runtime that predates the grant, not a capsule that declined it.
    ///
    /// It opens no host resource of its own. It is reported beside the grants because it changes
    /// *how much* of every other grant one model turn can reach: a plan runs many steps against
    /// this capsule's own tools, shell allowlist and spawn grant without a turn in between.
    pub plan_submit: bool,
    /// `capabilities.install.skill`, verbatim: which skill artifacts a pull (`manage.pull`,
    /// `pull-artifact`) may install into the session. Always serialized, an empty list included,
    /// on [`Self::plan_submit`]'s terms: an absent key identifies a runtime that predates the
    /// grant.
    ///
    /// It opens no host resource of its own. A pulled skill adds text to what the model reads and
    /// gains no grant.
    pub install_skill: Vec<String>,
    /// `capabilities.install.tool`, verbatim: which tool artifacts a pull may install. Always
    /// serialized, on [`Self::install_skill`]'s terms. A pulled tool joins the dispatch allowlist,
    /// runs on this capsule's own grants and gains none of its own.
    pub install_tool: Vec<String>,
    /// `capabilities.install.from`: `["stores"]`, or `["stores", "sources"]` when a pull the
    /// stores cannot answer is fetched from the scope's GitHub source. `[]` for a capsule with no
    /// install grant, which pulls from nowhere. Always serialized, on [`Self::install_skill`]'s
    /// terms.
    pub install_from: Vec<String>,
    /// Every host directory a `capabilities.shell.interpreter_runtime` grant opens, rendered as
    /// `<binary>: <dir>[ (list_dir)]`. These are the paths outside the workdir that stay
    /// reachable even at `scoped`.
    pub interpreter_runtime_grants: Vec<String>,
    /// Every host path staging derived from a recognised `shell.allow` interpreter's own import
    /// path, rendered `<binary>: <dir> (list_dir)` for a directory and `<binary>: <path>` for a zip
    /// archive or an extension-module library. Each is readable and none is executable. Always
    /// serialized, `[]` when no allowlisted binary is a recognised interpreter.
    pub derived_interpreter_runtime: Vec<String>,
    /// Every host trust-store path staging found for a capsule whose commands reach the network,
    /// rendered `<path> (list_dir)` for a directory and `<path>` for a CA bundle file. Each is
    /// readable and none is executable. Always serialized, `[]` when `shell.allow` or
    /// `network.allow` is empty or the host has none of the locations
    /// [`crate::trust_store`] checks.
    pub trust_store: Vec<String>,
    /// What the host programs the capsule's tools run need, one element per (artifact, program)
    /// with at least one need, as [`crate::declared_needs`] resolved it: each granted need with
    /// its paths, each missing one with why. Always serialized, `[]` when no tool pinned with an
    /// `operator` lock origin declares a need for a program `shell.allow` grants.
    pub declared_needs: Vec<DeclaredNeedsReport>,
    /// Which copy of each program a tool pins a download of runs, one element per (artifact,
    /// program) gated as [`Self::declared_needs`] and with a download for this host's platform,
    /// as [`crate::pinned_programs`] chose it. Always serialized, `[]` when none applies.
    pub pinned_programs: Vec<PinnedProgramReport>,
    /// The host files a command reads to resolve `localhost` and the other names `/etc/hosts`
    /// carries, granted read-only by the tier: `/etc/hosts` and `/etc/nsswitch.conf` on the
    /// Landlock tiers (`scoped`, `sealed`), `[]` on every other. Always serialized.
    pub name_resolution: Vec<String>,
    /// Every host folder `~/.murmur/config.yaml`'s `host_paths:` lends this session, in config
    /// order, with what the applied tier does with it (see [`crate::host_paths`]). Always
    /// serialized, `[]` with none. A grant from the operator's global config, never the manifest,
    /// and it changes no class, floor or tier.
    pub host_paths: Vec<crate::host_paths::HostPathReport>,
    /// The read-only file surface `exports.files` declares, or `null` when the capsule declares
    /// none. Always serialized (never skipped), so a consumer can tell "this runtime predates
    /// exports" from "this capsule declined to export anything" by the field's presence rather
    /// than by its value — the same terms [`Self::workdir_exec`] is reported on.
    ///
    /// An export is a *disclosure*, not a grant: it never appears in
    /// [`Self::achieved_containment`], and declaring one cannot change any other field of this
    /// report. It is printed beside the grants because it is the other direction of the same
    /// question — what crosses the capsule boundary, and which way.
    pub exports_files: Option<ExportsFilesReport>,
    /// The peer-handoff surface `exports.peer_files` declares, or `null` when the capsule
    /// declares none. Always serialized, on the same terms as [`Self::exports_files`].
    ///
    /// A separate authoriser from `exports.files`, over a separate subtree: a capsule may declare
    /// either, both or neither, and this field says nothing about the other. Like every export it
    /// is a disclosure rather than a grant, and declaring it cannot change
    /// [`Self::achieved_containment`], [`Self::enforcement_tier`] or [`Self::floor_met`].
    pub peer_files: Option<PeerFilesReport>,
    /// Whether the door serves tasks from peer capsules: `exports.peer_tasks.accept`, with an
    /// absent block reported as `false`. Always serialized.
    ///
    /// Like every export it opens a surface rather than granting the guest anything, and it cannot
    /// change [`Self::achieved_containment`], [`Self::enforcement_tier`] or [`Self::floor_met`].
    pub peer_tasks: bool,
    /// `capabilities.peer_fetch.allow`, verbatim — the peers this capsule may redeem a handle
    /// against. Empty when the block is absent, which is deny.
    ///
    /// A separate list from [`Self::network_allow`] and never an alias for it: a destination
    /// reachable by `send` is not redeemable unless it also appears here, because ingesting a
    /// peer's bytes into this workdir is a different question from being allowed to talk to it.
    pub peer_fetch_allow: Vec<String>,
    /// Every durable state store this session's artifacts declare, one entry per declaring
    /// artifact, empty when none does. Always serialized as an array (never skipped), on the same
    /// terms as [`Self::exports_files`]: an absent key identifies a runtime that predates state
    /// stores, not a capsule that declined one.
    ///
    /// Declaring a store cannot change [`Self::achieved_containment`], [`Self::floor_met`] or
    /// [`Self::enforcement_tier`] — it is copied through untouched, exactly as the export reports
    /// beside it are. It is reported because it is the one grant that opens a path *outside* every
    /// workdir, which nothing else in this block would reveal.
    pub state_stores: Vec<StateStoreReport>,
    /// Every artifact whose manifest entry declares a `config:` block, in manifest order, empty
    /// when none does. Always serialized as an array (never skipped), on the same terms as
    /// [`Self::state_stores`]: an absent key identifies a runtime that predates artifact config,
    /// not a capsule that declined it.
    ///
    /// Names only. What an artifact is configured *with* is operator-authored plaintext that
    /// belongs in the manifest and in nothing this report — or `trace.jsonl`, which carries this
    /// report verbatim — writes out.
    ///
    /// Declaring config cannot change [`Self::achieved_containment`], [`Self::floor_met`] or
    /// [`Self::enforcement_tier`]: it adds one environment variable to one guest and reaches no
    /// grant. Reported because a variable an artifact silently depends on is otherwise invisible
    /// to an operator reading the capsule's scope.
    pub configured_artifacts: Vec<String>,
    /// Every `capabilities.shell.staged_runtime` grant, rendered as
    /// `<binary>: <source_path> (pin: <pin>)`. These are host runtime trees a `sealed` capsule
    /// asks to have bind-mounted read-only into its composed root.
    ///
    /// Reported whatever the floor and whatever this host can back — a capsule that declares a
    /// staged runtime and cannot run here is exactly the case an operator is inspecting when they
    /// reach for `--explain-scope`, so hiding the grant behind a met floor would blank out the
    /// diagnostic in the one case it is for.
    pub staged_runtime_grants: Vec<String>,
    /// Every path the runtime itself writes inside the capsule's workdir, so a consumer for whom
    /// that workdir is the deliverable can subtract them and be left with what the *capsule*
    /// changed. Always serialized as an array (never skipped), on the same terms as
    /// [`Self::state_stores`]: an absent key identifies a runtime that predates the declaration,
    /// not a runtime that writes nothing.
    ///
    /// The one field here that describes what the runtime does rather than what the capsule may
    /// do. It is reported beside the grants because it answers the other half of "what is this
    /// workdir", and because a consumer that has to maintain its own list of runtime entries
    /// silently falls behind every time the set changes — enabling `sealed` being exactly that
    /// case.
    ///
    /// The same set whatever [`Self::enforcement_tier`] and [`Self::achieved_containment`] say:
    /// no row turns on the tier. See [`crate::workdir_writes`].
    pub runtime_writes: Vec<crate::workdir_writes::RuntimeWriteReport>,
    /// Every path the manifest names a file by — `inference.system_prompt_file`,
    /// `inference.compaction.system_prompt_file`, each local-source `artifacts[].source` — and
    /// where it is read from: the manifest folder, the installed package, or the host path itself.
    /// Always serialized as an array (never skipped), on the same terms as
    /// [`Self::state_stores`]; `[]` when the manifest names no file.
    ///
    /// Reports where a path resolves, not whether the file is there: nothing is read to build it.
    /// Changes no class and no floor. Set through [`Self::with_manifest_files`] from
    /// [`crate::manifest_files::manifest_file_reports`].
    pub manifest_files: Vec<crate::manifest_files::ManifestFileReport>,
    /// The declared `capabilities.resources.cgroup_io_bytes_per_sec` ceiling, and whether it is
    /// actually on this session's cgroup scope.
    ///
    /// Always serialized (never skipped), on the same terms as [`Self::workdir_exec`] and
    /// [`Self::state_stores`]: an absent key identifies a runtime that predates the field, not a
    /// host that was not asked. [`IoMaxStatus::NotProbed`] is what "not asked" looks like, and it
    /// is a value in the report rather than a missing key.
    ///
    /// The one field here filled from a *performed* operation rather than from the policy: at
    /// launch it carries the outcome of the real `io.max` write, established before `session_start`
    /// is written so `trace.jsonl` cannot claim a ceiling the kernel refused; under
    /// `--explain-scope` it carries the outcome of the same write against a throwaway cgroup that
    /// is created and removed. Declaring a ceiling that does not apply changes no class and refuses
    /// no launch — `io.max` is the one non-fatal cgroup limit — so this field is the only place
    /// the shortfall appears.
    pub io_max: IoMaxReport,
    /// Whether this session's subprocesses get a delegated cgroup v2 scope.
    ///
    /// Always serialized. At launch it is `delegated` or `not-required`, from
    /// [`crate::cgroup::prepare_scope`]'s real outcome: a launch that cannot get a scope it needs
    /// is refused with `E-RUN-012` and writes no `session_start`. Under `--explain-scope` it is
    /// [`crate::cgroup::probe_scope`]'s answer, and `unavailable` there is that refusal predicted,
    /// with the reason it would print. Changes no class and no floor: a refused capsule runs under
    /// no class at all. Set through [`Self::with_cgroup_scope`]; `not-probed` until then.
    pub cgroup_scope: CgroupScopeReport,
    /// What this session's filesystem mechanism does not restrict — the block `--explain-scope`
    /// prints as `Not protected here`, directly under `Containment`.
    ///
    /// Always serialized (never skipped), on the same terms as [`Self::state_stores`]: an absent
    /// key identifies a runtime that predates the field, and an empty
    /// [`FilesystemBoundaryReport::not_protected`] identifies a session with nothing to disclaim.
    ///
    /// Derived in [`filesystem_boundary`] from the tier [`Self::enforcement_tier`] names, and
    /// from no [`CapabilityPolicy`] field: `workdir_exec` caps [`Self::achieved_containment`]
    /// without changing what the mechanism restricts. It stands beside the grants rather than
    /// among them because it qualifies every one at once.
    pub filesystem_boundary: FilesystemBoundaryReport,
    /// What the session's runtime directory is to its commands, from [`runtime_dir_report`] on
    /// the applied tier, [`Self::landlock_abi`] and where the directory lies. Always serialized.
    /// When it is not read-only, its reason is also a [`FilesystemBoundaryReport::not_protected`]
    /// statement.
    pub runtime_dir: RuntimeDirReport,
    /// What the session's records directory is to its commands, from [`records_dir_report`] on
    /// the same inputs. Always serialized; its reason, when set, is also a
    /// [`FilesystemBoundaryReport::not_protected`] statement.
    pub records_dir: RuntimeDirReport,
    /// The session's temporary folder and whether a literal `/tmp` reaches it, from
    /// [`temp_dir_report`] on the session's [`crate::temp_dir::temp_mapping`] answer — the one
    /// its enforcement installs. Always serialized.
    pub temp_dir: TempDirReport,
    /// Which `/proc` the session's commands see, from [`crate::proc_view::process_view_report`]
    /// on the view the session's probe decided ([`Self::with_process_view`]) — the one its
    /// enforcement installs. Always serialized.
    pub process_view: crate::proc_view::ProcessViewReport,
    /// Who the capsule's subprocesses run as when the session is unconfined; `null` on every
    /// contained session. Always serialized.
    pub unconfined: Option<UnconfinedReport>,
}

/// What an unconfined session's subprocesses run as: `ScopeReport::unconfined`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnconfinedReport {
    /// The account name of the uid `mur` runs as, `null` when the account database has none.
    pub user: Option<String>,
    /// The uid `mur` runs as, which every unconfined subprocess runs as.
    pub uid: u32,
    /// The `HOME` `mur run` was started with, which every unconfined subprocess sees; `null` when
    /// it had none.
    pub home: Option<String>,
    /// The subprocess routes that run unconfined, as stable wire names.
    pub applies_to: Vec<&'static str>,
    /// The `capabilities.resources` fields the manifest declares, the only limits an unconfined
    /// session applies.
    pub resource_limits: Vec<&'static str>,
}

/// The subprocess routes an unconfined session runs as the operator, as
/// [`UnconfinedReport::applies_to`] names them.
pub const UNCONFINED_APPLIES_TO: &[&str] = &["shell", "plan-shell-step", "process-harness"];

impl UnconfinedReport {
    /// The report for this process: its uid, that uid's account name, its `HOME`, and the fields
    /// `declared` names.
    pub(crate) fn for_this_process(declared: &murmur_artifact::ResourceCapabilities) -> Self {
        let uid = current_uid();
        Self {
            user: account_name(uid),
            uid,
            home: std::env::var_os("HOME").map(|home| home.to_string_lossy().into_owned()),
            applies_to: UNCONFINED_APPLIES_TO.to_vec(),
            resource_limits: crate::resources::declared_resource_fields(declared),
        }
    }

    /// The `Unconfined` block of `--explain-scope`. Ends with a newline.
    #[must_use]
    pub fn render(&self) -> String {
        let applies_to = self
            .applies_to
            .iter()
            .map(|route| match *route {
                "shell" => "shell commands",
                "plan-shell-step" => "plan shell steps",
                "process-harness" => "process harness",
                other => other,
            })
            .collect::<Vec<_>>()
            .join(", ");
        let limits = if self.resource_limits.is_empty() {
            "none declared".to_string()
        } else {
            self.resource_limits.join(", ")
        };
        format!(
            "Unconfined\n  user:      {}\n  uid:       {}\n  HOME:      {}\n  applies to: \
             {applies_to}\n  resource limits: {limits}\n",
            self.user.as_deref().unwrap_or("<unknown>"),
            self.uid,
            self.home.as_deref().unwrap_or("<unset>"),
        )
    }
}

/// The `W-SEC-036` message for an unconfined session running as `report` says.
pub(crate) fn unconfined_warning(report: &UnconfinedReport) -> String {
    let user = match &report.user {
        Some(user) => format!("{user} (uid {})", report.uid),
        None => format!("uid {}", report.uid),
    };
    let home = report.home.as_deref().unwrap_or("<unset>");
    format!(
        "this capsule runs unconfined: its shell commands, plan shell steps and process harness \
         run as {user} with HOME {home}, the whole environment mur run was \
         started with (credential variables included), the host filesystem and network as that \
         account has them, and no no_new_privs, so sudo works wherever that account's sudoers \
         allows it"
    )
}

/// Prints `W-SEC-036` once, on stderr and in the bootstrap log. Called by `launch_session` after
/// staging succeeds, never by `--explain-scope`.
pub(crate) fn warn_unconfined_launch(
    records_root: &crate::workdir_fs::WorkdirRoot,
    report: &UnconfinedReport,
) {
    let code = murmur_artifact::security_warnings::W_SEC_036;
    let message = unconfined_warning(report);
    let link = murmur_artifact::security_warning_link(code);
    crate::runtime_err!("[capsule-runtime] warning[{code}]: {message} ({link})");
    crate::agent::append_bootstrap_log(
        records_root,
        &format!("[capability-policy] warning[{code}]: {message} ({link})"),
    );
}

/// The uid this process runs as.
#[cfg(unix)]
pub(crate) fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

#[cfg(not(unix))]
pub(crate) fn current_uid() -> u32 {
    0
}

/// The account name the account database holds for `uid`, `None` when it holds none.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn account_name(uid: u32) -> Option<String> {
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: an all-zero `passwd` is a valid value of a plain C struct of pointers and integers;
    // `getpwuid_r` overwrites it before it is read.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local that outlives the call, and `buffer.len()` is the
    // buffer's real length, so `getpwuid_r` writes only inside it.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || entry.pw_name.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` points at a NUL-terminated string inside `buffer`, which is
    // still alive.
    let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(not(unix))]
pub(crate) fn account_name(_uid: u32) -> Option<String> {
    None
}

impl ScopeReport {
    /// Human-readable rendering used by `mur run --explain-scope` without `--json`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("Containment\n");
        out.push_str(&format!("  declared:  {}\n", self.declared_containment));
        out.push_str(&format!("  achieved:  {}\n", self.achieved_containment));
        out.push_str(&format!("  host can back: {}\n", self.host_containment));
        out.push_str(&format!(
            "  floor met: {}\n",
            if self.floor_met { "yes" } else { "no" }
        ));
        if let Some(reason) = &self.shortfall_reason {
            out.push_str(&format!("  reason:    {reason}\n"));
        }
        out.push_str(&format!("  mechanism: {}\n", self.enforcement_tier));
        out.push_str(&format!(
            "  userns grant: {}\n",
            match self.userns_grant {
                Some(grant) => grant.wire_name(),
                None => "n/a",
            }
        ));
        out.push_str(&format!(
            "  landlock ABI: {}\n",
            match self.landlock_abi {
                Some(abi) => abi.to_string(),
                None => "n/a".to_string(),
            }
        ));
        out.push_str(&format!(
            "  {}\n",
            crate::cgroup::render_cgroup_scope(&self.cgroup_scope)
        ));

        out.push('\n');
        out.push_str(&render_routes(&self.routes));

        if let Some(unconfined) = &self.unconfined {
            out.push('\n');
            out.push_str(&unconfined.render());
        }

        out.push('\n');
        out.push_str(&render_runtime_dir(&self.runtime_dir));
        out.push('\n');
        out.push_str(&render_records_dir(&self.records_dir));
        out.push('\n');
        out.push_str(&render_temp_dir(&self.temp_dir));
        out.push('\n');
        out.push_str(&crate::proc_view::render_process_view(&self.process_view));

        out.push_str(&render_not_protected(&self.filesystem_boundary));

        out.push_str("\nEffective grants\n");
        out.push_str(&format!(
            "  filesystem scope: {}\n",
            self.filesystem_scope.as_deref().unwrap_or("<none>")
        ));
        out.push_str(&format!("  workdir exec:     {}\n", self.workdir_exec));
        out.push_str(&render_read_only(
            &self.read_only_paths,
            &self.read_only_advisory_for,
        ));
        push_list(
            &mut out,
            "preopens",
            &self
                .preopens
                .iter()
                .map(PreopenReport::render)
                .collect::<Vec<_>>(),
        );
        push_list(&mut out, "network allow", &self.network_allow);
        out.push_str(&format!("  unix sockets:     {}\n", self.unix_sockets));
        push_list(&mut out, "shell allow", &self.shell_allow);
        push_list(&mut out, "spawn allow", &self.spawn_allow);
        push_list(
            &mut out,
            "spawn workdir snapshot",
            &self.spawn_workdir_snapshot,
        );
        push_list(&mut out, "env allow", &self.env_allow);
        out.push_str(&format!("  plan submit:      {}\n", self.plan_submit));
        out.push_str(&render_install(
            &self.install_skill,
            &self.install_tool,
            &self.install_from,
        ));
        push_list(
            &mut out,
            "interpreter runtime",
            &self.interpreter_runtime_grants,
        );
        push_list(
            &mut out,
            "derived interpreter runtime",
            &self.derived_interpreter_runtime,
        );
        push_list(&mut out, "trust store", &self.trust_store);
        push_list(
            &mut out,
            "declared needs",
            &self
                .declared_needs
                .iter()
                .flat_map(DeclaredNeedsReport::render_lines)
                .collect::<Vec<_>>(),
        );
        push_list(
            &mut out,
            "pinned programs",
            &self
                .pinned_programs
                .iter()
                .map(PinnedProgramReport::render_line)
                .collect::<Vec<_>>(),
        );
        push_list(&mut out, "name resolution", &self.name_resolution);
        push_list(
            &mut out,
            "host paths",
            &self
                .host_paths
                .iter()
                .map(crate::host_paths::HostPathReport::render)
                .collect::<Vec<_>>(),
        );
        push_list(&mut out, "staged runtime", &self.staged_runtime_grants);
        push_list(
            &mut out,
            "state stores",
            &self
                .state_stores
                .iter()
                .map(|store| format!("{}: {} -> {}", store.artifact, store.store, store.host_path))
                .collect::<Vec<_>>(),
        );
        push_list(&mut out, "artifact config", &self.configured_artifacts);

        if let Some(parent) = &self.parent_workdir {
            out.push_str(&render_parent_workdir(parent));
        }

        if let Some(images) = &self.images {
            out.push_str(&render_images(images));
        }

        out.push_str("\nResource plane\n");
        match &self.exports_files {
            None => out.push_str("  exports.files: <none>\n"),
            Some(export) => {
                out.push_str(&format!("  exports.files root: {}\n", export.root));
                out.push_str(&format!("  mode:               {}\n", export.mode));
                out.push_str(&format!(
                    "  max bytes:          {} (per file)\n",
                    export.max_bytes
                ));
            }
        }

        out.push_str(&format!(
            "  io.max:        {} ({} bytes/s declared)\n",
            match self.io_max.status {
                IoMaxStatus::Enforced => "enforced",
                IoMaxStatus::Unavailable => "unavailable",
                IoMaxStatus::NotRequired => "not-required",
                IoMaxStatus::NotProbed => "not-probed",
            },
            self.io_max.declared_bytes_per_sec
        ));
        if let Some(reason) = &self.io_max.reason {
            out.push_str(&format!("    {reason}\n"));
        }

        out.push_str("\nPeer handoff\n");
        match &self.peer_files {
            None => out.push_str("  exports.peer_files: <none>\n"),
            Some(peer) => {
                out.push_str(&format!("  exports.peer_files root: {}\n", peer.root));
                out.push_str(&format!(
                    "  max handle ttl:          {}s\n",
                    peer.max_ttl_secs
                ));
                out.push_str(&format!(
                    "  max bytes:               {} (per file)\n",
                    peer.max_bytes
                ));
            }
        }
        out.push_str(if self.peer_tasks {
            "  exports.peer_tasks: accept\n"
        } else {
            "  exports.peer_tasks: refuse\n"
        });
        push_list(&mut out, "peer_fetch allow", &self.peer_fetch_allow);

        out.push_str("\nRuntime writes\n");
        out.push_str(&crate::workdir_writes::render_runtime_writes(
            &self.runtime_writes,
        ));
        out.push_str(&crate::manifest_files::render_manifest_files(
            &self.manifest_files,
        ));

        if !self.floor_met {
            out.push_str(
                "\nThis is a report only — `mur run` without --explain-scope would refuse to \
                 launch here.\n",
            );
        } else if self.cgroup_scope.status == CgroupScopeStatus::Unavailable {
            out.push_str(
                "\nThis is a report only — `mur run` without --explain-scope would refuse to \
                 launch here with E-RUN-012: this capsule can run a subprocess and no \
                 cgroup scope can be created to bound it (see `cgroup scope:` above).\n",
            );
        }
        out
    }
}

/// The `Images` block of the scope report: the limits on images a tool returns or an A2A message
/// carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ImagesReport {
    /// `context.images.keep`: the newest images the driver receives as bytes.
    pub keep: u32,
    /// `context.images.max_bytes`: the largest image accepted.
    pub max_bytes: u64,
    /// `context.images.max_per_turn`: images accepted per model turn, or per tool call on
    /// `inference.transport: process`.
    pub max_per_turn: u32,
    /// Whether `keep` does anything: `false` on `inference.transport: process`, whose harness
    /// holds the conversation, and where `max_per_turn` counts the images of one tool call.
    pub keep_applies: bool,
}

impl ScopeReport {
    /// This report with [`Self::images`] set from `images`, for a capsule on `transport`; `None`
    /// is a capsule with no `inference:` block.
    #[must_use]
    pub fn with_images(
        mut self,
        images: murmur_artifact::ContextImagesConfig,
        transport: Option<&str>,
    ) -> Self {
        self.images = Some(ImagesReport {
            keep: images.keep,
            max_bytes: images.max_bytes,
            max_per_turn: images.max_per_turn,
            keep_applies: transport != Some("process"),
        });
        self
    }

    /// This report with [`Self::routes`] built again by [`route_table`], now that the caller
    /// knows the inference `transport` (`None` for a capsule with no `inference:` block).
    ///
    /// Reads [`Self::preopens`] for the capsule's tool, driver and hook entries, and the grants
    /// beside them, so the table and the rest of the report describe one capsule.
    #[must_use]
    pub fn with_routes(mut self, transport: Option<&str>) -> Self {
        let inputs = RouteInputs::new(
            &self.shell_allow,
            self.plan_submit,
            &self.install_tool,
            &self.preopens,
            transport,
        );
        self.routes = route_table(&inputs, self.enforcement_tier);
        self
    }

    /// This report with [`Self::declared_needs`] built from `needs`: what staging derives after
    /// it has read every tool's manifest, which is after the report is first built.
    #[must_use]
    pub fn with_declared_needs(
        mut self,
        needs: &[crate::declared_needs::ResolvedProgramNeeds],
    ) -> Self {
        self.declared_needs = needs.iter().map(DeclaredNeedsReport::from).collect();
        self
    }

    /// This report with [`Self::pinned_programs`] built from `programs`, staging's source choice
    /// for each pinned program download.
    #[must_use]
    pub fn with_pinned_programs(
        mut self,
        programs: &[crate::pinned_programs::ResolvedPinnedProgram],
    ) -> Self {
        self.pinned_programs = programs.iter().map(PinnedProgramReport::from).collect();
        self
    }

    /// This report with [`Self::manifest_files`] set to `rows`.
    #[must_use]
    pub fn with_manifest_files(
        mut self,
        rows: Vec<crate::manifest_files::ManifestFileReport>,
    ) -> Self {
        self.manifest_files = rows;
        self
    }

    /// This report with [`Self::process_view`] stating `view`, the `/proc` the session's probe
    /// decided its commands see.
    #[must_use]
    pub fn with_process_view(mut self, view: &crate::proc_view::ProcView) -> Self {
        self.process_view = crate::proc_view::process_view_report(view);
        self
    }

    /// This report with [`Self::cgroup_scope`] set from `cgroup_scope`.
    #[must_use]
    pub fn with_cgroup_scope(mut self, cgroup_scope: CgroupScopeReport) -> Self {
        self.cgroup_scope = cgroup_scope;
        self
    }

    /// This report with [`Self::parent_workdir`] set from `snapshot`, the restriction read off
    /// [`Self::filesystem_boundary`] so the two cannot disagree about one tier. `None` leaves it
    /// `null`.
    #[must_use]
    pub fn with_parent_workdir(
        mut self,
        snapshot: Option<crate::workdir_snapshot::WorkdirSnapshot>,
    ) -> Self {
        let restriction = self.filesystem_boundary.restriction;
        self.parent_workdir =
            snapshot.map(|snapshot| crate::workdir_snapshot::ParentWorkdirReport {
                snapshot,
                restriction,
            });
        self
    }
}

/// The `Images` block of `mur run --explain-scope`. Starts with a blank line and ends with a newline.
#[must_use]
pub fn render_images(images: &ImagesReport) -> String {
    let mut out = String::from("\nImages\n");
    out.push_str(&format!("  max_bytes:     {}\n", images.max_bytes));
    if images.keep_applies {
        out.push_str(&format!("  keep:          {}\n", images.keep));
        out.push_str(&format!("  max_per_turn:  {}\n", images.max_per_turn));
    } else {
        out.push_str(&format!(
            "  keep:          {} (does nothing here: the harness holds the conversation)\n",
            images.keep
        ));
        out.push_str(&format!(
            "  max_per_turn:  {} (counts the images of one tool call here)\n",
            images.max_per_turn
        ));
    }
    out
}

/// The `Parent work folder` block of `mur run --explain-scope`, for a delegated child whose work
/// folder started as a copy of its parent's. Starts with a blank line and ends with a newline.
#[must_use]
pub fn render_parent_workdir(parent: &crate::workdir_snapshot::ParentWorkdirReport) -> String {
    let snapshot = &parent.snapshot;
    let mut out = String::from("\nParent work folder\n");
    out.push_str(&format!("  source:      {}\n", snapshot.source));
    out.push_str(&format!(
        "  copied:      {} files, {} directories, {} symlinks ({} re-pointed), {} bytes\n",
        snapshot.files,
        snapshot.dirs,
        snapshot.symlinks,
        snapshot.repointed_symlinks,
        snapshot.bytes
    ));
    if snapshot.skipped_count == 0 {
        out.push_str("  skipped:     <none>\n");
    } else {
        out.push_str(&format!("  skipped:     {}\n", snapshot.skipped_count));
        for entry in &snapshot.skipped {
            let reason = serde_json::to_value(entry.reason)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default();
            out.push_str(&format!("    - {} ({reason})\n", entry.path));
        }
    }
    out.push_str(&format!(
        "  restriction: {}\n",
        parent.restriction_sentence()
    ));
    out
}

/// The allowlisted binaries a declared `capabilities.filesystem.read_only` is only advisory
/// against: those whose own file I/O the dispatch-time write-intent analyser cannot read, in
/// `shell_allow` order.
///
/// Exactly the set `W-SEC-017` fires one line for, resolved here so `--explain-scope`, `trace.jsonl`
/// and `mur doctor` cannot disagree with the warning printed beside them. Empty when nothing is
/// declared read-only: an interpreter is not advisory against a protection that does not exist.
#[must_use]
pub fn read_only_advisory_for(read_only_paths: &[String], shell_allow: &[String]) -> Vec<String> {
    if read_only_paths.is_empty() {
        return Vec::new();
    }
    shell_allow
        .iter()
        .filter(|binary| crate::protected_paths::is_advisory_interpreter(binary))
        .cloned()
        .collect()
}

/// The `read_only` block of the scope report: the declared paths, then one line saying whether the
/// protection is enforced for every readable call or advisory against a named binary.
///
/// Shared by [`ScopeReport::render`] and by `mur doctor` so the two cannot word the same grant
/// differently — the precedent [`PreopenReport::render`] sets. Ends with a newline; emits nothing
/// beyond the `<none>` line when no path is declared, because there is no enforcement question to
/// answer about a protection nobody asked for.
#[must_use]
pub fn render_read_only(read_only_paths: &[String], advisory_for: &[String]) -> String {
    let mut out = String::new();
    push_list(&mut out, "read_only", read_only_paths);
    if read_only_paths.is_empty() {
        return out;
    }
    if advisory_for.is_empty() {
        out.push_str(
            "  read_only enforcement: enforced for every tool call and every shell command the \
             dispatch check can read\n",
        );
    } else {
        out.push_str(&format!(
            "  read_only enforcement: advisory against {} — an interpreter's own file I/O is not \
             something the dispatch check can read; enforced for every tool call and for every \
             shell command it can read\n",
            advisory_for.join(", ")
        ));
    }
    out
}

/// The `install skill`, `install tool` and `install from` lines of the scope report: which
/// skills and tools a pull may install, `<none>` for a list the capsule left empty, and the
/// places it may install them from (`install from: stores` or `install from: stores, sources`),
/// printed only for a capsule holding an install grant.
///
/// Shared by [`ScopeReport::render`] and by `mur doctor`, on [`render_read_only`]'s terms. Ends
/// with a newline.
#[must_use]
pub fn render_install(
    install_skill: &[String],
    install_tool: &[String],
    install_from: &[String],
) -> String {
    let mut out = String::new();
    push_list(&mut out, "install skill", install_skill);
    push_list(&mut out, "install tool", install_tool);
    if !install_from.is_empty() {
        out.push_str(&format!("  install from: {}\n", install_from.join(", ")));
    }
    out
}

/// [`ScopeReport::install_from`] for `policy`: empty without an install grant.
#[must_use]
pub fn install_from_places(policy: &CapabilityPolicy) -> Vec<String> {
    if !crate::install_grant::is_granted(policy) {
        return Vec::new();
    }
    let mut places = vec!["stores".to_string()];
    if policy.install_from_sources {
        places.push("sources".to_string());
    }
    places
}

/// The `Not protected here` block: one line per statement in
/// [`FilesystemBoundaryReport::not_protected`], and nothing at all when that list is empty.
///
/// A heading with no entries under it would read as a disclaimer the reader had to interpret, so a
/// composed root prints no heading rather than an empty one. Every line is a statement from the report
/// verbatim, so the human rendering and `--explain-scope --json` say the same thing in the same
/// words.
fn render_not_protected(boundary: &FilesystemBoundaryReport) -> String {
    if boundary.not_protected.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nNot protected here\n");
    for statement in &boundary.not_protected {
        out.push_str(&format!("  - {statement}\n"));
    }
    out
}

fn push_list(out: &mut String, label: &str, values: &[String]) {
    if values.is_empty() {
        out.push_str(&format!("  {label}: <none>\n"));
        return;
    }
    out.push_str(&format!("  {label}:\n"));
    for value in values {
        out.push_str(&format!("    - {value}\n"));
    }
}

/// Builds a [`ScopeReport`] for `policy` against this host, with `declared` as the already-
/// combined floor. Takes one [`crate::sandbox::HostProbe`], so the tier, the shortfall and the
/// grant it reports all describe one host at one moment.
///
/// `state_stores`, `configured_artifacts` and `preopens` are resolved by the caller (from the
/// manifest's artifact entries, via [`crate::state_store::state_store_reports`],
/// [`crate::artifact_config::configured_artifact_names`] and
/// [`crate::network_policy::preopen_reports`]) rather than derived here, because all three are
/// per-artifact and `policy` is capsule-wide. No resolution creates anything, so this stays a
/// read-only diagnostic.
///
/// `workdir` is the workdir root the launch would use — `--workdir`, or `<manifest dir>/workdir`
/// without one — which names the records group and decides whether the runtime or records
/// directory lies inside it. Nothing is created to answer that; a `HOME` that cannot hold either
/// is refused, as the launch would be.
#[allow(clippy::too_many_arguments)]
pub fn explain_scope(
    policy: &CapabilityPolicy,
    declared: ContainmentClass,
    exports: Option<&Exports>,
    state_stores: Vec<StateStoreReport>,
    configured_artifacts: Vec<String>,
    preopens: Vec<PreopenReport>,
    io_max: IoMaxReport,
    workdir: &std::path::Path,
) -> Result<ScopeReport, crate::murmur_home::HomeUnusable> {
    let placement = crate::runtime_dir::runtime_dir_placement(workdir)?;
    let probe = HostProbe::probe();
    let decision = ContainmentDecision::decide(
        probe.tier(),
        declared,
        policy.workdir_exec_allowed,
        placement.inside_workdir(),
    );
    let temp_mapping = crate::temp_dir::temp_mapping(
        decision.applied_tier(),
        probe.mount_namespace_usable(),
        &placement.workdir,
    );
    Ok(scope_report_for_tier(
        policy,
        &decision,
        probe.sealed_shortfall().as_ref(),
        probe.userns_grant(),
        probe.landlock_kernel_abi(),
        exports,
        state_stores,
        configured_artifacts,
        preopens,
        &placement,
        temp_mapping,
        io_max,
    )
    .with_process_view(&crate::proc_view::proc_view(
        decision.applied_tier(),
        probe.namespaces(),
    )))
}

/// `mur doctor`'s `Runtime directory`, `Records directory` and `Temporary folder` sections for a
/// capsule declaring `declared`, run from `workdir`: the same [`runtime_dir_report`],
/// [`records_dir_report`] and [`temp_dir_report`] on the tier `probe`'s [`ContainmentDecision`]
/// applies, its ruleset ABI and mount namespace, or why `HOME` cannot hold them.
///
/// `probe` is the reading the rest of the doctor run reports from, so every section describes one
/// host at one moment.
pub fn doctor_runtime_dir(
    probe: &HostProbe,
    declared: ContainmentClass,
    workdir: &std::path::Path,
) -> Result<DoctorDirReports, crate::murmur_home::HomeUnusable> {
    let placement = crate::runtime_dir::runtime_dir_placement(workdir)?;
    // `workdir_exec` and the placement cap the class, never the tier, and only the tier is read.
    let applied = ContainmentDecision::decide(probe.tier(), declared, false, false).applied_tier();
    let abi = crate::sandbox::ruleset_landlock_abi(probe.landlock_kernel_abi(), applied);
    let temp_mapping =
        crate::temp_dir::temp_mapping(applied, probe.mount_namespace_usable(), &placement.workdir);
    Ok(DoctorDirReports {
        runtime_dir: runtime_dir_report(applied, abi, &placement),
        records_dir: records_dir_report(applied, abi, &placement),
        temp_dir: temp_dir_report(temp_mapping, &placement),
        process_view: crate::proc_view::process_view_report(&crate::proc_view::proc_view(
            applied,
            probe.namespaces(),
        )),
        placement,
    })
}

/// What [`doctor_runtime_dir`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorDirReports {
    pub runtime_dir: RuntimeDirReport,
    pub records_dir: RuntimeDirReport,
    pub temp_dir: TempDirReport,
    pub process_view: crate::proc_view::ProcessViewReport,
    pub placement: RuntimeDirPlacement,
}

/// [`explain_scope`] with the [`ContainmentDecision`] and the sealed shortfall injected — the
/// seam `runtime::stage_session` builds the session's report through, and every test, through
/// [`report_for`], so no test depends on the host it happens to run on.
///
/// `decision` must have been decided with `policy.workdir_exec_allowed` and
/// `runtime_dir_placement.inside_workdir()`: the report states both beside the class they cap.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scope_report_for_tier(
    policy: &CapabilityPolicy,
    decision: &ContainmentDecision,
    sealed_shortfall: Option<&SealedShortfall>,
    userns_grant: Option<UsernsGrant>,
    landlock_kernel_abi: Option<u8>,
    exports: Option<&Exports>,
    state_stores: Vec<StateStoreReport>,
    configured_artifacts: Vec<String>,
    preopens: Vec<PreopenReport>,
    runtime_dir_placement: &RuntimeDirPlacement,
    temp_mapping: crate::temp_dir::TempMapping,
    io_max: IoMaxReport,
) -> ScopeReport {
    debug_assert_eq!(
        *decision,
        ContainmentDecision::decide(
            decision.host_tier,
            decision.declared,
            policy.workdir_exec_allowed,
            runtime_dir_placement.inside_workdir(),
        ),
        "the decision must be made from the policy and placement this report states"
    );
    let declared = decision.declared();
    let achieved = decision.achieved();
    // The tier `ShellEnforcement::resolve` installs from the same decision. The mechanism fields
    // below are keyed on it rather than on `achieved`, which `workdir_exec` and the placement can
    // cap without changing what the mechanism restricts.
    let applied = decision.applied_tier();
    let shortfall_reason = containment_shortfall_reason(
        declared,
        achieved,
        sealed_shortfall,
        policy.workdir_exec_allowed,
        runtime_dir_placement.inside(),
    );
    let landlock_abi = crate::sandbox::ruleset_landlock_abi(landlock_kernel_abi, applied);
    let runtime_dir = runtime_dir_report(applied, landlock_abi, runtime_dir_placement);
    let records_dir = records_dir_report(applied, landlock_abi, runtime_dir_placement);
    let mut filesystem_boundary = filesystem_boundary(applied);
    let enforcement_tier = decision.enforcement_tier();
    // Built again by `with_routes` once the caller knows the transport.
    let routes = route_table(
        &RouteInputs::new(
            &policy.shell_allow,
            policy.plan_submit,
            &policy.install_tool,
            &preopens,
            None,
        ),
        enforcement_tier,
    );
    // On the unconfined tier the boundary's one statement already covers both directories.
    if applied != EnforcementTier::Unconfined {
        for report in [&runtime_dir, &records_dir] {
            if let Some(reason) = &report.not_protected_because {
                filesystem_boundary.not_protected.push(reason.clone());
            }
        }
    }
    let unconfined = (applied == EnforcementTier::Unconfined)
        .then(|| UnconfinedReport::for_this_process(&policy.declared_resources));

    ScopeReport {
        declared_containment: declared,
        achieved_containment: achieved,
        host_containment: decision.host_containment(),
        floor_met: shortfall_reason.is_none(),
        shortfall_reason,
        enforcement_tier,
        routes,
        userns_grant,
        landlock_abi,
        // Copied straight through and never consulted above: `achieved` is already computed, and
        // no export or peer grant may reach it.
        exports_files: exports
            .and_then(|exports| exports.files.as_ref())
            .map(ExportsFilesReport::from),
        peer_files: exports
            .and_then(|exports| exports.peer_files.as_ref())
            .map(PeerFilesReport::from),
        peer_tasks: exports.is_some_and(Exports::accepts_peer_tasks),
        peer_fetch_allow: policy.peer_fetch_allow.clone(),
        // Resolved by `state_store::state_store_reports` before this call and copied through on
        // the same terms as the export reports above: a grant that opens a path outside the
        // workdir still gets no say in what class the session is reported to achieve.
        state_stores,
        // Resolved by `artifact_config::configured_artifact_names` before this call and copied
        // through on the same terms: one environment variable delivered to one guest gets no say
        // in what class the session is reported to achieve.
        configured_artifacts,
        filesystem_scope: policy.filesystem_scope.clone(),
        workdir_exec: policy.workdir_exec_allowed,
        read_only_paths: policy.read_only_paths.clone(),
        read_only_advisory_for: read_only_advisory_for(
            &policy.read_only_paths,
            &policy.shell_allow,
        ),
        // Resolved by `network_policy::preopen_reports` before this call and copied through on the
        // same terms as the reports above: what one guest is preopened into gets no say in what
        // class the session is reported to achieve.
        preopens,
        network_allow: policy.network_allow.clone(),
        unix_sockets: policy.unix_sockets_allowed,
        shell_allow: policy.shell_allow.clone(),
        spawn_allow: policy.spawn_allow.clone(),
        spawn_workdir_snapshot: policy.spawn_workdir_snapshot.clone(),
        // Set by the caller that knows it, through `with_parent_workdir`: the snapshot is what
        // the launching parent did, not something the policy declares.
        parent_workdir: None,
        // Set through `with_images` by the caller that holds the manifest's transport.
        images: None,
        env_allow: policy.env_allow.clone(),
        plan_submit: policy.plan_submit,
        install_skill: policy.install_skill.clone(),
        install_tool: policy.install_tool.clone(),
        install_from: install_from_places(policy),
        interpreter_runtime_grants: policy
            .shell_interpreter_runtime
            .iter()
            .flat_map(|grant| {
                grant.dirs.iter().map(move |dir| {
                    let listable = if dir.list_dir { " (list_dir)" } else { "" };
                    format!("{}: {}{}", grant.binary, dir.path, listable)
                })
            })
            .collect(),
        derived_interpreter_runtime: policy
            .shell_derived_interpreter_runtime
            .iter()
            .flat_map(|runtime| {
                let binary = runtime.binary.display();
                runtime
                    .entries()
                    .map(|(path, is_dir)| {
                        let listable = if is_dir { " (list_dir)" } else { "" };
                        format!("{binary}: {}{listable}", path.display())
                    })
                    .chain(
                        runtime
                            .libraries
                            .iter()
                            .map(|library| format!("{binary}: {}", library.display())),
                    )
                    .collect::<Vec<_>>()
            })
            .collect(),
        // Keyed on the applied tier: the Landlock tiers grant these files whatever the manifest
        // says, and every other tier grants nothing.
        name_resolution: match applied {
            EnforcementTier::KernelFull | EnforcementTier::KernelSealed => {
                crate::sandbox::SCOPED_NAME_RESOLUTION_PATHS
                    .iter()
                    .map(|path| (*path).to_string())
                    .collect()
            }
            _ => Vec::new(),
        },
        declared_needs: policy
            .shell_declared_needs
            .iter()
            .map(DeclaredNeedsReport::from)
            .collect(),
        pinned_programs: policy
            .shell_pinned_programs
            .iter()
            .map(PinnedProgramReport::from)
            .collect(),
        trust_store: policy
            .shell_trust_store
            .iter()
            .map(crate::trust_store::TrustStorePath::render)
            .collect(),
        // Keyed on the applied tier, like `filesystem_boundary`: what scopes a host path is the
        // mechanism this session installs, not the class it achieves.
        host_paths: crate::host_paths::host_path_reports(
            &policy.host_paths,
            crate::host_paths::HostPathMechanism::for_enforcement_tier(decision.enforcement_tier()),
        ),
        staged_runtime_grants: policy
            .shell_staged_runtime
            .iter()
            .map(|grant| {
                format!(
                    "{}: {} (pin: {})",
                    grant.binary, grant.source_path, grant.pin
                )
            })
            .collect(),
        // Nothing here touches the filesystem: `--explain-scope` returns before a workdir exists,
        // and a diagnostic that created the directory it describes would be answering a question
        // it had changed.
        runtime_writes: crate::workdir_writes::runtime_writes(),
        // Set through `with_manifest_files` by the caller that knows where the manifest's
        // relative paths resolve.
        manifest_files: Vec::new(),
        // Established by performing the `io.max` write, never inferred: `prepare_scope` fills it
        // at launch and `cgroup::probe_scope` fills it under `--explain-scope`. Like every field
        // above it, it reaches no class and no floor.
        io_max,
        // Set by the caller that performed the operation, through `with_cgroup_scope`: the launch
        // from `prepare_scope`, `--explain-scope` from `probe_scope`.
        cgroup_scope: CgroupScopeReport::default(),
        // Derived here, from the `applied` tier already resolved above, rather than filled by
        // either caller: `--explain-scope --json` and `session_start.effective_grants` are the
        // same object, and a field either of them worded for itself would make them differ.
        filesystem_boundary,
        runtime_dir,
        records_dir,
        temp_dir: temp_dir_report(temp_mapping, runtime_dir_placement),
        // Until a caller holding the session's probe states it through `with_process_view`, the
        // report claims no private view.
        process_view: crate::proc_view::process_view_report(&crate::proc_view::tier_proc_view(
            applied,
        )),
        unconfined,
    }
}

/// [`scope_report_for_tier`] for a capsule declaring `declared` on a host whose tier is `tier`,
/// deciding from `policy` and `runtime_dir_placement` as `runtime::stage_session` does.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn report_for(
    policy: &CapabilityPolicy,
    declared: ContainmentClass,
    tier: EnforcementTier,
    sealed_shortfall: Option<&SealedShortfall>,
    userns_grant: Option<UsernsGrant>,
    landlock_kernel_abi: Option<u8>,
    exports: Option<&Exports>,
    state_stores: Vec<StateStoreReport>,
    configured_artifacts: Vec<String>,
    preopens: Vec<PreopenReport>,
    runtime_dir_placement: &RuntimeDirPlacement,
    temp_mapping: crate::temp_dir::TempMapping,
    io_max: IoMaxReport,
) -> ScopeReport {
    let decision = ContainmentDecision::decide(
        tier,
        declared,
        policy.workdir_exec_allowed,
        runtime_dir_placement.inside_workdir(),
    );
    scope_report_for_tier(
        policy,
        &decision,
        sealed_shortfall,
        userns_grant,
        landlock_kernel_abi,
        exports,
        state_stores,
        configured_artifacts,
        preopens,
        runtime_dir_placement,
        temp_mapping,
        io_max,
    )
}

/// The `mechanism` of a [`RouteReport`] whose route the session's class does not cover: the code
/// runs as part of the `mur` process, or as a child it starts outside the class.
pub const HOST_PROCESS_MECHANISM: &str = "host-process";

/// The `note` of a [`RouteReport`] for a route [`Route::runs_unconfined`] on the unconfined tier.
const UNCONFINED_ROUTE_NOTE: &str = "Runs as the operator, with their HOME, whole environment, \
     filesystem, network and sudo; nothing holds it.";

/// One route a capsule can run code on, as [`RouteReport::route`] names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    /// A `capabilities.shell.allow` command the model runs.
    Shell,
    /// A plan's shell step.
    PlanShellSteps,
    /// A WASM `runtime: tool` artifact, declared or pulled.
    WasmTools,
    /// A `runtime: driver` artifact.
    Drivers,
    /// A `runtime: hook` artifact.
    Hooks,
    /// The coding harness `inference.transport: process` runs.
    ProcessHarness,
    /// The tools that harness runs on its own.
    HarnessTools,
}

impl Route {
    /// Every route, in the order [`route_table`] reports them.
    pub const ALL: [Self; 7] = [
        Self::Shell,
        Self::PlanShellSteps,
        Self::WasmTools,
        Self::Drivers,
        Self::Hooks,
        Self::ProcessHarness,
        Self::HarnessTools,
    ];

    /// The route's name in `--explain-scope`'s text, `mur doctor` and `W-SEC-034`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell commands",
            Self::PlanShellSteps => "plan shell steps",
            Self::WasmTools => "WASM tools",
            Self::Drivers => "drivers",
            Self::Hooks => "hooks",
            Self::ProcessHarness => "the process harness",
            Self::HarnessTools => "harness tools",
        }
    }

    /// Whether the session's class covers this route: exactly the routes spawned through
    /// `sandbox::prepare_enforcement`, the one path that installs the session's tier. This follows
    /// where each route is spawned, not a policy: confining another route means flipping its arm
    /// here.
    #[must_use]
    pub fn under_class(self) -> bool {
        match self {
            Self::Shell | Self::PlanShellSteps => true,
            Self::WasmTools
            | Self::Drivers
            | Self::Hooks
            | Self::ProcessHarness
            | Self::HarnessTools => false,
        }
    }

    /// Whether an unconfined session runs this route as the operator: the routes
    /// [`UNCONFINED_APPLIES_TO`] names.
    #[must_use]
    pub fn runs_unconfined(self) -> bool {
        match self {
            Self::Shell | Self::PlanShellSteps | Self::ProcessHarness => true,
            Self::WasmTools | Self::Drivers | Self::Hooks | Self::HarnessTools => false,
        }
    }

    /// What an uncovered route does run under, as [`RouteReport::note`] states it; `None` for a
    /// covered route, which the tier's mechanism holds.
    #[must_use]
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Shell | Self::PlanShellSteps => None,
            Self::WasmTools => Some(
                "Runs inside the mur process tree, in a guest worker held by its WASI preopens and \
                 the host-side network allowlist only.",
            ),
            Self::Drivers => Some(
                "Runs inside the mur process tree, in a guest worker held by its WASI preopens and \
                 the host-side network allowlist only.",
            ),
            Self::Hooks => Some(
                "Runs inside the mur process tree, in a guest worker held only by the preopens \
                 and network grants its own capabilities block declares.",
            ),
            Self::ProcessHarness => Some(
                "A child of mur with a cleared environment and nothing else: no Landlock, \
                 seccomp, network namespace or rlimits, in a cgroup with no limits.",
            ),
            Self::HarnessTools => Some(
                "Run by the harness with the user's authority, kept off only by the arguments \
                 the process driver passes.",
            ),
        }
    }
}

/// One row of [`ScopeReport::routes`]: whether this capsule has the route, and what holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteReport {
    /// Which route, as a kebab-case wire name.
    pub route: Route,
    /// Whether this capsule has the route.
    pub present: bool,
    /// Whether the session's class covers the route, from [`Route::under_class`].
    pub under_class: bool,
    /// [`ScopeReport::enforcement_tier`]'s wire name when [`Self::under_class`], else
    /// [`HOST_PROCESS_MECHANISM`]. `unconfined` on the unconfined tier for every route
    /// [`Route::runs_unconfined`].
    pub mechanism: &'static str,
    /// What an uncovered route runs under, from [`Route::note`]; `null` when covered, except on
    /// the unconfined tier, where a route [`Route::runs_unconfined`] says nothing holds it.
    /// Always serialized.
    pub note: Option<&'static str>,
    /// The artifacts on the route, in manifest order: the tool, driver or hook entries it runs.
    /// Empty for a route no artifact names. Always serialized.
    pub artifacts: Vec<String>,
}

/// What [`route_table`] decides presence from, gathered by [`RouteInputs::new`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteInputs {
    /// `capabilities.shell.allow` is non-empty.
    pub shell_allow: bool,
    /// `capabilities.plan.submit`.
    pub plan_submit: bool,
    /// `capabilities.install.tool` is non-empty, so a pull may install a tool.
    pub install_tool: bool,
    /// Every `runtime: tool` entry.
    pub tools: Vec<String>,
    /// Every `runtime: driver` entry.
    pub drivers: Vec<String>,
    /// Every `runtime: hook` entry.
    pub hooks: Vec<String>,
    /// `inference.transport` is `process`.
    pub process_transport: bool,
}

impl RouteInputs {
    /// The inputs for a capsule with these grants, these resolved `preopens` (one per tool,
    /// driver and hook entry) and the inference `transport`.
    #[must_use]
    pub fn new(
        shell_allow: &[String],
        plan_submit: bool,
        install_tool: &[String],
        preopens: &[PreopenReport],
        transport: Option<&str>,
    ) -> Self {
        let named = |role: murmur_artifact::ArtifactRuntime| {
            preopens
                .iter()
                .filter(|preopen| preopen.role == role.as_str())
                .map(|preopen| preopen.artifact.clone())
                .collect::<Vec<_>>()
        };
        Self {
            shell_allow: !shell_allow.is_empty(),
            plan_submit,
            install_tool: !install_tool.is_empty(),
            tools: named(murmur_artifact::ArtifactRuntime::Tool),
            drivers: named(murmur_artifact::ArtifactRuntime::Driver),
            hooks: named(murmur_artifact::ArtifactRuntime::Hook),
            process_transport: transport == Some("process"),
        }
    }
}

/// The route table: one [`RouteReport`] per [`Route::ALL`] entry, in that order, for a capsule
/// with `inputs` whose session installs the tier named `enforcement_tier`.
///
/// The only place a route's presence, coverage and mechanism are decided. `--explain-scope`,
/// `session_start.effective_grants` and `mur doctor` all read its rows, and `W-SEC-034` decides
/// from them.
#[must_use]
pub fn route_table(inputs: &RouteInputs, enforcement_tier: &'static str) -> Vec<RouteReport> {
    let unconfined = enforcement_tier == enforcement_tier_name(EnforcementTier::Unconfined);
    Route::ALL
        .into_iter()
        .map(|route| {
            let (present, artifacts) = match route {
                Route::Shell => (inputs.shell_allow, Vec::new()),
                // A plan's shell step runs only a binary `shell.allow` names (see `plan.rs`).
                Route::PlanShellSteps => (inputs.plan_submit && inputs.shell_allow, Vec::new()),
                Route::WasmTools => (
                    !inputs.tools.is_empty() || inputs.install_tool,
                    inputs.tools.clone(),
                ),
                Route::Drivers => (!inputs.drivers.is_empty(), inputs.drivers.clone()),
                Route::Hooks => (!inputs.hooks.is_empty(), inputs.hooks.clone()),
                Route::ProcessHarness | Route::HarnessTools => {
                    (inputs.process_transport, Vec::new())
                }
            };
            if unconfined && route.runs_unconfined() {
                return RouteReport {
                    route,
                    present,
                    under_class: true,
                    mechanism: enforcement_tier,
                    note: Some(UNCONFINED_ROUTE_NOTE),
                    artifacts,
                };
            }
            let under_class = route.under_class();
            RouteReport {
                route,
                present,
                under_class,
                mechanism: if under_class {
                    enforcement_tier
                } else {
                    HOST_PROCESS_MECHANISM
                },
                note: route.note(),
                artifacts,
            }
        })
        .collect()
}

/// The `Routes` block `--explain-scope` and `mur doctor` print: one line per route, saying
/// whether the capsule has it and what holds it, with an uncovered present route's note under
/// it. Starts with `Routes` and ends with a newline.
#[must_use]
pub fn render_routes(routes: &[RouteReport]) -> String {
    let mut out = String::from("Routes\n");
    for row in routes {
        let label = match row.route {
            Route::ProcessHarness => "process harness",
            route => route.label(),
        };
        let presence = if row.present { "present" } else { "absent" };
        let mechanism = if row.under_class {
            row.mechanism
        } else {
            "host process, not confined by the class"
        };
        let artifacts = if row.artifacts.is_empty() {
            String::new()
        } else {
            format!(" ({})", row.artifacts.join(", "))
        };
        out.push_str(&format!(
            "  {label:<17} {presence:<8} {mechanism}{artifacts}\n"
        ));
        if let (true, Some(note)) = (row.present, row.note) {
            out.push_str(&format!("    {note}\n"));
        }
    }
    out
}

/// The `W-SEC-034` message for a capsule declaring `declared` with `routes`, or `None` when it
/// does not fire: below a `scoped` floor, or when no present route lies outside the class.
///
/// Pure, split from [`warn_on_uncovered_routes`] on [`crate::sandbox::tier_warning`]'s terms.
#[must_use]
pub fn uncovered_route_warning(
    declared: ContainmentClass,
    routes: &[RouteReport],
) -> Option<String> {
    if declared < ContainmentClass::Scoped {
        return None;
    }
    let uncovered: Vec<String> = routes
        .iter()
        .filter(|row| row.present && !row.under_class)
        .map(|row| {
            if row.artifacts.is_empty() {
                row.route.label().to_string()
            } else {
                format!("{} ({})", row.route.label(), row.artifacts.join(", "))
            }
        })
        .collect();
    let named = match uncovered.as_slice() {
        [] => return None,
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    Some(format!(
        "the declared {declared} floor does not confine every route this capsule runs code on: \
         {named} run outside the class, with none of its Landlock, seccomp or network namespace \
         around them (`mur run --explain-scope` lists what holds each under Routes). The class \
         confines shell commands and plan shell steps; nothing is refused"
    ))
}

/// Prints `W-SEC-034` when [`uncovered_route_warning`] fires: to stderr, and to the session's
/// bootstrap log when `bootstrap_log` names a records directory. The one emitter the launch and
/// `mur doctor` (with `None`) both call.
pub fn warn_on_uncovered_routes(
    declared: ContainmentClass,
    routes: &[RouteReport],
    bootstrap_log: Option<&crate::workdir_fs::WorkdirRoot>,
) {
    let Some(message) = uncovered_route_warning(declared, routes) else {
        return;
    };
    let code = murmur_artifact::W_SEC_034;
    let link = murmur_artifact::security_warning_link(code);
    crate::runtime_err!("[capsule-runtime] warning[{code}]: {message} ({link})");
    if let Some(records) = bootstrap_log {
        crate::agent::append_bootstrap_log(
            records,
            &format!("[capability-policy] warning[{code}]: {message} ({link})"),
        );
    }
}

/// The wire name of the tier a shell command of a capsule declaring `declared` runs under on the
/// host `probe` read: [`ContainmentDecision::enforcement_tier`], which is what
/// `ShellEnforcement::resolve` installs for every subprocess. Spelled as
/// [`ScopeReport::enforcement_tier`] spells a tier.
pub fn applied_mechanism(probe: &HostProbe, declared: ContainmentClass) -> &'static str {
    // `workdir_exec` and the placement cap the class, never the tier, and only the tier is read.
    ContainmentDecision::decide(probe.tier(), declared, false, false).enforcement_tier()
}

/// Stable wire name for a tier, so `--explain-scope --json` consumers see the mechanism rather
/// than a `Debug` rendering that is free to change.
fn enforcement_tier_name(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::KernelSealed => "mountns+pivot_root+landlock+seccomp",
        EnforcementTier::KernelFull => "landlock+seccomp",
        EnforcementTier::KernelSeccompOnly => "seccomp-only",
        EnforcementTier::EnvironmentOnly => "none",
        EnforcementTier::Unconfined => "unconfined",
    }
}

#[cfg(test)]
#[allow(clippy::print_stdout, clippy::print_stderr)]
mod tests {
    use super::*;
    use crate::sealed::SealedBlocker;

    /// A shortfall of `blocker` with no failed attempt, whose reason is the class sentence alone.
    fn blocked(blocker: SealedBlocker) -> SealedShortfall {
        SealedShortfall {
            blocker,
            failed_call: None,
            errno: None,
            userns_grant: UsernsGrant::Withheld,
            attachment: crate::sealed::ProfileAttachment::NotReported,
            profile_gap: crate::sealed::ProfileGap::Undetermined,
        }
    }
    use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant, StagedRuntimeGrant};

    /// Every tier, so a new variant cannot quietly escape a test that iterates "all of them".
    const ALL_TIERS: &[EnforcementTier] = &[
        EnforcementTier::KernelSealed,
        EnforcementTier::KernelFull,
        EnforcementTier::KernelSeccompOnly,
        EnforcementTier::EnvironmentOnly,
    ];

    #[test]
    fn kernel_full_achieves_scoped() {
        assert_eq!(
            achieved_class_for_tier(EnforcementTier::KernelFull),
            ContainmentClass::Scoped
        );
    }

    /// The sealed tier, and only the sealed tier, reports `sealed`.
    #[test]
    fn only_the_sealed_tier_achieves_sealed() {
        assert_eq!(
            achieved_class_for_tier(EnforcementTier::KernelSealed),
            ContainmentClass::Sealed
        );
        for tier in ALL_TIERS
            .iter()
            .filter(|t| **t != EnforcementTier::KernelSealed)
        {
            assert_ne!(achieved_class_for_tier(*tier), ContainmentClass::Sealed);
        }
    }

    #[test]
    fn tiers_without_landlock_achieve_only_advisory() {
        assert_eq!(
            achieved_class_for_tier(EnforcementTier::KernelSeccompOnly),
            ContainmentClass::Advisory
        );
        assert_eq!(
            achieved_class_for_tier(EnforcementTier::EnvironmentOnly),
            ContainmentClass::Advisory
        );
    }

    /// The whole tier × `workdir_exec` truth table, with both inputs supplied by the test and no
    /// kernel consulted — the seam `achieved_class_for_tier`/`achieved_containment_class` exists
    /// for. This is emphatically *not* a test of the security property (that a workdir binary
    /// cannot execute); it tests only the class arithmetic, which is all that can honestly be
    /// tested without a Landlock-capable host.
    #[test]
    fn workdir_exec_caps_the_achieved_class_at_advisory_on_every_tier() {
        // Without the declaration: the host ceiling, unchanged.
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelSealed, false, false),
            ContainmentClass::Sealed
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelFull, false, false),
            ContainmentClass::Scoped
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelSeccompOnly, false, false),
            ContainmentClass::Advisory
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::EnvironmentOnly, false, false),
            ContainmentClass::Advisory
        );

        // With it: advisory, whatever the host could otherwise back.
        for tier in ALL_TIERS {
            assert_eq!(
                achieved_containment_class(*tier, true, false),
                ContainmentClass::Advisory,
                "workdir_exec must cap tier {tier:?} at advisory"
            );
        }
    }

    /// The cap goes in the wrapper, never in the tier-only function — a regression here would mean
    /// a manifest key had leaked into the one mapping that must stay host-only.
    #[test]
    fn the_tier_only_mapping_is_unaffected_by_the_cap() {
        for tier in ALL_TIERS {
            assert_eq!(
                achieved_class_for_tier(*tier),
                achieved_containment_class(*tier, false, false),
                "the tier-only mapping and the uncapped wrapper must agree on tier {tier:?}"
            );
        }
    }

    /// The refusal an operator actually hits: `workdir_exec: true` plus
    /// `capabilities.containment: scoped`, on a host that could otherwise back `scoped`. It must
    /// refuse, and the reason must send them to their manifest rather than to another machine.
    #[test]
    fn workdir_exec_refuses_a_scoped_floor_even_on_a_landlock_capable_host() {
        let achieved = achieved_containment_class(EnforcementTier::KernelFull, true, false);
        assert_eq!(achieved, ContainmentClass::Advisory);

        let error = check_containment_floor(ContainmentClass::Scoped, achieved, None, true, None)
            .expect_err("workdir_exec + scoped must refuse");
        match error {
            RuntimeError::ContainmentFloorUnmet {
                declared,
                achieved,
                reason,
                ..
            } => {
                assert_eq!(declared, ContainmentClass::Scoped);
                assert_eq!(achieved, ContainmentClass::Advisory);
                assert!(
                    reason.contains("workdir_exec"),
                    "the reason must name the manifest key, got: {reason}"
                );
                assert!(
                    !reason.contains("this host provides no kernel filesystem mediation"),
                    "the reason must not blame the host for a manifest declaration: {reason}"
                );
            }
            other => panic!("expected ContainmentFloorUnmet, got {other:?}"),
        }
    }

    /// `sealed` gets the same manifest-side reason rather than a `SealedBlocker` one: a probed
    /// blocker is real but is not what is stopping *this* capsule, and reporting it would send the
    /// operator to install an AppArmor profile that changes nothing.
    #[test]
    fn workdir_exec_outranks_a_probed_sealed_blocker_in_the_reason() {
        let reason = containment_shortfall_reason(
            ContainmentClass::Sealed,
            ContainmentClass::Advisory,
            Some(&blocked(SealedBlocker::AppArmorProfileMissing)),
            true,
            None,
        )
        .expect("refusal");
        assert!(reason.contains("workdir_exec"));
        assert!(!reason.contains("apparmor_parser"));
    }

    /// The declaration is not itself a refusal: a capsule that declares `workdir_exec` and asks
    /// for nothing above `advisory` launches normally on every host.
    #[test]
    fn workdir_exec_alone_refuses_nothing() {
        for tier in ALL_TIERS {
            let achieved = achieved_containment_class(*tier, true, false);
            assert!(
                check_containment_floor(ContainmentClass::Advisory, achieved, None, true, None)
                    .is_ok(),
                "workdir_exec at an advisory floor must launch on tier {tier:?}"
            );
        }
    }

    #[test]
    fn advisory_floor_is_met_on_every_tier() {
        for tier in ALL_TIERS {
            let achieved = achieved_class_for_tier(*tier);
            assert!(check_containment_floor(
                ContainmentClass::Advisory,
                achieved,
                None,
                false,
                None
            )
            .is_ok());
        }
    }

    #[test]
    fn scoped_floor_is_met_on_every_landlock_capable_tier() {
        for tier in [EnforcementTier::KernelFull, EnforcementTier::KernelSealed] {
            let achieved = achieved_class_for_tier(tier);
            assert!(
                check_containment_floor(ContainmentClass::Scoped, achieved, None, false, None)
                    .is_ok()
            );
        }

        for tier in [
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let achieved = achieved_class_for_tier(tier);
            let error =
                check_containment_floor(ContainmentClass::Scoped, achieved, None, false, None)
                    .expect_err("scoped must refuse without Landlock");
            match error {
                RuntimeError::ContainmentFloorUnmet {
                    declared,
                    achieved,
                    reason,
                    ..
                } => {
                    assert_eq!(declared, ContainmentClass::Scoped);
                    assert_eq!(achieved, ContainmentClass::Advisory);
                    assert!(
                        reason.contains("Landlock"),
                        "reason must name the missing mechanism, got: {reason}"
                    );
                }
                other => panic!("expected ContainmentFloorUnmet, got {other:?}"),
            }
        }
    }

    /// The edge case the sealed tier makes possible: a capsule declaring a *weaker* class on a
    /// sealed-capable host. It runs on `landlock+seccomp` and achieves `scoped` — never the
    /// `sealed` the host could back — and that still meets its floor.
    #[test]
    fn a_weaker_declaration_is_satisfied_by_a_sealed_host_without_being_upgraded() {
        for declared in [ContainmentClass::Advisory, ContainmentClass::Scoped] {
            let decision =
                ContainmentDecision::decide(EnforcementTier::KernelSealed, declared, false, false);
            assert_eq!(decision.applied_tier(), EnforcementTier::KernelFull);
            assert_eq!(decision.achieved(), ContainmentClass::Scoped);
            assert_eq!(decision.enforcement_tier(), "landlock+seccomp");
            assert_eq!(decision.host_containment(), ContainmentClass::Sealed);
            assert!(
                check_containment_floor(declared, decision.achieved(), None, false, None).is_ok()
            );
            assert_eq!(
                containment_shortfall_reason(declared, decision.achieved(), None, false, None),
                None
            );
        }
    }

    const ALL_CLASSES: &[ContainmentClass] = &[
        ContainmentClass::Advisory,
        ContainmentClass::Scoped,
        ContainmentClass::Sealed,
    ];

    /// Every host tier × declared class × `workdir_exec` × runtime directory inside the workdir:
    /// the tier the session installs, the class read off it, the host's ceiling and the wire name
    /// of the installed tier, from a table written out here rather than recomputed.
    #[test]
    fn the_decision_reads_the_class_off_the_tier_the_session_installs() {
        use ContainmentClass::{Advisory, Scoped, Sealed};
        use EnforcementTier::{EnvironmentOnly, KernelFull, KernelSealed, KernelSeccompOnly};
        // (host tier, declared) → (applied tier, uncapped class, wire name)
        let table = [
            (
                (KernelSealed, Advisory),
                (KernelFull, Scoped, "landlock+seccomp"),
            ),
            (
                (KernelSealed, Scoped),
                (KernelFull, Scoped, "landlock+seccomp"),
            ),
            (
                (KernelSealed, Sealed),
                (KernelSealed, Sealed, "mountns+pivot_root+landlock+seccomp"),
            ),
            (
                (KernelFull, Advisory),
                (KernelFull, Scoped, "landlock+seccomp"),
            ),
            (
                (KernelFull, Scoped),
                (KernelFull, Scoped, "landlock+seccomp"),
            ),
            (
                (KernelFull, Sealed),
                (KernelFull, Scoped, "landlock+seccomp"),
            ),
            (
                (KernelSeccompOnly, Advisory),
                (KernelSeccompOnly, Advisory, "seccomp-only"),
            ),
            (
                (KernelSeccompOnly, Scoped),
                (KernelSeccompOnly, Advisory, "seccomp-only"),
            ),
            (
                (KernelSeccompOnly, Sealed),
                (KernelSeccompOnly, Advisory, "seccomp-only"),
            ),
            (
                (EnvironmentOnly, Advisory),
                (EnvironmentOnly, Advisory, "none"),
            ),
            (
                (EnvironmentOnly, Scoped),
                (EnvironmentOnly, Advisory, "none"),
            ),
            (
                (EnvironmentOnly, Sealed),
                (EnvironmentOnly, Advisory, "none"),
            ),
        ];
        assert_eq!(table.len(), ALL_TIERS.len() * ALL_CLASSES.len());
        for ((host_tier, declared), (applied, class, wire)) in table {
            let host_class = match host_tier {
                KernelSealed => Sealed,
                KernelFull => Scoped,
                KernelSeccompOnly | EnvironmentOnly => Advisory,
                EnforcementTier::Unconfined => unreachable!("never a host tier"),
            };
            for workdir_exec in [false, true] {
                for inside in [false, true] {
                    let decision =
                        ContainmentDecision::decide(host_tier, declared, workdir_exec, inside);
                    let what = format!("{host_tier:?} {declared} exec={workdir_exec} in={inside}");
                    assert_eq!(decision.declared(), declared, "{what}");
                    assert_eq!(decision.applied_tier(), applied, "{what}");
                    assert_eq!(decision.enforcement_tier(), wire, "{what}");
                    assert_eq!(decision.host_containment(), host_class, "{what}");
                    let expected = if workdir_exec || inside {
                        Advisory
                    } else {
                        class
                    };
                    assert_eq!(decision.achieved(), expected, "{what}");
                    // No declaration raises a class above the host's, and only a `sealed`
                    // declaration is ever reported `sealed`.
                    assert!(decision.achieved() <= decision.host_containment(), "{what}");
                    assert!(
                        decision.achieved() != Sealed || declared == Sealed,
                        "{what}"
                    );
                }
            }
        }

        // The three rows a sealed-capable host turns on, by name.
        let scoped = ContainmentDecision::decide(KernelSealed, Scoped, false, false);
        assert_eq!(
            (
                scoped.achieved(),
                scoped.enforcement_tier(),
                scoped.host_containment()
            ),
            (Scoped, "landlock+seccomp", Sealed)
        );
        let sealed = ContainmentDecision::decide(KernelSealed, Sealed, false, false);
        assert_eq!(
            (sealed.achieved(), sealed.enforcement_tier()),
            (Sealed, "mountns+pivot_root+landlock+seccomp")
        );
        let advisory = ContainmentDecision::decide(KernelSealed, Advisory, false, false);
        assert_eq!(
            (advisory.achieved(), advisory.enforcement_tier()),
            (Scoped, "landlock+seccomp")
        );
    }

    /// The floor refusal decides from the decision's class exactly as it did from the host tier's
    /// class: the two differ only where the host is sealed-capable and the declaration is below
    /// `sealed`, and there both meet the floor (or, capped, both refuse with the same words).
    #[test]
    fn the_floor_verdict_and_wording_are_those_of_the_host_tier_class() {
        for &host_tier in ALL_TIERS {
            for &declared in ALL_CLASSES {
                for workdir_exec in [false, true] {
                    for inside in [None, Some(inside_placement())] {
                        let in_workdir = inside.is_some();
                        let decision = ContainmentDecision::decide(
                            host_tier,
                            declared,
                            workdir_exec,
                            in_workdir,
                        );
                        let host_class =
                            achieved_containment_class(host_tier, workdir_exec, in_workdir);
                        let shortfall = blocked(SealedBlocker::NamespaceCreationDenied);
                        assert_eq!(
                            containment_shortfall_reason(
                                declared,
                                decision.achieved(),
                                Some(&shortfall),
                                workdir_exec,
                                inside.as_ref(),
                            ),
                            containment_shortfall_reason(
                                declared,
                                host_class,
                                Some(&shortfall),
                                workdir_exec,
                                inside.as_ref(),
                            ),
                            "{host_tier:?} {declared} exec={workdir_exec} in={in_workdir}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn sealed_floor_is_met_only_on_the_sealed_tier() {
        assert!(check_containment_floor(
            ContainmentClass::Sealed,
            achieved_class_for_tier(EnforcementTier::KernelSealed),
            None,
            false,
            None
        )
        .is_ok());

        for tier in ALL_TIERS
            .iter()
            .filter(|t| **t != EnforcementTier::KernelSealed)
        {
            let achieved = achieved_class_for_tier(*tier);
            let error = check_containment_floor(
                ContainmentClass::Sealed,
                achieved,
                Some(&blocked(SealedBlocker::NamespaceCreationDenied)),
                false,
                None,
            )
            .expect_err("sealed must refuse on a host that cannot back it");
            match error {
                RuntimeError::ContainmentFloorUnmet {
                    declared, reason, ..
                } => {
                    assert_eq!(declared, ContainmentClass::Sealed);
                    assert!(
                        reason.contains("seccomp profile") && !reason.contains("--cap-add"),
                        "reason must name the blocking mechanism's remediation, got: {reason}"
                    );
                }
                other => panic!("expected ContainmentFloorUnmet, got {other:?}"),
            }
        }
    }

    /// The refusal text is mechanism-specific, not one fixed sentence: the same
    /// declared/achieved pair produces different, individually actionable reasons.
    #[test]
    fn the_sealed_refusal_names_the_specific_blocker() {
        let achieved = ContainmentClass::Scoped;

        let apparmor = containment_shortfall_reason(
            ContainmentClass::Sealed,
            achieved,
            Some(&blocked(SealedBlocker::AppArmorProfileMissing)),
            false,
            None,
        )
        .expect("refusal");
        assert!(apparmor.contains("mur-sealed"));
        assert!(apparmor.contains("apparmor_parser -r"));
        assert!(!apparmor.contains("--cap-add"));

        let container = containment_shortfall_reason(
            ContainmentClass::Sealed,
            achieved,
            Some(&blocked(SealedBlocker::NamespaceCreationDenied)),
            false,
            None,
        )
        .expect("refusal");
        assert!(!container.contains("--cap-add"), "{container}");
        assert!(container.contains("seccomp profile"), "{container}");
        assert!(container.contains("unshare(CLONE_NEWUSER)"), "{container}");
        assert!(container.contains("--security-opt seccomp="), "{container}");
        assert!(
            container.contains("seccompProfile: Localhost"),
            "{container}"
        );
        assert!(container.contains("outside the container"));
        assert!(!container.contains("apparmor_parser"));

        // A host that mounts and will not pivot is refused at launch with its own text, rather
        // than at the first subprocess with `E-RUN-014`.
        let pivot = containment_shortfall_reason(
            ContainmentClass::Sealed,
            ContainmentClass::Advisory,
            Some(&blocked(SealedBlocker::PivotRootDenied)),
            false,
            None,
        )
        .expect("refusal");
        assert!(pivot.contains("pivot_root(2)"), "reason was: {pivot}");
        assert_ne!(
            pivot,
            containment_shortfall_reason(
                ContainmentClass::Sealed,
                ContainmentClass::Advisory,
                Some(&blocked(SealedBlocker::MountDenied)),
                false,
                None,
            )
            .expect("refusal")
        );

        // Never the pre-mechanism blanket text again, under any blocker.
        for blocker in [
            SealedBlocker::NotLinux,
            SealedBlocker::AppArmorProfileMissing,
            SealedBlocker::NamespaceCreationDenied,
            SealedBlocker::MountDenied,
            SealedBlocker::PivotRootDenied,
            SealedBlocker::KernelUnsupported,
            SealedBlocker::LandlockUnavailable,
        ] {
            let reason = containment_shortfall_reason(
                ContainmentClass::Sealed,
                achieved,
                Some(&blocked(blocker)),
                false,
                None,
            )
            .expect("refusal");
            assert!(
                !reason.contains("no host can provide it today"),
                "got: {reason}"
            );
        }
    }

    /// A caller with no probe result still gets a reason naming the mechanism rather than an
    /// empty or misleading one.
    #[test]
    fn the_sealed_refusal_falls_back_to_naming_the_mechanism_without_a_probe() {
        let reason = containment_shortfall_reason(
            ContainmentClass::Sealed,
            ContainmentClass::Advisory,
            None,
            false,
            None,
        )
        .expect("refusal");
        assert!(reason.contains("pivot_root"));
        assert!(reason.contains("CLONE_NEWNS"));
    }

    #[test]
    fn shortfall_reason_is_absent_when_the_floor_is_met() {
        assert_eq!(
            containment_shortfall_reason(
                ContainmentClass::Advisory,
                ContainmentClass::Scoped,
                None,
                false,
                None
            ),
            None
        );
        assert_eq!(
            containment_shortfall_reason(
                ContainmentClass::Scoped,
                ContainmentClass::Scoped,
                None,
                false,
                None
            ),
            None
        );
        assert_eq!(
            containment_shortfall_reason(
                ContainmentClass::Sealed,
                ContainmentClass::Sealed,
                Some(&blocked(SealedBlocker::NamespaceCreationDenied)),
                false,
                None
            ),
            None,
            "a met floor must ignore a stale blocker rather than manufacture a refusal"
        );
    }

    fn sample_policy() -> CapabilityPolicy {
        CapabilityPolicy {
            network_allow: vec!["https://api.example.com".to_string()],
            unix_sockets_allowed: false,
            filesystem_scope: Some("workdir".to_string()),
            shell_allow: vec!["python3".to_string()],
            spawn_allow: vec!["helper".to_string()],
            env_allow: vec!["TZ".to_string()],
            shell_interpreter_runtime: vec![InterpreterRuntimeGrant {
                binary: "python3".to_string(),
                dirs: vec![InterpreterRuntimeDir {
                    path: "/usr/lib/python3.11".to_string(),
                    list_dir: true,
                }],
            }],
            ..CapabilityPolicy::default()
        }
    }

    /// The unconfined stance is decided to the unconfined tier and class on every host, meets its
    /// own floor, and every tier-keyed field of its report says what is true: no mechanism, no
    /// Landlock ABI, no filesystem restriction, nothing read-only and `/tmp` not mapped.
    #[test]
    fn an_unconfined_report_names_no_protection_it_does_not_install() {
        for host_tier in [
            EnforcementTier::KernelSealed,
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let decision =
                ContainmentDecision::decide(host_tier, ContainmentClass::Unconfined, true, true);
            assert_eq!(decision.applied_tier(), EnforcementTier::Unconfined);
            assert_eq!(decision.achieved(), ContainmentClass::Unconfined);
            assert_eq!(decision.enforcement_tier(), "unconfined");
            assert_eq!(
                decision.host_containment(),
                achieved_class_for_tier(host_tier)
            );
        }
        let policy = CapabilityPolicy {
            containment_floor: ContainmentClass::Unconfined,
            declared_resources: murmur_artifact::ResourceCapabilities {
                max_open_files: Some(256),
                ..Default::default()
            },
            ..sample_policy()
        };
        let report = report_for(
            &policy,
            ContainmentClass::Unconfined,
            EnforcementTier::KernelFull,
            None,
            None,
            Some(4),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(EnforcementTier::Unconfined),
            IoMaxReport::default(),
        );
        assert!(report.floor_met);
        assert_eq!(report.achieved_containment, ContainmentClass::Unconfined);
        assert_eq!(report.enforcement_tier, "unconfined");
        // The routes the stance runs as the operator say so; the in-process ones keep their notes.
        for row in &report.routes {
            if row.route.runs_unconfined() {
                assert!(row.under_class, "{row:?}");
                assert_eq!(row.mechanism, "unconfined", "{row:?}");
                assert_eq!(row.note, Some(UNCONFINED_ROUTE_NOTE), "{row:?}");
            } else {
                assert_eq!(row.note, row.route.note(), "{row:?}");
            }
        }
        assert_eq!(
            uncovered_route_warning(report.declared_containment, &report.routes),
            None,
            "W-SEC-034 keys on a scoped or sealed floor"
        );
        assert_eq!(report.landlock_abi, None);
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::None
        );
        assert_eq!(
            report.filesystem_boundary.not_protected,
            vec![UNCONFINED_NOT_PROTECTED.to_string()]
        );
        for dir in [&report.runtime_dir, &report.records_dir] {
            assert!(!dir.read_only_to_commands);
            assert_eq!(dir.enforced_by, None);
            assert!(dir
                .not_protected_because
                .as_deref()
                .unwrap()
                .contains("unconfined"));
        }
        assert!(!report.temp_dir.tmp_mapped);
        assert_eq!(
            report.temp_dir.not_mapped_because.as_deref(),
            Some(crate::temp_dir::NotMappedReason::Unconfined.describe())
        );
        let unconfined = report.unconfined.as_ref().expect("an unconfined report");
        assert_eq!(unconfined.uid, current_uid());
        assert_eq!(unconfined.applies_to, UNCONFINED_APPLIES_TO.to_vec());
        assert_eq!(unconfined.resource_limits, vec!["max_open_files"]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["enforcement_tier"], "unconfined");
        assert_eq!(json["achieved_containment"], "unconfined");
        assert_eq!(json["filesystem_boundary"]["restriction"], "none");
        assert!(json["unconfined"].is_object(), "{json}");
        let rendered = report.render();
        assert!(rendered.contains("  achieved:  unconfined\n"), "{rendered}");
        assert!(rendered.contains("\nUnconfined\n  user:"), "{rendered}");
        assert!(rendered.contains("  uid:       "), "{rendered}");
        assert!(rendered.contains("  HOME:      "), "{rendered}");
        assert!(
            rendered.contains("applies to: shell commands, plan shell steps, process harness"),
            "{rendered}"
        );
        assert!(unconfined_warning(unconfined).contains("sudo"));

        // A contained report carries the key, `null`.
        let contained = report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(EnforcementTier::KernelFull),
            IoMaxReport::default(),
        );
        let json = serde_json::to_value(&contained).unwrap();
        assert!(json["unconfined"].is_null(), "{json}");
        assert!(json.as_object().unwrap().contains_key("unconfined"));
    }

    #[test]
    fn scope_report_mirrors_the_policy_and_the_probed_tier() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );

        assert_eq!(report.declared_containment, ContainmentClass::Scoped);
        assert_eq!(report.achieved_containment, ContainmentClass::Scoped);
        assert!(report.floor_met);
        assert_eq!(report.shortfall_reason, None);
        assert_eq!(report.enforcement_tier, "landlock+seccomp");
        assert_eq!(report.filesystem_scope.as_deref(), Some("workdir"));
        assert_eq!(report.network_allow, vec!["https://api.example.com"]);
        assert!(!report.unix_sockets);
        assert_eq!(report.shell_allow, vec!["python3"]);
        assert_eq!(report.spawn_allow, vec!["helper"]);
        assert_eq!(report.env_allow, vec!["TZ"]);
        assert!(!report.workdir_exec);
        assert_eq!(
            report.interpreter_runtime_grants,
            vec!["python3: /usr/lib/python3.11 (list_dir)"]
        );
    }

    /// `--explain-scope` is where an operator finds out *why* a Landlock-capable host is reporting
    /// `advisory`, so the declaration and the class it produced have to appear together — in the
    /// struct, in the JSON, and in the rendered text.
    #[test]
    fn scope_report_surfaces_workdir_exec_and_the_advisory_it_forces() {
        let policy = CapabilityPolicy {
            shell_allow: vec!["gcc".to_string()],
            workdir_exec_allowed: true,
            ..CapabilityPolicy::default()
        };

        let report = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );

        assert!(report.workdir_exec);
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert!(!report.floor_met);
        // The host is fully capable; the mechanism line must keep saying so, so the report does
        // not read as a broken host.
        assert_eq!(report.enforcement_tier, "landlock+seccomp");
        assert!(report
            .shortfall_reason
            .as_deref()
            .expect("refusal")
            .contains("workdir_exec"));

        let value: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["workdir_exec"], true);

        let rendered = report.render();
        assert!(rendered.contains("workdir exec:     true"));
        assert!(rendered.contains("achieved:  advisory"));
    }

    /// The key is always written, on the same terms `workdir_exec` and `state_stores` are: an
    /// absent `io_max` identifies a runtime that predates the field, not a host that was not
    /// asked. "Not asked" is a value — `not-probed` — inside the object.
    #[test]
    fn scope_report_json_always_carries_io_max() {
        let unprobed: serde_json::Value = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(unprobed["io_max"]["status"], "not-probed");
        assert_eq!(unprobed["io_max"]["declared_bytes_per_sec"], 0);
        assert!(unprobed["io_max"].get("reason").is_none());

        let enforced: serde_json::Value = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport {
                declared_bytes_per_sec: 52_428_800,
                status: IoMaxStatus::Enforced,
                reason: None,
            },
        ))
        .unwrap();
        assert_eq!(enforced["io_max"]["status"], "enforced");
        assert_eq!(enforced["io_max"]["declared_bytes_per_sec"], 52_428_800);
    }

    /// A ceiling that did not apply has to be visible in the rendering an operator reads, not
    /// only in the JSON: this report is where the claim of an I/O bound would otherwise stand
    /// uncontradicted.
    #[test]
    fn the_rendered_resource_plane_states_what_became_of_the_io_ceiling() {
        let rendered = report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport {
                declared_bytes_per_sec: 52_428_800,
                status: IoMaxStatus::Unavailable,
                reason: Some("the io controller is not delegated".to_string()),
            },
        )
        .render();

        assert!(rendered.contains("Resource plane"), "{rendered}");
        assert!(rendered.contains("io.max:"), "{rendered}");
        assert!(rendered.contains("unavailable"), "{rendered}");
        assert!(rendered.contains("52428800 bytes/s declared"), "{rendered}");
        assert!(
            rendered.contains("the io controller is not delegated"),
            "{rendered}"
        );
    }

    /// The field is always written, including `false`, so a consumer can distinguish "this capsule
    /// declared nothing" from "this runtime does not know the key".
    #[test]
    fn scope_report_json_always_carries_workdir_exec() {
        let value: serde_json::Value = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(value["workdir_exec"], false);
    }

    /// `--explain-scope` is a diagnostic, not a launch: a capsule declaring `staged_runtime` on a
    /// host that cannot back `sealed` is exactly the case an operator inspects with it, so the
    /// grant must be reported on every tier — including the ones where `mur run` would refuse.
    #[test]
    fn staged_runtime_grants_are_reported_on_every_tier() {
        let policy = CapabilityPolicy {
            shell_allow: vec!["python3".to_string()],
            shell_staged_runtime: vec![StagedRuntimeGrant {
                binary: "python3".to_string(),
                source_path: "/opt/testbed/conda/envs/django__django".to_string(),
                pin: "conda-4.10.3/python-3.9.19".to_string(),
            }],
            ..CapabilityPolicy::default()
        };

        for tier in ALL_TIERS {
            let report = report_for(
                &policy,
                ContainmentClass::Sealed,
                *tier,
                Some(&blocked(SealedBlocker::NamespaceCreationDenied)),
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    *tier,
                    ContainmentClass::Sealed,
                )),
                IoMaxReport::default(),
            );
            assert_eq!(
                report.staged_runtime_grants,
                vec![
                    "python3: /opt/testbed/conda/envs/django__django \
                     (pin: conda-4.10.3/python-3.9.19)"
                ],
                "tier {tier:?} must still report the declared staged runtime"
            );
            assert!(
                report.render().contains("staged runtime"),
                "the rendered report must carry a staged runtime section on tier {tier:?}"
            );
        }
    }

    /// The absent case: no `staged_runtime` in the manifest leaves the section empty rather than
    /// inventing one, and `render` still names it as `<none>` like every other empty list.
    #[test]
    fn absent_staged_runtime_reports_nothing() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );
        assert!(report.staged_runtime_grants.is_empty());
        assert!(report.render().contains("staged runtime: <none>"));
    }

    /// A capsule that declares `scoped` *and* opens host paths through `interpreter_runtime`
    /// still achieves only `scoped` — the grant set is reported, never consulted to upgrade
    /// the achieved class.
    #[test]
    fn grant_set_never_raises_the_achieved_class() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelFull,
            Some(&blocked(SealedBlocker::NamespaceCreationDenied)),
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Sealed,
            )),
            IoMaxReport::default(),
        );
        assert_eq!(report.achieved_containment, ContainmentClass::Scoped);
        assert!(!report.floor_met);
    }

    #[test]
    fn scope_report_still_reports_when_the_floor_is_unmet() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::EnvironmentOnly,
            Some(&blocked(SealedBlocker::NotLinux)),
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::EnvironmentOnly,
                ContainmentClass::Sealed,
            )),
            IoMaxReport::default(),
        );

        assert_eq!(report.declared_containment, ContainmentClass::Sealed);
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert!(!report.floor_met);
        assert!(report.shortfall_reason.unwrap().contains("non-Linux"));
        assert_eq!(report.enforcement_tier, "none");
    }

    #[test]
    fn scope_report_names_the_sealed_mechanism_on_a_sealed_host() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Sealed,
            )),
            IoMaxReport::default(),
        );
        assert_eq!(report.achieved_containment, ContainmentClass::Sealed);
        assert_eq!(report.host_containment, ContainmentClass::Sealed);
        assert!(report.floor_met);
        assert_eq!(
            report.enforcement_tier,
            "mountns+pivot_root+landlock+seccomp"
        );
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Absent
        );
    }

    /// Every host tier × declared class: the class, the tier, the filesystem boundary, the ruleset
    /// ABI and the host's ceiling a report names all follow the one decision, so no report can
    /// pair `mountns+pivot_root+landlock+seccomp` with `restriction: enforced`.
    #[test]
    fn every_report_field_follows_the_decision() {
        for &host_tier in ALL_TIERS {
            for &declared in ALL_CLASSES {
                let decision = ContainmentDecision::decide(host_tier, declared, false, false);
                let report = scope_report_for_tier(
                    &sample_policy(),
                    &decision,
                    None,
                    None,
                    Some(6),
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    crate::temp_dir::tier_temp_mapping(decision.applied_tier()),
                    IoMaxReport::default(),
                );
                let what = format!("{host_tier:?} {declared}");
                assert_eq!(report.declared_containment, declared, "{what}");
                assert_eq!(report.achieved_containment, decision.achieved(), "{what}");
                assert_eq!(
                    report.enforcement_tier,
                    decision.enforcement_tier(),
                    "{what}"
                );
                assert_eq!(
                    report.host_containment,
                    decision.host_containment(),
                    "{what}"
                );
                assert_eq!(
                    report.landlock_abi,
                    crate::sandbox::ruleset_landlock_abi(Some(6), decision.applied_tier()),
                    "{what}"
                );
                let restriction = filesystem_boundary(decision.applied_tier()).restriction;
                assert_eq!(
                    report.filesystem_boundary.restriction, restriction,
                    "{what}"
                );
                assert_eq!(
                    restriction,
                    match report.enforcement_tier {
                        "mountns+pivot_root+landlock+seccomp" => FilesystemRestriction::Absent,
                        "landlock+seccomp" => FilesystemRestriction::Enforced,
                        _ => FilesystemRestriction::Advisory,
                    },
                    "{what}"
                );
                assert_eq!(
                    report.temp_dir,
                    temp_dir_report(
                        crate::temp_dir::tier_temp_mapping(decision.applied_tier()),
                        &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    ),
                    "{what}"
                );
            }
        }
    }

    /// The host's ceiling is its own field and its own line, directly under the session's class,
    /// so a reader sees both and cannot take one for the other.
    #[test]
    fn host_containment_is_reported_beside_the_achieved_class() {
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(EnforcementTier::KernelFull),
            IoMaxReport::default(),
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["achieved_containment"], "scoped");
        assert_eq!(value["host_containment"], "sealed");
        assert_eq!(value["enforcement_tier"], "landlock+seccomp");
        assert_eq!(value["filesystem_boundary"]["restriction"], "enforced");

        let rendered = report.render();
        assert!(
            rendered.contains("  achieved:  scoped\n  host can back: sealed\n  floor met: yes\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  mechanism: landlock+seccomp\n"),
            "{rendered}"
        );
    }

    #[test]
    fn scope_report_json_uses_wire_names() {
        let report = report_for(
            &CapabilityPolicy::default(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSeccompOnly,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSeccompOnly,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        );
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["declared_containment"], "advisory");
        assert_eq!(value["achieved_containment"], "advisory");
        assert_eq!(value["host_containment"], "advisory");
        assert_eq!(value["floor_met"], true);
        assert_eq!(value["enforcement_tier"], "seccomp-only");
        // Absent rather than null when the floor is met.
        assert!(value.get("shortfall_reason").is_none());
    }

    /// `landlock_abi` is the ruleset ABI of the *applied* tier: the kernel's capped at 3 on a
    /// Landlock tier, `null` below one, and the key is present either way.
    #[test]
    fn scope_report_names_the_landlock_abi_the_ruleset_is_built_at() {
        let report = |tier, kernel| {
            report_for(
                &sample_policy(),
                ContainmentClass::Scoped,
                tier,
                None,
                None,
                kernel,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    tier,
                    ContainmentClass::Scoped,
                )),
                IoMaxReport::default(),
            )
        };

        for (tier, kernel, expected) in [
            (EnforcementTier::KernelFull, Some(8), Some(3)),
            (EnforcementTier::KernelSealed, Some(8), Some(3)),
            (EnforcementTier::KernelFull, Some(2), Some(2)),
            (EnforcementTier::KernelFull, Some(1), Some(1)),
            (EnforcementTier::KernelFull, None, None),
            (EnforcementTier::KernelSeccompOnly, Some(8), None),
            (EnforcementTier::EnvironmentOnly, None, None),
        ] {
            let scope = report(tier, kernel);
            assert_eq!(
                scope.landlock_abi, expected,
                "{tier:?} with kernel {kernel:?}"
            );

            let value: serde_json::Value = serde_json::to_value(&scope).unwrap();
            assert!(value.as_object().unwrap().contains_key("landlock_abi"));
            match expected {
                Some(abi) => assert_eq!(value["landlock_abi"], abi),
                None => assert!(value["landlock_abi"].is_null()),
            }

            let rendered = scope.render();
            let line = match expected {
                Some(abi) => format!("  landlock ABI: {abi}\n"),
                None => "  landlock ABI: n/a\n".to_string(),
            };
            assert!(rendered.contains(&line), "{rendered}");
            assert!(
                rendered.find("userns grant:").unwrap() < rendered.find("landlock ABI:").unwrap()
            );
        }
    }

    /// The grant is reported, and it never moves a class, a floor or a mechanism: the same tier
    /// with four different grants produces four reports that differ in exactly one field.
    #[test]
    fn the_userns_grant_is_reported_without_changing_any_verdict() {
        let baseline = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        );

        for grant in UsernsGrant::ALL {
            let report = report_for(
                &sample_policy(),
                ContainmentClass::Advisory,
                EnforcementTier::KernelSealed,
                None,
                Some(*grant),
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    EnforcementTier::KernelSealed,
                    ContainmentClass::Advisory,
                )),
                IoMaxReport::default(),
            );
            assert_eq!(report.achieved_containment, baseline.achieved_containment);
            assert_eq!(report.floor_met, baseline.floor_met);
            assert_eq!(report.enforcement_tier, baseline.enforcement_tier);
            assert_eq!(report.userns_grant, Some(*grant));

            let value: serde_json::Value = serde_json::to_value(&report).unwrap();
            assert_eq!(value["userns_grant"], grant.wire_name());
            assert!(report
                .render()
                .contains(&format!("userns grant: {}", grant.wire_name())));
        }

        // Off Linux the key is present and null — AppArmor does not exist there, which is not the
        // same statement as "the grant was withheld".
        let value: serde_json::Value = serde_json::to_value(&baseline).unwrap();
        assert!(value["userns_grant"].is_null());
        assert!(baseline.render().contains("userns grant: n/a"));
    }

    #[test]
    fn rendered_report_names_both_classes_and_the_refusal() {
        let rendered = report_for(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelFull,
            Some(&blocked(SealedBlocker::AppArmorProfileMissing)),
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Sealed,
            )),
            IoMaxReport::default(),
        )
        .render();

        assert!(rendered.contains("declared:  sealed"));
        assert!(rendered.contains("achieved:  scoped"));
        assert!(rendered.contains("floor met: no"));
        assert!(rendered.contains("mur-sealed"));
        assert!(rendered.contains("would refuse to launch"));
    }

    /// The invariant the whole design rests on: an export is a disclosure, not a grant. Declaring
    /// one must leave the achieved class — and everything else the report says about what the
    /// guest can reach — byte-identical.
    #[test]
    fn an_export_never_changes_what_the_report_says_about_containment() {
        let export = FileExport {
            root: "out/".to_string(),
            mode: ExportMode::ReadOnly,
            max_bytes: 10 * 1024 * 1024,
        };
        for tier in ALL_TIERS {
            for workdir_exec in [false, true] {
                let policy = CapabilityPolicy {
                    workdir_exec_allowed: workdir_exec,
                    ..sample_policy()
                };
                let without = report_for(
                    &policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                        *tier,
                        ContainmentClass::Scoped,
                    )),
                    IoMaxReport::default(),
                );
                let with = report_for(
                    &policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    None,
                    Some(&Exports {
                        files: Some(export.clone()),
                        peer_files: None,
                        peer_tasks: None,
                    }),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                        *tier,
                        ContainmentClass::Scoped,
                    )),
                    IoMaxReport::default(),
                );
                assert_eq!(
                    with.achieved_containment, without.achieved_containment,
                    "tier {tier:?}, workdir_exec {workdir_exec}"
                );
                assert_eq!(with.enforcement_tier, without.enforcement_tier);
                assert_eq!(with.floor_met, without.floor_met);
                assert_eq!(with.shortfall_reason, without.shortfall_reason);
                // Everything but the one new field is identical, which is the strongest form of
                // "an export changes nothing else" this report can state.
                assert_eq!(
                    ScopeReport {
                        exports_files: None,
                        ..with.clone()
                    },
                    without
                );
                assert_eq!(
                    with.exports_files,
                    Some(ExportsFilesReport {
                        root: "out/".to_string(),
                        mode: ExportMode::ReadOnly,
                        max_bytes: 10 * 1024 * 1024,
                    })
                );
            }
        }
    }

    /// The same invariant for the peer-handoff grants, and stated separately because they are a
    /// separate authoriser: declaring either half must leave the achieved class, the tier and the
    /// floor verdict byte-identical, and must change no field of the report but its own.
    #[test]
    fn peer_handoff_never_changes_what_the_report_says_about_containment() {
        let peer_files = PeerFilesExport {
            root: "out/".to_string(),
            max_ttl_secs: Some(900),
            max_bytes: 10 * 1024 * 1024,
        };
        for tier in ALL_TIERS {
            for workdir_exec in [false, true] {
                let without_policy = CapabilityPolicy {
                    workdir_exec_allowed: workdir_exec,
                    ..sample_policy()
                };
                let with_policy = CapabilityPolicy {
                    peer_fetch_allow: vec!["localhost:41234".to_string()],
                    ..without_policy.clone()
                };
                let without = report_for(
                    &without_policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                        *tier,
                        ContainmentClass::Scoped,
                    )),
                    IoMaxReport::default(),
                );
                let with = report_for(
                    &with_policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    None,
                    Some(&Exports {
                        files: None,
                        peer_files: Some(peer_files.clone()),
                        peer_tasks: None,
                    }),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                    crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                        *tier,
                        ContainmentClass::Scoped,
                    )),
                    IoMaxReport::default(),
                );
                assert_eq!(
                    with.achieved_containment, without.achieved_containment,
                    "tier {tier:?}, workdir_exec {workdir_exec}"
                );
                assert_eq!(with.enforcement_tier, without.enforcement_tier);
                assert_eq!(with.floor_met, without.floor_met);
                assert_eq!(with.shortfall_reason, without.shortfall_reason);
                assert_eq!(
                    ScopeReport {
                        peer_files: None,
                        peer_fetch_allow: Vec::new(),
                        ..with.clone()
                    },
                    without
                );
                assert_eq!(
                    with.peer_files,
                    Some(PeerFilesReport {
                        root: "out/".to_string(),
                        max_ttl_secs: 900,
                        max_bytes: 10 * 1024 * 1024,
                    })
                );
                assert_eq!(with.peer_fetch_allow, vec!["localhost:41234".to_string()]);
                // Declaring `peer_fetch` does not widen `network.allow`, on either side of the
                // report: the two lists are carried separately and neither is derived from the
                // other.
                assert_eq!(with.network_allow, without.network_allow);
            }
        }
    }

    /// Both peer fields are written whether or not they were declared, on the same terms as
    /// `exports_files`: an absent key identifies an older runtime, a `null`/`[]` identifies a
    /// capsule that declined.
    #[test]
    fn the_peer_fields_are_always_serialized() {
        let undeclared: serde_json::Value = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(undeclared["peer_files"], serde_json::Value::Null);
        assert_eq!(undeclared["peer_fetch_allow"], serde_json::json!([]));

        let declared: serde_json::Value = serde_json::to_value(report_for(
            &CapabilityPolicy {
                peer_fetch_allow: vec!["localhost:41234".to_string()],
                ..sample_policy()
            },
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            Some(&Exports {
                files: None,
                peer_files: Some(PeerFilesExport {
                    root: "out/".to_string(),
                    max_ttl_secs: None,
                    max_bytes: 10 * 1024 * 1024,
                }),
                peer_tasks: None,
            }),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(
            declared["peer_files"],
            serde_json::json!({"root": "out/", "max_ttl_secs": 3600, "max_bytes": 10_485_760})
        );
        assert_eq!(
            declared["peer_fetch_allow"],
            serde_json::json!(["localhost:41234"])
        );
    }

    #[test]
    fn render_names_peer_handoff_in_both_directions() {
        let undeclared = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        )
        .render();
        assert!(undeclared.contains("Peer handoff"));
        assert!(undeclared.contains("exports.peer_files: <none>"));
        assert!(undeclared.contains("peer_fetch allow: <none>"));

        let declared = report_for(
            &CapabilityPolicy {
                peer_fetch_allow: vec!["localhost:41234".to_string()],
                ..sample_policy()
            },
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
            Some(&Exports {
                files: None,
                peer_files: Some(PeerFilesExport {
                    root: "out/".to_string(),
                    max_ttl_secs: Some(900),
                    max_bytes: 4096,
                }),
                peer_tasks: None,
            }),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        )
        .render();
        assert!(declared.contains("exports.peer_files root: out/"));
        assert!(declared.contains("max handle ttl:          900s"));
        assert!(declared.contains("max bytes:               4096 (per file)"));
        assert!(declared.contains("- localhost:41234"));
    }

    fn report_with_peer_tasks(tier: EnforcementTier, peer_tasks: Option<bool>) -> ScopeReport {
        report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            tier,
            None,
            None,
            None,
            peer_tasks
                .map(|accept| Exports {
                    files: None,
                    peer_files: None,
                    peer_tasks: Some(murmur_artifact::PeerTasksExport { accept }),
                })
                .as_ref(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                tier,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        )
    }

    /// Consenting to peer tasks is an export like the others: it moves no containment field and
    /// no other field of the report but its own. Absent and `accept: false` are one report.
    #[test]
    fn peer_tasks_is_reported_and_moves_no_other_field() {
        for tier in ALL_TIERS {
            let absent = report_with_peer_tasks(*tier, None);
            let refused = report_with_peer_tasks(*tier, Some(false));
            let accepted = report_with_peer_tasks(*tier, Some(true));
            assert!(!absent.peer_tasks);
            assert_eq!(refused, absent, "tier {tier:?}");
            assert!(accepted.peer_tasks);
            assert_eq!(accepted.achieved_containment, absent.achieved_containment);
            assert_eq!(accepted.enforcement_tier, absent.enforcement_tier);
            assert_eq!(accepted.floor_met, absent.floor_met);
            assert_eq!(
                ScopeReport {
                    peer_tasks: false,
                    ..accepted.clone()
                },
                absent
            );
        }
    }

    /// `peer_tasks` is always written, as a boolean, so an absent key identifies an older runtime.
    #[test]
    fn peer_tasks_is_always_serialized() {
        let tier = EnforcementTier::KernelFull;
        let absent = serde_json::to_value(report_with_peer_tasks(tier, None)).unwrap();
        assert_eq!(absent["peer_tasks"], serde_json::json!(false));
        let accepted = serde_json::to_value(report_with_peer_tasks(tier, Some(true))).unwrap();
        assert_eq!(accepted["peer_tasks"], serde_json::json!(true));
    }

    #[test]
    fn render_names_the_peer_task_posture_under_peer_handoff() {
        let tier = EnforcementTier::KernelFull;
        for (declared, line) in [
            (None, "  exports.peer_tasks: refuse\n"),
            (Some(false), "  exports.peer_tasks: refuse\n"),
            (Some(true), "  exports.peer_tasks: accept\n"),
        ] {
            let rendered = report_with_peer_tasks(tier, declared).render();
            let section = rendered
                .split("\nPeer handoff\n")
                .nth(1)
                .and_then(|rest| rest.split("\n\n").next())
                .unwrap_or_else(|| panic!("no Peer handoff section:\n{rendered}"));
            assert!(
                section.contains(line.trim_start_matches(' ').trim_end()),
                "{declared:?}:\n{section}"
            );
            assert!(rendered.contains(line), "{declared:?}:\n{rendered}");
        }
    }

    /// A declared state store is reported, and moves nothing else. Same discipline the export
    /// reports are held to: it is copied through, never consulted, so a durable directory grant
    /// cannot talk a host into reporting a class it does not back.
    #[test]
    fn a_state_store_is_reported_and_moves_no_other_field() {
        let policy = sample_policy();
        let stores = vec![StateStoreReport {
            artifact: "murmur/tool-corpus".to_string(),
            store: "shey".to_string(),
            host_path: "/home/dev/.murmur/state/shey".to_string(),
        }];

        let without = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );
        let with = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            stores.clone(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );

        assert_eq!(with.state_stores, stores);
        assert!(without.state_stores.is_empty());
        assert_eq!(with.achieved_containment, without.achieved_containment);
        assert_eq!(with.floor_met, without.floor_met);
        assert_eq!(with.enforcement_tier, without.enforcement_tier);
        assert_eq!(with.shortfall_reason, without.shortfall_reason);
        // The only field that moved is its own.
        assert_eq!(
            ScopeReport {
                state_stores: Vec::new(),
                ..with.clone()
            },
            without
        );

        // Rendered under the grants, one line per declaring artifact; `<none>` when empty.
        assert!(with
            .render()
            .contains("- murmur/tool-corpus: shey -> /home/dev/.murmur/state/shey"));
        assert!(without.render().contains("state stores: <none>"));

        // Always an array in JSON, never skipped: an absent key means an older runtime, not a
        // capsule that declined a store.
        let json = serde_json::to_value(&without).unwrap();
        assert_eq!(json["state_stores"], serde_json::json!([]));
        assert_eq!(
            serde_json::to_value(&with).unwrap()["state_stores"],
            serde_json::json!([{
                "artifact": "murmur/tool-corpus",
                "store": "shey",
                "host_path": "/home/dev/.murmur/state/shey",
            }])
        );
    }

    fn extra_roots_preopen(artifact: &str, role: &str, scope: Option<&str>) -> PreopenReport {
        PreopenReport {
            artifact: artifact.to_string(),
            role: role.to_string(),
            scope: scope.map(str::to_string),
            surface: if scope.is_some() {
                PreopenSurface::ScopedSubtree
            } else {
                PreopenSurface::WholeWorkdir
            },
            runtime_dir_reach: (role == "tool" && scope.is_none())
                .then_some(RuntimeDirReach::MurmurRuntime),
            extra_roots: Vec::new(),
        }
    }

    fn two_host_folders() -> Vec<crate::guest_roots::GuestRoot> {
        let host_path = |path: &str, access| crate::host_paths::HostPath {
            declared: path.to_string(),
            path: std::path::PathBuf::from(path),
            access,
            credential_locations: Vec::new(),
        };
        crate::guest_roots::extra_guest_roots_for(
            ContainmentClass::Advisory,
            &[
                host_path("/data/ref", crate::host_paths::HostPathAccess::Read),
                host_path("/data/out", crate::host_paths::HostPathAccess::ReadWrite),
            ],
        )
    }

    /// Every extra root serializes with its four keys, and an entry holding none serializes `[]`.
    #[test]
    fn extra_roots_serialize_with_four_keys_always_present() {
        let mut reports = vec![
            extra_roots_preopen("probe", "tool", None),
            extra_roots_preopen("cache-tool", "tool", Some("cache")),
            extra_roots_preopen("driver", "driver", None),
        ];
        attach_extra_roots(&mut reports, &two_host_folders());
        let json = serde_json::to_value(&reports).unwrap();
        assert_eq!(
            json[0]["extra_roots"],
            serde_json::json!([
                {
                    "guest_path": "murmur-host/data/ref",
                    "path": "/data/ref",
                    "access": "read",
                    "source": "host_paths",
                },
                {
                    "guest_path": "murmur-host/data/out",
                    "path": "/data/out",
                    "access": "read-write",
                    "source": "host_paths",
                },
            ])
        );
        assert_eq!(json[1]["extra_roots"], serde_json::json!([]));
        assert_eq!(json[2]["extra_roots"], serde_json::json!([]));

        let mut reports = vec![extra_roots_preopen("probe", "tool", None)];
        attach_extra_roots(
            &mut reports,
            &crate::guest_roots::extra_guest_roots_for(ContainmentClass::Unconfined, &[]),
        );
        assert_eq!(
            serde_json::to_value(&reports[0].extra_roots).unwrap(),
            serde_json::json!([{
                "guest_path": "murmur-host",
                "path": "/",
                "access": "read-write",
                "source": "unconfined",
            }])
        );
    }

    /// A tool's extra roots render after its runtime directory: each host folder with its access,
    /// or the whole host when the session is unconfined.
    #[test]
    fn extra_roots_render_after_the_runtime_directory() {
        let mut reports = vec![extra_roots_preopen("wasm-tool", "tool", None)];
        attach_extra_roots(&mut reports, &two_host_folders());
        assert_eq!(reports[0].extra_roots.len(), 2);
        assert!(
            reports[0].render().ends_with(
                "; runtime directory read-only at murmur-runtime/; host folders under \
                 murmur-host/: /data/ref (read), /data/out (read-write)"
            ),
            "{}",
            reports[0].render()
        );

        let mut whole = vec![extra_roots_preopen("wasm-tool", "tool", None)];
        attach_extra_roots(
            &mut whole,
            &crate::guest_roots::extra_guest_roots_for(ContainmentClass::Unconfined, &[]),
        );
        assert!(whole[0].render().ends_with(
            "; runtime directory read-only at murmur-runtime/; whole host read-write \
             under murmur-host/"
        ));
    }

    /// Without roots the line is exactly the one a session without extra roots renders.
    #[test]
    fn extra_roots_absent_leave_the_line_unchanged() {
        let mut reports = vec![extra_roots_preopen("wasm-tool", "tool", None)];
        attach_extra_roots(&mut reports, &[]);
        assert_eq!(
            reports[0].render(),
            "wasm-tool (tool): the whole accessible workdir — no capabilities.filesystem.scope \
             declared; runtime directory read-only at murmur-runtime/"
        );
    }

    /// The preopen set is reported and moves nothing else, on the same terms the state-store
    /// report beside it is held to. It is also the field that answers what `filesystem_scope`
    /// does not: a capsule-wide scope is joined into no guest preopen root, so both can be
    /// present and disagree without either being wrong.
    #[test]
    fn preopens_are_reported_and_move_no_other_field() {
        let policy = sample_policy();
        let preopens = vec![
            PreopenReport {
                artifact: "notes-tool".to_string(),
                role: "tool".to_string(),
                scope: None,
                surface: PreopenSurface::WholeWorkdir,
                runtime_dir_reach: Some(RuntimeDirReach::MurmurRuntime),
                extra_roots: Vec::new(),
            },
            PreopenReport {
                artifact: "cache-tool".to_string(),
                role: "tool".to_string(),
                scope: Some("cache".to_string()),
                surface: PreopenSurface::ScopedSubtree,
                runtime_dir_reach: None,
                extra_roots: Vec::new(),
            },
            PreopenReport {
                artifact: "telemetry-hook".to_string(),
                role: "hook".to_string(),
                scope: None,
                surface: PreopenSurface::Nothing,
                runtime_dir_reach: None,
                extra_roots: Vec::new(),
            },
        ];

        let without = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );
        let with = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            preopens.clone(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );

        assert_eq!(with.preopens, preopens);
        assert!(without.preopens.is_empty());
        // The only fields that moved are its own and the route table, which reads each entry's
        // role off the preopens.
        assert_eq!(
            ScopeReport {
                preopens: Vec::new(),
                routes: without.routes.clone(),
                ..with.clone()
            },
            without
        );

        // Three visibly different lines under `Effective grants`; `<none>` when empty.
        let rendered = with.render();
        assert!(rendered.contains(
            "- notes-tool (tool): the whole accessible workdir — no capabilities.filesystem.scope \
             declared; runtime directory read-only at murmur-runtime/"
        ));
        assert!(rendered.contains(
            "- cache-tool (tool): one subtree of the accessible workdir — \
             capabilities.filesystem.scope: cache"
        ));
        assert!(rendered.contains(
            "- telemetry-hook (hook): nothing preopened — no capabilities.filesystem.scope declared"
        ));
        assert!(without.render().contains("preopens: <none>"));

        // Always an array in JSON, never skipped, with an undeclared scope as `null`.
        assert_eq!(
            serde_json::to_value(&without).unwrap()["preopens"],
            serde_json::json!([])
        );
        assert_eq!(
            serde_json::to_value(&with).unwrap()["preopens"],
            serde_json::json!([
                {
                    "artifact": "notes-tool",
                    "role": "tool",
                    "scope": null,
                    "surface": "whole-workdir",
                    "runtime_dir_reach": "murmur-runtime",
                    "extra_roots": [],
                },
                {
                    "artifact": "cache-tool",
                    "role": "tool",
                    "scope": "cache",
                    "surface": "scoped-subtree",
                    "runtime_dir_reach": null,
                    "extra_roots": [],
                },
                {
                    "artifact": "telemetry-hook",
                    "role": "hook",
                    "scope": null,
                    "surface": "nothing",
                    "runtime_dir_reach": null,
                    "extra_roots": [],
                },
            ])
        );
    }

    /// `exports_files` is written whether or not it was declared, on the same terms as
    /// `workdir_exec`: an absent key identifies an older runtime, a `null` a capsule that
    /// exported nothing.
    #[test]
    fn exports_files_is_always_serialized() {
        let undeclared = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(undeclared["exports_files"], serde_json::Value::Null);

        let declared = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            Some(&Exports {
                files: Some(FileExport {
                    root: "out/".to_string(),
                    mode: ExportMode::ReadOnly,
                    max_bytes: 10_485_760,
                }),
                peer_files: None,
                peer_tasks: None,
            }),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        assert_eq!(
            declared["exports_files"],
            serde_json::json!({"root": "out/", "mode": "read-only", "max_bytes": 10_485_760})
        );
    }

    #[test]
    fn render_names_the_resource_plane_in_both_directions() {
        let undeclared = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        )
        .render();
        assert!(
            undeclared.contains("Resource plane") && undeclared.contains("exports.files: <none>"),
            "{undeclared}"
        );

        let declared = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            Some(&Exports {
                files: Some(FileExport {
                    root: "out/".to_string(),
                    mode: ExportMode::ReadOnly,
                    max_bytes: 10_485_760,
                }),
                peer_files: None,
                peer_tasks: None,
            }),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        )
        .render();
        assert!(declared.contains("exports.files root: out/"), "{declared}");
        assert!(
            declared.contains("mode:               read-only"),
            "{declared}"
        );
        assert!(
            declared.contains("max bytes:          10485760 (per file)"),
            "{declared}"
        );
    }

    // ── read_only reporting ───────────────────────────────────────────────────

    fn read_only_report(read_only: &[&str], shell_allow: &[&str]) -> ScopeReport {
        let policy = CapabilityPolicy {
            read_only_paths: read_only.iter().map(|p| (*p).to_string()).collect(),
            shell_allow: shell_allow.iter().map(|b| (*b).to_string()).collect(),
            ..CapabilityPolicy::default()
        };
        report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        )
    }

    /// With no allowlisted interpreter the declaration holds everywhere the check can reach, and
    /// the report says exactly that and nothing stronger.
    #[test]
    fn declared_read_only_with_no_interpreter_reports_as_enforced() {
        let report = read_only_report(&["tests", "bench/fixtures"], &["git"]);
        assert_eq!(
            report.read_only_paths,
            vec!["tests".to_string(), "bench/fixtures".to_string()]
        );
        assert!(report.read_only_advisory_for.is_empty());

        let rendered = report.render();
        assert!(rendered.contains("  read_only:\n    - tests\n    - bench/fixtures\n"));
        assert!(rendered.contains(
            "read_only enforcement: enforced for every tool call and every shell command the \
             dispatch check can read"
        ));
        assert!(!rendered.contains("advisory against"));
    }

    /// An allowlisted interpreter is named, and the enforcement line is qualified — the report
    /// must never claim more than `W-SEC-017` already withdraws.
    #[test]
    fn an_allowlisted_interpreter_is_named_as_advisory() {
        let report = read_only_report(&["tests"], &["git", "python3"]);
        assert_eq!(report.read_only_advisory_for, vec!["python3".to_string()]);

        let rendered = report.render();
        assert!(rendered.contains("read_only enforcement: advisory against python3"));
        assert!(rendered
            .contains("enforced for every tool call and for every shell command it can read"));
    }

    /// The report's advisory set is the set `W-SEC-017` fires for, because both call one
    /// resolver. Asserted through the resolver rather than by re-listing interpreters, so adding
    /// one to `protected_paths::ADVISORY_INTERPRETERS` cannot make the two disagree.
    #[test]
    fn the_reported_advisory_set_is_the_set_the_warning_fires_for() {
        let shell_allow: Vec<String> = ["git", "python3", "node", "make", "bash"]
            .iter()
            .map(|b| (*b).to_string())
            .collect();
        let declared = vec!["tests".to_string()];
        assert_eq!(
            read_only_report(&["tests"], &["git", "python3", "node", "make", "bash"])
                .read_only_advisory_for,
            read_only_advisory_for(&declared, &shell_allow)
        );
        // Nothing is advisory against a protection that was never declared, whatever is allowed.
        assert!(read_only_advisory_for(&[], &shell_allow).is_empty());
    }

    /// Both keys are always arrays, including for a capsule with no `capabilities.filesystem`
    /// block at all: an absent key must identify a runtime that predates the field, never a
    /// capsule that declared nothing.
    #[test]
    fn both_read_only_keys_are_always_serialized_arrays() {
        let json = serde_json::to_value(read_only_report(&[], &["python3"])).unwrap();
        assert_eq!(json["read_only_paths"], serde_json::json!([]));
        assert_eq!(json["read_only_advisory_for"], serde_json::json!([]));

        let json = serde_json::to_value(read_only_report(&["tests"], &["python3"])).unwrap();
        assert_eq!(json["read_only_paths"], serde_json::json!(["tests"]));
        assert_eq!(
            json["read_only_advisory_for"],
            serde_json::json!(["python3"])
        );
    }

    /// An undeclared `read_only` renders one `<none>` line and asks no enforcement question,
    /// because there is no protection to qualify.
    #[test]
    fn an_undeclared_read_only_renders_none_and_no_enforcement_line() {
        let rendered = read_only_report(&[], &["python3"]).render();
        assert!(rendered.contains("  read_only: <none>\n"));
        assert!(!rendered.contains("read_only enforcement:"));
    }

    // ── Grant coverage ────────────────────────────────────────────────────────

    /// Every `pub` field of [`CapabilityPolicy`] is either reported by [`ScopeReport`] or
    /// exempted here with a written reason.
    ///
    /// The field list is derived from `types.rs`'s own source text rather than written out, so a
    /// grant added later fails this test until someone decides which it is.
    ///
    /// The mapping is checked in both directions — every policy field is accounted for, *and*
    /// every report key the mapping names is really present in a serialized report — so the
    /// table cannot claim coverage it does not have.
    #[test]
    fn every_capability_policy_field_is_reported_or_exempted() {
        /// Policy fields deliberately absent from the scope report, each with the reason.
        const EXEMPT: &[(&str, &str)] = &[
            (
                "shell_strip_env",
                "Names host variables removed from the shell child's environment on top of the \
                 credential patterns the runtime always strips. It subtracts from what the \
                 capsule sees and can open nothing, so reporting it beside the grants would pad \
                 the list an operator audits for reach.",
            ),
            (
                "shell_baseline_env",
                "The fixed variables the shell child starts with (PATH, HOME, …). It shapes the \
                 environment of a subprocess the capsule was already granted, and names no host \
                 resource `shell allow` and `env allow` do not already account for.",
            ),
            (
                "limits",
                "Execution ceilings for a WASM guest — memory, table elements, instance count, \
                 deadline. A bound on how much the capsule may consume, never a reach it gains.",
            ),
            (
                "resources",
                "The same, for subprocesses: rlimits, the Linux cgroup v2 scope and the \
                 workdir size ceiling. A ceiling, not a grant, on exactly `limits`' terms.",
            ),
            (
                "declared_deadline_seconds",
                "The undefaulted form of `limits.deadline_seconds`, retained only so hook calls \
                 can apply a lower default. Reporting it would report one ceiling twice.",
            ),
            (
                "state_declared",
                "Records that a capsule-wide `capabilities.state` block was declared where \
                 nothing reads it, which `W-SEC-014` says in words. The stores that actually \
                 exist are reported per artifact in `state_stores`.",
            ),
            (
                "conversation_declared",
                "Records a capsule-wide `capabilities.conversation` block that reaches no \
                 artifact, on `state_declared`'s terms; `W-SEC-016` is what an operator needs, \
                 and the grant itself is per artifact.",
            ),
        ];

        /// Policy field → the key it is reported under in a serialized [`ScopeReport`].
        const REPORTED: &[(&str, &str)] = &[
            ("network_allow", "network_allow"),
            ("peer_fetch_allow", "peer_fetch_allow"),
            ("unix_sockets_allowed", "unix_sockets"),
            ("filesystem_scope", "filesystem_scope"),
            ("workdir_exec_allowed", "workdir_exec"),
            ("read_only_paths", "read_only_paths"),
            ("shell_allow", "shell_allow"),
            ("spawn_allow", "spawn_allow"),
            ("spawn_workdir_snapshot", "spawn_workdir_snapshot"),
            ("shell_interpreter_runtime", "interpreter_runtime_grants"),
            (
                "shell_derived_interpreter_runtime",
                "derived_interpreter_runtime",
            ),
            ("shell_trust_store", "trust_store"),
            ("shell_declared_needs", "declared_needs"),
            ("shell_pinned_programs", "pinned_programs"),
            ("host_paths", "host_paths"),
            ("shell_staged_runtime", "staged_runtime_grants"),
            ("env_allow", "env_allow"),
            ("plan_submit", "plan_submit"),
            ("install_skill", "install_skill"),
            ("install_tool", "install_tool"),
            ("install_from_sources", "install_from"),
            ("containment_floor", "declared_containment"),
            ("declared_resources", "unconfined"),
        ];

        let declared = capability_policy_field_names(include_str!("types.rs"));
        assert!(
            declared.len() > 10,
            "failed to read CapabilityPolicy's fields out of types.rs (found {declared:?})"
        );

        for field in &declared {
            let reported = REPORTED.iter().any(|(policy, _)| policy == field);
            let exempt = EXEMPT.iter().any(|(policy, _)| policy == field);
            assert!(
                reported || exempt,
                "CapabilityPolicy::{field} is a grant no --explain-scope report mentions. Either \
                 add it to ScopeReport and to this test's REPORTED table, or add it to EXEMPT \
                 with a written reason for why an operator does not need to see it."
            );
            assert!(
                !(reported && exempt),
                "CapabilityPolicy::{field} is listed as both reported and exempt"
            );
        }

        for (policy_field, _) in REPORTED.iter().chain(EXEMPT.iter()) {
            assert!(
                declared.iter().any(|field| field == policy_field),
                "the table names CapabilityPolicy::{policy_field}, which types.rs no longer \
                 declares — remove the stale row"
            );
        }

        // The other direction: a report key the table claims must really be in the JSON, so no
        // row can cover a field with a key that does not exist.
        let json = serde_json::to_value(report_for(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        ))
        .unwrap();
        let object = json.as_object().expect("a scope report is a JSON object");
        for (policy_field, report_key) in REPORTED {
            assert!(
                object.contains_key(*report_key),
                "the table maps CapabilityPolicy::{policy_field} to a report key \
                 '{report_key}' that a serialized ScopeReport does not carry"
            );
        }
    }

    /// A report off [`sample_policy`] at a chosen host tier and declared floor, so the mechanism
    /// this session would install is the only thing that varies between the cases below.
    fn boundary_report(declared: ContainmentClass, tier: EnforcementTier) -> ScopeReport {
        report_for(
            &sample_policy(),
            declared,
            tier,
            None,
            None,
            // A kernel that mediates `truncate(2)`, so a Landlock tier's runtime directory is
            // read-only and adds no statement of its own.
            Some(6),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(tier, declared)),
            IoMaxReport::default(),
        )
    }

    fn sample_snapshot() -> crate::workdir_snapshot::WorkdirSnapshot {
        crate::workdir_snapshot::WorkdirSnapshot {
            source: "/home/you/project".to_string(),
            files: 12,
            dirs: 3,
            symlinks: 1,
            repointed_symlinks: 0,
            bytes: 40960,
            skipped_count: 1,
            skipped: vec![crate::workdir_snapshot::SkippedEntry {
                path: "run/pipe".to_string(),
                reason: crate::workdir_snapshot::SkipReason::Fifo,
            }],
        }
    }

    /// Both keys are always serialized: `[]` and `null` for a session that declares no snapshot
    /// grant and was started in no copy, and the human report lists the grant beside `spawn allow`.
    #[test]
    fn spawn_workdir_snapshot_and_parent_workdir_are_always_reported() {
        let report = boundary_report(ContainmentClass::Scoped, EnforcementTier::KernelFull);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["spawn_workdir_snapshot"], serde_json::json!([]));
        assert!(json.as_object().unwrap().contains_key("parent_workdir"));
        assert_eq!(json["parent_workdir"], serde_json::Value::Null);
        let rendered = report.render();
        let snapshot_line = rendered
            .find("\n  spawn workdir snapshot: <none>\n")
            .unwrap_or_else(|| panic!("{rendered}"));
        assert!(
            rendered.find("\n  spawn allow:").unwrap() < snapshot_line,
            "{rendered}"
        );

        assert!(!rendered.contains("Parent work folder"), "{rendered}");

        let mut policy = sample_policy();
        policy.spawn_allow = vec!["local/reviewer".to_string(), "local/worker".to_string()];
        policy.spawn_workdir_snapshot = vec!["local/reviewer".to_string()];
        let report = report_for(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            Some(6),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
            )),
            IoMaxReport::default(),
        );
        assert_eq!(report.spawn_workdir_snapshot, vec!["local/reviewer"]);
        assert_eq!(
            serde_json::to_value(&report).unwrap()["spawn_workdir_snapshot"],
            serde_json::json!(["local/reviewer"])
        );
        assert!(
            report
                .render()
                .contains("  spawn workdir snapshot:\n    - local/reviewer\n"),
            "{}",
            report.render()
        );
    }

    /// A child's `parent_workdir.restriction` is its own filesystem restriction on the tier it
    /// applies — never the achieved class — and the human report says it in words.
    #[test]
    fn parent_workdir_restriction_follows_the_applied_tier() {
        for (declared, tier, restriction) in [
            (
                ContainmentClass::Sealed,
                EnforcementTier::KernelSealed,
                "absent",
            ),
            (
                ContainmentClass::Scoped,
                EnforcementTier::KernelFull,
                "enforced",
            ),
            (
                ContainmentClass::Advisory,
                EnforcementTier::KernelSeccompOnly,
                "advisory",
            ),
            (
                ContainmentClass::Advisory,
                EnforcementTier::EnvironmentOnly,
                "advisory",
            ),
        ] {
            let report =
                boundary_report(declared, tier).with_parent_workdir(Some(sample_snapshot()));
            let json = serde_json::to_value(&report).unwrap();
            let parent = &json["parent_workdir"];
            assert_eq!(parent["restriction"], restriction, "{tier:?}");
            assert_eq!(
                parent["restriction"],
                json["filesystem_boundary"]["restriction"]
            );
            assert_eq!(parent["source"], "/home/you/project");
            assert_eq!(parent["files"], 12);
            assert_eq!(parent["skipped"][0]["reason"], "fifo");

            let rendered = report.render();
            assert!(
                rendered.contains(
                    "\nParent work folder\n  source:      /home/you/project\n  copied:      \
                     12 files, 3 directories, 1 symlinks (0 re-pointed), 40960 bytes\n  \
                     skipped:     1\n    - run/pipe (fifo)\n  restriction: "
                ),
                "{rendered}"
            );
            assert!(
                rendered.contains(&format!("  restriction: {restriction}: ")),
                "{rendered}"
            );
        }
        assert_eq!(
            boundary_report(ContainmentClass::Scoped, EnforcementTier::KernelFull)
                .with_parent_workdir(None)
                .parent_workdir,
            None
        );
    }

    /// Every presence input the table reads, one at a time, with its expected present routes.
    fn presence_cases() -> Vec<(&'static str, RouteInputs, Vec<Route>)> {
        vec![
            ("nothing", RouteInputs::default(), vec![]),
            (
                "one tool",
                RouteInputs {
                    tools: vec!["t".to_string()],
                    ..RouteInputs::default()
                },
                vec![Route::WasmTools],
            ),
            (
                "a driver",
                RouteInputs {
                    drivers: vec!["d".to_string()],
                    ..RouteInputs::default()
                },
                vec![Route::Drivers],
            ),
            (
                "a hook",
                RouteInputs {
                    hooks: vec!["h".to_string()],
                    ..RouteInputs::default()
                },
                vec![Route::Hooks],
            ),
            (
                "install.tool",
                RouteInputs {
                    install_tool: true,
                    ..RouteInputs::default()
                },
                vec![Route::WasmTools],
            ),
            (
                "transport: process",
                RouteInputs {
                    process_transport: true,
                    ..RouteInputs::default()
                },
                vec![Route::ProcessHarness, Route::HarnessTools],
            ),
            (
                "plan.submit without shell.allow",
                RouteInputs {
                    plan_submit: true,
                    ..RouteInputs::default()
                },
                vec![],
            ),
            (
                "plan.submit with shell.allow",
                RouteInputs {
                    plan_submit: true,
                    shell_allow: true,
                    ..RouteInputs::default()
                },
                vec![Route::Shell, Route::PlanShellSteps],
            ),
            (
                "shell.allow alone",
                RouteInputs {
                    shell_allow: true,
                    ..RouteInputs::default()
                },
                vec![Route::Shell],
            ),
        ]
    }

    /// The table over every tier, every declared class and every presence input: one row per
    /// [`Route::ALL`] entry in the fixed order; the first two covered by the applied tier's
    /// mechanism and the rest `host-process` with their note, whatever the tier; presence exactly
    /// as each input implies.
    #[test]
    fn the_route_table_holds_its_invariants_on_every_input() {
        for tier in ALL_TIERS {
            for declared in [
                ContainmentClass::Advisory,
                ContainmentClass::Scoped,
                ContainmentClass::Sealed,
            ] {
                let mechanism =
                    ContainmentDecision::decide(*tier, declared, false, false).enforcement_tier();
                for (case, inputs, present) in presence_cases() {
                    let rows = route_table(&inputs, mechanism);
                    assert_eq!(
                        rows.iter().map(|row| row.route).collect::<Vec<_>>(),
                        Route::ALL.to_vec(),
                        "{case}"
                    );
                    for row in &rows {
                        let covered = matches!(row.route, Route::Shell | Route::PlanShellSteps);
                        assert_eq!(row.under_class, covered, "{case}: {row:?}");
                        if covered {
                            assert_eq!(row.mechanism, mechanism, "{case}: {row:?}");
                            assert_eq!(row.note, None, "{case}: {row:?}");
                        } else {
                            assert_eq!(row.mechanism, HOST_PROCESS_MECHANISM, "{case}: {row:?}");
                            assert!(row.note.is_some(), "{case}: {row:?}");
                        }
                        assert_eq!(
                            row.present,
                            present.contains(&row.route),
                            "{case} on {tier:?}/{declared}: {row:?}"
                        );
                    }
                }
            }
        }
    }

    /// The notes say what each uncovered route does run under.
    #[test]
    fn each_uncovered_route_names_what_holds_it() {
        let notes = |route: Route| route.note().unwrap();
        for route in [Route::WasmTools, Route::Drivers] {
            assert!(notes(route).contains("inside the mur process"), "{route:?}");
            assert!(notes(route).contains("WASI preopens"), "{route:?}");
            assert!(
                notes(route).contains("host-side network allowlist only"),
                "{route:?}"
            );
        }
        assert!(notes(Route::Hooks).contains("its own capabilities block declares"));
        let harness = notes(Route::ProcessHarness);
        for word in [
            "cleared environment",
            "Landlock",
            "seccomp",
            "network namespace",
            "rlimits",
            "cgroup with no limits",
        ] {
            assert!(harness.contains(word), "{word}: {harness}");
        }
        assert!(notes(Route::HarnessTools).contains("the user's authority"));
        assert!(notes(Route::HarnessTools).contains("arguments the process driver passes"));
    }

    /// The artifacts on each route come from the preopens by role.
    #[test]
    fn route_inputs_read_roles_off_the_preopens() {
        let preopen = |artifact: &str, role: &str| PreopenReport {
            artifact: artifact.to_string(),
            role: role.to_string(),
            scope: None,
            surface: PreopenSurface::WholeWorkdir,
            runtime_dir_reach: None,
            extra_roots: Vec::new(),
        };
        let preopens = vec![
            preopen("fs-tool", "tool"),
            preopen("browser", "tool"),
            preopen("driver-anthropic", "driver"),
            preopen("audit", "hook"),
        ];
        let inputs = RouteInputs::new(&[], false, &[], &preopens, Some("http"));
        let rows = route_table(&inputs, "landlock+seccomp");
        let row = |route: Route| rows.iter().find(|row| row.route == route).unwrap();
        assert_eq!(row(Route::WasmTools).artifacts, vec!["fs-tool", "browser"]);
        assert_eq!(row(Route::Drivers).artifacts, vec!["driver-anthropic"]);
        assert_eq!(row(Route::Hooks).artifacts, vec!["audit"]);
        assert!(!row(Route::ProcessHarness).present);
        assert!(RouteInputs::new(&[], false, &[], &[], Some("process")).process_transport);
    }

    /// The `routes` key as `--explain-scope --json` and `session_start` carry it: seven rows,
    /// always serialized, every key on every row.
    #[test]
    fn the_routes_json_is_pinned() {
        let report = boundary_report(ContainmentClass::Advisory, EnforcementTier::KernelFull)
            .with_routes(Some("process"));
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("mechanism_covers").is_none(), "{json}");
        let routes = json["routes"].as_array().expect("routes is an array");
        assert_eq!(routes.len(), 7);
        let shell_present = !report.shell_allow.is_empty();
        assert_eq!(
            routes[0],
            serde_json::json!({
                "route": "shell",
                "present": shell_present,
                "under_class": true,
                "mechanism": "landlock+seccomp",
                "note": null,
                "artifacts": [],
            })
        );
        assert_eq!(
            routes[5],
            serde_json::json!({
                "route": "process-harness",
                "present": true,
                "under_class": false,
                "mechanism": "host-process",
                "note": Route::ProcessHarness.note(),
                "artifacts": [],
            })
        );
        assert_eq!(
            routes
                .iter()
                .map(|row| row["route"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "shell",
                "plan-shell-steps",
                "wasm-tools",
                "drivers",
                "hooks",
                "process-harness",
                "harness-tools"
            ]
        );
        assert_eq!(
            json["cgroup_scope"],
            serde_json::json!({"status": "not-probed"})
        );
    }

    /// The text report replaces the `covers:` line with a `Routes` block under `Containment`.
    #[test]
    fn the_routes_block_follows_the_containment_block() {
        let preopens = vec![PreopenReport {
            artifact: "fs-tool".to_string(),
            role: "tool".to_string(),
            scope: None,
            surface: PreopenSurface::WholeWorkdir,
            runtime_dir_reach: None,
            extra_roots: Vec::new(),
        }];
        let report = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            Some(6),
            None,
            Vec::new(),
            Vec::new(),
            preopens,
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(EnforcementTier::KernelFull),
            IoMaxReport::default(),
        )
        .with_routes(None);
        let rendered = report.render();
        assert!(!rendered.contains("covers:"), "{rendered}");
        let routes = rendered.find("\nRoutes\n").expect("a Routes block");
        assert!(
            routes > rendered.find("  mechanism: ").unwrap(),
            "{rendered}"
        );
        assert!(
            routes < rendered.find("\nRuntime directory\n").unwrap(),
            "{rendered}"
        );
        assert!(
            rendered.contains("  shell commands    present  landlock+seccomp\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "  WASM tools        present  host process, not confined by the class (fs-tool)\n    \
                 Runs inside the mur process"
            ),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("  process harness   absent   host process, not confined by the class\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("  cgroup scope: not-probed\n"),
            "{rendered}"
        );
    }

    /// `W-SEC-034` fires only at a `scoped` or `sealed` floor, and names exactly the present
    /// routes the class does not cover, with their artifacts.
    #[test]
    fn the_uncovered_route_warning_names_exactly_the_present_uncovered_routes() {
        let inputs = RouteInputs {
            shell_allow: true,
            tools: vec!["fs-tool".to_string()],
            drivers: vec!["driver-anthropic".to_string()],
            ..RouteInputs::default()
        };
        let routes = route_table(&inputs, "landlock+seccomp");
        assert_eq!(
            uncovered_route_warning(ContainmentClass::Advisory, &routes),
            None
        );

        let covered_only = route_table(
            &RouteInputs {
                shell_allow: true,
                plan_submit: true,
                ..RouteInputs::default()
            },
            "landlock+seccomp",
        );
        assert_eq!(
            uncovered_route_warning(ContainmentClass::Scoped, &covered_only),
            None
        );

        for declared in [ContainmentClass::Scoped, ContainmentClass::Sealed] {
            let message = uncovered_route_warning(declared, &routes).expect("fires");
            assert!(
                message.contains(&format!("the declared {declared} floor")),
                "{message}"
            );
            assert!(
                message.contains(
                    "WASM tools (fs-tool) and drivers (driver-anthropic) run outside the class"
                ),
                "{message}"
            );
            assert!(message.contains("nothing is refused"), "{message}");
            for absent in ["hooks", "process harness", "harness tools"] {
                assert!(!message.contains(absent), "{absent}: {message}");
            }
        }

        let process = route_table(
            &RouteInputs {
                process_transport: true,
                drivers: vec!["driver-claude-code".to_string()],
                ..RouteInputs::default()
            },
            "landlock+seccomp",
        );
        let message = uncovered_route_warning(ContainmentClass::Scoped, &process).unwrap();
        assert!(
            message.contains(
                "drivers (driver-claude-code), the process harness and harness tools run"
            ),
            "{message}"
        );
    }

    /// The emitter writes the code, the message and the link to the bootstrap log it is handed.
    #[test]
    fn the_uncovered_route_warning_reaches_the_bootstrap_log() {
        let temp = tempfile::tempdir().unwrap();
        let routes = route_table(
            &RouteInputs {
                drivers: vec!["d".to_string()],
                ..RouteInputs::default()
            },
            "landlock+seccomp",
        );
        warn_on_uncovered_routes(
            ContainmentClass::Sealed,
            &routes,
            Some(&crate::workdir_fs::WorkdirRoot::open(temp.path()).unwrap()),
        );
        let log = std::fs::read_to_string(temp.path().join("logs").join("bootstrap.log"))
            .unwrap_or_default();
        assert_eq!(log.matches("warning[W-SEC-034]").count(), 1, "{log}");
        assert!(log.contains("drivers (d)"), "{log}");
        assert!(log.contains("#w-sec-034"), "{log}");
    }

    /// Without a kernel filesystem mechanism `HOME` is relocated and nothing more; a Landlock
    /// session does not carry the statement and a composed root carries nothing.
    #[test]
    fn an_advisory_session_says_home_is_relocated_not_protected() {
        for tier in [
            EnforcementTier::EnvironmentOnly,
            EnforcementTier::KernelSeccompOnly,
        ] {
            let boundary = filesystem_boundary(tier);
            let home = boundary
                .not_protected
                .iter()
                .find(|statement| statement.contains("relocated, not protected"))
                .unwrap_or_else(|| panic!("{tier:?}: {boundary:?}"));
            assert!(home.contains("session's home folder"), "{home}");
            assert!(
                home.contains("real home directory stays readable and writable by absolute path"),
                "{home}"
            );
            assert!(home.contains("no kernel mechanism"), "{home}");
        }
        assert!(!filesystem_boundary(EnforcementTier::KernelFull)
            .not_protected
            .iter()
            .any(|statement| statement.contains("relocated, not protected")));
        assert!(filesystem_boundary(EnforcementTier::KernelSealed)
            .not_protected
            .is_empty());
    }

    /// A host that mediates nothing says so, in the block an operator reads before the grants.
    #[test]
    fn a_session_with_no_kernel_mechanism_reports_what_it_does_not_protect() {
        let report = boundary_report(ContainmentClass::Advisory, EnforcementTier::EnvironmentOnly);

        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Advisory
        );
        // The filesystem, path rewriting, the relocated HOME, and the runtime and records
        // directories no command is kept from.
        assert_eq!(report.filesystem_boundary.not_protected.len(), 5);
        assert_eq!(
            report.filesystem_boundary.not_protected[2],
            ADVISORY_HOME_NOT_PROTECTED
        );
        assert_eq!(
            Some(&report.filesystem_boundary.not_protected[3]),
            report.runtime_dir.not_protected_because.as_ref()
        );
        assert_eq!(
            Some(&report.filesystem_boundary.not_protected[4]),
            report.records_dir.not_protected_because.as_ref()
        );
        assert!(report.filesystem_boundary.not_protected[4].contains("the records directory"));

        let filesystem = &report.filesystem_boundary.not_protected[0];
        assert!(filesystem.contains("advisory"), "{filesystem}");
        assert!(
            filesystem.contains("denies a read or a write outside the work folder"),
            "{filesystem}"
        );

        let rewriting = &report.filesystem_boundary.not_protected[1];
        assert!(rewriting.contains("not a boundary"), "{rewriting}");
        assert!(rewriting.contains("HOME"), "{rewriting}");

        let rendered = report.render();
        let heading = rendered
            .find("\nNot protected here\n")
            .expect("a session that composes no root prints the block");
        assert!(
            heading > rendered.find("Containment\n").unwrap(),
            "the block belongs under Containment:\n{rendered}"
        );
        assert!(
            heading < rendered.find("\nEffective grants\n").unwrap(),
            "the block belongs ahead of the grants an operator reads it against:\n{rendered}"
        );
        for statement in &report.filesystem_boundary.not_protected {
            assert!(rendered.contains(statement.as_str()), "{rendered}");
        }
    }

    fn inside_placement() -> RuntimeDirPlacement {
        RuntimeDirPlacement {
            sessions_dir: std::path::PathBuf::from("/home/dev/.murmur/sessions"),
            records_group: std::path::PathBuf::from("/home/dev/.murmur/records/home/dev"),
            workdir: std::path::PathBuf::from("/home/dev"),
            sessions_inside_workdir: true,
            records_inside_workdir: true,
        }
    }

    /// One claim per applied tier, from the tier, the ruleset ABI and the placement alone.
    #[test]
    fn runtime_dir_report_makes_one_claim_per_tier() {
        let outside = RuntimeDirPlacement::outside_for_test();
        for abi in [None, Some(1), Some(2), Some(3), Some(6)] {
            let sealed = runtime_dir_report(EnforcementTier::KernelSealed, abi, &outside);
            assert!(sealed.read_only_to_commands, "sealed at {abi:?}");
            assert_eq!(sealed.enforced_by, Some("mountns+landlock"));
            assert_eq!(sealed.not_protected_because, None);
        }

        let full = runtime_dir_report(EnforcementTier::KernelFull, Some(3), &outside);
        assert!(full.read_only_to_commands);
        assert_eq!(full.enforced_by, Some("landlock"));
        assert_eq!(full.not_protected_because, None);

        for abi in [Some(1), Some(2)] {
            let old = runtime_dir_report(EnforcementTier::KernelFull, abi, &outside);
            assert!(!old.read_only_to_commands);
            assert_eq!(old.enforced_by, None);
            let reason = old.not_protected_because.unwrap();
            assert!(
                reason.contains(&format!("Landlock ABI {}", abi.unwrap()))
                    && reason.contains("truncate(2)")
                    && reason.contains("6.2"),
                "{reason}"
            );
        }

        let seccomp = runtime_dir_report(EnforcementTier::KernelSeccompOnly, None, &outside);
        assert!(!seccomp.read_only_to_commands);
        assert!(seccomp
            .not_protected_because
            .unwrap()
            .contains("no Landlock"));

        let none = runtime_dir_report(EnforcementTier::EnvironmentOnly, None, &outside);
        assert!(!none.read_only_to_commands);
        let reason = none.not_protected_because.unwrap();
        assert!(
            reason.contains("no kernel containment") && reason.contains("macOS"),
            "{reason}"
        );

        assert_eq!(
            full.path, "/home/test/.murmur/sessions/<session-id>/runtime",
            "the path carries the literal session-id segment"
        );
    }

    /// Inside the workdir, nothing protects it on any tier, and the reason names both paths.
    #[test]
    fn runtime_dir_report_inside_the_workdir_is_never_protected() {
        for tier in ALL_TIERS {
            let report = runtime_dir_report(*tier, Some(6), &inside_placement());
            assert!(!report.read_only_to_commands, "{tier:?}");
            assert_eq!(report.enforced_by, None);
        }
        let reason =
            runtime_dir_report(EnforcementTier::KernelSealed, Some(6), &inside_placement())
                .not_protected_because
                .unwrap();
        assert!(
            reason.contains("/home/dev/.murmur/sessions/<session-id>/runtime"),
            "{reason}"
        );
        assert!(reason.contains("inside the workdir /home/dev"), "{reason}");
    }

    /// A runtime directory inside the workdir caps the class at `advisory`, refuses a `scoped` or
    /// `sealed` floor naming both paths and the fix, and lets an `advisory` launch through.
    #[test]
    fn runtime_dir_inside_the_workdir_caps_the_class_and_refuses_a_kernel_floor() {
        let inside = inside_placement();
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelSealed, false, true),
            ContainmentClass::Advisory
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelSealed, false, false),
            ContainmentClass::Sealed
        );
        for declared in [ContainmentClass::Scoped, ContainmentClass::Sealed] {
            match check_containment_floor(
                declared,
                ContainmentClass::Advisory,
                None,
                false,
                inside.inside(),
            ) {
                Err(RuntimeError::ContainmentFloorUnmet { reason, .. }) => {
                    assert!(reason.contains("/home/dev/.murmur/sessions"), "{reason}");
                    assert!(reason.contains("the workdir /home/dev"), "{reason}");
                    assert!(
                        reason.contains("run with a --workdir that does not contain ~/.murmur"),
                        "{reason}"
                    );
                }
                other => panic!("{declared:?}: {other:?}"),
            }
        }
        assert!(check_containment_floor(
            ContainmentClass::Advisory,
            ContainmentClass::Advisory,
            None,
            false,
            inside.inside()
        )
        .is_ok());

        let report = report_for(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            Some(6),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &inside,
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelFull,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        );
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        let reason = report.runtime_dir.not_protected_because.clone().unwrap();
        assert!(report.filesystem_boundary.not_protected.contains(&reason));
        let rendered = report.render();
        assert!(
            rendered.contains(&format!("  commands:  can change it — {reason}\n")),
            "{rendered}"
        );
    }

    /// The block `--explain-scope` prints after `Containment`, and the JSON object it renders.
    #[test]
    fn runtime_dir_block_renders_after_containment_and_serializes_every_field() {
        let report = boundary_report(ContainmentClass::Scoped, EnforcementTier::KernelFull);
        let rendered = report.render();
        let block =
            "Runtime directory\n  path:      /home/test/.murmur/sessions/<session-id>/runtime\n  \
                     commands:  read only (landlock)\n";
        let at = rendered.find(block).expect(&rendered);
        assert!(at > rendered.find("Containment\n").unwrap());
        assert!(at < rendered.find("Effective grants").unwrap());

        let sealed = boundary_report(ContainmentClass::Sealed, EnforcementTier::KernelSealed);
        assert!(sealed
            .render()
            .contains("  commands:  read only (mountns+landlock)\n"));

        let json = serde_json::to_string(&report.runtime_dir).unwrap();
        assert_eq!(
            json,
            r#"{"path":"/home/test/.murmur/sessions/<session-id>/runtime","read_only_to_commands":true,"enforced_by":"landlock","not_protected_because":null}"#
        );
    }

    /// A Landlock domain really does mediate the filesystem, so its statement says what Landlock
    /// leaves open rather than repeating the advisory one.
    #[test]
    fn a_landlock_session_reports_denial_rather_than_absence() {
        let report = boundary_report(ContainmentClass::Advisory, EnforcementTier::KernelFull);

        assert_eq!(report.achieved_containment, ContainmentClass::Scoped);
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Enforced
        );
        assert_eq!(report.filesystem_boundary.not_protected.len(), 2);

        let filesystem = &report.filesystem_boundary.not_protected[0];
        assert!(
            !filesystem.contains("advisory"),
            "Landlock mediates the filesystem here: {filesystem}"
        );
        assert!(
            filesystem.contains("denied rather than absent"),
            "{filesystem}"
        );
        for probe in ["stat", "access", "readlink"] {
            assert!(filesystem.contains(probe), "{filesystem}");
        }
    }

    /// A capsule that declares less than `sealed` composes no root on a sealed-capable host: it
    /// runs Landlock-scoped, is reported `scoped` on `landlock+seccomp`, and the block says so
    /// rather than claim the paths outside its grants are absent.
    #[test]
    fn a_sealed_capable_host_still_discloses_landlock_for_a_capsule_that_declared_less() {
        let report = boundary_report(ContainmentClass::Advisory, EnforcementTier::KernelSealed);

        assert_eq!(report.achieved_containment, ContainmentClass::Scoped);
        assert_eq!(report.enforcement_tier, "landlock+seccomp");
        assert_eq!(
            report.host_containment,
            ContainmentClass::Sealed,
            "the host can back sealed, which only `host_containment` says"
        );
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Enforced,
            "no composed root is installed for a capsule that did not declare one"
        );
        assert_eq!(
            report.filesystem_boundary.not_protected,
            boundary_report(ContainmentClass::Advisory, EnforcementTier::KernelFull)
                .filesystem_boundary
                .not_protected,
            "the same mechanism gets the same statements on either host"
        );
        assert!(report.render().contains("Not protected here"));
    }

    /// The half of the block that is genuinely mechanism-independent: `HOME` is rewritten on every
    /// tier, so the statement about it is one string, not one per mechanism.
    #[test]
    fn the_path_rewriting_statement_is_identical_wherever_it_is_carried() {
        let advisory =
            boundary_report(ContainmentClass::Advisory, EnforcementTier::EnvironmentOnly);
        let enforced = boundary_report(ContainmentClass::Advisory, EnforcementTier::KernelFull);

        assert_eq!(
            advisory.filesystem_boundary.not_protected[1],
            enforced.filesystem_boundary.not_protected[1]
        );
        assert!(
            advisory.filesystem_boundary.not_protected[1]
                .contains("home/ in its records directory"),
            "the statement must name the directory `shell::build_shell_env` really sets HOME to"
        );
    }

    /// A composed root prints no heading — not a heading with an empty list under it — and still
    /// carries the key, because an absent key must identify a runtime that predates the field.
    #[test]
    fn a_composed_root_carries_the_key_with_nothing_to_disclaim() {
        let report = boundary_report(ContainmentClass::Sealed, EnforcementTier::KernelSealed);

        assert_eq!(report.achieved_containment, ContainmentClass::Sealed);
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Absent
        );
        assert!(report.filesystem_boundary.not_protected.is_empty());
        assert!(
            !report.render().contains("Not protected here"),
            "{}",
            report.render()
        );

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["filesystem_boundary"]["restriction"], "absent");
        assert_eq!(
            json["filesystem_boundary"]["not_protected"],
            serde_json::json!([]),
            "the key is present and empty at absent, never missing"
        );
    }

    /// `capabilities.filesystem.workdir_exec` caps the achieved *class* at `advisory` and leaves
    /// the *mechanism* alone: Landlock is still installed, so the block prints the Landlock
    /// statement rather than claiming the host denies nothing.
    #[test]
    fn workdir_exec_caps_the_class_without_changing_the_mechanism() {
        let policy = CapabilityPolicy {
            workdir_exec_allowed: true,
            ..CapabilityPolicy::default()
        };

        let report = report_for(
            &policy,
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            Some(6),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
            crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
            )),
            IoMaxReport::default(),
        );

        assert_eq!(report.enforcement_tier, "landlock+seccomp");
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert_eq!(
            report.filesystem_boundary.restriction,
            FilesystemRestriction::Enforced
        );
        assert_eq!(report.filesystem_boundary.not_protected.len(), 2);
        assert!(report.render().contains("Not protected here"));
    }

    /// The restriction follows the tier the session applies, at every combination of host tier and
    /// declared floor — which is what `runtime_writes` beside it is already keyed on.
    #[test]
    fn the_restriction_follows_the_tier_the_session_would_install() {
        for tier in ALL_TIERS {
            for declared in [
                ContainmentClass::Advisory,
                ContainmentClass::Scoped,
                ContainmentClass::Sealed,
            ] {
                let report = boundary_report(declared, *tier);
                let expected = match crate::sandbox::applied_tier(*tier, declared) {
                    EnforcementTier::KernelSealed => FilesystemRestriction::Absent,
                    EnforcementTier::KernelFull => FilesystemRestriction::Enforced,
                    EnforcementTier::KernelSeccompOnly | EnforcementTier::EnvironmentOnly => {
                        FilesystemRestriction::Advisory
                    }
                    EnforcementTier::Unconfined => FilesystemRestriction::None,
                };
                assert_eq!(
                    report.filesystem_boundary.restriction, expected,
                    "{tier:?} / {declared:?}"
                );
                assert_eq!(
                    report.filesystem_boundary.not_protected.is_empty(),
                    expected == FilesystemRestriction::Absent,
                    "{tier:?} / {declared:?}"
                );
                let wire = match expected {
                    FilesystemRestriction::Advisory => "advisory",
                    FilesystemRestriction::Enforced => "enforced",
                    FilesystemRestriction::Absent => "absent",
                    FilesystemRestriction::None => "none",
                };
                assert_eq!(
                    serde_json::to_value(report.filesystem_boundary.restriction).unwrap(),
                    serde_json::Value::String(wire.to_string())
                );
            }
        }
    }

    /// The one assertion the report cannot satisfy by agreeing with itself: what a subprocess in a
    /// real session actually gets back when it probes a host file outside its workdir, compared
    /// against the `restriction` the report prints for that same session.
    ///
    /// Runs on whatever tier this host reaches, and asserts against the restriction it observes
    /// rather than against a tier it assumes. `bash` reads the file with a redirect and tests its
    /// existence with `[[ -e ]]` — both are syscalls the shell makes itself, so nothing here needs
    /// a second binary on the exec allowlist.
    ///
    /// Driving `execute_shell` needs an egress network namespace, so it stands down where the host
    /// withholds one rather than failing: a host that restricts unprivileged user namespaces
    /// refuses the uid_map write every spawn goes through, whatever the filesystem tier.
    #[test]
    fn the_reported_restriction_matches_what_a_subprocess_actually_reaches() {
        if crate::network_namespace::skip_without_egress_namespace(
            "the_reported_restriction_matches_what_a_subprocess_actually_reaches",
        ) {
            return;
        }
        if crate::sandbox::find_on_path("bash").is_none() {
            eprintln!(
                "[SKIP-HOST] the_reported_restriction_matches_what_a_subprocess_actually_reaches: \
                 no bash on PATH"
            );
            return;
        }
        let outside = tempfile::tempdir().expect("a temp directory outside the workdir");
        let secret = outside.path().join("host-file");
        std::fs::write(&secret, "PROBE-CONTENTS\n").unwrap();
        let secret = secret.to_str().unwrap().to_string();

        for declared in [ContainmentClass::Advisory, ContainmentClass::Sealed] {
            let workdir = tempfile::tempdir().unwrap();
            let policy = CapabilityPolicy {
                shell_allow: vec!["bash".to_string()],
                ..CapabilityPolicy::default()
            };
            let probe = crate::sandbox::HostProbe::probe();
            let host_tier = probe.tier();
            let runtime = tempfile::tempdir().unwrap();
            let enforcement =
                crate::sandbox::ShellEnforcement::resolve_declared(&policy, declared, probe)
                    .unwrap()
                    .with_runtime_dir(crate::sandbox::test_runtime_dir(runtime.path()), None);
            let report = report_for(
                &policy,
                declared,
                host_tier,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &crate::runtime_dir::RuntimeDirPlacement::outside_for_test(),
                crate::temp_dir::tier_temp_mapping(crate::sandbox::applied_tier(
                    host_tier, declared,
                )),
                IoMaxReport::default(),
            );

            let command = format!(
                "if read -r line < '{secret}' 2>/dev/null; then echo \"read=$line\"; \
                 else echo read=refused; fi; \
                 if [[ -e '{secret}' ]]; then echo exists=yes; else echo exists=no; fi"
            );
            let result = crate::shell::execute_shell(
                "bash",
                &["-c", &command],
                &[],
                &crate::workdir_fs::WorkdirRoot::open(workdir.path()).unwrap(),
                &policy,
                &enforcement,
            )
            .unwrap_or_else(|error| panic!("declared {declared:?}: {error}"));
            let seen = result.stdout;

            match report.filesystem_boundary.restriction {
                FilesystemRestriction::Advisory => assert!(
                    seen.contains("read=PROBE-CONTENTS"),
                    "reported advisory, so nothing denies the read: {seen}"
                ),
                FilesystemRestriction::Enforced => assert!(
                    seen.contains("read=refused") && seen.contains("exists=yes"),
                    "reported enforced, so the read is denied and the path still exists: {seen}"
                ),
                FilesystemRestriction::Absent => assert!(
                    seen.contains("read=refused") && seen.contains("exists=no"),
                    "reported absent, so the path is not in the composed root: {seen}"
                ),
                FilesystemRestriction::None => assert!(
                    seen.contains("read=PROBE-CONTENTS"),
                    "reported none, so nothing denies the read: {seen}"
                ),
            }
        }
    }

    /// The `pub` field names of `CapabilityPolicy`, read out of `types.rs`'s own text.
    fn capability_policy_field_names(source: &str) -> Vec<String> {
        source
            .split_once("pub struct CapabilityPolicy {")
            .expect("types.rs declares CapabilityPolicy")
            .1
            .split_once("\n}")
            .expect("CapabilityPolicy's declaration is closed at column 0")
            .0
            .lines()
            .filter_map(|line| {
                line.strip_prefix("    pub ")
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(name, _)| name.to_string())
            })
            .collect()
    }

    /// Every row of the per-tier `/tmp` table, through the decision staging makes and the report
    /// built from it: exact `tmp_mapped`, `mapped_by` and `tmpdir`, and a reason exactly when not
    /// mapped. The report the session records is the one `scope_report_for_tier` builds from the
    /// same answer.
    #[test]
    fn the_temp_dir_report_follows_the_per_tier_table() {
        use crate::temp_dir::temp_mapping;

        let placement = RuntimeDirPlacement::outside_for_test();
        let folder = "/home/test/.murmur/records/work/<session-id>/tmp";
        let tmp = crate::temp_dir::canonical_tmp();
        let elsewhere = std::path::Path::new("/work");
        // (host tier, declared, mount namespace usable, workdir, tmp_mapped, mapped_by)
        let rows = [
            (
                EnforcementTier::KernelSealed,
                ContainmentClass::Sealed,
                true,
                elsewhere,
                true,
                Some("composed-root"),
            ),
            (
                EnforcementTier::KernelSealed,
                ContainmentClass::Scoped,
                true,
                elsewhere,
                true,
                Some("mount-namespace"),
            ),
            (
                EnforcementTier::KernelSealed,
                ContainmentClass::Advisory,
                true,
                elsewhere,
                true,
                Some("mount-namespace"),
            ),
            (
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
                true,
                elsewhere,
                true,
                Some("mount-namespace"),
            ),
            (
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
                false,
                elsewhere,
                false,
                None,
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                ContainmentClass::Advisory,
                true,
                elsewhere,
                true,
                Some("mount-namespace"),
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                ContainmentClass::Advisory,
                false,
                elsewhere,
                false,
                None,
            ),
            (
                EnforcementTier::EnvironmentOnly,
                ContainmentClass::Advisory,
                false,
                elsewhere,
                false,
                None,
            ),
            (
                EnforcementTier::KernelFull,
                ContainmentClass::Scoped,
                true,
                tmp.as_path(),
                false,
                None,
            ),
            (
                EnforcementTier::KernelSeccompOnly,
                ContainmentClass::Advisory,
                true,
                tmp.as_path(),
                false,
                None,
            ),
        ];
        for (tier, declared, usable, workdir, mapped, mapped_by) in rows {
            let applied = crate::sandbox::applied_tier(tier, declared);
            let mapping = temp_mapping(applied, usable, workdir);
            let report = temp_dir_report(mapping, &placement);
            let row = format!(
                "{tier:?} {declared:?} usable={usable} workdir={}",
                workdir.display()
            );
            assert_eq!(report.path, folder, "{row}");
            assert_eq!(report.tmp_mapped, mapped, "{row}");
            assert_eq!(report.mapped_by, mapped_by, "{row}");
            assert_eq!(report.tmpdir, if mapped { "/tmp" } else { folder }, "{row}");
            assert_eq!(
                report
                    .not_mapped_because
                    .as_deref()
                    .is_some_and(|reason| !reason.is_empty()),
                !mapped,
                "{row}"
            );

            let scope = report_for(
                &CapabilityPolicy::default(),
                declared,
                tier,
                None,
                None,
                None,
                None,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                &placement,
                mapping,
                IoMaxReport::default(),
            );
            assert_eq!(scope.temp_dir, report, "{row}");
        }

        let reason = |tier, usable, workdir| {
            temp_dir_report(temp_mapping(tier, usable, workdir), &placement)
                .not_mapped_because
                .unwrap()
        };
        assert!(reason(EnforcementTier::EnvironmentOnly, false, elsewhere)
            .starts_with("this host has no kernel containment for capsule commands"));
        assert!(reason(EnforcementTier::KernelSeccompOnly, false, elsewhere)
            .contains("neither a private mount namespace nor Landlock"));
        assert!(reason(EnforcementTier::KernelFull, false, elsewhere)
            .contains("a literal /tmp path is refused"));
        assert!(reason(EnforcementTier::KernelFull, true, tmp.as_path())
            .starts_with("the workdir is /tmp itself"));
    }

    /// The `Temporary folder` block: its shape mapped and not, its JSON keys, and its place after
    /// the records directory in `--explain-scope`.
    #[test]
    fn the_temp_folder_block_renders_after_the_records_directory() {
        use crate::temp_dir::{NotMappedReason, TempMapping};

        let placement = RuntimeDirPlacement::outside_for_test();
        let mapped = temp_dir_report(TempMapping::MountNamespace, &placement);
        assert_eq!(
            render_temp_dir(&mapped),
            "Temporary folder\n  path:      /home/test/.murmur/records/work/<session-id>/tmp\n  \
             TMPDIR:    /tmp\n  /tmp:      mapped (mount-namespace)\n"
        );
        let unmapped = temp_dir_report(
            TempMapping::NotMapped(NotMappedReason::NoMountNamespace),
            &placement,
        );
        assert_eq!(
            render_temp_dir(&unmapped),
            format!(
                "Temporary folder\n  path:      {path}\n  TMPDIR:    {path}\n  /tmp:      not \
                 mapped — {}\n",
                NotMappedReason::NoMountNamespace.describe(),
                path = "/home/test/.murmur/records/work/<session-id>/tmp"
            )
        );
        assert_eq!(
            serde_json::to_value(&mapped).unwrap(),
            serde_json::json!({
                "path": "/home/test/.murmur/records/work/<session-id>/tmp",
                "tmpdir": "/tmp",
                "tmp_mapped": true,
                "mapped_by": "mount-namespace",
                "not_mapped_because": null,
            })
        );

        let scope = report_for(
            &CapabilityPolicy::default(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            &placement,
            TempMapping::MountNamespace,
            IoMaxReport::default(),
        );
        let rendered = scope.render();
        let records = rendered.find("Records directory\n").expect("records block");
        let temp = rendered.find("Temporary folder\n").expect("temp block");
        assert!(records < temp, "{rendered}");
        assert!(rendered.contains(&render_temp_dir(&mapped)), "{rendered}");
        assert!(serde_json::to_value(&scope)
            .unwrap()
            .get("temp_dir")
            .is_some());
    }
}
