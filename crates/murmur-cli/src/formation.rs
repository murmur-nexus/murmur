//! What a whole formation — a capsule and the transitive closure of its
//! `capabilities.spawn.allow` — needs from the operator's environment, computed offline from
//! manifests alone.
//!
//! Names only. No variable's value is read, and nothing here launches a capsule, contacts a
//! daemon or opens a socket. The report this module produces is rendered by
//! [`crate::commands::doctor`]; the split keeps the traversal testable without a terminal.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use capsule_runtime::EnvelopeAxis;
use murmur_artifact::{
    current_platform, dotenv_variable_names, DotenvError, LocalRegistry, MurmurLock, Registry,
    RuntimeManifest,
};

use crate::registry_client::FallbackRegistry;

/// Everything the walk found, ready to render.
pub(crate) struct FormationEnvReport {
    /// Every capsule whose declarations were read, root first, in first-visit order.
    pub(crate) inspected: Vec<CapsuleCoordinate>,
    /// Every variable the closure declares, sorted by name, one entry per name.
    pub(crate) variables: Vec<RequiredVariable>,
    /// Every declaration `mur-roost` would refuse at the moment of the spawn.
    pub(crate) refusals: Vec<DeclarationRefusal>,
    /// Every capsule named by a `spawn.allow` whose declarations could not be read, one entry
    /// per name however many parents named it.
    pub(crate) uninspectable: Vec<UninspectableCapsule>,
    /// Every `spawn.allow` edge pointing back at a capsule already on the walk.
    pub(crate) cycles: Vec<CycleEdge>,
}

impl FormationEnvReport {
    /// How many capsules the walk reached at all — the denominator of "could not inspect N of M
    /// capsules in this formation". A capsule it could not open was still reached.
    pub(crate) fn reached(&self) -> usize {
        self.inspected.len() + self.uninspectable.len()
    }
}

/// One capsule, at the version the walk resolved it to. There is no `latest` alias, so a
/// coordinate always carries an exact version.
pub(crate) struct CapsuleCoordinate {
    pub(crate) name: String,
    pub(crate) version: String,
}

