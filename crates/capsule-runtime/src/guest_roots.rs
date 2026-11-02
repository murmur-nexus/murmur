//! The host folders a WASM tool is handed beyond the work folder: its *extra roots*.
//!
//! A WASM tool reaches the host only through the directories preopened into its WASI context.
//! Every session hands an unscoped `runtime: tool` the work folder at `.`, `murmur-runtime/`
//! read-only and `state/` when its entry grants a store. A session the operator gave more of the
//! host hands it more:
//!
//! | Session | Extra roots |
//! |---|---|
//! | Unconfined ([`ContainmentClass::Unconfined`]) | The host's `/`, read-write, at `murmur-host` |
//! | Contained, with `host_paths` | Each entry at `murmur-host` + its canonical path, with its access |
//! | Contained, no `host_paths` | None |
//!
//! Every root sits under the one reserved guest folder [`HOST_ROOT_GUEST_PATH`] rather than at its
//! own absolute path. wasi-libc strips a leading `/` from both preopen names and paths and resolves
//! a relative path as if it were absolute, so a `/` preopen would collide with `.` and catch every
//! relative path of every tool, and a root at `/data` would catch a relative `data/out.csv`. Under
//! one reserved name, a relative path still resolves in the work folder, and only a work-folder
//! folder named `murmur-host` is shadowed for a tool holding a root.
//!
//! A guest learns which roots it holds from `wasi:filesystem/preopens.get-directories`, whose names
//! are the guest paths built here.
//!
//! Who gets roots is [`crate::runtime_dir::gets_runtime_dir_preopen`]'s set: a tool narrowed to a
//! `capabilities.filesystem.scope` keeps exactly its subtree, and drivers, hooks, the capsule's own
//! store, a script capsule's component and native tools get none.

use std::path::{Component, Path, PathBuf};

use murmur_artifact::ContainmentClass;
use serde::Serialize;

use crate::host_paths::{HostPath, HostPathAccess};
use crate::types::CapabilityPolicy;

/// The guest folder every extra root sits under. A root at host path `P` is at this name followed
/// by `P`; the host's `/` is this name itself.
pub const HOST_ROOT_GUEST_PATH: &str = "murmur-host";

/// Why a session hands its WASM tools a root, as a stable wire name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestRootSource {
    /// The session is unconfined: the root is the host's `/`.
    Unconfined,
    /// A `host_paths` entry of `~/.murmur/config.yaml`.
    HostPaths,
}

/// One host directory preopened into a WASM tool beyond its usual folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestRoot {
    /// The preopen's name, which is the path the guest opens it at: [`guest_path_for`] of
    /// [`Self::host_path`].
    pub guest_path: String,
    /// The canonical host directory preopened.
    pub host_path: PathBuf,
    /// `read` preopens it read-only; `read-write` read and write. Never executable: a WASM guest
    /// cannot exec.
    pub access: HostPathAccess,
    pub source: GuestRootSource,
}

/// The extra roots of every tool [`crate::runtime_dir::gets_runtime_dir_preopen`] admits, in
/// preopen order, for a session run under `policy`.
#[must_use]
pub fn extra_guest_roots(policy: &CapabilityPolicy) -> Vec<GuestRoot> {
    extra_guest_roots_for(policy.containment_floor, &policy.host_paths)
}

/// [`extra_guest_roots`] from its two inputs, for a report that holds them without a policy.
///
/// Unconfined gives the host's `/` alone: a host path nested under it would add nothing, and a
/// `read` one would narrow the tool, since the longest matching preopen wins. Otherwise each host
/// path gives one root, in config order, on every contained tier: a WASM tool runs inside `mur`,
/// which no tier confines, so its preopens are its only fence on every host.
#[must_use]
pub fn extra_guest_roots_for(
    containment_floor: ContainmentClass,
    host_paths: &[HostPath],
) -> Vec<GuestRoot> {
    if containment_floor == ContainmentClass::Unconfined {
        let root = Path::new("/");
        return vec![GuestRoot {
            guest_path: guest_path_for(root),
            host_path: root.to_path_buf(),
            access: HostPathAccess::ReadWrite,
            source: GuestRootSource::Unconfined,
        }];
    }
    host_paths
        .iter()
        .map(|host_path| GuestRoot {
            guest_path: guest_path_for(&host_path.path),
            host_path: host_path.path.clone(),
            access: host_path.access,
            source: GuestRootSource::HostPaths,
        })
        .collect()
}

