//! Durable, capsule-scoped state stores: `~/.murmur/state/<store>/`, granted per artifact by
//! `capabilities.state` and mounted into a guest as a second WASI preopen named
//! [`STATE_PREOPEN_NAME`].
//!
//! Two properties decide the layout, and both are load-bearing:
//!
//! * **Outside every workdir.** A subdirectory of the session workdir is reachable by any artifact
//!   that holds the workdir preopen, and `capabilities.filesystem.scope` cannot protect it —
//!   `scope` is a single path prefix, so hiding one subtree from one artifact would mean narrowing
//!   every *other* artifact to some sibling prefix. A path the workdir preopen cannot name is the
//!   only version of this that holds.
//! * **Keyed by capsule, not by workdir.** A workdir-keyed store would be an undeclared sharing
//!   channel: two capsules launched in the same directory would read each other's state with no
//!   grant on either side. Sharing between capsules goes over A2A, with a grant on both ends, or
//!   it does not happen. A capsule wanting per-project notes writes them into the workdir — the
//!   workdir already *is* the project. `state/` is for what outlives one.
//!
//! Resolution and creation are deliberately separate calls. [`state_store_reports`] resolves and
//! validates without touching the filesystem, so `mur run --explain-scope` can print exactly what
//! a launch would open while remaining the read-only diagnostic it claims to be; only
//! [`ensure_state_store`], on the real staging path, creates anything.

use std::path::{Component, Path, PathBuf};

use crate::{containment::StateStoreReport, errors::RuntimeError};

/// The guest path a granted state directory is preopened at. A guest reaches its store by writing
/// `state/<file>` — a relative path resolved against this preopen rather than against the workdir
/// preopen mounted as `"."`.
///
/// One stable literal, shared by both `build_wasi_ctx` functions and by the docs, so a guest never
/// has to know which role it is running as to find its own store.
pub const STATE_PREOPEN_NAME: &str = "state";

/// Directory under the murmur home that holds every store, one subdirectory per store name.
const STATE_ROOT_DIR: &str = "state";

/// Mode applied to the state root and to each store directory: owner-only, because a store is
/// exactly the durable half of one capsule's private working set.
///
/// Every platform murmur targets is Unix, so this is stated once here rather than behind a `cfg`
/// fork — a future non-Unix target has one call site to handle, not one per directory.
const STATE_DIR_MODE: u32 = 0o700;

/// Whether `store` is a single usable directory segment under `~/.murmur/state/`.
///
/// A store name is one path segment: non-empty, no `/`, no `.` or `..` component, not absolute,
/// and not beginning with `.`. The rule is deliberately stricter than
/// [`crate::network_policy::validate_filesystem_scope`]'s, which permits nesting: a store name is
/// an *identifier* for a capsule's state, and a nested one would let two declarations that read
/// differently (`a/b` and `a`) resolve into overlapping trees.
///
/// The leading-dot ban subsumes `.` and `..`, but is kept as its own arm because the hazard is
/// wider than escaping: a store called `.config` is a hidden directory under a root an operator
/// inspects with `ls`, which is not somewhere state should be able to put itself.
pub fn validate_store_name(store: &str) -> Result<(), RuntimeError> {
    let refuse = |message: &str| {
        Err(RuntimeError::InvalidStateStore {
            store: store.to_string(),
            message: message.to_string(),
        })
    };

    if store.is_empty() {
        return refuse("a store name must not be empty");
    }
    if store.contains('/') {
        return refuse("a store name is a single path segment and must not contain '/'");
    }
    if store.starts_with('.') {
        return refuse("a store name must not begin with '.'");
    }

    let path = Path::new(store);
    if path.is_absolute() {
        return refuse("a store name is a single path segment and must not be absolute");
    }

    let mut segments = path.components();
    match (segments.next(), segments.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        // `.` and `..` are already caught by the leading-dot arm; this arm catches whatever a
        // future platform's path grammar admits (a prefix, a bare root) rather than letting it
        // through by omission.
        _ => refuse("a store name is a single path segment and must not contain '.' or '..'"),
    }
}

