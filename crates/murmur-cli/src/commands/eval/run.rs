//! `mur eval run`: every case of a dataset against a baseline manifest and, with `--variant`,
//! against a second one from the same start, each run's work judged by the case's check.
//!
//! The comparison is written as one eval-run record, `evr_<32 hex>.json`, in the records group of
//! the baseline's workdir root, beside the session directories. It is rewritten after every run, so
//! an interrupted run still leaves one, and it holds everything `mur eval diff --run` prints: the
//! sessions' traces are read once, here, and never again.

use std::{
    collections::HashSet,
    fs,
    io::Read as _,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, copy_tree_into, launch_session, stage_session,
    walk_tree, ArtifactRequest, CapabilityPolicy, CheckOutcome, CheckResult, LockExpectation,
    RuntimeError, SessionCheck, StageRequest,
};
use murmur_artifact::{
    current_platform, load_dotenv_non_override, load_runtime_manifest, read_lockfile,
    resolve_manifest_path, write_lockfile_atomic, ArtifactRuntime, LocalRegistry, LockOrigin,
    LockedArtifact, LockfileError, MurmurLock, Registry, RuntimeManifest, LOCK_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{CliError, E_IO_001, E_IO_003};
use crate::stores::FallbackRegistry;

use super::super::{fail_run, lockfile_error_to_cli, runtime_manifest_error_to_cli};
use super::{
    compare, parse_eval_file, staged_records_display, DatasetRunRecord, EVAL_RECORD_FILE,
    E_EVAL_001,
};

const E_EVAL_003: &str = "E-EVAL-003";
const E_EVAL_004: &str = "E-EVAL-004";

/// A check's timeout when its dataset line names none.
const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 600;

/// The version of the eval-run record's shape.
const RECORD_VERSION: u32 = 1;

/// The file-name prefix of an eval-run record, before its uuid v7 in simple hex.
const RUN_ID_PREFIX: &str = "evr_";

pub(super) const BASELINE: &str = "baseline";
pub(super) const VARIANT: &str = "variant";

// ── Dataset ───────────────────────────────────────────────────────────────────

/// One line of `eval.jsonl`.
#[derive(Debug, Deserialize)]
pub(super) struct DatasetCase {
    pub(super) case_id: String,
    pub(super) task_path: String,
    /// A directory copied into the work folder before the session.
    #[serde(default)]
    pub(super) start: Option<String>,
    /// A command run in the work folder after the session.
    #[serde(default)]
    pub(super) check: Option<CaseCheck>,
}

/// A case's `check`: the command alone, or the command with its files and timeout.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(super) enum CaseCheck {
    Command(String),
    Full {
        command: String,
        #[serde(default)]
        files: Option<String>,
        #[serde(default = "default_check_timeout_secs")]
        timeout_secs: u64,
    },
}

fn default_check_timeout_secs() -> u64 {
    DEFAULT_CHECK_TIMEOUT_SECS
}

impl CaseCheck {
    fn command(&self) -> &str {
        match self {
            Self::Command(command) | Self::Full { command, .. } => command,
        }
    }

    fn files(&self) -> Option<&str> {
        match self {
            Self::Command(_) => None,
            Self::Full { files, .. } => files.as_deref(),
        }
    }

    fn timeout_secs(&self) -> u64 {
        match self {
            Self::Command(_) => DEFAULT_CHECK_TIMEOUT_SECS,
            Self::Full { timeout_secs, .. } => *timeout_secs,
        }
    }
}

/// Every case of the dataset at `path`, in file order. `E-EVAL-001` names the line that does not
/// parse.
pub(super) fn parse_dataset(path: &Path, content: &str) -> Result<Vec<DatasetCase>, CliError> {
    let mut cases = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let case: DatasetCase = serde_json::from_str(line).map_err(|err| {
            CliError::new(E_EVAL_001, format!("{}:{}: {err}", path.display(), i + 1))
        })?;
        cases.push(case);
    }
    Ok(cases)
}

// ── Start trees ───────────────────────────────────────────────────────────────

