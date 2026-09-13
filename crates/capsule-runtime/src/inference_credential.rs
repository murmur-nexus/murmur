//! The inference credential: the value the gateway attaches to each `transport: http` driver
//! request, and where it comes from.
//!
//! A `${NAME}` in `inference.api_key` is looked up at staging, first in the global config's
//! `credentials:` map and then in the launching environment. A value from the config stays
//! re-readable for the whole session: every keyed request stats the file, and a changed stamp
//! re-reads it, so `mur config set -g credentials.NAME` reaches a running capsule on its next
//! request. A value from the environment or written literally in the manifest is read once.
//!
//! Only the `mur` runtime process reads the credentials file — never a guest, tool or subprocess.
//! Nothing here writes a value, a hash of one, its length or any prefix to the trace, stderr, an
//! error or `Debug` output.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use murmur_artifact::ApiKeyReference;
use serde::Deserialize;

use crate::{errors::RuntimeError, trace::ResourceTraceAppender};

/// The diagnostic code a session fails with when the provider keeps rejecting its credential.
pub(crate) const E_RUN_026: &str = "E-RUN-026";

/// Where a session's inference credential comes from. Carries names and paths, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// `credentials.<name>` in the global config file at `path`. Re-read when the file changes.
    Config { path: PathBuf, name: String },
    /// The launching environment's variable `name`. Read once at launch.
    Environment { name: String },
    /// A literal `inference.api_key` in the manifest. Read once at launch.
    ManifestLiteral,
}

impl CredentialSource {
    /// Names the source for an operator, without the value.
    pub fn label(&self) -> String {
        match self {
            Self::Config { path, name } => {
                format!("credentials.{name} in {}", path.display())
            }
            Self::Environment { name } => {
                format!("from the environment variable {name}, which is read once at launch")
            }
            Self::ManifestLiteral => "written literally as inference.api_key in murmur.yaml, \
                                      which is read once at launch"
                .to_string(),
        }
    }

    /// The value of `session_start.credential_source` and `inference_credential.source`.
    pub fn trace_name(&self) -> &'static str {
        match self {
            Self::Config { .. } => "config",
            Self::Environment { .. } => "environment",
            Self::ManifestLiteral => "manifest",
        }
    }

    /// The credential name `${NAME}` referenced, or `None` for a manifest literal.
    pub fn credential_name(&self) -> Option<&str> {
        match self {
            Self::Config { name, .. } | Self::Environment { name } => Some(name),
            Self::ManifestLiteral => None,
        }
    }
}

/// A provider rejection still waiting for the agent loop or a hook's `run-inference` to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CredentialRejection {
    pub(crate) status: u16,
    /// Whether the rejected request was the one resend made after a re-read found a new value.
    pub(crate) retried: bool,
}

/// What an `inference_credential` trace event records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialChange {
    /// A re-read found a different value. `trigger` is `"file_changed"` or `"rejection"`.
    Rotated { trigger: &'static str },
    /// The provider answered a request carrying the credential with `status`.
    Rejected { status: u16, retried: bool },
    /// The config could not supply a value; the last good one stays in use.
    Unreadable { reason: &'static str },
}

/// The config file's identity and last change, as `stat(2)` reports it. Any difference means the
/// file may hold something else. `Unavailable` is a file that could not be stat'd or opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStamp {
    Unavailable,
    Present {
        dev: u64,
        ino: u64,
        len: u64,
        mtime: (i64, i64),
        ctime: (i64, i64),
    },
}

impl FileStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        Self::Present {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    fn stat(path: &Path) -> Self {
        fs::metadata(path)
            .map(|metadata| Self::of(&metadata))
            .unwrap_or(Self::Unavailable)
    }
}

