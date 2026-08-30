//! The capability ceiling a spawned capsule must fit inside, and the comparison that refuses one
//! that does not.
//!
//! A delegating capsule hands its child a manifest of the child's own, and the runtime lowers that
//! manifest independently of the parent's. Without a comparison between the two, delegation is a
//! privilege-escalation ladder: a capsule granted one network host, no shell and a narrow
//! filesystem scope can spawn a child declaring unrestricted network, a full shell allow-list and
//! the whole workdir. [`SpawnEnvelope::contains`] is the comparison, and it names the axis and the
//! exact entry that exceeded.
//!
//! **Refusing, not narrowing.** This is the deliberate opposite of
//! [`crate::network_policy::ToolCapabilityGrant::derive`], which clamps a per-artifact allow-list
//! to the capsule ceiling and reports the dropped entries as `W-SEC-007`. An artifact's declared
//! capabilities are its *author's* wish list, so keeping the covered subset is right — the
//! operator did not write them. A child capsule's manifest is the operator's own declaration, so a
//! mismatch is a mistake in something they control. A refusal is auditable and fixable; a silent
//! narrowing produces a child that runs and mysteriously cannot do its job.
//!
//! **One subset predicate.** Both paths decide "is this network entry covered?" with
//! [`crate::network_policy::NetworkAllowRule::covers`], which errs toward deny on ambiguity — a
//! bare `example.com` is not covered by a ceiling of `https://example.com`, because the bare form
//! spans both schemes and every port. There is one subset rule in the workspace, not two, and the
//! shared case table in this module's tests drives both call sites and fails if their verdicts
//! ever diverge.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path};

use murmur_artifact::{ContainmentClass, RuntimeManifest};

use crate::network_policy::{validate_filesystem_scope, NetworkAllowRule};

/// Every axis two capsules are compared on, and the manifest key each one renders as in a refusal.
///
/// One variant per *declaration site*, not per lowered field: `unix_sockets` and `workdir_exec`
/// are separate axes from the allow-list and the scope they are declared beside, because each is a
/// real widening on its own. `unix_sockets: true` reaches a local daemon socket the parent cannot,
/// and `workdir_exec: true` makes `capabilities.shell.allow` unenforceable for anything inside the
/// workdir. Comparing the list while ignoring the boolean beside it would make the axis hollow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeAxis {
    NetworkAllow,
    UnixSockets,
    PeerFetchAllow,
    ShellAllow,
    SpawnAllow,
    EnvAllow,
    FilesystemScope,
    WorkdirExec,
    StateStore,
    Containment,
}

impl EnvelopeAxis {
    /// The manifest key an operator edits to fix a refusal on this axis, spelled exactly as it
    /// appears in `murmur.yaml`.
    pub fn manifest_key(self) -> &'static str {
        match self {
            Self::NetworkAllow => "capabilities.network.allow",
            Self::UnixSockets => "capabilities.network.unix_sockets",
            Self::PeerFetchAllow => "capabilities.peer_fetch.allow",
            Self::ShellAllow => "capabilities.shell.allow",
            Self::SpawnAllow => "capabilities.spawn.allow",
            Self::EnvAllow => "capabilities.env.allow",
            Self::FilesystemScope => "capabilities.filesystem.scope",
            Self::WorkdirExec => "capabilities.filesystem.workdir_exec",
            Self::StateStore => "capabilities.state.store",
            Self::Containment => "capabilities.containment",
        }
    }
}

/// The first axis a child exceeded its parent on, with the exact declaration that did it.
///
/// Carries one offending entry rather than every difference: the operator's next action is to edit
/// one line, and a comparison that stops at the first failure cannot be read as an exhaustive
/// audit of what else would have been refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeViolation {
    pub axis: EnvelopeAxis,
    /// The child declaration that exceeded, rendered as the operator wrote it: one allow-list
    /// entry, one filesystem scope, one store name, one containment class, or the literal `true`
    /// for a boolean widening.
    ///
    /// Empty on the one axis where the offending declaration is an *absence*: a child that
    /// declares no `capabilities.filesystem.scope` under a parent that declares one reaches the
    /// whole workdir, which is wider than the parent's scope.
    pub entry: String,
    /// The parent's own value, on the two axes that hold a single value rather than a list: the
    /// parent's `capabilities.filesystem.scope` and its containment floor. `None` on every list
    /// axis and on the two boolean ones, where the parent's value is implied by the refusal itself
    /// — it does not hold the entry, and the boolean is `false`.
    pub parent_entry: Option<String>,
}

