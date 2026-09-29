//! The `capabilities.install` gate on `murmur:artifact-manager/manage.pull`.
//!
//! A pull is decided in two halves. [`refuse_before_resolve`] needs only the requested name and
//! runs before the registry is read, so an ungranted capsule, a name outside the artifact-name
//! rule (a path, a `github:` reference) and a name no entry matches never reach the registry.
//! [`refuse_after_resolve`] needs the resolved [`ArtifactMeta`], because whether a name is a skill
//! or a tool is a property of the payload, and runs before `murmur.lock` is read and before
//! anything is written or compiled.
//!
//! Every refusal is a string beginning [`NOT_GRANTED`], the token `murmur:conversation/read`
//! answers an ungranted hook with: a refused pull is a value the calling component branches on,
//! never a trap. Entry matching is [`murmur_artifact::install_pattern::matches`], the predicate
//! whose [`murmur_artifact::install_pattern::covers`] the spawn referee compares install grants
//! with.
//!
//! A pull that passes gains no grant: the pulled artifact has no `artifact_grants` entry, no
//! gateway and no place in the `invoke()` allowlist, and its bundled `murmur.yaml` is never read
//! into one. It dispatches on the session's own [`CapabilityPolicy`].

use murmur_artifact::install_pattern;
use murmur_artifact::{artifact_name_format_error, ArtifactMeta, RuntimeType};

use crate::types::CapabilityPolicy;

/// The leading token of every refusal this module returns.
pub(crate) const NOT_GRANTED: &str = "not-granted:";

/// Which `capabilities.install` list decides a resolved artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallTier {
    Skill,
    Tool,
}

impl InstallTier {
    /// The tier of a resolved artifact, or `None` when it is not pullable at all.
    ///
    /// Keyed on the payload type, because that is what the pull extracts and so what can
    /// execute: a static payload yields only `skill.md`, a WASM or native payload yields code. The
    /// declared role must agree with it. A `driver` or `hook`, or a role that disagrees with its
    /// payload, is never pullable.
    pub(crate) fn of(meta: &ArtifactMeta) -> Option<Self> {
        match (meta.runtime, meta.artifact_runtime.as_str()) {
            (RuntimeType::Static, "skill") => Some(Self::Skill),
            (RuntimeType::Wasm | RuntimeType::Native, "tool") => Some(Self::Tool),
            _ => None,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Tool => "tool",
        }
    }

    fn entries(self, policy: &CapabilityPolicy) -> &[String] {
        match self {
            Self::Skill => &policy.install_skill,
            Self::Tool => &policy.install_tool,
        }
    }
}

/// Whether the session holds any install grant.
pub(crate) fn is_granted(policy: &CapabilityPolicy) -> bool {
    !(policy.install_skill.is_empty() && policy.install_tool.is_empty())
}

/// The refusal for pulling `name`, decided without the registry, or `None` to go on and resolve
/// it. Checks, in order: the session holds a grant; `name` is an artifact name; some entry of
/// either list matches it.
pub(crate) fn refuse_before_resolve(policy: &CapabilityPolicy, name: &str) -> Option<String> {
    if !is_granted(policy) {
        return Some(format!(
            "{NOT_GRANTED} this capsule declares no capabilities.install, so pulling '{name}' is \
             refused"
        ));
    }
    if let Some(reason) = artifact_name_format_error(name) {
        return Some(format!(
            "{NOT_GRANTED} '{name}' is not an artifact name ({reason}); capabilities.install \
             matches bare artifact names only"
        ));
    }
    let matched = policy
        .install_skill
        .iter()
        .chain(&policy.install_tool)
        .any(|entry| install_pattern::matches(entry, name));
    if !matched {
        return Some(format!(
            "{NOT_GRANTED} '{name}' matches no capabilities.install entry ({})",
            granted_lists(policy)
        ));
    }
    None
}

/// The refusal for installing the resolved `meta` under `name`, or `None` to install it. Checks,
/// in order: the artifact is a skill or a tool ([`InstallTier::of`]); an entry of that tier's own
/// list matches `name`.
pub(crate) fn refuse_after_resolve(
    policy: &CapabilityPolicy,
    name: &str,
    meta: &ArtifactMeta,
) -> Option<String> {
    let Some(tier) = InstallTier::of(meta) else {
        return Some(format!(
            "{NOT_GRANTED} '{name}' is a '{}' artifact ({}); capabilities.install grants only \
             skills and tools",
            meta.artifact_runtime,
            meta.runtime.as_str()
        ));
    };
    let matched = tier
        .entries(policy)
        .iter()
        .any(|entry| install_pattern::matches(entry, name));
    if !matched {
        let key = tier.key();
        return Some(format!(
            "{NOT_GRANTED} '{name}' is a {key}, and capabilities.install.{key} does not name it \
             ({})",
            granted_lists(policy)
        ));
    }
    None
}