/// The only part of the config file this module reads.
#[derive(Deserialize)]
struct CredentialsFile {
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

enum EntryRead {
    Value(String),
    /// One of `missing`, `unreadable`, `unparseable`, `no_entry`, `empty_entry`.
    Unreadable(&'static str),
}

/// Opens `path`, stamps the open handle and reads `credentials.<name>` from it, so the stamp and
/// the value describe the same file even when it is replaced mid-read.
fn read_entry(path: &Path, name: &str) -> (FileStamp, EntryRead) {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (FileStamp::Unavailable, EntryRead::Unreadable("missing"))
        }
        Err(_) => return (FileStamp::Unavailable, EntryRead::Unreadable("unreadable")),
    };
    let stamp = file
        .metadata()
        .map(|metadata| FileStamp::of(&metadata))
        .unwrap_or(FileStamp::Unavailable);
    let mut text = String::new();
    if file.read_to_string(&mut text).is_err() {
        return (stamp, EntryRead::Unreadable("unreadable"));
    }
    let Ok(parsed) = serde_yaml::from_str::<CredentialsFile>(&text) else {
        return (stamp, EntryRead::Unreadable("unparseable"));
    };
    let read = match parsed.credentials.get(name) {
        None => EntryRead::Unreadable("no_entry"),
        Some(value) if value.trim().is_empty() => EntryRead::Unreadable("empty_entry"),
        Some(value) => EntryRead::Value(value.clone()),
    };
    (stamp, read)
}

/// Whether the config file at `path` holds a non-empty `credentials.<name>`. Reads nothing else,
/// and returns nothing of the value.
pub(crate) fn config_holds_credential(path: &Path, name: &str) -> bool {
    matches!(read_entry(path, name).1, EntryRead::Value(_))
}

struct CredentialState {
    /// The last good value. Never cleared: an unreadable source keeps it.
    value: String,
    /// The stamp of the file `value` was last checked against. `None` for a source with no file.
    stamp: Option<FileStamp>,
    /// The stamp an `unreadable` event was last written for, so one broken file is reported once.
    unreadable_reported_for: Option<FileStamp>,
    pending_rejection: Option<CredentialRejection>,
}

/// One session's inference credential, shared by the gateway and the code that reports a
/// rejection.
pub(crate) struct InferenceCredential {
    source: CredentialSource,
    state: Mutex<CredentialState>,
    /// The session trace's `O_APPEND` handle, set once the trace is open. Events are written only
    /// when it is set.
    trace: OnceLock<Arc<ResourceTraceAppender>>,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for InferenceCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceCredential")
            .field("source", &self.source)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl InferenceCredential {
    /// Resolves `reference` in precedence order: a non-empty `credentials.<NAME>` in
    /// `credentials_file`, then a non-empty environment variable `NAME`. A literal is used as is.
    ///
    /// Refuses with [`RuntimeError::InferenceCredentialNotFound`] when neither place holds a
    /// `${NAME}`.
    pub(crate) fn resolve(
        reference: &ApiKeyReference,
        credentials_file: Option<&Path>,
    ) -> Result<Self, RuntimeError> {
        let name = match reference {
            ApiKeyReference::Literal(value) => {
                return Ok(Self::new(
                    CredentialSource::ManifestLiteral,
                    value.clone(),
                    None,
                ))
            }
            ApiKeyReference::Environment(name) => name,
        };
        if let Some(path) = credentials_file {
            let (stamp, read) = read_entry(path, name);
            if let EntryRead::Value(value) = read {
                let credential = Self::new(
                    CredentialSource::Config {
                        path: path.to_path_buf(),
                        name: name.clone(),
                    },
                    value,
                    Some(stamp),
                );
                credential.count_read();
                return Ok(credential);
            }
        }
        match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(Self::new(
                CredentialSource::Environment { name: name.clone() },
                value,
                None,
            )),
            _ => Err(RuntimeError::InferenceCredentialNotFound {
                variable: name.clone(),
                credentials_file: credentials_file.map(Path::to_path_buf),
            }),
        }
    }

