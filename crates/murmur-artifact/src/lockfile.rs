use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only `lock_version` this build reads or writes.
///
/// Version 2 pins a hash per platform; version 1 pinned one `sha256.wasm` per artifact and is
/// refused rather than migrated, because accepting it would silently reinstate a single hash for
/// an artifact whose payload differs per platform.
pub const LOCK_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MurmurLock {
    pub lock_version: u32,
    pub artifacts: Vec<LockedArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawLockedArtifact", into = "RawLockedArtifact")]
pub struct LockedArtifact {
    pub name: String,
    pub resolved_version: String,
    pub sha256: LockedSha256,
    pub origin: LockOrigin,
}

/// Who wrote a lock entry's pin.
///
/// On disk this is two flat keys on the entry: `origin` (always written) and `session` (only
/// for [`LockOrigin::Runtime`]). An entry with no `origin` key reads as [`LockOrigin::Operator`],
/// so a lock with no `origin` keys at all reads as entirely operator-declared.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LockOrigin {
    /// Written by an operator command: `mur install`, or the `mur run` / `mur eval` that created
    /// the lock.
    #[default]
    Operator,
    /// Written by a running capsule through `manage.pull()`. `session` is the id of the session
    /// whose store performed the pull — host state, never an argument the guest supplied.
    Runtime { session: String },
}

const ORIGIN_OPERATOR: &str = "operator";
const ORIGIN_RUNTIME: &str = "runtime";

impl LockOrigin {
    /// The lowercase spelling of the entry's `origin` key: `operator` or `runtime`. The same
    /// spelling a trace record carries for an artifact's origin.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Operator => ORIGIN_OPERATOR,
            Self::Runtime { .. } => ORIGIN_RUNTIME,
        }
    }
}

/// The on-disk shape of a [`LockedArtifact`]: `origin` and `session` as flat keys after
/// `sha256`.
#[derive(Serialize, Deserialize)]
struct RawLockedArtifact {
    name: String,
    resolved_version: String,
    sha256: LockedSha256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

impl TryFrom<RawLockedArtifact> for LockedArtifact {
    type Error = String;

    /// Refuses the combinations [`LockOrigin`] cannot hold: an unknown `origin`, `runtime`
    /// without `session`, and `operator` with one. A blank `session` is carried through and
    /// refused by [`MurmurLock::validate`], which the write path runs too.
    fn try_from(raw: RawLockedArtifact) -> Result<Self, Self::Error> {
        let origin = match (raw.origin.as_deref(), raw.session) {
            (None | Some(ORIGIN_OPERATOR), None) => LockOrigin::Operator,
            (None | Some(ORIGIN_OPERATOR), Some(_)) => {
                return Err(format!(
                    "artifact '{}' is origin: operator but carries a session key (only an \
                     origin: runtime entry names a session)",
                    raw.name
                ))
            }
            (Some(ORIGIN_RUNTIME), Some(session)) => LockOrigin::Runtime { session },
            (Some(ORIGIN_RUNTIME), None) => {
                return Err(format!(
                    "artifact '{}' is origin: runtime but has no session key",
                    raw.name
                ))
            }
            (Some(other), _) => {
                return Err(format!(
                    "artifact '{}' has unknown origin '{other}' (expected {ORIGIN_OPERATOR} or \
                     {ORIGIN_RUNTIME})",
                    raw.name
                ))
            }
        };
        Ok(Self {
            name: raw.name,
            resolved_version: raw.resolved_version,
            sha256: raw.sha256,
            origin,
        })
    }
}

impl From<LockedArtifact> for RawLockedArtifact {
    fn from(entry: LockedArtifact) -> Self {
        let (origin, session) = match entry.origin {
            LockOrigin::Operator => (ORIGIN_OPERATOR, None),
            LockOrigin::Runtime { session } => (ORIGIN_RUNTIME, Some(session)),
        };
        Self {
            name: entry.name,
            resolved_version: entry.resolved_version,
            sha256: entry.sha256,
            origin: Some(origin.to_string()),
            session,
        }
    }
}

/// The pinned hash (or hashes) of one artifact's payload.
///
/// Exactly one of the two fields is populated. Which one is a property of the payload, not of
/// the host that wrote the lock: a WASM component or a static skill is one set of bytes every
/// host resolves, and a native tool is a different binary per platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSha256 {
    /// Hash of a platform-independent payload (WASM component, static skill).
    /// Mutually exclusive with [`Self::platforms`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub any: Option<String>,
    /// Hash per platform tag (`"linux-x86_64"` → sha256), for native payloads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub platforms: BTreeMap<String, String>,
}

