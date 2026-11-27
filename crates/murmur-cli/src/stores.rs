//! The on-disk artifact stores a session resolves against: the project store
//! `<project>/.murmur/artifacts/` and the global store `~/.murmur/artifacts/`.

use std::{collections::BTreeMap, fs::File, path::Path};

use murmur_artifact::{
    close_artifact_names, wit_contracts_from_artifact_reader, ArtifactMeta, ArtifactName,
    LocalRegistry, MurmurLock, PublishResult, Registry, RegistryError, ResolvedArtifact,
    UnservedInterface, WitContracts, LOCAL_SCOPE,
};

/// Resolves against `primary` first and, on `NotFound` only, falls back to `secondary`.
///
/// A session finds its artifacts in the project store first, then the global one. Both `mur run`
/// and `mur eval` stage against this chain, so an artifact the pre-flight check in
/// `commands::run::artifact_presence` reports as present is also one staging can load; staging
/// against a single store would let the two disagree.
///
/// Only `NotFound` falls through. Any other error is a real failure of the primary store and is
/// returned as such rather than being masked by a lookup somewhere else.
pub(crate) struct FallbackRegistry {
    pub(crate) primary: LocalRegistry,
    pub(crate) secondary: LocalRegistry,
}

impl Registry for FallbackRegistry {
    fn resolve(
        &self,
        name: &ArtifactName,
        version: &str,
    ) -> Result<ResolvedArtifact, RegistryError> {
        match self.primary.resolve(name, version) {
            Err(RegistryError::NotFound { .. }) => self.secondary.resolve(name, version),
            other => other,
        }
    }

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError> {
        self.primary.publish(meta, bytes)
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        self.primary.list_index()
    }

    /// Both stores' rows, the project store's winning on the same `name@version`, as a resolve
    /// would.
    fn list_stored(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        let mut stored = self.primary.list_index()?;
        for meta in self.secondary.list_index()? {
            if !stored
                .iter()
                .any(|held| held.name == meta.name && held.version == meta.version)
            {
                stored.push(meta);
            }
        }
        Ok(stored)
    }

    /// `project store` when the project store holds `name@version`, else `global store` when the
    /// global one does: the store a resolve takes it from.
    fn held_in(&self, name: &ArtifactName, version: &str) -> Option<String> {
        let holds = |store: &LocalRegistry| {
            store
                .artifact_path_for(name, version)
                .parent()
                .is_some_and(std::path::Path::is_dir)
        };
        if holds(&self.primary) {
            Some(PROJECT_STORE.to_string())
        } else if holds(&self.secondary) {
            Some(GLOBAL_STORE.to_string())
        } else {
            None
        }
    }
}

/// How a pull's `fetched_from` and its messages name the project store.
pub(crate) const PROJECT_STORE: &str = "project store";
/// How a pull's `fetched_from` and its messages name the global store.
pub(crate) const GLOBAL_STORE: &str = "global store";

/// The interfaces one installed artifact speaks: the `wit_contracts` its sidecar records when the
/// key is present, otherwise derived from its payload.
///
/// The store derives the recorded value from the payload on every write, so a recorded value is
/// used as is. The key is absent for a sidecar written before the field existed, which is the
/// case derivation covers, and for a payload with no component, where derivation finds nothing
/// after one zip central-directory read. The payload is opened as a file and only its root wasm
/// entry is read.
pub(crate) fn installed_contracts(
    registry: &LocalRegistry,
    meta: &ArtifactMeta,
) -> Option<WitContracts> {
    if let Some(contracts) = &meta.wit_contracts {
        return Some(contracts.clone());
    }
    let payload = File::open(registry.artifact_path_for(&meta.name, &meta.version)).ok()?;
    let mut contracts = wit_contracts_from_artifact_reader(payload)?;
    for names in [&mut contracts.exports, &mut contracts.imports] {
        names.sort();
        names.dedup();
    }
    Some(contracts)
}

/// An interface name as a doctor line or a search note shows it: as recorded, or marked when it
/// carries no version.
pub(crate) fn interface_label(interface: &UnservedInterface) -> String {
    match interface.artifact_version {
        Some(_) => interface.interface.clone(),
        None => format!("{} (unversioned)", interface.interface),
    }
}

/// What this `mur` serves of the interface's package, completing a doctor line or a search note.
pub(crate) fn served_phrase(interface: &UnservedInterface) -> String {
    match &interface.served_version {
        Some(served) => format!("this mur serves {}@{served}", interface.package),
        None => format!("this mur serves no version of {}", interface.package),
    }
}