impl CapsuleCoordinate {
    pub(crate) fn reference(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// One variable name the formation declares, and whether a run from this workspace would find it.
pub(crate) struct RequiredVariable {
    pub(crate) name: String,
    /// Every capsule in the closure declaring it, rendered `name@version`, first-visit order.
    pub(crate) declared_by: Vec<String>,
    pub(crate) set: bool,
}

/// A child declaring an `env.allow` entry the capsule that spawns it does not hold — the spawn
/// envelope violation `mur-roost` raises, predicted before either capsule runs.
pub(crate) struct DeclarationRefusal {
    /// The spawning capsule, `name@version`.
    pub(crate) parent: String,
    /// The spawned capsule, `name@version`.
    pub(crate) child: String,
    pub(crate) variable: String,
    /// Always [`EnvelopeAxis::EnvAllow`]'s manifest key, so the prediction and the refusal cannot
    /// word the same axis differently.
    pub(crate) axis: &'static str,
}

/// A capsule the walk reached but could not read, and why.
pub(crate) struct UninspectableCapsule {
    pub(crate) name: String,
    /// The capsule whose `spawn.allow` named it, `name@version`.
    pub(crate) declared_by: String,
    pub(crate) reason: UninspectableReason,
}

pub(crate) enum UninspectableReason {
    /// Neither store holds any version of this name.
    NotInstalled,
    /// More than one version is installed and nothing pins which one a run would get. The walk
    /// picks none: a guess would report the wrong capsule's requirement as fact.
    AmbiguousVersion { installed: Vec<String> },
    /// A version resolved, but its archive or packed manifest could not be read.
    Unreadable { detail: String },
}

/// A `spawn.allow` edge pointing at a capsule already on the current path.
pub(crate) struct CycleEdge {
    pub(crate) parent: String,
    pub(crate) child: String,
}

/// The one question the report asks of the host: is this name present in the environment a
/// `mur run` from this workspace would start with?
///
/// That is this process's own environment unioned with the names the workspace `.env` declares,
/// which is the same set `load_dotenv_non_override` leaves behind under its non-override rule.
/// Only presence is ever asked; no value is read.
pub(crate) struct EnvironmentNames {
    /// Names the workspace `.env` declares.
    declared: BTreeSet<String>,
    /// Whether this process's own environment counts. False only for the test constructor, so a
    /// unit test's expectations do not depend on what the machine running it exports.
    include_process_env: bool,
}

impl EnvironmentNames {
    /// The environment a run from `workspace_root` would start with.
    ///
    /// A `.env` that cannot be parsed is returned alongside an otherwise usable answer rather
    /// than replacing it: the process environment still decides every name, and suppressing the
    /// whole variable list over one malformed line would hide the report the operator came for.
    pub(crate) fn for_workspace(workspace_root: &Path) -> (Self, Option<DotenvError>) {
        match dotenv_variable_names(workspace_root) {
            Ok(declared) => (
                Self {
                    declared,
                    include_process_env: true,
                },
                None,
            ),
            Err(error) => (
                Self {
                    declared: BTreeSet::new(),
                    include_process_env: true,
                },
                Some(error),
            ),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            declared: names.into_iter().map(Into::into).collect(),
            include_process_env: false,
        }
    }

    /// A name set to the empty string counts as present, because `child_environment` copies it
    /// through to the child as-is.
    pub(crate) fn contains(&self, name: &str) -> bool {
        if self.include_process_env && std::env::var_os(name).is_some() {
            return true;
        }
        self.declared.contains(name)
    }
}

/// Walk the `spawn.allow` closure from `root` and report what the whole formation declares.
///
/// Depth-first, through the stores and the precedence `mur run` resolves an installed capsule
/// through. Every failure is a finding rather than an error: a formation the walk cannot fully
/// read is reported as partially read, never silently under-reported.
pub(crate) fn walk_formation_env(
    root: &RuntimeManifest,
    project_dir: &Path,
    lock: Option<&MurmurLock>,
    environment: &EnvironmentNames,
) -> FormationEnvReport {
    let mut walk = Walk {
        project_dir: project_dir.to_path_buf(),
        project_registry: LocalRegistry::new(project_dir.join(".murmur").join("artifacts")),
        // Absent only on a host with no home directory, where nothing is installed globally
        // either. Every lookup then answers from the project store alone.
        global_registry: LocalRegistry::from_default_home().ok(),
        lock,
        environment,
        cache: HashMap::new(),
        visited: HashSet::new(),
        named_uninspectable: HashSet::new(),
        path: Vec::new(),
        inspected: Vec::new(),
        variables: Vec::new(),
        variable_index: HashMap::new(),
        refusals: Vec::new(),
        uninspectable: Vec::new(),
        cycles: Vec::new(),
    };

    let root_env_allow = declared_env_allow(root);
    let root_spawn_allow = declared_spawn_allow(root);
    let root_ref = format!("{}@{}", root.name, root.version);

    walk.visited.insert(root_ref.clone());
    walk.inspected.push(CapsuleCoordinate {
        name: root.name.clone(),
        version: root.version.clone(),
    });
    walk.record_variables(&root_env_allow, &root_ref);

    walk.path.push(root_ref.clone());
    walk.descend(&root_ref, &root_env_allow, &root_spawn_allow);
    walk.path.pop();

    walk.finish()
}

fn declared_env_allow(manifest: &RuntimeManifest) -> Vec<String> {
    manifest
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.env.as_ref())
        .map(|env| env.allow.clone())
        .unwrap_or_default()
}

fn declared_spawn_allow(manifest: &RuntimeManifest) -> Vec<String> {
    manifest
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.spawn.as_ref())
        .map(|spawn| spawn.allow.clone())
        .unwrap_or_default()
}

/// The two lists one capsule declares, as read from its packed manifest.
#[derive(Clone)]
struct Declarations {
    env_allow: Vec<String>,
    spawn_allow: Vec<String>,
}