/// The extra roots of tool `runtime`'s guest under `policy`: [`extra_guest_roots`] when
/// [`crate::runtime_dir::gets_runtime_dir_preopen`] holds for it, none otherwise. The one gate the
/// tool dispatch and every report share.
#[must_use]
pub(crate) fn tool_extra_roots(
    runtime: &murmur_artifact::ArtifactRuntime,
    declares_filesystem_scope: bool,
    policy: &CapabilityPolicy,
) -> Vec<GuestRoot> {
    if crate::runtime_dir::gets_runtime_dir_preopen(runtime, declares_filesystem_scope) {
        extra_guest_roots(policy)
    } else {
        Vec::new()
    }
}

/// One directory a guest's WASI context preopens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedPreopen {
    pub(crate) host_path: PathBuf,
    /// The preopen's name, which `wasi:filesystem/preopens.get-directories` hands the guest.
    pub(crate) guest_path: String,
    pub(crate) read_only: bool,
}

/// Every directory a guest's WASI context preopens, in registration order: the work folder (or the
/// entry's scope) at `.`, then [`crate::state_store::STATE_PREOPEN_NAME`] when a store is granted,
/// then [`crate::runtime_dir::RUNTIME_DIR_GUEST_PATH`] read-only, then each extra root.
///
/// Extra roots come last so the first three keep their positions; nested roots resolve by longest
/// prefix in the guest whatever their order.
pub(crate) fn planned_preopens(
    preopen_root: &Path,
    state_dir: Option<&Path>,
    runtime_dir: Option<&Path>,
    extra_roots: &[GuestRoot],
) -> Vec<PlannedPreopen> {
    let mut planned = vec![PlannedPreopen {
        host_path: preopen_root.to_path_buf(),
        guest_path: ".".to_string(),
        read_only: false,
    }];
    if let Some(state_dir) = state_dir {
        planned.push(PlannedPreopen {
            host_path: state_dir.to_path_buf(),
            guest_path: crate::state_store::STATE_PREOPEN_NAME.to_string(),
            read_only: false,
        });
    }
    // Read-only: the guest reads `MURMUR.md`, `task.md`, plans and installed tools there and
    // changes none of them.
    if let Some(runtime_dir) = runtime_dir {
        planned.push(PlannedPreopen {
            host_path: runtime_dir.to_path_buf(),
            guest_path: crate::runtime_dir::RUNTIME_DIR_GUEST_PATH.to_string(),
            read_only: true,
        });
    }
    planned.extend(extra_roots.iter().map(|root| PlannedPreopen {
        host_path: root.host_path.clone(),
        guest_path: root.guest_path.clone(),
        read_only: root.access == HostPathAccess::Read,
    }));
    planned
}

/// The guest path of a root at absolute host path `host_path`: [`HOST_ROOT_GUEST_PATH`] joined
/// with it, `/`-separated, no trailing `/`. `/` is `murmur-host`; `/home/op/.cargo` is
/// `murmur-host/home/op/.cargo`.
#[must_use]
pub fn guest_path_for(host_path: &Path) -> String {
    let mut guest = HOST_ROOT_GUEST_PATH.to_string();
    for component in host_path.components() {
        if let Component::Normal(name) = component {
            guest.push('/');
            guest.push_str(&name.to_string_lossy());
        }
    }
    guest
}

/// The host path a guest path names through `roots`, or `None` when it names none.
///
/// Read the way wasi-libc reads it: leading `/` and `.` components dropped, `.` and `..` folded,
/// then the root with the longest component-wise prefix wins. A path whose first folded component
/// is not [`HOST_ROOT_GUEST_PATH`], one that `..` takes above its start, and one no root covers
/// all give `None` — the guest's libc resolves each of those into the work folder or nowhere.
#[must_use]
pub fn map_guest_path(roots: &[GuestRoot], guest: &str) -> Option<PathBuf> {
    let components = fold_guest_path(guest)?;
    if components.first().map(String::as_str) != Some(HOST_ROOT_GUEST_PATH) {
        return None;
    }
    let (root, depth) = roots
        .iter()
        .filter_map(|root| {
            let prefix = fold_guest_path(&root.guest_path)?;
            components
                .starts_with(&prefix)
                .then_some((root, prefix.len()))
        })
        .max_by_key(|(_, depth)| *depth)?;
    let mut host = root.host_path.clone();
    for name in &components[depth..] {
        host.push(name);
    }
    Some(host)
}

