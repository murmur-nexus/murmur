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
pub struct LockedArtifact {
    pub name: String,
    pub resolved_version: String,
    pub sha256: LockedSha256,
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
    pub fn pin(&mut self, resolved_version: &str, sha256: LockedSha256) {
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

    /// Pin `name` at `version`/`sha256`, creating the entry when it is not there yet and
    /// merging into it through [`LockedArtifact::pin`] when it is. Every other entry is left
    /// untouched.
    pub fn upsert(&mut self, name: &str, version: &str, sha256: LockedSha256) {
        if let Some(entry) = self.artifacts.iter_mut().find(|entry| entry.name == name) {
            entry.pin(version, sha256);
        } else {
            self.artifacts.push(LockedArtifact {
                name: name.to_string(),
                resolved_version: version.to_string(),
                sha256,
            });
        }
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
        );

        lock.upsert(
            "native-tool",
            "0.1.0",
            LockedSha256::for_one_platform("linux-x86_64", "rebuilt-linux-hash"),
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

        lock.upsert("tool", "0.1.0", LockedSha256::any("wasm-hash"));

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