impl fmt::Display for EnvelopeViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let key = self.axis.manifest_key();
        let parent = self.parent_entry.as_deref().unwrap_or_default();
        match self.axis {
            EnvelopeAxis::Containment => write!(
                f,
                "{key}: the child declares containment '{}', below its parent's floor '{parent}' \
                 — a spawned capsule's containment floor may only rise",
                self.entry,
            ),
            EnvelopeAxis::FilesystemScope if self.entry.is_empty() => write!(
                f,
                "{key}: the child declares no scope and would reach the whole workdir, which its \
                 parent's scope '{parent}' does not cover — a spawned capsule can never hold more \
                 capability than the capsule that spawned it",
            ),
            EnvelopeAxis::FilesystemScope => write!(
                f,
                "{key}: the child declares '{}', which its parent's scope '{parent}' does not \
                 cover — a spawned capsule can never hold more capability than the capsule that \
                 spawned it",
                self.entry,
            ),
            _ => write!(
                f,
                "{key}: the child declares '{}', which its parent does not hold — a spawned \
                 capsule can never hold more capability than the capsule that spawned it",
                self.entry,
            ),
        }
    }
}

/// One capsule's declared capability surface, in the shape the spawn comparison needs.
///
/// Deliberately narrower than [`crate::types::CapabilityPolicy`], and deliberately wider in one
/// place. Narrower: limits, resources and the `shell_strip_env`/`baseline_env`/`interpreter_runtime`
/// grants are bounds and plumbing rather than reach, so a child that sets them differently is not
/// escalating. Wider: [`Self::state_stores`] is per *artifact*, so it is not on the capsule-wide
/// policy at all, and a child whose artifacts open a store the parent's do not is reaching durable
/// state outside every workdir that its parent cannot see.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpawnEnvelope {
    /// `capabilities.network.allow`, unparsed. Held as declared so a refusal quotes the operator's
    /// own line; parsing happens inside [`Self::contains`], where an unparseable entry is
    /// uncovered rather than an error (`validate_capability_policy` refuses it again, with the
    /// parse error, if a spawn ever gets as far as staging).
    pub network_allow: Vec<String>,
    pub unix_sockets_allowed: bool,
    pub peer_fetch_allow: Vec<String>,
    pub shell_allow: Vec<String>,
    /// `capabilities.spawn.allow`. Compared as an axis like any other — a child that can spawn a
    /// name its parent cannot has widened the delegation graph, not just its own reach. This is a
    /// different question from the name check `mur-roost` runs first, which asks whether *this*
    /// child is a name the parent may spawn at all.
    pub spawn_allow: Vec<String>,
    pub env_allow: Vec<String>,
    pub filesystem_scope: Option<String>,
    pub workdir_exec_allowed: bool,
    /// Every store name this capsule's artifacts would open, with the capsule-name default already
    /// applied by [`crate::state_store::resolve_store_name`] — so two capsules that both declare a
    /// bare `capabilities.state` compare as the different directories they actually open.
    ///
    /// A set rather than a list: the axis is "which stores are opened", and an artifact list that
    /// names one store twice is not a wider grant.
    pub state_stores: BTreeSet<String>,
    /// `capabilities.containment`. The one axis where more is *safer*, so it is the one axis
    /// compared with `>=` rather than by containment of a set — see [`Self::contains`].
    pub containment_floor: ContainmentClass,
}