struct Walk<'a> {
    project_dir: PathBuf,
    project_registry: LocalRegistry,
    global_registry: Option<LocalRegistry>,
    lock: Option<&'a MurmurLock>,
    environment: &'a EnvironmentNames,
    /// Declarations already read, keyed `name@version`, so no archive is opened twice.
    cache: HashMap<String, Declarations>,
    /// Coordinates already descended from, keyed `name@version`.
    visited: HashSet<String>,
    /// Names already reported uninspectable, so two parents naming one missing capsule report it
    /// once.
    named_uninspectable: HashSet<String>,
    /// The coordinates on the current root-to-here path. What makes a cycle bounded and namable.
    path: Vec<String>,
    inspected: Vec<CapsuleCoordinate>,
    variables: Vec<RequiredVariable>,
    variable_index: HashMap<String, usize>,
    refusals: Vec<DeclarationRefusal>,
    uninspectable: Vec<UninspectableCapsule>,
    cycles: Vec<CycleEdge>,
}

impl Walk<'_> {
    fn descend(&mut self, parent_ref: &str, parent_env_allow: &[String], children: &[String]) {
        for child_name in children {
            let version = match self.resolve_version(child_name) {
                Ok(version) => version,
                Err(reason) => {
                    self.record_uninspectable(child_name, parent_ref, reason);
                    continue;
                }
            };
            let child_ref = format!("{child_name}@{version}");

            let declarations = match self.declarations_for(child_name, &version) {
                Ok(declarations) => declarations,
                Err(detail) => {
                    self.record_uninspectable(
                        child_name,
                        parent_ref,
                        UninspectableReason::Unreadable { detail },
                    );
                    continue;
                }
            };

            // A refusal is a property of the edge, not of the child: the same capsule reached
            // again under a different parent is judged against that parent too.
            for variable in &declarations.env_allow {
                if !parent_env_allow.iter().any(|held| held == variable) {
                    self.refusals.push(DeclarationRefusal {
                        parent: parent_ref.to_string(),
                        child: child_ref.clone(),
                        variable: variable.clone(),
                        axis: EnvelopeAxis::EnvAllow.manifest_key(),
                    });
                }
            }

            if self.path.iter().any(|on_path| on_path == &child_ref) {
                self.cycles.push(CycleEdge {
                    parent: parent_ref.to_string(),
                    child: child_ref.clone(),
                });
                continue;
            }

            // Reached before but not on this path: a diamond, not a cycle. Its declarations are
            // already in the report, and descending again would only repeat them.
            if !self.visited.insert(child_ref.clone()) {
                continue;
            }

            self.inspected.push(CapsuleCoordinate {
                name: child_name.clone(),
                version,
            });
            self.record_variables(&declarations.env_allow, &child_ref);

            self.path.push(child_ref.clone());
            self.descend(
                &child_ref,
                &declarations.env_allow,
                &declarations.spawn_allow,
            );
            self.path.pop();
        }
    }

    /// Which version of `name` a run would get, by the precedence `mur run` resolves through:
    /// the lockfile pins it, else the project store decides alone if it holds the name at all,
    /// else the global store.
    fn resolve_version(&self, name: &str) -> Result<String, UninspectableReason> {
        if let Some(locked) = self.lock.and_then(|lock| lock.artifact_for(name)) {
            return Ok(locked.resolved_version.clone());
        }

        // Only a *missing* artifact falls through to the global store, the same rule
        // `FallbackRegistry` applies: a name the project store holds is that store's answer.
        let mut installed = installed_versions(&self.project_registry, name);
        if installed.is_empty() {
            if let Some(global) = &self.global_registry {
                installed = installed_versions(global, name);
            }
        }

        match installed.len() {
            0 => Err(UninspectableReason::NotInstalled),
            1 => Ok(installed.remove(0)),
            _ => Err(UninspectableReason::AmbiguousVersion {
                installed: installed.into_iter().collect(),
            }),
        }
    }

    /// The `env.allow` and `spawn.allow` one capsule's packed manifest declares.
    ///
    /// Read through the two narrow readers rather than a whole-manifest parse: a child declaring
    /// `inference.api_key: ${PROVIDER_KEY}` is exactly the capsule this preflight exists for, and
    /// a full parse would demand the operator already hold the variable before it could report
    /// that the variable is needed.
    fn declarations_for(&mut self, name: &str, version: &str) -> Result<Declarations, String> {
        let key = format!("{name}@{version}");
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }

        let resolved = self
            .resolve_artifact(name, version)
            .map_err(|error| error.to_string())?;
        let manifest_yaml =
            capsule_runtime::artifact::extract_manifest_yaml(name, version, &resolved.bytes)
                .map_err(|error| error.to_string())?;
        let declarations = Declarations {
            env_allow: capsule_runtime::artifact::extract_declared_env_allow(
                name,
                version,
                &manifest_yaml,
            )
            .map_err(|error| error.to_string())?,
            spawn_allow: capsule_runtime::artifact::extract_declared_spawn_allow(
                name,
                version,
                &manifest_yaml,
            )
            .map_err(|error| error.to_string())?,
        };

        self.cache.insert(key, declarations.clone());
        Ok(declarations)
    }

    fn resolve_artifact(
        &self,
        name: &str,
        version: &str,
    ) -> Result<murmur_artifact::ResolvedArtifact, murmur_artifact::RegistryError> {
        let platform = Some(current_platform());
        let resolved = match &self.global_registry {
            Some(global) => FallbackRegistry {
                primary: LocalRegistry::new(self.project_dir.join(".murmur").join("artifacts")),
                secondary: global.clone(),
            }
            .resolve_with_platform(name, version, platform)?,
            None => self
                .project_registry
                .resolve_with_platform(name, version, platform)?,
        };
        Ok(resolved)
    }

    fn record_variables(&mut self, env_allow: &[String], declaring_ref: &str) {
        for variable in env_allow {
            match self.variable_index.get(variable) {
                Some(&index) => {
                    let declared_by = &mut self.variables[index].declared_by;
                    if !declared_by.iter().any(|had| had == declaring_ref) {
                        declared_by.push(declaring_ref.to_string());
                    }
                }
                None => {
                    self.variable_index
                        .insert(variable.clone(), self.variables.len());
                    self.variables.push(RequiredVariable {
                        name: variable.clone(),
                        declared_by: vec![declaring_ref.to_string()],
                        set: self.environment.contains(variable),
                    });
                }
            }
        }
    }

    fn record_uninspectable(&mut self, name: &str, declared_by: &str, reason: UninspectableReason) {
        if !self.named_uninspectable.insert(name.to_string()) {
            return;
        }
        self.uninspectable.push(UninspectableCapsule {
            name: name.to_string(),
            declared_by: declared_by.to_string(),
            reason,
        });
    }

    fn finish(mut self) -> FormationEnvReport {
        self.variables.sort_by(|a, b| a.name.cmp(&b.name));
        FormationEnvReport {
            inspected: self.inspected,
            variables: self.variables,
            refusals: self.refusals,
            uninspectable: self.uninspectable,
            cycles: self.cycles,
        }
    }
}

