//! Containment classes: what an operator *declares* versus what a host actually *achieves*.
//!
//! The two directions are deliberately kept apart:
//!
//! * The **declared floor** ([`murmur_artifact::ContainmentClass`]) is a requirement, combined
//!   from the manifest, the workspace config and `--containment` by taking the strongest (see
//!   [`murmur_artifact::effective_containment_floor`]).
//! * The **achieved class** starts from [`EnforcementTier`], which is host-probed at launch time.
//!   [`achieved_class_for_tier`] takes nothing else — not the manifest, not the grant set — so no
//!   declaration can ever talk a host into reporting a class it cannot back. Exactly one manifest
//!   property may then talk a *capsule* down from that ceiling, and only downwards:
//!   `capabilities.filesystem.workdir_exec: true` caps the result at
//!   [`ContainmentClass::Advisory`] (see [`achieved_containment_class`]), because a capsule whose
//!   own workdir is executable has given up the claim `scoped` makes. The cap is applied in a
//!   separate wrapper rather than inside [`achieved_class_for_tier`] precisely so the "no
//!   declaration raises a class" property stays a property of one small, pure function.
//!
//! [`ContainmentClass::Sealed`] is reachable exactly when [`EnforcementTier::KernelSealed`] is:
//! a Linux host with a usable Landlock ABI, AppArmor out of the way of unprivileged user
//! namespaces, and a namespace probe that really created one. See [`crate::sealed`] for the
//! mechanism and for the [`SealedBlocker`] taxonomy that turns "not sealed here" into a specific
//! command an operator can run.
//!
//! The shortfall reason for `sealed` is therefore *mechanism-specific*, not a fixed string: a
//! host missing the AppArmor profile and a container missing `CAP_SYS_ADMIN` both fail to reach
//! `sealed`, but they are fixed in completely different places, and a refusal that cannot tell
//! them apart is a refusal an operator cannot act on.

use murmur_artifact::{ContainmentClass, ExportMode, FileExport};
use serde::Serialize;

use crate::{
    errors::RuntimeError,
    sandbox::{detect_enforcement_tier, EnforcementTier},
    sealed::{SealedBlocker, UsernsGrant},
    types::CapabilityPolicy,
};

pub use crate::sandbox::{detect_sealed_blocker, detect_userns_grant};

/// The class a host in `tier` can actually back, and nothing stronger.
///
/// Pure — the host probe lives in [`detect_enforcement_tier`], mirroring the
/// `tier_from_probe`/`detect_enforcement_tier` split in `sandbox.rs` so this mapping stays
/// unit-testable on any OS.
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
    }
}

/// The class a session actually achieves: the host's ceiling from [`achieved_class_for_tier`],
/// capped by what the capsule's own `capabilities.filesystem.workdir_exec` leaves standing.
///
/// This is the one place a *manifest* property lowers an achieved class, and it does so by capping
/// rather than by deciding: [`achieved_class_for_tier`] above stays pure and tier-only, so no
/// declaration can talk a host up, and this wrapper can only ever talk a capsule *down*.
///
/// Why `workdir_exec` caps at [`ContainmentClass::Advisory`]: `scoped` (and `sealed` above it) both
/// claim that what runs inside the capsule is bounded by what the operator declared. With the
/// workdir's `Execute` right granted, a binary the agent compiles, downloads or renames inside its
/// own workdir runs whatever `capabilities.shell.allow` says — the allowlist stops being an
/// enforceable property. A host that could back `scoped` still cannot back a claim the capsule
/// itself has given up, so the honest report is `advisory`, on every tier.
pub(crate) fn achieved_containment_class(
    tier: EnforcementTier,
    workdir_exec: bool,
) -> ContainmentClass {
    let host_ceiling = achieved_class_for_tier(tier);
    if workdir_exec {
        host_ceiling.min(ContainmentClass::Advisory)
    } else {
        host_ceiling
    }
}

