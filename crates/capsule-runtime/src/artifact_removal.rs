//! The bound on `murmur:artifact-manager/manage.remove`: a capsule removes only an artifact it
//! pulled.
//!
//! # The bound
//!
//! An artifact is removable when all three hold:
//!
//! - the session's [`InstalledArtifactSummary::origin`] is [`LockOrigin::Runtime`];
//! - the `murmur.lock` entry, read at the moment of removal, is [`LockOrigin::Runtime`];
//! - `murmur.yaml` does not declare the name.
//!
//! The origin says who pinned an artifact; the declaration says the next launch depends on it.
//! Deleting the lock entry of a declared artifact makes the operator's next `mur run` fail with
//! `E-RUN-003`, so a declared artifact is refused whatever its origin. The session record covers
//! a pull that matched an existing operator pin; the lock read covers `mur install` adopting the
//! pin while the session runs. Neither is scoped to this session: an undeclared runtime entry an
//! earlier session pulled, and this one pulled again, is removable. A hook or a driver is fixed at
//! launch and is refused on its role alone, whatever its origin: a policy hook vanishing
//! mid-session is privilege escalation, and a driver vanishing is fatal.
//!
//! Every refusal is a string beginning [`NOT_REMOVABLE`], or [`NOT_GRANTED`] for a capsule with
//! no `capabilities.install` or a name outside the artifact-name rule: the same token and
//! wording `manage.pull` refuses with. A refusal is a value the guest branches on, never a trap, and is
//! decided before anything changes. A name the session has not installed, including a
//! `shell.allow` binary `manage.list` shows, is `Ok(false)` rather than a refusal.
//!
//! # Order of effects
//!
//! After [`refuse_before_lock`] and [`refuse_against_lock`] pass, `manage::Host::remove` writes
//! `murmur.lock` without the entry, then updates the session (`tool_components`,
//! `installed_artifacts`, `removed_artifacts`, `installed_generation`), then deletes
//! `workdir/tools/<name>/`. A lock write that fails leaves everything unchanged. A directory that
//! cannot be deleted is reported with [`undeletable_directory`], and the name is already out of
//! the session and the lock, so dispatch refuses it. The compiled form under
//! `~/.murmur/compiled` is shared across sessions and kept, so pulling the same version again is
//! warm; `mur install --prune` removes stale forms.
//!
//! # In-flight calls
//!
//! `manage` is linked only on the script-capsule path, where the guest is the only caller and
//! `remove` runs on `&mut CapsuleStoreState`. Every dispatch borrows that store for its whole
//! duration, so no call of any tool is in flight on it when `remove` runs. An instance already
//! built holds its own reference-counted `Component`; removal applies to calls that start after
//! it. A `remove` reachable from the agent path, where dispatches run concurrently on a shared
//! store, has no such guarantee and must decide what an in-flight call of the removed name sees.
//!
//! # What the model and the guest see
//!
//! The removed name is recorded in `removed_artifacts`, and `invoke()` and the agent-loop dispatch
//! answer a call to it with [`called_after_removal`]. A native tool shadows a same-named
//! `shell.allow` binary, because the native branch dispatches first; removal hands the name back
//! to the shell branch, the state before the pull.
//!
//! Conversation history is never rewritten. Every earlier `tool_use` and `tool_result`, in the
//! history, the conversation record and the trace, records what happened, and rewriting them
//! would break the durable record and the prompt-cache prefix. A held tool array is not shrunk by
//! `remove`: removal moves `installed_generation`, and the array drops the name at the next
//! rebuild `inference.tool_refresh` releases, because the rebuild reads `workdir/tools/`.
//!
//! No trace line records a removal, and the WIT signature `remove(name) -> result<bool, string>`
//! is unchanged.

use std::collections::HashSet;
use std::path::Path;

use murmur_artifact::{artifact_name_format_error, ArtifactRuntime, LockOrigin, MurmurLock};

use crate::install_grant::{self, NOT_GRANTED};
use crate::types::{CapabilityPolicy, InstalledArtifactSummary};

/// The leading token of every refusal this module returns for an artifact the capsule may not
/// remove.
pub(crate) const NOT_REMOVABLE: &str = "not-removable:";

