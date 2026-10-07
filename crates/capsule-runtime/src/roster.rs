//! Roster admission: a parsed `roster.yaml` checked against the stores and the project's
//! `murmur.lock`, and either bound to exact artifacts with its reachability expanded into edges, or
//! refused whole.
//!
//! Admission launches nothing, writes nothing and contacts no daemon. Its inputs are the roster, a
//! [`Registry`] and the lock already read by the caller; the only reads it makes are the registry
//! lookups.
//!
//! The checks run in a fixed order and the first refusal is returned:
//!
//! 1. member names are unique;
//! 2. exactly one member is the entry member;
//! 3. every rule's `from` and `to` names a member;
//! 4. no rule calls the entry member;
//! 5. each member resolves, its packed manifest parses, and `murmur.lock` agrees with it;
//! 6. every member an explicit rule calls serves peers;
//! 7. when any edge exists, every member's door requires authentication.
//!
//! Checks 1–4 read no store, so a structurally broken roster is refused without a lookup. Within a
//! check, members are taken in roster order.
//!
//! An admitted roster can be asked for [`AdmittedRoster::caller_overflows`]: the members more
//! members may call than their `lifecycle` holds at once, which `mur` prints as `W-ROS-001`
//! without refusing anything.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use murmur_artifact::{
    current_platform, load_roster, resolve_roster_path, roster_warning_link, sha256_hex,
    LockedSha256, MurmurLock, Registry, RegistryError, Roster, RosterReachability, RuntimeManifest,
    TaskAcceptance, ROSTER_FILENAME, W_ROS_001,
};
use thiserror::Error;

use crate::artifact::extract_manifest_yaml;

/// A roster that passed every admission check.
#[derive(Debug, Clone)]
pub struct AdmittedRoster {
    source: Option<PathBuf>,
    members: Vec<AdmittedMember>,
    entry: usize,
    reachability: RosterReachability,
    edges: Vec<RosterEdge>,
}

/// One member, bound to the artifact admission resolved for it.
#[derive(Debug, Clone)]
pub struct AdmittedMember {
    /// The member's name within the roster.
    pub name: String,
    /// The artifact name the member runs.
    pub capsule: String,
    /// The exact version `roster.yaml` pins.
    pub version: String,
    /// Whether this is the roster's entry member.
    pub entry: bool,
    /// SHA-256 of the artifact bytes admission resolved and checked, as 64 lowercase hex
    /// characters. Whatever starts this member must start these bytes and no others.
    pub sha256: String,
    /// The member's packed `murmur.yaml`, parsed.
    pub manifest: RuntimeManifest,
}

impl AdmittedMember {
    /// Whether the member's door requires authentication: its manifest declares
    /// `network.authentication`.
    #[must_use]
    pub fn requires_authentication(&self) -> bool {
        self.manifest
            .network
            .as_ref()
            .is_some_and(|network| network.authentication.is_some())
    }
}

/// `from` may call `to`. Both are member names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEdge {
    pub from: String,
    pub to: String,
}

/// A member the roster lets more members call than it holds at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerOverflow {
    pub member: String,
    pub capsule: String,
    pub version: String,
    /// Every member with an edge into `member`, in roster order. Never empty.
    pub callers: Vec<String>,
    pub task_acceptance: TaskAcceptance,
    pub queue_depth: usize,
    /// [`murmur_artifact::LifecycleConfig::tasks_held_at_once`] of the member's effective
    /// lifecycle. Always less than `callers.len()`.
    pub holds: usize,
    /// The `queue_depth` that, under `task_acceptance: queue`, holds every caller at once:
    /// `max(callers.len() - 1, 1)`.
    pub sufficient_queue_depth: usize,
}

/// Why a roster was not admitted. One refusal covers the whole roster: no member of a refused
/// roster is admitted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RosterRefusal {
    /// `roster.yaml` is missing, unreadable or fails a shape rule. The message names the file and
    /// the key path.
    #[error("{message}")]
    Malformed { message: String },
    #[error("{ROSTER_FILENAME}: member name '{member}' is declared more than once")]
    DuplicateMember { member: String },
    #[error(
        "{ROSTER_FILENAME}: no member is marked `entry: true`; a roster designates exactly one \
         entry member"
    )]
    EntryNotDesignated,
    #[error(
        "{ROSTER_FILENAME}: members {} are each marked `entry: true`; a roster designates exactly \
         one entry member",
        quoted_list(members)
    )]
    EntryDesignatedMoreThanOnce { members: Vec<String> },
    /// `from` is the rule's own `from`; `unknown` is the name that matches no member, which is
    /// `from` itself when the rule's `from` is the unknown one.
    #[error(
        "{ROSTER_FILENAME}: the reachability rule from '{from}' names '{unknown}', which is not a \
         member of this roster"
    )]
    UnknownMemberInRule { from: String, unknown: String },
    /// `from` is the offending rule's own `from`; `entry` is the entry member's name.
    #[error(
        "{ROSTER_FILENAME}: the reachability rule from '{from}' lists the entry member '{entry}' \
         in `to`, but the entry member is never called: it runs the formation's own task from \
         launch until the formation ends"
    )]
    RuleCallsEntryMember { from: String, entry: String },
    /// `coordinate` is `capsule@version`.
    #[error("{ROSTER_FILENAME}: member '{member}' ({coordinate}) cannot be admitted: {reason}")]
    MemberUnresolvable {
        member: String,
        coordinate: String,
        reason: String,
    },
    /// `conflict` is [`murmur_artifact::LockedArtifact::conflict_with`]'s own text.
    #[error(
        "{ROSTER_FILENAME}: member '{member}': murmur.lock conflict for '{capsule}': {conflict}"
    )]
    MemberLockConflict {
        member: String,
        capsule: String,
        conflict: String,
    },
    #[error(
        "{ROSTER_FILENAME}: member '{member}' is listed in a reachability rule's `to`, but its \
         manifest does not declare `exports.peer_tasks.accept: true`"
    )]
    CalleeDoesNotServePeers { member: String },
    #[error(
        "{ROSTER_FILENAME}: member '{member}' declares no `network.authentication`, and this \
         roster declares peer traffic, so every member's door must require authentication"
    )]
    MemberNotAuthenticated { member: String },
}

fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl AdmittedRoster {
    /// The `roster.yaml` this was admitted from. `None` for a roster admitted through
    /// [`admit_roster`] from a value rather than a file.
    #[must_use]
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Every member, in roster order.
    #[must_use]
    pub fn members(&self) -> &[AdmittedMember] {
        &self.members
    }

    #[must_use]
    pub fn member(&self, name: &str) -> Option<&AdmittedMember> {
        self.members.iter().find(|member| member.name == name)
    }

    /// The member that receives the formation's task. No edge ever leads into it: admission
    /// refuses a rule that lists it in `to`, and `all` never pairs it as a callee. Its outgoing
    /// edges come from rules, or from `all` when it serves peers.
    #[must_use]
    pub fn entry(&self) -> &AdmittedMember {
        &self.members[self.entry]
    }

    /// The `reachability` the roster declared, before expansion.
    #[must_use]
    pub fn reachability(&self) -> &RosterReachability {
        &self.reachability
    }

    /// Every edge, without repeats, ordered by the roster index of `from` and then of `to`. No
    /// edge's `to` is the entry member.
    #[must_use]
    pub fn edges(&self) -> &[RosterEdge] {
        &self.edges
    }

    /// Whether `from` may call `to`. `false` for a name that is not a member.
    #[must_use]
    pub fn may_call(&self, from: &str, to: &str) -> bool {
        self.edges
            .iter()
            .any(|edge| edge.from == from && edge.to == to)
    }

    /// The members `from` may call, in roster order. The entry member is never among them.
    pub fn callees<'a>(&'a self, from: &'a str) -> impl Iterator<Item = &'a AdmittedMember> + 'a {
        self.edges
            .iter()
            .filter(move |edge| edge.from == from)
            .filter_map(|edge| self.member(&edge.to))
    }

    /// Whether any member may call any other: the expanded edge set is non-empty.
    #[must_use]
    pub fn declares_peer_traffic(&self) -> bool {
        !self.edges.is_empty()
    }

    /// Every member whose callers outnumber the tasks it holds at once, in roster order.
    ///
    /// Callers are the `from` of each edge into the member, so `reachability: all` counts as
    /// expanded, and each calling member counts once: a member holds at most one unanswered call
    /// to a given member. A member nothing calls never overflows, the entry member included.
    #[must_use]
    pub fn caller_overflows(&self) -> Vec<CallerOverflow> {
        self.members
            .iter()
            .filter_map(|member| {
                let callers: Vec<String> = self
                    .edges
                    .iter()
                    .filter(|edge| edge.to == member.name)
                    .map(|edge| edge.from.clone())
                    .collect();
                let lifecycle = member.manifest.effective_lifecycle();
                let holds = lifecycle.tasks_held_at_once();
                (callers.len() > holds).then(|| CallerOverflow {
                    member: member.name.clone(),
                    capsule: member.capsule.clone(),
                    version: member.version.clone(),
                    sufficient_queue_depth: callers.len().saturating_sub(1).max(1),
                    callers,
                    task_acceptance: lifecycle.task_acceptance,
                    queue_depth: lifecycle.queue_depth,
                    holds,
                })
            })
            .collect()
    }
}

/// The whole `warning[W-ROS-001]: …` line for `overflow`, link included. `mur run --roster` and
/// `mur doctor` both print this string, so the two read the same.
#[must_use]
pub fn caller_overflow_warning(overflow: &CallerOverflow) -> String {
    let count = overflow.callers.len();
    let noun = if count == 1 { "member" } else { "members" };
    let depth = overflow.sufficient_queue_depth;
    let (holding, rejection, remedy) = match overflow.task_acceptance {
        TaskAcceptance::Queue if overflow.queue_depth > 0 => (
            format!(
                "{} tasks at once — one running and lifecycle.queue_depth: {} waiting —",
                overflow.holds, overflow.queue_depth
            ),
            "a call that arrives while it is full is rejected",
            format!("lifecycle.queue_depth: {depth}"),
        ),
        TaskAcceptance::Queue => (
            "no task — lifecycle.queue_depth: 0 —".to_string(),
            "every call is rejected",
            format!("lifecycle.queue_depth: {depth}"),
        ),
        TaskAcceptance::Single => (
            "1 task at once — lifecycle.task_acceptance: single —".to_string(),
            "a call that arrives while it is busy is rejected",
            format!("lifecycle.task_acceptance: queue with lifecycle.queue_depth: {depth}"),
        ),
        TaskAcceptance::None => (
            "no task — lifecycle.task_acceptance: none —".to_string(),
            "every call is rejected",
            format!("lifecycle.task_acceptance: queue with lifecycle.queue_depth: {depth}"),
        ),
    };
    format!(
        "warning[{W_ROS_001}]: {ROSTER_FILENAME} lets {count} {noun} call '{}' ({}), but it holds \
         {holding} so {rejection}; {remedy} would hold them all ({})",
        overflow.member,
        overflow.callers.join(", "),
        roster_warning_link(W_ROS_001),
    )
}

/// The one-line remedy `mur doctor` prints under `Fix:` for `overflow`.
#[must_use]
pub fn caller_overflow_fix(overflow: &CallerOverflow) -> String {
    let depth = overflow.sufficient_queue_depth;
    let setting = match overflow.task_acceptance {
        TaskAcceptance::Queue => format!("lifecycle.queue_depth: {depth}"),
        TaskAcceptance::Single | TaskAcceptance::None => {
            format!("lifecycle.task_acceptance: queue and lifecycle.queue_depth: {depth}")
        }
    };
    let held = match overflow.callers.len() {
        1 => "the 1 member that may call it is held".to_string(),
        count => format!("all {count} members that may call it are held"),
    };
    format!(
        "{} ({}@{}): set {setting} in its murmur.yaml so {held} at once, or narrow its callers \
         in {ROSTER_FILENAME} (warning[{W_ROS_001}])",
        overflow.member, overflow.capsule, overflow.version,
    )
}