impl LockedSha256 {
    /// One platform-independent hash, for a WASM or static payload.
    #[must_use]
    pub fn any(sha256: impl Into<String>) -> Self {
        Self {
            any: Some(sha256.into()),
            platforms: BTreeMap::new(),
        }
    }

    /// One hash pinned to one platform tag, for a native payload.
    #[must_use]
    pub fn for_one_platform(platform: &str, sha256: impl Into<String>) -> Self {
        let mut platforms = BTreeMap::new();
        platforms.insert(platform.to_string(), sha256.into());
        Self {
            any: None,
            platforms,
        }
    }

    /// The hash to verify a payload resolved on `platform` against.
    ///
    /// A populated `platforms` map is answered from that map alone — never from [`Self::any`].
    /// Falling back would verify this host's payload against another platform's hash and report
    /// a hash mismatch, which sends the operator to re-publish an artifact that is correct.
    #[must_use]
    pub fn for_platform(&self, platform: &str) -> Option<&str> {
        if self.platforms.is_empty() {
            return self.any.as_deref();
        }
        self.platforms.get(platform).map(String::as_str)
    }

    /// The platform tags this entry does carry, in sorted order, for an error message naming
    /// what a host that found no hash for itself could have used.
    #[must_use]
    pub fn platform_tags(&self) -> Vec<&str> {
        self.platforms.keys().map(String::as_str).collect()
    }

    /// Whether `other` pins the same kind of payload — both platform-independent, or both
    /// per-platform. A shape change means every hash held here describes different bytes.
    fn same_shape_as(&self, other: &Self) -> bool {
        self.platforms.is_empty() == other.platforms.is_empty()
    }

    /// Reject an entry that pins nothing, pins both shapes at once, or pins an empty string.
    /// `artifact_name` names the offending entry in the message.
    fn validate(&self, artifact_name: &str) -> Result<(), LockfileError> {
        match (&self.any, self.platforms.is_empty()) {
            (None, true) => {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{artifact_name}' pins no hash (expected either sha256.any or \
                     sha256.platforms)"
                )))
            }
            (Some(_), false) => {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{artifact_name}' pins both sha256.any and sha256.platforms \
                     (expected exactly one)"
                )))
            }
            _ => {}
        }

        if let Some(any) = &self.any {
            if any.trim().is_empty() {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{artifact_name}' has empty sha256.any"
                )));
            }
        }

        for (platform, sha256) in &self.platforms {
            if platform.trim().is_empty() {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{artifact_name}' has an empty platform key under sha256.platforms"
                )));
            }
            if sha256.trim().is_empty() {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{artifact_name}' has empty sha256.platforms.{platform}"
                )));
            }
        }

        Ok(())
    }
}

impl LockedArtifact {
    /// Merge a freshly resolved pin into this entry.
    ///
    /// Adding a platform to an artifact already pinned for another one must not discard the
    /// hash that other platform verifies against — a lock is shared across the machines that
    /// build against it. So the incoming hashes are merged into the existing map, and the whole
    /// `sha256` is replaced only when everything already in it is stale: a different
    /// `resolved_version`, or a change of shape between platform-independent and per-platform.
    ///
    /// A [`LockOrigin::Operator`] pin makes the entry operator-declared, whatever it was. A
    /// [`LockOrigin::Runtime`] pin leaves the entry's origin as it was: a runtime pull never
    /// demotes an operator pin or relabels an earlier pull with its own session.
    pub fn pin(&mut self, resolved_version: &str, sha256: LockedSha256, origin: LockOrigin) {
        if origin == LockOrigin::Operator {
            self.origin = LockOrigin::Operator;
        }
        let stale =
            self.resolved_version != resolved_version || !self.sha256.same_shape_as(&sha256);
        if stale || sha256.platforms.is_empty() {
            self.sha256 = sha256;
        } else {
            self.sha256.platforms.extend(sha256.platforms);
        }
        self.resolved_version = resolved_version.to_string();
    }

