//! The map from an A2A context to the harness session that holds its conversation:
//! `~/.murmur/conversations/<record>/<context-id>/harness-session.json`.
//!
//! On `inference.transport: process` the harness owns the conversation. The runtime keeps no
//! message list for it — [`crate::conversation`] writes no `conversation.jsonl`, and a hook granted
//! `capabilities.conversation.read` reads an empty page — and remembers exactly one thing per
//! context: the opaque session id the harness answers to. That is the whole of this module.
//!
//! Three properties decide the layout:
//!
//! * **Beside the record it replaces.** One file per context, in the directory an `http` capsule
//!   would have put `conversation.jsonl` in, so `<record>` and `<context-id>` mean the same thing
//!   on both transports and `context.record_store` points both at the same place.
//! * **Opaque.** `session_id` is whatever the harness calls its session. `harness` and `driver` are
//!   written for a person reading the directory; nothing compares them. Whether a session still
//!   exists is a question only the harness can answer, and it answers it by failing the turn.
//! * **Never fatal, and never silently discarded.** A root that cannot be resolved or written
//!   costs the conversation its memory beyond the running capsule, reported once and survived. A
//!   session the harness cannot find fails the turn — see [`crate::errors::RuntimeError`]'s
//!   `HarnessSessionGone` — rather than starting a new conversation behind the person's back.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// The map file inside `<record>/<context-id>/`, beside where `conversation.jsonl` would be.
pub(crate) const HARNESS_SESSION_FILE_NAME: &str = "harness-session.json";

/// The `type` value on the map file, and the only value one is recognised by. A file carrying
/// anything else is another tool's, and reads as no entry at all.
pub(crate) const HARNESS_SESSION_TYPE: &str = "murmur.harness-session";

/// One context's harness session, as the file holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HarnessSessionEntry {
    /// Always [`HARNESS_SESSION_TYPE`].
    #[serde(rename = "type")]
    pub kind: String,
    /// What the harness calls this conversation. Opaque to the runtime, which only ever hands it
    /// back.
    pub session_id: String,
    /// The harness the driver's `describe()` named when this entry was written.
    pub harness: String,
    /// The process driver artifact that drove it.
    pub driver: String,
    /// When this context first got a session, in milliseconds since the epoch. Survives every
    /// later write for the same context.
    pub created_ms: u64,
    /// When the entry was last written, in milliseconds since the epoch.
    pub updated_ms: u64,
}

/// What one launch knows about the harness sessions of the contexts it serves.
///
/// Holds the resolved root — `None` for a capsule that keeps no file, which is
/// `context.record: off` or a host with no usable `HOME` — and what this launch has already seen.
/// The in-memory half is what makes a capsule with no root still thread across the tasks of one
/// life: it forgets on stop, which is exactly what "keeps no file" means.
pub(crate) struct HarnessSessionMap {
    root: Option<PathBuf>,
    /// The session workdir every survived failure is reported into, beside stderr.
    workdir: PathBuf,
    /// Context id → the session id this launch last saw for it.
    seen: Mutex<HashMap<String, String>>,
    /// A write failure is reported once and then survived silently: an unwritable home must not
    /// produce one stderr line per task.
    failure_reported: Mutex<bool>,
}

impl HarnessSessionMap {
    pub(crate) fn new(root: Option<PathBuf>, workdir: &Path) -> Self {
        Self {
            root,
            workdir: workdir.to_path_buf(),
            seen: Mutex::new(HashMap::new()),
            failure_reported: Mutex::new(false),
        }
    }

    /// `~/.murmur/conversations/<record>`, or `None` for a launch that remembers only in memory.
    pub(crate) fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// The map file `context_id` is kept in, or `None` when this launch keeps none and when the
    /// id is not a usable path segment.
    ///
    /// A context id arrives from an A2A client as well as from `mur run --context`, so it is
    /// checked here rather than trusted: a rejected id costs that conversation its file and
    /// nothing else.
    pub(crate) fn entry_path(&self, context_id: &str) -> Option<PathBuf> {
        let root = self.root()?;
        if crate::state_store::validate_store_name(context_id).is_err() {
            return None;
        }
        Some(entry_file(root, context_id))
    }

    /// The session id this context already has, or `None` when it has none.
    ///
    /// What this launch has already seen wins over the file: a task that has just been answered
    /// is remembered even on a host where nothing could be written.
    pub(crate) fn get(&self, context_id: &str) -> Option<String> {
        if let Ok(seen) = self.seen.lock() {
            if let Some(id) = seen.get(context_id) {
                return Some(id.clone());
            }
        }
        let entry = read_entry(&self.entry_path(context_id)?)?;
        Some(entry.session_id)
    }