    fn new(source: CredentialSource, value: String, stamp: Option<FileStamp>) -> Self {
        Self {
            source,
            state: Mutex::new(CredentialState {
                value,
                stamp,
                unreadable_reported_for: None,
                pending_rejection: None,
            }),
            trace: OnceLock::new(),
            #[cfg(test)]
            reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn source(&self) -> &CredentialSource {
        &self.source
    }

    /// Hands the credential the session trace's appender. A second call is ignored.
    pub(crate) fn attach_trace(&self, trace: Arc<ResourceTraceAppender>) {
        let _ = self.trace.set(trace);
    }

    /// The value to attach to the next request. For a config source, stats the file and re-reads
    /// it only when its stamp differs from the one last seen.
    pub(crate) async fn current(&self) -> String {
        self.refresh(false).await
    }

    /// Re-reads a config source whatever its stamp says, after the provider rejected the value.
    /// A same-size in-place edit within one timestamp tick is seen here even though
    /// [`Self::current`] missed it.
    pub(crate) async fn reread_after_rejection(&self) -> String {
        self.refresh(true).await
    }

    async fn refresh(&self, after_rejection: bool) -> String {
        let (value, change) = self.refresh_locked(after_rejection);
        if let Some(change) = change {
            if let CredentialChange::Unreadable { reason } = change {
                eprintln!(
                    "[capsule-runtime] warning: the inference credential {} could not be read \
                     ({reason}); the value read before stays in use",
                    self.source.label()
                );
            }
            self.write_event(change).await;
        }
        value
    }

    fn refresh_locked(&self, after_rejection: bool) -> (String, Option<CredentialChange>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let CredentialSource::Config { path, name } = &self.source else {
            return (state.value.clone(), None);
        };
        if !after_rejection && state.stamp == Some(FileStamp::stat(path)) {
            return (state.value.clone(), None);
        }
        self.count_read();
        let (stamp, read) = read_entry(path, name);
        state.stamp = Some(stamp);
        let change = match read {
            EntryRead::Value(value) => {
                state.unreadable_reported_for = None;
                if value == state.value {
                    None
                } else {
                    state.value = value;
                    Some(CredentialChange::Rotated {
                        trigger: if after_rejection {
                            "rejection"
                        } else {
                            "file_changed"
                        },
                    })
                }
            }
            EntryRead::Unreadable(reason) => {
                if state.unreadable_reported_for == Some(stamp) {
                    None
                } else {
                    state.unreadable_reported_for = Some(stamp);
                    Some(CredentialChange::Unreadable { reason })
                }
            }
        };
        (state.value.clone(), change)
    }

    /// Records that the provider rejected a request carrying this credential, for the failure
    /// report to pick up, and writes the `rejected` event.
    pub(crate) async fn record_rejection(&self, status: u16, retried: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_rejection = Some(CredentialRejection { status, retried });
        self.write_event(CredentialChange::Rejected { status, retried })
            .await;
    }

    /// Forgets a pending rejection once a later request carrying the credential was accepted, so
    /// an unrelated driver error is not reported as a credential failure.
    pub(crate) fn clear_rejection(&self) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_rejection = None;
    }

    pub(crate) fn take_rejection(&self) -> Option<CredentialRejection> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_rejection
            .take()
    }

    /// The failure message for `rejection`, without the code.
    pub(crate) fn rejection_message(&self, rejection: CredentialRejection) -> String {
        let mut message = format!(
            "the provider rejected the inference credential {} (HTTP {})",
            self.source.label(),
            rejection.status
        );
        if rejection.retried {
            message.push_str(
                "; the credential was re-read after the first rejection and its new value was \
                 rejected too",
            );
        }
        message
    }

    /// What the operator does about a rejection from this source.
    pub(crate) fn rejection_hint(&self) -> String {
        match &self.source {
            CredentialSource::Config { name, .. } => format!(
                "replace it with `mur config set -g credentials.{name} <key>`; running capsules \
                 use it on their next call"
            ),
            other => {
                let name = other.credential_name().unwrap_or("<NAME>");
                format!(
                    "this capsule reads its key only at launch; restart it with a valid key, or \
                     store the key with `mur config set -g credentials.{name} <key>`"
                )
            }
        }
    }

    /// Takes a pending rejection and, when there was one, prints `E-RUN-026` and its hint to
    /// stderr and returns the message for `out/result.txt`.
    pub(crate) fn report_rejection(&self) -> Option<String> {
        let rejection = self.take_rejection()?;
        let message = self.rejection_message(rejection);
        eprintln!("error[{E_RUN_026}]: {message}");
        eprintln!("  hint: {}", self.rejection_hint());
        Some(message)
    }