impl SpawnEnvelope {
    /// Lower a capsule's manifest into the envelope it both holds and imposes.
    ///
    /// Every capsule-wide axis is read off [`crate::types::capability_policy_from_runtime_manifest`]
    /// rather than off the manifest again, so the envelope compared here and the
    /// [`crate::types::CapabilityPolicy`] the session is actually staged with cannot read the same
    /// manifest two different ways.
    pub fn from_runtime_manifest(manifest: &RuntimeManifest) -> Self {
        let policy = crate::types::capability_policy_from_runtime_manifest(manifest);
        Self {
            network_allow: policy.network_allow,
            unix_sockets_allowed: policy.unix_sockets_allowed,
            peer_fetch_allow: policy.peer_fetch_allow,
            shell_allow: policy.shell_allow,
            spawn_allow: policy.spawn_allow,
            env_allow: policy.env_allow,
            filesystem_scope: policy.filesystem_scope,
            workdir_exec_allowed: policy.workdir_exec_allowed,
            state_stores: declared_state_stores(manifest),
            containment_floor: policy.containment_floor,
        }
    }

    /// `Ok(())` when `child` holds no more capability than `self` on any axis, otherwise the first
    /// axis it exceeded.
    ///
    /// Containment is the exception to "the child must be a subset": a floor is a requirement
    /// rather than a grant, so a child may only ever raise it. `ContainmentClass` lists its
    /// variants weakest-first and derives `Ord` for exactly this comparison.
    ///
    /// Axis order is the order a refusal reports in, and matters only when a child exceeds on more
    /// than one: it is the table order in the reference docs, so the page and the daemon agree
    /// about which refusal an operator sees first.
    pub fn contains(&self, child: &Self) -> Result<(), EnvelopeViolation> {
        if let Some(entry) = first_uncovered_host(&self.network_allow, &child.network_allow) {
            return Err(violation(EnvelopeAxis::NetworkAllow, entry));
        }
        if child.unix_sockets_allowed && !self.unix_sockets_allowed {
            return Err(violation(EnvelopeAxis::UnixSockets, "true".to_string()));
        }
        if let Some(entry) = first_uncovered_host(&self.peer_fetch_allow, &child.peer_fetch_allow) {
            return Err(violation(EnvelopeAxis::PeerFetchAllow, entry));
        }
        if let Some(entry) = first_absent(&self.shell_allow, &child.shell_allow) {
            return Err(violation(EnvelopeAxis::ShellAllow, entry));
        }
        if let Some(entry) = first_absent(&self.spawn_allow, &child.spawn_allow) {
            return Err(violation(EnvelopeAxis::SpawnAllow, entry));
        }
        if let Some(entry) = first_absent(&self.env_allow, &child.env_allow) {
            return Err(violation(EnvelopeAxis::EnvAllow, entry));
        }
        // An unset parent scope is the whole workdir and covers anything. A set one covers only a
        // child scope at or beneath it — and never a child that declares none, because that child
        // reaches the whole workdir too.
        if let Some(parent_scope) = self.filesystem_scope.as_deref() {
            let child_scope = child.filesystem_scope.as_deref().unwrap_or_default();
            if !scope_covers(parent_scope, child_scope) {
                return Err(EnvelopeViolation {
                    axis: EnvelopeAxis::FilesystemScope,
                    entry: child_scope.to_string(),
                    parent_entry: Some(parent_scope.to_string()),
                });
            }
        }
        if child.workdir_exec_allowed && !self.workdir_exec_allowed {
            return Err(violation(EnvelopeAxis::WorkdirExec, "true".to_string()));
        }
        if let Some(store) = child
            .state_stores
            .difference(&self.state_stores)
            .next()
            .cloned()
        {
            return Err(violation(EnvelopeAxis::StateStore, store));
        }
        if child.containment_floor < self.containment_floor {
            return Err(EnvelopeViolation {
                axis: EnvelopeAxis::Containment,
                entry: child.containment_floor.to_string(),
                parent_entry: Some(self.containment_floor.to_string()),
            });
        }
        Ok(())
    }
}

fn violation(axis: EnvelopeAxis, entry: String) -> EnvelopeViolation {
    EnvelopeViolation {
        axis,
        entry,
        parent_entry: None,
    }
}