/// Resolve `store`, create both the state root and the store directory at `0700`, and return the
/// store's host path.
///
/// The only function here with side effects, and it is called only from the staging path — never
/// from a diagnostic. A capsule that fails to launch for any other reason therefore leaves no
/// directory behind, and an undeclared capability creates nothing at all.
pub fn ensure_state_store(store: &str) -> Result<PathBuf, RuntimeError> {
    ensure_state_store_in(&murmur_home(store)?, store)
}

/// Every state store the given artifacts declare, resolved and validated but not created.
///
/// Takes `(artifact name, that artifact's operator-declared capabilities)` pairs rather than a
/// concrete artifact type so the two callers that must agree can both satisfy it: `stage_session`
/// holds [`crate::types::ArtifactRequest`]s, and `mur run --explain-scope` holds
/// [`murmur_artifact::RuntimeArtifact`]s and has staged nothing. One resolution shared by both is
/// what makes the report a description of the launch rather than a second opinion about it.
///
/// `capsule_name` supplies the default store name. It comes from the capsule operator's own
/// manifest, never from an artifact's bundled one, so a tool pulled from a registry under a
/// familiar name cannot claim a store that already holds someone else's state.
///
/// The home directory is looked up only once a declaration is actually found, so a capsule that
/// declares no state anywhere still reports (and still launches) on a host where `HOME` is unset.
pub fn state_store_reports<'a, I>(
    artifacts: I,
    capsule_name: &str,
) -> Result<Vec<StateStoreReport>, RuntimeError>
where
    I: IntoIterator<Item = (&'a str, Option<&'a murmur_artifact::Capabilities>)>,
{
    let mut reports = Vec::new();
    let mut home: Option<PathBuf> = None;

    for (name, capabilities) in artifacts {
        let Some(state) = capabilities.and_then(|capabilities| capabilities.state.as_ref()) else {
            continue;
        };
        let store = resolve_store_name(state, capsule_name);
        // Name shape first: a malformed declaration is refused identically on a host that cannot
        // resolve a home directory at all, so the operator is told about the line they wrote
        // rather than about their environment.
        validate_store_name(&store)?;
        let home = match home.as_deref() {
            Some(home) => home.to_path_buf(),
            None => home.insert(murmur_home(&store)?).clone(),
        };
        reports.push(StateStoreReport {
            artifact: name.to_string(),
            host_path: state_store_path_in(&home, &store)?.display().to_string(),
            store,
        });
    }

    Ok(reports)
}

/// The store name a declaration resolves to: what it named, or the capsule name when it named
/// nothing. Shared by the reporting path and by both grant `derive` functions so a report can
/// never name a different directory than the one a guest is handed.
pub(crate) fn resolve_store_name(
    state: &murmur_artifact::StateCapabilities,
    capsule_name: &str,
) -> String {
    state
        .store
        .clone()
        .unwrap_or_else(|| capsule_name.to_string())
}

/// Where `store` lives under a supplied murmur home. Resolves only — nothing is created and
/// nothing is checked for existence — so `mur run --explain-scope` can print the path a launch
/// would open without becoming a command that has side effects.
///
/// `store` is validated here rather than assumed, so no caller can turn an unvalidated name into
/// a path by reaching for this function instead of [`validate_store_name`].
fn state_store_path_in(murmur_home: &Path, store: &str) -> Result<PathBuf, RuntimeError> {
    validate_store_name(store)?;
    Ok(murmur_home.join(STATE_ROOT_DIR).join(store))
}

/// [`ensure_state_store`] against a supplied murmur home rather than one read from the
/// environment — the seam the unit tests resolve through, so none of them has to mutate the test
/// process's `HOME` and race every other test in the binary.
fn ensure_state_store_in(murmur_home: &Path, store: &str) -> Result<PathBuf, RuntimeError> {
    let store_path = state_store_path_in(murmur_home, store)?;
    // The root is created (and its mode asserted) before the store beneath it, so a store
    // directory is never reachable through a root a wider mode left open.
    let root = store_path
        .parent()
        .expect("a store path always has the state root as its parent");
    create_private_dir(store, root)?;
    create_private_dir(store, &store_path)?;
    Ok(store_path)
}