    async fn write_event(&self, change: CredentialChange) {
        if let Some(trace) = self.trace.get() {
            trace
                .write_inference_credential(
                    self.source.trace_name(),
                    self.source.credential_name(),
                    change,
                )
                .await;
        }
    }

    fn count_read(&self) {
        #[cfg(test)]
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    const OLD: &str = "unit-credential-old-marker";
    const NEW: &str = "unit-credential-new-marker";
    const NAME: &str = "UNIT_CREDENTIAL_KEY";

    fn write_config(path: &Path, value: &str) {
        fs::write(
            path,
            format!("registry:\n  default: official\ncredentials:\n  {NAME}: {value}\n"),
        )
        .unwrap();
    }

    /// Replaces the file by rename, as `mur config set` does.
    fn replace_config(path: &Path, value: &str) {
        let tmp = path.with_file_name(".config.yaml.tmp");
        write_config(&tmp, value);
        fs::rename(tmp, path).unwrap();
    }

    fn config_credential(dir: &tempfile::TempDir) -> (PathBuf, InferenceCredential) {
        let path = dir.path().join("config.yaml");
        write_config(&path, OLD);
        let credential = InferenceCredential::resolve(
            &ApiKeyReference::Environment(NAME.to_string()),
            Some(&path),
        )
        .unwrap();
        (path, credential)
    }

    fn reads(credential: &InferenceCredential) -> usize {
        credential.reads.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn inference_credential_unchanged_file_is_read_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_, credential) = config_credential(&dir);
        assert_eq!(credential.source().trace_name(), "config");
        for _ in 0..5 {
            assert_eq!(credential.current().await, OLD);
        }
        assert_eq!(reads(&credential), 1);
    }

