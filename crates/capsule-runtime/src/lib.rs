// `security::harden_process_dumpable` needs one `unsafe { libc::prctl(...) }` call on Linux;
// `#[allow(unsafe_code)]` on that item overrides this crate-level default of `deny` rather
// than `forbid`, since `forbid` cannot be overridden by a narrower `allow` anywhere in the crate.
#![deny(unsafe_code)]

pub(crate) mod a2a;
pub(crate) mod agent;
pub mod artifact;
pub mod bindings;
pub(crate) mod cgroup;
pub(crate) mod egress_proxy;
pub mod containment;
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
pub mod plan;
pub mod resources;
pub mod runtime;
pub(crate) mod sandbox;
pub mod sealed;
pub mod security;
pub mod staged_runtime;
pub(crate) mod shell;
pub(crate) mod streaming;
pub(crate) mod trace;
pub mod types;

pub use containment::{
    check_containment_floor, containment_shortfall_reason, detect_achieved_containment,
    detect_achieved_containment_for, detect_sealed_blocker, explain_scope, ScopeReport,
};
pub use network_namespace::{
    check_egress_namespace, detect_egress_namespace_blocker, EgressNamespaceBlocker,
};
pub use sealed::SealedBlocker;
pub use errors::RuntimeError;
pub use limits::ExecutionLimits;
pub use murmur_artifact::{AfterTask, LifecycleConfig, LifecycleOverride, TaskAcceptance};
pub use resources::HostResourceLimits;
pub use runtime::{
    launch_session, stage_session, warn_on_interpreter_runtime_grants, warn_on_workdir_exec,
};
pub use staged_runtime::check_staged_runtime_floor;
pub use types::{
    capability_policy_from_runtime_manifest, ArtifactRequest, CapabilityPolicy,
    InstalledArtifactSummary, LaunchResult, LockExpectation, ResolvedLockArtifact, StageRequest,
    StagedSession,
};
