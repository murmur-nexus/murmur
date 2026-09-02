pub(crate) mod beta;
pub(crate) mod build;
pub(crate) mod config_cmd;
pub(crate) mod conversation;
#[cfg(feature = "beta-mur-deploy")]
pub(crate) mod deploy;
#[cfg(feature = "beta-mur-deploy")]
pub(crate) mod deploy_state;
#[cfg(feature = "beta-mur-deploy")]
pub(crate) mod destroy;
pub(crate) mod doctor;
pub(crate) mod eval;
pub(crate) mod install;
pub(crate) mod list;
#[cfg(feature = "beta-mur-new")]
pub(crate) mod new;
#[cfg(feature = "beta-mur-deploy")]
pub(crate) mod ps;
pub(crate) mod publish;
pub(crate) mod run;
pub(crate) mod search;
#[cfg(feature = "beta-mur-topology")]
pub(crate) mod topology;
pub(crate) mod trace;
pub(crate) mod watch;

use std::path::Path;

use capsule_runtime::ResolvedLockArtifact;
use murmur_artifact::{LockedSha256, LockfileError, RuntimeManifestError, MANIFEST_FILENAME};

use crate::error::{CliError, E_IO_001, E_IO_003, E_MAN_001, E_MAN_002, E_MAN_003, E_RUN_003};

pub(crate) enum RunStatus {
    Success,
    Failed,
    Trapped,
}

/// Print the post-completion status line. Session and workdir are shown at startup now,
/// so only `status:` is emitted here to avoid duplication.
pub(crate) fn print_run_output(_session_id: &str, _workdir: &Path, status: RunStatus) {
    let status_str = match status {
        RunStatus::Success => "ok",
        RunStatus::Failed => "failed",
        RunStatus::Trapped => "trapped",
    };
    println!("status:  {status_str}");
}

/// Prints `status: failed` and returns the error, eliminating the
/// repeated print-then-return pattern throughout run_run.
pub(crate) fn fail_run(session_id: &str, workdir: &Path, error: CliError) -> CliError {
    print_run_output(session_id, workdir, RunStatus::Failed);
    error
}

pub(crate) fn runtime_manifest_error_to_cli(error: RuntimeManifestError) -> CliError {
    match error {
        RuntimeManifestError::NotFound(path) => {
            CliError::new(E_IO_001, format!("{MANIFEST_FILENAME} not found at {path}"))
        }
        RuntimeManifestError::YamlSyntax(message) => CliError::new(E_MAN_002, message),
        RuntimeManifestError::MissingField { field } => CliError::new(
            E_MAN_001,
            format!("{MANIFEST_FILENAME}: missing required field '{field}'"),
        ),
        RuntimeManifestError::InvalidArtifact { index, message } => CliError::new(
            E_MAN_003,
            format!("{MANIFEST_FILENAME}: invalid artifact declaration at index {index}: {message}"),
        ),
        RuntimeManifestError::InvalidInferenceConfig { field, message } => CliError::new(
            E_MAN_003,
            format!("{MANIFEST_FILENAME}: invalid inference config for '{field}': {message}"),
        ),
        RuntimeManifestError::InvalidCapabilities { field, message } => CliError::new(
            E_MAN_003,
            format!("{MANIFEST_FILENAME}: invalid capability config for '{field}': {message}"),
        ),
        RuntimeManifestError::InvalidExports { field, message } => CliError::new(
            E_MAN_003,
            format!("{MANIFEST_FILENAME}: invalid exports config for '{field}': {message}"),
        ),
        RuntimeManifestError::InvalidTraceConfig { field, message } => CliError::new(
            E_MAN_003,
            format!("{MANIFEST_FILENAME}: invalid trace config for '{field}': {message}"),
        ),
        RuntimeManifestError::MissingInferenceEnvVar {
            field: _,
            reference,
            variable: _,
        } => CliError::new(
            E_MAN_003,
            format!(
                "{MANIFEST_FILENAME}: inference.api_key references {reference} but the environment variable is not set"
            ),
        ),
        RuntimeManifestError::Io { path, source } => CliError::new(
            E_IO_003,
            format!("failed to read {MANIFEST_FILENAME} at {path}: {source}"),
        ),
    }
}

pub(crate) fn lockfile_error_to_cli(error: LockfileError) -> CliError {
    match error {
        LockfileError::NotFound(path) => {
            CliError::new(E_RUN_003, format!("murmur.lock not found at {path}"))
        }
        LockfileError::Invalid(message) => {
            CliError::new(E_RUN_003, format!("invalid murmur.lock: {message}"))
        }
        LockfileError::ReadIo { path, source } => CliError::new(
            E_IO_003,
            format!("failed to read murmur.lock at {path}: {source}"),
        ),
        LockfileError::WriteIo { path, source } => CliError::new(
            E_IO_003,
            format!("failed to write murmur.lock at {path}: {source}"),
        ),
    }
}

/// The `murmur.lock` pin for a payload staging resolved, mirroring `mur install`'s rule: a
/// native binary is pinned under the platform it was resolved for, everything else under `any`.
pub(crate) fn locked_sha256(entry: &ResolvedLockArtifact) -> LockedSha256 {
    match &entry.platform {
        Some(platform) => LockedSha256::for_one_platform(platform, entry.sha256.clone()),
        None => LockedSha256::any(entry.sha256.clone()),
    }
}