    /// Why this pin disagrees with a freshly resolved `version`/`sha256`, or `None` when they
    /// agree.
    ///
    /// A platform this entry has no key for is not a disagreement — it is a platform that has
    /// not been pinned yet, which is what installing on a second machine produces.
    #[must_use]
    pub fn conflict_with(&self, version: &str, sha256: &LockedSha256) -> Option<String> {
        if self.resolved_version != version {
            return Some(format!(
                "pinned {}@{}, but {}@{version} was resolved",
                self.name, self.resolved_version, self.name
            ));
        }
        if !self.sha256.same_shape_as(sha256) {
            return Some(format!(
                "'{}' is pinned as a {} payload but resolved as a {} one",
                self.name,
                shape_label(&self.sha256),
                shape_label(sha256),
            ));
        }
        if let Some(incoming) = &sha256.any {
            let pinned = self.sha256.any.as_deref()?;
            if pinned != incoming {
                return Some(format!(
                    "'{}' is pinned at sha256 {pinned}, but {incoming} was resolved",
                    self.name
                ));
            }
        }
        for (platform, incoming) in &sha256.platforms {
            let Some(pinned) = self.sha256.platforms.get(platform) else {
                continue;
            };
            if pinned != incoming {
                return Some(format!(
                    "'{}' is pinned at sha256 {pinned} for {platform}, but {incoming} was resolved",
                    self.name
                ));
            }
        }
        None
    }
}

fn shape_label(sha256: &LockedSha256) -> &'static str {
    if sha256.platforms.is_empty() {
        "platform-independent"
    } else {
        "per-platform"
    }
}

#[derive(Debug, Error)]
pub enum LockfileError {
    #[error("murmur.lock not found at {0}")]
    NotFound(String),
    #[error("invalid murmur.lock: {0}")]
    Invalid(String),
    #[error("failed to read murmur.lock at {path}: {source}")]
    ReadIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write murmur.lock at {path}: {source}")]
    WriteIo {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl MurmurLock {
    #[must_use]
    pub fn artifact_for(&self, name: &str) -> Option<&LockedArtifact> {
        self.artifacts.iter().find(|entry| entry.name == name)
    }

    /// Pin `name` at `version`/`sha256`, creating the entry with `origin` when it is not there
    /// yet and merging into it through [`LockedArtifact::pin`] when it is. Every other entry is
    /// left untouched.
    ///
    /// Returns the entry's origin before the call, or `None` when the entry was created — so an
    /// operator command can tell that it adopted a runtime pin.
    pub fn upsert(
        &mut self,
        name: &str,
        version: &str,
        sha256: LockedSha256,
        origin: LockOrigin,
    ) -> Option<LockOrigin> {
        if let Some(entry) = self.artifacts.iter_mut().find(|entry| entry.name == name) {
            let previous = entry.origin.clone();
            entry.pin(version, sha256, origin);
            Some(previous)
        } else {
            self.artifacts.push(LockedArtifact {
                name: name.to_string(),
                resolved_version: version.to_string(),
                sha256,
                origin,
            });
            None
        }
    }

    /// Delete `name`'s entry and return it, or `None` when there is none. Every other entry is
    /// left untouched and in its order, so the rewritten file differs by that entry alone.
    ///
    /// A lock left with no entries is still a valid lock: it serialises as `artifacts: []`.
    pub fn remove(&mut self, name: &str) -> Option<LockedArtifact> {
        let index = self.artifacts.iter().position(|entry| entry.name == name)?;
        Some(self.artifacts.remove(index))
    }

    pub fn validate(&self) -> Result<(), LockfileError> {
        if self.lock_version != LOCK_VERSION {
            return Err(LockfileError::Invalid(format!(
                "unsupported lock_version {} (expected {}) \u{2014} delete murmur.lock and run \
                 `mur install` to regenerate it",
                self.lock_version, LOCK_VERSION
            )));
        }

        for entry in &self.artifacts {
            if entry.name.trim().is_empty() {
                return Err(LockfileError::Invalid(
                    "artifact entry has empty name".to_string(),
                ));
            }
            if entry.resolved_version.trim().is_empty() {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{}' has empty resolved_version",
                    entry.name
                )));
            }
            entry.sha256.validate(&entry.name)?;
            if let LockOrigin::Runtime { session } = &entry.origin {
                if session.trim().is_empty() {
                    return Err(LockfileError::Invalid(format!(
                        "artifact '{}' is origin: runtime with an empty session",
                        entry.name
                    )));
                }
            }
        }

