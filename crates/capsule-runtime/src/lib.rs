// `security::harden_process_dumpable` needs one `unsafe { libc::prctl(...) }` call on Linux;
// `#[allow(unsafe_code)]` on that item overrides this crate-level default of `deny` rather
// than `forbid`, since `forbid` cannot be overridden by a narrower `allow` anywhere in the crate.
#![deny(unsafe_code)]

pub(crate) mod a2a;
#[cfg(test)]
mod a2a_conformance;
#[cfg(test)]
mod a2a_conformance_tests;
pub(crate) mod agent;
#[cfg(feature = "alloc-bench")]
pub mod alloc_bench;
pub mod artifact;
pub mod artifact_config;
pub(crate) mod artifact_removal;
pub mod artifact_supply;
pub mod background_command;
pub mod bindings;
pub(crate) mod cancel;
pub(crate) mod capsule_facts;
pub(crate) mod cgroup;
pub mod child_launch;
pub(crate) mod compiled_forms;
pub mod containment;
pub(crate) mod control_plane;
pub(crate) mod conversation;
pub(crate) mod conversation_import;
pub mod delegation;
pub mod delegation_plane;
pub mod detached;
pub(crate) mod detached_reconcile;
pub mod door_auth;
pub(crate) mod driver_choice;
// Public so the `runtime_out!` / `runtime_err!` macros resolve `$crate::diagnostic` from outside
// this crate, and so `mur run`'s launch path writes its lines the same way the runtime does.
pub(crate) mod credential_gateway;
pub mod diagnostic;
pub(crate) mod dns_resolver;
pub(crate) mod done_check;
pub(crate) mod egress_proxy;
pub mod errors;
pub(crate) mod fence;
pub mod formation;
pub mod formation_credentials;
pub mod formation_launch;
pub(crate) mod gateway_credential;
pub(crate) mod harness_session;
pub(crate) mod hooks;
mod host_facts;
pub mod host_warnings;
pub(crate) mod http_client;
pub(crate) mod identity;
pub(crate) mod images;
pub(crate) mod inference_import;
pub(crate) mod inference_retry;
pub(crate) mod install_grant;
pub(crate) mod instruction_files;
pub(crate) mod interpreter_runtime;
pub(crate) mod keep_working;
pub mod lanes;
pub mod lifeline;
pub mod limits;
pub mod mac_token;
pub(crate) mod member_call;
pub mod model_limits;
pub mod murmur_home;
pub(crate) mod murmur_md;
pub mod namespace_probe;
pub mod network_namespace;
pub(crate) mod network_policy;
pub mod origin;
pub(crate) mod otel;
pub(crate) mod outgoing;
pub mod peer_handoff;
pub(crate) mod peer_tasks;
#[cfg(test)]
mod pinned_port;
pub mod plan;
pub mod precompile;
pub mod process_driver;
pub(crate) mod protected_paths;
pub(crate) mod reachability;
pub(crate) mod recipes;
pub mod records_dir;
pub mod registration;
pub(crate) mod required_fields;
pub mod resource_plane;
pub mod resources;
pub mod retention;
pub mod roster;
pub mod running;
pub mod runtime;
pub mod runtime_dir;
pub(crate) mod sandbox;
pub mod sealed;
pub mod security;
pub mod session_check;
pub mod session_status;
pub(crate) mod shell;
#[cfg(test)]
mod source_scan;
pub mod spawn_credential;
pub mod spawn_envelope;
pub(crate) mod spend;
pub mod staged_runtime;
pub mod state_store;
pub(crate) mod stream_events;
pub(crate) mod streaming;
pub(crate) mod task_io_import;
pub mod temp_dir;
pub(crate) mod tokens_import;
pub(crate) mod tool_annotations;
pub(crate) mod tool_call_progress;
pub mod tool_names;
pub(crate) mod trace;
pub(crate) mod trace_blobs;
pub(crate) mod trust_store;
pub mod types;
pub(crate) mod wit_versions;
pub mod workdir_fs;
pub mod workdir_snapshot;
pub mod workdir_writes;