/// The start digest of the tree at `root`: sha256, in lowercase hex, over every entry in
/// byte-wise order of its relative path — `d` or `f`, a space, `mode & 0o7777` in octal, a space,
/// the relative path, a NUL, and for a file its length in decimal, a NUL and its bytes.
pub(super) fn tree_digest(root: &Path) -> Result<String, String> {
    let mut hasher = Sha256::new();
    for entry in walk_tree(root)? {
        let kind = if entry.is_dir { "d" } else { "f" };
        hasher.update(format!("{kind} {:o} ", entry.mode).as_bytes());
        hasher.update(entry.relative.as_os_str().as_bytes());
        hasher.update(b"\0");
        if !entry.is_dir {
            let path = root.join(&entry.relative);
            let mut file = fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let len = file
                .metadata()
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            hasher.update(format!("{len}\0").as_bytes());
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let read = file
                    .read(&mut chunk)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&chunk[..read]);
            }
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// ── The eval-run record ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct EvalRunRecord {
    pub(super) record_version: u32,
    pub(super) run_id: String,
    pub(super) started_at_ms: u64,
    /// `None` until the run is complete.
    pub(super) ended_at_ms: Option<u64>,
    pub(super) complete: bool,
    pub(super) dataset: String,
    pub(super) repeat: u32,
    pub(super) arms: Vec<ArmRecord>,
    pub(super) cases: Vec<CaseRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ArmRecord {
    pub(super) name: String,
    pub(super) manifest: String,
    pub(super) manifest_sha256: String,
    pub(super) model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CaseRecord {
    pub(super) case_id: String,
    pub(super) task_path: String,
    pub(super) start: Option<String>,
    pub(super) start_sha256: Option<String>,
    pub(super) check: Option<CheckSpecRecord>,
    pub(super) runs: Vec<RunRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct CheckSpecRecord {
    pub(super) command: String,
    pub(super) files: Option<String>,
    pub(super) timeout_secs: u64,
}

/// How one run got as far as it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RunOutcome {
    /// Launched, and the launch returned without an error.
    Completed,
    /// Launched, and the launch returned an error: the task did not complete.
    LaunchFailed,
    /// Staging refused the session; nothing was launched.
    StageFailed,
    /// The copied start's digest differed from the source's; nothing was launched.
    StartMismatch,
}

/// A run's check result: a [`CheckResult`], or `not_run` for a run that never reached its check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RunCheckResult {
    Pass,
    Fail,
    TimedOut,
    Error,
    NotRun,
}

impl From<CheckResult> for RunCheckResult {
    fn from(result: CheckResult) -> Self {
        match result {
            CheckResult::Pass => Self::Pass,
            CheckResult::Fail => Self::Fail,
            CheckResult::TimedOut => Self::TimedOut,
            CheckResult::Error => Self::Error,
        }
    }
}

impl RunCheckResult {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::TimedOut => "timed_out",
            Self::Error => "error",
            Self::NotRun => "not_run",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RunRecord {
    pub(super) arm: String,
    /// 0-based.
    pub(super) repeat: u32,
    /// 0 when this arm ran first in its pair, 1 when it ran second.
    pub(super) order: u32,
    pub(super) session_id: Option<String>,
    pub(super) records_dir: Option<String>,
    pub(super) workdir: Option<String>,
    /// The digest of the start as copied into this run's work folder.
    pub(super) start_sha256: Option<String>,
    pub(super) outcome: RunOutcome,
    pub(super) exit_status: Option<String>,
    /// `None` when the case has no check.
    pub(super) check_result: Option<RunCheckResult>,
    pub(super) check: Option<CheckOutcome>,
    pub(super) turns: Option<u32>,
    pub(super) input_tokens: Option<u64>,
    pub(super) output_tokens: Option<u64>,
    pub(super) cached_tokens: Option<u64>,
    pub(super) cache_write_tokens: Option<u64>,
    pub(super) agent_ms: Option<u64>,
    pub(super) scores_overall: Option<String>,
}

impl RunRecord {
    fn not_launched(arm: &str, repeat: u32, order: u32, outcome: RunOutcome) -> Self {
        Self {
            arm: arm.to_string(),
            repeat,
            order,
            session_id: None,
            records_dir: None,
            workdir: None,
            start_sha256: None,
            outcome,
            exit_status: None,
            check_result: None,
            check: None,
            turns: None,
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            cache_write_tokens: None,
            agent_ms: None,
            scores_overall: None,
        }
    }
}

/// Writes `record` to `path` at `0600`, replacing what is there in one rename.
fn write_record(path: &Path, record: &EvalRunRecord) -> Result<(), CliError> {
    let group = path.parent().unwrap_or(path);
    if !group.is_dir() {
        fs::create_dir_all(group).map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to create {}: {e}", group.display()),
            )
        })?;
        capsule_runtime::murmur_home::hold_private_dir(group)
            .map_err(|reason| CliError::new(E_IO_003, reason))?;
    }
    let mut json = serde_json::to_vec_pretty(record).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to encode the eval-run record: {e}"),
        )
    })?;
    json.push(b'\n');
    capsule_runtime::murmur_home::write_private_file(path, &json).map_err(|reason| {
        CliError::new(
            E_IO_003,
            format!("failed to write {}: {reason}", path.display()),
        )
    })
}

// ── Arms ──────────────────────────────────────────────────────────────────────

/// Everything one manifest's sessions are staged from, prepared once per run.
struct Arm {
    name: &'static str,
    manifest_path: PathBuf,
    project_dir: PathBuf,
    manifest: RuntimeManifest,
    capability_policy: CapabilityPolicy,
    source_supply: Option<Arc<dyn capsule_runtime::SourceSupply>>,
    allowlisted_tools: HashSet<String>,
    pinned_artifacts: Vec<ArtifactRequest>,
    lock_expectations: Option<Vec<LockExpectation>>,
    /// `Some(path)` when this arm may write the missing lock after its first successful staging:
    /// only the baseline arm, and only when it has none.
    lock_to_write: Option<PathBuf>,
    registry: Arc<dyn Registry>,
    eval_config_json: Option<String>,
    dataset_id: Option<String>,
}

