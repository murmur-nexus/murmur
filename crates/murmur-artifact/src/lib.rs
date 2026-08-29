#![forbid(unsafe_code)]

pub mod artifact;
pub mod artifact_ref;
pub mod build;
pub mod build_lints;
pub mod dotenv;
pub mod lockfile;
pub mod manifest;
pub mod manifest_path;
pub mod payload_shape;
pub mod platform;
pub mod registry;
pub mod runtime_manifest;
pub mod secrets;
pub mod security_warnings;
pub mod trace_capture;
pub mod zip_guard;

pub use artifact::{
    load_manifest_from_artifact, load_manifest_from_artifact_bytes,
    load_manifest_yaml_from_artifact, load_manifest_yaml_from_artifact_bytes, ArtifactError,
};
pub use artifact_ref::{ArtifactRef, ArtifactRefError};
pub use build::{build_artifact, BuildError, MAX_ARTIFACT_NAME_LEN, PACKED_MANIFEST_ENTRY};
pub use build_lints::{
    build_warning_link, lint_build_warnings, BuildWarning, RESERVED_ROOT_ENTRIES, W_BLD_001,
    W_BLD_002, W_BLD_003,
};
pub use dotenv::{load_dotenv_non_override, DotenvError};
pub use lockfile::{
    read_lockfile, write_lockfile_atomic, LockedArtifact, LockedSha256, LockfileError, MurmurLock,
    LOCK_VERSION,
};
pub use manifest::{load_manifest, Manifest, ManifestError};
pub use manifest_path::{resolve_manifest_path, MANIFEST_FILENAME};
pub use payload_shape::{
    is_root_wasm_candidate, native_binary_entry, root_wasm_candidates, select_root_wasm,
    select_root_wasm_from_entries, select_root_wasm_in_archive, PayloadShapeError,
    CAPSULE_WASM_ENTRY, NATIVE_BIN_DIR, SKILL_MD_ENTRY, WASM_EXTENSION,
};
pub use platform::current_platform;
pub use registry::{
    is_reserved_version, sha256_hex, sha256_hex_of_reader, verify_sha256, ArtifactMeta,
    LocalRegistry, Platform, PublishResult, Registry, RegistryError, ResolvedArtifact, RuntimeType,
    RESERVED_VERSIONS,
};
pub use runtime_manifest::{
    commit_policy_for_binding, effective_containment_floor, load_runtime_manifest, parse_byte_size,
    parse_duration_secs, parse_hook_config_from_yaml, parse_tool_implementation_from_yaml,
    read_hook_config, read_tool_implementation, AfterTask, ArtifactImplementation, ArtifactRuntime,
    Capabilities, CompactionConfig, ContainmentClass, ContextConfig, ConversationCapabilities,
    ConversationMode, EnvCapabilities, EvalConfig, ExportMode, Exports, FileExport,
    FilesystemCapabilities, HookBinding, HookCommitPolicy, HookConfig, HookExecutionMode,
    HookOverflowPolicy, InferenceConfig, InferenceDriver, InterpreterRuntimeDir,
    InterpreterRuntimeGrant, LifecycleConfig, LifecycleOverride, NetworkCapabilities,
    NetworkConfig, ObservabilityConfig, ParseContainmentClassError, PeerFetchCapabilities,
    PeerFilesExport, ResourceCapabilities, ResourceLimits, RuntimeArtifact, RuntimeManifest,
    RuntimeManifestError, ScorerConfig, ShellCapabilities, StagedRuntimeGrant, StateCapabilities,
    TaskAcceptance, TaskIoCapabilities, TraceConfig, BYTE_SIZE_ACCEPTED_FORM,
    DEFAULT_EXPORT_MAX_BYTES, DEFAULT_PEER_FILES_MAX_BYTES, DEFAULT_PEER_HANDLE_TTL_SECS,
    DEFAULT_SEED_BUDGET, DEFAULT_SEED_OVERFLOW_MARGIN, DURATION_ACCEPTED_FORM,
    PEER_FETCH_ALLOW_ACCEPTED_FORM, PERSISTENT_PEER_HANDLE_TTL_CEILING_SECS,
};
pub use secrets::{scan_yaml_secrets, SecretWarning};
pub use security_warnings::{
    security_warning_link, W_SEC_001, W_SEC_002, W_SEC_003, W_SEC_004, W_SEC_005, W_SEC_006,
    W_SEC_007, W_SEC_008, W_SEC_009, W_SEC_010, W_SEC_011, W_SEC_012, W_SEC_013, W_SEC_014,
    W_SEC_015, W_SEC_016,
};
pub use trace_capture::{
    resolve_trace_capture, ParseTraceCaptureError, TraceCapture, TRACE_CAPTURE_ACCEPTED_VALUES,
};
pub use zip_guard::{
    max_artifact_decompressed_bytes, read_zip_entry_capped, read_zip_entry_to_string_capped,
    resolve_within, sanitize_entry_path, ZipGuardError, DEFAULT_MAX_ARTIFACT_DECOMPRESSED_BYTES,
    MAX_ARTIFACT_DECOMPRESSED_BYTES_ENV,
};