        Ok(())
    }
}

pub fn read_lockfile(path: &Path) -> Result<MurmurLock, LockfileError> {
    let raw = fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            return LockfileError::NotFound(path.display().to_string());
        }

        LockfileError::ReadIo {
            path: path.display().to_string(),
            source,
        }
    })?;

    let lock: MurmurLock = serde_yaml::from_str(&raw)
        .map_err(|err| LockfileError::Invalid(format!("YAML parse error: {err}")))?;

    lock.validate()?;
    Ok(lock)
}

pub fn write_lockfile_atomic(path: &Path, lock: &MurmurLock) -> Result<(), LockfileError> {
    lock.validate()?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LockfileError::WriteIo {
            path: parent.display().to_string(),
            source,
        })?;
    }

    let yaml = serde_yaml::to_string(lock)
        .map_err(|err| LockfileError::Invalid(format!("YAML serialize error: {err}")))?;

    let tmp = temp_path(path);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&tmp)
        .map_err(|source| LockfileError::WriteIo {
            path: tmp.display().to_string(),
            source,
        })?;

    file.write_all(yaml.as_bytes())
        .map_err(|source| LockfileError::WriteIo {
            path: tmp.display().to_string(),
            source,
        })?;
    file.sync_all().map_err(|source| LockfileError::WriteIo {
        path: tmp.display().to_string(),
        source,
    })?;

    fs::rename(&tmp, path).map_err(|source| LockfileError::WriteIo {
        path: format!("{} -> {}", tmp.display(), path.display()),
        source,
    })?;

    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("murmur.lock");

    path.with_file_name(format!(".{file_name}.{nanos}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_with(entries: Vec<LockedArtifact>) -> MurmurLock {
        MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: entries,
        }
    }

    fn entry(name: &str, version: &str, sha256: LockedSha256) -> LockedArtifact {
        LockedArtifact {
            name: name.to_string(),
            resolved_version: version.to_string(),
            sha256,
            origin: LockOrigin::Operator,
        }
    }

    fn runtime(session: &str) -> LockOrigin {
        LockOrigin::Runtime {
            session: session.to_string(),
        }
    }

    /// `as_str` is the spelling the lock file writes, so an entry read back carries the origin
    /// `as_str` named.
    #[test]
    fn lock_origin_as_str_round_trips_through_the_lock_file() {
        assert_eq!(LockOrigin::Operator.as_str(), "operator");
        assert_eq!(runtime("ses_puller").as_str(), "runtime");
        assert_eq!(runtime("").as_str(), "runtime");

        for origin in [LockOrigin::Operator, runtime("ses_puller")] {
            let raw = RawLockedArtifact::from(LockedArtifact {
                origin: origin.clone(),
                ..entry("a", "1.0.0", LockedSha256::any("abc"))
            });
            assert_eq!(raw.origin.as_deref(), Some(origin.as_str()));
            let read_back = LockedArtifact::try_from(raw).unwrap();
            assert_eq!(read_back.origin, origin);
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        let lock = lock_with(vec![
            entry("echo-tool", "0.0.1", LockedSha256::any("abc123")),
            entry(
                "native-tool",
                "0.1.0",
                LockedSha256::for_one_platform("linux-x86_64", "def456"),
            ),
            LockedArtifact {
                origin: runtime("ses_0190a1b2"),
                ..entry("pulled-tool", "1.2.3", LockedSha256::any("fed789"))
            },
        ]);

        write_lockfile_atomic(&path, &lock).unwrap();
        let read_back = read_lockfile(&path).unwrap();
        assert_eq!(read_back, lock);
    }

    /// The serialised shape is what an operator reads and what `mur install` must produce, so it
    /// is asserted directly rather than through a round trip: an absent key and an empty map are
    /// indistinguishable after deserialisation.
    #[test]
    fn only_the_populated_shape_is_serialised() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        write_lockfile_atomic(
            &path,
            &lock_with(vec![
                entry("component-tool", "0.1.0", LockedSha256::any("aaa")),
                entry(
                    "native-tool",
                    "0.1.0",
                    LockedSha256::for_one_platform("linux-x86_64", "bbb"),
                ),
            ]),
        )
        .unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("lock_version: 2"), "{raw}");
        assert!(
            !raw.contains("wasm"),
            "a v1 sha256.wasm key survived: {raw}"
        );
        assert!(raw.contains("any: aaa"), "{raw}");
        assert!(raw.contains("linux-x86_64: bbb"), "{raw}");
        assert_eq!(raw.matches("origin: operator").count(), 2, "{raw}");
        assert!(!raw.contains("session:"), "{raw}");
    }

    #[test]
    fn a_lock_without_origin_reads_as_all_operator() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        fs::write(
            &path,
            "lock_version: 2\nartifacts:\n  - name: echo-tool\n    resolved_version: 0.0.1\n    sha256:\n      any: abc\n  - name: native-tool\n    resolved_version: 0.1.0\n    sha256:\n      platforms:\n        linux-x86_64: def\n",
        )
        .unwrap();

        let lock = read_lockfile(&path).unwrap();
        assert!(lock
            .artifacts
            .iter()
            .all(|entry| entry.origin == LockOrigin::Operator));

        write_lockfile_atomic(&path, &lock).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.matches("origin: operator").count(), 2, "{raw}");
        assert!(!raw.contains("session:"), "{raw}");
    }

    #[test]
    fn a_runtime_upsert_keeps_the_existing_origin() {
        let mut lock = lock_with(vec![
            entry("operator-tool", "0.1.0", LockedSha256::any("aaa")),
            LockedArtifact {
                origin: runtime("ses_first"),
                ..entry("pulled-tool", "0.1.0", LockedSha256::any("bbb"))
            },
        ]);

        let previous = lock.upsert(
            "operator-tool",
            "0.1.0",
            LockedSha256::any("aaa"),
            runtime("ses_second"),
        );
        assert_eq!(previous, Some(LockOrigin::Operator));
        let previous = lock.upsert(
            "pulled-tool",
            "0.2.0",
            LockedSha256::any("ccc"),
            runtime("ses_second"),
        );
        assert_eq!(previous, Some(runtime("ses_first")));

        assert_eq!(
            lock.artifact_for("operator-tool").unwrap().origin,
            LockOrigin::Operator
        );
        let pulled = lock.artifact_for("pulled-tool").unwrap();
        assert_eq!(pulled.origin, runtime("ses_first"));
        assert_eq!(pulled.resolved_version, "0.2.0");

        let created = lock.upsert(
            "new-tool",
            "0.1.0",
            LockedSha256::any("ddd"),
            runtime("ses_second"),
        );
        assert_eq!(created, None);
        assert_eq!(
            lock.artifact_for("new-tool").unwrap().origin,
            runtime("ses_second")
        );
    }

    #[test]
    fn an_operator_upsert_adopts_a_runtime_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        let mut lock = lock_with(vec![LockedArtifact {
            origin: runtime("ses_puller"),
            ..entry("pulled-tool", "0.1.0", LockedSha256::any("aaa"))
        }]);

        let previous = lock.upsert(
            "pulled-tool",
            "0.1.0",
            LockedSha256::any("aaa"),
            LockOrigin::Operator,
        );

        assert_eq!(previous, Some(runtime("ses_puller")));
        assert_eq!(
            lock.artifact_for("pulled-tool").unwrap().origin,
            LockOrigin::Operator
        );
        write_lockfile_atomic(&path, &lock).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("origin: operator"), "{raw}");
        assert!(!raw.contains("session"), "{raw}");
    }

    #[test]
    fn removing_a_lock_entry_leaves_every_other_entry_untouched() {
        let first = entry("operator-tool", "0.1.0", LockedSha256::any("aaa"));
        let middle = LockedArtifact {
            origin: runtime("ses_puller"),
            ..entry("pulled-tool", "1.2.3", LockedSha256::any("bbb"))
        };
        let last = entry(
            "native-tool",
            "0.2.0",
            LockedSha256::for_one_platform("linux-x86_64", "ccc"),
        );
        let mut lock = lock_with(vec![first.clone(), middle.clone(), last.clone()]);

        assert_eq!(lock.remove("pulled-tool"), Some(middle));
        assert_eq!(lock.artifacts, vec![first, last]);
        assert_eq!(lock.remove("absent"), None);
        assert_eq!(lock.artifacts.len(), 2);

        lock.remove("operator-tool").unwrap();
        lock.remove("native-tool").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        write_lockfile_atomic(&path, &lock).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("artifacts: []"), "{raw}");
        assert_eq!(read_lockfile(&path).unwrap(), lock_with(Vec::new()));
    }

    #[test]
    fn malformed_origin_fields_are_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        let refusal = |origin_keys: &str| {
            fs::write(
                &path,
                format!(
                    "lock_version: 2\nartifacts:\n  - name: odd-tool\n    resolved_version: 0.1.0\n    sha256:\n      any: abc\n{origin_keys}"
                ),
            )
            .unwrap();
            match read_lockfile(&path).unwrap_err() {
                LockfileError::Invalid(message) => message,
                other => panic!("expected Invalid, got {other:?}"),
            }
        };

        let message = refusal("    origin: runtime\n");
        assert!(message.contains("'odd-tool'"), "{message}");
        assert!(message.contains("session"), "{message}");

        let message = refusal("    origin: runtime\n    session: \"  \"\n");
        assert!(message.contains("'odd-tool'"), "{message}");
        assert!(message.contains("session"), "{message}");

        let message = refusal("    origin: operator\n    session: ses_x\n");
        assert!(message.contains("'odd-tool'"), "{message}");
        assert!(message.contains("session"), "{message}");

        let message = refusal("    origin: capsule\n");
        assert!(message.contains("'odd-tool'"), "{message}");
        assert!(message.contains("origin"), "{message}");
        assert!(message.contains("capsule"), "{message}");

        let blank_on_write = lock_with(vec![LockedArtifact {
            origin: runtime(""),
            ..entry("odd-tool", "0.1.0", LockedSha256::any("abc"))
        }]);
        assert!(write_lockfile_atomic(&path, &blank_on_write).is_err());
    }

    #[test]
    fn a_version_one_lockfile_is_refused_with_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        fs::write(
            &path,
            "lock_version: 1\nartifacts:\n  - name: echo-tool\n    resolved_version: 0.0.1\n    sha256:\n      wasm: abc\n",
        )
        .unwrap();

        let err = read_lockfile(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("lock_version 1"), "{message}");
        assert!(message.contains("expected 2"), "{message}");
        assert!(message.contains("delete murmur.lock"), "{message}");
        assert!(message.contains("mur install"), "{message}");
    }

    #[test]
    fn an_entry_must_pin_exactly_one_shape() {
        let neither = lock_with(vec![entry(
            "tool",
            "0.1.0",
            LockedSha256 {
                any: None,
                platforms: BTreeMap::new(),
            },
        )]);
        assert!(neither
            .validate()
            .unwrap_err()
            .to_string()
            .contains("pins no hash"));

        let mut both = LockedSha256::any("aaa");
        both.platforms
            .insert("linux-x86_64".to_string(), "bbb".to_string());
        let both = lock_with(vec![entry("tool", "0.1.0", both)]);
        assert!(both
            .validate()
            .unwrap_err()
            .to_string()
            .contains("pins both"));
    }

    #[test]
    fn empty_hashes_and_empty_platform_keys_are_rejected() {
        let empty_any = lock_with(vec![entry("tool", "0.1.0", LockedSha256::any("  "))]);
        assert!(empty_any
            .validate()
            .unwrap_err()
            .to_string()
            .contains("empty sha256.any"));

        let empty_platform_hash = lock_with(vec![entry(
            "tool",
            "0.1.0",
            LockedSha256::for_one_platform("linux-x86_64", ""),
        )]);
        assert!(empty_platform_hash
            .validate()
            .unwrap_err()
            .to_string()
            .contains("empty sha256.platforms.linux-x86_64"));

        let empty_key = lock_with(vec![entry(
            "tool",
            "0.1.0",
            LockedSha256::for_one_platform("  ", "aaa"),
        )]);
        assert!(empty_key
            .validate()
            .unwrap_err()
            .to_string()
            .contains("empty platform key"));
    }

    #[test]
    fn for_platform_answers_a_platform_map_from_that_map_alone() {
        let native = LockedSha256::for_one_platform("darwin-aarch64", "mac-hash");
        assert_eq!(native.for_platform("darwin-aarch64"), Some("mac-hash"));
        // No fallback to `any` — and `any` is not even populated here, which is the invariant.
        assert_eq!(native.for_platform("linux-x86_64"), None);

        let independent = LockedSha256::any("one-hash");
        assert_eq!(independent.for_platform("linux-x86_64"), Some("one-hash"));
        assert_eq!(independent.for_platform("darwin-aarch64"), Some("one-hash"));
    }

    #[test]
    fn a_second_platform_is_added_beside_the_first() {
        let mut lock = lock_with(vec![entry(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
        )]);

        lock.upsert(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("linux-x86_64", "linux-hash"),
            LockOrigin::Operator,
        );

        let pinned = &lock.artifact_for("native-tool").unwrap().sha256;
        assert_eq!(pinned.for_platform("darwin-aarch64"), Some("mac-hash"));
        assert_eq!(pinned.for_platform("linux-x86_64"), Some("linux-hash"));
    }

    #[test]
    fn one_platforms_hash_is_replaced_without_touching_the_others() {
        let mut lock = lock_with(vec![entry(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
        )]);
        lock.upsert(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("linux-x86_64", "linux-hash"),
            LockOrigin::Operator,
        );

        lock.upsert(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("linux-x86_64", "rebuilt-linux-hash"),
            LockOrigin::Operator,
        );

        let pinned = &lock.artifact_for("native-tool").unwrap().sha256;
        assert_eq!(
            pinned.for_platform("linux-x86_64"),
            Some("rebuilt-linux-hash")
        );
        assert_eq!(pinned.for_platform("darwin-aarch64"), Some("mac-hash"));
    }

    #[test]
    fn a_new_version_discards_every_hash_pinned_for_the_old_one() {
        let mut lock = lock_with(vec![entry(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
        )]);

        lock.upsert(
            "native-tool",
            "0.2.0",
            LockedSha256::for_one_platform("linux-x86_64", "linux-hash"),
            LockOrigin::Operator,
        );

        let pinned = &lock.artifact_for("native-tool").unwrap();
        assert_eq!(pinned.resolved_version, "0.2.0");
        assert_eq!(pinned.sha256.platform_tags(), vec!["linux-x86_64"]);
    }

    #[test]
    fn a_shape_change_discards_the_hashes_of_the_old_shape() {
        let mut lock = lock_with(vec![entry(
            "tool",
            "0.1.0",
            LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
        )]);

        lock.upsert(
            "tool",
            "0.1.0",
            LockedSha256::any("wasm-hash"),
            LockOrigin::Operator,
        );

        let pinned = &lock.artifact_for("tool").unwrap().sha256;
        assert_eq!(pinned.any.as_deref(), Some("wasm-hash"));
        assert!(pinned.platforms.is_empty());
    }

    #[test]
    fn an_unpinned_platform_is_not_a_conflict_but_a_differing_hash_is() {
        let pinned = entry(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
        );

        assert_eq!(
            pinned.conflict_with(
                "0.1.0",
                &LockedSha256::for_one_platform("linux-x86_64", "linux-hash")
            ),
            None
        );

        let same_platform_new_hash = pinned
            .conflict_with(
                "0.1.0",
                &LockedSha256::for_one_platform("darwin-aarch64", "other-hash"),
            )
            .unwrap();
        assert!(
            same_platform_new_hash.contains("darwin-aarch64"),
            "{same_platform_new_hash}"
        );

        let new_version = pinned
            .conflict_with(
                "0.2.0",
                &LockedSha256::for_one_platform("darwin-aarch64", "mac-hash"),
            )
            .unwrap();
        assert!(new_version.contains("0.2.0"), "{new_version}");

        let shape_change = pinned
            .conflict_with("0.1.0", &LockedSha256::any("wasm-hash"))
            .unwrap();
        assert!(shape_change.contains("per-platform"), "{shape_change}");
    }
}