/// Every store name a capsule's artifacts would open, resolved through the same helper the grant
/// lowering and `mur run --explain-scope` resolve through, so no two of them can name different
/// directories for one declaration.
///
/// The capsule's *own* `capabilities.state` is not read: a capsule-wide declaration grants no store
/// to anything (see [`crate::types::CapabilityPolicy::state_declared`]), so treating it as reach
/// would refuse spawns over a key that opens nothing.
fn declared_state_stores(manifest: &RuntimeManifest) -> BTreeSet<String> {
    manifest
        .artifacts
        .iter()
        .filter_map(|artifact| artifact.capabilities.as_ref())
        .filter_map(|capabilities| capabilities.state.as_ref())
        .map(|state| crate::state_store::resolve_store_name(state, &manifest.name))
        .collect()
}

/// The first `candidate` entry no `ceiling` entry covers, judged by
/// [`NetworkAllowRule::covers`] — the same predicate that clamps a per-artifact allow-list.
///
/// An entry neither side can parse counts as uncovered, in both directions: a malformed child entry
/// is the one named in the refusal, and a malformed parent entry grants nothing. Both fail closed,
/// and `validate_capability_policy` refuses the entry again with its parse error if a spawn reaches
/// staging.
fn first_uncovered_host(ceiling: &[String], candidate: &[String]) -> Option<String> {
    let ceiling_rules: Vec<NetworkAllowRule> = ceiling
        .iter()
        .filter_map(|entry| NetworkAllowRule::parse(entry).ok())
        .collect();
    candidate
        .iter()
        .find(|entry| match NetworkAllowRule::parse(entry) {
            Ok(rule) => !ceiling_rules
                .iter()
                .any(|ceiling_rule| ceiling_rule.covers(&rule)),
            Err(_) => true,
        })
        .cloned()
}

/// The first `candidate` name absent from `ceiling`. Plain string membership: a shell binary name,
/// a capsule name and an environment variable name are identifiers, with no covering relation
/// between two different ones.
fn first_absent(ceiling: &[String], candidate: &[String]) -> Option<String> {
    candidate
        .iter()
        .find(|entry| !ceiling.contains(entry))
        .cloned()
}

/// Whether a directory tree rooted at `parent` contains the one rooted at `child`.
///
/// Compared as resolved path components rather than as strings, so `data/../other` is judged as
/// `other` — outside a parent scoped to `data` — rather than as a string that happens to start with
/// `data`. An empty component list is the workdir root, which contains everything, and is what both
/// `.` and an undeclared scope resolve to.
///
/// A scope [`validate_filesystem_scope`] refuses is contained by nothing, including by itself: the
/// launch would refuse it anyway, and a comparison that admitted it would be deciding reach from a
/// path it cannot resolve.
fn scope_covers(parent: &str, child: &str) -> bool {
    match (scope_components(parent), scope_components(child)) {
        (Some(parent), Some(child)) => child.starts_with(&parent),
        _ => false,
    }
}