/// Every version of `name` this store holds, sorted and without the duplicate a
/// platform-tagged payload set would otherwise contribute.
fn installed_versions(registry: &LocalRegistry, name: &str) -> Vec<String> {
    let Ok(index) = registry.list_index() else {
        return Vec::new();
    };
    let mut versions: Vec<String> = index
        .into_iter()
        .filter(|meta| meta.name == name)
        .map(|meta| meta.version)
        .collect();
    versions.sort();
    versions.dedup();
    versions
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use murmur_artifact::{sha256_hex, ArtifactMeta, RuntimeType};
    use tempfile::TempDir;

    use super::*;

    /// Store a capsule in `project_dir`'s own artifact store, whose packed `murmur.yaml` is
    /// exactly `manifest_yaml`. The project store is enough for every case here and, unlike the
    /// global one, needs no `HOME` override.
    fn install_capsule(project_dir: &Path, name: &str, version: &str, manifest_yaml: &str) {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file("murmur.yaml", options).unwrap();
            zip.write_all(manifest_yaml.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        let bytes = cursor.into_inner();
        let sha256 = sha256_hex(&bytes);
        LocalRegistry::new(project_dir.join(".murmur").join("artifacts"))
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
    }

    fn root_manifest(yaml: &str) -> RuntimeManifest {
        RuntimeManifest::from_yaml_str(yaml).unwrap()
    }

    #[test]
    fn walks_three_levels_and_attributes_every_variable() {
        let project = TempDir::new().unwrap();
        install_capsule(
            project.path(),
            "fmt-worker",
            "0.1.0",
            "name: fmt-worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [A_KEY, B_KEY]\n  spawn:\n    allow: [fmt-deep]\n",
        );
        install_capsule(
            project.path(),
            "fmt-deep",
            "0.2.0",
            "name: fmt-deep\nversion: 0.2.0\ncapabilities:\n  env:\n    allow: [B_KEY]\n",
        );

        let root = root_manifest(
            "name: fmt-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  env:\n    allow: [A_KEY, B_KEY]\n  spawn:\n    allow: [fmt-worker]\n",
        );
        let environment = EnvironmentNames::from_names(["A_KEY", "B_KEY"]);
        let report = walk_formation_env(&root, project.path(), None, &environment);

        let capsules: Vec<String> = report
            .inspected
            .iter()
            .map(CapsuleCoordinate::reference)
            .collect();
        assert_eq!(
            capsules,
            ["fmt-root@0.0.1", "fmt-worker@0.1.0", "fmt-deep@0.2.0"]
        );
        assert_eq!(
            report
                .variables
                .iter()
                .map(|variable| variable.name.as_str())
                .collect::<Vec<_>>(),
            ["A_KEY", "B_KEY"]
        );
        assert_eq!(
            report.variables[1].declared_by,
            ["fmt-root@0.0.1", "fmt-worker@0.1.0", "fmt-deep@0.2.0"]
        );
        assert!(report.variables.iter().all(|variable| variable.set));
        assert!(report.refusals.is_empty());
        assert!(report.uninspectable.is_empty());
        assert!(report.cycles.is_empty());
        assert_eq!(report.reached(), 3);
    }

    #[test]
    fn a_cycle_is_named_once_and_terminates() {
        let project = TempDir::new().unwrap();
        install_capsule(
            project.path(),
            "cyc-worker",
            "0.1.0",
            "name: cyc-worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [ROOT_KEY]\n  spawn:\n    allow: [cyc-root]\n",
        );
        install_capsule(
            project.path(),
            "cyc-root",
            "0.0.1",
            "name: cyc-root\nversion: 0.0.1\ncapabilities:\n  env:\n    allow: [ROOT_KEY]\n  spawn:\n    allow: [cyc-worker]\n",
        );

        let root = root_manifest(
            "name: cyc-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  env:\n    allow: [ROOT_KEY]\n  spawn:\n    allow: [cyc-worker]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(["ROOT_KEY"]),
        );

        assert_eq!(report.cycles.len(), 1);
        assert_eq!(report.cycles[0].parent, "cyc-worker@0.1.0");
        assert_eq!(report.cycles[0].child, "cyc-root@0.0.1");
        assert_eq!(
            report
                .inspected
                .iter()
                .filter(|capsule| capsule.reference() == "cyc-root@0.0.1")
                .count(),
            1
        );
    }

    #[test]
    fn a_child_declaration_its_parent_lacks_is_a_refusal_on_the_axis_the_envelope_names() {
        let project = TempDir::new().unwrap();
        install_capsule(
            project.path(),
            "ref-worker",
            "0.1.0",
            "name: ref-worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [ROOT_KEY, WORKER_ONLY]\n",
        );

        let root = root_manifest(
            "name: ref-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  env:\n    allow: [ROOT_KEY]\n  spawn:\n    allow: [ref-worker]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(["ROOT_KEY", "WORKER_ONLY"]),
        );

        assert_eq!(report.refusals.len(), 1);
        let refusal = &report.refusals[0];
        assert_eq!(refusal.parent, "ref-root@0.0.1");
        assert_eq!(refusal.child, "ref-worker@0.1.0");
        assert_eq!(refusal.variable, "WORKER_ONLY");
        assert_eq!(refusal.axis, "capabilities.env.allow");
        assert_eq!(refusal.axis, EnvelopeAxis::EnvAllow.manifest_key());
    }

    #[test]
    fn two_installed_versions_with_nothing_pinning_which_is_ambiguous() {
        let project = TempDir::new().unwrap();
        install_capsule(
            project.path(),
            "amb-worker",
            "0.1.0",
            "name: amb-worker\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: [OLD_ONLY]\n",
        );
        install_capsule(
            project.path(),
            "amb-worker",
            "0.2.0",
            "name: amb-worker\nversion: 0.2.0\ncapabilities:\n  env:\n    allow: [NEW_ONLY]\n",
        );

        let root = root_manifest(
            "name: amb-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  env:\n    allow: [ROOT_KEY]\n  spawn:\n    allow: [amb-worker]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(["ROOT_KEY"]),
        );

        assert_eq!(report.uninspectable.len(), 1);
        match &report.uninspectable[0].reason {
            UninspectableReason::AmbiguousVersion { installed } => {
                assert_eq!(installed, &["0.1.0".to_string(), "0.2.0".to_string()]);
            }
            _ => panic!("expected AmbiguousVersion"),
        }
        assert!(report
            .variables
            .iter()
            .all(|variable| variable.name == "ROOT_KEY"));
    }

    #[test]
    fn a_capsule_no_store_holds_is_not_installed_and_stops_that_edge() {
        let project = TempDir::new().unwrap();
        let root = root_manifest(
            "name: ghost-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  spawn:\n    allow: [no-such-capsule-4c7e05b1]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(Vec::<String>::new()),
        );

        assert_eq!(report.uninspectable.len(), 1);
        assert_eq!(report.uninspectable[0].name, "no-such-capsule-4c7e05b1");
        assert_eq!(report.uninspectable[0].declared_by, "ghost-root@0.0.1");
        assert!(matches!(
            report.uninspectable[0].reason,
            UninspectableReason::NotInstalled
        ));
        assert_eq!(report.reached(), 2);
    }

    #[test]
    fn an_unreadable_archive_is_uninspectable_rather_than_an_error() {
        let project = TempDir::new().unwrap();
        let store = project.path().join(".murmur").join("artifacts");
        let bytes = b"not a zip archive".to_vec();
        let sha256 = sha256_hex(&bytes);
        LocalRegistry::new(&store)
            .store_installed_overwrite(
                ArtifactMeta {
                    name: "torn-worker".to_string(),
                    version: "0.1.0".to_string(),
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

        let root = root_manifest(
            "name: torn-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  spawn:\n    allow: [torn-worker]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(Vec::<String>::new()),
        );

        assert_eq!(report.uninspectable.len(), 1);
        assert!(matches!(
            report.uninspectable[0].reason,
            UninspectableReason::Unreadable { .. }
        ));
    }

    #[test]
    fn a_child_manifest_with_an_unresolvable_reference_is_still_read() {
        let project = TempDir::new().unwrap();
        install_capsule(
            project.path(),
            "infer-worker",
            "0.1.0",
            "name: infer-worker\nversion: 0.1.0\ninference:\n  driver: anthropic\n  model: some-model\n  api_key: ${PROVIDER_KEY_4C7E05B1}\ncapabilities:\n  env:\n    allow: [PROVIDER_KEY_4C7E05B1]\n",
        );

        let root = root_manifest(
            "name: infer-root\nversion: 0.0.1\nartifacts: []\ncapabilities:\n  env:\n    allow: [PROVIDER_KEY_4C7E05B1]\n  spawn:\n    allow: [infer-worker]\n",
        );
        let report = walk_formation_env(
            &root,
            project.path(),
            None,
            &EnvironmentNames::from_names(Vec::<String>::new()),
        );

        assert!(report.uninspectable.is_empty());
        assert_eq!(report.inspected.len(), 2);
        let variable = &report.variables[0];
        assert_eq!(variable.name, "PROVIDER_KEY_4C7E05B1");
        assert!(!variable.set);
        assert_eq!(
            variable.declared_by,
            ["infer-root@0.0.1", "infer-worker@0.1.0"]
        );
    }

    #[test]
    fn a_name_the_dotenv_declares_counts_as_set() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "FROM_DOTENV_4C7E05B1=value\n").unwrap();

        let (environment, error) = EnvironmentNames::for_workspace(dir.path());

        assert!(error.is_none());
        assert!(environment.contains("FROM_DOTENV_4C7E05B1"));
        assert!(!environment.contains("NOT_DECLARED_4C7E05B1"));
    }

    #[test]
    fn an_unparseable_dotenv_surfaces_without_losing_the_process_environment() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(".env"), "no-equals-here\n").unwrap();

        let (environment, error) = EnvironmentNames::for_workspace(dir.path());

        assert!(matches!(
            error,
            Some(DotenvError::InvalidLine { line: 1, .. })
        ));
        assert!(environment.contains("PATH"));
    }
}