/// `~/.murmur`, or a refusal naming the store that needed it.
///
/// Reported as [`RuntimeError::StateStoreUnavailable`] rather than as a generic home-directory
/// error because the operator's question is "why did my capsule not launch?", and the answer is
/// the pairing of the two: a declaration that needs a durable location, and a host that cannot say
/// where the user's home is.
fn murmur_home(store: &str) -> Result<PathBuf, RuntimeError> {
    murmur_home_dir().map_err(|message| RuntimeError::StateStoreUnavailable {
        store: store.to_string(),
        path: "~/.murmur".to_string(),
        message,
    })
}

/// `~/.murmur`, or why it could not be resolved.
///
/// The one home-directory lookup in the runtime, shared with [`crate::conversation`], so a host
/// whose `HOME` is unusable says the same thing about a state store and about a conversation
/// record. Each caller wraps the reason in its own error type.
pub(crate) fn murmur_home_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            "the home directory could not be resolved: HOME is not set in the environment"
                .to_string()
        })?;

    if !home.is_absolute() {
        return Err(
            "the home directory could not be resolved: HOME is not an absolute path".to_string(),
        );
    }

    Ok(home.join(".murmur"))
}

/// Create `path` if missing and hold it at [`STATE_DIR_MODE`].
///
/// The mode is applied whether or not this call created the directory: `0700` is what a store
/// *is*, so a run must not silently continue against one an earlier umask, a restore or a stray
/// `chmod` left group- or world-readable.
fn create_private_dir(store: &str, path: &Path) -> Result<(), RuntimeError> {
    ensure_private_dir(path, STATE_DIR_MODE).map_err(|message| {
        RuntimeError::StateStoreUnavailable {
            store: store.to_string(),
            path: path.display().to_string(),
            message,
        }
    })
}

