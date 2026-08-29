// `security::harden_process_dumpable` needs one `unsafe { libc::prctl(...) }` call on Linux;
// `#[allow(unsafe_code)]` on that item overrides this crate-level default of `deny` rather
// than `forbid`, since `forbid` cannot be overridden by a narrower `allow` anywhere in the crate.
#![deny(unsafe_code)]

pub(crate) mod a2a;
pub(crate) mod agent;
pub mod artifact;
pub mod artifact_config;
pub mod bindings;
pub(crate) mod cgroup;
pub mod containment;
pub(crate) mod conversation;
pub(crate) mod conversation_import;
pub(crate) mod egress_proxy;
pub mod errors;
pub(crate) mod hooks;
pub(crate) mod identity;
pub(crate) mod inference_import;
pub mod limits;
pub(crate) mod murmur_md;
pub mod network_namespace;
pub(crate) mod network_policy;
pub(crate) mod otel;
pub(crate) mod outgoing;
pub mod peer_handoff;
pub mod plan;
pub(crate) mod reachability;
pub mod resource_plane;
pub mod resources;
pub mod runtime;
pub(crate) mod sandbox;
pub mod sealed;
pub mod security;
pub(crate) mod shell;
pub mod staged_runtime;
pub mod state_store;
pub(crate) mod streaming;
pub(crate) mod task_io_import;
pub(crate) mod tokens_import;
pub(crate) mod trace;
pub(crate) mod trace_blobs;
pub mod types;

pub use artifact_config::{
    configured_artifact_names, lower_artifact_config, ARTIFACT_CONFIG_ENV,
    MAX_ARTIFACT_CONFIG_BYTES,
};
pub use containment::{
    check_containment_floor, containment_shortfall_reason, detect_achieved_containment,
    detect_sealed_blocker, detect_userns_grant, explain_scope, ExportsFilesReport, ScopeReport,
    StateStoreReport,
};
pub use network_namespace::{
    check_egress_namespace, detect_egress_namespace_blocker, skip_without_egress_namespace,
    EgressNamespaceBlocker,
};
pub use sealed::{
    classify_installed_profile, inspect_installed_profile, InstalledProfileState, SealedBlocker,
    UsernsGrant, SEALED_APPARMOR_PROFILE_PATH, SEALED_APPARMOR_PROFILE_SHA256,
};
// `cgroup` is a private module, but the two test-support entry points below are consumed from
// `murmur-cli`'s integration tests as well as from this crate's own, so they are re-exported here
// rather than duplicating the delegation probe once per crate that has to skip on it.
pub use cgroup::{cgroup_delegation_available, skip_without_host_support};
// `reachability` is a private module, but both of its entry points are consumed from
// `murmur-cli`'s `mur doctor` as well as from `stage_session`, so they are re-exported here — the
// same facade shape `check_staged_runtime_floor` has, without making the module's internals
// (`shebang_interpreter_name`, the probe, the prefix helpers) part of any crate's API.
pub use errors::{RuntimeError, UnreachableEntrypoint};
pub use limits::ExecutionLimits;
pub use murmur_artifact::{AfterTask, LifecycleConfig, LifecycleOverride, TaskAcceptance};
pub use peer_handoff::{
    audience_from_card, handle_id, handle_peer_request, is_peer_path, mint, stored_path_for,
    verify, HandleError, HandlePayload, MintedHandle, PeerError, PeerMintKey, PeerPlane,
    AUDIENCE_HEADER, HANDLE_ID_HEADER, PEER_INBOX_DIR, PEER_PATH_PREFIX,
};
pub use reachability::{
    check_interpreted_entrypoints_reachable, warn_on_unreachable_toolchain_helpers,
    ToolchainHelperWarning,
};
pub use resource_plane::{
    check_export_root, check_peer_files_root, handle_resource_request, reason_phrase,
    symlink_policy, DeclaredExport, ListEntry, ListResponse, ReadResponse, ResourceError,
    ResourcePlane, ResourceResponse, SymlinkPolicy, RESOURCE_PATH_PREFIX,
};
pub use resources::HostResourceLimits;
pub use runtime::{
    launch_session, stage_session, warn_on_interpreter_runtime_grants,
    warn_on_userns_restriction_disabled_host_wide, warn_on_workdir_exec,
};
pub use staged_runtime::check_staged_runtime_floor;
pub use state_store::{
    ensure_state_store, state_store_reports, validate_store_name, STATE_PREOPEN_NAME,
};
pub use trace::ResourceTraceAppender;
pub use types::{
    capability_policy_from_runtime_manifest, ArtifactRequest, CapabilityPolicy,
    InstalledArtifactSummary, LaunchResult, LockExpectation, ResolvedLockArtifact, StageRequest,
    StagedSession,
};
