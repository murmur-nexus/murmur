use std::path::PathBuf;

use thiserror::Error;

/// One `capabilities.shell.allow` entry that resolved to a script whose interpreter's package
/// tree nothing declared could reach inside a `sealed` composed root.
///
/// Defined here rather than in [`crate::reachability`] — which is a `pub(crate)` module — because
/// it is a field of a public [`RuntimeError`] variant and so has to be nameable by anything that
/// matches on one (`murmur-cli`'s `CliError` conversion, above all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreachableEntrypoint {
    /// The `capabilities.shell.allow` entry, verbatim as the manifest wrote it.
    pub binary: String,
    /// Where that entry resolves on this host — the first `PATH` match, i.e. the one `execvp`
    /// would run.
    pub resolved_path: PathBuf,
    /// The bare interpreter name read out of the file's `#!` line, with one level of
    /// `env NAME` indirection resolved. This is the name a covering
    /// `interpreter_runtime`/`staged_runtime` grant would have to declare.
    pub interpreter: String,
}

impl std::fmt::Display for UnreachableEntrypoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "'{}' ({}, a script run by '{}')",
            self.binary,
            self.resolved_path.display(),
            self.interpreter
        )
    }
}

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

    /// An artifact entry's `config:` block cannot be delivered as `MURMUR_ARTIFACT_CONFIG`: it is
    /// not a string-keyed mapping, does not serialize to JSON, or exceeds
    /// [`crate::artifact_config::MAX_ARTIFACT_CONFIG_BYTES`]. Raised at staging (and from `mur run
    /// --explain-scope`), before any registry pull, workdir creation or component instantiation,
    /// so a malformed channel is refused once at launch rather than one tool call at a time.
    #[error("invalid config for artifact '{artifact}': {message}")]
    InvalidArtifactConfig { artifact: String, message: String },

    /// A `capabilities.state.store` name is not a single usable directory segment under
    /// `~/.murmur/state/`. Raised at staging (and from `mur run --explain-scope`), before any
    /// registry pull, workdir creation or component instantiation, so a bad name never gets as far
    /// as creating something on disk.
    #[error("invalid state store name '{store}': {message}")]
    InvalidStateStore { store: String, message: String },

    /// A `context.record_store` value, or a `mur run --context` value, is not a single usable
    /// directory segment of a conversation record path. Raised at staging, before any registry
    /// pull, workdir creation or component instantiation, so a bad name never gets as far as
    /// creating something on disk.
    ///
    /// Distinct from [`Self::InvalidStateStore`] because the remedies point at different keys, and
    /// because a record is on by default while a state store is declared: an operator seeing this
    /// may not have written a `context:` block at all, only a `--context` flag.
    #[error("invalid {field} '{value}': {message}")]
    InvalidConversationRecord {
        field: String,
        value: String,
        message: String,
    },

    /// `mur run --resume` named a session whose context kept no conversation record on disk, so
    /// there is nothing to continue. Raised at staging, before this launch's workdir exists: a
    /// resume that silently started fresh would be indistinguishable from one that worked.
    #[error(
        "cannot resume session {session}: context '{context_id}' has no conversation record ({reason})"
    )]
    ResumeRecordMissing {
        session: String,
        context_id: String,
        reason: String,
    },

    /// `mur run --resume-mode compact` on a capsule with no hook bound to `on-compaction`. There
    /// is nothing to produce the summary, and falling back to `full` would silently give the
    /// operator the mode they did not ask for.
    #[error(
        "--resume-mode compact needs a hook bound to on-compaction; this capsule declares none"
    )]
    ResumeCompactionHookMissing,

    /// The state store directory a validated name resolves to could not be resolved or created.
    /// Distinct from [`Self::InvalidStateStore`]: the declaration is well-formed and the failure is
    /// the host's — an unset `HOME`, or a path that cannot be made a `0700` directory.
    #[error("state store '{store}' is unavailable at {path}: {message}")]
    StateStoreUnavailable {
        store: String,
        path: String,
        message: String,
    },

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
    /// A capsule declares an `exports.files.root` that, once resolved against the session's
    /// accessible workdir, lands somewhere else — the root already exists as a symlink pointing
    /// out of it.
    ///
    /// Refused at launch rather than per request: an export root outside the workdir is a
    /// misconfiguration the operator can fix, and discovering it one served file at a time would
    /// mean the first file had already left.
    #[error(
        "{field} '{declared}' resolves to '{resolved}', which is outside the capsule \
         workdir '{workdir}'"
    )]
    ExportRootOutsideWorkdir {
        /// The manifest key that declared it — `exports.files.root` or
        /// `exports.peer_files.root`. Carried rather than hard-coded because the two roots are
        /// separate authorisers and an operator fixing one must be told which.
        field: String,
        declared: String,
        resolved: String,
        workdir: String,
    },

    /// A capsule declares `exports.peer_files` together with `lifecycle.after_task: sleep`, and
    /// either declared no `max_ttl` or declared one above the persistent ceiling.
    ///
    /// An ephemeral capsule needs no ceiling: teardown destroys the minting key, so every
    /// outstanding handle stops verifying at once and the declared lifetime can never outlive the
    /// process. `sleep` withdraws that bound deliberately, which makes the declared lifetime the
    /// only one there is — so it has to be declared, and it has to be short.
    #[error(
        "exports.peer_files with lifecycle.after_task: sleep requires exports.peer_files.max_ttl \
         to be declared and at most {ceiling_secs}s{}; a handle's lifetime is not a durability \
         mechanism",
        match declared_secs {
            Some(secs) => format!(" (declared {secs}s)"),
            None => String::new(),
        }
    )]
    PersistentCapsuleNeedsHandleTtl {
        /// The declared `max_ttl` in seconds, or `None` when the manifest declared none.
        declared_secs: Option<u64>,
        ceiling_secs: u64,
    },

    /// A capsule declares `capabilities.shell.staged_runtime` without an effective `sealed`
    /// containment floor.
    ///
    /// Deliberately distinct from [`Self::ContainmentFloorUnmet`]: that one compares the declared
    /// floor against what the *host* can back, and its remedy is to lower the floor or move hosts.
    /// This one is decided against the declared floor alone and never looks at the host at all, so
    /// it fires identically on a machine that could deliver `sealed` — a staged runtime has no
    /// composed root to be staged into unless the capsule asked for one, and quietly launching it
    /// without the mount would leave the interpreter simply absent. Its remedy is to raise the
    /// declared floor (or drop the grant), which is the opposite advice.
    ///
    /// `binaries` names every offending `staged_runtime` binary so the operator does not have to
    /// re-run to find the second one.
    #[error(
        "capabilities.shell.staged_runtime is declared for {} but the effective containment floor \
         is '{declared}' — staging a runtime tree requires the 'sealed' floor, because there is no \
         composed root to bind-mount it into below that",
        .binaries.join(", ")
    )]
    StagedRuntimeRequiresSealed {
        binaries: Vec<String>,
        declared: murmur_artifact::ContainmentClass,
    },

    /// A `sealed` capsule allowlists a script whose interpreter's package tree nothing declared
    /// could make reachable inside the composed root.
    ///
    /// Deliberately distinct from [`Self::StagedRuntimeRequiresSealed`], which it sits next to in
    /// `stage_session`: that one fires when a grant *was* declared at too low a floor, this one
    /// fires when no grant was declared at all and the floor is already `sealed`. Their remedies
    /// point in opposite directions — raise the floor there, add a grant here.
    ///
    /// `entries` names every offending allowlist entry so an operator fixing this does not have to
    /// re-run to discover the second script, following the same precedent.
    #[error(
        "capabilities.shell.allow grants {} under the 'sealed' containment floor, but nothing \
         declared makes the interpreted entrypoint's own package tree reachable inside the \
         composed root — the script's ELF/DT_NEEDED closure is empty, so staging it stages \
         nothing its interpreter imports, and the capsule would fail with a module-not-found \
         error partway into a run rather than here. Declare \
         capabilities.shell.interpreter_runtime or capabilities.shell.staged_runtime naming the \
         interpreter (measure the real directories with \
         `strace -f -e trace=openat,getdents64 <the command>`), or use a copy of the interpreter \
         and its packages that already lives under a fixed sealed runtime path",
        .entries
            .iter()
            .map(UnreachableEntrypoint::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    )]
    ShellBinaryPackageUnreachable { entries: Vec<UnreachableEntrypoint> },

    /// A `sealed` session's composed root could not be built in the forked child, *after* the
    /// pre-launch probe reported the mechanism available.
    ///
    /// Deliberately distinct from [`Self::ContainmentFloorUnmet`]: that one is a refusal decided
    /// before anything is staged, on a host that never claimed to offer `sealed`. This one is a
    /// host that claimed it and then failed to deliver — a race, a mount table that changed under
    /// the runtime, or an edge case the cheap probe cannot catch. Conflating the two would tell an
    /// operator to lower their declared floor when the real answer is that something moved.
    ///
    /// `detail` is the message the child wrote to the `pre_exec` diagnostic pipe, which names the
    /// exact step (`unshare`, a specific bind mount, `pivot_root`) and its errno.
    #[error(
        "the sealed containment class was achievable at launch but its composed root could not be \
         built for this subprocess: {detail}"
    )]
    SealedRootConstructionFailed { detail: String },

    /// This host cannot give a capsule's native subprocess tree its own network namespace, so the
    /// egress boundary that mediates `capabilities.network.allow` cannot be built.
    ///
    /// Raised before any registry pull, component compile or workdir creation, and raised for
    /// **every** Linux capsule that can spawn a subprocess — including one whose
    /// `capabilities.network.allow` is empty. That breadth is the point: with the namespace
    /// missing, an empty allowlist would mean *unrestricted* egress rather than none, because the
    /// seccomp `connect`/`sendto` interception that used to back it was deleted as unsound rather
    /// than kept as a fallback. There is deliberately no path that continues at reduced
    /// enforcement.
    ///
    /// Deliberately distinct from [`Self::ContainmentFloorUnmet`], which this would otherwise
    /// look like: that one compares a *declared* containment class against what the host can
    /// back, and its remedy is to lower the declared floor. A network namespace is not part of
    /// the containment ladder — every class needs it equally — so lowering the floor fixes
    /// nothing here, and offering that advice would be actively misleading.
    #[error(
        "this host cannot give the capsule's subprocess tree its own network namespace, so \
         capabilities.network.allow cannot be enforced for it: {reason}"
    )]
    EgressNamespaceUnavailable {
        blocker: crate::network_namespace::EgressNamespaceBlocker,
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

    #[error(
        "shell subprocess '{binary}' was killed for exceeding \
         capabilities.resources.{limit}: {detail}"
    )]
    ShellResourceLimitExceeded {
        binary: String,
        limit: String,
        detail: String,
    },

    #[error(
        "this capsule can spawn native subprocesses but no cgroup v2 scope could be created to \
         bound them ({reason}); on Linux the runtime refuses to launch rather than run a \
         subprocess tree with no aggregate memory/pids/cpu ceiling — see the systemd user \
         delegation requirement in docs/content/reference/resource-limits-manual-verification.md"
    )]
    CgroupDelegationUnavailable { reason: String },

    #[error(
        "session workdir grew to {observed_bytes} bytes, past the {max_bytes} byte ceiling \
         (capabilities.resources.workdir_max_bytes)"
    )]
    WorkdirSizeExceeded { max_bytes: u64, observed_bytes: u64 },

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

    /// A session whose manifest declares `capabilities.spawn.allow` could not register with
    /// `mur-roost`, so the daemon does not hold the grants it would referee that session's
    /// delegations against.
    ///
    /// Fatal rather than a warning: a capsule that can delegate but is unknown to the daemon is a
    /// capsule running outside the only thing that bounds what it may spawn. A capsule that
    /// declares no spawn capability never registers and never reaches this.
    ///
    /// `reason` is the transport or daemon-side failure verbatim. Neither this nor anything
    /// derived from it carries a credential or an approval: the registration request's tokens
    /// travel in headers, and the HTTP client never puts a request header into an error.
    #[error(
        "failed to register this session with mur-roost at {roost_url}: {reason}; a capsule \
         declaring capabilities.spawn.allow must be known to the daemon that referees its \
         spawns, so the launch is refused rather than run unrefereed"
    )]
    SpawnRegistrationFailed { roost_url: String, reason: String },

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