fn scope_components(scope: &str) -> Option<Vec<String>> {
    if scope.is_empty() {
        return Some(Vec::new());
    }
    validate_filesystem_scope(scope).ok()?;
    let mut components = Vec::new();
    for component in Path::new(scope).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => components.push(part.to_string_lossy().into_owned()),
            // `validate_filesystem_scope` has already established that no `..` escapes the
            // workdir, so every one of them has a component to remove.
            Component::ParentDir => {
                components.pop();
            }
            Component::Prefix(_) | Component::RootDir => return None,
        }
    }
    Some(components)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_policy::{parse_network_allow_rules, ToolCapabilityGrant};

    /// `(ceiling entry, candidate entry, the ceiling covers the candidate)`.
    ///
    /// One table, two consumers: the artifact-narrowing path and the spawn-envelope path. The
    /// asymmetric pair in the middle is the whole reason the table exists — a bare host spans both
    /// schemes and every port, so it is broader than a scheme-bound rule and is *not* covered by
    /// one, while the reverse is.
    const COVERAGE_CASES: &[(&str, &str, bool)] = &[
        ("example.com", "https://example.com", true),
        ("https://example.com", "example.com", false),
        ("example.com", "example.com", true),
        ("https://example.com", "https://example.com", true),
        ("https://example.com", "http://example.com", false),
        ("example.com:8443", "example.com:8443", true),
        ("example.com:8443", "example.com:9000", false),
        ("example.com", "other.example.com", false),
    ];

    fn envelope(capabilities_yaml: &str) -> SpawnEnvelope {
        manifest_envelope("cap", capabilities_yaml)
    }

    fn manifest_envelope(name: &str, capabilities_yaml: &str) -> SpawnEnvelope {
        let manifest = RuntimeManifest::from_yaml_str(&format!(
            "name: {name}\nversion: 0.1.0\n{capabilities_yaml}"
        ))
        .expect("manifest fixture must parse");
        SpawnEnvelope::from_runtime_manifest(&manifest)
    }

    fn network_envelope(entries: &[&str]) -> SpawnEnvelope {
        SpawnEnvelope {
            network_allow: entries.iter().map(|entry| entry.to_string()).collect(),
            ..SpawnEnvelope::default()
        }
    }

    /// The subset decision cannot drift. Replacing either call site with a second, independently
    /// written host/scheme/port comparison makes this fail — the two verdicts are asserted equal to
    /// each other as well as to the expected value, so a divergence is a failure even if the new
    /// implementation is the "right" one.
    #[test]
    fn the_narrowing_path_and_the_spawn_path_share_one_subset_rule() {
        for (ceiling, candidate, expected) in COVERAGE_CASES {
            let ceiling_rules = parse_network_allow_rules(&[ceiling.to_string()])
                .expect("ceiling fixture must parse");
            let capabilities = murmur_artifact::Capabilities {
                network: Some(murmur_artifact::NetworkCapabilities {
                    allow: vec![candidate.to_string()],
                    unix_sockets: false,
                }),
                peer_fetch: None,
                filesystem: None,
                shell: None,
                spawn: None,
                env: None,
                limits: None,
                resources: None,
                state: None,
                task_io: None,
                conversation: None,
                containment: None,
            };
            let narrowed = ToolCapabilityGrant::derive(Some(&capabilities), &ceiling_rules, "cap")
                .expect("candidate fixture must parse");
            let narrowing_verdict = narrowed.dropped_network_entries.is_empty();

            let spawn_verdict = network_envelope(&[ceiling])
                .contains(&network_envelope(&[candidate]))
                .is_ok();

            assert_eq!(
                narrowing_verdict, spawn_verdict,
                "'{ceiling}' vs '{candidate}': the narrowing path and the spawn path disagree",
            );
            assert_eq!(
                spawn_verdict, *expected,
                "'{ceiling}' vs '{candidate}': expected covered={expected}",
            );
        }
    }

    #[test]
    fn a_network_host_the_parent_does_not_hold_is_refused_by_name() {
        let parent = envelope("capabilities:\n  network:\n    allow: [registry.internal]\n");
        let child = envelope("capabilities:\n  network:\n    allow: [api.example.com]\n");

        let violation = parent.contains(&child).unwrap_err();

        assert_eq!(violation.axis, EnvelopeAxis::NetworkAllow);
        assert_eq!(violation.entry, "api.example.com");
        let message = violation.to_string();
        assert!(message.contains("capabilities.network.allow"), "{message}");
        assert!(message.contains("api.example.com"), "{message}");
        assert!(message.contains("its parent does not hold"), "{message}");
    }

    /// One case per axis, each child exceeding on exactly one of them: the axis a refusal names is
    /// the axis that was exceeded, never a neighbour.
    #[test]
    fn every_axis_refuses_naming_its_own_manifest_key_and_entry() {
        let parent_yaml = "capabilities:\n  \
             network:\n    allow: [registry.internal]\n    unix_sockets: false\n  \
             peer_fetch:\n    allow: [peer.internal]\n  \
             shell:\n    allow: [git]\n  \
             spawn:\n    allow: [worker-a]\n  \
             env:\n    allow: [HOME]\n  \
             filesystem:\n    scope: data\n    workdir_exec: false\n";
        let parent = envelope(parent_yaml);

        let cases: &[(&str, EnvelopeAxis, &str)] = &[
            (
                "capabilities:\n  network:\n    allow: [api.example.com]\n",
                EnvelopeAxis::NetworkAllow,
                "api.example.com",
            ),
            (
                "capabilities:\n  network:\n    unix_sockets: true\n",
                EnvelopeAxis::UnixSockets,
                "true",
            ),
            (
                "capabilities:\n  peer_fetch:\n    allow: [other.internal]\n",
                EnvelopeAxis::PeerFetchAllow,
                "other.internal",
            ),
            (
                "capabilities:\n  shell:\n    allow: [curl]\n",
                EnvelopeAxis::ShellAllow,
                "curl",
            ),
            (
                "capabilities:\n  spawn:\n    allow: [worker-b]\n",
                EnvelopeAxis::SpawnAllow,
                "worker-b",
            ),
            (
                "capabilities:\n  env:\n    allow: [GITHUB_TOKEN]\n",
                EnvelopeAxis::EnvAllow,
                "GITHUB_TOKEN",
            ),
            (
                "capabilities:\n  filesystem:\n    scope: other\n",
                EnvelopeAxis::FilesystemScope,
                "other",
            ),
            (
                "capabilities:\n  filesystem:\n    scope: data/../other\n",
                EnvelopeAxis::FilesystemScope,
                "data/../other",
            ),
            (
                "capabilities:\n  filesystem:\n    scope: data\n    workdir_exec: true\n",
                EnvelopeAxis::WorkdirExec,
                "true",
            ),
        ];

        for (child_yaml, axis, entry) in cases {
            let violation = parent.contains(&envelope(child_yaml)).unwrap_err();
            assert_eq!(violation.axis, *axis, "child: {child_yaml}");
            assert_eq!(violation.entry, *entry, "child: {child_yaml}");
            let message = violation.to_string();
            assert!(message.contains(axis.manifest_key()), "{message}");
            assert!(message.contains(entry), "{message}");
        }
    }

    /// A parent that scoped itself to a subtree is not widened by a child that declares nothing:
    /// an undeclared scope preopens the whole workdir.
    #[test]
    fn a_child_declaring_no_scope_is_refused_under_a_scoped_parent() {
        let parent = envelope("capabilities:\n  filesystem:\n    scope: data\n");
        let child = envelope("capabilities:\n  env:\n    allow: []\n");

        let violation = parent.contains(&child).unwrap_err();

        assert_eq!(violation.axis, EnvelopeAxis::FilesystemScope);
        assert!(violation.entry.is_empty());
        let message = violation.to_string();
        assert!(
            message.contains("capabilities.filesystem.scope"),
            "{message}"
        );
        assert!(message.contains("declares no scope"), "{message}");
        assert!(message.contains("'data'"), "{message}");
    }

    #[test]
    fn a_scope_at_or_beneath_the_parents_is_within_the_envelope() {
        let parent = envelope("capabilities:\n  filesystem:\n    scope: data\n");

        for scope in ["data", "data/in", "data/in/../in"] {
            let child = envelope(&format!(
                "capabilities:\n  filesystem:\n    scope: {scope}\n"
            ));
            assert!(parent.contains(&child).is_ok(), "scope '{scope}'");
        }
    }

    /// A parent that declares no scope holds the whole workdir, so every child scope is beneath it.
    #[test]
    fn an_unscoped_parent_covers_any_child_scope() {
        let parent = envelope("capabilities:\n  env:\n    allow: []\n");
        let child = envelope("capabilities:\n  filesystem:\n    scope: data\n");

        assert!(parent.contains(&child).is_ok());
    }

    /// A store is granted per artifact and defaults to the declaring capsule's own name, so two
    /// capsules that both declare a bare `capabilities.state` open two different directories.
    #[test]
    fn a_state_store_the_parents_artifacts_do_not_open_is_refused() {
        let with_state = |name: &str, store: &str| {
            manifest_envelope(
                name,
                &format!(
                    "artifacts:\n  - name: writer\n    version: 0.1.0\n    runtime: tool\n    \
                     capabilities:\n      state:\n        store: {store}\n"
                ),
            )
        };
        let parent = with_state("parent", "parent-notes");
        let child = with_state("child", "child-notes");

        let violation = parent.contains(&child).unwrap_err();

        assert_eq!(violation.axis, EnvelopeAxis::StateStore);
        assert_eq!(violation.entry, "child-notes");
        assert!(violation.to_string().contains("capabilities.state.store"));

        let shared = with_state("child", "parent-notes");
        assert!(parent.contains(&shared).is_ok());
    }

    /// An undeclared `store:` resolves to the declaring capsule's name, which is what makes two
    /// bare declarations compare as the different directories they open.
    #[test]
    fn an_undeclared_store_resolves_to_the_capsule_name() {
        let envelope = manifest_envelope(
            "notes-capsule",
            "artifacts:\n  - name: writer\n    version: 0.1.0\n    runtime: tool\n    \
             capabilities:\n      state: {}\n",
        );

        assert_eq!(
            envelope.state_stores.iter().collect::<Vec<_>>(),
            vec!["notes-capsule"]
        );
    }

    /// The one axis where more is safer. A floor may rise, and only a floor that *falls* is an
    /// escalation.
    #[test]
    fn a_containment_floor_may_rise_but_never_fall() {
        let parent = envelope("capabilities:\n  containment: scoped\n");

        for raised in ["scoped", "sealed"] {
            let child = envelope(&format!("capabilities:\n  containment: {raised}\n"));
            assert!(parent.contains(&child).is_ok(), "floor '{raised}'");
        }

        let child = envelope("capabilities:\n  containment: advisory\n");
        let violation = parent.contains(&child).unwrap_err();

        assert_eq!(violation.axis, EnvelopeAxis::Containment);
        assert_eq!(violation.entry, "advisory");
        let message = violation.to_string();
        assert!(message.contains("capabilities.containment"), "{message}");
        assert!(message.contains("advisory"), "{message}");
        assert!(message.contains("may only rise"), "{message}");
    }

    /// The operator-facing text, pinned in full. A refusal is read by someone who has to find one
    /// line in one manifest, so the key, the offending declaration and the rule are all in it.
    #[test]
    fn a_refusal_names_the_key_the_declaration_and_the_rule() {
        let list_axis = envelope("capabilities:\n  network:\n    allow: [registry.internal]\n")
            .contains(&envelope(
                "capabilities:\n  network:\n    allow: [api.example.com]\n",
            ))
            .unwrap_err();
        assert_eq!(
            list_axis.to_string(),
            "capabilities.network.allow: the child declares 'api.example.com', which its parent \
             does not hold — a spawned capsule can never hold more capability than the capsule \
             that spawned it",
        );

        let absent_scope = envelope("capabilities:\n  filesystem:\n    scope: data\n")
            .contains(&envelope("artifacts: []\n"))
            .unwrap_err();
        assert_eq!(
            absent_scope.to_string(),
            "capabilities.filesystem.scope: the child declares no scope and would reach the whole \
             workdir, which its parent's scope 'data' does not cover — a spawned capsule can never \
             hold more capability than the capsule that spawned it",
        );

        let floor = envelope("capabilities:\n  containment: scoped\n")
            .contains(&envelope("capabilities:\n  containment: advisory\n"))
            .unwrap_err();
        assert_eq!(
            floor.to_string(),
            "capabilities.containment: the child declares containment 'advisory', below its \
             parent's floor 'scoped' — a spawned capsule's containment floor may only rise",
        );
    }

    /// A capsule that declares nothing holds nothing, so it is inside any parent — including one
    /// that also declares nothing.
    #[test]
    fn a_silent_child_is_within_any_parent() {
        let parent = envelope(
            "capabilities:\n  network:\n    allow: [registry.internal]\n  \
             shell:\n    allow: [git]\n",
        );

        assert!(parent.contains(&envelope("artifacts: []\n")).is_ok());
        assert!(envelope("artifacts: []\n")
            .contains(&envelope("artifacts: []\n"))
            .is_ok());
    }
}