/// Create `path` if missing and hold it at `mode`, reporting why it could not be done.
///
/// The one owner-only-directory helper in the runtime, shared with [`crate::conversation`]. The
/// mode is applied whether or not this call created the directory: an earlier umask, a restore or
/// a stray `chmod` must not leave a private directory group- or world-readable. Each caller wraps
/// the reason in its own error type.
pub(crate) fn ensure_private_dir(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)
        .map_err(|err| format!("failed to create the directory: {err}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|err| format!("failed to set mode {mode:04o}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{Capabilities, StateCapabilities};

    fn state_capabilities(store: Option<&str>) -> Capabilities {
        Capabilities {
            network: None,
            peer_fetch: None,
            filesystem: None,
            shell: None,
            spawn: None,
            env: None,
            limits: None,
            resources: None,
            state: Some(StateCapabilities {
                store: store.map(str::to_string),
            }),
            task_io: None,
            conversation: None,
            containment: None,
        }
    }

    #[test]
    fn a_single_segment_is_the_only_accepted_store_name() {
        for store in ["shey", "murmur-tool-corpus", "notes_2026", "a"] {
            validate_store_name(store).unwrap_or_else(|err| panic!("{store} must be valid: {err}"));
        }
    }

    /// Every rejection names the offending value, because the operator's next action is to find
    /// that string in their own manifest.
    #[test]
    fn a_store_name_that_is_not_a_single_segment_is_refused_by_name() {
        for store in [
            "",
            "../escape",
            "/abs/path",
            "a/b",
            ".",
            "..",
            ".hidden",
            "a/",
        ] {
            let err = match validate_store_name(store) {
                Ok(()) => panic!("'{store}' must be refused"),
                Err(err) => err,
            };
            assert!(
                matches!(err, RuntimeError::InvalidStateStore { .. }),
                "'{store}' must refuse as InvalidStateStore, got: {err}"
            );
            assert!(
                err.to_string().contains(&format!("'{store}'")),
                "the refusal must quote the offending value: {err}"
            );
        }
    }

    /// Resolution creates nothing. `--explain-scope` prints host paths and must not be a command
    /// that leaves a directory tree behind.
    #[test]
    fn resolving_a_path_creates_nothing() {
        let home = tempfile::tempdir().unwrap();
        let murmur_home = home.path().join(".murmur");

        let path = state_store_path_in(&murmur_home, "shey").unwrap();

        assert_eq!(path, murmur_home.join("state/shey"));
        assert!(!murmur_home.exists());
    }

    #[test]
    fn ensuring_a_store_creates_the_root_and_the_store_at_0700() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let murmur_home = home.path().join(".murmur");

        let path = ensure_state_store_in(&murmur_home, "shey").unwrap();
        let root = murmur_home.join("state");

        assert_eq!(path, root.join("shey"));
        for dir in [&root, &path] {
            assert!(dir.is_dir(), "{} must exist", dir.display());
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o700,
                "{} must be 0700, got {mode:04o}",
                dir.display()
            );
        }

        // Idempotent, and state written by an earlier launch survives a later one.
        std::fs::write(path.join("notes.jsonl"), b"one\n").unwrap();
        assert_eq!(ensure_state_store_in(&murmur_home, "shey").unwrap(), path);
        assert_eq!(
            std::fs::read_to_string(path.join("notes.jsonl")).unwrap(),
            "one\n"
        );
    }

    /// A store directory a previous run (or a restore) left group-readable is brought back to
    /// `0700` rather than used as found.
    #[test]
    fn ensuring_a_store_reasserts_the_mode_on_an_existing_directory() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let murmur_home = home.path().join(".murmur");
        let path = murmur_home.join("state/shey");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_state_store_in(&murmur_home, "shey").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// Two capsules, one host: each name resolves to its own directory, and neither can read the
    /// other's. The isolation is the naming, so it is asserted at the naming layer too.
    #[test]
    fn two_store_names_never_resolve_into_one_directory() {
        let home = tempfile::tempdir().unwrap();
        let murmur_home = home.path().join(".murmur");

        let first = ensure_state_store_in(&murmur_home, "capsule-one").unwrap();
        let second = ensure_state_store_in(&murmur_home, "capsule-two").unwrap();

        assert_ne!(first, second);
        std::fs::write(first.join("notes.jsonl"), b"one\n").unwrap();
        assert!(!second.join("notes.jsonl").exists());
    }

    #[test]
    fn an_unresolvable_home_names_the_store_that_needed_it() {
        let err = RuntimeError::StateStoreUnavailable {
            store: "shey".to_string(),
            path: "~/.murmur".to_string(),
            message: "the home directory could not be resolved: HOME is not set in the environment"
                .to_string(),
        };
        let message = err.to_string();
        assert!(message.contains("shey"), "must name the store: {message}");
        assert!(
            message.contains("home directory could not be resolved"),
            "must say what could not be resolved: {message}"
        );
    }

    /// The name check runs before the host is consulted, so a malformed declaration is refused as
    /// `InvalidStateStore` even when the home lookup would also have failed.
    #[test]
    fn a_malformed_name_is_refused_before_the_home_lookup() {
        let declared = state_capabilities(Some("a/b"));
        assert!(matches!(
            state_store_reports([("notes-tool", Some(&declared))], "state-capsule"),
            Err(RuntimeError::InvalidStateStore { .. })
        ));
    }

    #[test]
    fn reports_default_the_store_name_to_the_capsule_name() {
        let declared = state_capabilities(None);
        let named = state_capabilities(Some("shey"));

        let reports = state_store_reports(
            [
                ("notes-tool", Some(&declared)),
                ("corpus-tool", Some(&named)),
                ("plain-tool", None),
            ],
            "state-capsule",
        )
        .unwrap();

        assert_eq!(reports.len(), 2, "an undeclared artifact reports nothing");
        assert_eq!(reports[0].artifact, "notes-tool");
        assert_eq!(reports[0].store, "state-capsule");
        assert!(reports[0]
            .host_path
            .ends_with(".murmur/state/state-capsule"));
        assert_eq!(reports[1].artifact, "corpus-tool");
        assert_eq!(reports[1].store, "shey");
        assert!(reports[1].host_path.ends_with(".murmur/state/shey"));
    }

    /// No declaration anywhere means no home lookup, so the report is empty rather than a refusal
    /// on a host that cannot resolve one. This is the half that proves an absent-`HOME` failure
    /// comes from the declaration and not from the variable.
    #[test]
    fn an_undeclared_artifact_set_reports_nothing_and_resolves_no_home() {
        let plain = Capabilities {
            state: None,
            ..state_capabilities(None)
        };
        assert!(state_store_reports(
            [("plain-tool", Some(&plain)), ("bare-tool", None)],
            "capsule"
        )
        .unwrap()
        .is_empty());
    }
}