/// The manifest `path` names, made absolute against the current directory: a capsule directory's
/// `murmur.yaml`, or the manifest file itself. Its project directory is the manifest's parent.
fn manifest_of(path: &Path, as_directory: bool) -> Result<(PathBuf, PathBuf), CliError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|source| {
                CliError::new(E_IO_003, format!("failed to determine cwd: {source}"))
            })?
            .join(path)
    };
    // Rebuilt from its components, which leave out every `.`: `./variant` is recorded and
    // printed as `<cwd>/variant`.
    let path: PathBuf = path.components().collect();
    let manifest_path = if as_directory || path.is_dir() {
        resolve_manifest_path(&path)
    } else {
        path
    };
    let project_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::new(E_IO_003, "failed to determine manifest directory"))?;
    Ok((manifest_path, project_dir))
}

/// The manifest at `manifest_path`, after its project's `.env` is loaded beneath the variables
/// already set.
fn load_arm_manifest(
    manifest_path: &Path,
    project_dir: &Path,
) -> Result<RuntimeManifest, CliError> {
    load_dotenv_non_override(project_dir)
        .map_err(|source| CliError::new(E_IO_003, source.to_string()))?;
    load_runtime_manifest(manifest_path).map_err(runtime_manifest_error_to_cli)
}

/// The policy, artifacts, lock pins and registry `manifest`'s sessions are staged with.
fn prepare_arm(
    name: &'static str,
    manifest_path: PathBuf,
    project_dir: PathBuf,
    manifest: RuntimeManifest,
    may_write_lock: bool,
) -> Result<Arm, CliError> {
    let eval = manifest
        .observability
        .as_ref()
        .and_then(|o| o.eval.as_ref());
    let eval_config_json = eval.and_then(|e| serde_json::to_string(e).ok());
    let dataset_id = eval.and_then(|e| e.dataset_id.clone());

    let capability_policy = capability_policy_from_runtime_manifest(&manifest);
    let source_supply = crate::source::supply::CliSourceSupply::for_policy(&capability_policy)?;

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts: Vec<ArtifactRequest> = Vec::new();
    for artifact in &manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let lock_path = project_dir.join("murmur.lock");
    let (pinned_artifacts, lock_expectations, lock_to_write) = match read_lockfile(&lock_path) {
        Ok(lock) => {
            let mut pinned = Vec::with_capacity(requested_artifacts.len());
            let mut expectations = Vec::with_capacity(requested_artifacts.len());
            for artifact in &requested_artifacts {
                if let Some(entry) = lock.artifact_for(&artifact.name) {
                    pinned.push(ArtifactRequest {
                        name: artifact.name.clone(),
                        version: entry.resolved_version.clone(),
                        runtime: artifact.runtime.clone(),
                        source: None,
                        on_overflow: artifact.on_overflow,
                        config: artifact.config.clone(),
                        gateway: artifact.gateway.clone(),
                        capabilities: artifact.capabilities.clone(),
                    });
                    // This host's hash, never another platform's — the same selection
                    // `mur run` makes, so an eval enforces exactly what a session would.
                    let host_platform = current_platform();
                    let sha256 = entry.sha256.for_platform(host_platform).ok_or_else(|| {
                        CliError::with_hint(
                            E_IO_003,
                            format!(
                                "murmur.lock has no sha256 for '{}' on platform {host_platform} \u{2014} it pins {}",
                                artifact.name,
                                entry.sha256.platform_tags().join(", ")
                            ),
                            "run `mur install` on this host to add this platform's hash to murmur.lock",
                        )
                    })?;
                    expectations.push(LockExpectation {
                        name: artifact.name.clone(),
                        resolved_version: entry.resolved_version.clone(),
                        sha256: sha256.to_string(),
                        origin: entry.origin.clone(),
                    });
                } else {
                    pinned.push(artifact.clone());
                }
            }
            (pinned, Some(expectations), None)
        }
        Err(LockfileError::NotFound(_)) => (
            requested_artifacts.clone(),
            None,
            may_write_lock.then_some(lock_path),
        ),
        Err(error) => return Err(lockfile_error_to_cli(error)),
    };

    let project_registry = LocalRegistry::new(project_dir.join(".murmur").join("artifacts"));
    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let registry: Arc<dyn Registry> = Arc::new(FallbackRegistry {
        primary: project_registry,
        secondary: global_registry,
    });

    Ok(Arm {
        name,
        manifest_path,
        project_dir,
        manifest,
        capability_policy,
        source_supply,
        allowlisted_tools,
        pinned_artifacts,
        lock_expectations,
        lock_to_write,
        registry,
        eval_config_json,
        dataset_id,
    })
}