/// Load `roster.yaml` from `project_dir` and admit it.
///
/// Every [`murmur_artifact::RosterError`], a missing file included, is
/// [`RosterRefusal::Malformed`]; a caller that treats a missing roster as "no formation" checks
/// [`resolve_roster_path`] first. `lock` is the project's `murmur.lock`, already read; admission
/// never reads, creates or rewrites it.
pub fn admit_roster_file(
    project_dir: &Path,
    registry: &dyn Registry,
    lock: Option<&MurmurLock>,
) -> Result<AdmittedRoster, RosterRefusal> {
    let path = resolve_roster_path(project_dir);
    let roster = load_roster(&path).map_err(|error| RosterRefusal::Malformed {
        message: error.to_string(),
    })?;
    let mut admitted = admit_roster(&roster, registry, lock)?;
    admitted.source = Some(path);
    Ok(admitted)
}

/// Admit `roster`: run every check in the module's order and return the first refusal, or bind
/// each member to the artifact `registry` resolves for this host's platform.
pub fn admit_roster(
    roster: &Roster,
    registry: &dyn Registry,
    lock: Option<&MurmurLock>,
) -> Result<AdmittedRoster, RosterRefusal> {
    let mut seen = HashSet::new();
    for member in &roster.members {
        if !seen.insert(member.name.as_str()) {
            return Err(RosterRefusal::DuplicateMember {
                member: member.name.clone(),
            });
        }
    }

    let claimants: Vec<usize> = roster
        .members
        .iter()
        .enumerate()
        .filter(|(_, member)| member.entry)
        .map(|(index, _)| index)
        .collect();
    let entry = match claimants.as_slice() {
        [] => return Err(RosterRefusal::EntryNotDesignated),
        [only] => *only,
        several => {
            return Err(RosterRefusal::EntryDesignatedMoreThanOnce {
                members: several
                    .iter()
                    .map(|index| roster.members[*index].name.clone())
                    .collect(),
            })
        }
    };

    let index_of: HashMap<&str, usize> = roster
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| (member.name.as_str(), index))
        .collect();
    if let RosterReachability::Rules(rules) = &roster.reachability {
        for rule in rules {
            for name in std::iter::once(&rule.from).chain(&rule.to) {
                if !index_of.contains_key(name.as_str()) {
                    return Err(RosterRefusal::UnknownMemberInRule {
                        from: rule.from.clone(),
                        unknown: name.clone(),
                    });
                }
            }
        }
        let entry_name = &roster.members[entry].name;
        if let Some(rule) = rules.iter().find(|rule| rule.to.contains(entry_name)) {
            return Err(RosterRefusal::RuleCallsEntryMember {
                from: rule.from.clone(),
                entry: entry_name.clone(),
            });
        }
    }

    let platform = current_platform();
    let mut members = Vec::with_capacity(roster.members.len());
    for member in &roster.members {
        let coordinate = format!("{}@{}", member.capsule, member.version);
        let unresolvable = |reason: String| RosterRefusal::MemberUnresolvable {
            member: member.name.clone(),
            coordinate: coordinate.clone(),
            reason,
        };

        let resolved = registry
            .resolve_with_platform(&member.capsule, &member.version, Some(platform))
            .map_err(|error| {
                unresolvable(match error {
                    RegistryError::NotFound { .. } => {
                        "not installed in the project or global store".to_string()
                    }
                    other => other.to_string(),
                })
            })?;
        // The store's `.sha256` sidecar is what the lock compares against, so the bytes have to
        // be the ones it describes before either is trusted.
        let actual = sha256_hex(&resolved.bytes);
        if actual != resolved.sha256 {
            return Err(unresolvable(format!(
                "the stored bytes hash to {actual}, but the store records {}",
                resolved.sha256
            )));
        }
        let manifest_yaml =
            extract_manifest_yaml(&member.capsule, &member.version, &resolved.bytes)
                .map_err(|error| unresolvable(error.to_string()))?;
        let manifest = RuntimeManifest::from_yaml_str(&manifest_yaml).map_err(|error| {
            unresolvable(format!("its packed murmur.yaml does not parse: {error}"))
        })?;

        if let Some(pinned) = lock.and_then(|lock| lock.artifact_for(&member.capsule)) {
            let incoming = LockedSha256::for_resolved(&resolved, platform);
            if let Some(conflict) = pinned.conflict_with(&member.version, &incoming) {
                return Err(RosterRefusal::MemberLockConflict {
                    member: member.name.clone(),
                    capsule: member.capsule.clone(),
                    conflict,
                });
            }
        }

        members.push(AdmittedMember {
            name: member.name.clone(),
            capsule: member.capsule.clone(),
            version: member.version.clone(),
            entry: member.entry,
            sha256: resolved.sha256,
            manifest,
        });
    }

    let pairs: BTreeSet<(usize, usize)> = match &roster.reachability {
        RosterReachability::Closed => BTreeSet::new(),
        // The entry member calls under `all` when it serves peers, but is never called.
        RosterReachability::All => {
            let serving: Vec<usize> = members
                .iter()
                .enumerate()
                .filter(|(_, member)| member.manifest.accepts_peer_tasks())
                .map(|(index, _)| index)
                .collect();
            serving
                .iter()
                .flat_map(|from| {
                    serving
                        .iter()
                        .filter(move |to| *to != from && **to != entry)
                        .map(move |to| (*from, *to))
                })
                .collect()
        }
        RosterReachability::Rules(rules) => {
            let index_of = &index_of;
            let pairs: BTreeSet<(usize, usize)> = rules
                .iter()
                .flat_map(|rule| {
                    let from = index_of[rule.from.as_str()];
                    rule.to.iter().map(move |to| (from, index_of[to.as_str()]))
                })
                .collect();
            let callees: BTreeSet<usize> = pairs.iter().map(|(_, to)| *to).collect();
            if let Some(refused) = callees
                .into_iter()
                .find(|to| !members[*to].manifest.accepts_peer_tasks())
            {
                return Err(RosterRefusal::CalleeDoesNotServePeers {
                    member: members[refused].name.clone(),
                });
            }
            pairs
        }
    };

    if !pairs.is_empty() {
        if let Some(public) = members
            .iter()
            .find(|member| !member.requires_authentication())
        {
            return Err(RosterRefusal::MemberNotAuthenticated {
                member: public.name.clone(),
            });
        }
    }

    let edges = pairs
        .into_iter()
        .map(|(from, to)| RosterEdge {
            from: members[from].name.clone(),
            to: members[to].name.clone(),
        })
        .collect();

    Ok(AdmittedRoster {
        source: None,
        members,
        entry,
        reachability: roster.reachability.clone(),
        edges,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    use murmur_artifact::{
        ArtifactMeta, LocalRegistry, LockOrigin, LockedArtifact, ReachabilityRule, RosterMember,
        RuntimeType, LOCK_VERSION,
    };

    use super::*;

    const SERVES: &str = "exports:\n  peer_tasks:\n    accept: true\n";
    const REFUSES: &str = "exports:\n  peer_tasks:\n    accept: false\n";
    const AUTHENTICATED: &str = "network:\n  authentication:\n    scheme: bearer\n";

    fn manifest(name: &str, version: &str, serves: bool, authenticated: bool) -> String {
        let mut yaml = format!("name: {name}\nversion: {version}\nartifacts: []\n");
        if serves {
            yaml.push_str(SERVES);
        }
        if authenticated {
            yaml.push_str(AUTHENTICATED);
        }
        yaml
    }

    fn pack(manifest_yaml: &str) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            zip.start_file("murmur.yaml", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(manifest_yaml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// Install a capsule whose packed `murmur.yaml` is `manifest_yaml`; returns its sha256.
    fn install(store: &LocalRegistry, name: &str, version: &str, manifest_yaml: &str) -> String {
        let bytes = pack(manifest_yaml);
        let sha256 = sha256_hex(&bytes);
        store
            .store_installed_overwrite(
                ArtifactMeta {
                    name: name.to_string(),
                    version: version.to_string(),
                    runtime: RuntimeType::Wasm,
                    artifact_runtime: "capsule".to_string(),
                    platforms: Vec::new(),
                    description: None,
                    tags: Vec::new(),
                    wit_contracts: None,
                },
                &bytes,
                &sha256,
            )
            .unwrap();
        sha256
    }

    fn member(name: &str, version: &str, entry: bool) -> RosterMember {
        RosterMember {
            name: name.to_string(),
            capsule: name.to_string(),
            version: version.to_string(),
            entry,
        }
    }

    fn rule(from: &str, to: &[&str]) -> ReachabilityRule {
        ReachabilityRule {
            from: from.to_string(),
            to: to.iter().map(ToString::to_string).collect(),
        }
    }

    fn edges(admitted: &AdmittedRoster) -> Vec<(&str, &str)> {
        admitted
            .edges()
            .iter()
            .map(|edge| (edge.from.as_str(), edge.to.as_str()))
            .collect()
    }

    fn lock_pinning(name: &str, version: &str, sha256: &str) -> MurmurLock {
        MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: vec![LockedArtifact {
                name: name.to_string(),
                resolved_version: version.to_string(),
                sha256: LockedSha256::any(sha256.to_string()),
                origin: LockOrigin::Operator,
            }],
        }
    }

    /// S1's store and roster: three members, each serving peers behind an authenticated door.
    struct Formation {
        _dir: tempfile::TempDir,
        store: LocalRegistry,
        hashes: BTreeMap<&'static str, String>,
        roster: Roster,
    }

    fn formation() -> Formation {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalRegistry::new(dir.path().join("artifacts"));
        let mut hashes = BTreeMap::new();
        for (name, version) in [
            ("planner", "0.3.0"),
            ("coder", "1.2.0"),
            ("reviewer", "0.9.0"),
        ] {
            hashes.insert(
                name,
                install(&store, name, version, &manifest(name, version, true, true)),
            );
        }
        let roster = Roster {
            members: vec![
                member("planner", "0.3.0", true),
                member("coder", "1.2.0", false),
                member("reviewer", "0.9.0", false),
            ],
            reachability: RosterReachability::Rules(vec![
                rule("planner", &["coder", "reviewer"]),
                rule("reviewer", &["coder"]),
            ]),
        };
        Formation {
            _dir: dir,
            store,
            hashes,
            roster,
        }
    }

    fn refusal(roster: &Roster, store: &LocalRegistry, lock: Option<&MurmurLock>) -> RosterRefusal {
        admit_roster(roster, store, lock).expect_err("the roster should be refused")
    }

    #[test]
    fn s1_admits_members_in_order_bound_to_their_hashes_with_explicit_edges() {
        let f = formation();
        let admitted = admit_roster(&f.roster, &f.store, None).unwrap();
        let names: Vec<_> = admitted.members().iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["planner", "coder", "reviewer"]);
        for member in admitted.members() {
            assert_eq!(member.sha256, f.hashes[member.name.as_str()]);
            assert_eq!(member.manifest.name, member.capsule);
            assert!(member.manifest.accepts_peer_tasks());
        }
        assert_eq!(admitted.entry().name, "planner");
        assert_eq!(
            edges(&admitted),
            [
                ("planner", "coder"),
                ("planner", "reviewer"),
                ("reviewer", "coder")
            ]
        );
        assert!(admitted.may_call("planner", "coder"));
        assert!(!admitted.may_call("coder", "planner"));
        assert!(admitted.declares_peer_traffic());
        let callees: Vec<_> = admitted
            .callees("planner")
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(callees, ["coder", "reviewer"]);
        assert_eq!(admitted.source(), None);
    }

    #[test]
    fn repeated_edges_collapse_and_edges_sort_by_roster_index() {
        let mut f = formation();
        f.roster.reachability = RosterReachability::Rules(vec![
            rule("reviewer", &["coder", "coder"]),
            rule("planner", &["reviewer", "coder"]),
            rule("planner", &["coder"]),
        ]);
        let admitted = admit_roster(&f.roster, &f.store, None).unwrap();
        assert_eq!(
            edges(&admitted),
            [
                ("planner", "coder"),
                ("planner", "reviewer"),
                ("reviewer", "coder")
            ]
        );
    }

    #[test]
    fn s2_a_single_public_member_declares_no_traffic() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalRegistry::new(dir.path());
        install(
            &store,
            "solo",
            "1.0.0",
            &manifest("solo", "1.0.0", false, false),
        );
        let roster = Roster {
            members: vec![member("solo", "1.0.0", true)],
            reachability: RosterReachability::Closed,
        };
        let admitted = admit_roster(&roster, &store, None).unwrap();
        assert!(admitted.edges().is_empty());
        assert!(!admitted.declares_peer_traffic());
        assert_eq!(admitted.entry().name, "solo");
    }

    #[test]
    fn s3_all_pairs_only_members_that_both_serve_peers() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalRegistry::new(dir.path());
        for (name, serves) in [("a", true), ("b", true), ("c", false), ("d", false)] {
            install(
                &store,
                name,
                "1.0.0",
                &manifest(name, "1.0.0", serves, true),
            );
        }
        let roster = Roster {
            members: ["a", "b", "c", "d"]
                .iter()
                .map(|name| member(name, "1.0.0", *name == "a"))
                .collect(),
            reachability: RosterReachability::All,
        };
        let admitted = admit_roster(&roster, &store, None).unwrap();
        // `a` is the entry member: it calls `b`, and nothing calls it.
        assert_eq!(edges(&admitted), [("a", "b")]);
        assert!(matches!(admitted.reachability(), RosterReachability::All));

        // `a` stops serving: `all` expands to nothing, and nothing then needs authentication.
        for (name, serves) in [("a", false), ("b", true), ("c", false), ("d", false)] {
            install(
                &store,
                name,
                "1.0.0",
                &manifest(name, "1.0.0", serves, false),
            );
        }
        let admitted = admit_roster(&roster, &store, None).unwrap();
        assert!(admitted.edges().is_empty());
    }

    #[test]
    fn s4_entry_must_be_designated_exactly_once() {
        let mut f = formation();
        f.roster.members[0].entry = false;
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::EntryNotDesignated
        );

        f.roster.members[0].entry = true;
        f.roster.members[1].entry = true;
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::EntryDesignatedMoreThanOnce {
                members: vec!["planner".to_string(), "coder".to_string()]
            }
        );
    }

    #[test]
    fn s5_a_duplicate_name_is_refused_before_entry_and_before_any_lookup() {
        let empty = tempfile::tempdir().unwrap();
        let empty_store = LocalRegistry::new(empty.path());
        let f = formation();
        let roster = Roster {
            members: vec![
                member("coder", "1.2.0", true),
                member("coder", "1.2.0", true),
            ],
            reachability: RosterReachability::Closed,
        };
        for store in [&f.store, &empty_store] {
            assert_eq!(
                refusal(&roster, store, None),
                RosterRefusal::DuplicateMember {
                    member: "coder".to_string()
                }
            );
        }
    }

    #[test]
    fn s6_a_rule_naming_no_member_is_refused_naming_it() {
        let mut f = formation();
        f.roster.reachability =
            RosterReachability::Rules(vec![rule("planner", &["coder", "tester"])]);
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::UnknownMemberInRule {
                from: "planner".to_string(),
                unknown: "tester".to_string()
            }
        );
        f.roster.reachability = RosterReachability::Rules(vec![rule("ghost", &["coder"])]);
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::UnknownMemberInRule {
                from: "ghost".to_string(),
                unknown: "ghost".to_string()
            }
        );
    }

    #[test]
    fn a_rule_calling_the_entry_member_is_refused_naming_both() {
        let mut f = formation();
        f.roster.reachability = RosterReachability::Rules(vec![
            rule("planner", &["coder", "reviewer"]),
            rule("reviewer", &["coder", "planner"]),
        ]);
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::RuleCallsEntryMember {
                from: "reviewer".to_string(),
                entry: "planner".to_string()
            }
        );
    }

    #[test]
    fn the_first_rule_calling_the_entry_member_in_file_order_is_named() {
        let mut f = formation();
        f.roster.reachability = RosterReachability::Rules(vec![
            rule("planner", &["coder"]),
            rule("coder", &["reviewer", "planner"]),
            rule("reviewer", &["planner"]),
        ]);
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::RuleCallsEntryMember {
                from: "coder".to_string(),
                entry: "planner".to_string()
            }
        );
    }

    #[test]
    fn a_rule_calling_the_entry_member_is_refused_before_any_lookup() {
        let empty = tempfile::tempdir().unwrap();
        let empty_store = LocalRegistry::new(empty.path());
        let mut f = formation();
        f.roster.reachability =
            RosterReachability::Rules(vec![rule("reviewer", &["coder", "planner"])]);
        assert_eq!(
            refusal(&f.roster, &empty_store, None),
            RosterRefusal::RuleCallsEntryMember {
                from: "reviewer".to_string(),
                entry: "planner".to_string()
            }
        );

        // An entry member that does not serve peers is still refused as the entry member, not as
        // a callee that does not serve.
        install(
            &f.store,
            "planner",
            "0.3.0",
            &manifest("planner", "0.3.0", false, true),
        );
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::RuleCallsEntryMember {
                from: "reviewer".to_string(),
                entry: "planner".to_string()
            }
        );
    }

    #[test]
    fn an_unknown_rule_name_is_refused_before_a_rule_calling_the_entry_member() {
        let mut f = formation();
        for rules in [
            vec![rule("ghost", &["coder"]), rule("reviewer", &["planner"])],
            vec![rule("reviewer", &["planner"]), rule("coder", &["ghost"])],
        ] {
            f.roster.reachability = RosterReachability::Rules(rules);
            assert!(matches!(
                refusal(&f.roster, &f.store, None),
                RosterRefusal::UnknownMemberInRule { unknown, .. } if unknown == "ghost"
            ));
        }
    }

    #[test]
    fn no_admitted_edge_ever_leads_into_the_entry_member() {
        let mut f = formation();
        let reachabilities = [
            RosterReachability::All,
            f.roster.reachability.clone(),
            RosterReachability::Rules(vec![
                rule("reviewer", &["coder", "coder"]),
                rule("planner", &["reviewer", "coder"]),
                rule("planner", &["coder"]),
            ]),
            RosterReachability::Rules(vec![
                rule("planner", &["coder"]),
                rule("reviewer", &["coder"]),
            ]),
        ];
        for reachability in reachabilities {
            f.roster.reachability = reachability;
            let admitted = admit_roster(&f.roster, &f.store, None).unwrap();
            let entry = admitted.entry().name.clone();
            assert!(admitted.declares_peer_traffic());
            assert!(
                admitted.edges().iter().all(|edge| edge.to != entry),
                "{:?}",
                admitted.edges()
            );
            for member in admitted.members() {
                assert!(admitted.callees(&member.name).all(|callee| !callee.entry));
            }
        }

        // Under `all`, whichever member is the entry member calls every other and is never called.
        f.roster.reachability = RosterReachability::All;
        for entry in 0..f.roster.members.len() {
            for (index, member) in f.roster.members.iter_mut().enumerate() {
                member.entry = index == entry;
            }
            let admitted = admit_roster(&f.roster, &f.store, None).unwrap();
            let entry = admitted.entry().name.as_str();
            let pairs = edges(&admitted);
            assert_eq!(pairs.len(), 4, "{pairs:?}");
            assert!(pairs.iter().all(|(_, to)| *to != entry), "{pairs:?}");
            assert_eq!(pairs.iter().filter(|(from, _)| *from == entry).count(), 2);
        }
    }

    #[test]
    fn s7_a_callee_must_serve_peers_but_a_caller_need_not() {
        for reviewer_manifest in [
            "name: reviewer\nversion: 0.9.0\nartifacts: []\nnetwork:\n  authentication:\n    scheme: bearer\n"
                .to_string(),
            format!("name: reviewer\nversion: 0.9.0\nartifacts: []\n{REFUSES}{AUTHENTICATED}"),
        ] {
            let mut f = formation();
            install(&f.store, "reviewer", "0.9.0", &reviewer_manifest);
            assert_eq!(
                refusal(&f.roster, &f.store, None),
                RosterRefusal::CalleeDoesNotServePeers {
                    member: "reviewer".to_string()
                }
            );
            f.roster.reachability = RosterReachability::Rules(vec![
                rule("planner", &["coder"]),
                rule("reviewer", &["coder"]),
            ]);
            admit_roster(&f.roster, &f.store, None).unwrap();
        }
    }

    #[test]
    fn s8_with_traffic_every_member_needs_authentication_even_off_every_edge() {
        let f = formation();
        install(
            &f.store,
            "coder",
            "1.2.0",
            &manifest("coder", "1.2.0", true, false),
        );
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::MemberNotAuthenticated {
                member: "coder".to_string()
            }
        );

        let mut f = formation();
        install(
            &f.store,
            "notes",
            "1.0.0",
            &manifest("notes", "1.0.0", false, false),
        );
        f.roster.members.push(member("notes", "1.0.0", false));
        assert_eq!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::MemberNotAuthenticated {
                member: "notes".to_string()
            }
        );
    }

    #[test]
    fn s9_an_uninstalled_or_unreadable_member_is_refused_naming_its_coordinate() {
        let mut f = formation();
        f.roster.members[1].version = "9.9.9".to_string();
        match refusal(&f.roster, &f.store, None) {
            RosterRefusal::MemberUnresolvable {
                member, coordinate, ..
            } => {
                assert_eq!(member, "coder");
                assert_eq!(coordinate, "coder@9.9.9");
            }
            other => panic!("unexpected refusal {other:?}"),
        }

        let f = formation();
        install(&f.store, "coder", "1.2.0", "name: [unclosed\n");
        match refusal(&f.roster, &f.store, None) {
            RosterRefusal::MemberUnresolvable {
                member, coordinate, ..
            } => {
                assert_eq!(member, "coder");
                assert_eq!(coordinate, "coder@1.2.0");
            }
            other => panic!("unexpected refusal {other:?}"),
        }
    }

    #[test]
    fn bytes_that_do_not_match_the_store_hash_are_refused() {
        let f = formation();
        let artifact = f
            .store
            .root()
            .join("coder")
            .join("1.2.0")
            .join("coder-1.2.0.mur.zip");
        fs::write(&artifact, pack(&manifest("coder", "1.2.0", true, false))).unwrap();
        assert!(matches!(
            refusal(&f.roster, &f.store, None),
            RosterRefusal::MemberUnresolvable { member, .. } if member == "coder"
        ));
    }

    #[test]
    fn s10_the_lock_verifies_a_member_it_pins() {
        let f = formation();
        let agreeing = lock_pinning("coder", "1.2.0", &f.hashes["coder"]);
        admit_roster(&f.roster, &f.store, Some(&agreeing)).unwrap();

        for lock in [
            lock_pinning("coder", "1.2.0", &"0".repeat(64)),
            lock_pinning("coder", "1.1.0", &f.hashes["coder"]),
        ] {
            match refusal(&f.roster, &f.store, Some(&lock)) {
                RosterRefusal::MemberLockConflict {
                    member,
                    capsule,
                    conflict,
                } => {
                    assert_eq!(member, "coder");
                    assert_eq!(capsule, "coder");
                    assert!(!conflict.is_empty());
                }
                other => panic!("unexpected refusal {other:?}"),
            }
        }
    }

    #[test]
    fn s13_a_duplicate_and_an_unknown_rule_name_is_always_the_duplicate() {
        let f = formation();
        let roster = Roster {
            members: vec![
                member("coder", "1.2.0", true),
                member("coder", "1.2.0", false),
            ],
            reachability: RosterReachability::Rules(vec![rule("coder", &["ghost"])]),
        };
        for _ in 0..5 {
            assert!(matches!(
                refusal(&roster, &f.store, None),
                RosterRefusal::DuplicateMember { .. }
            ));
        }
    }

    #[test]
    fn admit_roster_file_maps_every_load_error_to_malformed_and_records_the_source() {
        let f = formation();
        let project = tempfile::tempdir().unwrap();
        assert!(matches!(
            admit_roster_file(project.path(), &f.store, None),
            Err(RosterRefusal::Malformed { message }) if message.contains("roster.yaml not found")
        ));

        fs::write(
            resolve_roster_path(project.path()),
            "roster_version: 2\nmembers: []\n",
        )
        .unwrap();
        assert!(matches!(
            admit_roster_file(project.path(), &f.store, None),
            Err(RosterRefusal::Malformed { message }) if message.contains("roster_version")
        ));

        fs::write(
            resolve_roster_path(project.path()),
            "roster_version: 1\nmembers:\n  - {name: planner, capsule: planner, version: 0.3.0, entry: true}\n",
        )
        .unwrap();
        let admitted = admit_roster_file(project.path(), &f.store, None).unwrap();
        assert_eq!(
            admitted.source(),
            Some(resolve_roster_path(project.path()).as_path())
        );
    }

    #[test]
    fn every_refusal_names_roster_yaml() {
        for refusal in [
            RosterRefusal::DuplicateMember { member: "a".into() },
            RosterRefusal::EntryNotDesignated,
            RosterRefusal::EntryDesignatedMoreThanOnce {
                members: vec!["a".into(), "b".into()],
            },
            RosterRefusal::UnknownMemberInRule {
                from: "a".into(),
                unknown: "b".into(),
            },
            RosterRefusal::RuleCallsEntryMember {
                from: "a".into(),
                entry: "b".into(),
            },
            RosterRefusal::MemberUnresolvable {
                member: "a".into(),
                coordinate: "a@1".into(),
                reason: "r".into(),
            },
            RosterRefusal::MemberLockConflict {
                member: "a".into(),
                capsule: "a".into(),
                conflict: "c".into(),
            },
            RosterRefusal::CalleeDoesNotServePeers { member: "a".into() },
            RosterRefusal::MemberNotAuthenticated { member: "a".into() },
        ] {
            assert!(refusal.to_string().starts_with("roster.yaml"), "{refusal}");
        }
    }

    const S5A_WARNING: &str = "warning[W-ROS-001]: roster.yaml lets 10 members call 'reviewer' \
        (w01, w02, w03, w04, w05, w06, w07, w08, w09, w10), but it holds 2 tasks at once — one \
        running and lifecycle.queue_depth: 1 waiting — so a call that arrives while it is full is \
        rejected; lifecycle.queue_depth: 9 would hold them all \
        (https://docs.murmur.nexus/reference/diagnostics/#w-ros-001)";

    /// A serving, authenticated member manifest with `lifecycle_yaml` as its `lifecycle:` block's
    /// body, two-space indented; an empty body declares no `lifecycle:` at all.
    fn manifest_with_lifecycle(name: &str, lifecycle_yaml: &str) -> String {
        let mut yaml = manifest(name, "1.0.0", true, true);
        if !lifecycle_yaml.is_empty() {
            yaml.push_str(&format!("lifecycle:\n  {lifecycle_yaml}\n"));
        }
        yaml
    }

    const QUEUE_DEPTH_1: &str = "task_acceptance: queue\n  after_task: sleep";

    fn queue_at(depth: usize) -> String {
        format!("task_acceptance: queue\n  after_task: sleep\n  queue_depth: {depth}")
    }

    /// Install every `(name, lifecycle)` and admit them as one roster, the first member the entry.
    fn admit_with(
        members: &[(&str, &str)],
        reachability: RosterReachability,
    ) -> (tempfile::TempDir, AdmittedRoster) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalRegistry::new(dir.path());
        for (name, lifecycle) in members {
            install(
                &store,
                name,
                "1.0.0",
                &manifest_with_lifecycle(name, lifecycle),
            );
        }
        let roster = Roster {
            members: members
                .iter()
                .enumerate()
                .map(|(index, (name, _))| member(name, "1.0.0", index == 0))
                .collect(),
            reachability,
        };
        let admitted = admit_roster(&roster, &store, None).unwrap();
        (dir, admitted)
    }

    const WORKERS: [&str; 10] = [
        "w01", "w02", "w03", "w04", "w05", "w06", "w07", "w08", "w09", "w10",
    ];

    /// S5a's shape: `lead` calls ten workers, each worker calls `reviewer`.
    fn s5a_shaped(reviewer_lifecycle: &str) -> (tempfile::TempDir, AdmittedRoster) {
        let mut members = vec![("lead", "")];
        members.extend(WORKERS.iter().map(|worker| (*worker, QUEUE_DEPTH_1)));
        members.push(("reviewer", reviewer_lifecycle));
        let mut rules = vec![rule("lead", &WORKERS)];
        rules.extend(WORKERS.iter().map(|worker| rule(worker, &["reviewer"])));
        admit_with(&members, RosterReachability::Rules(rules))
    }

    #[test]
    fn s5a_ten_callers_overflow_a_reviewer_at_depth_one() {
        let (_dir, admitted) = s5a_shaped(QUEUE_DEPTH_1);
        assert_eq!(
            admitted.caller_overflows(),
            [CallerOverflow {
                member: "reviewer".to_string(),
                capsule: "reviewer".to_string(),
                version: "1.0.0".to_string(),
                callers: WORKERS.iter().map(ToString::to_string).collect(),
                task_acceptance: TaskAcceptance::Queue,
                queue_depth: 1,
                holds: 2,
                sufficient_queue_depth: 9,
            }]
        );
    }

    #[test]
    fn s5a_the_warning_line_is_exact() {
        let (_dir, admitted) = s5a_shaped(QUEUE_DEPTH_1);
        let overflows = admitted.caller_overflows();
        assert_eq!(caller_overflow_warning(&overflows[0]), S5A_WARNING);
        assert_eq!(
            caller_overflow_fix(&overflows[0]),
            "reviewer (reviewer@1.0.0): set lifecycle.queue_depth: 9 in its murmur.yaml so all 10 \
             members that may call it are held at once, or narrow its callers in roster.yaml \
             (warning[W-ROS-001])"
        );
    }

    #[test]
    fn a_reviewer_deep_enough_for_every_caller_does_not_overflow() {
        for depth in [9, 10] {
            let (_dir, admitted) = s5a_shaped(&queue_at(depth));
            assert!(
                admitted.caller_overflows().is_empty(),
                "depth {depth} holds all ten callers"
            );
        }
    }

    #[test]
    fn all_counts_every_expanded_caller_and_never_the_entry_member() {
        for (lifecycle, overflows) in [
            (String::new(), true),
            (QUEUE_DEPTH_1.to_string(), true),
            (queue_at(2), false),
        ] {
            let members: Vec<(&str, &str)> = ["a", "b", "c", "d"]
                .iter()
                .map(|name| (*name, lifecycle.as_str()))
                .collect();
            let (_dir, admitted) = admit_with(&members, RosterReachability::All);
            let found = admitted.caller_overflows();
            if !overflows {
                assert!(found.is_empty(), "'{lifecycle}' holds three callers");
                continue;
            }
            let named: Vec<_> = found.iter().map(|o| o.member.as_str()).collect();
            assert_eq!(named, ["b", "c", "d"], "'{lifecycle}'");
            for overflow in &found {
                assert_eq!(overflow.callers.len(), 3, "{overflow:?}");
                assert!(!overflow.callers.contains(&overflow.member));
                assert_eq!(overflow.sufficient_queue_depth, 2);
            }
            assert_eq!(found[0].callers, ["a", "c", "d"]);
        }
    }

    #[test]
    fn a_single_member_with_two_callers_is_told_to_switch_to_queue() {
        let (_dir, admitted) = admit_with(
            &[("lead", ""), ("helper", ""), ("coder", "")],
            RosterReachability::Rules(vec![
                rule("lead", &["coder", "helper"]),
                rule("helper", &["coder"]),
            ]),
        );
        let overflows = admitted.caller_overflows();
        assert_eq!(overflows.len(), 1);
        let overflow = &overflows[0];
        assert_eq!(overflow.member, "coder");
        assert_eq!(overflow.callers, ["lead", "helper"]);
        assert_eq!(overflow.task_acceptance, TaskAcceptance::Single);
        assert_eq!(overflow.holds, 1);
        assert_eq!(overflow.sufficient_queue_depth, 1);
        assert_eq!(
            caller_overflow_warning(overflow),
            "warning[W-ROS-001]: roster.yaml lets 2 members call 'coder' (lead, helper), but it \
             holds 1 task at once — lifecycle.task_acceptance: single — so a call that arrives \
             while it is busy is rejected; lifecycle.task_acceptance: queue with \
             lifecycle.queue_depth: 1 would hold them all \
             (https://docs.murmur.nexus/reference/diagnostics/#w-ros-001)"
        );
        assert_eq!(
            caller_overflow_fix(overflow),
            "coder (coder@1.0.0): set lifecycle.task_acceptance: queue and \
             lifecycle.queue_depth: 1 in its murmur.yaml so all 2 members that may call it are \
             held at once, or narrow its callers in roster.yaml (warning[W-ROS-001])"
        );
    }

    #[test]
    fn a_member_that_accepts_no_task_overflows_with_one_caller() {
        let (_dir, admitted) = admit_with(
            &[("lead", ""), ("idle", "task_acceptance: none")],
            RosterReachability::Rules(vec![rule("lead", &["idle"])]),
        );
        let overflows = admitted.caller_overflows();
        assert_eq!(overflows.len(), 1);
        assert_eq!(overflows[0].holds, 0);
        assert_eq!(
            caller_overflow_warning(&overflows[0]),
            "warning[W-ROS-001]: roster.yaml lets 1 member call 'idle' (lead), but it holds no \
             task — lifecycle.task_acceptance: none — so every call is rejected; \
             lifecycle.task_acceptance: queue with lifecycle.queue_depth: 1 would hold them all \
             (https://docs.murmur.nexus/reference/diagnostics/#w-ros-001)"
        );
        assert_eq!(
            caller_overflow_fix(&overflows[0]),
            "idle (idle@1.0.0): set lifecycle.task_acceptance: queue and lifecycle.queue_depth: 1 \
             in its murmur.yaml so the 1 member that may call it is held at once, or narrow its \
             callers in roster.yaml (warning[W-ROS-001])"
        );
    }

    #[test]
    fn a_queue_member_at_depth_zero_holds_no_task() {
        let (_dir, admitted) = admit_with(
            &[("lead", ""), ("shut", &queue_at(0))],
            RosterReachability::Rules(vec![rule("lead", &["shut"])]),
        );
        let overflows = admitted.caller_overflows();
        assert_eq!(overflows.len(), 1);
        assert_eq!(
            caller_overflow_warning(&overflows[0]),
            "warning[W-ROS-001]: roster.yaml lets 1 member call 'shut' (lead), but it holds no \
             task — lifecycle.queue_depth: 0 — so every call is rejected; \
             lifecycle.queue_depth: 1 would hold them all \
             (https://docs.murmur.nexus/reference/diagnostics/#w-ros-001)"
        );
    }

    /// The entry member holds no task of its own here, yet nothing may call it, so it is never
    /// the member named.
    #[test]
    fn the_entry_member_is_never_named() {
        let (_dir, admitted) = admit_with(
            &[
                ("lead", "task_acceptance: none"),
                ("a", ""),
                ("b", &queue_at(5)),
            ],
            RosterReachability::All,
        );
        assert!(!admitted.edges().is_empty());
        assert!(admitted
            .caller_overflows()
            .iter()
            .all(|overflow| overflow.member != "lead"));
    }

    #[test]
    fn a_roster_with_no_edges_has_no_overflow() {
        let (_dir, admitted) = admit_with(
            &[
                ("lead", "task_acceptance: none"),
                ("idle", "task_acceptance: none"),
            ],
            RosterReachability::Closed,
        );
        assert!(admitted.edges().is_empty());
        assert!(admitted.caller_overflows().is_empty());
    }
}
