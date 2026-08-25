use std::fmt;

use capsule_runtime::RuntimeError;
use murmur_artifact::{BuildError, ManifestError, RegistryError, MANIFEST_FILENAME};

// Manifest parsing
pub const E_MAN_001: &str = "E-MAN-001"; // missing required field
pub const E_MAN_002: &str = "E-MAN-002"; // YAML syntax error
pub const E_MAN_003: &str = "E-MAN-003"; // field type mismatch

// Registry
pub const E_REG_001: &str = "E-REG-001"; // artifact not found
pub const E_REG_002: &str = "E-REG-002"; // artifact integrity check failed
pub const E_REG_003: &str = "E-REG-003"; // artifact already exists (conflict)
pub const E_REG_004: &str = "E-REG-004"; // reserved version string
pub const E_REG_005: &str = "E-REG-005"; // registry-resolved artifact conflicts with murmur.lock

// Capsule execution
pub const E_RUN_001: &str = "E-RUN-001"; // capsule trap
pub const E_RUN_002: &str = "E-RUN-002"; // missing linker import (WASI interface not linked)
pub const E_RUN_003: &str = "E-RUN-003"; // lock version mismatch or missing lock entry
pub const E_RUN_004: &str = "E-RUN-004"; // capsule wasm not found at expected path
pub const E_RUN_005: &str = "E-RUN-005"; // inference driver not configured in manifest
pub const E_RUN_006: &str = "E-RUN-006"; // inference driver artifact not installed
pub const E_RUN_007: &str = "E-RUN-007"; // agent loop failed at runtime
pub const E_RUN_008: &str = "E-RUN-008"; // required artifact not installed locally
pub const E_RUN_009: &str = "E-RUN-009"; // system prompt file could not be read
pub const E_RUN_010: &str = "E-RUN-010"; // network.internal_port already in use
pub const E_RUN_011: &str = "E-RUN-011"; // subprocess killed for exceeding a capabilities.resources limit
pub const E_RUN_012: &str = "E-RUN-012"; // no cgroup v2 scope could be delegated on Linux
pub const E_RUN_013: &str = "E-RUN-013"; // session workdir grew past capabilities.resources.workdir_max_bytes
pub const E_RUN_014: &str = "E-RUN-014"; // a sealed session's composed root could not be built after the host probe had cleared it

// Capability enforcement
pub const E_CAP_001: &str = "E-CAP-001"; // capabilities.network.allow entry could not be parsed
pub const E_CAP_002: &str = "E-CAP-002"; // capabilities.filesystem.scope value is not a usable workdir subpath
pub const E_CAP_003: &str = "E-CAP-003"; // host cannot meet the declared containment class
pub const E_CAP_004: &str = "E-CAP-004"; // staged_runtime declared without a sealed containment floor
pub const E_CAP_005: &str = "E-CAP-005"; // host cannot give the subprocess tree its own network namespace
pub const E_CAP_006: &str = "E-CAP-006"; // sealed capsule allowlists a script whose interpreter's package tree nothing declared reaches
pub const E_CAP_007: &str = "E-CAP-007"; // exports.files.root resolves outside the capsule workdir
pub const E_CAP_008: &str = "E-CAP-008"; // persistent capsule declares exports.peer_files without a short enough max_ttl

// Build lints
pub const E_BLD_001: &str = "E-BLD-001"; // artifact name is not a valid identifier
pub const E_BLD_002: &str = "E-BLD-002"; // requires_files entry is unsafe or collides in the archive
pub const E_BLD_003: &str = "E-BLD-003"; // packed entry set is not a launchable payload

// Host I/O
pub const E_IO_001: &str = "E-IO-001"; // file or directory not found
pub const E_IO_002: &str = "E-IO-002"; // permission denied (host, not capsule)
pub const E_IO_003: &str = "E-IO-003"; // general I/O error

// Config
#[cfg(feature = "beta-mur-new")]
pub const E_CFG_001: &str = "E-CFG-001"; // inference provider not configured; wizard requires TTY
pub const E_CFG_002: &str = "E-CFG-002"; // `mur config set` given an unsupported dotted key