impl Arm {
    fn stage_request(&self, case_id: &str) -> StageRequest {
        let manifest = &self.manifest;
        StageRequest {
            credentials_file: crate::config::mur_config_path().ok(),
            manifest_dir: self.project_dir.clone(),
            capsule_name: manifest.name.clone(),
            capsule_version: manifest.version.clone(),
            capsule_artifact: None,
            capsule_component_bytes: Vec::new(),
            artifacts: self.pinned_artifacts.clone(),
            allowlisted_tools: self.allowlisted_tools.clone(),
            lock_expectations: self.lock_expectations.clone(),
            capability_policy: self.capability_policy.clone(),
            inference: manifest.inference.clone(),
            system_prompt_overridden: false,
            context: manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: manifest
                .observability
                .as_ref()
                .and_then(|o| o.otel_endpoint.clone()),
            eval_config_json: self.eval_config_json.clone(),
            case_id: Some(case_id.to_string()),
            dataset_id: self.dataset_id.clone(),
            lifecycle: manifest.lifecycle.clone(),
            lifecycle_override: None,
            trace: manifest.trace,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: manifest.network.as_ref().and_then(|n| n.internal_port),
            // `mur eval` has no --containment flag and reads no workspace config, so the
            // manifest is the only source of a floor here.
            declared_containment_floor: manifest
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.containment)
                .unwrap_or_default(),
            exports: manifest.exports.clone(),
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
            source_supply: self.source_supply.clone(),
            expected_workdir_identity: None,
            parent_workdir_snapshot: None,
            session_deadline: None,
            manifest_file: Some(self.manifest_path.clone()),
        }
    }

    fn model(&self) -> Option<&str> {
        self.manifest
            .inference
            .as_ref()
            .map(|inference| inference.model.as_str())
    }

    fn record(&self) -> Result<ArmRecord, CliError> {
        let bytes = fs::read(&self.manifest_path).map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to read {}: {e}", self.manifest_path.display()),
            )
        })?;
        Ok(ArmRecord {
            name: self.name.to_string(),
            manifest: self.manifest_path.display().to_string(),
            manifest_sha256: murmur_artifact::sha256_hex(&bytes),
            model: self.model().map(str::to_string),
        })
    }
}

// ── Preflight ─────────────────────────────────────────────────────────────────

/// One case, checked and resolved before anything is staged.
struct PreparedCase {
    case_id: String,
    task_path: String,
    start: Option<PathBuf>,
    start_sha256: Option<String>,
    check: Option<(SessionCheck, CheckSpecRecord)>,
}

/// `path`, made absolute against the current directory, as `task_path` is read.
fn from_cwd(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// The directory a case's `field` names, refused with `E-EVAL-003` when it is missing, is not a
/// directory, or holds a symbolic link or special file.
fn case_directory(case_id: &str, field: &str, value: &str) -> Result<PathBuf, CliError> {
    let path = from_cwd(value);
    let refuse = |reason: String| {
        CliError::new(
            E_EVAL_003,
            format!("case '{case_id}': {field} {}: {reason}", path.display()),
        )
    };
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(refuse("is not a directory".to_string())),
        Err(e) => return Err(refuse(e.to_string())),
    }
    walk_tree(&path).map_err(refuse)?;
    Ok(path)
}

/// Checks every case against both arms before any staging or provider request: `E-EVAL-003` for
/// a case's own fields, `E-EVAL-004` for a check binary an arm does not allow.
fn preflight(cases: &[DatasetCase], arms: &[Arm]) -> Result<Vec<PreparedCase>, CliError> {
    let mut prepared = Vec::with_capacity(cases.len());
    for case in cases {
        let case_id = &case.case_id;
        let (start, start_sha256) = match &case.start {
            Some(start) => {
                let path = case_directory(case_id, "start", start)?;
                let digest = tree_digest(&path).map_err(|reason| {
                    CliError::new(
                        E_EVAL_003,
                        format!("case '{case_id}': start {}: {reason}", path.display()),
                    )
                })?;
                (Some(path), Some(digest))
            }
            None => (None, None),
        };
        let check = match &case.check {
            Some(spec) => {
                if spec.command().trim().is_empty() {
                    return Err(CliError::new(
                        E_EVAL_003,
                        format!("case '{case_id}': check.command is empty"),
                    ));
                }
                if spec.timeout_secs() == 0 {
                    return Err(CliError::new(
                        E_EVAL_003,
                        format!("case '{case_id}': check.timeout_secs is 0"),
                    ));
                }
                let files = spec
                    .files()
                    .map(|files| case_directory(case_id, "check.files", files))
                    .transpose()?;
                let check = SessionCheck::new(
                    spec.command(),
                    files.clone(),
                    Duration::from_secs(spec.timeout_secs()),
                )
                .map_err(|reason| {
                    CliError::new(E_EVAL_003, format!("case '{case_id}': check: {reason}"))
                })?;
                for arm in arms {
                    let binary = check.binary();
                    if !arm
                        .capability_policy
                        .shell_allow
                        .iter()
                        .any(|allowed| allowed == binary)
                    {
                        return Err(CliError::with_hint(
                            E_EVAL_004,
                            format!(
                                "case '{case_id}': the {} arm's capabilities.shell.allow has no \
                                 '{binary}', the check's binary",
                                arm.name
                            ),
                            format!(
                                "add {binary} to capabilities.shell.allow in {}: a check runs \
                                 under the session's own containment",
                                arm.manifest_path.display()
                            ),
                        ));
                    }
                }
                let record = CheckSpecRecord {
                    command: spec.command().to_string(),
                    files: files.map(|files| files.display().to_string()),
                    timeout_secs: spec.timeout_secs(),
                };
                Some((check, record))
            }
            None => None,
        };
        prepared.push(PreparedCase {
            case_id: case_id.clone(),
            task_path: case.task_path.clone(),
            start,
            start_sha256,
            check,
        });
    }
    Ok(prepared)
}