    /// Remember `session_id` for `context_id`, in memory and — when this launch has a root — in
    /// its file. An empty id is not a session and is ignored.
    ///
    /// `created_ms` is carried over from whatever the file already held, so the first time a
    /// context got a session survives every later turn in it.
    pub(crate) fn put(&self, context_id: &str, session_id: &str, harness: &str, driver: &str) {
        if session_id.is_empty() {
            return;
        }
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert(context_id.to_string(), session_id.to_string());
        }
        let Some(path) = self.entry_path(context_id) else {
            return;
        };
        let now_ms = crate::retention::now_ms();
        let entry = HarnessSessionEntry {
            kind: HARNESS_SESSION_TYPE.to_string(),
            session_id: session_id.to_string(),
            harness: harness.to_string(),
            driver: driver.to_string(),
            created_ms: read_entry(&path).map(|e| e.created_ms).unwrap_or(now_ms),
            updated_ms: now_ms,
        };
        if let Err(reason) = write_entry(&path, &entry) {
            self.report_once(&format!(
                "[harness-session] the session id for context '{context_id}' could not be written \
                 to {} ({reason}); this conversation is remembered only until this capsule stops",
                path.display()
            ));
        }
    }

    fn report_once(&self, message: &str) {
        if let Ok(mut reported) = self.failure_reported.lock() {
            if *reported {
                return;
            }
            *reported = true;
        }
        crate::conversation::report(&self.workdir, message);
    }
}

/// `~/.murmur/conversations/<record>` for a launch whose harness owns the conversation, or `None`
/// when nothing is kept on disk.
///
/// Three ways to have none, and all three are ordinary: a capsule that is not on
/// `transport: process` (whose conversation the runtime records itself — see
/// [`crate::runtime`]'s `resolve_conversation_root`), `context.record: off`, and a host whose home
/// directory cannot be resolved. The last is reported and survived rather than refused: a host
/// with no usable `HOME` must not become one where nothing launches.
pub(crate) fn resolve_harness_session_root(
    context: Option<&murmur_artifact::ContextConfig>,
    capsule_name: &str,
    inference: &murmur_artifact::InferenceConfig,
    workdir: &Path,
) -> Option<PathBuf> {
    if inference.transport != "process" {
        return None;
    }
    let record = crate::conversation::resolve_record_name(context, capsule_name)?;
    match crate::conversation::record_root(&record) {
        Ok(root) => Some(root),
        Err(reason) => {
            crate::conversation::report(
                workdir,
                &format!(
                    "[harness-session] the conversation root for '{record}' could not be located \
                     ({reason}); this capsule's conversations last only as long as it runs"
                ),
            );
            None
        }
    }
}

/// The map file one context keeps under `root`. Resolves only — nothing is created and nothing is
/// checked for existence — so a launch that never establishes a session leaves no directory
/// behind.
///
/// `mur run --resume` asks whether this path exists before staging goes any further, which is the
/// one question that separates "continue this conversation" from "start a new one and say nothing".
pub(crate) fn entry_file(root: &Path, context_id: &str) -> PathBuf {
    root.join(context_id).join(HARNESS_SESSION_FILE_NAME)
}

/// Mint the id a fresh conversation is offered to the harness under.
///
/// A bare hyphenated UUID, with none of the runtime's own `ses_` / `ctx_` prefixes: a harness that
/// validates a caller-chosen session id expects a UUID, and one that mints its own says so with a
/// `session-started` event that replaces this.
pub(crate) fn new_harness_session_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// What `path` holds, or `None` for a file that does not exist, will not parse, is not a map file,
/// or names no session.
///
/// Every one of those reads as "this context has no session", which starts a new conversation
/// rather than handing the harness something it cannot use.
fn read_entry(path: &Path) -> Option<HarnessSessionEntry> {
    let raw = std::fs::read(path).ok()?;
    let entry: HarnessSessionEntry = serde_json::from_slice(&raw).ok()?;
    if entry.kind != HARNESS_SESSION_TYPE || entry.session_id.is_empty() {
        return None;
    }
    Some(entry)
}