/// One full name an artifact is installed or pinned under, with every version held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledName {
    pub(crate) name: ArtifactName,
    /// Sorted, each once.
    pub(crate) versions: Vec<String>,
}

/// Every artifact name the project store `<project_root>/.murmur/artifacts/`, the global store
/// and `lock` hold, sorted by full name: the names a refusal of a name that resolves to nothing
/// can offer.
///
/// Best effort, for a hint only: a store that cannot be read, a home that cannot be found and a
/// lock entry whose name does not parse contribute nothing, and nothing is created.
pub(crate) fn installed_names(
    project_root: &Path,
    lock: Option<&MurmurLock>,
) -> Vec<InstalledName> {
    let mut stores = vec![LocalRegistry::new(
        project_root.join(".murmur").join("artifacts"),
    )];
    if let Ok(global) = LocalRegistry::from_default_home() {
        stores.push(global);
    }
    installed_names_in(&stores, lock)
}

fn installed_names_in(stores: &[LocalRegistry], lock: Option<&MurmurLock>) -> Vec<InstalledName> {
    let mut held: BTreeMap<ArtifactName, Vec<String>> = BTreeMap::new();
    for meta in stores
        .iter()
        .filter_map(|store| store.list_index().ok())
        .flatten()
    {
        held.entry(meta.name).or_default().push(meta.version);
    }
    for locked in lock.map_or(&[][..], |lock| &lock.artifacts) {
        if let Ok(name) = ArtifactName::parse(&locked.name) {
            held.entry(name)
                .or_default()
                .push(locked.resolved_version.clone());
        }
    }
    held.into_iter()
        .map(|(name, mut versions)| {
            versions.sort();
            versions.dedup();
            InstalledName { name, versions }
        })
        .collect()
}

/// How many close names a hint lists before it counts the rest.
const CLOSE_NAMES_LISTED: usize = 5;