// ── Run ───────────────────────────────────────────────────────────────────────

/// What `mur eval run` was asked.
pub(crate) struct EvalRunArgs<'a> {
    pub(crate) capsule: Option<&'a Path>,
    pub(crate) dataset: Option<&'a Path>,
    pub(crate) variant: Option<&'a Path>,
    pub(crate) repeat: u32,
}

pub(crate) fn run_eval_run(args: EvalRunArgs<'_>) -> Result<(), CliError> {
    let session_id = "n/a".to_string();
    let cwd_workdir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("/"))
        .join("workdir");

    let capsule = args.capsule.unwrap_or_else(|| Path::new("."));
    let (manifest_path, project_dir) =
        manifest_of(capsule, true).map_err(|error| fail_run(&session_id, &cwd_workdir, error))?;
    let workdir = project_dir.join("workdir");

    // Resolve dataset — default to ./eval.jsonl (project root / cwd) if not specified
    let (resolved_dataset, dataset_explicit) = match args.dataset {
        Some(p) => (p.to_path_buf(), true),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            (cwd.join("eval.jsonl"), false)
        }
    };

    let baseline_manifest = load_arm_manifest(&manifest_path, &project_dir)
        .map_err(|error| fail_run(&session_id, &workdir, error))?;
    let variant = match args.variant {
        Some(variant) => {
            let (variant_manifest_path, variant_project_dir) = manifest_of(variant, false)
                .map_err(|error| fail_run(&session_id, &workdir, error))?;
            let manifest = load_arm_manifest(&variant_manifest_path, &variant_project_dir)
                .map_err(|error| fail_run(&session_id, &workdir, error))?;
            Some((variant_manifest_path, variant_project_dir, manifest))
        }
        None => None,
    };

    let dataset_content = fs::read_to_string(&resolved_dataset).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_001,
                if dataset_explicit {
                    format!("dataset not found at {}", resolved_dataset.display())
                } else {
                    "no dataset found. Expected ./eval.jsonl or specify with --dataset <path>"
                        .to_string()
                },
            ),
        ),
        _ => fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_003,
                format!("failed to read dataset {}: {e}", resolved_dataset.display()),
            ),
        ),
    })?;
    let cases = parse_dataset(&resolved_dataset, &dataset_content)
        .map_err(|error| fail_run(&session_id, &workdir, error))?;
    if cases.is_empty() {
        return Err(fail_run(
            &session_id,
            &workdir,
            CliError::new(
                E_IO_003,
                format!("dataset is empty: {}", resolved_dataset.display()),
            ),
        ));
    }

    if let Some((variant_manifest_path, _, variant_manifest)) = &variant {
        let model = |manifest: &RuntimeManifest| {
            manifest
                .inference
                .as_ref()
                .map(|inference| inference.model.clone())
        };
        let (baseline_model, variant_model) = (model(&baseline_manifest), model(variant_manifest));
        if baseline_model != variant_model {
            let shown =
                |model: &Option<String>| model.clone().unwrap_or_else(|| "none".to_string());
            return Err(fail_run(
                &session_id,
                &workdir,
                CliError::new(
                    E_EVAL_004,
                    format!(
                        "the baseline's inference.model is '{}' ({}) and the variant's is '{}' \
                         ({}); a comparison holds the model fixed",
                        shown(&baseline_model),
                        manifest_path.display(),
                        shown(&variant_model),
                        variant_manifest_path.display()
                    ),
                ),
            ));
        }
    }

    let mut arms = vec![prepare_arm(
        BASELINE,
        manifest_path.clone(),
        project_dir.clone(),
        baseline_manifest,
        true,
    )
    .map_err(|error| fail_run(&session_id, &workdir, error))?];
    if let Some((variant_manifest_path, variant_project_dir, variant_manifest)) = variant {
        arms.push(
            prepare_arm(
                VARIANT,
                variant_manifest_path,
                variant_project_dir,
                variant_manifest,
                false,
            )
            .map_err(|error| fail_run(&session_id, &workdir, error))?,
        );
    }

    let prepared =
        preflight(&cases, &arms).map_err(|error| fail_run(&session_id, &workdir, error))?;

    let group = capsule_runtime::records_group(&workdir)
        .map_err(|error| fail_run(&session_id, &workdir, CliError::from(error)))?;
    let run_id = format!("{RUN_ID_PREFIX}{}", uuid::Uuid::now_v7().simple());
    let record_path = group.join(format!("{run_id}.json"));
    let mut record = EvalRunRecord {
        record_version: RECORD_VERSION,
        run_id: run_id.clone(),
        started_at_ms: capsule_runtime::retention::now_ms(),
        ended_at_ms: None,
        complete: false,
        dataset: from_cwd(&resolved_dataset).display().to_string(),
        repeat: args.repeat,
        arms: arms.iter().map(Arm::record).collect::<Result<_, _>>()?,
        cases: prepared
            .iter()
            .map(|case| CaseRecord {
                case_id: case.case_id.clone(),
                task_path: case.task_path.clone(),
                start: case.start.as_ref().map(|start| start.display().to_string()),
                start_sha256: case.start_sha256.clone(),
                check: case.check.as_ref().map(|(_, spec)| spec.clone()),
                runs: Vec::new(),
            })
            .collect(),
    };
    write_record(&record_path, &record)?;

    // A run with one arm and one repeat prints only the per-case lines; a matched run also says
    // which arm and repeat each line belongs to.
    let matched = arms.len() > 1 || args.repeat > 1;

    capsule_runtime::report_println!("Running {} case(s) …", cases.len());

    let mut case_results: Vec<(String, String, PathBuf, Option<DatasetRunRecord>)> = Vec::new();
    let mut lock_written = false;

    for (case_index, case) in prepared.iter().enumerate() {
        capsule_runtime::report_println!("  case: {}", case.case_id);
        for repeat in 0..args.repeat {
            let mut order: Vec<&Arm> = arms.iter().collect();
            if repeat % 2 == 1 {
                order.reverse();
            }
            for (position, arm) in order.into_iter().enumerate() {
                if matched {
                    capsule_runtime::report_println!(
                        "    arm: {}  repeat: {}",
                        arm.name,
                        repeat + 1
                    );
                }
                let (run, overall, case_workdir, scores) = run_one(
                    arm,
                    case,
                    repeat,
                    position as u32,
                    &workdir,
                    &mut lock_written,
                );
                case_results.push((case.case_id.clone(), overall, case_workdir, scores));
                record.cases[case_index].runs.push(run);
                write_record(&record_path, &record)?;
            }
        }
    }

    record.complete = true;
    record.ended_at_ms = Some(capsule_runtime::retention::now_ms());
    write_record(&record_path, &record)?;

    capsule_runtime::report_println!();
    capsule_runtime::report_println!("── Summary ──────────────────────────────────────");
    let pass_count = case_results
        .iter()
        .filter(|(_, r, _, _)| r == "pass")
        .count();
    let total = case_results.len();
    capsule_runtime::report_println!("pass: {pass_count}/{total}");
    capsule_runtime::report_println!();

    for (case_id, result, case_workdir, run) in &case_results {
        let scores_str = run
            .as_ref()
            .map(|r| {
                let mut parts: Vec<String> = r
                    .scores
                    .iter()
                    .map(|(k, v)| format!("{k}={:.2}", v))
                    .collect();
                parts.sort();
                parts.join(" ")
            })
            .unwrap_or_default();
        capsule_runtime::report_println!(
            "  {:<24} {}  {}  ({})",
            case_id,
            result,
            scores_str,
            case_workdir.display()
        );
    }

    capsule_runtime::report_println!();
    compare::print_human(&record);
    capsule_runtime::report_println!();
    capsule_runtime::report_println!("run: {run_id}");
    // `mur eval diff --run` reads `./workdir` unless told otherwise, so the hint names the
    // baseline's workdir root whenever that is not where it would look.
    let default_workdir = std::env::current_dir().map(|cwd| cwd.join("workdir"));
    let workdir_arg = match default_workdir {
        Ok(default)
            if capsule_runtime::records_group(&default).ok()
                == capsule_runtime::records_group(&workdir).ok() =>
        {
            String::new()
        }
        _ => format!(" --workdir {}", workdir.display()),
    };
    capsule_runtime::report_println!("  hint: mur eval diff --run {run_id}{workdir_arg}");
    Ok(())
}