/// The install grant as `manage.diagnostics()` states it in `runtime-state.capabilities`.
pub(crate) fn describe(policy: &CapabilityPolicy) -> String {
    if is_granted(policy) {
        format!(
            "install skill: {}; install tool: {}; search and remove are not implemented",
            render_entries(&policy.install_skill),
            render_entries(&policy.install_tool)
        )
    } else {
        "pull: not granted (no capabilities.install); search and remove are not implemented"
            .to_string()
    }
}

fn granted_lists(policy: &CapabilityPolicy) -> String {
    format!(
        "skill: {}; tool: {}",
        render_entries(&policy.install_skill),
        render_entries(&policy.install_tool)
    )
}

fn render_entries(entries: &[String]) -> String {
    if entries.is_empty() {
        "<none>".to_string()
    } else {
        entries.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use murmur_artifact::{
        current_platform, read_lockfile, LocalRegistry, PublishResult, Registry, RegistryError,
        ResolvedArtifact, PACKED_MANIFEST_ENTRY,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::bindings::host::murmur::artifact_manager::manage;
    use crate::runtime::{
        build_test_state, cwasm_entries, grant_install, iface_component_bytes,
        scratch_compiled_dir, wasm_tool_zip, zip_with_files, CapsuleStoreState,
    };
    use crate::spawn_envelope::SpawnEnvelope;
    use crate::types::capability_policy_from_runtime_manifest;

    const VERSION: &str = "1.0.0";

    /// A real on-disk [`LocalRegistry`] that records the name of every resolve it serves, so a
    /// test can prove a refusal was decided before the registry was read.
    struct CountingRegistry {
        inner: LocalRegistry,
        resolved: Mutex<Vec<String>>,
    }

    impl CountingRegistry {
        fn resolved(&self) -> Vec<String> {
            self.resolved.lock().unwrap().clone()
        }
    }

    impl Registry for CountingRegistry {
        fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError> {
            self.resolve_with_platform(name, version, None)
        }

        fn resolve_with_platform(
            &self,
            name: &str,
            version: &str,
            platform: Option<&str>,
        ) -> Result<ResolvedArtifact, RegistryError> {
            self.resolved.lock().unwrap().push(name.to_string());
            self.inner.resolve_with_platform(name, version, platform)
        }

        fn publish(
            &self,
            meta: ArtifactMeta,
            bytes: &[u8],
        ) -> Result<PublishResult, RegistryError> {
            self.inner.publish(meta, bytes)
        }

        fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
            self.inner.list_index()
        }
    }

    /// One project: a registry rooted in a tempdir, and the workdir and `murmur.lock` its
    /// sessions pull into.
    struct Project {
        _dir: TempDir,
        registry: Arc<CountingRegistry>,
        workdir: PathBuf,
        lock_path: PathBuf,
    }

    impl Project {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let workdir = dir.path().join("workdir");
            fs::create_dir_all(&workdir).unwrap();
            let registry = Arc::new(CountingRegistry {
                inner: LocalRegistry::new(dir.path().join("registry")),
                resolved: Mutex::new(Vec::new()),
            });
            Self {
                lock_path: dir.path().join("murmur.lock"),
                _dir: dir,
                registry,
                workdir,
            }
        }

        /// A session on this project granted `skill` and `tool`.
        fn session(&self, skill: &[&str], tool: &[&str]) -> CapsuleStoreState {
            let mut state = build_test_state(
                self.registry.clone(),
                self.workdir.clone(),
                self.lock_path.clone(),
            );
            grant_install(&mut state, skill, tool);
            state
        }

        /// A session on this project holding exactly `policy`.
        fn session_with(&self, policy: CapabilityPolicy) -> CapsuleStoreState {
            let mut state = build_test_state(
                self.registry.clone(),
                self.workdir.clone(),
                self.lock_path.clone(),
            );
            state.capability_policy = policy;
            state
        }

        fn publish(&self, name: &str, runtime: RuntimeType, role: &str, bytes: Vec<u8>) -> String {
            let platforms = match runtime {
                RuntimeType::Native => {
                    let (os, arch) = current_platform()
                        .split_once('-')
                        .expect("a supported host platform");
                    vec![(os.to_string(), arch.to_string())]
                }
                _ => Vec::new(),
            };
            let meta = ArtifactMeta {
                name: name.to_string(),
                version: VERSION.to_string(),
                runtime,
                artifact_runtime: role.to_string(),
                platforms,
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            };
            self.registry.publish(meta, &bytes).unwrap().sha256
        }

        /// Publishes skill `name` whose `skill.md` is `text`, returning its sha256.
        fn publish_skill(&self, name: &str, text: &str) -> String {
            self.publish(
                name,
                RuntimeType::Static,
                "skill",
                zip_with_files(&[
                    (
                        PACKED_MANIFEST_ENTRY,
                        format!("name: {name}\nversion: {VERSION}\nruntime: skill\n").as_bytes(),
                    ),
                    ("skill.md", text.as_bytes()),
                ]),
            )
        }

        /// Publishes a WASM tool `name` whose component compiles.
        fn publish_wasm_tool(&self, name: &str) {
            self.publish(
                name,
                RuntimeType::Wasm,
                "tool",
                wasm_tool_zip(name, &iface_component_bytes("murmur-test:pull/first")),
            );
        }

        /// Publishes a WASM component under role `role` (`driver`, `hook`).
        fn publish_wasm_role(&self, name: &str, role: &str) {
            self.publish(
                name,
                RuntimeType::Wasm,
                role,
                zip_with_files(&[
                    (
                        PACKED_MANIFEST_ENTRY,
                        format!("name: {name}\nversion: {VERSION}\nruntime: {role}\n").as_bytes(),
                    ),
                    (
                        "component.wasm",
                        &iface_component_bytes("murmur-test:pull/first"),
                    ),
                ]),
            );
        }

        /// Publishes a native tool `name` for this host's platform.
        fn publish_native_tool(&self, name: &str) {
            self.publish(
                name,
                RuntimeType::Native,
                "tool",
                zip_with_files(&[
                    (
                        PACKED_MANIFEST_ENTRY,
                        format!(
                            "name: {name}\nversion: {VERSION}\nruntime: tool\nimplementation: native\n"
                        )
                        .as_bytes(),
                    ),
                    (&format!("bin/{name}"), b"#!/bin/sh\necho ok\n"),
                ]),
            );
        }

        fn installed(&self, name: &str) -> PathBuf {
            self.workdir.join("tools").join(name)
        }

        fn locked(&self, name: &str) -> bool {
            read_lockfile(&self.lock_path)
                .map(|lock| lock.artifact_for(name).is_some())
                .unwrap_or(false)
        }
    }

    fn pull(state: &mut CapsuleStoreState, name: &str) -> Result<manage::ArtifactSummary, String> {
        manage::Host::pull(state, name.to_string(), VERSION.to_string())
    }

    /// Every path under `root`, recursively, sorted.
    fn tree(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out.sort();
        out
    }

    /// Runs `install_grant::tests::<inner>` in a child process whose `HOME` is a fresh tempdir,
    /// so the compiled-form cache under `$HOME/.murmur/compiled` is a scratch one.
    fn under_scratch_home(inner: &str) {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(&format!("install_grant::tests::{inner}"), home.path());
    }

    fn meta(runtime: RuntimeType, role: &str) -> ArtifactMeta {
        ArtifactMeta {
            name: "a".to_string(),
            version: VERSION.to_string(),
            runtime,
            artifact_runtime: role.to_string(),
            platforms: Vec::new(),
            description: None,
            tags: Vec::new(),
            wit_contracts: None,
        }
    }

    #[test]
    fn the_tier_is_the_payload_type_and_the_role_must_agree_with_it() {
        let cases = [
            (RuntimeType::Static, "skill", Some(InstallTier::Skill)),
            (RuntimeType::Wasm, "tool", Some(InstallTier::Tool)),
            (RuntimeType::Native, "tool", Some(InstallTier::Tool)),
            (RuntimeType::Wasm, "driver", None),
            (RuntimeType::Wasm, "hook", None),
            (RuntimeType::Native, "driver", None),
            (RuntimeType::Wasm, "skill", None),
            (RuntimeType::Native, "skill", None),
            (RuntimeType::Static, "tool", None),
            (RuntimeType::Static, "driver", None),
        ];
        for (runtime, role, expected) in cases {
            assert_eq!(
                InstallTier::of(&meta(runtime, role)),
                expected,
                "{runtime:?} / {role}"
            );
        }
    }

    #[test]
    fn a_capsule_with_no_install_grant_cannot_pull() {
        let project = Project::new();
        project.publish_skill("my-skill", "# guidance");
        let mut state = project.session(&[], &[]);

        let err = pull(&mut state, "my-skill").expect_err("an ungranted pull is refused");

        assert!(err.starts_with("not-granted:"), "{err}");
        assert!(err.contains("capabilities.install"), "{err}");
        assert!(err.contains("'my-skill'"), "{err}");
        assert!(project.registry.resolved().is_empty());
        assert!(!project.installed("my-skill").exists());
        assert!(!project.lock_path.exists());
        assert!(state.installed_artifacts.is_empty());
    }

    #[test]
    fn a_granted_skill_is_pulled() {
        let project = Project::new();
        let sha256 = project.publish_skill("my-skill", "# guidance");
        let mut state = project.session(&["my-skill"], &[]);

        let summary = pull(&mut state, "my-skill").expect("a granted skill pulls");

        assert_eq!(summary.name, "my-skill");
        assert_eq!(summary.version, VERSION);
        assert_eq!(
            fs::read_to_string(project.installed("my-skill").join("skill.md")).unwrap(),
            "# guidance"
        );
        let lock = read_lockfile(&project.lock_path).unwrap();
        let entry = lock.artifact_for("my-skill").unwrap();
        assert_eq!(entry.resolved_version, VERSION);
        assert_eq!(entry.sha256.any.as_deref(), Some(sha256.as_str()));
        assert!(state
            .installed_artifacts
            .iter()
            .any(|artifact| artifact.name == "my-skill" && artifact.version == VERSION));
    }

    #[test]
    fn a_name_the_grant_does_not_match_is_refused_before_resolving() {
        let project = Project::new();
        project.publish_skill("style-rust", "# rust style");
        project.publish_skill("stylist", "# not granted");
        let mut state = project.session(&["style-*"], &[]);

        pull(&mut state, "style-rust").expect("a name the prefix matches pulls");
        let err = pull(&mut state, "stylist").expect_err("a name the prefix misses is refused");

        assert!(err.starts_with("not-granted:"), "{err}");
        assert!(err.contains("'stylist'"), "{err}");
        assert!(err.contains("capabilities.install"), "{err}");
        assert!(err.contains("style-*"), "{err}");
        assert_eq!(project.registry.resolved(), ["style-rust"]);
        assert!(!project.installed("stylist").exists());
        assert!(project.locked("style-rust"));
        assert!(!project.locked("stylist"));
        assert!(!state
            .installed_artifacts
            .iter()
            .any(|artifact| artifact.name == "stylist"));
    }

    #[test]
    fn a_skill_grant_does_not_admit_a_tool_and_leaves_no_compiled_form() {
        under_scratch_home("inner_a_skill_grant_does_not_admit_a_tool");
    }

    #[test]
    #[ignore = "run by a_skill_grant_does_not_admit_a_tool_and_leaves_no_compiled_form"]
    fn inner_a_skill_grant_does_not_admit_a_tool() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        project.publish_wasm_tool("wasm-tool");
        let mut state = project.session(&["*"], &[]);

        let err = pull(&mut state, "wasm-tool").expect_err("a skill grant admits no tool");

        assert!(err.starts_with("not-granted:"), "{err}");
        assert!(err.contains("'wasm-tool' is a tool"), "{err}");
        assert!(err.contains("capabilities.install.tool"), "{err}");
        assert!(cwasm_entries(&scratch_compiled_dir()).is_empty());
        assert!(tree(&scratch_compiled_dir()).is_empty());
        assert!(!project.installed("wasm-tool").exists());
        assert!(!project.lock_path.exists());
        assert!(state.tool_components.is_empty());
        assert!(state.installed_artifacts.is_empty());
    }

    #[test]
    fn a_tool_grant_admits_wasm_and_native_tools_and_not_skills() {
        under_scratch_home("inner_a_tool_grant_admits_wasm_and_native_tools");
    }

    #[test]
    #[ignore = "run by a_tool_grant_admits_wasm_and_native_tools_and_not_skills"]
    fn inner_a_tool_grant_admits_wasm_and_native_tools() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        project.publish_wasm_tool("wasm-tool");
        project.publish_native_tool("native-tool");
        project.publish_skill("my-skill", "# guidance");
        // `my-skill` is named in the tool list, so it is the tier and not the name that refuses it.
        let mut state = project.session(&[], &["wasm-tool", "native-tool", "my-skill"]);

        pull(&mut state, "wasm-tool").expect("a granted WASM tool pulls");
        assert!(state.tool_components.contains_key("wasm-tool"));
        assert_eq!(cwasm_entries(&scratch_compiled_dir()).len(), 1);

        pull(&mut state, "native-tool").expect("a granted native tool pulls");
        assert!(project
            .installed("native-tool")
            .join("native-tool")
            .is_file());

        let err = pull(&mut state, "my-skill").expect_err("a tool grant admits no skill");
        assert!(err.starts_with("not-granted:"), "{err}");
        assert!(err.contains("'my-skill' is a skill"), "{err}");
        assert!(err.contains("capabilities.install.skill"), "{err}");
        assert!(!project.installed("my-skill").exists());
        assert!(!project.locked("my-skill"));
        assert_eq!(cwasm_entries(&scratch_compiled_dir()).len(), 1);
    }

    #[test]
    fn a_driver_or_hook_is_refused_under_any_grant() {
        under_scratch_home("inner_a_driver_or_hook_is_refused_under_any_grant");
    }

    #[test]
    #[ignore = "run by a_driver_or_hook_is_refused_under_any_grant"]
    fn inner_a_driver_or_hook_is_refused_under_any_grant() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        project.publish_wasm_role("my-driver", "driver");
        project.publish_wasm_role("my-hook", "hook");
        let mut state = project.session(&["*"], &["*"]);

        for (name, role) in [("my-driver", "driver"), ("my-hook", "hook")] {
            let err = pull(&mut state, name).expect_err("a driver or hook is never pullable");
            assert!(err.starts_with("not-granted:"), "{err}");
            assert!(
                err.contains(&format!("'{name}' is a '{role}' artifact")),
                "{err}"
            );
            assert!(err.contains("grants only skills and tools"), "{err}");
            assert!(!project.installed(name).exists());
        }
        assert!(tree(&scratch_compiled_dir()).is_empty());
        assert!(!project.lock_path.exists());
        assert!(state.tool_components.is_empty());
        assert!(state.installed_artifacts.is_empty());
    }

    #[test]
    fn a_name_that_is_not_an_artifact_name_is_refused_under_a_wildcard() {
        let project = Project::new();
        let mut state = project.session(&["*"], &["*"]);
        let before = tree(project._dir.path());

        for name in ["../escape", "github:owner/repo@v1", "Upper", "a/b", ""] {
            let err = pull(&mut state, name).expect_err("a non-name is refused");
            assert!(err.starts_with("not-granted:"), "{name:?}: {err}");
            assert!(err.contains("is not an artifact name"), "{name:?}: {err}");
        }

        assert!(project.registry.resolved().is_empty());
        assert_eq!(tree(project._dir.path()), before);
        assert!(state.installed_artifacts.is_empty());
    }

    #[test]
    fn a_pulled_artifact_gains_no_grant_of_its_own() {
        under_scratch_home("inner_a_pulled_artifact_gains_no_grant_of_its_own");
    }

    #[test]
    #[ignore = "run by a_pulled_artifact_gains_no_grant_of_its_own"]
    fn inner_a_pulled_artifact_gains_no_grant_of_its_own() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        // The artifact's own manifest asks for far more than the capsule holds.
        project.publish(
            "wasm-tool",
            RuntimeType::Wasm,
            "tool",
            zip_with_files(&[
                (
                    PACKED_MANIFEST_ENTRY,
                    b"name: wasm-tool\nversion: 1.0.0\nruntime: tool\ncapabilities:\n  network:\n    \
                      allow: [\"*\"]\n  shell:\n    allow: [bash, curl]\n  install:\n    tool: [\"*\"]\n",
                ),
                (
                    "tool.wasm",
                    &iface_component_bytes("murmur-test:pull/first"),
                ),
            ]),
        );
        let mut state = project.session_with(CapabilityPolicy {
            network_allow: vec!["api.example.com".to_string()],
            install_tool: vec!["wasm-tool".to_string()],
            ..CapabilityPolicy::default()
        });
        let policy_before = state.capability_policy.clone();
        let grants_before: Vec<String> = state.artifact_grants.keys().cloned().collect();
        let allowlist_before = state.allowlisted_tools.clone();

        pull(&mut state, "wasm-tool").expect("a granted tool pulls");

        assert!(state.tool_components.contains_key("wasm-tool"));
        assert_eq!(state.capability_policy, policy_before);
        assert_eq!(
            state.artifact_grants.keys().cloned().collect::<Vec<_>>(),
            grants_before
        );
        assert!(!state.artifact_grants.contains_key("wasm-tool"));
        assert!(state.gateways.for_artifact("wasm-tool").is_none());
        assert!(state.gateways.inference().is_none());
        assert_eq!(state.allowlisted_tools, allowlist_before);
        assert!(!state.allowlisted_tools.contains("wasm-tool"));
    }

    #[test]
    fn a_child_without_the_grant_cannot_pull_what_its_parent_can() {
        let parent = murmur_artifact::RuntimeManifest::from_yaml_str(
            "name: parent\nversion: 0.1.0\ncapabilities:\n  install:\n    skill: [my-skill]\n  \
             spawn:\n    allow: [child]\n",
        )
        .unwrap();
        let child = murmur_artifact::RuntimeManifest::from_yaml_str(
            "name: child\nversion: 0.1.0\ncapabilities:\n  env:\n    allow: []\n",
        )
        .unwrap();

        let parent_project = Project::new();
        parent_project.publish_skill("my-skill", "# guidance");
        let mut parent_state =
            parent_project.session_with(capability_policy_from_runtime_manifest(&parent));
        pull(&mut parent_state, "my-skill").expect("the parent's grant admits my-skill");

        let child_project = Project::new();
        child_project.publish_skill("my-skill", "# guidance");
        let mut child_state =
            child_project.session_with(capability_policy_from_runtime_manifest(&child));
        let err = pull(&mut child_state, "my-skill").expect_err("the child holds no grant");
        assert!(err.starts_with("not-granted:"), "{err}");
        assert!(child_project.registry.resolved().is_empty());
        assert!(!child_project.installed("my-skill").exists());

        assert_eq!(
            SpawnEnvelope::from_runtime_manifest(&parent)
                .contains(&SpawnEnvelope::from_runtime_manifest(&child)),
            Ok(())
        );
    }

    #[test]
    fn diagnostics_states_the_install_grant() {
        let project = Project::new();

        let mut ungranted = project.session(&[], &[]);
        let capabilities = manage::Host::diagnostics(&mut ungranted)
            .unwrap()
            .capabilities;
        assert!(capabilities.contains("pull: not granted"), "{capabilities}");
        assert!(
            capabilities.contains("capabilities.install"),
            "{capabilities}"
        );
        assert!(
            capabilities.ends_with("search and remove are not implemented"),
            "{capabilities}"
        );

        let mut granted = project.session(&["a"], &[]);
        let capabilities = manage::Host::diagnostics(&mut granted)
            .unwrap()
            .capabilities;
        assert_eq!(
            capabilities,
            "install skill: a; install tool: <none>; search and remove are not implemented"
        );
    }

    #[test]
    fn every_refusal_names_what_the_capsule_is_allowed() {
        let policy = CapabilityPolicy {
            install_skill: vec!["code-review".to_string(), "style-*".to_string()],
            install_tool: vec!["jq".to_string()],
            ..CapabilityPolicy::default()
        };
        assert_eq!(
            refuse_before_resolve(&policy, "stylist").unwrap(),
            "not-granted: 'stylist' matches no capabilities.install entry (skill: code-review, \
             style-*; tool: jq)"
        );
        assert_eq!(
            refuse_after_resolve(&policy, "jq", &meta(RuntimeType::Static, "skill")).unwrap(),
            "not-granted: 'jq' is a skill, and capabilities.install.skill does not name it \
             (skill: code-review, style-*; tool: jq)"
        );
        assert_eq!(
            refuse_after_resolve(&policy, "jq", &meta(RuntimeType::Wasm, "driver")).unwrap(),
            "not-granted: 'jq' is a 'driver' artifact (wasm); capabilities.install grants only \
             skills and tools"
        );
        assert_eq!(
            refuse_before_resolve(&CapabilityPolicy::default(), "jq").unwrap(),
            "not-granted: this capsule declares no capabilities.install, so pulling 'jq' is \
             refused"
        );
        assert!(refuse_before_resolve(&policy, "Upper")
            .unwrap()
            .starts_with("not-granted: 'Upper' is not an artifact name ("));
        assert_eq!(refuse_before_resolve(&policy, "style-rust"), None);
        assert_eq!(
            refuse_after_resolve(&policy, "jq", &meta(RuntimeType::Native, "tool")),
            None
        );
    }
}