#[derive(Debug, Clone)]
pub struct CliError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl CliError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn with_hint(
        code: &'static str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error[{}]: {}", self.code, self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {}", hint)?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {}

impl From<RuntimeError> for CliError {
    fn from(error: RuntimeError) -> Self {
        match error {
            RuntimeError::CapsuleTrap(message) => {
                if message.contains("matching implementation was not found in the linker") {
                    linker_error_from_message(&message)
                } else {
                    CliError::new(E_RUN_001, format!("capsule execution trapped: {message}"))
                }
            }
            RuntimeError::CapsuleDeadlineExceeded { seconds } => CliError::with_hint(
                E_RUN_001,
                format!("capsule execution exceeded its {seconds}s deadline and was interrupted"),
                "the capsule ran longer than capabilities.limits.deadline_seconds — check for an \
                 unbounded loop, or raise that limit in murmur.yaml if the work genuinely needs longer",
            ),
            RuntimeError::CapsuleResourceLimit { message } => CliError::with_hint(
                E_RUN_001,
                format!("capsule execution exceeded its configured resource limits: {message}"),
                "the capsule requested more memory or table space than capabilities.limits allows — \
                 check for a runaway allocation, or raise the limit in murmur.yaml",
            ),
            // The three host-process (OS-level) resource bounds, kept distinct from E-RUN-001's
            // WASM-guest limits above: those fire from a wasmtime trap inside the store, these
            // from rlimits, a cgroup, or the workdir check applied to native subprocesses.
            // Delegate to the RuntimeError Display text so the message can't drift from
            // capsule-runtime's errors.rs — see the ToolExportMissing precedent below.
            error @ RuntimeError::ShellResourceLimitExceeded { .. } => CliError::with_hint(
                E_RUN_011,
                error.to_string(),
                "the subprocess crossed a host resource ceiling — raise that field under \
                 capabilities.resources in murmur.yaml if the work genuinely needs more, or fix \
                 the runaway that hit it",
            ),
            error @ RuntimeError::CgroupDelegationUnavailable { .. } => CliError::with_hint(
                E_RUN_012,
                error.to_string(),
                "the systemd user unit `mur` runs under needs `Delegate=yes` for memory, pids, \
                 cpu and io — see docs/content/reference/resource-limits-manual-verification.md",
            ),
            // Distinct from E-CAP-003 on purpose: that one is a pre-launch refusal by a host that
            // never claimed to offer `sealed`, so the remedy is to lower the floor or move hosts.
            // This one is a host that cleared the probe and then failed to build the root, so the
            // remedy is to look at what moved underneath it.
            error @ RuntimeError::SealedRootConstructionFailed { .. } => CliError::with_hint(
                E_RUN_014,
                error.to_string(),
                "the host cleared the sealed probe at launch and then failed to construct the \
                 composed root — check that the mount namespace was not restricted mid-session \
                 (AppArmor profile reloaded, container policy changed), re-run `mur run \
                 --explain-scope` to re-probe, and see \
                 docs/content/reference/sealed-containment-manual-verification.md",
            ),
            error @ RuntimeError::WorkdirSizeExceeded { .. } => CliError::with_hint(
                E_RUN_013,
                error.to_string(),
                "raise capabilities.resources.workdir_max_bytes in murmur.yaml if the capsule \
                 legitimately writes this much, or find what is filling the workdir",
            ),
            RuntimeError::ArtifactNotFound { name, version } => CliError::with_hint(
                E_REG_001,
                format!("artifact {name}@{version} not found in registry"),
                "run `mur publish` first, or check registry config in murmur.yaml",
            ),
            RuntimeError::ArtifactIntegrityFailed { name, version } => CliError::with_hint(
                E_REG_002,
                format!("artifact integrity check failed for {name}@{version}"),
                "artifact on disk does not match murmur.lock — re-publish or delete the lock",
            ),
            RuntimeError::LockMissingEntry { name } => CliError::new(
                E_RUN_003,
                format!("murmur.lock missing artifact entry for '{name}'"),
            ),
            RuntimeError::LockVersionMismatch {
                name,
                requested,
                pinned,
            } => CliError::new(
                E_RUN_003,
                format!(
                    "lockfile version mismatch for '{name}': manifest requested {requested}, lock pinned {pinned}"
                ),
            ),
            RuntimeError::ArtifactArchive {
                name,
                version,
                message,
            } => CliError::new(
                E_IO_003,
                format!("failed to read artifact archive for {name}@{version}: {message}"),
            ),
            RuntimeError::ToolComponentCompile {
                name,
                version,
                message,
            } => CliError::new(
                E_RUN_001,
                format!("failed to compile component for {name}@{version}: {message}"),
            ),
            RuntimeError::CapsuleCompile(message) => {
                CliError::new(E_RUN_001, format!("failed to compile capsule component: {message}"))
            }
            RuntimeError::CreateWorkdir { path, source } => CliError::new(
                E_IO_003,
                format!("failed to create workdir at {path}: {source}"),
            ),
            RuntimeError::WriteToolManifest { path, source } => CliError::new(
                E_IO_003,
                format!("failed to write tool manifest at {path}: {source}"),
            ),
            RuntimeError::SystemPromptFileRead { path, source } => CliError::new(
                E_RUN_009,
                format!("failed to read inference.system_prompt_file at {path}: {source}"),
            ),
            RuntimeError::CompactionSystemPromptFileRead { path, source } => CliError::new(
                E_RUN_009,
                format!(
                    "failed to read inference.compaction.system_prompt_file at {path}: {source}"
                ),
            ),
            RuntimeError::SystemPromptArtifactRead { name, source } => CliError::new(
                E_RUN_009,
                format!(
                    "inference.system_prompt_artifact '{name}': skill.md not found or unreadable \
                     ({source}); ensure the skill is declared in artifacts: and was staged"
                ),
            ),
            RuntimeError::SkillSourceNotFound { path } => CliError::new(
                E_IO_001,
                format!("skill source path not found: {path}"),
            ),
            RuntimeError::SkillSourceMissingSkillMd { path } => CliError::new(
                E_IO_001,
                format!("skill source directory '{path}' contains no skill.md"),
            ),
            RuntimeError::SkillSourceRead { path, source } => CliError::new(
                E_IO_003,
                format!("failed to read skill source at {path}: {source}"),
            ),
            RuntimeError::InvalidNetworkAllowEntry { entry, message } => CliError::new(
                E_CAP_001,
                format!("invalid network allow entry '{entry}': {message}"),
            ),
            RuntimeError::InvalidFilesystemScope { scope, message } => CliError::new(
                E_CAP_002,
                format!("invalid filesystem scope '{scope}': {message}"),
            ),
            // Delegate the message to the RuntimeError Display text so the declared/achieved
            // pair and the missing-mechanism reason cannot drift from capsule-runtime's
            // `containment` module, which is what actually decided the refusal.
            error @ RuntimeError::ContainmentFloorUnmet {
                declared, achieved, ..
            } => {
                let hint = format!(
                    "lower the declared floor to '{achieved}' (capabilities.containment in \
                     murmur.yaml, containment in .murmur/config.yaml, or --containment), or run \
                     on a host that provides '{declared}'"
                );
                CliError::with_hint(E_CAP_003, error.to_string(), hint)
            }
            // An export is a disclosure, not a grant, so this is not a containment shortfall and
            // no floor change fixes it: the root itself names somewhere outside the capsule. The
            // hint therefore points at the path, never at the containment ladder.
            error @ RuntimeError::ExportRootOutsideWorkdir { .. } => CliError::with_hint(
                E_CAP_007,
                error.to_string(),
                "point the export root at a directory inside the capsule workdir. A root \
                 that already exists as a symlink out of the workdir is refused whole rather than \
                 followed — see docs/content/reference/resource-plane.md",
            ),
            // The remedy is never "declare a longer handle lifetime", so the hint does not offer
            // one: a consumer that needs bytes after teardown wants the workdir read again, not a
            // credential that outlives the process that could verify it.
            error @ RuntimeError::PersistentCapsuleNeedsHandleTtl { .. } => CliError::with_hint(
                E_CAP_008,
                error.to_string(),
                "declare `exports.peer_files.max_ttl: 15m` or shorter, or drop \
                 `lifecycle.after_task: sleep` so teardown bounds every handle instead. A \
                 consumer that needs these bytes after the capsule is gone should have the \
                 operator relaunch the runtime against the still-present workdir and request \
                 again — see docs/content/reference/resource-plane.md",
            ),
            // Distinct from E-CAP-003 above, and the remedies point in opposite directions: that
            // one means the host is too weak for the declared floor (lower it, or move hosts),
            // this one means the capsule's own declaration is too weak for what it asked for
            // (raise it, or drop the grant). Deciding it on the declared floor alone is why this
            // fires identically on a host that could deliver `sealed`.
            error @ RuntimeError::StagedRuntimeRequiresSealed { .. } => CliError::with_hint(
                E_CAP_004,
                error.to_string(),
                "set `capabilities.containment: sealed` in murmur.yaml (or pass \
                 `--containment sealed`) so the capsule gets a composed root to stage the runtime \
                 into, or remove the capabilities.shell.staged_runtime grant. Run `mur run \
                 --explain-scope` to see the declared grants and whether this host can back \
                 `sealed` — see docs/content/reference/containment.md",
            ),
            // Sits next to E-CAP-004 and is deliberately not it: that one means a grant was
            // declared at too low a floor (raise the floor, or drop the grant), this one means the
            // floor is already `sealed` and no grant was declared at all for a script that needs
            // one (add a grant). Handing an operator E-CAP-004's "raise the floor" advice here
            // would send them to change the one thing that is already correct.
            error @ RuntimeError::ShellBinaryPackageUnreachable { .. } => CliError::with_hint(
                E_CAP_006,
                error.to_string(),
                "declare `capabilities.shell.interpreter_runtime` (or `staged_runtime`) for the \
                 interpreter named above, listing the directories its import machinery actually \
                 reads — measure them on this host with \
                 `strace -f -e trace=openat,getdents64 <the command>` rather than guessing, since \
                 murmur deliberately does not try to derive an interpreted program's import \
                 closure. Alternatively point `capabilities.shell.allow` at a copy that already \
                 lives under a fixed sealed runtime path (a distro `/usr/bin` interpreter and its \
                 system packages need no grant at all). See \
                 docs/content/reference/containment.md",
            ),
            // Distinct from both E-CAP-003 and E-CAP-004, and none of the three remedies help
            // with another: this one is not about the containment ladder at all. A network
            // namespace is what makes `capabilities.network.allow` mean anything for a native
            // subprocess since the seccomp connect/sendto interception was retired, and every
            // Linux capsule that can spawn one needs it at every containment class — so neither
            // raising nor lowering a declared floor changes this answer. The hint therefore names
            // the host mechanism, never the manifest.
            error @ RuntimeError::EgressNamespaceUnavailable { .. } => CliError::with_hint(
                E_CAP_005,
                error.to_string(),
                "the refusal above names the exact remediation for this host. Run `mur doctor` \
                 to see what this machine can back — see \
                 docs/content/reference/network-namespace-egress-proxy-manual-verification.md",
            ),
            // Delegate to the RuntimeError Display text so the versioned interface
            // name and rebuild hint can't drift from capsule-runtime's errors.rs.
            error @ RuntimeError::CapsuleExportMissing => {
                CliError::new(E_RUN_001, error.to_string())
            }
            error @ RuntimeError::ToolExportMissing { .. } => {
                CliError::new(E_RUN_001, error.to_string())
            }
            RuntimeError::WasiInit { path, message } => CliError::new(
                E_IO_003,
                format!("failed to initialize WASI for {path}: {message}"),
            ),
            RuntimeError::DriverNotConfigured => CliError::with_hint(
                E_RUN_005,
                "inference driver is not configured; add inference.driver.artifact to murmur.yaml",
                "add an inference.driver.artifact field naming the driver artifact to use",
            ),
            RuntimeError::DriverNotInstalled(name) => CliError::with_hint(
                E_RUN_006,
                format!("inference driver '{name}' is not installed in the local tool registry"),
                "declare the driver artifact in murmur.yaml artifacts: and run `mur run` to install it",
            ),
            RuntimeError::AgentLoopFailed(message) => CliError::new(
                E_RUN_007,
                format!("agent loop failed: {message}"),
            ),
            RuntimeError::PortInUse { port } => CliError::with_hint(
                E_RUN_010,
                format!("internal_port {port} is already bound"),
                "choose another port or omit network.internal_port to use an OS-assigned port",
            ),
            RuntimeError::Runtime(message) => {
                if message.contains("matching implementation was not found in the linker") {
                    linker_error_from_message(&message)
                } else {
                    CliError::new(
                        E_IO_003,
                        format!("runtime failure (debug): {:?}", RuntimeError::Runtime(message)),
                    )
                }
            }
        }
    }
}

impl From<RegistryError> for CliError {
    fn from(error: RegistryError) -> Self {
        match error {
            RegistryError::NotFound { name, version } => CliError::new(
                E_REG_001,
                format!("artifact {name}@{version} not found in registry"),
            ),
            RegistryError::Conflict { name, version } => CliError::with_hint(
                E_REG_003,
                format!("artifact {name}@{version} already exists in registry"),
                "use a new version string or remove the existing artifact",
            ),
            RegistryError::ReservedVersion(version) => CliError::with_hint(
                E_REG_004,
                format!("version '{version}' is reserved and cannot be published"),
                "reserved strings: latest, stable, edge — use an explicit semver version",
            ),
            RegistryError::InvalidInput(message) => {
                CliError::new(E_IO_003, format!("registry error: {message}"))
            }
            RegistryError::IntegrityMismatch { name, version, .. } => CliError::with_hint(
                E_REG_002,
                format!("artifact integrity check failed for {name}@{version}"),
                "artifact on disk does not match murmur.lock — re-publish or delete the lock",
            ),
            RegistryError::HomeDirNotFound => {
                CliError::new(E_IO_001, "could not determine home directory")
            }
            RegistryError::Io { source, .. } => {
                CliError::new(E_IO_003, format!("registry I/O error: {source}"))
            }
        }
    }
}

impl From<ManifestError> for CliError {
    fn from(error: ManifestError) -> Self {
        match error {
            ManifestError::MissingField { field } => CliError::new(
                E_MAN_001,
                format!("{MANIFEST_FILENAME}: missing required field '{field}'"),
            ),
            ManifestError::YamlSyntax(message) => CliError::new(E_MAN_002, message),
            ManifestError::InvalidType {
                field,
                expected,
                got,
            } => CliError::new(
                E_MAN_003,
                format!(
                    "{MANIFEST_FILENAME}: field '{field}' has invalid type (expected {expected}, got {got})"
                ),
            ),
            ManifestError::NotFound(path) => {
                CliError::new(E_IO_001, format!("{MANIFEST_FILENAME} not found at {path}"))
            }
            ManifestError::Io { path, source } => CliError::new(
                E_IO_003,
                format!("failed to read {MANIFEST_FILENAME} at {path}: {source}"),
            ),
        }
    }
}

impl From<BuildError> for CliError {
    fn from(error: BuildError) -> Self {
        match error {
            BuildError::Manifest(error) => CliError::from(error),
            BuildError::InvalidOutputExtension(path) => CliError::new(
                E_IO_003,
                format!("output path must end with .mur.zip: {path}"),
            ),
            BuildError::CreateOutput { path, source } => {
                io_error_with_path("failed to create output file", &path, source)
            }
            BuildError::ReadSource { path, source } => {
                io_error_with_path("failed to read source", &path, source)
            }
            BuildError::WorkingDir(source) => CliError::new(
                E_IO_003,
                format!("failed to determine working directory: {source}"),
            ),
            BuildError::PackageFile { path, source } => {
                io_error_with_path("failed to package file", &path, source)
            }
            BuildError::Zip { path, source } => CliError::new(
                E_IO_003,
                format!("zip error while writing {path}: {source}"),
            ),
            BuildError::MissingRequiredFile { file, path } => CliError::new(
                E_IO_003,
                format!("missing required file '{file}' in {path}"),
            ),
            BuildError::InvalidArtifactName { name, reason } => CliError::with_hint(
                E_BLD_001,
                format!("invalid artifact name '{name}': {reason}"),
                "artifact names are lowercase letters, digits and inner hyphens, e.g. my-tool",
            ),
            BuildError::UnsafeRequiredPath { entry, reason } => CliError::with_hint(
                E_BLD_002,
                format!("requires_files entry '{entry}' has an unsafe path: {reason}"),
                "declare files by a relative path inside the source directory",
            ),
            BuildError::SymlinkedRequiredFile { entry, path } => CliError::with_hint(
                E_BLD_002,
                format!(
                    "requires_files entry '{entry}' is a symlink ({path}); declare the file it points to instead"
                ),
                "copy the target into the source directory, or declare its real path",
            ),
            BuildError::DuplicateArchiveEntry {
                first,
                second,
                entry,
            } => CliError::with_hint(
                E_BLD_002,
                format!(
                    "requires_files entries '{first}' and '{second}' both pack as the archive entry '{entry}'"
                ),
                "give each packaged file a distinct path inside the artifact",
            ),
            BuildError::PayloadShape(error) => CliError::with_hint(
                E_BLD_003,
                error.to_string(),
                "declare exactly one root *.wasm in requires_files: (or name it capsule.wasm)",
            ),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => {
                CliError::new(E_IO_002, format!("permission denied: {error}"))
            }
            std::io::ErrorKind::NotFound => CliError::new(E_IO_001, format!("not found: {error}")),
            _ => CliError::new(E_IO_003, format!("I/O error: {error}")),
        }
    }
}

fn io_error_with_path(prefix: &str, path: &str, source: std::io::Error) -> CliError {
    match source.kind() {
        std::io::ErrorKind::NotFound => {
            CliError::new(E_IO_001, format!("{prefix} at {path}: {source}"))
        }
        std::io::ErrorKind::PermissionDenied => {
            CliError::new(E_IO_002, format!("{prefix} at {path}: {source}"))
        }
        _ => CliError::new(E_IO_003, format!("{prefix} at {path}: {source}")),
    }
}

fn linker_error_from_message(message: &str) -> CliError {
    let mut text = "capsule requires a WASI interface the runtime has not linked; check that the required capability is declared in murmur.yaml".to_string();
    if let Some(iface) = extract_interface_name(message) {
        text.push_str(&format!(" (missing interface: {iface})"));
    }
    CliError::new(E_RUN_002, text)
}

fn extract_interface_name(message: &str) -> Option<String> {
    let mut in_tick = false;
    let mut current = String::new();

    for ch in message.chars() {
        if ch == '`' {
            if in_tick {
                if current.contains(':') || current.contains('/') {
                    return Some(current);
                }
                current.clear();
                in_tick = false;
            } else {
                in_tick = true;
                current.clear();
            }
            continue;
        }

        if in_tick {
            current.push(ch);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression coverage for the versioned-only export errors (see
    // capsule-runtime/src/errors.rs): the CLI mapping must surface the versioned
    // interface name and the rebuild hint, not reconstruct stale unversioned text.
    #[test]
    fn capsule_export_missing_surfaces_versioned_name_and_rebuild_hint() {
        let cli = CliError::from(RuntimeError::CapsuleExportMissing);
        assert_eq!(cli.code, E_RUN_001);
        assert!(
            cli.message.contains("murmur:capsule/run@0.1.0"),
            "message should name the versioned interface: {}",
            cli.message
        );
        assert!(
            cli.message.contains("rebuild"),
            "message should carry the rebuild hint: {}",
            cli.message
        );
    }

    #[test]
    fn tool_export_missing_surfaces_versioned_name_and_rebuild_hint() {
        let cli = CliError::from(RuntimeError::ToolExportMissing {
            name: "echo-tool".to_string(),
        });
        assert_eq!(cli.code, E_RUN_001);
        assert!(
            cli.message.contains("'echo-tool'"),
            "message should name the artifact: {}",
            cli.message
        );
        assert!(
            cli.message.contains("murmur:tool/run@0.1.0"),
            "message should name the versioned interface: {}",
            cli.message
        );
        assert!(
            cli.message.contains("rebuild"),
            "message should carry the rebuild hint: {}",
            cli.message
        );
    }

    /// The last hop of the composed-root failure path: `capsule-runtime` returns this variant out
    /// of the agent turn loop (see `runtime::tests::a_sealed_composed_root_failure_ends_the_
    /// session_not_just_the_tool_call` for where it is produced), and it must land on its own
    /// code with its own remediation rather than on `E-CAP-003`'s "lower your declared floor",
    /// which would be the wrong instruction for a host that already cleared the probe.
    #[test]
    fn sealed_root_construction_failure_has_its_own_code_and_remediation() {
        let cli = CliError::from(RuntimeError::SealedRootConstructionFailed {
            detail: "sealed-root: bind (ro) /usr -> /tmp/usr failed: No such file or directory \
                     (os error 2)"
                .to_string(),
        });

        assert_eq!(cli.code, E_RUN_014);
        assert_ne!(
            cli.code, E_CAP_003,
            "a mid-session construction failure is not the pre-launch refusal"
        );
        assert!(
            cli.message.contains("composed root") && cli.message.contains("/usr"),
            "message should say what failed and carry the child's diagnostic: {}",
            cli.message
        );
        let hint = cli.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("--explain-scope"),
            "hint should point at the re-probe command: {hint}"
        );
    }

    /// `staged_runtime` without a `sealed` floor must not land on `E-CAP-003`. The two refusals
    /// read alike (both are pre-launch, both mention containment classes) and their remedies are
    /// exact opposites — `E-CAP-003` says lower the declared floor, this one says raise it — so an
    /// operator handed the wrong code would be told to make the problem worse.
    #[test]
    fn staged_runtime_without_sealed_has_its_own_code_and_opposite_remediation() {
        let cli = CliError::from(RuntimeError::StagedRuntimeRequiresSealed {
            binaries: vec!["python3".to_string()],
            declared: murmur_artifact::ContainmentClass::Scoped,
        });

        assert_eq!(cli.code, E_CAP_004);
        assert_ne!(
            cli.code, E_CAP_003,
            "an under-declared capsule is not a host that cannot deliver"
        );
        assert!(
            cli.message.contains("python3") && cli.message.contains("staged_runtime"),
            "message should name the grant and its binary: {}",
            cli.message
        );
        let hint = cli.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("sealed") && hint.contains("--explain-scope"),
            "hint should name the floor to set and how to inspect it: {hint}"
        );
    }

    /// The third member of the same family, and the one most easily confused with `E-CAP-004`:
    /// both are pre-launch refusals about a `sealed` capsule's runtime staging. `E-CAP-004` fires
    /// when a grant exists at too low a floor and says *raise the floor*; this one fires when the
    /// floor is already `sealed` and no grant exists at all, and says *add a grant*. An operator
    /// handed `E-CAP-004`'s advice here would go and change the one thing already correct, so the
    /// codes and the hints are asserted to be distinct.
    #[test]
    fn an_unreachable_interpreted_entrypoint_has_its_own_code_and_names_the_measurement() {
        let cli = CliError::from(RuntimeError::ShellBinaryPackageUnreachable {
            entries: vec![capsule_runtime::UnreachableEntrypoint {
                binary: "pip".to_string(),
                resolved_path: std::path::PathBuf::from("/home/dev/.local/bin/pip"),
                interpreter: "python3".to_string(),
            }],
        });

        assert_eq!(cli.code, E_CAP_006);
        assert_ne!(
            cli.code, E_CAP_004,
            "a missing grant at sealed is not a grant declared below sealed"
        );
        for expected in ["pip", "/home/dev/.local/bin/pip", "python3"] {
            assert!(
                cli.message.contains(expected),
                "message should name {expected}: {}",
                cli.message
            );
        }
        let hint = cli.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("interpreter_runtime") && hint.contains("strace"),
            "hint should name the grant to declare and how to measure its directories: {hint}"
        );
    }
}