/// The hint for a refusal of `wanted`, a name that resolves to nothing: the `installed` names
/// close to it, by full `<scope>/<name>` and versions, and the name to write. `None` when none is
/// close.
pub(crate) fn close_names_hint(wanted: &str, installed: &[InstalledName]) -> Option<String> {
    let close = close_artifact_names(wanted, installed.iter().map(|held| &held.name));
    let (first, _) = close.split_first()?;
    let listed: Vec<&InstalledName> = close
        .iter()
        .take(CLOSE_NAMES_LISTED)
        .filter_map(|name| installed.iter().find(|held| held.name == **name))
        .collect();
    let mut names = listed
        .iter()
        .map(|held| format!("{} ({})", held.name, held.versions.join(", ")))
        .collect::<Vec<_>>()
        .join(", ");
    if close.len() > CLOSE_NAMES_LISTED {
        names.push_str(&format!(
            ", and {} more (mur list shows them)",
            close.len() - CLOSE_NAMES_LISTED
        ));
    }
    let mut hint = if close.len() == 1 {
        format!("installed under a close name: {names} \u{2014} name it '{first}' in murmur.yaml")
    } else {
        format!(
            "installed under a close name: {names} \u{2014} name the one you mean in murmur.yaml \
             by its full <scope>/<name>"
        )
    };
    if listed.iter().any(|held| held.name.is_local()) {
        hint.push_str(&format!(
            "; an artifact installed from a .mur.zip file is named {LOCAL_SCOPE}/<name>"
        ));
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{LockOrigin, LockedArtifact, LockedSha256};

    fn held(name: &str, versions: &[&str]) -> InstalledName {
        InstalledName {
            name: ArtifactName::parse(name).unwrap(),
            versions: versions.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn close_names_hint_is_none_when_nothing_is_close() {
        assert_eq!(close_names_hint("murmur/tool-editor", &[]), None);
        let installed = [held("local/echo-tool", &["0.0.1"])];
        assert_eq!(close_names_hint("murmur/tool-editor", &installed), None);
        assert_eq!(close_names_hint("local/echo-tool", &installed), None);
    }

    #[test]
    fn close_names_hint_names_the_one_local_name_to_write() {
        let installed = [
            held("local/echo-tool", &["0.0.1"]),
            held("murmur/driver-anthropic", &["0.1.4"]),
        ];
        let expected = "installed under a close name: local/echo-tool (0.0.1) \u{2014} name it \
                        'local/echo-tool' in murmur.yaml; an artifact installed from a .mur.zip \
                        file is named local/<name>";
        assert_eq!(
            close_names_hint("echo-tool", &installed).as_deref(),
            Some(expected)
        );
        assert_eq!(
            close_names_hint("murmur/echo-tool", &installed).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn close_names_hint_without_a_local_name_has_no_local_clause() {
        let installed = [held("ana/echo-tool", &["0.1.0", "0.2.0"])];
        assert_eq!(
            close_names_hint("echo-tool", &installed).as_deref(),
            Some(
                "installed under a close name: ana/echo-tool (0.1.0, 0.2.0) \u{2014} name it \
                 'ana/echo-tool' in murmur.yaml"
            )
        );
    }

    #[test]
    fn close_names_hint_with_several_asks_for_the_full_name() {
        let installed = [
            held("ana/echo-tool", &["0.1.0"]),
            held("local/echo-tool", &["0.0.1"]),
        ];
        assert_eq!(
            close_names_hint("murmur/echo-tool", &installed).as_deref(),
            Some(
                "installed under a close name: ana/echo-tool (0.1.0), local/echo-tool (0.0.1) \
                 \u{2014} name the one you mean in murmur.yaml by its full <scope>/<name>; an \
                 artifact installed from a .mur.zip file is named local/<name>"
            )
        );
    }

    #[test]
    fn close_names_hint_lists_five_and_counts_the_rest() {
        let installed: Vec<InstalledName> = ["a", "b", "c", "d", "e", "f"]
            .iter()
            .map(|scope| held(&format!("{scope}/echo-tool"), &["1.0.0"]))
            .collect();
        assert_eq!(
            close_names_hint("echo-tool", &installed).as_deref(),
            Some(
                "installed under a close name: a/echo-tool (1.0.0), b/echo-tool (1.0.0), \
                 c/echo-tool (1.0.0), d/echo-tool (1.0.0), e/echo-tool (1.0.0), and 1 more (mur \
                 list shows them) \u{2014} name the one you mean in murmur.yaml by its full \
                 <scope>/<name>"
            )
        );
    }

    #[test]
    fn close_names_hint_local_clause_follows_the_listed_names_only() {
        let mut installed: Vec<InstalledName> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|scope| held(&format!("{scope}/echo-tool"), &["1.0.0"]))
            .collect();
        installed.push(held("local/echo-tool", &["1.0.0"]));
        let hint = close_names_hint("echo-tool", &installed).unwrap();
        assert!(hint.contains("and 1 more"), "{hint}");
        assert!(!hint.contains("local/"), "{hint}");
    }

    fn store_with(root: &Path, name: &str, versions: &[&str]) {
        let name = ArtifactName::parse(name).unwrap();
        for version in versions {
            let store = LocalRegistry::new(root);
            let dir = store.artifact_path_for(&name, version);
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            std::fs::write(&dir, b"zip").unwrap();
            std::fs::write(store.sha256_path_for(&name, version), "0".repeat(64)).unwrap();
        }
    }

    #[test]
    fn installed_names_merges_the_stores_and_the_lock_by_full_name() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project-store");
        let missing = temp.path().join("no-such-home").join("artifacts");
        store_with(&project, "local/echo-tool", &["0.2.0", "0.0.1"]);
        store_with(&project, "ana/tool-x", &["1.0.0"]);
        let lock = MurmurLock {
            lock_version: 3,
            artifacts: vec![
                LockedArtifact {
                    name: "local/echo-tool".to_string(),
                    resolved_version: "0.0.1".to_string(),
                    sha256: LockedSha256::any("a".repeat(64)),
                    origin: LockOrigin::Operator,
                    owner_id: None,
                    signer: None,
                },
                LockedArtifact {
                    name: "murmur/driver-anthropic".to_string(),
                    resolved_version: "0.1.4".to_string(),
                    sha256: LockedSha256::any("b".repeat(64)),
                    origin: LockOrigin::Operator,
                    owner_id: None,
                    signer: None,
                },
                LockedArtifact {
                    name: "bare-name".to_string(),
                    resolved_version: "9.9.9".to_string(),
                    sha256: LockedSha256::any("c".repeat(64)),
                    origin: LockOrigin::Operator,
                    owner_id: None,
                    signer: None,
                },
            ],
        };
        let names = installed_names_in(
            &[LocalRegistry::new(&project), LocalRegistry::new(&missing)],
            Some(&lock),
        );
        assert_eq!(
            names,
            [
                held("ana/tool-x", &["1.0.0"]),
                held("local/echo-tool", &["0.0.1", "0.2.0"]),
                held("murmur/driver-anthropic", &["0.1.4"]),
            ]
        );
        assert!(!missing.exists(), "listing a missing store creates nothing");
        assert!(!missing.parent().unwrap().exists());

        let without_lock = installed_names_in(&[LocalRegistry::new(&missing)], None);
        assert!(without_lock.is_empty());
    }
}