pub use artifact_config::{
    configured_artifact_names, lower_artifact_config, ARTIFACT_CONFIG_ENV,
    MAX_ARTIFACT_CONFIG_BYTES,
};
pub use artifact_supply::{SourceSupply, SourcedArtifact};
pub use containment::{
    applied_mechanism, check_containment_floor, containment_shortfall_reason,
    detect_containment_decision, detect_sealed_shortfall, detect_userns_grant, doctor_runtime_dir,
    explain_scope, install_from_places, read_only_advisory_for, render_install, render_read_only,
    render_records_dir, render_runtime_dir, render_temp_dir, resolve_runtime_dir_reach,
    resolve_runtime_dir_reaches, ContainmentDecision, DoctorDirReports, ExportsFilesReport,
    FilesystemBoundaryReport, FilesystemRestriction, ImagesReport, PreopenReport, PreopenSurface,
    RuntimeDirReach, RuntimeDirReport, ScopeReport, ShortfallCause, StateStoreReport,
    TempDirReport,
};
pub use diagnostic::{diagnostic_workdir, set_diagnostic_workdir, Emitted};
// `mur run --explain-scope` derives an allowlisted interpreter's runtime directories through the
// same function `stage_session` does, so the report and the session's grants name one set.
// `mur install` keeps host facts through the same derivation, so the first launch keeps them.
pub use interpreter_runtime::{
    capsule_writable_roots, derive_interpreter_runtimes, keep_host_facts,
    DerivedInterpreterRuntime, HostFactsKept, InterpreterRuntimes,
};
// Both pass the same host facts cache, and `session_start.host_facts` reports what a launch kept.
pub use host_facts::{HostFactsCache, HostFactsReason, HostFactsSource, HostFactsUse};
pub use murmur_home::{usable_murmur_home, HomeUnusable, HOME_UNUSABLE_HINT};
pub use namespace_probe::{MountAttempt, MountAttemptError, MountStep, NamespaceReading};
pub use network_namespace::{
    check_egress_namespace, detect_egress_namespace_failure, skip_without_egress_namespace,
    CreationStep, EgressNamespaceBlocker, EgressNamespaceFailure, NamespaceAttemptError,
};
pub use records_dir::{
    records_dir_root, records_group, records_group_under, RecordsDir, HOME_SUBDIR, HOOKS_SUBDIR,
    RECORDS_DIR_ENV, RECORDS_DIR_NAME, RESULTS_SUBDIR, RESULT_FILE,
};
pub use runtime_dir::{
    runtime_dir_inside_warning, runtime_dir_placement, RuntimeDirPlacement, RUNTIME_DIR_ENV,
    RUNTIME_DIR_GUEST_PATH,
};
pub use session_check::{
    copy_tree_into, walk_tree, CheckOutcome, CheckResult, CheckRunner, SessionCheck, TreeEntry,
};
// `mur run --explain-scope` derives the host trust store through the same function
// `stage_session` does, so the report and the session's grants name one set.
pub use trust_store::{derive_trust_store, TrustStorePath};
// `network_policy` is a private module; `preopen_reports` is re-exported because `mur run
// --explain-scope` and `mur doctor` both resolve the preopen set the same way `stage_session`
// does, and one resolver shared by all three is what keeps the report a description of the launch.
pub use network_policy::preopen_reports;
// `sandbox` is a private module; `HostProbe` is re-exported because the freshness of a session's
// host reading is asserted from integration tests outside this crate. `find_on_path` is re-exported
// so a caller outside this crate resolves a binary name the way the shell allowlist and the
// invoked-binary report already resolve one, rather than open-coding the `PATH` walk again.
pub use sandbox::{detect_landlock_abi, find_on_path, landlock_doctor_lines, HostProbe};
pub use sealed::{
    classify_installed_profile, classify_profile_attachment, inspect_installed_profile,
    inspect_profile_attachment, InstalledProfileState, ProfileAttachment, SealedBlocker,
    SealedShortfall, UsernsGrant, SEALED_APPARMOR_ATTACHMENT_PATHS, SEALED_APPARMOR_PROFILE_PATH,
    SEALED_APPARMOR_PROFILE_SHA256,
};
pub use streaming::STREAM_EXTENSION_URI;
// `cgroup` is a private module, but the entry points below are consumed from `murmur-cli` — the
// three test-support probes from its integration tests, the `io.max` reporting from
// `mur run --explain-scope` and the swap-bound reporting from `mur doctor`, each of which has to
// answer the question a launch does without staging a session. Re-exported here rather than
// duplicating a probe once per crate that needs it.
pub use cgroup::{
    active_swap_areas, cgroup_delegation_available, io_max_device_available, probe_io_max,
    probe_swap_control, requires_process_bounding, skip_without_host_support, IoMaxReport,
    IoMaxStatus, SwapControl, SCOPE_SWAP_MAX,
};
// The door's protocol version and error tables: every client of a door sends the header pair,
// and names the error it is answered with through `A2aError::from_code`.
pub use a2a::{
    murmur_state_name, A2aError, MurmurError, A2A_EXTENSIONS_HEADER, A2A_PROTOCOL_VERSION,
    A2A_VERSION_HEADER,
};
pub use child_launch::{
    child_workdir_for, format_workdir_identity, launch_child_capsule, parse_workdir_identity,
    ChildLaunchRequest, LaunchedChild, ParentWorkdirSource, MUR_BINARY_ENV,
    PARENT_WORKDIR_SNAPSHOT_FLAG, READINESS_TOKENS_FLAG, WORKDIR_IDENTITY_FLAG,
};
// The delegation types and constants stay module-qualified beyond these: `Spawner` and
// `SpawnerHandle` are what a caller of `launch_child_capsule` composes, and the rest of the
// module — the outcome, the file, the two headers — is read by name from `delegation::`.
pub use delegation::{CompletionAddress, Spawner, SpawnerHandle};
// The delegating side's own types. `DelegationPlane` is built by `launch_session` and reached
// only through the `delegate-task` dispatch branch, so what leaves this crate is the vocabulary a
// reader of a trace or a tool result needs, plus the bound and its override.
pub use delegation_plane::{
    DelegationLaunch, DelegationOrigin, DelegationPlane, DelegationRequest, DelegationResult,
    StartedDelegation, DELEGATION_RESULT_TIMEOUT, DELEGATION_TIMEOUT_ENV,
};
// `reachability` is a private module, but both of its entry points are consumed from
// `murmur-cli`'s `mur doctor` as well as from `stage_session`, so they are re-exported here — the
// same facade shape `check_staged_runtime_floor` has, without making the module's internals
// (`shebang_interpreter_name`, the probe, the prefix helpers) part of any crate's API.
pub use door_auth::{
    bearer_header, door_posture, is_loopback_bind, public_door_bind_warning, public_door_warning,
    render_public_door_warning, AuthRefusal, DoorAuth, DoorExposure, DoorGrant, DoorToken,
    PublicDoorDisclosure, DOOR_TOKEN_ENV,
};
pub use errors::{RuntimeError, UnreachableEntrypoint};
pub use formation::{FormationId, FormationPeer, FormationPeers};
pub use formation_credentials::{
    FormationAuthority, FormationBundle, FormationMember, FormationToken, FormationVerifier,
    FormationVerifyKey, FORMATION_CALL_SCOPES, FORMATION_CHANNEL_ENV,
};
pub use host_warnings::{
    host_warnings_reported, host_warnings_reported_in, HOST_WARNINGS_REPORTED_ENV,
};
pub use lanes::TaskLane;
#[cfg(unix)]
pub use lifeline::{ChildLifeline, FormationLifeline, MemberLifeline, SpawnerLifeline};
pub use lifeline::{FORMATION_LIFELINE_ENV, SPAWNER_LIFELINE_ENV};
pub use limits::ExecutionLimits;
pub use murmur_artifact::{
    AfterTask, DoneCheck, KeepWorking, LifecycleConfig, LifecycleOverride, TaskAcceptance,
};
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
    ToolchainHelperReport, ToolchainHelperWarning, UnprobedToolchainDriver,
};
pub use registration::{check_roost_health, deregister_session, register_session, SessionOutcome};
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
    find_context_dirs, list_records, locate_message, prune_records, prune_sessions,
    read_context_dir, remove_record, truncate_record, ContextContents, ContextDir,
    HarnessSessionEntry, MessageLocation, MessageStatus, PrunedRecord, PrunedSession, RecordHeader,
    RecordSummary, RemovedRecord, TruncationMarker, TruncationOutcome,
};
pub use roster::{
    admit_roster, admit_roster_file, caller_overflow_fix, caller_overflow_warning, AdmittedMember,
    AdmittedRoster, CallerOverflow, RosterEdge, RosterRefusal,
};
// The running-capsule vocabulary a reader of a record needs. The verbs stay module-qualified —
// `running::list`, `running::verify`, `running::prune`, `running::running_dir` — on the same terms
// as `origin::from_wire`: their bare names say nothing about capsules.
pub use running::{Liveness, ProcessState, RunningGuard, RunningRecord, SignalOutcome};
pub use runtime::{
    check_env_allow_reaches_guests, check_no_reserved_tool_names, has_subprocess_route_at_staging,
    launch_session, launch_session_handling_sigterm, secret_shaped_env_grants, stage_session,
    stripped_env_allow_entries, undeclarable_runtime_pin_role,
    warn_on_gateway_endpoint_in_network_allow, warn_on_interpreter_runtime_grants,
    warn_on_launch_only_gateway_credential, warn_on_secret_shaped_env_grants,
    warn_on_unmetered_gateways, warn_on_userns_restriction_disabled_host_wide,
    warn_on_workdir_exec, SecretShapedEnvGrant, StrippedEnvAllowEntry,
};
// The one question about a variable name a caller outside the runtime needs answered: would the
// credential backstop drop it, and by which pattern. `shell` is otherwise crate-private, and these
// are what `secret_shaped_env_grants` and `check_env_allow_reaches_guests` judge a
// `capabilities.env.allow` entry with.
pub use shell::{
    credential_backstop_drops, credential_backstop_match, BackstopMatch, BackstopPatternSource,
};
pub use spawn_credential::{
    SpawnApproval, SpawnCredential, SPAWN_APPROVAL_HEADER, SPAWN_CREDENTIAL_HEADER,
};
pub use spawn_envelope::{EnvelopeAxis, EnvelopeViolation, SpawnEnvelope};
pub use staged_runtime::check_staged_runtime_floor;
pub use state_store::{
    ensure_state_store, state_store_reports, validate_store_name, STATE_PREOPEN_NAME,
};
pub use trace::{PlanTraceAppender, ResourceTraceAppender};
pub use types::{
    capability_policy_from_runtime_manifest, ArtifactRequest, CapabilityPolicy,
    InstalledArtifactSummary, LaunchEnding, LaunchResult, LockExpectation, ResolvedLockArtifact,
    ResumeMode, ResumeRequest, StageRequest, StagedSession,
};
// The one way the runtime touches a path inside a capsule's workdir, and what it reports when a
// link there is refused.
pub use workdir_fs::{
    EntryKind, RefusalReason, WorkdirError, WorkdirOperation, WorkdirRefusal, WorkdirRoot,
};
// The declaration `--explain-scope` prints and `trace.jsonl` carries as
// `session_start.effective_grants.runtime_writes`. Flat, on the same terms as the other report
// types: a consumer of a scope report reads these names off it.
pub use runtime_dir::SESSION_ID_PLACEHOLDER;
// What a delegated child started with when its parent copied its work folder into the child's:
// the parent's `delegation_start.workdir_snapshot` and the child's
// `session_start.effective_grants.parent_workdir`.
pub use workdir_snapshot::{
    copy_workdir, ParentWorkdirReport, SkipReason, SkippedEntry, SnapshotError, SnapshotLimits,
    WorkdirSnapshot,
};
pub use workdir_writes::{
    render_runtime_writes, RuntimeWriteCondition, RuntimeWriteKind, RuntimeWriteReport,
};
// The served version of each `murmur:*` package, and the WIT levels: the committed, numbered sets
// of those versions, the newest of which is the one this host serves.
pub use wit_versions::{served_wit_level, wit_levels, SERVED_WIT_PACKAGES, WIT_LEVELS_FILE};
