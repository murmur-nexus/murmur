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
pub mod child_launch;
pub mod containment;
pub(crate) mod conversation;
pub(crate) mod conversation_import;
pub mod detached;
pub(crate) mod egress_proxy;
pub mod errors;
pub(crate) mod fence;
pub(crate) mod hooks;
pub(crate) mod http_client;
pub(crate) mod identity;
pub(crate) mod inference_import;
pub mod lanes;
pub mod limits;
pub mod mac_token;
pub(crate) mod murmur_md;
pub mod network_namespace;
pub(crate) mod network_policy;
pub mod origin;
pub(crate) mod otel;
pub(crate) mod outgoing;
pub mod peer_handoff;
pub mod plan;
pub(crate) mod reachability;
pub mod registration;
pub mod resource_plane;
pub mod resources;
pub mod retention;
pub mod runtime;
pub(crate) mod sandbox;
pub mod sealed;
pub mod security;
pub(crate) mod shell;
pub mod spawn_credential;
pub mod spawn_envelope;
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
pub use child_launch::{
    child_workdir_for, launch_child_capsule, ChildLaunchRequest, LaunchedChild, MUR_BINARY_ENV,
};
// `reachability` is a private module, but both of its entry points are consumed from
// `murmur-cli`'s `mur doctor` as well as from `stage_session`, so they are re-exported here — the
// same facade shape `check_staged_runtime_floor` has, without making the module's internals
// (`shebang_interpreter_name`, the probe, the prefix helpers) part of any crate's API.
pub use errors::{RuntimeError, UnreachableEntrypoint};
pub use lanes::TaskLane;
pub use limits::ExecutionLimits;
pub use murmur_artifact::{AfterTask, LifecycleConfig, LifecycleOverride, TaskAcceptance};
// The types and header names are flat; `origin::from_wire` and `origin::stamp_for_peer` stay
// module-qualified, because their bare names say nothing about tasks.
pub use origin::{TaskOrigin, TaskProvenance, TrustClass, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
pub use peer_handoff::{
    audience_from_card, handle_id, handle_peer_request, is_peer_path, mint, stored_path_for,
    verify, HandleError, HandlePayload, MintedHandle, PeerError, PeerMintKey, PeerPlane,
    AUDIENCE_HEADER, HANDLE_ID_HEADER, PEER_INBOX_DIR, PEER_PATH_PREFIX,
};
pub use reachability::{
    check_interpreted_entrypoints_reachable, warn_on_unreachable_toolchain_helpers,
    ToolchainHelperWarning,
};
pub use registration::{deregister_session, register_session, SessionOutcome};
pub use resource_plane::{
    check_export_root, check_peer_files_root, handle_resource_request, reason_phrase,
    symlink_policy, DeclaredExport, ListEntry, ListResponse, ReadResponse, ResourceError,
    ResourcePlane, ResourceResponse, SymlinkPolicy, RESOURCE_PATH_PREFIX,
};
pub use resources::HostResourceLimits;
// `retention` is its own public module rather than a set of flat re-exports: `mur conversation`
// consumes six of its entry points and four of its types, and a facade that wide is a second
// place for the names to drift.
pub use retention::{
    list_records, locate_message, prune_records, prune_sessions, remove_record, truncate_record,
    MessageLocation, MessageStatus, PrunedRecord, PrunedSession, RecordHeader, RecordSummary,
    RemovedRecord, TruncationMarker, TruncationOutcome,
};
pub use runtime::{
    launch_session, stage_session, warn_on_interpreter_runtime_grants,
    warn_on_userns_restriction_disabled_host_wide, warn_on_workdir_exec,
};
pub use spawn_credential::{
    SpawnApproval, SpawnCredential, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};
pub use spawn_envelope::{EnvelopeAxis, EnvelopeViolation, SpawnEnvelope};
pub use staged_runtime::check_staged_runtime_floor;
pub use state_store::{
    ensure_state_store, state_store_reports, validate_store_name, STATE_PREOPEN_NAME,
};
pub use trace::ResourceTraceAppender;
pub use types::{
    capability_policy_from_runtime_manifest, ArtifactRequest, CapabilityPolicy,
    InstalledArtifactSummary, LaunchResult, LockExpectation, ResolvedLockArtifact, ResumeMode,
    ResumeRequest, StageRequest, StagedSession,
};