/// Stages, prepares, launches and checks one run of `case` on `arm`. Returns the run's record,
/// the hook-eval `overall` the per-case lines print, the work folder and the hook's scores.
fn run_one(
    arm: &Arm,
    case: &PreparedCase,
    repeat: u32,
    order: u32,
    project_workdir: &Path,
    lock_written: &mut bool,
) -> (RunRecord, String, PathBuf, Option<DatasetRunRecord>) {
    let not_run = case.check.as_ref().map(|_| RunCheckResult::NotRun);

    let staged = match stage_session(Arc::clone(&arm.registry), arm.stage_request(&case.case_id)) {
        Ok(s) => s,
        Err(error) => {
            let error = crate::wit_level::staging_error(error);
            capsule_runtime::report_eprintln!(
                "    stage failed: error[{}]: {}",
                error.code,
                error.message
            );
            if let Some(hint) = &error.hint {
                capsule_runtime::report_eprintln!("      hint: {hint}");
            }
            let mut run = RunRecord::not_launched(arm.name, repeat, order, RunOutcome::StageFailed);
            run.check_result = not_run;
            return (
                run,
                "stage_failed".to_string(),
                project_workdir.to_path_buf(),
                None,
            );
        }
    };

    if let Some(lock_path) = arm.lock_to_write.as_ref().filter(|_| !*lock_written) {
        let lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: staged
                .resolved_lock_artifacts
                .iter()
                .map(|entry| LockedArtifact {
                    name: entry.name.clone(),
                    resolved_version: entry.resolved_version.clone(),
                    sha256: super::super::locked_sha256(entry),
                    origin: LockOrigin::Operator,
                    owner_id: None,
                })
                .collect(),
        };
        if write_lockfile_atomic(lock_path, &lock).is_ok() {
            *lock_written = true;
        }
    }

    let case_workdir = staged.accessible_workdir.clone();
    let mut run = RunRecord::not_launched(arm.name, repeat, order, RunOutcome::Completed);
    run.session_id = Some(staged.session_id.clone());
    run.records_dir = Some(staged.records_dir.display().to_string());
    run.workdir = Some(case_workdir.display().to_string());

    if let (Some(start), Some(expected)) = (&case.start, &case.start_sha256) {
        let copied = copy_tree_into(start, &staged.accessible_root)
            .and_then(|()| tree_digest(&case_workdir));
        let matches = match &copied {
            Ok(digest) => digest == expected,
            Err(_) => false,
        };
        run.start_sha256 = copied.as_ref().ok().cloned();
        if !matches {
            match &copied {
                Ok(digest) => capsule_runtime::report_eprintln!(
                    "    start mismatch: the copy's digest is {digest}, the source's {expected}; \
                     not launched"
                ),
                Err(reason) => capsule_runtime::report_eprintln!(
                    "    start mismatch: could not copy {}: {reason}; not launched",
                    start.display()
                ),
            }
            run.outcome = RunOutcome::StartMismatch;
            run.check_result = not_run;
            return (run, "start_mismatch".to_string(), case_workdir, None);
        }
    }

    // Copy the case task into the runtime directory's task.md
    let task_path = Path::new(&case.task_path);
    if task_path.exists() {
        let copied = fs::read(task_path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| staged.write_task_input(&bytes).map_err(|e| e.to_string()));
        if let Err(e) = copied {
            capsule_runtime::report_eprintln!("    warning: failed to copy task file: {e}");
        }
    } else if !case.task_path.is_empty() {
        capsule_runtime::report_eprintln!("    warning: task_path '{}' not found", case.task_path);
    }

    let check_runner = staged.check_runner();
    let records_dir = staged.records_dir.clone();
    let case_eval = records_dir.join(EVAL_RECORD_FILE);
    let case_session_id = staged.session_id.clone();

    match launch_session(staged, |_| {}) {
        Ok(_) => {}
        Err(RuntimeError::CapsuleTrap(msg)) => {
            run.outcome = RunOutcome::LaunchFailed;
            capsule_runtime::report_eprintln!("    session trapped: {msg}");
        }
        Err(error) => {
            run.outcome = RunOutcome::LaunchFailed;
            capsule_runtime::report_eprintln!("    session failed: {error}");
        }
    }

    // After the launch has returned, whatever it returned: every process the session started is
    // dead by then.
    if let Some((check, _)) = &case.check {
        let outcome = check_runner.run(check);
        run.check_result = Some(outcome.result.into());
        run.check = Some(outcome);
    }

    match super::super::trace::eval_trace_summary(&records_dir.join("trace.jsonl")) {
        Ok(summary) => {
            run.exit_status = Some(summary.exit_status);
            run.turns = Some(summary.turns);
            run.input_tokens = Some(summary.input_tokens);
            run.output_tokens = Some(summary.output_tokens);
            run.cached_tokens = summary.cached_tokens;
            run.cache_write_tokens = summary.cache_write_tokens;
            run.agent_ms = Some(summary.agent_ms);
        }
        Err(error) => {
            capsule_runtime::report_eprintln!("    warning: failed to read the trace: {error}");
        }
    }

    // The scoring hook's `eval.jsonl`, from this session's records directory, never from the
    // case's workdir, which its commands write.
    let eval_path = case_eval;
    let run_record = if eval_path.exists() {
        match parse_eval_file(&eval_path) {
            Ok(metrics) => metrics.dataset_run,
            Err(e) => {
                capsule_runtime::report_eprintln!("    warning: failed to read eval.jsonl: {e}");
                None
            }
        }
    } else {
        capsule_runtime::report_eprintln!(
            "    warning: no {EVAL_RECORD_FILE} in {} — the scoring hook writes it there only \
             when its entry declares capabilities.filesystem.records: true",
            staged_records_display(&eval_path)
        );
        None
    };
    run.scores_overall = run_record.as_ref().map(|r| r.overall.clone());

    let overall = run_record
        .as_ref()
        .map(|r| r.overall.clone())
        .unwrap_or_else(|| "no_scores".to_string());

    capsule_runtime::report_println!("    result: {overall}  session: {case_session_id}");
    if let Some(check) = &run.check {
        capsule_runtime::report_println!(
            "    check: {}",
            RunCheckResult::from(check.result).as_str()
        );
    }
    (run, overall, case_workdir, run_record)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn an_old_dataset_line_parses_unchanged() {
        let cases = parse_dataset(
            Path::new("eval.jsonl"),
            "{\"case_id\":\"c1\",\"task_path\":\"t.md\",\"expected\":{\"x\":1}}\n\n",
        )
        .unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].case_id, "c1");
        assert!(cases[0].start.is_none());
        assert!(cases[0].check.is_none());
    }

    #[test]
    fn a_check_is_a_command_or_an_object_with_defaults() {
        let cases = parse_dataset(
            Path::new("eval.jsonl"),
            concat!(
                "{\"case_id\":\"a\",\"task_path\":\"t\",\"start\":\"s\",\"check\":\"make test\"}\n",
                "{\"case_id\":\"b\",\"task_path\":\"t\",\"check\":{\"command\":\"grep x f\"}}\n",
                "{\"case_id\":\"c\",\"task_path\":\"t\",\"check\":{\"command\":\"true\",",
                "\"files\":\"f\",\"timeout_secs\":5}}\n",
            ),
        )
        .unwrap();
        assert_eq!(cases[0].start.as_deref(), Some("s"));
        let a = cases[0].check.as_ref().unwrap();
        assert_eq!(
            (a.command(), a.files(), a.timeout_secs()),
            ("make test", None, 600)
        );
        let b = cases[1].check.as_ref().unwrap();
        assert_eq!(
            (b.command(), b.files(), b.timeout_secs()),
            ("grep x f", None, 600)
        );
        let c = cases[2].check.as_ref().unwrap();
        assert_eq!(
            (c.command(), c.files(), c.timeout_secs()),
            ("true", Some("f"), 5)
        );
    }

    #[test]
    fn a_malformed_line_names_its_line() {
        let error = parse_dataset(
            Path::new("eval.jsonl"),
            "{\"case_id\":\"a\",\"task_path\":\"t\"}\n{\"case_id\":\"b\"}\n",
        )
        .unwrap_err();
        assert_eq!(error.code, E_EVAL_001);
        assert!(error.message.contains("eval.jsonl:2"), "{}", error.message);
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/lib.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::set_permissions(dir.path().join("src"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(
            dir.path().join("src/lib.rs"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        dir
    }

    #[test]
    fn the_digest_follows_the_documented_algorithm() {
        let dir = tree();
        // "a.txt" < "src" < "src/lib.rs", byte-wise.
        let stream: &[u8] =
            b"f 644 a.txt\x001\x00ad 755 src\x00f 644 src/lib.rs\x0013\x00fn main() {}\n";
        assert_eq!(
            tree_digest(dir.path()).unwrap(),
            murmur_artifact::sha256_hex(stream)
        );
        assert_eq!(
            tree_digest(dir.path()).unwrap(),
            tree_digest(dir.path()).unwrap()
        );
    }

    #[test]
    fn the_digest_sees_content_mode_and_name() {
        let dir = tree();
        let before = tree_digest(dir.path()).unwrap();

        fs::write(dir.path().join("a.txt"), "b").unwrap();
        let content = tree_digest(dir.path()).unwrap();
        assert_ne!(content, before);
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        assert_eq!(tree_digest(dir.path()).unwrap(), before);

        fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(tree_digest(dir.path()).unwrap(), before);
        fs::set_permissions(dir.path().join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();

        fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        assert_ne!(tree_digest(dir.path()).unwrap(), before);
    }

    #[test]
    fn a_link_in_a_tree_is_refused() {
        let dir = tree();
        std::os::unix::fs::symlink("/etc", dir.path().join("src/etc")).unwrap();
        let refused = tree_digest(dir.path()).unwrap_err();
        assert!(refused.contains("symbolic link"), "{refused}");
    }

    #[test]
    fn a_copied_start_has_the_source_digest() {
        let dir = tree();
        fs::set_permissions(dir.path().join("src"), fs::Permissions::from_mode(0o555)).unwrap();
        let target = tempfile::tempdir().unwrap();
        let root = capsule_runtime::workdir_fs::WorkdirRoot::open(target.path()).unwrap();
        copy_tree_into(dir.path(), &root).unwrap();
        assert_eq!(
            tree_digest(target.path()).unwrap(),
            tree_digest(dir.path()).unwrap()
        );
        fs::set_permissions(dir.path().join("src"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(target.path().join("src"), fs::Permissions::from_mode(0o755)).unwrap();
    }
}
