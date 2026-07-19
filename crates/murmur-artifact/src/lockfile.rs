use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LOCK_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedSha256 {
    pub wasm: String,
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

    pub fn validate(&self) -> Result<(), LockfileError> {
        if self.lock_version != LOCK_VERSION {
            return Err(LockfileError::Invalid(format!(
                "unsupported lock_version {} (expected {})",
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
            if entry.sha256.wasm.trim().is_empty() {
                return Err(LockfileError::Invalid(format!(
                    "artifact '{}' has empty sha256.wasm",
                    entry.name
                )));
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

    #[test]
    fn write_then_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("murmur.lock");
        let lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: vec![LockedArtifact {
                name: "echo-tool".to_string(),
                resolved_version: "0.0.1".to_string(),
                sha256: LockedSha256 {
                    wasm: "abc123".to_string(),
                },
            }],
        };

        write_lockfile_atomic(&path, &lock).unwrap();
        let read_back = read_lockfile(&path).unwrap();
        assert_eq!(read_back, lock);
    }
}