/// The refusal for removing `name` decided on session state alone, or the installed entry to go
/// on and check against `murmur.lock`. `Ok(None)` means the session has not installed `name` and
/// `remove` answers `Ok(false)`.
///
/// Checks, in order: the session holds an install grant; `name` is an artifact name; the session
/// has installed `name`; it is not a hook or a driver; `murmur.yaml` does not declare it; the
/// session's record of its origin is runtime.
pub(crate) fn refuse_before_lock<'a>(
    policy: &CapabilityPolicy,
    name: &str,
    installed: &'a [InstalledArtifactSummary],
    declared: &HashSet<String>,
) -> Result<Option<&'a InstalledArtifactSummary>, String> {
    if !install_grant::is_granted(policy) {
        return Err(format!(
            "{NOT_GRANTED} this capsule declares no capabilities.install, so removing '{name}' is \
             refused"
        ));
    }
    if let Some(reason) = artifact_name_format_error(name) {
        return Err(format!(
            "{NOT_GRANTED} '{name}' is not an artifact name ({reason}); capabilities.install \
             matches bare artifact names only"
        ));
    }
    let Some(summary) = installed.iter().find(|artifact| artifact.name == name) else {
        return Ok(None);
    };
    let fixed_role = match summary.runtime {
        ArtifactRuntime::Hook => Some("hook"),
        ArtifactRuntime::Driver => Some("driver"),
        ArtifactRuntime::Tool | ArtifactRuntime::Skill => None,
    };
    if let Some(role) = fixed_role {
        return Err(format!(
            "{NOT_REMOVABLE} '{name}' is a {role}; hooks and drivers are fixed at launch and \
             cannot be removed while the session runs"
        ));
    }
    if declared.contains(name) {
        return Err(format!(
            "{NOT_REMOVABLE} '{name}' is declared in murmur.yaml; an artifact the operator \
             declared stays for the life of the session"
        ));
    }
    if summary.origin == LockOrigin::Operator {
        return Err(operator_pinned(name));
    }
    Ok(Some(summary))
}

/// The refusal for removing `name` decided on `murmur.lock` as read at the moment of removal, or
/// `None` to remove it. A missing lock file is passed as a lock with no entries.
///
/// Refuses a missing entry, since then nothing records that the capsule pulled the artifact, and
/// an entry whose origin is operator.
pub(crate) fn refuse_against_lock(name: &str, lock: &MurmurLock) -> Option<String> {
    let Some(entry) = lock.artifact_for(name) else {
        return Some(format!(
            "{NOT_REMOVABLE} murmur.lock has no entry for '{name}', so nothing records that this \
             capsule pulled it"
        ));
    };
    (entry.origin == LockOrigin::Operator).then(|| operator_pinned(name))
}

/// What `invoke()` and the agent-loop dispatch return for a call to a name this session removed.
pub(crate) fn called_after_removal(name: &str) -> String {
    format!(
        "tool '{name}' was removed from this session by manage.remove; it can no longer be called"
    )
}

/// The error for a removal whose lock write and session update succeeded but whose directory
/// could not be deleted.
pub(crate) fn undeletable_directory(name: &str, dir: &Path, err: &std::io::Error) -> String {
    format!(
        "'{name}' is removed from this session and from murmur.lock, but {} could not be \
         deleted: {err}",
        dir.display()
    )
}

