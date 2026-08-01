//! Containment classes: what an operator *declares* versus what a host actually *achieves*.
//!
//! The two directions are deliberately kept apart:
//!
//! * The **declared floor** ([`murmur_artifact::ContainmentClass`]) is a requirement, combined
//!   from the manifest, the workspace config and `--containment` by taking the strongest (see
//!   [`murmur_artifact::effective_containment_floor`]).
//! * The **achieved class** is derived *only* from [`EnforcementTier`], which is host-probed at
//!   launch time. [`achieved_class_for_tier`] takes nothing else — not the manifest, not the
//!   grant set — so no declaration can ever talk a host into reporting a class it cannot back.
//!
//! [`ContainmentClass::Sealed`] is structurally unreachable here: `achieved_class_for_tier` has
//! no `Sealed` arm, because the mechanism (mount namespace + `pivot_root`) does not exist in this
//! runtime. Declaring `sealed` therefore refuses to launch on every host, including a modern
//! Linux one. That is the honest answer until a slice implements the mechanism, not a gap.

use murmur_artifact::ContainmentClass;
use serde::Serialize;

use crate::{
    errors::RuntimeError,
    sandbox::{detect_enforcement_tier, EnforcementTier},
    types::CapabilityPolicy,
};

/// The class a host in `tier` can actually back, and nothing stronger.
///
/// Pure — the host probe lives in [`detect_enforcement_tier`], mirroring the
/// `tier_from_probe`/`detect_enforcement_tier` split in `sandbox.rs` so this mapping stays
/// unit-testable on any OS.
pub(crate) fn achieved_class_for_tier(tier: EnforcementTier) -> ContainmentClass {
    match tier {
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

/// Probes this host and reports the strongest class it can actually provide.
pub fn detect_achieved_containment() -> ContainmentClass {
    achieved_class_for_tier(detect_enforcement_tier())
}

/// Why `achieved` falls short of `declared`, naming the missing *mechanism* rather than the
/// missing class. `None` when the floor is met.
///
/// Pure and host-independent: the caller supplies both classes.
pub fn containment_shortfall_reason(
    declared: ContainmentClass,
    achieved: ContainmentClass,
) -> Option<String> {
    if achieved >= declared {
        return None;
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
        ContainmentClass::Sealed => {
            "sealed requires mount-namespace + pivot_root isolation, which this runtime does not \
             implement yet; no host can provide it today"
                .to_string()
        }
    };

    Some(reason)
}

/// The refusal gate: `Ok(())` when the host's `achieved` class meets or exceeds the `declared`
/// floor, [`RuntimeError::ContainmentFloorUnmet`] otherwise.
///
/// Pure — takes both classes as arguments and probes nothing, so the whole decision is
/// testable without a kernel.
pub fn check_containment_floor(
    declared: ContainmentClass,
    achieved: ContainmentClass,
) -> Result<(), RuntimeError> {
    match containment_shortfall_reason(declared, achieved) {
        None => Ok(()),
        Some(reason) => Err(RuntimeError::ContainmentFloorUnmet {
            declared,
            achieved,
            reason,
        }),
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
    /// `capabilities.filesystem.scope`, verbatim.
    pub filesystem_scope: Option<String>,
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

        out.push_str("\nEffective grants\n");
        out.push_str(&format!(
            "  filesystem scope: {}\n",
            self.filesystem_scope.as_deref().unwrap_or("<none>")
        ));
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
pub fn explain_scope(policy: &CapabilityPolicy, declared: ContainmentClass) -> ScopeReport {
    scope_report_for_tier(policy, declared, detect_enforcement_tier())
}

/// [`explain_scope`] with the tier injected — the seam every test uses so no test depends on
/// the host it happens to run on.
pub(crate) fn scope_report_for_tier(
    policy: &CapabilityPolicy,
    declared: ContainmentClass,
    tier: EnforcementTier,
) -> ScopeReport {
    let achieved = achieved_class_for_tier(tier);
    let shortfall_reason = containment_shortfall_reason(declared, achieved);

    ScopeReport {
        declared_containment: declared,
        achieved_containment: achieved,
        floor_met: shortfall_reason.is_none(),
        shortfall_reason,
        enforcement_tier: enforcement_tier_name(tier),
        filesystem_scope: policy.filesystem_scope.clone(),
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
    }
}

/// Stable wire name for a tier, so `--explain-scope --json` consumers see the mechanism rather
/// than a `Debug` rendering that is free to change.
fn enforcement_tier_name(tier: EnforcementTier) -> &'static str {
    match tier {
        EnforcementTier::KernelFull => "landlock+seccomp",
        EnforcementTier::KernelSeccompOnly => "seccomp-only",
        EnforcementTier::EnvironmentOnly => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{InterpreterRuntimeDir, InterpreterRuntimeGrant};

    #[test]
    fn kernel_full_achieves_scoped() {
        assert_eq!(
            achieved_class_for_tier(EnforcementTier::KernelFull),
            ContainmentClass::Scoped
        );
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

    /// The invariant this whole slice exists to hold: `sealed` cannot be reported as achieved
    /// by any host, because no tier maps to it.
    #[test]
    fn no_tier_achieves_sealed() {
        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            assert_ne!(achieved_class_for_tier(tier), ContainmentClass::Sealed);
        }
    }

    #[test]
    fn advisory_floor_is_met_on_every_tier() {
        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let achieved = achieved_class_for_tier(tier);
            assert!(check_containment_floor(ContainmentClass::Advisory, achieved).is_ok());
        }
    }

    #[test]
    fn scoped_floor_is_met_only_on_a_landlock_host() {
        let full = achieved_class_for_tier(EnforcementTier::KernelFull);
        assert!(check_containment_floor(ContainmentClass::Scoped, full).is_ok());

        for tier in [
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let achieved = achieved_class_for_tier(tier);
            let error = check_containment_floor(ContainmentClass::Scoped, achieved)
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

    #[test]
    fn sealed_floor_is_refused_on_every_tier_including_landlock() {
        for tier in [
            EnforcementTier::KernelFull,
            EnforcementTier::KernelSeccompOnly,
            EnforcementTier::EnvironmentOnly,
        ] {
            let achieved = achieved_class_for_tier(tier);
            let error = check_containment_floor(ContainmentClass::Sealed, achieved)
                .expect_err("sealed must refuse everywhere until the mechanism exists");
            match error {
                RuntimeError::ContainmentFloorUnmet {
                    declared, reason, ..
                } => {
                    assert_eq!(declared, ContainmentClass::Sealed);
                    assert!(
                        reason.contains("pivot_root"),
                        "reason must name the missing mechanism, got: {reason}"
                    );
                }
                other => panic!("expected ContainmentFloorUnmet, got {other:?}"),
            }
        }
    }

    #[test]
    fn shortfall_reason_is_absent_when_the_floor_is_met() {
        assert_eq!(
            containment_shortfall_reason(ContainmentClass::Advisory, ContainmentClass::Scoped),
            None
        );
        assert_eq!(
            containment_shortfall_reason(ContainmentClass::Scoped, ContainmentClass::Scoped),
            None
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
        assert_eq!(
            report.interpreter_runtime_grants,
            vec!["python3: /usr/lib/python3.11 (list_dir)"]
        );
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
        );

        assert_eq!(report.declared_containment, ContainmentClass::Sealed);
        assert_eq!(report.achieved_containment, ContainmentClass::Advisory);
        assert!(!report.floor_met);
        assert!(report.shortfall_reason.unwrap().contains("pivot_root"));
        assert_eq!(report.enforcement_tier, "none");
    }

    #[test]
    fn scope_report_json_uses_wire_names() {
        let report = scope_report_for_tier(
            &CapabilityPolicy::default(),
            ContainmentClass::Advisory,
            EnforcementTier::KernelSeccompOnly,
        );
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["declared_containment"], "advisory");
        assert_eq!(value["achieved_containment"], "advisory");
        assert_eq!(value["floor_met"], true);
        assert_eq!(value["enforcement_tier"], "seccomp-only");
        // Absent rather than null when the floor is met.
        assert!(value.get("shortfall_reason").is_none());
    }

    #[test]
    fn rendered_report_names_both_classes_and_the_refusal() {
        let rendered = scope_report_for_tier(
            &sample_policy(),
            ContainmentClass::Sealed,
            EnforcementTier::KernelFull,
        )
        .render();

        assert!(rendered.contains("declared:  sealed"));
        assert!(rendered.contains("achieved:  scoped"));
        assert!(rendered.contains("floor met: no"));
        assert!(rendered.contains("pivot_root"));
        assert!(rendered.contains("would refuse to launch"));
    }
}