/// Probes this host and reports the strongest class it can actually provide.
///
/// Host-only, and deliberately still takes no capsule input: it answers "what can this machine
/// back?", which is what `mur doctor` and the escape-conformance harness ask. A *session* asks the
/// narrower question and must use [`achieved_containment_class`] with its own probed tier instead
/// — see `runtime::stage_session`, which probes the tier once and passes it to both that call and
/// [`scope_report_for_tier`].
pub fn detect_achieved_containment() -> ContainmentClass {
    achieved_class_for_tier(detect_enforcement_tier())
}

/// Why `achieved` falls short of `declared`, naming the missing *mechanism* rather than the
/// missing class. `None` when the floor is met.
///
/// Pure and host-independent: the caller supplies both classes *and* — for the `sealed` arm — the
/// already-probed [`SealedBlocker`], so this function still probes nothing and stays testable
/// without a kernel. `sealed_blocker` is `None` when the caller has not probed (or cannot: a
/// non-Linux host), in which case the arm falls back to naming the mechanism generically.
///
/// `workdir_exec` is the capsule's own `capabilities.filesystem.workdir_exec`. It is checked before
/// every host arm below, because it is the one shortfall an operator cannot fix by changing hosts:
/// no kernel backs `scoped` for a capsule that has declared its workdir executable. Naming a
/// missing Landlock ABI to someone whose real problem is a line in their own manifest sends them to
/// the wrong machine.
pub fn containment_shortfall_reason(
    declared: ContainmentClass,
    achieved: ContainmentClass,
    sealed_blocker: Option<SealedBlocker>,
    workdir_exec: bool,
) -> Option<String> {
    if achieved >= declared {
        return None;
    }

    if workdir_exec && declared > ContainmentClass::Advisory {
        return Some(WORKDIR_EXEC_SHORTFALL.to_string());
    }

    let reason = match declared {
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
        ContainmentClass::Sealed => match sealed_blocker {
            Some(blocker) => blocker.reason(),
            None => "sealed requires a private mount namespace pivoted onto a composed root \
                     (unshare(CLONE_NEWUSER|CLONE_NEWNS) + pivot_root), which this host did not \
                     provide"
                .to_string(),
        },
    };

    Some(reason)
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

/// The refusal gate: `Ok(())` when the host's `achieved` class meets or exceeds the `declared`
/// floor, [`RuntimeError::ContainmentFloorUnmet`] otherwise.
///
/// Pure — takes every input as an argument and probes nothing, so the whole decision is
/// testable without a kernel.
pub fn check_containment_floor(
    declared: ContainmentClass,
    achieved: ContainmentClass,
    sealed_blocker: Option<SealedBlocker>,
    workdir_exec: bool,
) -> Result<(), RuntimeError> {
    match containment_shortfall_reason(declared, achieved, sealed_blocker, workdir_exec) {
        None => Ok(()),
        Some(reason) => Err(RuntimeError::ContainmentFloorUnmet {
            declared,
            achieved,
            reason,
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

/// Read-only answer to "what would this capsule actually be allowed to do on this host?".
///
/// Built from the already-parsed [`CapabilityPolicy`] plus one host tier probe. Nothing here
/// stages artifacts, touches a registry, creates a workdir or resolves DNS — it is a diagnostic,
/// so it reports even when [`Self::floor_met`] is `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeReport {
    /// Strongest class any source asked for (manifest / workspace config / `--containment`).
    pub declared_containment: ContainmentClass,
    /// Strongest class this host can actually back, derived from the tier probe alone.
    pub achieved_containment: ContainmentClass,
    /// `achieved_containment >= declared_containment`. `false` means `mur run` would refuse.
    pub floor_met: bool,
    /// The missing mechanism when `floor_met` is `false`; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortfall_reason: Option<String>,
    /// The probed host tier, as a stable wire name.
    pub enforcement_tier: &'static str,
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
    /// `capabilities.network.allow`, verbatim — declared destinations, not resolved IPs.
    pub network_allow: Vec<String>,
    /// `capabilities.network.unix_sockets`.
    pub unix_sockets: bool,
    /// `capabilities.shell.allow`, verbatim.
    pub shell_allow: Vec<String>,
    /// `capabilities.spawn.allow`, verbatim.
    pub spawn_allow: Vec<String>,
    /// `capabilities.env.allow`, verbatim.
    pub env_allow: Vec<String>,
    /// Every host directory a `capabilities.shell.interpreter_runtime` grant opens, rendered as
    /// `<binary>: <dir>[ (list_dir)]`. These are the paths outside the workdir that stay
    /// reachable even at `scoped`.
    pub interpreter_runtime_grants: Vec<String>,
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
    /// Every `capabilities.shell.staged_runtime` grant, rendered as
    /// `<binary>: <source_path> (pin: <pin>)`. These are host runtime trees a `sealed` capsule
    /// asks to have bind-mounted read-only into its composed root.
    ///
    /// Reported whatever the floor and whatever this host can back — a capsule that declares a
    /// staged runtime and cannot run here is exactly the case an operator is inspecting when they
    /// reach for `--explain-scope`, so hiding the grant behind a met floor would blank out the
    /// diagnostic in the one case it is for.
    pub staged_runtime_grants: Vec<String>,
}

impl ScopeReport {
    /// Human-readable rendering used by `mur run --explain-scope` without `--json`.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("Containment\n");
        out.push_str(&format!("  declared:  {}\n", self.declared_containment));
        out.push_str(&format!("  achieved:  {}\n", self.achieved_containment));
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

        out.push_str("\nEffective grants\n");
        out.push_str(&format!(
            "  filesystem scope: {}\n",
            self.filesystem_scope.as_deref().unwrap_or("<none>")
        ));
        out.push_str(&format!("  workdir exec:     {}\n", self.workdir_exec));
        push_list(&mut out, "network allow", &self.network_allow);
        out.push_str(&format!("  unix sockets:     {}\n", self.unix_sockets));
        push_list(&mut out, "shell allow", &self.shell_allow);
        push_list(&mut out, "spawn allow", &self.spawn_allow);
        push_list(&mut out, "env allow", &self.env_allow);
        push_list(
            &mut out,
            "interpreter runtime",
            &self.interpreter_runtime_grants,
        );
        push_list(&mut out, "staged runtime", &self.staged_runtime_grants);

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

        if !self.floor_met {
            out.push_str(
                "\nThis is a report only — `mur run` without --explain-scope would refuse to \
                 launch here.\n",
            );
        }
        out
    }
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
/// combined floor. Probes the host tier once and reads nothing else.
pub fn explain_scope(
    policy: &CapabilityPolicy,
    declared: ContainmentClass,
    exports_files: Option<&FileExport>,
) -> ScopeReport {
    scope_report_for_tier(
        policy,
        declared,
        detect_enforcement_tier(),
        detect_sealed_blocker(),
        detect_userns_grant(),
        exports_files,
    )
}

/// [`explain_scope`] with the tier and the sealed blocker injected — the seam every test uses so
/// no test depends on the host it happens to run on.
pub(crate) fn scope_report_for_tier(
    policy: &CapabilityPolicy,
    declared: ContainmentClass,
    tier: EnforcementTier,
    sealed_blocker: Option<SealedBlocker>,
    userns_grant: Option<UsernsGrant>,
    exports_files: Option<&FileExport>,
) -> ScopeReport {
    let achieved = achieved_containment_class(tier, policy.workdir_exec_allowed);
    let shortfall_reason = containment_shortfall_reason(
        declared,
        achieved,
        sealed_blocker,
        policy.workdir_exec_allowed,
    );

    ScopeReport {
        declared_containment: declared,
        achieved_containment: achieved,
        floor_met: shortfall_reason.is_none(),
        shortfall_reason,
        enforcement_tier: enforcement_tier_name(tier),
        userns_grant,
        // Copied straight through and never consulted above: `achieved` is already computed, and
        // an export must not be able to reach it.
        exports_files: exports_files.map(ExportsFilesReport::from),
        filesystem_scope: policy.filesystem_scope.clone(),
        workdir_exec: policy.workdir_exec_allowed,
        network_allow: policy.network_allow.clone(),
        unix_sockets: policy.unix_sockets_allowed,
        shell_allow: policy.shell_allow.clone(),
        spawn_allow: policy.spawn_allow.clone(),
        env_allow: policy.env_allow.clone(),
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
    }
}

/// Stable wire name for a tier, so `--explain-scope --json` consumers see the mechanism rather
/// than a `Debug` rendering that is free to change.
fn enforcement_tier_name(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::KernelSealed => "mountns+pivot_root+landlock+seccomp",
        EnforcementTier::KernelFull => "landlock+seccomp",
        EnforcementTier::KernelSeccompOnly => "seccomp-only",
        EnforcementTier::EnvironmentOnly => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            achieved_containment_class(EnforcementTier::KernelSealed, false),
            ContainmentClass::Sealed
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelFull, false),
            ContainmentClass::Scoped
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::KernelSeccompOnly, false),
            ContainmentClass::Advisory
        );
        assert_eq!(
            achieved_containment_class(EnforcementTier::EnvironmentOnly, false),
            ContainmentClass::Advisory
        );

        // With it: advisory, whatever the host could otherwise back.
        for tier in ALL_TIERS {
            assert_eq!(
                achieved_containment_class(*tier, true),
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
                achieved_containment_class(*tier, false),
                "the tier-only mapping and the uncapped wrapper must agree on tier {tier:?}"
            );
        }
    }

    /// The refusal an operator actually hits: `workdir_exec: true` plus
    /// `capabilities.containment: scoped`, on a host that could otherwise back `scoped`. It must
    /// refuse, and the reason must send them to their manifest rather than to another machine.
    #[test]
    fn workdir_exec_refuses_a_scoped_floor_even_on_a_landlock_capable_host() {
        let achieved = achieved_containment_class(EnforcementTier::KernelFull, true);
        assert_eq!(achieved, ContainmentClass::Advisory);

        let error = check_containment_floor(ContainmentClass::Scoped, achieved, None, true)
            .expect_err("workdir_exec + scoped must refuse");
        match error {
            RuntimeError::ContainmentFloorUnmet {
                declared,
                achieved,
                reason,
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
            Some(SealedBlocker::AppArmorProfileMissing),
            true,
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
            let achieved = achieved_containment_class(*tier, true);
            assert!(
                check_containment_floor(ContainmentClass::Advisory, achieved, None, true).is_ok(),
                "workdir_exec at an advisory floor must launch on tier {tier:?}"
            );
        }
    }

    #[test]
    fn advisory_floor_is_met_on_every_tier() {
        for tier in ALL_TIERS {
            let achieved = achieved_class_for_tier(*tier);
            assert!(
                check_containment_floor(ContainmentClass::Advisory, achieved, None, false).is_ok()
            );
        }
    }

    #[test]
    fn scoped_floor_is_met_on_every_landlock_capable_tier() {
        for tier in [EnforcementTier::KernelFull, EnforcementTier::KernelSealed] {
            let achieved = achieved_class_for_tier(tier);
            assert!(
                check_containment_floor(ContainmentClass::Scoped, achieved, None, false).is_ok()
            );
        }

        for tier in [
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let achieved = achieved_class_for_tier(tier);
            let error = check_containment_floor(ContainmentClass::Scoped, achieved, None, false)
                .expect_err("scoped must refuse without Landlock");
            match error {
                RuntimeError::ContainmentFloorUnmet {
                    declared,
                    achieved,
                    reason,
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

    /// The edge case the new tier makes possible: a capsule declaring the *weaker* class on a
    /// sealed-capable host. The floor is met (a stronger host satisfies a weaker requirement),
    /// and the applied mechanism is the declared one — asserted next door in
    /// `sandbox::applied_tier`'s tests, since a careless `match` here could force sealed-only
    /// behaviour onto a `scoped` declaration.
    #[test]
    fn a_weaker_declaration_is_satisfied_by_a_sealed_host_without_being_upgraded() {
        let achieved = achieved_class_for_tier(EnforcementTier::KernelSealed);
        assert_eq!(achieved, ContainmentClass::Sealed);
        assert!(check_containment_floor(ContainmentClass::Advisory, achieved, None, false).is_ok());
        assert!(check_containment_floor(ContainmentClass::Scoped, achieved, None, false).is_ok());
        assert_eq!(
            containment_shortfall_reason(ContainmentClass::Scoped, achieved, None, false),
            None
        );
    }

    #[test]
    fn sealed_floor_is_met_only_on_the_sealed_tier() {
        assert!(check_containment_floor(
            ContainmentClass::Sealed,
            achieved_class_for_tier(EnforcementTier::KernelSealed),
            None,
            false
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
                Some(SealedBlocker::NamespaceCreationDenied),
                false,
            )
            .expect_err("sealed must refuse on a host that cannot back it");
            match error {
                RuntimeError::ContainmentFloorUnmet {
                    declared, reason, ..
                } => {
                    assert_eq!(declared, ContainmentClass::Sealed);
                    assert!(
                        reason.contains("--cap-add SYS_ADMIN"),
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
            Some(SealedBlocker::AppArmorProfileMissing),
            false,
        )
        .expect("refusal");
        assert!(apparmor.contains("mur-sealed"));
        assert!(apparmor.contains("apparmor_parser -r"));
        assert!(!apparmor.contains("--cap-add"));

        let container = containment_shortfall_reason(
            ContainmentClass::Sealed,
            achieved,
            Some(SealedBlocker::NamespaceCreationDenied),
            false,
        )
        .expect("refusal");
        assert!(container.contains("--cap-add SYS_ADMIN"));
        assert!(container.contains("outside the container"));
        assert!(!container.contains("apparmor_parser"));

        // Never the pre-mechanism blanket text again, under any blocker.
        for blocker in [
            SealedBlocker::NotLinux,
            SealedBlocker::AppArmorProfileMissing,
            SealedBlocker::NamespaceCreationDenied,
            SealedBlocker::MountDenied,
            SealedBlocker::KernelUnsupported,
            SealedBlocker::LandlockUnavailable,
        ] {
            let reason = containment_shortfall_reason(
                ContainmentClass::Sealed,
                achieved,
                Some(blocker),
                false,
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
                false
            ),
            None
        );
        assert_eq!(
            containment_shortfall_reason(
                ContainmentClass::Scoped,
                ContainmentClass::Scoped,
                None,
                false
            ),
            None
        );
        assert_eq!(
            containment_shortfall_reason(
                ContainmentClass::Sealed,
                ContainmentClass::Sealed,
                Some(SealedBlocker::NamespaceCreationDenied),
                false
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

    #[test]
    fn scope_report_mirrors_the_policy_and_the_probed_tier() {
        let report = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
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

        let report = scope_report_for_tier(
            &policy,
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
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

    /// The field is always written, including `false`, so a consumer can distinguish "this capsule
    /// declared nothing" from "this runtime does not know the key".
    #[test]
    fn scope_report_json_always_carries_workdir_exec() {
        let value: serde_json::Value = serde_json::to_value(scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
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
            let report = scope_report_for_tier(
                &policy,
                ContainmentClass::Sealed,
                *tier,
                Some(SealedBlocker::NamespaceCreationDenied),
                None,
                None,
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
        let report = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Scoped,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
        );
        assert!(report.staged_runtime_grants.is_empty());
        assert!(report.render().contains("staged runtime: <none>"));
    }

    /// A capsule that declares `scoped` *and* opens host paths through `interpreter_runtime`
    /// still achieves only `scoped` — the grant set is reported, never consulted to upgrade
    /// the achieved class.
    #[test]
    fn grant_set_never_raises_the_achieved_class() {
        let report = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelFull,
            Some(SealedBlocker::NamespaceCreationDenied),
            None,
            None,
        );
        assert_eq!(report.achieved_containment, ContainmentClass::Scoped);
        assert!(!report.floor_met);
    }

    #[test]
    fn scope_report_still_reports_when_the_floor_is_unmet() {
        let report = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::EnvironmentOnly,
            Some(SealedBlocker::NotLinux),
            None,
            None,
        );

        assert_eq!(report.declared_containment, ContainmentClass::Sealed);
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert!(!report.floor_met);
        assert!(report.shortfall_reason.unwrap().contains("non-Linux"));
        assert_eq!(report.enforcement_tier, "none");
    }

    #[test]
    fn scope_report_names_the_sealed_mechanism_on_a_sealed_host() {
        let report = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
        );
        assert_eq!(report.achieved_containment, ContainmentClass::Sealed);
        assert!(report.floor_met);
        assert_eq!(
            report.enforcement_tier,
            "mountns+pivot_root+landlock+seccomp"
        );
    }

    #[test]
    fn scope_report_json_uses_wire_names() {
        let report = scope_report_for_tier(
            &CapabilityPolicy::default(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSeccompOnly,
            None,
            None,
            None,
        );
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["declared_containment"], "advisory");
        assert_eq!(value["achieved_containment"], "advisory");
        assert_eq!(value["floor_met"], true);
        assert_eq!(value["enforcement_tier"], "seccomp-only");
        // Absent rather than null when the floor is met.
        assert!(value.get("shortfall_reason").is_none());
    }

    /// The grant is reported, and it never moves a class, a floor or a mechanism: the same tier
    /// with four different grants produces four reports that differ in exactly one field.
    #[test]
    fn the_userns_grant_is_reported_without_changing_any_verdict() {
        let baseline = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSealed,
            None,
            None,
            None,
        );

        for grant in UsernsGrant::ALL {
            let report = scope_report_for_tier(
                &sample_policy(),
                ContainmentClass::Advisory,
                EnforcementTier::KernelSealed,
                None,
                Some(*grant),
                None,
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
        let rendered = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelFull,
            Some(SealedBlocker::AppArmorProfileMissing),
            None,
            None,
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
                let without = scope_report_for_tier(
                    &policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    None,
                );
                let with = scope_report_for_tier(
                    &policy,
                    ContainmentClass::Scoped,
                    *tier,
                    None,
                    None,
                    Some(&export),
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

    /// `exports_files` is written whether or not it was declared, on the same terms as
    /// `workdir_exec`: an absent key identifies an older runtime, a `null` a capsule that
    /// exported nothing.
    #[test]
    fn exports_files_is_always_serialized() {
        let undeclared = serde_json::to_value(scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
        ))
        .unwrap();
        assert_eq!(undeclared["exports_files"], serde_json::Value::Null);

        let declared = serde_json::to_value(scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            Some(&FileExport {
                root: "out/".to_string(),
                mode: ExportMode::ReadOnly,
                max_bytes: 10_485_760,
            }),
        ))
        .unwrap();
        assert_eq!(
            declared["exports_files"],
            serde_json::json!({"root": "out/", "mode": "read-only", "max_bytes": 10_485_760})
        );
    }

    #[test]
    fn render_names_the_resource_plane_in_both_directions() {
        let undeclared = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            None,
        )
        .render();
        assert!(
            undeclared.contains("Resource plane") && undeclared.contains("exports.files: <none>"),
            "{undeclared}"
        );

        let declared = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelFull,
            None,
            None,
            Some(&FileExport {
                root: "out/".to_string(),
                mode: ExportMode::ReadOnly,
                max_bytes: 10_485_760,
            }),
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
}