fn operator_pinned(name: &str) -> String {
    format!(
        "{NOT_REMOVABLE} murmur.lock pins '{name}' with origin: operator; only an artifact this \
         capsule pulled (origin: runtime) can be removed"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;

    use murmur_artifact::{
        read_lockfile, write_lockfile_atomic, ArtifactImplementation, ArtifactRuntime, LockOrigin,
        LockedArtifact, LockedSha256, MurmurLock, LOCK_VERSION, PACKED_MANIFEST_ENTRY,
    };

    use super::*;
    use crate::agent::inventory::build_tool_inventory;
    use crate::bindings::host::murmur::{
        artifact_manager::manage, tool::run::ToolInput, tool_registry::invoke,
    };
    use crate::hooks::ResolvedCall;
    use crate::install_grant::{pull, tree, Project, VERSION};
    use crate::runtime::{cwasm_entries, scratch_compiled_dir, CapsuleStoreState};

    const SKILL: &str = "late-skill";
    const SKILL_TEXT: &str = "# late guidance";

    /// Runs `artifact_removal::tests::<inner>` in a child process whose `HOME` is a fresh
    /// tempdir, so the compiled-form cache under `$HOME/.murmur/compiled` is a scratch one.
    fn under_scratch_home(inner: &str) {
        let home = tempfile::tempdir().unwrap();
        crate::murmur_home::run_with_home(
            &format!("artifact_removal::tests::{inner}"),
            home.path(),
        );
    }

    fn remove(state: &mut CapsuleStoreState, name: &str) -> Result<bool, String> {
        manage::Host::remove(state, name.to_string())
    }

    fn input() -> ToolInput {
        ToolInput {
            data: Some(r#"{"command":"--version"}"#.to_string()),
            log_path: None,
        }
    }

    fn listed(state: &mut CapsuleStoreState) -> Vec<String> {
        manage::Host::list(state)
            .into_iter()
            .map(|summary| summary.name)
            .collect()
    }

    fn lock_bytes(project: &Project) -> Option<Vec<u8>> {
        fs::read(&project.lock_path).ok()
    }

    /// Everything a removal can change: the workdir, the lock file and the session's record.
    #[derive(Debug, PartialEq)]
    struct Snapshot {
        tree: Vec<PathBuf>,
        lock: Option<Vec<u8>>,
        installed: Vec<InstalledArtifactSummary>,
        components: Vec<String>,
        removed: HashSet<String>,
        generation: u64,
    }

    fn snapshot(project: &Project, state: &CapsuleStoreState) -> Snapshot {
        let mut components: Vec<String> = state.tool_components.keys().cloned().collect();
        components.sort();
        Snapshot {
            tree: tree(&project.workdir),
            lock: lock_bytes(project),
            installed: state.installed_artifacts.clone(),
            components,
            removed: state.removed_artifacts.clone(),
            generation: state.installed_generation,
        }
    }

    async fn dispatch(state: &CapsuleStoreState, name: &str) -> Result<String, String> {
        state
            .dispatch_agent_tool_async(name, input(), None)
            .await
            .map(|outcome| outcome.result.data.unwrap_or_default())
    }

    /// Asserts that `state` and `project` hold no trace of `name`: no directory, no lock entry,
    /// no installed entry, not listed, not described.
    fn assert_gone(project: &Project, state: &mut CapsuleStoreState, name: &str) {
        assert!(!project.installed(name).exists());
        assert!(!project.locked(name));
        assert!(state.installed_artifacts.iter().all(|a| a.name != name));
        assert!(!listed(state).contains(&name.to_string()));
        let err = manage::Host::describe(state, name.to_string())
            .expect_err("a removed artifact is not described");
        assert!(err.contains("is not installed"), "{err}");
        assert!(state.removed_artifacts.contains(name));
    }

    fn summary(
        name: &str,
        runtime: ArtifactRuntime,
        origin: LockOrigin,
    ) -> InstalledArtifactSummary {
        InstalledArtifactSummary {
            name: name.to_string(),
            version: VERSION.to_string(),
            runtime,
            implementation: Some(ArtifactImplementation::Wasm),
            origin,
        }
    }

    fn runtime_origin(session: &str) -> LockOrigin {
        LockOrigin::Runtime {
            session: session.to_string(),
        }
    }

    /// Installs each of `artifacts` in `state` as staging would: a directory with its manifest, a
    /// `murmur.lock` entry of the same origin, and an installed entry.
    fn stage(
        project: &Project,
        state: &mut CapsuleStoreState,
        artifacts: &[InstalledArtifactSummary],
    ) {
        let mut lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: Vec::new(),
        };
        for artifact in artifacts {
            let dir = project.installed(&artifact.name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(PACKED_MANIFEST_ENTRY),
                format!(
                    "name: {}\nversion: {VERSION}\nruntime: tool\n",
                    artifact.name
                ),
            )
            .unwrap();
            lock.artifacts.push(LockedArtifact {
                name: artifact.name.clone(),
                resolved_version: VERSION.to_string(),
                sha256: LockedSha256::any("aaa"),
                origin: artifact.origin.clone(),
            });
            state.installed_artifacts.push(artifact.clone());
        }
        write_lockfile_atomic(&project.lock_path, &lock).unwrap();
    }

    fn operator_pinned_refusal(name: &str) -> String {
        format!(
            "not-removable: murmur.lock pins '{name}' with origin: operator; only an artifact \
             this capsule pulled (origin: runtime) can be removed"
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pulled_skill_is_removed_from_every_place_it_was_installed() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        let mut state = project.session(&[SKILL], &[]);
        pull(&mut state, SKILL).expect("a granted skill pulls");
        assert!(dispatch(&state, SKILL).await.unwrap().contains(SKILL_TEXT));
        let generation = state.installed_generation;

        assert_eq!(remove(&mut state, SKILL), Ok(true));

        assert_gone(&project, &mut state, SKILL);
        assert_eq!(state.installed_generation, generation + 1);
    }

    #[test]
    fn a_pulled_wasm_tool_is_removed_and_its_component_dropped() {
        under_scratch_home("inner_a_pulled_wasm_tool_is_removed_and_its_component_dropped");
    }

    #[test]
    #[ignore = "run by a_pulled_wasm_tool_is_removed_and_its_component_dropped"]
    fn inner_a_pulled_wasm_tool_is_removed_and_its_component_dropped() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        project.publish_wasm_tool("wasm-tool");
        let mut state = project.session(&[], &["wasm-tool"]);
        pull(&mut state, "wasm-tool").expect("a granted WASM tool pulls");
        assert!(state.tool_components.contains_key("wasm-tool"));
        let sha256 = read_lockfile(&project.lock_path)
            .unwrap()
            .artifact_for("wasm-tool")
            .unwrap()
            .sha256
            .any
            .clone()
            .unwrap();

        assert_eq!(remove(&mut state, "wasm-tool"), Ok(true));

        assert_gone(&project, &mut state, "wasm-tool");
        assert!(!state.tool_components.contains_key("wasm-tool"));
        let forms = cwasm_entries(&scratch_compiled_dir());
        assert_eq!(forms.len(), 1, "{forms:?}");
        let form = forms[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(form.starts_with(&format!("{sha256}-")), "{form}");
    }

    #[test]
    fn a_pulled_native_tool_is_removed_and_its_binary_deleted() {
        let project = Project::new();
        project.publish_native_tool("native-tool");
        let mut state = project.session(&[], &["native-tool"]);
        pull(&mut state, "native-tool").expect("a granted native tool pulls");
        let binary = project.installed("native-tool").join("native-tool");
        assert!(binary.is_file());

        assert_eq!(remove(&mut state, "native-tool"), Ok(true));

        assert!(!binary.exists());
        assert_gone(&project, &mut state, "native-tool");
    }

    #[test]
    fn a_removed_artifact_cannot_be_called() {
        under_scratch_home("inner_a_removed_artifact_cannot_be_called");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "run by a_removed_artifact_cannot_be_called"]
    async fn inner_a_removed_artifact_cannot_be_called() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        project.publish_wasm_tool("wasm-tool");
        let mut state = project.session(&[SKILL], &["wasm-tool"]);
        pull(&mut state, SKILL).unwrap();
        pull(&mut state, "wasm-tool").unwrap();
        state.allowlisted_tools.insert("wasm-tool".to_string());

        for name in [SKILL, "wasm-tool"] {
            assert_eq!(remove(&mut state, name), Ok(true));
            let refusal = format!(
                "tool '{name}' was removed from this session by manage.remove; it can no longer \
                 be called"
            );
            assert_eq!(dispatch(&state, name).await, Err(refusal.clone()));
            assert_eq!(
                invoke::Host::invoke(&mut state, name.to_string(), input()).map(|_| ()),
                Err(refusal)
            );
        }
    }

    #[test]
    fn a_removed_artifact_is_absent_from_a_rebuilt_inventory() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        let existing = project.installed("zzz-existing-tool");
        fs::create_dir_all(&existing).unwrap();
        fs::write(
            existing.join(PACKED_MANIFEST_ENTRY),
            "name: zzz-existing-tool\nversion: 1.0.0\nruntime: tool\n",
        )
        .unwrap();
        let mut state = project.session(&[SKILL], &[]);
        let names = |state: &CapsuleStoreState| -> Vec<String> {
            crate::agent::inventory::tool_names(&build_tool_inventory(&state.workdir, None))
        };

        pull(&mut state, SKILL).unwrap();
        assert_eq!(names(&state), vec![SKILL, "zzz-existing-tool"]);

        assert_eq!(remove(&mut state, SKILL), Ok(true));
        assert_eq!(names(&state), vec!["zzz-existing-tool"]);
    }

    #[test]
    fn a_shell_binary_shadowed_by_a_pulled_tool_is_reachable_again_after_removal() {
        let project = Project::new();
        project.publish_native_tool("shadow-tool");
        let mut state = project.session(&[], &["shadow-tool"]);
        state.capability_policy.shell_allow = vec!["shadow-tool".to_string()];

        pull(&mut state, "shadow-tool").unwrap();
        assert!(matches!(
            state.resolve_call("shadow-tool", &input()),
            ResolvedCall::Tool { .. }
        ));

        assert_eq!(remove(&mut state, "shadow-tool"), Ok(true));
        match state.resolve_call("shadow-tool", &input()) {
            ResolvedCall::Shell { argv, .. } => assert_eq!(argv, vec!["--version"]),
            ResolvedCall::Tool { .. } => panic!("the shell branch is reachable again"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_operator_pinned_artifact_cannot_be_removed() {
        let project = Project::new();
        let sha256 = project.publish_skill(SKILL, SKILL_TEXT);
        write_lockfile_atomic(
            &project.lock_path,
            &MurmurLock {
                lock_version: LOCK_VERSION,
                artifacts: vec![LockedArtifact {
                    name: SKILL.to_string(),
                    resolved_version: VERSION.to_string(),
                    sha256: LockedSha256::any(sha256),
                    origin: LockOrigin::Operator,
                }],
            },
        )
        .unwrap();
        let mut state = project.session(&[SKILL], &[]);
        pull(&mut state, SKILL).expect("a pull matching the operator pin succeeds");
        let before = snapshot(&project, &state);

        assert_eq!(
            remove(&mut state, SKILL),
            Err(operator_pinned_refusal(SKILL))
        );

        assert_eq!(snapshot(&project, &state), before);
        assert!(dispatch(&state, SKILL).await.unwrap().contains(SKILL_TEXT));
    }

    #[test]
    fn an_artifact_declared_in_murmur_yaml_cannot_be_removed_whatever_its_pin_origin() {
        let project = Project::new();
        let mut state = project.session(&["*"], &["*"]);
        stage(
            &project,
            &mut state,
            &[
                summary("decl-op", ArtifactRuntime::Tool, LockOrigin::Operator),
                summary("decl-rt", ArtifactRuntime::Tool, runtime_origin("ses_prev")),
            ],
        );
        state.declared_artifacts = ["decl-op", "decl-rt"].map(String::from).into();
        let before = snapshot(&project, &state);

        for name in ["decl-op", "decl-rt"] {
            assert_eq!(
                remove(&mut state, name),
                Err(format!(
                    "not-removable: '{name}' is declared in murmur.yaml; an artifact the \
                     operator declared stays for the life of the session"
                ))
            );
        }
        assert_eq!(snapshot(&project, &state), before);
    }

    #[test]
    fn a_hook_or_driver_cannot_be_removed() {
        let project = Project::new();
        let mut state = project.session(&["*"], &["*"]);
        let cases = [
            (
                "decl-hook",
                ArtifactRuntime::Hook,
                LockOrigin::Operator,
                "hook",
            ),
            (
                "decl-driver",
                ArtifactRuntime::Driver,
                LockOrigin::Operator,
                "driver",
            ),
            (
                "rt-hook",
                ArtifactRuntime::Hook,
                runtime_origin("ses_test"),
                "hook",
            ),
            (
                "rt-driver",
                ArtifactRuntime::Driver,
                runtime_origin("ses_test"),
                "driver",
            ),
        ];
        let artifacts: Vec<_> = cases
            .iter()
            .map(|(name, runtime, origin, _)| summary(name, runtime.clone(), origin.clone()))
            .collect();
        stage(&project, &mut state, &artifacts);
        state.declared_artifacts = ["decl-hook", "decl-driver"].map(String::from).into();
        let before = snapshot(&project, &state);

        for (name, _, _, role) in cases {
            assert_eq!(
                remove(&mut state, name),
                Err(format!(
                    "not-removable: '{name}' is a {role}; hooks and drivers are fixed at launch \
                     and cannot be removed while the session runs"
                ))
            );
        }
        assert_eq!(snapshot(&project, &state), before);
    }

    #[test]
    fn a_capsule_without_an_install_grant_cannot_remove() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        let mut state = project.session(&[SKILL], &[]);
        pull(&mut state, SKILL).unwrap();
        let granted = state.capability_policy.clone();

        state.capability_policy = CapabilityPolicy::default();
        let before = snapshot(&project, &state);
        assert_eq!(
            remove(&mut state, SKILL),
            Err(format!(
                "not-granted: this capsule declares no capabilities.install, so removing \
                 '{SKILL}' is refused"
            ))
        );
        assert_eq!(snapshot(&project, &state), before);

        state.capability_policy = granted;
        let err = remove(&mut state, "../x").expect_err("a path is not an artifact name");
        assert!(
            err.starts_with("not-granted: '../x' is not an artifact name ("),
            "{err}"
        );
        assert_eq!(snapshot(&project, &state), before);
    }

    #[test]
    fn removing_what_is_not_installed_changes_nothing() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        let mut state = project.session(&[SKILL], &[]);
        state.capability_policy.shell_allow = vec!["some-binary".to_string()];
        pull(&mut state, SKILL).unwrap();
        assert!(listed(&mut state).contains(&"some-binary".to_string()));

        for name in ["never-pulled", "some-binary"] {
            let before = snapshot(&project, &state);
            assert_eq!(remove(&mut state, name), Ok(false));
            assert_eq!(snapshot(&project, &state), before);
        }

        assert_eq!(remove(&mut state, SKILL), Ok(true));
        let before = snapshot(&project, &state);
        assert_eq!(remove(&mut state, SKILL), Ok(false));
        assert_eq!(snapshot(&project, &state), before);
    }

    #[test]
    fn the_lock_decides_at_removal_time() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        let mut state = project.session(&[SKILL], &[]);
        pull(&mut state, SKILL).unwrap();
        let pulled = read_lockfile(&project.lock_path).unwrap();

        let mut adopted = pulled.clone();
        adopted.artifacts[0].origin = LockOrigin::Operator;
        write_lockfile_atomic(&project.lock_path, &adopted).unwrap();
        let before = snapshot(&project, &state);
        assert_eq!(
            remove(&mut state, SKILL),
            Err(operator_pinned_refusal(SKILL))
        );
        assert_eq!(snapshot(&project, &state), before);

        let mut dropped = pulled;
        dropped.remove(SKILL);
        write_lockfile_atomic(&project.lock_path, &dropped).unwrap();
        let before = snapshot(&project, &state);
        assert_eq!(
            remove(&mut state, SKILL),
            Err(format!(
                "not-removable: murmur.lock has no entry for '{SKILL}', so nothing records that \
                 this capsule pulled it"
            ))
        );
        assert_eq!(snapshot(&project, &state), before);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_removed_runtime_artifact_can_be_pulled_again_at_another_version() {
        let project = Project::new();
        project.publish_skill(SKILL, SKILL_TEXT);
        project.publish_skill_at(SKILL, "2.0.0", "# second guidance");
        let mut state = project.session(&[SKILL], &[]);
        pull(&mut state, SKILL).unwrap();
        assert_eq!(remove(&mut state, SKILL), Ok(true));

        manage::Host::pull(&mut state, SKILL.to_string(), "2.0.0".to_string())
            .expect("a removed runtime artifact pulls again");

        let lock = read_lockfile(&project.lock_path).unwrap();
        let entry = lock.artifact_for(SKILL).unwrap();
        assert_eq!(entry.resolved_version, "2.0.0");
        assert_eq!(entry.origin, runtime_origin("ses_test"));
        let installed: Vec<_> = state
            .installed_artifacts
            .iter()
            .filter(|a| a.name == SKILL)
            .map(|a| a.version.as_str())
            .collect();
        assert_eq!(installed, vec!["2.0.0"]);
        assert!(!state.removed_artifacts.contains(SKILL));
        assert!(dispatch(&state, SKILL)
            .await
            .unwrap()
            .contains("# second guidance"));
    }
}