/// Write `entry` whole, at `0600` under directories held at `0700`.
fn write_entry(path: &Path, entry: &HarnessSessionEntry) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    crate::conversation::ensure_context_dirs(dir)?;
    let contents = serde_json::to_vec(entry)
        .map_err(|err| format!("failed to encode the harness session entry: {err}"))?;
    crate::murmur_home::write_private_file(path, &contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::murmur_home::{mode_of, run_with_home};

    const HARNESS: &str = "fixture-harness";
    const DRIVER: &str = "fixture-process-driver";

    fn map_in(home: &Path) -> HarnessSessionMap {
        HarnessSessionMap::new(
            Some(home.join(".murmur/conversations/capsule")),
            &home.join("session"),
        )
    }

    #[test]
    fn a_minted_session_id_is_a_bare_uuid() {
        let id = new_harness_session_id();
        assert_eq!(id.len(), 36, "{id}");
        assert!(
            id.chars()
                .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{id}"
        );
        assert_ne!(id, new_harness_session_id());
    }

    #[test]
    fn entry_file_sits_beside_the_record() {
        assert_eq!(
            entry_file(Path::new("/root"), "ctx_1"),
            Path::new("/root/ctx_1/harness-session.json")
        );
    }

    #[test]
    fn a_context_id_that_is_not_a_path_segment_gets_no_file() {
        let map = HarnessSessionMap::new(Some(PathBuf::from("/root")), Path::new("/tmp"));
        assert_eq!(map.entry_path("../escape"), None);
        assert_eq!(map.entry_path(".hidden"), None);
        assert!(map.entry_path("ctx_1").is_some());
    }

    #[test]
    fn a_launch_with_no_root_still_threads_in_memory() {
        let map = HarnessSessionMap::new(None, Path::new("/tmp"));
        assert_eq!(map.root(), None);
        assert_eq!(map.entry_path("ctx_1"), None);
        map.put("ctx_1", "sess-1", HARNESS, DRIVER);
        assert_eq!(map.get("ctx_1").as_deref(), Some("sess-1"));
        assert_eq!(map.get("ctx_2"), None);
    }

    #[test]
    fn an_empty_session_id_is_not_remembered() {
        let map = HarnessSessionMap::new(None, Path::new("/tmp"));
        map.put("ctx_1", "", HARNESS, DRIVER);
        assert_eq!(map.get("ctx_1"), None);
    }

    #[test]
    fn harness_session_round_trip_through_a_home() {
        let home = tempfile::tempdir().unwrap();
        run_with_home(
            "harness_session::tests::harness_session_round_trip_inner",
            home.path(),
        );
    }

    #[test]
    #[ignore = "run by harness_session_round_trip_through_a_home"]
    fn harness_session_round_trip_inner() {
        if !crate::murmur_home::in_scratch_home() {
            return;
        }
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let map = map_in(&home);
        map.put("ctx_1", "sess-1", HARNESS, DRIVER);

        let path = map.entry_path("ctx_1").unwrap();
        let entry = read_entry(&path).expect("the entry is readable");
        assert_eq!(entry.kind, HARNESS_SESSION_TYPE);
        assert_eq!(entry.session_id, "sess-1");
        assert_eq!(entry.harness, HARNESS);
        assert_eq!(entry.driver, DRIVER);
        assert_eq!(entry.created_ms, entry.updated_ms);

        assert_eq!(mode_of(&path), 0o600);
        for dir in [
            path.parent().unwrap(),
            path.parent().unwrap().parent().unwrap(),
            path.parent().unwrap().parent().unwrap().parent().unwrap(),
        ] {
            assert_eq!(mode_of(dir), 0o700, "{}", dir.display());
        }

        // A second launch reads what the first wrote, with no memory of its own.
        let reread = map_in(&home);
        assert_eq!(reread.get("ctx_1").as_deref(), Some("sess-1"));

        // A later write keeps the moment the context first got a session.
        std::thread::sleep(std::time::Duration::from_millis(2));
        reread.put("ctx_1", "sess-2", HARNESS, DRIVER);
        let updated = read_entry(&path).unwrap();
        assert_eq!(updated.session_id, "sess-2");
        assert_eq!(updated.created_ms, entry.created_ms);
        assert!(updated.updated_ms > entry.created_ms);
    }

    #[test]
    fn a_file_that_is_not_a_map_entry_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HARNESS_SESSION_FILE_NAME);

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(read_entry(&path), None);

        std::fs::write(&path, r#"{"type":"something.else","session_id":"s"}"#).unwrap();
        assert_eq!(read_entry(&path), None);

        std::fs::write(
            &path,
            r#"{"type":"murmur.harness-session","session_id":"","harness":"h","driver":"d","created_ms":1,"updated_ms":1}"#,
        )
        .unwrap();
        assert_eq!(read_entry(&path), None);

        assert_eq!(read_entry(&dir.path().join("absent.json")), None);
    }

    #[test]
    fn an_unwritable_root_is_reported_once_and_survived() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the conversation root has to be a directory: every write beneath it fails.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let workdir = dir.path().join("session");
        std::fs::create_dir_all(&workdir).unwrap();

        let map = HarnessSessionMap::new(Some(blocked.clone()), &workdir);
        map.put("ctx_1", "sess-1", HARNESS, DRIVER);
        map.put("ctx_1", "sess-2", HARNESS, DRIVER);

        // Nothing landed, the launch is still usable, and the log carries exactly one line.
        assert!(!blocked.is_dir());
        assert_eq!(map.get("ctx_1").as_deref(), Some("sess-2"));
        let log = std::fs::read_to_string(workdir.join("logs/bootstrap.log")).unwrap_or_default();
        assert_eq!(
            log.matches("[harness-session]").count(),
            1,
            "one report, not one per task:\n{log}"
        );
    }
}