/// `guest` as a list of names: leading `/` dropped, `.` skipped, `..` popping the previous name.
/// `None` when `..` pops past the start.
fn fold_guest_path(guest: &str) -> Option<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for component in Path::new(guest).components() {
        match component {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir => {
                names.pop()?;
            }
            Component::Normal(name) => names.push(name.to_string_lossy().into_owned()),
        }
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_path(path: &str, access: HostPathAccess) -> HostPath {
        HostPath {
            declared: path.to_string(),
            path: PathBuf::from(path),
            access,
            credential_locations: Vec::new(),
        }
    }

    fn policy(floor: ContainmentClass, host_paths: Vec<HostPath>) -> CapabilityPolicy {
        CapabilityPolicy {
            containment_floor: floor,
            host_paths,
            ..CapabilityPolicy::default()
        }
    }

    fn summary(roots: &[GuestRoot]) -> Vec<(String, String, &'static str, String)> {
        roots
            .iter()
            .map(|root| {
                (
                    root.guest_path.clone(),
                    root.host_path.display().to_string(),
                    root.access.wire_name(),
                    serde_json::to_value(root.source)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect()
    }

    /// `(guest path, read-only)` of each planned preopen of a tool with runtime `runtime`.
    fn tool_plan(
        runtime: &murmur_artifact::ArtifactRuntime,
        scoped: bool,
        state: bool,
        policy: &CapabilityPolicy,
    ) -> Vec<(String, bool)> {
        let roots = tool_extra_roots(runtime, scoped, policy);
        let runtime_dir = crate::runtime_dir::gets_runtime_dir_preopen(runtime, scoped)
            .then_some(Path::new("/runtime"));
        planned_preopens(
            Path::new("/work"),
            state.then_some(Path::new("/state")),
            runtime_dir,
            &roots,
        )
        .into_iter()
        .map(|preopen| (preopen.guest_path, preopen.read_only))
        .collect()
    }

    fn owned(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter()
            .map(|(guest, read_only)| (guest.to_string(), *read_only))
            .collect()
    }

    #[test]
    fn guest_roots_contained_plan_is_todays_preopens() {
        use murmur_artifact::ArtifactRuntime;
        let contained = policy(ContainmentClass::Scoped, Vec::new());
        assert_eq!(
            tool_plan(&ArtifactRuntime::Tool, false, false, &contained),
            owned(&[(".", false), ("murmur-runtime", true)])
        );
        assert_eq!(
            tool_plan(&ArtifactRuntime::Tool, false, true, &contained),
            owned(&[(".", false), ("state", false), ("murmur-runtime", true)])
        );
    }

    #[test]
    fn guest_roots_unconfined_plan_adds_the_host_root_last() {
        use murmur_artifact::ArtifactRuntime;
        for host_paths in [Vec::new(), vec![host_path("/a", HostPathAccess::Read)]] {
            let unconfined = policy(ContainmentClass::Unconfined, host_paths);
            assert_eq!(
                tool_plan(&ArtifactRuntime::Tool, false, true, &unconfined),
                owned(&[
                    (".", false),
                    ("state", false),
                    ("murmur-runtime", true),
                    ("murmur-host", false),
                ])
            );
        }
    }

    #[test]
    fn guest_roots_host_paths_plan_follows_the_existing_three() {
        use murmur_artifact::ArtifactRuntime;
        let contained = policy(
            ContainmentClass::Advisory,
            vec![
                host_path("/a", HostPathAccess::Read),
                host_path("/a/b", HostPathAccess::ReadWrite),
            ],
        );
        assert_eq!(
            tool_plan(&ArtifactRuntime::Tool, false, true, &contained),
            owned(&[
                (".", false),
                ("state", false),
                ("murmur-runtime", true),
                ("murmur-host/a", true),
                ("murmur-host/a/b", false),
            ])
        );
    }

    #[test]
    fn guest_roots_never_reach_a_scoped_tool_a_driver_or_a_hook() {
        use murmur_artifact::ArtifactRuntime;
        let policies = [
            policy(ContainmentClass::Unconfined, Vec::new()),
            policy(
                ContainmentClass::Advisory,
                vec![host_path("/a", HostPathAccess::ReadWrite)],
            ),
        ];
        for policy in &policies {
            assert!(tool_extra_roots(&ArtifactRuntime::Tool, true, policy).is_empty());
            assert_eq!(
                tool_plan(&ArtifactRuntime::Tool, true, false, policy),
                owned(&[(".", false)])
            );
            for runtime in [
                ArtifactRuntime::Driver,
                ArtifactRuntime::Hook,
                ArtifactRuntime::Skill,
            ] {
                for scoped in [false, true] {
                    assert!(tool_extra_roots(&runtime, scoped, policy).is_empty());
                }
            }
            // The capsule's own store and a script capsule's component are built with no roots.
            assert_eq!(
                planned_preopens(Path::new("/work"), None, None, &[]),
                vec![PlannedPreopen {
                    host_path: PathBuf::from("/work"),
                    guest_path: ".".to_string(),
                    read_only: false,
                }]
            );
        }
    }

    #[test]
    fn guest_roots_guest_path_is_murmur_host_and_the_absolute_path() {
        assert_eq!(guest_path_for(Path::new("/")), "murmur-host");
        assert_eq!(
            guest_path_for(Path::new("/home/op/.cargo")),
            "murmur-host/home/op/.cargo"
        );
        assert_eq!(guest_path_for(Path::new("/data/")), "murmur-host/data");
    }

    #[test]
    fn guest_roots_contained_without_host_paths_has_none() {
        for floor in [
            ContainmentClass::Advisory,
            ContainmentClass::Scoped,
            ContainmentClass::Sealed,
        ] {
            assert!(extra_guest_roots(&policy(floor, Vec::new())).is_empty());
        }
    }

    #[test]
    fn guest_roots_unconfined_is_the_whole_host_read_write_whatever_host_paths_say() {
        let expected = vec![(
            "murmur-host".to_string(),
            "/".to_string(),
            "read-write",
            "unconfined".to_string(),
        )];
        let plain = extra_guest_roots(&policy(ContainmentClass::Unconfined, Vec::new()));
        assert_eq!(summary(&plain), expected);
        let with_host_paths = extra_guest_roots(&policy(
            ContainmentClass::Unconfined,
            vec![host_path("/a", HostPathAccess::Read)],
        ));
        assert_eq!(summary(&with_host_paths), expected);
    }

    #[test]
    fn guest_roots_host_paths_follow_config_order_with_their_access() {
        for floor in [
            ContainmentClass::Advisory,
            ContainmentClass::Scoped,
            ContainmentClass::Sealed,
        ] {
            let roots = extra_guest_roots(&policy(
                floor,
                vec![
                    host_path("/a", HostPathAccess::Read),
                    host_path("/a/b", HostPathAccess::ReadWrite),
                ],
            ));
            assert_eq!(
                summary(&roots),
                vec![
                    (
                        "murmur-host/a".to_string(),
                        "/a".to_string(),
                        "read",
                        "host_paths".to_string()
                    ),
                    (
                        "murmur-host/a/b".to_string(),
                        "/a/b".to_string(),
                        "read-write",
                        "host_paths".to_string()
                    ),
                ]
            );
        }
    }

    #[test]
    fn guest_roots_map_by_longest_prefix() {
        let roots = extra_guest_roots(&policy(
            ContainmentClass::Advisory,
            vec![
                host_path("/a/b", HostPathAccess::ReadWrite),
                host_path("/a", HostPathAccess::Read),
            ],
        ));
        assert_eq!(
            map_guest_path(&roots, "murmur-host/a/b/c"),
            Some(PathBuf::from("/a/b/c"))
        );
        assert_eq!(
            map_guest_path(&roots, "murmur-host/a/bc"),
            Some(PathBuf::from("/a/bc"))
        );
        assert_eq!(
            map_guest_path(&roots, "/murmur-host/a/x"),
            Some(PathBuf::from("/a/x"))
        );
        assert_eq!(
            map_guest_path(&roots, "./murmur-host/a/./x/../y"),
            Some(PathBuf::from("/a/y"))
        );
        assert_eq!(map_guest_path(&roots, "murmur-host/other/x"), None);
        assert_eq!(map_guest_path(&roots, "murmur-host"), None);
        assert_eq!(map_guest_path(&roots, "src/murmur-host/a"), None);
        assert_eq!(map_guest_path(&roots, "murmur-hostile/a"), None);
        assert_eq!(map_guest_path(&roots, "../murmur-host/a"), None);
        assert_eq!(map_guest_path(&[], "murmur-host/a/x"), None);
    }

    #[test]
    fn guest_roots_whole_host_maps_every_murmur_host_path() {
        let roots = extra_guest_roots(&policy(ContainmentClass::Unconfined, Vec::new()));
        assert_eq!(
            map_guest_path(&roots, "murmur-host/home/op/notes"),
            Some(PathBuf::from("/home/op/notes"))
        );
        assert_eq!(
            map_guest_path(&roots, "murmur-host"),
            Some(PathBuf::from("/"))
        );
        assert_eq!(map_guest_path(&roots, "notes"), None);
    }
}
