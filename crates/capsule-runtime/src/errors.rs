use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("artifact {name}@{version} not found in registry")]
    ArtifactNotFound { name: String, version: String },

    #[error("artifact integrity check failed for {name}@{version}")]
    ArtifactIntegrityFailed { name: String, version: String },

    #[error("lockfile missing artifact entry for '{name}'")]
    LockMissingEntry { name: String },

    #[error(
        "lockfile version mismatch for '{name}': manifest requested {requested}, lock pinned {pinned}"
    )]
    LockVersionMismatch {
        name: String,
        requested: String,
        pinned: String,
    },

    #[error("failed to read artifact archive for {name}@{version}: {message}")]
    ArtifactArchive {
        name: String,
        version: String,
        message: String,
    },

    #[error("failed to compile component for {name}@{version}: {message}")]
    ToolComponentCompile {
        name: String,
        version: String,
        message: String,
    },

    #[error("failed to compile capsule component: {0}")]
    CapsuleCompile(String),

    #[error("failed to create workdir at {path}: {source}")]
    CreateWorkdir {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write tool manifest at {path}: {source}")]
    WriteToolManifest {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read inference.system_prompt_file at {path}: {source}")]
    SystemPromptFileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read inference.compaction.system_prompt_file at {path}: {source}")]
    CompactionSystemPromptFileRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "inference.system_prompt_artifact '{name}': skill.md not found or unreadable; \
         ensure the skill is declared in artifacts: and the capsule was staged"
    )]
    SystemPromptArtifactRead {
        name: String,
        #[source]
        source: std::io::Error,
    },

    #[error("skill source path not found: {path}")]
    SkillSourceNotFound { path: String },

    #[error("skill source directory '{path}' contains no skill.md")]
    SkillSourceMissingSkillMd { path: String },

    #[error("failed to read skill source at {path}: {source}")]
    SkillSourceRead {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid network capability allow entry '{entry}': {message}")]
    InvalidNetworkAllowEntry { entry: String, message: String },

    #[error("invalid filesystem capability scope '{scope}': {message}")]
    InvalidFilesystemScope { scope: String, message: String },

    /// The declared containment floor is stronger than what this host's kernel can back.
    /// Raised in the stage/launch seam, before any registry pull, component compile or workdir
    /// creation — `achieved` always comes from a live host probe, never from the manifest.
    #[error(
        "declared containment class '{declared}' is not achievable on this host (achieved: '{achieved}'): {reason}"
    )]
    ContainmentFloorUnmet {
        declared: murmur_artifact::ContainmentClass,
        achieved: murmur_artifact::ContainmentClass,
        reason: String,
    },

    #[error(
        "capsule component missing export murmur:capsule/run@0.1.0#run; rebuild the artifact against the versioned WIT (run `mur install` for a default artifact, or rebuild from source otherwise)"
    )]
    CapsuleExportMissing,

    #[error(
        "tool component '{name}' missing export murmur:tool/run@0.1.0#run; rebuild the artifact against the versioned WIT (run `mur install` for a default artifact, or rebuild from source otherwise)"
    )]
    ToolExportMissing { name: String },

    #[error("capsule execution trapped: {0}")]
    CapsuleTrap(String),

    #[error(
        "capsule execution exceeded its {seconds}s deadline and was interrupted; \
         raise capabilities.limits.deadline_seconds in murmur.yaml if the capsule \
         legitimately needs longer"
    )]
    CapsuleDeadlineExceeded { seconds: u64 },

    #[error("capsule execution exceeded its configured resource limits: {message}")]
    CapsuleResourceLimit { message: String },

    #[error("failed to initialize WASI for {path}: {message}")]
    WasiInit { path: String, message: String },

    #[error("inference driver is not configured; add inference.driver.artifact to murmur.yaml")]
    DriverNotConfigured,

    #[error("inference driver '{0}' is not installed in the local tool registry")]
    DriverNotInstalled(String),

    #[error("agent loop failed: {0}")]
    AgentLoopFailed(String),

    #[error("internal_port {port} is already bound; choose another port or omit internal_port to use an OS-assigned port")]
    PortInUse { port: u16 },

    #[error("runtime failure: {0}")]
    Runtime(String),
}

impl RuntimeError {
    #[must_use]
    pub fn artifact_not_found(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self::ArtifactNotFound {
            name: name.into(),
            version: version.into(),
        }
    }

    #[must_use]
    pub fn artifact_integrity_failed(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self::ArtifactIntegrityFailed {
            name: name.into(),
            version: version.into(),
        }
    }

    #[must_use]
    pub fn wasi(path: PathBuf, message: impl Into<String>) -> Self {
        Self::WasiInit {
            path: path.display().to_string(),
            message: message.into(),
        }
    }
}