    #[tokio::test]
    async fn inference_credential_rename_replace_is_seen_on_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        assert_eq!(credential.current().await, OLD);
        replace_config(&path, NEW);
        assert_eq!(credential.current().await, NEW);
        assert_eq!(credential.current().await, NEW);
        assert_eq!(reads(&credential), 2);
    }

    #[tokio::test]
    async fn inference_credential_in_place_write_of_a_different_size_is_seen_on_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        write_config(&path, &format!("{NEW}-longer"));
        assert_eq!(credential.current().await, format!("{NEW}-longer"));
        assert_eq!(reads(&credential), 2);
    }

    #[tokio::test]
    async fn inference_credential_reread_after_rejection_sees_a_same_size_edit_the_stamp_misses() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        assert_eq!(OLD.len(), NEW.len());
        write_config(&path, NEW);
        // An edit inside one filesystem timestamp tick leaves the stamp as it was.
        credential.state.lock().unwrap().stamp = Some(FileStamp::stat(&path));
        assert_eq!(credential.current().await, OLD);
        assert_eq!(reads(&credential), 1);
        assert_eq!(credential.reread_after_rejection().await, NEW);
        assert_eq!(reads(&credential), 2);
    }

    #[tokio::test]
    async fn inference_credential_unreadable_source_keeps_the_last_good_value() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        let unreadable = |credential: &InferenceCredential, after_rejection| {
            credential.refresh_locked(after_rejection).1
        };

        fs::remove_file(&path).unwrap();
        assert_eq!(
            unreadable(&credential, false),
            Some(CredentialChange::Unreadable { reason: "missing" })
        );
        // The same (absent) stamp is reported once, whether or not a rejection forced the read.
        assert_eq!(unreadable(&credential, false), None);
        assert_eq!(unreadable(&credential, true), None);
        assert_eq!(credential.current().await, OLD);

        fs::write(&path, "credentials: [\n").unwrap();
        assert_eq!(
            unreadable(&credential, false),
            Some(CredentialChange::Unreadable {
                reason: "unparseable"
            })
        );
        assert_eq!(unreadable(&credential, false), None);

        replace_config(&path, "\"\"");
        assert_eq!(
            unreadable(&credential, false),
            Some(CredentialChange::Unreadable {
                reason: "empty_entry"
            })
        );

        let tmp = path.with_file_name(".config.yaml.tmp");
        fs::write(&tmp, "credentials:\n  OTHER_KEY: x\n").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert_eq!(
            unreadable(&credential, false),
            Some(CredentialChange::Unreadable { reason: "no_entry" })
        );
        assert_eq!(credential.current().await, OLD);

        replace_config(&path, NEW);
        assert_eq!(
            unreadable(&credential, false),
            Some(CredentialChange::Rotated {
                trigger: "file_changed"
            })
        );
        assert_eq!(credential.current().await, NEW);
    }

    #[tokio::test]
    async fn inference_credential_environment_and_literal_sources_never_touch_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        write_config(&path, OLD);
        let variable = "UNIT_CREDENTIAL_ENVIRONMENT_ONLY_KEY";
        // The variable name is unique to this test, so no other test reads or writes it.
        std::env::set_var(variable, NEW);
        let environment = InferenceCredential::resolve(
            &ApiKeyReference::Environment(variable.to_string()),
            Some(&path),
        )
        .unwrap();
        std::env::remove_var(variable);
        assert_eq!(
            environment.source(),
            &CredentialSource::Environment {
                name: variable.to_string()
            }
        );

        let literal =
            InferenceCredential::resolve(&ApiKeyReference::Literal(OLD.to_string()), Some(&path))
                .unwrap();
        assert_eq!(literal.source(), &CredentialSource::ManifestLiteral);

        fs::remove_file(&path).unwrap();
        for (credential, value) in [(&environment, NEW), (&literal, OLD)] {
            assert_eq!(credential.current().await, value);
            assert_eq!(credential.reread_after_rejection().await, value);
            assert_eq!(reads(credential), 0);
        }
    }

    #[test]
    fn inference_credential_config_wins_over_the_environment_and_absence_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        write_config(&path, OLD);
        // `NAME` is set by this test alone.
        std::env::set_var(NAME, NEW);
        let credential = InferenceCredential::resolve(
            &ApiKeyReference::Environment(NAME.to_string()),
            Some(&path),
        );
        std::env::remove_var(NAME);
        assert_eq!(
            credential.unwrap().source(),
            &CredentialSource::Config {
                path: path.clone(),
                name: NAME.to_string()
            }
        );

        let missing = "UNIT_CREDENTIAL_NOWHERE_KEY";
        let err = InferenceCredential::resolve(
            &ApiKeyReference::Environment(missing.to_string()),
            Some(&path),
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&format!("credentials.{missing}")),
            "{message}"
        );
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("environment variable"), "{message}");
    }

    #[tokio::test]
    async fn inference_credential_rejection_is_taken_once_and_names_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        assert_eq!(credential.take_rejection(), None);
        credential.record_rejection(401, true).await;
        let rejection = credential.take_rejection().unwrap();
        assert_eq!(
            rejection,
            CredentialRejection {
                status: 401,
                retried: true
            }
        );
        assert_eq!(credential.take_rejection(), None);
        let message = credential.rejection_message(rejection);
        assert!(
            message.contains(&format!("credentials.{NAME}")),
            "{message}"
        );
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("HTTP 401"), "{message}");

        credential.record_rejection(401, false).await;
        credential.clear_rejection();
        assert_eq!(credential.take_rejection(), None);
    }

    #[tokio::test]
    async fn inference_credential_debug_and_labels_carry_no_value() {
        let dir = tempfile::tempdir().unwrap();
        let (path, credential) = config_credential(&dir);
        replace_config(&path, NEW);
        credential.current().await;
        let literal =
            InferenceCredential::resolve(&ApiKeyReference::Literal(OLD.to_string()), None).unwrap();
        for credential in [&credential, &literal] {
            let rejection = CredentialRejection {
                status: 401,
                retried: true,
            };
            for text in [
                format!("{credential:?}"),
                credential.source().label(),
                credential.rejection_message(rejection),
                credential.rejection_hint(),
            ] {
                assert!(!text.contains(OLD) && !text.contains(NEW), "{text}");
            }
        }
    }
}
