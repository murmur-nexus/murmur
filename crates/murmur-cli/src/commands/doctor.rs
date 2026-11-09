use std::collections::HashSet;
use std::path::Path;

use capsule_runtime::murmur_home::{audit_murmur_home, wide_entry_warning, HomeEntryState};
use capsule_runtime::{
    applied_mechanism, caller_overflow_fix, caller_overflow_warning,
    capability_policy_from_runtime_manifest, check_egress_namespace,
    check_interpreted_entrypoints_reachable, check_roost_health, check_staged_runtime_floor,
    detect_landlock_abi, doctor_runtime_dir, find_on_path, has_subprocess_route_at_staging,
    inspect_installed_profile, install_from_places, landlock_doctor_lines, preopen_reports,
    probe_scope, profile_gap_remedy_parts, read_only_advisory_for, render_cgroup_scope,
    render_install, render_read_only, render_records_dir, render_routes, render_runtime_dir,
    render_temp_dir, route_table, runtime_dir_inside_warning, served_wit_level,
    undeclarable_runtime_pin_role, warn_on_gateway_endpoint_in_network_allow,
    warn_on_interpreter_runtime_grants, warn_on_launch_only_gateway_credential,
    warn_on_secret_shaped_env_grants, warn_on_uncovered_routes, warn_on_unmetered_gateways,
    warn_on_unreachable_toolchain_helpers, warn_on_userns_restriction_disabled_host_wide,
    warn_on_workdir_exec, wit_levels, ArtifactRequest, CgroupScopeReport, CgroupScopeStatus,
    EgressNamespaceFailure, HostProbe, InstalledProfileState, MountAttempt, NamespaceReading,
    PreopenReport, ProfileAttachment, ProfileGap, RouteInputs, UsernsGrant, HOME_UNUSABLE_HINT,
    SEALED_APPARMOR_ATTACHMENT_PATHS, SEALED_APPARMOR_PROFILE_PATH, SEALED_APPARMOR_PROFILE_SHA256,
};
use murmur_artifact::{
    effective_containment_floor, newest_fitting_level, read_runtime_manifest_text,
    registry_warning_link, resolve_manifest_path, resolve_roster_path, sha256_hex, version_fit,
    warn_on_unknown_manifest_keys, wit_contracts_from_artifact_bytes, ArtifactRuntime,
    ContractDirection, FitSubject, LocalRegistry, LockOrigin, MurmurLock, Registry,
    ResolvedArtifact, RosterReachability, RuntimeManifest, UnservedInterface, VersionFit,
    WitContracts, MANIFEST_FILENAME, ROSTER_FILENAME, W_REG_002, W_REG_003, W_REG_004, W_REG_005,
    W_REG_006, W_ROS_001, W_RUN_002, W_RUN_009, W_RUN_013,
};

use crate::commands::install::find_project_root;
use crate::commands::run::{artifact_presence, ArtifactPresence};
use crate::commands::{read_optional_lockfile, runtime_manifest_error_to_cli};
use crate::config::load_effective_mur_config_if_any_exists;
use crate::env_requirements::{
    env_requirements_report, EnvRequirementsReport, EnvironmentNames, RequiredVariable,
    UninspectableReason,
};
use crate::error::{
    CliError, E_CAP_002, E_CAP_004, E_CAP_005, E_CAP_006, E_CAP_014, E_CAP_015, E_CAP_016,
    E_IO_003, E_REG_002, E_RUN_003, E_RUN_008, E_RUN_012, E_RUN_019, E_RUN_043, E_RUN_064,
    E_WIT_LEVEL_001,
};
use crate::stores::{installed_contracts, interface_label, served_phrase, FallbackRegistry};

/// The doctor block a finding comes from: the `block` key of `mur doctor --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
enum DoctorBlock {
    /// The project itself: no `murmur.yaml` found, or one that cannot be read.
    Project,
    /// What the manifest's `capabilities` declare.
    Capabilities,
    /// The containment stance `mur run` resolves.
    Containment,
    /// What this host can do, whatever the manifest says.
    Host,
    /// The `mur-roost` daemon a delegating capsule registers with.
    Roost,
    /// The `Environment requirements` block.
    Environment,
    /// The `Roster` block.
    Roster,
    /// The `Filesystem preopens` block.
    Preopens,
    /// The artifact checklist, and each pinned tool's declared needs.
    Artifacts,
    /// The `Model limits` block.
    ModelLimits,
    /// The `Process harness` block.
    Harness,
    /// The `capabilities.resources` bounds.
    Resources,
    /// The `WIT level` block.
    WitLevel,
    /// The `Cgroup scope` block.
    CgroupScope,
    /// The `Runtime directory` block.
    RuntimeDirectory,
    /// The `Host paths` block.
    HostPaths,
}

/// One counted finding: an error, which makes `mur doctor` exit `1`, or a warning, which does not.
/// Each ends in a `Fix:` line and is one entry of `mur doctor --json`'s `findings`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    /// The `E-`/`W-` code `mur run` gives the same thing, or doctor's own code for it.
    code: &'static str,
    block: DoctorBlock,
    /// What the finding concerns — an artifact `name@version`, a member, a capsule, a variable, a
    /// path, a binary or a manifest field — or `None` for a host-wide finding.
    subject: Option<String>,
    /// The finding's one-line headline, without an `error[<CODE>]: ` prefix.
    message: String,
    /// Whether the text form heads the finding `error[<code>]: `. A checklist line is headed by
    /// its `name@version` instead.
    headed_by_code: bool,
    /// The `Fix:` line's text.
    fix: String,
}

impl Finding {
    /// The line printed after the tally as `First refusal: <headline>` when this is the first
    /// error doctor printed.
    fn headline(&self) -> String {
        if self.headed_by_code {
            format!("error[{}]: {}", self.code, self.message)
        } else {
            self.message.clone()
        }
    }
}

/// `line` without a leading `warning[<code>]: ` or `error[<code>]: `, for a message printed with
/// its code that a [`Finding`] carries separately.
fn without_code(line: &str, code: &str) -> String {
    line.strip_prefix(&format!("warning[{code}]: "))
        .or_else(|| line.strip_prefix(&format!("error[{code}]: ")))
        .unwrap_or(line)
        .to_string()
}

/// Every finding that ends in a `Fix:` line, each list in the order doctor printed its findings.
///
/// Created at the start of [`check_project`] and [`check_formation`], so every block, the
/// manifest-only prologue included, records into the same tally. Adding a finding that predicts a
/// `mur run` refusal is one [`Findings::predict`] call; any error recorded fails the exit code.
/// Every finding carries a code, a block and a subject, which is what puts it in
/// `mur doctor --json`.
#[derive(Debug, Default)]
struct Findings {
    /// Findings that make doctor exit `1`.
    errors: Vec<Finding>,
    /// Findings that leave the exit code at `0`.
    warnings: Vec<Finding>,
}

impl Findings {
    /// Records an error headed by `line` itself, such as a checklist line's
    /// `<name@version> — <reason>`.
    fn error(
        &mut self,
        code: &'static str,
        block: DoctorBlock,
        subject: Option<String>,
        line: impl Into<String>,
        fix: impl Into<String>,
    ) {
        self.errors.push(Finding {
            code,
            block,
            subject,
            message: line.into(),
            headed_by_code: false,
            fix: fix.into(),
        });
    }

    /// Records an error headed `error[<code>]: <first line of message>`.
    fn coded(
        &mut self,
        code: &'static str,
        block: DoctorBlock,
        subject: Option<String>,
        message: &str,
        fix: impl Into<String>,
    ) {
        self.errors.push(Finding {
            code,
            block,
            subject,
            message: message.lines().next().unwrap_or_default().to_string(),
            headed_by_code: true,
            fix: fix.into(),
        });
    }

    /// Prints `refusal` on stderr and records it as an error.
    fn predict(&mut self, block: DoctorBlock, subject: Option<String>, refusal: PredictedRefusal) {
        capsule_runtime::report_eprintln!("{}", refusal.stderr_text());
        self.coded(refusal.code, block, subject, &refusal.message, refusal.fix);
    }

    /// Records a warning: `message` is its one-line headline.
    fn warning(
        &mut self,
        code: &'static str,
        block: DoctorBlock,
        subject: Option<String>,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) {
        let message = message.into();
        self.warnings.push(Finding {
            code,
            block,
            subject,
            message: message.lines().next().unwrap_or_default().to_string(),
            headed_by_code: false,
            fix: fix.into(),
        });
    }
}

/// A refusal `mur run` with no flags would make of this project's capsule on this host, found
/// without launching it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PredictedRefusal {
    /// The code `mur run` refuses with.
    code: &'static str,
    /// The refusal's own text, as `mur run` words it.
    message: String,
    /// The line under it, which says that `mur run` will refuse and why.
    consequence: String,
    /// The `Fix:` line's text: what to change, in the finding's own terms.
    fix: String,
}

impl PredictedRefusal {
    /// The stderr lines: `[mur doctor] error[<code>]: <message>`, then the consequence indented.
    fn stderr_text(&self) -> String {
        format!(
            "[mur doctor] error[{}]: {}\n  {}",
            self.code, self.message, self.consequence
        )
    }
}

/// The `E-CAP-005` refusal of a capsule that can spawn a subprocess on a host that cannot give it
/// a network namespace, or `None` when `mur run` would not refuse it for that.
fn egress_namespace_refusal(
    can_spawn_subprocess: bool,
    failure: Option<&EgressNamespaceFailure>,
) -> Option<PredictedRefusal> {
    let error = check_egress_namespace(can_spawn_subprocess, failure).err()?;
    Some(PredictedRefusal {
        code: E_CAP_005,
        message: error.to_string(),
        consequence: "`mur run` will refuse this capsule on this host until that is resolved. \
                      Nothing in murmur.yaml can change it — the refusal is about this machine, \
                      not the manifest."
            .to_string(),
        fix: concat!(
            "make this host able to create the capsule's network namespace \u{2014} see ",
            murmur_artifact::docs_reference_url!("diagnostics/#e-cap-005")
        )
        .to_string(),
    })
}

/// The `network namespace:` line of the `AppArmor / user namespaces` block: the answer of the
/// probe `mur run` consults, printed for every project whatever it declares.
fn network_namespace_line(is_linux: bool, failure: Option<&EgressNamespaceFailure>) -> String {
    match failure {
        _ if !is_linux => "n/a (a capsule network namespace is a Linux mechanism)".to_string(),
        None => "created (a real probe created one; the readings above explain a refusal, they \
                 never cause one)"
            .to_string(),
        Some(failure) => format!("refused \u{2014} {}", failure.summary()),
    }
}

/// The `mount namespace:` line: whether the probe's private mount namespace, inside the network
/// namespace, was created — which decides whether a command's literal `/tmp` is mapped.
fn mount_namespace_line(is_linux: bool, reading: &NamespaceReading) -> String {
    attempt_line(
        is_linux,
        &reading.mount,
        "a private mount namespace inside the session's user namespace was created, so /tmp can \
         be mapped",
        "a private mount namespace",
    )
}

/// The `sealed root:` line: whether the probe's composed root — a nested user+mount namespace
/// inside the network namespace, entered with `pivot_root(2)` — was built.
fn sealed_root_line(is_linux: bool, reading: &NamespaceReading) -> String {
    attempt_line(
        is_linux,
        &reading.sealed_root,
        "a nested user+mount namespace inside the session's was created and pivot_root(2) entered \
         a composed root, so sealed can be backed",
        "a composed root",
    )
}

/// The `private proc:` line: whether a process forked into the probe's PID namespace mounted a
/// private `procfs` over `/proc` — which decides the session's process view.
fn private_proc_line(is_linux: bool, reading: &NamespaceReading) -> String {
    attempt_line(
        is_linux,
        &reading.private_proc,
        "a process in the session's PID namespace mounted its own procfs over /proc, so commands \
         see only the session's processes",
        "a private procfs",
    )
}

/// One mount attempt's line, in the four shapes the three lines share.
fn attempt_line(is_linux: bool, attempt: &MountAttempt, created: &str, mechanism: &str) -> String {
    match attempt {
        _ if !is_linux => format!("n/a ({mechanism} is a Linux mechanism)"),
        MountAttempt::Created => format!("created ({created})"),
        MountAttempt::NotAttempted => {
            "not attempted (no network namespace to create it in)".to_string()
        }
        MountAttempt::Failed(_) => format!(
            "refused \u{2014} {}",
            attempt.failure_summary().unwrap_or_default()
        ),
    }
}

/// Whether the install-the-profile advice applies: an attempt failed, the grant is withheld and
/// `mur` is unconfined. Under any other profile — a container's — the profile `mur` ships cannot
/// attach, and the refusal itself names the container's remedy.
fn profile_advice_applies(
    grant: Option<UsernsGrant>,
    attachment: &ProfileAttachment,
    attempt_failed: bool,
) -> bool {
    attempt_failed
        && grant == Some(UsernsGrant::Withheld)
        && *attachment == ProfileAttachment::Unconfined
}

/// Whether the probe saw an attempt `sealed` depends on fail — the network namespace or the
/// composed root. The install-the-profile advice is printed only then: a withheld grant over
/// attempts that all succeeded explains nothing.
fn an_attempt_failed(reading: &NamespaceReading) -> bool {
    reading.network.is_err() || matches!(reading.sealed_root, MountAttempt::Failed(_))
}

/// The verdict for one installed artifact when `murmur.lock` is present. Mirrors the
/// three ways `mur run` rejects a locked artifact (`stage_session`'s lock enforcement),
/// so a green doctor line means a session would accept the same artifact.
enum LockVerdict {
    /// Lock agrees with the manifest pin and with the bytes on disk. `pulled_by` is the session
    /// whose `manage.pull()` wrote the pin, `None` for an operator-declared one.
    Ok { pulled_by: Option<String> },
    /// A `manage.pull()` pin the manifest declares as a hook, a driver, with `gateway:`, or binds
    /// as `inference.system_prompt_artifact` — `mur run` fails with E-RUN-043. `declared_as` is
    /// that manifest text.
    RuntimeOriginNotDeclarable {
        session: String,
        declared_as: &'static str,
    },
    /// No entry for this artifact — `mur run` fails with E-RUN-003.
    MissingEntry,
    /// The lock pins a different version than the manifest declares.
    VersionMismatch { pinned: String },
    /// The bytes on disk hash to something other than the hash pinned — `mur run` fails with
    /// E-REG-002.
    HashMismatch { expected: String, actual: String },
}

/// Check one installed artifact against the lockfile the way `stage_session` does:
/// entry must exist, its `resolved_version` must equal the manifest pin, and the hash it pins
/// must equal the hash of the bytes that were actually resolved.
///
/// A version mismatch short-circuits the hash comparison — hashing bytes for a version
/// already known to be wrong would report one drifted artifact as two failures. The pin's origin
/// is judged last, against the manifest entry's `runtime`, whether it declares `gateway:`, and
/// whether `inference.system_prompt_artifact` names it.
#[allow(clippy::too_many_arguments)]
fn check_lock_entry(
    lock: &MurmurLock,
    name: &str,
    version: &str,
    artifact_bytes: &[u8],
    runtime: &ArtifactRuntime,
    declares_gateway: bool,
    bound_as_system_prompt: bool,
) -> LockVerdict {
    let Some(entry) = lock.artifact_for(name) else {
        return LockVerdict::MissingEntry;
    };

    if entry.resolved_version != version {
        return LockVerdict::VersionMismatch {
            pinned: entry.resolved_version.clone(),
        };
    }

    let actual = sha256_hex(artifact_bytes);
    if actual != entry.sha256.any {
        return LockVerdict::HashMismatch {
            expected: entry.sha256.any.clone(),
            actual,
        };
    }

    match &entry.origin {
        LockOrigin::Operator => LockVerdict::Ok { pulled_by: None },
        LockOrigin::Runtime { session } => {
            match undeclarable_runtime_pin_role(runtime, declares_gateway, bound_as_system_prompt) {
                Some(declared_as) => LockVerdict::RuntimeOriginNotDeclarable {
                    session: session.clone(),
                    declared_as,
                },
                None => LockVerdict::Ok {
                    pulled_by: Some(session.clone()),
                },
            }
        }
    }
}

/// Prints where this `mur` is and which AppArmor profile, if any, the kernel reports confining it.
///
/// Two separate facts, printed together because neither is useful alone: a profile name with no
/// path does not say what it attached *to*, and a path with no profile does not say whether
/// anything reached it. The path is resolved through `std::env::current_exe()` and canonicalized
/// where possible — a `mur` reached through a symlink is confined by whatever attaches to the
/// resolved target, not to the link — and stated as unavailable rather than guessed when the
/// platform will not report it.
///
/// Records nothing in [`Findings`] and changes no exit code, exactly as the rest of this block does
/// not.
fn report_binary_attachment(
    grant: Option<UsernsGrant>,
    attachment: &ProfileAttachment,
    profile_gap: &ProfileGap,
    attempt_failed: bool,
) {
    match std::env::current_exe() {
        Ok(path) => {
            let resolved = std::fs::canonicalize(&path).unwrap_or(path);
            capsule_runtime::report_println!("  this binary:  {}", resolved.display());
        }
        Err(error) => {
            capsule_runtime::report_println!("  this binary:  <unavailable> ({error})");
            capsule_runtime::report_println!(
                "    Without the running binary's path there is nothing to say which profile \
                 could attach to it. The `profile attached` line below is still what the kernel \
                 reports."
            );
        }
    }

    match attachment {
        ProfileAttachment::Sealed { profile } => {
            capsule_runtime::report_println!("  profile attached: {profile}");
            capsule_runtime::report_println!(
                "    An AppArmor profile named for the one mur ships is confining this binary, so \
                 the userns grant below covers mur alone."
            );
        }
        ProfileAttachment::Other { profile } => {
            capsule_runtime::report_println!("  profile attached: {profile}");
            capsule_runtime::report_println!(
                "    Some other profile confines this binary. It is not the profile mur ships and \
                 grants nothing mur asked for; it may also be narrower than unconfined."
            );
        }
        ProfileAttachment::Unconfined => {
            capsule_runtime::report_println!(
                "  profile attached: none (the kernel reports this binary as unconfined)"
            );
        }
        ProfileAttachment::NotReported => {
            capsule_runtime::report_println!(
                "  profile attached: not reported (/proc/self/attr/current is unreadable)"
            );
            capsule_runtime::report_println!(
                "    Expected where AppArmor is not enabled, and on every non-Linux host. Not the \
                 same as `none`: nothing attaches and I could not look are different findings."
            );
        }
    }

    if grant == Some(UsernsGrant::Withheld) && !attempt_failed {
        capsule_runtime::report_println!(
            "    The grant below reads withheld, and the namespaces below were created regardless: \
             the reading explains a refusal when an attempt fails, and caused none here."
        );
    } else if profile_advice_applies(grant, attachment, attempt_failed) {
        for line in profile_advice_lines(profile_gap, env!("CARGO_PKG_VERSION")) {
            capsule_runtime::report_println!("    {line}");
        }
    }
}

/// The advice printed under `profile attached: none` when the probe was refused for it, worded
/// for why no `mur-sealed` profile confines this binary. The commands are
/// [`profile_gap_remedy_parts`]'s, the ones `E-CAP-005` and `E-CAP-003` name for the same gap;
/// the `LAST RESORT` sysctl stays in the refusals, not here. Lines are unindented; a list entry
/// carries two spaces of its own.
fn profile_advice_lines(gap: &ProfileGap, version: &str) -> Vec<String> {
    let attachment_list = || {
        std::iter::once("The shipped profiles attach to:".to_string()).chain(
            SEALED_APPARMOR_ATTACHMENT_PATHS
                .iter()
                .map(|attachment| format!("  {attachment}")),
        )
    };
    let not_the_fix = "Adding this path to the profile is not the fix. Path attachment is the \
                       mechanism's design — a profile that matched everything would grant userns \
                       to everything — so the next unusual location would fail exactly the same \
                       way.";
    match gap {
        ProfileGap::NotLoaded { .. } | ProfileGap::NotAttached { .. } => {
            profile_gap_remedy_parts(gap, version)
        }
        ProfileGap::PathNotProfiled { .. } => {
            let mut parts = profile_gap_remedy_parts(gap, version).into_iter();
            let mut lines = vec!["This path is why the grant below is withheld.".to_string()];
            lines.extend(parts.next());
            lines.extend(attachment_list());
            lines.push(not_the_fix.to_string());
            lines.extend(parts);
            lines
        }
        ProfileGap::Undetermined => {
            let mut lines = vec![
                "This path is why the grant below is withheld: AppArmor attaches profiles by \
                 executable path, and no mur-sealed profile attaches to this one."
                    .to_string(),
                "The shipped profile attaches to:".to_string(),
            ];
            lines.extend(
                SEALED_APPARMOR_ATTACHMENT_PATHS
                    .iter()
                    .map(|attachment| format!("  {attachment}")),
            );
            lines.push(
                "Running from a checkout build: `scripts/install-dev-apparmor.sh` generates and \
                 loads the same grant for ./target/{debug,release}/mur. Running from anywhere \
                 else: install mur to one of the paths above."
                    .to_string(),
            );
            lines.push(not_the_fix.to_string());
            lines
        }
    }
}

/// Prints the Landlock block: the kernel's ABI, the ABI a capsule's ruleset is built at, and what a
/// capsule loses on this host below `capsule_runtime`'s target ABI. Informational, like the
/// AppArmor block beside it: no diagnostic code and no effect on the exit status.
fn report_landlock_abi() {
    capsule_runtime::report_println!("Landlock");
    for line in landlock_doctor_lines(detect_landlock_abi()) {
        capsule_runtime::report_println!("  {line}");
    }
}

/// Prints the AppArmor/user-namespace block, and emits `W-SEC-013` when this host's user
/// namespaces are unrestricted host-wide rather than granted to `mur` by the shipped profile.
///
/// Two findings, deliberately not merged. The **grant** is behavioural — what the kernel actually
/// did when this process asked — and it is the source of truth. The **profile file** comparison is
/// a byte comparison of `/etc/apparmor.d/mur-sealed` against the digest this build ships; it can
/// never establish what `apparmor_parser` has loaded, because a file can be edited without being
/// reloaded and a loaded profile can outlive the file it came from. So the file finding is
/// reported after the grant and never contradicts it, changes no class, and changes no exit code —
/// `run_doctor`'s exit status is driven by the errors in its [`Findings`] alone.
///
/// The **binary path and its attachment** are a third finding on the same terms. AppArmor attaches
/// profiles by executable path, so a `mur` at a bind mount, a build output or a test fixture gets
/// no profile however correctly the shipped one is installed — and the grant alone cannot say so,
/// because it reports `withheld` identically for "no profile is installed" and "a profile is
/// installed and does not attach here". The attachment is read from `/proc/self/attr/current`,
/// never inferred by matching the running path against
/// [`SEALED_APPARMOR_ATTACHMENT_PATHS`], which is remediation text only.
fn report_userns_grant(probe: &HostProbe) {
    let grant = probe.userns_grant();
    let reading = probe.namespaces();
    let is_linux = cfg!(target_os = "linux");
    capsule_runtime::report_println!("AppArmor / user namespaces");
    report_binary_attachment(
        grant,
        &reading.attachment,
        &reading.profile_gap,
        an_attempt_failed(reading),
    );
    match grant {
        Some(grant) => {
            capsule_runtime::report_println!("  userns grant: {}", grant.wire_name());
            capsule_runtime::report_println!("    {}", grant.summary());
        }
        None => {
            capsule_runtime::report_println!("  userns grant: n/a (AppArmor is a Linux mechanism)")
        }
    }
    capsule_runtime::report_println!(
        "  network namespace: {}",
        network_namespace_line(is_linux, probe.egress_namespace_failure().as_ref())
    );
    capsule_runtime::report_println!(
        "  mount namespace: {}",
        mount_namespace_line(is_linux, reading)
    );
    capsule_runtime::report_println!("  sealed root: {}", sealed_root_line(is_linux, reading));
    capsule_runtime::report_println!("  private proc: {}", private_proc_line(is_linux, reading));

    match inspect_installed_profile() {
        InstalledProfileState::Matches => {
            capsule_runtime::report_println!(
                "  {SEALED_APPARMOR_PROFILE_PATH}: matches the profile this build ships"
            );
        }
        InstalledProfileState::Drifted { installed_sha256 } => {
            capsule_runtime::report_println!(
                "  {SEALED_APPARMOR_PROFILE_PATH}: does NOT match the profile this build ships"
            );
            capsule_runtime::report_println!("    installed sha256: {installed_sha256}");
            capsule_runtime::report_println!(
                "    shipped sha256:   {SEALED_APPARMOR_PROFILE_SHA256}"
            );
            capsule_runtime::report_println!(
                "    This compares file contents only. It does not establish what the kernel has \
                 loaded — a file can be edited without `apparmor_parser -r` ever running. The \
                 `userns grant` line above is what the kernel actually did. Local customisation \
                 belongs in /etc/apparmor.d/local/mur-sealed, which is included inside the shipped \
                 profiles and is not hashed here."
            );
        }
        InstalledProfileState::Absent => {
            capsule_runtime::report_println!("  {SEALED_APPARMOR_PROFILE_PATH}: not installed");
            capsule_runtime::report_println!(
                "    Expected on a host without AppArmor, and on a checkout build using \
                 scripts/install-dev-apparmor.sh, which writes its own separate file. The `userns \
                 grant` line above is what decides whether sealed containment works here."
            );
        }
        InstalledProfileState::Unreadable { error } => {
            capsule_runtime::report_println!(
                "  {SEALED_APPARMOR_PROFILE_PATH}: present but unreadable ({error})"
            );
            capsule_runtime::report_println!(
                "    Not the same as absent, and it changes nothing: the `userns grant` line above \
                 is what the kernel actually did."
            );
        }
    }

    capsule_runtime::report_println!();

    // Stderr, in the same words `mur run` uses at staging, so the two cannot state it differently.
    warn_on_userns_restriction_disabled_host_wide(grant);
}

/// Prints an `Artifact stores` section naming every directory of the project and global stores
/// left in the flat layout that predates scopes: nothing resolves from one, so each is safe to
/// delete. Prints nothing when there is none. A report only: it adds no fix and cannot change
/// doctor's exit status, and an unreadable store is left to the checks that resolve from it.
fn report_artifact_stores(project: &LocalRegistry, global: &LocalRegistry) {
    let mut lines = Vec::new();
    for (label, store) in [("project", project), ("global", global)] {
        let Ok(entries) = store.old_layout_entries() else {
            continue;
        };
        for entry in entries {
            lines.push(format!(
                "  {label} store: {}: old layout, nothing reads it, safe to delete",
                store.root().join(&entry).display()
            ));
        }
    }
    if lines.is_empty() {
        return;
    }
    capsule_runtime::report_println!("Artifact stores");
    for line in lines {
        capsule_runtime::report_println!("{line}");
    }
    capsule_runtime::report_println!();
}

/// Prints the mode of `~/.murmur` and of every entry in it, and a `W-SEC-028` line on stderr for
/// each owner-only entry, or path beneath one, that is wider than expected.
///
/// Reads metadata only through [`audit_murmur_home`]: it changes no mode, records nothing in
/// [`Findings`] and cannot change doctor's exit status.
fn report_murmur_home() {
    let home = match crate::config::mur_config_path() {
        Ok(config) => config
            .parent()
            .expect("the global config path always has ~/.murmur as its parent")
            .to_path_buf(),
        Err(err) => {
            capsule_runtime::report_println!("Murmur home");
            capsule_runtime::report_println!("  not reported: {}", err.message);
            capsule_runtime::report_println!();
            return;
        }
    };

    capsule_runtime::report_println!("Murmur home ({})", home.display());
    let mut warnings = Vec::new();
    for report in audit_murmur_home(&home) {
        let name = match report.path.strip_prefix(&home) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
            _ => ".".to_string(),
        };
        let expected = if report.kind.expected_private() {
            ", expected owner-only"
        } else {
            ""
        };
        let deletable = if report.kind.safe_to_delete() {
            ", safe to delete"
        } else {
            ""
        };
        let holds = report.kind.holds();
        let kept = report
            .kind
            .kept()
            .map(|kept| format!(", {kept}"))
            .unwrap_or_default();
        match &report.state {
            HomeEntryState::Absent => {
                capsule_runtime::report_println!(
                    "  {name}: absent  {holds}{kept}{deletable}{expected}"
                )
            }
            HomeEntryState::Unreadable { error } => {
                capsule_runtime::report_println!(
                    "  {name}: unreadable ({error})  {holds}{kept}{deletable}{expected}"
                )
            }
            HomeEntryState::Present {
                mode,
                wide,
                wide_descendants,
                wide_descendants_omitted,
            } => {
                capsule_runtime::report_println!(
                    "  {name}: {mode:04o}  {holds}{kept}{deletable}{expected}"
                );
                if *wide {
                    warnings.push(wide_entry_warning(
                        &report.path,
                        holds,
                        *mode,
                        report.allowed(),
                    ));
                }
                for entry in wide_descendants {
                    let relative = entry.path.strip_prefix(&home).unwrap_or(&entry.path);
                    capsule_runtime::report_println!(
                        "    wider than {:04o}: {} is {:04o}",
                        entry.allowed,
                        relative.display(),
                        entry.mode
                    );
                    warnings.push(wide_entry_warning(
                        &entry.path,
                        holds,
                        entry.mode,
                        entry.allowed,
                    ));
                }
                if *wide_descendants_omitted > 0 {
                    capsule_runtime::report_println!(
                        "    and {wide_descendants_omitted} more wider than expected"
                    );
                }
            }
        }
    }
    capsule_runtime::report_println!();

    for warning in warnings {
        capsule_runtime::report_eprintln!("{warning}");
    }
}

/// The `Runtime directory` and `Records directory` blocks for a `HOME` that cannot hold
/// `~/.murmur`: the reason in place of both paths.
fn report_home_unusable(error: &capsule_runtime::HomeUnusable) {
    capsule_runtime::report_println!("Runtime directory\n  path:      unavailable — {error}");
    capsule_runtime::report_println!();
    capsule_runtime::report_println!("Records directory\n  path:      unavailable — {error}");
}

/// Prints the filesystem surface every guest-bearing artifact in `runtime_manifest` will be
/// preopened into, one line per artifact, resolved through the same `preopen_reports` that
/// `mur run --explain-scope` and `stage_session` use.
///
/// A whole-workdir preopen is reported, not counted: it is the default for a `runtime: tool` or
/// `runtime: driver` entry that declares no `capabilities.filesystem.scope`, and it trades away no
/// enforcement property the way `workdir_exec` does. A scope `mur run` would refuse is an
/// `E-CAP-002` error in `findings`, printed under `<unresolved>`, and the checklist continues.
///
/// Each WASM tool's extra roots come from `declared_floor` and the global config's `host_paths`
/// checked for the default workdir; a refused `host_paths` entry adds none here and is reported
/// under `Host paths`.
fn report_preopens(
    runtime_manifest: &murmur_artifact::RuntimeManifest,
    project_root: &Path,
    declared_floor: murmur_artifact::ContainmentClass,
    config: Option<&crate::config::MurConfig>,
    findings: &mut Findings,
) -> Vec<PreopenReport> {
    capsule_runtime::report_println!("Filesystem preopens");
    let mut resolved = Vec::new();
    match preopen_reports(runtime_manifest.artifacts.iter().map(|artifact| {
        (
            artifact.name.as_str(),
            &artifact.runtime,
            artifact.capabilities.as_ref(),
        )
    }))
    .map(|mut preopens| {
        let host_paths = crate::config::launch_host_paths(config, &project_root.join("workdir"))
            .unwrap_or_default();
        capsule_runtime::attach_extra_roots(
            &mut preopens,
            &capsule_runtime::guest_roots::extra_guest_roots_for(declared_floor, &host_paths),
        );
        preopens
    }) {
        Ok(preopens) if preopens.is_empty() => {
            capsule_runtime::report_println!(
                "  <none> (this capsule declares no tool, driver or hook artifact)"
            );
        }
        Ok(preopens) => {
            for preopen in &preopens {
                capsule_runtime::report_println!("  - {}", preopen.render());
            }
            resolved = preopens;
        }
        Err(error) => {
            capsule_runtime::report_println!("  <unresolved>");
            // `preopen_reports` names the scope, not the entry declaring it.
            let refused = runtime_manifest
                .artifacts
                .iter()
                .find(|artifact| {
                    preopen_reports(std::iter::once((
                        artifact.name.as_str(),
                        &artifact.runtime,
                        artifact.capabilities.as_ref(),
                    )))
                    .is_err()
                })
                .map_or("the refused entry", |artifact| artifact.name.as_str());
            findings.predict(
                DoctorBlock::Preopens,
                Some(refused.to_string()),
                PredictedRefusal {
                    code: E_CAP_002,
                    message: error.to_string(),
                    consequence: "`mur run` will refuse this capsule until that entry's \
                              capabilities.filesystem.scope names a path inside the workdir."
                        .to_string(),
                    fix: format!(
                        "{refused}: give it a capabilities.filesystem.scope inside the workdir"
                    ),
                },
            );
        }
    }

    capsule_runtime::report_println!();
    resolved
}

/// Prints the `Routes` block for `runtime_manifest`, from the `route_table` `--explain-scope`
/// reports, on the tier `declared_floor` installs here, and `W-SEC-034` from the emitter a launch
/// calls.
fn report_routes(
    runtime_manifest: &murmur_artifact::RuntimeManifest,
    policy: &capsule_runtime::CapabilityPolicy,
    preopens: &[PreopenReport],
    host_probe: &HostProbe,
    declared_floor: murmur_artifact::ContainmentClass,
) {
    let inputs = RouteInputs::new(
        &policy.shell_allow,
        policy.plan_submit,
        &policy.install_tool,
        preopens,
        runtime_manifest
            .inference
            .as_ref()
            .map(|inference| inference.transport.as_str()),
    );
    let routes = route_table(&inputs, applied_mechanism(host_probe, declared_floor));
    capsule_runtime::report_print!("{}", render_routes(&routes));
    capsule_runtime::report_println!();
    warn_on_uncovered_routes(declared_floor, &routes, None);
}

/// The `Cgroup scope` block: whether a launch here would get the delegated cgroup v2 scope a
/// capsule that runs subprocesses is refused without, answered by [`probe_scope`] creating and
/// removing a child cgroup the way a launch does. Printed for every project. Returns the `Fix:`
/// for a capsule that `needs_scope` on a host that cannot give one, which `mur run` refuses with
/// `E-RUN-012`.
fn report_cgroup_scope(report: &CgroupScopeReport, needs_scope: bool) -> Option<String> {
    capsule_runtime::report_println!("Cgroup scope");
    capsule_runtime::report_println!("  {}", render_cgroup_scope(report));
    let refused = needs_scope && report.status == CgroupScopeStatus::Unavailable;
    if refused {
        capsule_runtime::report_println!(
            "  refused: `mur run` refuses this capsule here with E-RUN-012, because it can run a \
             subprocess and no cgroup scope can be created to bound it"
        );
    } else if !needs_scope {
        capsule_runtime::report_println!(
            "  this capsule runs no subprocess, so a launch creates no scope for it"
        );
    }
    capsule_runtime::report_println!();
    refused.then(|| {
        concat!(
            "give `mur` a delegated cgroup v2 scope: run it under a systemd user unit with \
             `Delegate=yes` for memory, pids, cpu and io (for example `systemd-run --user --scope \
             -p Delegate=yes mur run`), or `mur run` refuses this capsule with E-RUN-012 \u{2014} see ",
            murmur_artifact::docs_reference_url!("resource-limits/#platform-behavior")
        )
        .to_string()
    })
}

/// Prints the workdir subtrees `capabilities.filesystem.read_only` protects, and whether that
/// protection is enforced for every readable call or advisory against an allowlisted interpreter.
///
/// Rendered by `render_read_only`, the same function `ScopeReport::render` calls, so `mur doctor`
/// and `mur run --explain-scope` cannot state one protection in two strengths. A report, never a
/// verdict: declaring nothing is the common case and records no finding, and an advisory
/// declaration is a legitimate pairing that `W-SEC-017` already states in words.
fn report_read_only(policy: &capsule_runtime::CapabilityPolicy) {
    capsule_runtime::report_println!("Read-only paths");
    capsule_runtime::report_print!(
        "{}",
        render_read_only(
            &policy.read_only_paths,
            &read_only_advisory_for(&policy.read_only_paths, &policy.shell_allow),
        )
    );
    capsule_runtime::report_println!();
}

/// Prints which skills and tools `capabilities.install` lets a pull install into a session, and
/// from where.
///
/// Rendered by `render_install`, the same function `ScopeReport::render` calls, so `mur doctor`
/// and `mur run --explain-scope` state the grant in one voice. A report, never a verdict: an
/// absent grant is the common case and records no finding.
fn report_install(policy: &capsule_runtime::CapabilityPolicy) {
    capsule_runtime::report_println!("Install grant");
    capsule_runtime::report_print!(
        "{}",
        render_install(
            &policy.install_skill,
            &policy.install_tool,
            &install_from_places(policy)
        )
    );
    capsule_runtime::report_println!();
}

/// Prints the aggregate memory ceiling `capabilities.resources.cgroup_memory_bytes` gives the
/// capsule's subprocess tree, as the cgroup files a launch writes: `memory.max` and
/// `memory.swap.max`.
///
/// The swap value is [`capsule_runtime::SCOPE_SWAP_MAX`], the constant the scope is written from,
/// and whether this kernel has a `memory.swap.max` to write is asked of the cgroups this process
/// can see rather than assumed. A report, never a verdict: an unbounded swap on this host records
/// no finding, because a launch proceeds there too.
fn report_memory_bound(policy: &capsule_runtime::CapabilityPolicy) {
    capsule_runtime::report_println!("Memory bound");
    capsule_runtime::report_print!(
        "{}",
        render_memory_bound(
            policy.resources.cgroup_memory_bytes,
            cfg!(target_os = "linux"),
            &capsule_runtime::probe_swap_control(),
            capsule_runtime::active_swap_areas(),
        )
    );
    capsule_runtime::report_println!();
}

/// The `Memory bound` block's lines, split from [`report_memory_bound`] so every host answer can be
/// rendered on any host.
fn render_memory_bound(
    cgroup_memory_bytes: u64,
    linux: bool,
    swap: &capsule_runtime::SwapControl,
    active_swap_areas: Option<usize>,
) -> String {
    use capsule_runtime::SwapControl;
    let mut out = format!("  cgroup_memory_bytes: {cgroup_memory_bytes}\n");
    if !linux {
        out.push_str(
            "  not applied on this host: cgroup bounds are Linux-only, so the subprocess tree is \
             bounded by rlimits alone\n",
        );
        return out;
    }
    out.push_str(&format!("  memory.max {cgroup_memory_bytes}\n"));
    let swap_state = match active_swap_areas {
        Some(0) => "no swap is active (/proc/swaps lists no swap area)".to_string(),
        Some(1) => "swap is active (/proc/swaps lists 1 swap area)".to_string(),
        Some(areas) => format!("swap is active (/proc/swaps lists {areas} swap areas)"),
        None => "whether swap is active is unknown (/proc/swaps is unreadable)".to_string(),
    };
    match swap {
        SwapControl::Exposed => {
            out.push_str(&format!(
                "  memory.swap.max {}\n",
                capsule_runtime::SCOPE_SWAP_MAX
            ));
        }
        SwapControl::Absent => {
            out.push_str(&format!(
                "  memory.swap.max: swap is not bounded on this host — this kernel exposes no \
                 memory.swap.max, so memory.max bounds resident memory only; {swap_state}\n"
            ));
        }
        SwapControl::Unknown => {
            out.push_str(&format!(
                "  memory.swap.max {} where this kernel exposes it — no cgroup this process can \
                 see has the memory controller, so whether it does is unknown; {swap_state}\n",
                capsule_runtime::SCOPE_SWAP_MAX
            ));
        }
    }
    out
}

/// The daemon binary a delegating capsule needs, under the name the installer puts on `PATH` and
/// the name `docs/content/reference/roost-api.md` starts.
const ROOST_BINARY: &str = "mur-roost";

/// The `E-RUN-019` refusal `mur run` makes of a capsule declaring `capabilities.spawn.allow`, or
/// `None` when it would register. Decided as the launch's registration decides it: on
/// `MURMUR_ROOST_URL` alone, which must be set and answer `GET <url>/health`. Whether `mur-roost`
/// is on this machine's `PATH` is advice inside the refusal, never a refusal of its own, because a
/// daemon running elsewhere serves the capsule just as well.
fn roost_daemon_refusal(
    roost_url: Option<&str>,
    roost_binary: Option<&Path>,
    health: impl FnOnce(&str) -> Result<(), String>,
) -> Option<PredictedRefusal> {
    const START: &str = "start one with `mur-roost --port 7700 --registry-path <store>`";
    let roost_url = roost_url.map(str::trim).unwrap_or_default();
    let (message, remedy) = if roost_url.is_empty() {
        (
            format!(
                "this capsule declares capabilities.spawn.allow, and MURMUR_ROOST_URL — the \
                 variable naming the {ROOST_BINARY} daemon to register with — is not set."
            ),
            format!(
                "Start the daemon with `{ROOST_BINARY} --port 7700 --registry-path <store>` and \
                 export MURMUR_ROOST_URL=http://127.0.0.1:7700."
            ),
        )
    } else {
        let reason = health(roost_url).err()?;
        (
            format!(
                "this capsule declares capabilities.spawn.allow, but no daemon answered at \
                 {roost_url}: {reason}"
            ),
            format!(
                "Start it with `{ROOST_BINARY} --port 7700 --registry-path <store>`, or point \
                 MURMUR_ROOST_URL at a daemon that is already running."
            ),
        )
    };
    let (advice, install_fix) = match roost_binary {
        Some(binary) => (
            format!(" {ROOST_BINARY} is installed at {}.", binary.display()),
            String::new(),
        ),
        None => (
            format!(
                " {ROOST_BINARY} was not found on PATH: it ships with `mur`, at the same version \
                 and in the same directory \u{2014} install both with \
                 `curl -fsSL https://install.murmur.rs | sh`."
            ),
            format!(
                "; {ROOST_BINARY} is not on PATH \u{2014} install it with \
                 `curl -fsSL https://install.murmur.rs | sh`"
            ),
        ),
    };
    Some(PredictedRefusal {
        code: E_RUN_019,
        message,
        consequence: format!(
            "`mur run` will refuse this capsule: it cannot register with the daemon that referees \
             its spawns. {remedy}{advice}"
        ),
        fix: format!(
            "set MURMUR_ROOST_URL to a running {ROOST_BINARY} daemon ({START}){install_fix}"
        ),
    })
}

/// Records [`roost_daemon_refusal`] for this process's `MURMUR_ROOST_URL` and `PATH`. A daemon
/// that answers prints nothing.
fn report_roost_daemon(findings: &mut Findings) {
    let roost_url = std::env::var("MURMUR_ROOST_URL").ok();
    let binary = find_on_path(ROOST_BINARY);
    if let Some(refusal) =
        roost_daemon_refusal(roost_url.as_deref(), binary.as_deref(), check_roost_health)
    {
        findings.predict(
            DoctorBlock::Roost,
            Some("MURMUR_ROOST_URL".to_string()),
            refusal,
        );
    }
}

/// Print the `Environment requirements` block: every variable the `capabilities.spawn.allow`
/// closure declares, every variable the project manifest references and this workspace does not
/// hold, every declaration `mur-roost` will refuse, every capsule the walk could not read, and
/// every edge that closes a cycle.
///
/// Names only. No value is read into the report or printed, and nothing is launched.
///
/// The `.env` is read for its names alone, so the block reports set/unset against the same
/// environment `mur run` starts a session with. An unparseable `.env` is reported and the walk
/// still runs against the process environment: an operator asking what a spawn closure needs
/// should get the list even when one line of one file is malformed.
///
/// Prints nothing, and finds nothing, for a capsule that delegates to nobody and holds every
/// variable it references — [`env_requirements_report`] is what decides there is nothing to say.
fn report_env_requirements(
    runtime_manifest: &RuntimeManifest,
    root_manifest_yaml: &str,
    project_root: &Path,
    lock: Option<&MurmurLock>,
    findings: &mut Findings,
) {
    // The project root is the workspace root here: `find_project_root` returns the nearest
    // ancestor holding a `murmur.yaml`, which is the same directory a run reads its `.env` from.
    let (environment, dotenv_error) = EnvironmentNames::for_workspace(project_root);
    let Some(report) = env_requirements_report(
        runtime_manifest,
        root_manifest_yaml,
        project_root,
        lock,
        &environment,
    ) else {
        return;
    };

    capsule_runtime::report_println!("Environment requirements");
    capsule_runtime::report_println!(
        "  capsules: {}",
        report
            .inspected
            .iter()
            .map(crate::env_requirements::CapsuleCoordinate::reference)
            .collect::<Vec<_>>()
            .join(", ")
    );

    if let Some(error) = dotenv_error {
        capsule_runtime::report_println!(
            "  {}/.env could not be read, so set/unset below is this shell's environment alone: {error}",
            project_root.display()
        );
        findings.error(
            E_IO_003,
            DoctorBlock::Environment,
            Some(format!("{}/.env", project_root.display())),
            format!(
                "{}/.env could not be read \u{2014} {error}",
                project_root.display()
            ),
            format!("fix {}/.env — {error}", project_root.display()),
        );
    }

    print_variables(&report, findings);
    print_refusals(&report, findings);
    print_uninspectable(&report, findings);

    for cycle in &report.cycles {
        capsule_runtime::report_println!(
            "  spawn.allow cycle: {} → {} — already on this walk, not descended again",
            cycle.parent,
            cycle.child
        );
    }

    capsule_runtime::report_println!();
}

/// Print the `Control secrets` block: every `control.secrets` name, which a controller supplies
/// to the running capsule and which no environment or config is asked for. Prints nothing for a
/// capsule that declares none, and is never a finding.
fn report_control_secrets(runtime_manifest: &RuntimeManifest) {
    let Some(control) = runtime_manifest
        .control
        .as_ref()
        .filter(|control| !control.secrets.is_empty())
    else {
        return;
    };
    capsule_runtime::report_println!("Control secrets");
    let width = control.secrets.iter().map(String::len).max().unwrap_or(0);
    for name in &control.secrets {
        capsule_runtime::report_println!(
            "  {name:<width$}   supplied by a controller at run time \u{2014} `mur control secret {name}`"
        );
    }
    capsule_runtime::report_println!();
}

/// Which installed artifact store an inventoried artifact sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoreScope {
    /// The project's `.murmur/artifacts/`.
    Project,
    /// `~/.murmur/artifacts/`, shared by every project on the machine.
    Global,
}

impl StoreScope {
    fn label(self) -> &'static str {
        match self {
            StoreScope::Project => "project",
            StoreScope::Global => "global",
        }
    }
}

/// One installed artifact that speaks at least one `murmur:` interface version this `mur` does
/// not serve.
struct StaleArtifact {
    scope: StoreScope,
    name: String,
    version: String,
    /// Never empty.
    unserved: Vec<UnservedInterface>,
}

/// What the `WIT level` inventory found across both stores.
///
/// Its findings become warnings only after the checklist runs: an artifact the checklist already
/// failed for the same stale interfaces gets no second, warning-tier `Fix:` line.
struct InterfaceFindings {
    stale: Vec<StaleArtifact>,
}

/// One row of the `WIT level` block's installed list.
struct InstalledRow {
    reference: String,
    scope: StoreScope,
    built_at: String,
    fits: bool,
}

/// Print the `WIT level` block: the level this mur serves and its package table, then every
/// artifact in either installed store with its built-at label and whether it fits that level. An
/// artifact that does not fit is followed by one line per mismatched interface, then the
/// `W-REG-003` line.
///
/// Always printed. A store whose index cannot be read is named and skipped; that never changes
/// the exit code. Reads only: neither store is written to, no registry is contacted, nothing is
/// launched.
fn report_interface_versions(project: &LocalRegistry, global: &LocalRegistry) -> InterfaceFindings {
    let served = served_wit_level();
    let mut stale = Vec::new();
    let mut rows: Vec<InstalledRow> = Vec::new();
    let mut unreadable = Vec::new();

    for (scope, registry) in [(StoreScope::Project, project), (StoreScope::Global, global)] {
        let index = match registry.list_index() {
            Ok(index) => index,
            Err(error) => {
                unreadable.push((scope, error.to_string()));
                continue;
            }
        };
        let mut seen: HashSet<String> = HashSet::new();
        for meta in index {
            let reference = format!("{}@{}", meta.name, meta.version);
            if !seen.insert(reference.clone()) {
                continue;
            }
            // A payload with no readable component names no `murmur:*` interface.
            let contracts = installed_contracts(registry, &meta).unwrap_or_default();
            let subject = FitSubject::component(contracts.clone());
            let mismatched = match version_fit(&subject, served) {
                VersionFit::Unserved(mismatched) => mismatched,
                VersionFit::Fits => Vec::new(),
            };
            rows.push(InstalledRow {
                reference,
                scope,
                built_at: built_at_label(meta.built_at_level, &contracts),
                fits: mismatched.is_empty(),
            });
            if !mismatched.is_empty() {
                stale.push(StaleArtifact {
                    scope,
                    name: meta.name.to_string(),
                    version: meta.version,
                    unserved: mismatched,
                });
            }
        }
    }

    capsule_runtime::report_println!("WIT level");
    capsule_runtime::report_println!("  this mur serves WIT level {}:", served.level);
    for line in crate::wit_level::package_table(served) {
        capsule_runtime::report_println!("    {line}");
    }
    capsule_runtime::report_println!();
    capsule_runtime::report_println!("  installed:");
    for (scope, error) in &unreadable {
        capsule_runtime::report_println!("    not checked ({} store): {error}", scope.label());
    }
    if rows.is_empty() && unreadable.is_empty() {
        capsule_runtime::report_println!("    none installed");
    }
    let ref_width = rows
        .iter()
        .map(|row| row.reference.len())
        .max()
        .unwrap_or(0);
    let built_width = rows.iter().map(|row| row.built_at.len()).max().unwrap_or(0);
    for row in &rows {
        let status = if row.fits {
            format!("fits level {}", served.level)
        } else {
            format!("does not fit level {}", served.level)
        };
        capsule_runtime::report_println!(
            "    {:<ref_width$}   {:<7}   built at {:<built_width$}   {status}",
            row.reference,
            row.scope.label(),
            row.built_at,
        );
    }
    let stale_width = stale
        .iter()
        .map(|artifact| artifact.name.len() + 1 + artifact.version.len())
        .max()
        .unwrap_or(0);
    for artifact in &stale {
        for interface in &artifact.unserved {
            capsule_runtime::report_println!(
                "{}",
                render_unserved_line(
                    &format!("{}@{}", artifact.name, artifact.version),
                    stale_width,
                    artifact.scope,
                    interface
                )
            );
        }
    }
    if !stale.is_empty() {
        capsule_runtime::report_println!(
            "  mur run refuses these at launch (warning[{W_REG_003}], {})",
            registry_warning_link(W_REG_003)
        );
    }
    capsule_runtime::report_println!();

    InterfaceFindings { stale }
}

/// The built-at label of an installed artifact: the level its `murmur-build.json` recorded, or,
/// when it recorded none, the newest level this mur knows that `contracts` fit, marked derived.
fn built_at_label(recorded: Option<u64>, contracts: &WitContracts) -> String {
    match recorded {
        Some(level) if level > served_wit_level().level => {
            format!("{level} (newer than this mur knows)")
        }
        Some(level) => level.to_string(),
        None => match newest_fitting_level(contracts, wit_levels()) {
            Some(level) => format!("{level} (derived)"),
            None => "none (derived)".to_string(),
        },
    }
}

/// The mismatched interfaces of one checklist artifact, as `stage_session` would resolve them
/// from the same resolved bytes: the sidecar's recorded `wit_contracts`, or the payload's own when
/// the sidecar has none, against the WIT level this mur serves.
fn resolved_unserved(resolved: &ResolvedArtifact) -> Vec<UnservedInterface> {
    let contracts = resolved
        .meta
        .wit_contracts
        .clone()
        .or_else(|| wit_contracts_from_artifact_bytes(&resolved.bytes))
        .unwrap_or_default();
    match version_fit(&FitSubject::component(contracts), served_wit_level()) {
        VersionFit::Unserved(mismatched) => mismatched,
        VersionFit::Fits => Vec::new(),
    }
}

/// One line of the `WIT level` block: one mismatched interface of one installed artifact.
fn render_unserved_line(
    artifact_ref: &str,
    ref_width: usize,
    scope: StoreScope,
    interface: &UnservedInterface,
) -> String {
    let direction = match interface.direction {
        ContractDirection::Export => "exports",
        ContractDirection::Import => "imports",
    };
    format!(
        "  \u{26A0}  {artifact_ref:<ref_width$}   {scope:<7}   {direction} {label} \u{2014} {served}",
        scope = scope.label(),
        label = interface_label(interface),
        served = served_phrase(interface),
    )
}

/// The release an artifact has to be replaced by, named by what it is built against: each
/// unserved package once, in first-seen order, at its served version, then each package this
/// `mur` serves none of under `without`.
fn release_requirement(unserved: &[UnservedInterface]) -> String {
    let mut against: Vec<String> = Vec::new();
    let mut without: Vec<String> = Vec::new();
    for interface in unserved {
        let term = match &interface.served_version {
            Some(served) => format!("{}@{served}", interface.package),
            None => interface.package.clone(),
        };
        let terms = match interface.served_version {
            Some(_) => &mut against,
            None => &mut without,
        };
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    let mut clauses = Vec::new();
    if !against.is_empty() {
        clauses.push(format!("against {}", against.join(", ")));
    }
    if !without.is_empty() {
        clauses.push(format!("without {}", without.join(", ")));
    }
    format!("a release built {}", clauses.join(", "))
}

/// `built against <interface>, <what this mur serves>` for the first of `unserved`, then
/// `(+N more)` when there are others. `unserved` is never empty.
fn unserved_reason(unserved: &[UnservedInterface]) -> String {
    let first = &unserved[0];
    let more = if unserved.len() > 1 {
        format!(" (+{} more)", unserved.len() - 1)
    } else {
        String::new()
    };
    format!(
        "built against {}, {}{more}",
        interface_label(first),
        served_phrase(first)
    )
}

/// The warning-tier `Fix:` text for a stale installed artifact. It names a release by what it is
/// built against, never the installed version: reinstalling that fetches the same build.
fn stale_install_fix(artifact: &StaleArtifact) -> String {
    let global_flag = match artifact.scope {
        StoreScope::Project => "",
        StoreScope::Global => "-g ",
    };
    format!(
        "mur install {global_flag}{}@<{}>",
        artifact.name,
        release_requirement(&artifact.unserved)
    )
}

/// Who needs a variable, as one line of attribution.
///
/// A `capabilities.env.allow` source is rendered bare, because that key is what the whole block is
/// about and naming it on every line would say nothing. Any other key is named, so an operator
/// reading `root@0.0.1 (artifacts.murmur-driver-anthropic.gateway.api_key)` knows which declaration
/// to look at.
fn render_sources(variable: &RequiredVariable) -> String {
    variable
        .sources
        .iter()
        .map(|source| {
            if source.manifest_key == capsule_runtime::EnvelopeAxis::EnvAllow.manifest_key() {
                source.capsule.clone()
            } else {
                format!("{} ({})", source.capsule, source.manifest_key)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_variables(report: &EnvRequirementsReport, findings: &mut Findings) {
    if report.variables.is_empty() {
        return;
    }

    capsule_runtime::report_println!("  variables:");
    // Align on the widest name, the way the artifact checklist aligns on `col_width`.
    let col_width = report
        .variables
        .iter()
        .map(|variable| variable.name.len())
        .max()
        .unwrap_or(0);
    for variable in &report.variables {
        let (mark, status) = if variable.set {
            ("\u{2713}", "set")
        } else {
            ("\u{2717}", "unset")
        };
        capsule_runtime::report_println!(
            "    {mark}  {name:<col_width$}   {status:<5}   \u{2014} {declared_by}",
            name = variable.name,
            declared_by = render_sources(variable)
        );
    }

    let unset: Vec<&RequiredVariable> = report
        .variables
        .iter()
        .filter(|variable| !variable.set)
        .collect();
    if unset.is_empty() {
        return;
    }

    let message = format!(
        "this project needs {count} variable{s} nothing in this environment sets: {names}",
        count = unset.len(),
        s = if unset.len() == 1 { "" } else { "s" },
        names = unset
            .iter()
            .map(|variable| variable.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for variable in &unset {
        findings.coded(
            E_CAP_014,
            DoctorBlock::Environment,
            Some(variable.name.clone()),
            &message,
            format!(
                "export {} — declared by {}",
                variable.name,
                render_sources(variable)
            ),
        );
    }
    capsule_runtime::report_eprintln!(
        "[mur doctor] error[{E_CAP_014}]: {message}\n  \
         Every name is copied from the launching shell at the moment of the spawn, so an unset \
         one reaches the capsule that declared it as absent — export it, or declare it in the \
         workspace .env."
    );
}

fn print_refusals(report: &EnvRequirementsReport, findings: &mut Findings) {
    if report.refusals.is_empty() {
        return;
    }

    capsule_runtime::report_println!("  declarations mur-roost will refuse:");
    for refusal in &report.refusals {
        capsule_runtime::report_println!(
            "    \u{2717}  {child} declares {axis} '{variable}', which its parent {parent} does not",
            child = refusal.child,
            axis = refusal.axis,
            variable = refusal.variable,
            parent = refusal.parent
        );
    }

    let message = format!(
        "{count} declaration{s} in the spawn closure exceed{verb} the envelope the capsule \
         spawning it holds, and mur-roost refuses a spawn that widens one.",
        count = report.refusals.len(),
        s = if report.refusals.len() == 1 { "" } else { "s" },
        verb = if report.refusals.len() == 1 { "s" } else { "" },
    );
    for refusal in &report.refusals {
        findings.coded(
            E_CAP_015,
            DoctorBlock::Environment,
            Some(refusal.child.to_string()),
            &message,
            format!(
                "{parent}: add '{variable}' to its {axis}, or drop it from {child}",
                parent = refusal.parent,
                variable = refusal.variable,
                axis = refusal.axis,
                child = refusal.child
            ),
        );
    }
    capsule_runtime::report_eprintln!(
        "[mur doctor] error[{E_CAP_015}]: {message}\n  \
         A child may declare no more than its parent does on {axis}.",
        axis = capsule_runtime::EnvelopeAxis::EnvAllow.manifest_key()
    );
}

fn print_uninspectable(report: &EnvRequirementsReport, findings: &mut Findings) {
    if report.uninspectable.is_empty() {
        return;
    }

    capsule_runtime::report_println!(
        "  could not inspect {} of {} capsules in the spawn closure:",
        report.uninspectable.len(),
        report.reached()
    );
    for capsule in &report.uninspectable {
        let reason = match &capsule.reason {
            UninspectableReason::NotInstalled => {
                "not installed in the project or global store".to_string()
            }
            UninspectableReason::AmbiguousVersion { installed } => format!(
                "{} versions installed ({}) and nothing pins which one a run would get",
                installed.len(),
                installed.join(", ")
            ),
            UninspectableReason::Unreadable { detail } => {
                format!("installed but unreadable: {detail}")
            }
        };
        capsule_runtime::report_println!(
            "    - {name} (declared by {declared_by}): {reason}",
            name = capsule.name,
            declared_by = capsule.declared_by
        );
        findings.warning(
            W_REG_002,
            DoctorBlock::Environment,
            Some(capsule.name.to_string()),
            format!(
                "{name} (declared by {declared_by}): {reason}",
                name = capsule.name,
                declared_by = capsule.declared_by
            ),
            format!(
                "{name}: {reason} — what it declares is missing from the list above",
                name = capsule.name
            ),
        );
    }

    capsule_runtime::report_eprintln!(
        "[mur doctor] warning[{W_REG_002}]: {count} capsule{s} in the spawn closure could not be \
         inspected, so what {pronoun} declares is absent from the report above: {names}\n  \
         The walk not being able to read a capsule is not evidence that a run fails, so this \
         does not change the exit code.\n  \
         ({link})",
        count = report.uninspectable.len(),
        s = if report.uninspectable.len() == 1 {
            ""
        } else {
            "s"
        },
        pronoun = if report.uninspectable.len() == 1 {
            "it"
        } else {
            "they"
        },
        names = report
            .uninspectable
            .iter()
            .map(|capsule| capsule.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        link = registry_warning_link(W_REG_002)
    );
}

/// Print the `Roster` block when `roster.yaml` sits in `project_root`: the roster admitted through
/// [`capsule_runtime::admit_roster_file`], with each member's peer posture, the expanded
/// reachability and a `W-ROS-001` line per member whose callers outnumber what it holds at once.
/// A refusal is an error in `findings`, and it fails the exit code; an overflow is a warning, and it
/// does not.
///
/// Prints nothing, and finds nothing, when there is no `roster.yaml`.
fn report_roster(
    project_root: &Path,
    registry: &dyn Registry,
    lock: Option<&MurmurLock>,
    findings: &mut Findings,
) {
    let path = resolve_roster_path(project_root);
    if !path.exists() {
        return;
    }

    capsule_runtime::report_println!("Roster");
    capsule_runtime::report_println!("  file: {}", path.display());
    let admitted = match capsule_runtime::admit_roster_file(project_root, registry, lock) {
        Ok(admitted) => admitted,
        Err(refusal) => {
            let error = CliError::from(refusal);
            capsule_runtime::report_println!("  \u{2717}  {}", error.message);
            capsule_runtime::report_println!();
            let hint = error.hint.unwrap_or_default();
            capsule_runtime::report_eprintln!(
                "[mur doctor] error[{code}]: {message}\n  hint: {hint}",
                code = error.code,
                message = error.message
            );
            findings.coded(error.code, DoctorBlock::Roster, None, &error.message, hint);
            return;
        }
    };

    let coordinates: Vec<String> = admitted
        .members()
        .iter()
        .map(|member| format!("{}@{}", member.capsule, member.version))
        .collect();
    let name_width = admitted
        .members()
        .iter()
        .map(|member| member.name.len())
        .max()
        .unwrap_or(0);
    let coordinate_width = coordinates.iter().map(String::len).max().unwrap_or(0);
    for (member, coordinate) in admitted.members().iter().zip(&coordinates) {
        let entry = if member.entry { "entry" } else { "" };
        let peers = if member.manifest.accepts_peer_tasks() {
            "serves peers"
        } else {
            "refuses peers"
        };
        let door = if member.requires_authentication() {
            "authenticated door"
        } else {
            "public door"
        };
        capsule_runtime::report_println!(
            "  {name:<name_width$}   {coordinate:<coordinate_width$}   {entry:<5}   {peers:<13}   {door}",
            name = member.name
        );
    }

    let edges = admitted
        .edges()
        .iter()
        .map(|edge| format!("{} \u{2192} {}", edge.from, edge.to))
        .collect::<Vec<_>>()
        .join(", ");
    match admitted.reachability() {
        _ if edges.is_empty() => capsule_runtime::report_println!("  reachability: none"),
        RosterReachability::All => {
            capsule_runtime::report_println!("  reachability (all): {edges}")
        }
        _ => capsule_runtime::report_println!("  reachability: {edges}"),
    }
    for overflow in admitted.caller_overflows() {
        let line = caller_overflow_warning(&overflow);
        capsule_runtime::report_println!("  {line}");
        findings.warning(
            W_ROS_001,
            DoctorBlock::Roster,
            Some(overflow.member.clone()),
            without_code(&line, W_ROS_001),
            caller_overflow_fix(&overflow),
        );
    }
    capsule_runtime::report_println!();
}

/// Who may call the capsule's door: the posture line, always, and `W-SEC-032` when `bind_addr`
/// exposes a door that declares no `network.authentication`. The warning is rendered from the
/// manifest by the same function `mur run --bind` uses, so the two lines are byte-identical.
fn report_door(runtime_manifest: &RuntimeManifest, bind_addr: &str) {
    capsule_runtime::report_println!("{}", capsule_runtime::door_posture(runtime_manifest));
    if let Some(line) = capsule_runtime::public_door_bind_warning(runtime_manifest, bind_addr) {
        capsule_runtime::report_println!("{line}");
    }
}

/// Check a formation directory, one holding `roster.yaml` and no `murmur.yaml`: admit its roster
/// from the stores, and against the lock, that `mur run --roster <dir>` uses, and return the
/// findings for doctor's tally. A formation directory has no artifacts of its own, so nothing else
/// is checked.
fn check_formation(formation_dir: &Path) -> Result<(u32, Findings), CliError> {
    let mut findings = Findings::default();
    capsule_runtime::report_println!(
        "No {MANIFEST_FILENAME} in {}: checking its {ROSTER_FILENAME}. Run mur doctor in a member's source directory to check that member's artifacts.",
        formation_dir.display()
    );
    capsule_runtime::report_println!();
    let lock = read_optional_lockfile(formation_dir)?;
    report_roster(
        formation_dir,
        &FallbackRegistry {
            primary: LocalRegistry::new(formation_dir.join(".murmur").join("artifacts")),
            secondary: LocalRegistry::from_default_home().map_err(CliError::from)?,
        },
        lock.as_ref(),
        &mut findings,
    );
    Ok((0, findings))
}

/// Each driver choice's output cap, context window, input ceiling and input budget under
/// `transport: http`, and where each comes from, through the resolver `mur run` stages with:
/// `W-RUN-009` in the words a launch prints, counted as a warning, and the `E-RUN-052`,
/// `E-RUN-054` or `E-RUN-055` refusal as an error.
/// `installed` is each installed artifact's declared name, `name@version` and bundled
/// `murmur.yaml`; a choice whose driver is not installed is left to the missing line the
/// checklist already printed.
fn report_model_limits(
    runtime_manifest: &RuntimeManifest,
    installed: &[(String, String, String)],
    findings: &mut Findings,
) {
    let Some(inference) = runtime_manifest
        .inference
        .as_ref()
        .filter(|inference| inference.transport == "http")
    else {
        return;
    };
    let find = |driver: &str| {
        installed.iter().find(|(name, _, _)| {
            murmur_artifact::tool_name_of(name) == murmur_artifact::tool_name_of(driver)
        })
    };
    let drivers = std::iter::once(
        inference
            .driver
            .as_ref()
            .map(|driver| driver.artifact.as_str())
            .unwrap_or_default(),
    )
    .chain(
        inference
            .alternates
            .iter()
            .map(|alternate| alternate.driver.as_str()),
    );
    if let Some(missing) = drivers.clone().find(|driver| find(driver).is_none()) {
        capsule_runtime::report_println!(
            "Model limits: not checked, driver {missing} is not installed"
        );
        return;
    }
    let resolved = capsule_runtime::model_limits::resolve_model_limits(
        inference,
        runtime_manifest.context.as_ref(),
        |driver| {
            let (label, manifest_yaml) =
                find(driver).map_or(("", ""), |(_, label, yaml)| (label.as_str(), yaml.as_str()));
            capsule_runtime::model_limits::DriverManifest {
                label,
                manifest_yaml,
            }
        },
    );
    capsule_runtime::report_println!("Model limits:");
    match resolved {
        Ok(choices) => {
            use capsule_runtime::model_limits::limit_or_none;
            for choice in &choices {
                let limits = choice.limits;
                capsule_runtime::report_println!(
                    "  \u{2713}  {} ({}, model {}): output cap {} ({}), context window {} ({}), \
                     input ceiling {} ({}), input budget {} ({})",
                    choice.choice,
                    choice.driver,
                    choice.model,
                    limits.max_output_tokens,
                    limits.max_output_tokens_source.wire_name(),
                    limit_or_none(limits.context_window),
                    limits.context_window_source.wire_name(),
                    limit_or_none(limits.max_input_tokens),
                    limits.max_input_tokens_source.wire_name(),
                    limit_or_none(limits.input_budget),
                    limits.input_budget_basis.wire_name()
                );
            }
            for choice in &choices {
                let Some(warning) = choice.warning.as_deref() else {
                    continue;
                };
                let line = capsule_runtime::model_limits::model_limits_warning_line(warning);
                capsule_runtime::report_eprintln!("[mur doctor] {line}");
                findings.warning(
                    W_RUN_009,
                    DoctorBlock::ModelLimits,
                    Some(choice.driver.to_string()),
                    without_code(&line, W_RUN_009),
                    format!(
                        "set context.max_tokens for model '{}' (driver choice '{}'), or use a \
                         release of {} whose murmur.yaml lists the model under models:",
                        choice.model, choice.choice, choice.driver
                    ),
                );
            }
        }
        Err(error) => {
            let refusal = crate::error::model_limits_error(&error);
            capsule_runtime::report_println!(
                "  \u{2717}  error[{}]: {}",
                refusal.code,
                refusal.message
            );
            findings.coded(
                refusal.code,
                DoctorBlock::ModelLimits,
                None,
                &refusal.message,
                refusal.hint.unwrap_or_default(),
            );
        }
    }
    capsule_runtime::report_println!();
}

/// A `transport: process` driver the artifact checklist passed: installed, locked as declared,
/// and built against interfaces this `mur` serves.
struct InstalledDriver {
    /// `name@version`, as the checklist names it.
    reference: String,
    name: String,
    version: String,
    /// The artifact's `.mur.zip` bytes.
    bytes: Vec<u8>,
}

/// The `Fix:` of an untested or unreadable harness version.
const HARNESS_VERSION_FIX: &str =
    "install a harness version the driver was tested against, or a driver release tested against \
     this one";

/// The `Fix:` of a harness the driver reads as logged out.
const HARNESS_LOGGED_OUT_FIX: &str =
    "log the harness in as this user on this host \u{2014} the process driver's reason above says how";

/// The `Fix:` of a harness whose login could not be checked.
const HARNESS_LOGIN_UNCHECKED_FIX: &str =
    "check by hand that the harness is logged in as this user on this host";

/// The `Process harness` block of a `transport: process` capsule: where the harness binary
/// resolves, whether its version is one the driver was tested against, and whether the driver
/// reads it as logged in, all asked through [`capsule_runtime::check_process_harness`] from the
/// driver, binary, environment and probes `mur run` uses.
///
/// `driver` is the inference driver when the checklist passed it. When it did not, the checklist
/// already counted why, so the block is one line and no finding; so is a driver declared with
/// `source:` (`local_source`), whose bytes doctor does not hold. A refusal staging would make
/// before resolving the binary is an error under that refusal's code; a binary that does not
/// resolve is `E-RUN-006` and gets no version or login line; a logged-out harness is `E-RUN-064`;
/// an untested version is `W-RUN-002` and a login doctor could not check is `W-RUN-013`. Only the
/// driver's reason is printed, never what the harness's login command printed.
fn report_process_harness(
    runtime_manifest: &RuntimeManifest,
    driver: Option<&InstalledDriver>,
    local_source: bool,
    policy: &capsule_runtime::CapabilityPolicy,
    project_root: &Path,
    findings: &mut Findings,
) {
    use capsule_runtime::{HarnessBinary, HarnessLogin, HarnessVersion};

    let Some(inference) = runtime_manifest
        .inference
        .as_ref()
        .filter(|inference| inference.transport == "process")
    else {
        return;
    };
    let Some(driver) = driver else {
        let name = inference
            .driver
            .as_ref()
            .map(|driver| driver.artifact.as_str())
            .unwrap_or_default();
        let why = if local_source {
            "is a local source"
        } else {
            "did not pass the checklist"
        };
        capsule_runtime::report_println!("Process harness: not checked, driver {name} {why}");
        return;
    };
    let reference = &driver.reference;
    let check = match capsule_runtime::check_process_harness(
        &driver.bytes,
        &driver.name,
        &driver.version,
        inference.command.as_deref(),
        policy,
        project_root,
    ) {
        Ok(check) => check,
        Err(error) => {
            let error = CliError::from(error);
            capsule_runtime::report_println!("Process harness ({reference}):");
            capsule_runtime::report_println!(
                "  \u{2717}  error[{}]: {}",
                error.code,
                error.message
            );
            findings.coded(
                error.code,
                DoctorBlock::Harness,
                Some(reference.clone()),
                &error.message,
                error.hint.unwrap_or_default(),
            );
            capsule_runtime::report_println!();
            return;
        }
    };
    let harness = check.harness;
    capsule_runtime::report_println!("Process harness ({reference}, harness {harness}):");
    match check.binary {
        HarnessBinary::Resolved { path, source } => {
            capsule_runtime::report_println!(
                "  \u{2713}  binary   {} (named by {source})",
                path.display()
            );
        }
        HarnessBinary::NotFound(error) => {
            let binary = match &error {
                capsule_runtime::RuntimeError::HarnessBinaryNotFound { binary, .. } => {
                    Some(binary.clone())
                }
                _ => None,
            };
            let error = CliError::from(error);
            capsule_runtime::report_println!(
                "  \u{2717}  error[{}]: {}",
                error.code,
                error.message
            );
            findings.coded(
                error.code,
                DoctorBlock::Harness,
                binary,
                &error.message,
                error.hint.unwrap_or_default(),
            );
        }
    }
    match check.version {
        Some(HarnessVersion::Tested { line }) => {
            capsule_runtime::report_println!("  \u{2713}  version  {line}, tested");
        }
        Some(HarnessVersion::Untested { message }) => {
            capsule_runtime::report_println!("  \u{26A0}  warning[{W_RUN_002}]: {message}");
            findings.warning(
                W_RUN_002,
                DoctorBlock::Harness,
                Some(harness.clone()),
                message,
                HARNESS_VERSION_FIX,
            );
        }
        None => {}
    }
    match check.login {
        Some(HarnessLogin::LoggedIn) => {
            capsule_runtime::report_println!("  \u{2713}  login    logged in");
        }
        Some(HarnessLogin::LoggedOut { reason }) => {
            let message = format!("{harness} is not logged in for this user: {reason}");
            capsule_runtime::report_println!("  \u{2717}  error[{E_RUN_064}]: {message}");
            findings.coded(
                E_RUN_064,
                DoctorBlock::Harness,
                Some(harness.clone()),
                &message,
                HARNESS_LOGGED_OUT_FIX,
            );
        }
        Some(HarnessLogin::NotChecked { reason }) => {
            let message = format!("login not checked \u{2014} {reason}");
            capsule_runtime::report_println!("  \u{26A0}  warning[{W_RUN_013}]: {message}");
            findings.warning(
                W_RUN_013,
                DoctorBlock::Harness,
                Some(harness.clone()),
                message,
                HARNESS_LOGIN_UNCHECKED_FIX,
            );
        }
        None => {}
    }
    capsule_runtime::report_println!();
}

/// The `Declared needs` section: for each tool the capsule pins that declares needs for a program
/// `shell.allow` grants, one line with how many needs this host grants and lacks, then one
/// `W-RUN-011` warning per missing need, each counted in the tally, naming the package the tool
/// declares for this host's distribution. Nothing is printed when no need applies. A malformed
/// `programs:` block is the launch's `E-RUN-062`, an error in `findings`.
fn report_declared_needs(
    runtime_manifest: &RuntimeManifest,
    lock: Option<&murmur_artifact::MurmurLock>,
    project_root: &std::path::Path,
    shell_allow: &[String],
    pinned: &[capsule_runtime::pinned_programs::ResolvedPinnedProgram],
    findings: &mut Findings,
) {
    let Some(writable) =
        crate::commands::declared_needs::capsule_writable_trees(&project_root.join("workdir"))
    else {
        return;
    };
    let writable: Vec<&Path> = writable.iter().map(std::path::PathBuf::as_path).collect();
    let resolved = match crate::commands::declared_needs::installed_declared_needs(
        &runtime_manifest.artifacts,
        lock,
        project_root,
        shell_allow,
        &writable,
        pinned,
    ) {
        Ok(resolved) => resolved,
        Err(refusal) => {
            capsule_runtime::report_println!("Declared needs:");
            capsule_runtime::report_println!(
                "  \u{2717}  error[{}]: {}",
                refusal.code,
                refusal.message
            );
            findings.coded(
                refusal.code,
                DoctorBlock::Artifacts,
                None,
                &refusal.message,
                refusal.hint.unwrap_or_default(),
            );
            capsule_runtime::report_println!();
            return;
        }
    };
    if resolved.is_empty() {
        return;
    }
    let distributions = capsule_runtime::declared_needs::host_distributions();
    capsule_runtime::report_println!("Declared needs:");
    for program in &resolved {
        let mark = if program.missing.is_empty() {
            "\u{2713}"
        } else {
            "\u{26A0}"
        };
        capsule_runtime::report_println!(
            "  {mark}  {} ({}): {} granted, {} missing",
            program.artifact,
            program.program,
            program.granted.len(),
            program.missing.len()
        );
    }
    for program in &resolved {
        for missing in &program.missing {
            let (line, fix) =
                declared_need_warning(&program.artifact, &program.program, missing, &distributions);
            capsule_runtime::report_println!("  {line}");
            findings.warning(
                murmur_artifact::W_RUN_011,
                DoctorBlock::Artifacts,
                Some(program.artifact.clone()),
                without_code(&line, murmur_artifact::W_RUN_011),
                fix,
            );
        }
    }
    capsule_runtime::report_println!();
}

/// The `Pinned programs` section: for each program a tool the capsule pins with an `operator`
/// lock origin pins a download of and `shell.allow` grants, one line with the state of the store's
/// copy (verified in full) and which copy runs, then one `W-RUN-012` warning, counted in the
/// tally, when nothing usable runs or the stored copy fails its checksum. Nothing is printed when
/// no program applies, and nothing here is an error: a launch never refuses over a pinned program.
fn report_pinned_programs(
    tools: &[capsule_runtime::pinned_programs::DeclaredToolDownloads],
    shell_allow: &[String],
    findings: &mut Findings,
) {
    use capsule_runtime::pinned_programs::{host_program, verify_entry, EntryState};

    let platform = murmur_artifact::host_download_platform();
    let platform_label = crate::commands::pinned_programs::platform_label();
    let store_root = capsule_runtime::pinned_programs::program_store_root().ok();
    let path_dirs = capsule_runtime::pinned_programs::host_path_dirs();
    let mut lines = Vec::new();
    let mut warnings = Vec::new();
    let code = murmur_artifact::W_RUN_012;
    for (tool, program) in capsule_runtime::pinned_programs::gated_programs(tools, shell_allow) {
        let name = &program.program;
        let artifact = &tool.artifact;
        let owner = format!("{artifact} ({name})");
        let host = host_program(name, &path_dirs);
        let runs = |path: &Path, source: &str| format!("runs {} ({source})", path.display());
        let Some(download) = platform.and_then(|platform| program.for_platform(platform)) else {
            match &host {
                Some(path) => lines.push(format!(
                    "\u{2713}  {owner}: no pinned download for {platform_label}; {}",
                    runs(path, "host")
                )),
                None => {
                    lines.push(format!(
                        "\u{26A0}  {owner}: no pinned download for {platform_label}; nothing \
                         runs"
                    ));
                    warnings.push((
                        artifact.clone(),
                        format!(
                            "warning[{code}]: {artifact} pins no download of {name} for \
                             {platform_label}, and this host has no {name}"
                        ),
                        format!(
                            "install {name} on this host so it is on PATH, or use a release \
                             of {artifact} that pins a download for {platform_label}"
                        ),
                    ));
                }
            }
            continue;
        };
        let state = store_root
            .as_deref()
            .map_or(EntryState::Missing, |root| verify_entry(root, download));
        let (copy, failing) = match &state {
            EntryState::Present { .. } => ("present", None),
            EntryState::Missing => ("missing", None),
            EntryState::ChecksumFailed { reason } => ("failing its checksum", Some(reason.clone())),
        };
        let running = match (&host, &state) {
            (Some(path), _) => Some(runs(path, "host")),
            (None, EntryState::Present { path, .. }) => Some(runs(path, "pinned")),
            _ => None,
        };
        let mark = if running.is_some() && failing.is_none() {
            "\u{2713}"
        } else {
            "\u{26A0}"
        };
        lines.push(format!(
            "{mark}  {owner}: pinned copy {copy}; {}",
            running.as_deref().unwrap_or("nothing runs")
        ));
        if let Some(reason) = failing {
            warnings.push((
                artifact.clone(),
                format!(
                    "warning[{code}]: {artifact}'s pinned copy of {name} fails its checksum: \
                     {reason}; run mur install to fetch it again"
                ),
                format!(
                    "run mur install to fetch {name} for {platform_label} ({artifact}) again; \
                     the stored copy fails its checksum: {reason}"
                ),
            ));
        } else if running.is_none() {
            warnings.push((
                artifact.clone(),
                format!(
                    "warning[{code}]: {artifact} pins {name} for {platform_label}, and the \
                     store holds no copy; run mur install"
                ),
                format!(
                    "run mur install in this project to fetch {name} for {platform_label} \
                     ({artifact})"
                ),
            ));
        }
    }
    if lines.is_empty() {
        return;
    }
    capsule_runtime::report_println!("Pinned programs:");
    for line in &lines {
        capsule_runtime::report_println!("  {line}");
    }
    for (artifact, line, fix) in warnings {
        capsule_runtime::report_println!("  {line}");
        findings.warning(
            code,
            DoctorBlock::Artifacts,
            Some(artifact),
            without_code(&line, code),
            fix,
        );
    }
    capsule_runtime::report_println!();
}

/// The `W-RUN-011` line for one missing need, and the `Fix:` the tally prints for it.
///
/// Only a need that resolves to nothing ([`capsule_runtime::declared_needs::NOT_FOUND`]) names a
/// package: a need whose program is not on `PATH`, or that resolves to a location no need may
/// reach, would stay missing with the package installed, so its line gives the reason instead.
fn declared_need_warning(
    artifact: &str,
    program: &str,
    missing: &capsule_runtime::declared_needs::MissingNeed,
    distributions: &[String],
) -> (String, String) {
    let need = &missing.need;
    let code = murmur_artifact::W_RUN_011;
    if missing.reason != capsule_runtime::declared_needs::NOT_FOUND {
        return (
            format!(
                "warning[{code}]: {artifact} declares {} for {program}, and it is not granted: {}",
                need.form, missing.reason
            ),
            format!(
                "{} for {program} ({artifact}) is not granted: {}",
                need.form, missing.reason
            ),
        );
    }
    let head = format!(
        "warning[{code}]: {artifact} declares {} for {program}, and this host has none",
        need.form
    );
    match capsule_runtime::declared_needs::package_for(need, distributions) {
        Some((package, distribution)) => (
            format!("{head}; install {package} ({distribution})"),
            format!(
                "install {package} for {} ({program}, {artifact})",
                need.form
            ),
        ),
        None => {
            let ids = if distributions.is_empty() {
                "this host, which has no os-release".to_string()
            } else {
                distributions.join(", ")
            };
            (
                format!("{head}; the artifact names no package for {ids}"),
                format!(
                    "install what provides {} for {program} on this host ({artifact} names no \
                     package for it)",
                    need.form
                ),
            )
        }
    }
}

/// Doctor's closing lines, and whether they fail it.
#[derive(Debug, PartialEq, Eq)]
struct Tally {
    /// Every closing line, each ending in a newline.
    text: String,
    /// Whether any error was found, which makes doctor exit `1`.
    fails: bool,
}

/// Renders doctor's closing lines from `findings`.
///
/// Nothing found is `All checks passed.`. Otherwise the tally,
/// `N check(s) passed, M error(s) found[, K warning(s)].`, then, when there is an error,
/// `First refusal: <headline>` for the first one doctor printed, then a blank line and one `Fix:`
/// line per error, then per warning. A warning alone does not fail.
fn render_tally(total_pass: u32, findings: &Findings) -> Tally {
    use std::fmt::Write as _;

    let (errors, warnings) = (&findings.errors, &findings.warnings);
    if errors.is_empty() && warnings.is_empty() {
        return Tally {
            text: "All checks passed.\n".to_string(),
            fails: false,
        };
    }

    let total_fail = errors.len();
    let total_warn = warnings.len();
    let ps = if total_pass == 1 { "" } else { "s" };
    let es = if total_fail == 1 { "" } else { "s" };
    let warn_tail = if total_warn == 0 {
        String::new()
    } else {
        let ws = if total_warn == 1 { "" } else { "s" };
        format!(", {total_warn} warning{ws}")
    };
    let mut text =
        format!("{total_pass} check{ps} passed, {total_fail} error{es} found{warn_tail}.\n");
    if let Some(first) = errors.first() {
        let _ = writeln!(text, "First refusal: {}", first.headline());
    }
    text.push('\n');
    for fix in errors.iter().chain(warnings).map(|finding| &finding.fix) {
        let _ = writeln!(text, "Fix: {fix}");
    }
    Tally {
        text,
        fails: total_fail > 0,
    }
}

/// Prints [`render_tally`]'s lines, and exits the process with status `1` when they fail.
fn print_tally(total_pass: u32, findings: &Findings) {
    let tally = render_tally(total_pass, findings);
    capsule_runtime::report_print!("{}", tally.text);
    if tally.fails {
        // Non-zero so `mur doctor` works as a CI pre-flight check. `process::exit` runs no
        // destructors, so every line must already be printed.
        std::process::exit(1);
    }
}

/// `mur doctor --json`'s verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    /// No error was found; warnings may have been.
    Ready,
    /// At least one error was found.
    Refused,
}

/// A finding's severity in `mur doctor --json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Error,
    Warning,
}

/// The one document `mur doctor --json` prints. Serialized keys keep the field order.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct DoctorReport {
    verdict: Verdict,
    /// What the process exits with: `0` for [`Verdict::Ready`], `1` for [`Verdict::Refused`].
    exit_code: i32,
    /// The tally line's three numbers.
    checks_passed: u32,
    errors: usize,
    warnings: usize,
    /// Every counted finding, in the order the text form prints its `Fix:` lines: errors, then
    /// warnings, each in print order.
    findings: Vec<DoctorFinding>,
}

/// One entry of [`DoctorReport::findings`]. Serialized keys keep the field order.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
struct DoctorFinding {
    severity: Severity,
    code: &'static str,
    block: DoctorBlock,
    /// `null` for a host-wide finding.
    subject: Option<String>,
    message: String,
    fix: String,
}

impl DoctorFinding {
    fn new(severity: Severity, finding: &Finding) -> Self {
        Self {
            severity,
            code: finding.code,
            block: finding.block,
            subject: finding.subject.clone(),
            message: finding.message.clone(),
            fix: finding.fix.clone(),
        }
    }
}

impl DoctorReport {
    /// The document for a doctor run that reached its tally.
    fn from_findings(total_pass: u32, findings: &Findings) -> Self {
        let refused = !findings.errors.is_empty();
        Self {
            verdict: if refused {
                Verdict::Refused
            } else {
                Verdict::Ready
            },
            exit_code: i32::from(refused),
            checks_passed: total_pass,
            errors: findings.errors.len(),
            warnings: findings.warnings.len(),
            findings: findings
                .errors
                .iter()
                .map(|finding| DoctorFinding::new(Severity::Error, finding))
                .chain(
                    findings
                        .warnings
                        .iter()
                        .map(|finding| DoctorFinding::new(Severity::Warning, finding)),
                )
                .collect(),
        }
    }

    /// The document for a doctor run `error` stopped before its tally: refused, with `error` as
    /// its one finding, its hint as the fix.
    fn early(error: &CliError, block: DoctorBlock) -> Self {
        Self {
            verdict: Verdict::Refused,
            exit_code: 1,
            checks_passed: 0,
            errors: 1,
            warnings: 0,
            findings: vec![DoctorFinding {
                severity: Severity::Error,
                code: error.code,
                block,
                subject: None,
                message: error.message.lines().next().unwrap_or_default().to_string(),
                fix: error.hint.clone().unwrap_or_default(),
            }],
        }
    }

    /// The pretty-printed document and its closing newline.
    fn render(&self) -> String {
        let text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|error| unreachable!("the doctor report always serializes: {error}"));
        format!("{text}\n")
    }
}

/// `mur doctor [--json]`.
///
/// A current directory holding `roster.yaml` and no `murmur.yaml` is a formation directory, and
/// is checked as one by [`check_formation`] without walking up; `bind_addr` has nothing to check
/// there. Anywhere else [`check_project`] checks the project.
///
/// The text form prints the report and its tally on stdout, and exits `1` when an error was found.
/// With `json`, every line of that text form goes to stderr unchanged, and stdout holds one JSON
/// document, [`DoctorReport`], whose `exit_code` the process exits with. An error that stops doctor
/// before its tally is printed as the text form prints it and becomes a `refused` document with
/// that error as its one finding.
pub(crate) fn run_doctor(bind_addr: &str, json: bool) -> Result<(), CliError> {
    if json {
        capsule_runtime::diagnostic::send_stdout_to_stderr();
    }
    let (checked, block) = match std::env::current_dir() {
        Err(e) => (
            Err(CliError::new(
                E_IO_003,
                format!("failed to determine current directory: {e}"),
            )),
            DoctorBlock::Project,
        ),
        Ok(cwd) if resolve_roster_path(&cwd).exists() && !resolve_manifest_path(&cwd).exists() => {
            (check_formation(&cwd), DoctorBlock::Roster)
        }
        Ok(_) => (check_project(bind_addr), DoctorBlock::Project),
    };
    if !json {
        let (total_pass, findings) = checked?;
        print_tally(total_pass, &findings);
        return Ok(());
    }
    let report = match checked {
        Ok((total_pass, findings)) => {
            capsule_runtime::report_print!("{}", render_tally(total_pass, &findings).text);
            DoctorReport::from_findings(total_pass, &findings)
        }
        Err(error) => {
            capsule_runtime::runtime_err!("{error}");
            DoctorReport::early(&error, block)
        }
    };
    capsule_runtime::diagnostic::document_to_stdout(&report.render());
    if report.exit_code != 0 {
        std::process::exit(report.exit_code);
    }
    Ok(())
}

/// What the Artifacts checklist says about one installed artifact's attestation.
enum AttestationLine {
    /// The kept bundle verified and its signer is the one `murmur.lock` records, or it records
    /// none.
    Attested(String),
    /// Anything else, as the status after `installed ·` and its warning finding.
    Warn {
        code: &'static str,
        status: String,
        fix: String,
    },
}

/// Verify, offline, the attestation kept beside an installed `name@version` (`resolved`): from
/// the project store when it holds the payload, else from the global store, held to the stored
/// owner id and to no repository. `None` for a `local/` artifact, which is never checked.
fn artifact_attestation(
    name: &str,
    version: &str,
    project_registry: &LocalRegistry,
    global_registry: &LocalRegistry,
    resolved: &ResolvedArtifact,
    lock: Option<&MurmurLock>,
) -> Option<AttestationLine> {
    use crate::attestation::{assess, trusted_root, AttestationState, Expected};

    let parsed = murmur_artifact::ArtifactName::parse(name).ok()?;
    if parsed.is_local() {
        return None;
    }
    let reference = format!("{name}@{version}");
    let store = if project_registry
        .artifact_path_for(&parsed, version)
        .exists()
    {
        project_registry
    } else {
        global_registry
    };
    let failed = |reason: String| AttestationLine::Warn {
        code: W_REG_005,
        status: format!("attestation does not verify: {reason}"),
        fix: format!("mur install {reference}"),
    };
    let bundle = match store.read_attestation(&parsed, version) {
        Ok(Some(bundle)) => bundle,
        Ok(None) => {
            return Some(AttestationLine::Warn {
                code: W_REG_005,
                status: "no attestation".to_string(),
                fix: format!(
                    "ask the publisher of {} to attest its releases, then run mur install \
                     {reference}",
                    parsed.scope()
                ),
            })
        }
        Err(error) => return Some(failed(error.to_string())),
    };
    let root = match trusted_root() {
        Ok(root) => root,
        Err(error) => return Some(failed(error.0)),
    };
    let expected = Expected {
        sha256: &resolved.sha256,
        owner_id: crate::commands::install::stored_owner_expectation(
            &parsed,
            resolved.meta.owner_id,
            lock,
        ),
        repo: None,
    };
    let recorded = lock
        .and_then(|lock| lock.artifact_for(name))
        .and_then(|entry| entry.signer.as_deref());
    Some(match assess(Some(&bundle), &expected, root) {
        Ok(AttestationState::Verified { signer }) => match recorded {
            Some(recorded) if recorded != signer => AttestationLine::Warn {
                code: W_REG_006,
                status: format!("attested by {signer}; murmur.lock recorded {recorded}"),
                fix: format!("mur install {reference}"),
            },
            _ => AttestationLine::Attested(signer),
        },
        Ok(AttestationState::Unverifiable(reason)) | Err(reason) => failed(reason),
        Ok(AttestationState::Absent) => failed("no bundle".to_string()),
    })
}

/// Check every artifact the current project declares against the stores a session
/// resolves from, and return the passed-check count and the findings for the tally. The checklist
/// is the manifest — editing `murmur.yaml` changes what is checked, with no change here.
fn check_project(bind_addr: &str) -> Result<(u32, Findings), CliError> {
    let mut findings = Findings::default();
    let project_root = find_project_root().map_err(|mut error| {
        error.hint = Some(
            "run `mur doctor` from inside a project directory, or from a formation directory holding `roster.yaml`"
                .to_string(),
        );
        error
    })?;
    let manifest_path = resolve_manifest_path(&project_root);
    // The text is kept for the reference scan below, so the file is read once. The parse keeps
    // what each `gateway.api_key` names without reading any value, so a manifest referencing a
    // variable this shell does not hold still loads — that is exactly the manifest an operator runs
    // `mur doctor` to diagnose.
    let manifest_yaml =
        read_runtime_manifest_text(&manifest_path).map_err(runtime_manifest_error_to_cli)?;
    let runtime_manifest =
        RuntimeManifest::from_yaml_str(&manifest_yaml).map_err(runtime_manifest_error_to_cli)?;

    // Keys this build does not recognize, in the same words and from the same emitter `mur run`
    // uses — an operator reaching for `mur doctor` to find out why a declaration did nothing is
    // exactly who this is for. Non-fatal, stderr only; nothing here is counted.
    warn_on_unknown_manifest_keys(&runtime_manifest);

    // `mur doctor` validates the manifest without launching a session, but the capsule-ceiling
    // `interpreter_runtime` grant is a posture warning an operator should see here too — surface
    // the same `W-SEC-009` a `mur run` would, straight from the parsed manifest's own
    // `capabilities.shell`. Non-fatal, stderr only, exactly as it fires at staging.
    if let Some(interpreter_runtime) = runtime_manifest
        .capabilities
        .as_ref()
        .and_then(|caps| caps.shell.as_ref())
        .map(|shell| shell.interpreter_runtime.as_slice())
    {
        warn_on_interpreter_runtime_grants(interpreter_runtime);
    }

    // Same reasoning for `capabilities.filesystem.workdir_exec`: a posture warning an operator
    // should see from `mur doctor` rather than only from a launched session, since it is the one
    // declaration that trades away an enforcement property (`shell.allow` stops being kernel-
    // enforced inside the workdir) and caps the capsule's achieved containment class at
    // `advisory`. Same `W-SEC-011`, same wording, straight from the parsed manifest.
    warn_on_workdir_exec(
        runtime_manifest
            .capabilities
            .as_ref()
            .and_then(|caps| caps.filesystem.as_ref())
            .is_some_and(|filesystem| filesystem.workdir_exec),
    );

    // Same reasoning for `capabilities.env.allow`: a credential-shaped entry hands the capsule a
    // host secret murmur does not broker, and an operator asking which capsule holds what should
    // get the answer from `mur doctor` too. Same `W-SEC-024`, same emitter, same wording as a
    // `mur run` — `None` for the override because doctor has no lifecycle flag to apply.
    warn_on_secret_shaped_env_grants(
        &capability_policy_from_runtime_manifest(&runtime_manifest),
        runtime_manifest.lifecycle.as_ref(),
        None,
    );

    // The entries `W-SEC-024` passes over because the backstop strips them are an `E-CAP-016`
    // refusal at `mur run`, and an error here.
    if let Err(error) = capsule_runtime::check_env_allow_reaches_guests(
        &capability_policy_from_runtime_manifest(&runtime_manifest),
    ) {
        let names = match &error {
            capsule_runtime::RuntimeError::EnvAllowStrippedByBackstop { entries } => entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            _ => "the stripped names".to_string(),
        };
        findings.predict(
            DoctorBlock::Capabilities,
            Some(names.clone()),
            PredictedRefusal {
                code: E_CAP_016,
                message: error.to_string(),
                consequence: "`mur run` will refuse this capsule — remove the entries above; no \
                          manifest setting exempts a name from the credential backstop."
                    .to_string(),
                fix: format!(
                "remove {names} from capabilities.env.allow; the credential backstop strips them \
                 whatever the manifest says"
            ),
            },
        );
    }

    // And `W-SEC-025`, from the same emitter `mur run` calls: an allow-list entry naming a
    // gateway's host grants direct reach without the key and does not serve the gateway.
    warn_on_gateway_endpoint_in_network_allow(
        &capability_policy_from_runtime_manifest(&runtime_manifest),
        &runtime_manifest.artifacts,
    );

    // And `W-SEC-027`, from the same emitter `mur run` calls: a key read only at launch cannot be
    // rotated into a running capsule.
    warn_on_launch_only_gateway_credential(
        &runtime_manifest.artifacts,
        crate::config::mur_config_path().ok().as_deref(),
        runtime_manifest.control.as_ref(),
    );

    // And `W-SEC-030`, from the same emitter `mur run` calls: a gateway the spend ceilings do not
    // cover.
    warn_on_unmetered_gateways(
        runtime_manifest.inference.as_ref(),
        &runtime_manifest.artifacts,
    );

    report_door(&runtime_manifest, bind_addr);

    // Everything below reads `~/.murmur`, and no launch here would start without it: the reason
    // stands in for both directories, and the check fails.
    if let Err(error) = capsule_runtime::usable_murmur_home() {
        report_home_unusable(&error);
        return Err(CliError::from(error));
    }

    let effective_config = load_effective_mur_config_if_any_exists()?;

    // The floor `mur run` with no flags would stage at, from the manifest and the workspace
    // config. A `--containment` flag could only raise it, so a refusal predicted against it is the
    // refusal of that flagless run, and it is an error here; the `E-CAP-004` fix names the flag as
    // one way out. The `staged_runtime` check and the two reachability checks below all read it.
    let manifest_floor = runtime_manifest
        .capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.containment);
    let containment_config = crate::config::load_containment_config()?;
    let declared_floor = doctor_containment_stance(
        containment_config,
        manifest_floor,
        effective_containment_floor(
            effective_config
                .as_ref()
                .and_then(|config| config.containment),
            manifest_floor,
            None,
        ),
        &mut findings,
    );
    let staged_runtime = runtime_manifest
        .capabilities
        .as_ref()
        .and_then(|caps| caps.shell.as_ref())
        .map(|shell| shell.staged_runtime.as_slice())
        .unwrap_or_default();
    if !staged_runtime.is_empty() {
        if let Err(error) = check_staged_runtime_floor(staged_runtime, declared_floor) {
            const RAISE: &str = "set `capabilities.containment: sealed` in murmur.yaml, set \
                                 `containment: sealed` in .murmur/config.yaml, or pass \
                                 `--containment sealed`";
            findings.predict(
                DoctorBlock::Capabilities,
                Some("capabilities.shell.staged_runtime".to_string()),
                PredictedRefusal {
                    code: E_CAP_004,
                    message: error.to_string(),
                    consequence: format!(
                        "`mur run` will refuse this capsule unless the floor is raised — {RAISE}."
                    ),
                    fix: RAISE.to_string(),
                },
            );
        }
    }

    // The two stage-time reachability checks: finding out from `mur doctor` that a `sealed`
    // capsule's `pip` can never import its own package beats finding it out from a
    // `ModuleNotFoundError` several agent turns into a run. Both resolve `shell.allow` against
    // this host's real `PATH` and read real files, but neither creates a session workdir nor
    // contacts a registry, so they belong in doctor's manifest-only prologue rather than in its
    // artifact loop below.
    //
    // The first is an `E-CAP-006` refusal at `mur run`, and an error here.
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);
    if let Err(error) = check_interpreted_entrypoints_reachable(&capability_policy, declared_floor)
    {
        let interpreters = match &error {
            capsule_runtime::RuntimeError::ShellBinaryPackageUnreachable { entries } => {
                let mut names: Vec<&str> = Vec::new();
                for entry in entries {
                    if !names.contains(&entry.interpreter.as_str()) {
                        names.push(&entry.interpreter);
                    }
                }
                names.join(", ")
            }
            _ => "the interpreter".to_string(),
        };
        findings.predict(
            DoctorBlock::Capabilities,
            Some(interpreters.clone()),
            PredictedRefusal {
            code: E_CAP_006,
            message: error.to_string(),
            consequence: "`mur run` will refuse this capsule at the declared floor — declare the \
                          grant above, or lower `capabilities.containment` if this capsule does \
                          not need a composed root."
                .to_string(),
            fix: format!(
                "declare capabilities.shell.interpreter_runtime or capabilities.shell.staged_runtime \
                 for {interpreters}, or lower capabilities.containment"
            ),
        },
        );
    }
    // The second already prints its own `W-SEC-012` and `W-SEC-029` lines, in the same words
    // `mur run` uses, so there is nothing to reformat here — the returned report is only for
    // callers that want to inspect what was warned about.
    let _ = warn_on_unreachable_toolchain_helpers(&capability_policy, declared_floor);

    // A capsule that can spawn a subprocess needs a network namespace to put it in, and a host
    // that cannot create one refuses the run with E-CAP-005, an error here. Unlike the two checks
    // above this is a pure *host* question — no flag can change the answer — so doctor asks it
    // whenever the capsule has a subprocess route at all, which is exactly what `mur run` does.
    let capabilities = runtime_manifest.capabilities.as_ref();
    let spawn_allows = capabilities
        .and_then(|caps| caps.spawn.as_ref())
        .is_some_and(|spawn| !spawn.allow.is_empty());
    // An unconfined launch's subprocesses run in no namespace, so none is needed.
    let can_spawn_subprocess = has_subprocess_route_at_staging(&capability_policy)
        && declared_floor != murmur_artifact::ContainmentClass::Unconfined;
    // One reading of the host for the whole checklist: the `E-CAP-005` error, the `AppArmor /
    // user namespaces` block and the runtime directory sections below cannot describe two
    // different hosts.
    let host_probe = HostProbe::probe();
    let egress_failure = host_probe.egress_namespace_failure();
    if let Some(refusal) = egress_namespace_refusal(can_spawn_subprocess, egress_failure.as_ref()) {
        findings.predict(DoctorBlock::Host, None, refusal);
    }

    // The daemon that capsule registers with at launch, gated on `spawn_allows` alone: a capsule
    // declaring only `capabilities.shell.allow` spawns subprocesses of its own and
    // addresses no daemon, so `can_spawn_subprocess` above is the wrong question here.
    if spawn_allows {
        report_roost_daemon(&mut findings);
    }

    // Where this host's permission to create an unprivileged user namespace comes from, and how
    // the installed AppArmor profile compares to the one this build ships. Both are pure host
    // questions with no manifest input, printed for every project — an operator reading `achieved:
    // sealed` needs to know whether that came from the profile murmur ships or from the host's
    // unprivileged-userns hardening being switched off for every binary on the machine.
    report_userns_grant(&host_probe);

    // Which Landlock ABI the kernel offers and which the runtime builds a capsule's ruleset at. A
    // host question too, printed for every project: below ABI v2 a capsule cannot move a file
    // between two folders of its own workdir, which breaks any tool that renames a file into place
    // with no hint of why.
    report_landlock_abi();

    // The modes of everything under `~/.murmur`, which holds provider credentials and deploy keys
    // whatever the project in front of doctor declares.
    report_murmur_home();

    // What each guest will actually be preopened into, printed before the artifact checklist for
    // the same reason the block above is: it is a property of the manifest, not of any one
    // artifact's presence in a store, and it is the question `capabilities.filesystem.scope` at
    // the capsule level does not answer.
    let preopens = report_preopens(
        &runtime_manifest,
        &project_root,
        declared_floor,
        effective_config.as_ref(),
        &mut findings,
    );

    // Every route the capsule runs code on and what holds each, from the table `--explain-scope`
    // prints, and `W-SEC-034` from the emitter a launch calls.
    report_routes(
        &runtime_manifest,
        &capability_policy,
        &preopens,
        &host_probe,
        declared_floor,
    );

    // The `read_only` declaration and whether it is enforced or advisory, through the renderer
    // `--explain-scope` prints from, for the reason `report_preopens` shares `PreopenReport::render`.
    report_read_only(&capability_policy);

    // Which skills and tools the capsule may pull at runtime, through the renderer
    // `--explain-scope` prints from.
    report_install(&capability_policy);

    // The memory ceiling `capabilities.resources` gives the subprocess tree, as the cgroup
    // files a launch writes it to.
    report_memory_bound(&capability_policy);

    // A lockfile is optional. When one is present it is what `mur run` enforces, so
    // doctor checks against it too; when it is absent doctor reports presence only,
    // exactly as before. A lockfile that exists but cannot be read is a hard failure
    // before any checklist line prints — same as a malformed murmur.yaml.
    //
    // Read here rather than beside the stores below because the spawn-closure walk pins a child
    // capsule's version from it, and reading `murmur.lock` twice could report two answers.
    let lock = read_optional_lockfile(&project_root)?;

    // What the whole delegation closure needs from the operator's environment, and what the
    // project manifest itself references and this workspace does not hold. Ungated: a capsule
    // that delegates to nobody has no `spawn.allow` to walk, but it can still reference a
    // variable nothing sets, and that is the one thing this block has to say about it.
    //
    // Where the walk knows a spawn in the closure cannot succeed it records an error; where it
    // does not know — a capsule it could not read — it warns.
    report_env_requirements(
        &runtime_manifest,
        &manifest_yaml,
        &project_root,
        lock.as_ref(),
        &mut findings,
    );
    report_control_secrets(&runtime_manifest);

    let project_registry = LocalRegistry::new(project_root.join(".murmur").join("artifacts"));
    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    // Members resolve from the stores `mur run --capsule` resolves from, in the same order.
    report_roster(
        &project_root,
        &FallbackRegistry {
            primary: project_registry.clone(),
            secondary: global_registry.clone(),
        },
        lock.as_ref(),
        &mut findings,
    );

    // Every installed artifact in either store, declared or not: the global store serves every
    // project on the machine, and an operator upgrading `mur` needs the whole list of what the
    // new host refuses.
    let interface_findings = report_interface_versions(&project_registry, &global_registry);
    report_artifact_stores(&project_registry, &global_registry);

    capsule_runtime::report_println!("Checking {}...", manifest_path.display());
    // Named only where an attestation is checked: a declared artifact that is neither `local/`
    // nor a `source:` path.
    let checks_attestations = runtime_manifest.artifacts.iter().any(|artifact| {
        artifact.source.is_none()
            && murmur_artifact::ArtifactName::parse(&artifact.name)
                .is_ok_and(|name| !name.is_local())
    });
    if let Some(path) = crate::attestation::trusted_root_override().filter(|_| checks_attestations)
    {
        capsule_runtime::report_println!(
            "  attestations checked against the Sigstore trusted root at {} ({})",
            path.display(),
            crate::attestation::TRUSTED_ROOT_ENV
        );
    }

    // Align every check line on the widest "name@version" reference string.
    let col_width = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.name.len() + 1 + artifact.version.len()) // +1 for '@'
        .max()
        .unwrap_or(0);

    let mut total_pass: u32 = 0;
    // `name@version` of every declared artifact the checklist failed for stale interfaces, so
    // the inventory adds no warning-tier `Fix:` for the same artifact.
    let mut interface_failed: HashSet<String> = HashSet::new();
    // Each installed artifact's declared name, `name@version`, and bundled `murmur.yaml`, for the
    // model limits each driver choice resolves to.
    let mut installed_manifests: Vec<(String, String, String)> = Vec::new();
    // The `transport: process` driver the checklist passed, for the `Process harness` block.
    let process_driver_name = runtime_manifest
        .inference
        .as_ref()
        .filter(|inference| inference.transport == "process")
        .and_then(|inference| inference.driver.as_ref())
        .map(|driver| driver.artifact.as_str());
    let mut process_driver: Option<InstalledDriver> = None;
    let mut process_driver_local_source = false;

    for artifact in &runtime_manifest.artifacts {
        let request = ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        };
        let name = &artifact.name;
        let version = &artifact.version;
        let ref_str = format!("{name}@{version}");

        match artifact_presence(&project_registry, &global_registry, &request) {
            // Local-source artifacts are never registry-resolved and never locked, so
            // they are exempt from every lock check — the same exemption `mur run` makes.
            ArtifactPresence::LocalSource => {
                capsule_runtime::report_println!(
                    "  \u{2713}  {ref_str:<col_width$}   local source"
                );
                process_driver_local_source |= process_driver_name == Some(name.as_str());
                total_pass += 1;
            }
            ArtifactPresence::Installed(resolved) => {
                if let Ok(yaml) =
                    murmur_artifact::load_manifest_yaml_from_artifact_bytes(&resolved.bytes)
                {
                    installed_manifests.push((name.clone(), ref_str.clone(), yaml));
                }
                // No lockfile: presence is the whole check, as it has always been.
                let verdict = match &lock {
                    Some(lock) => check_lock_entry(
                        lock,
                        name,
                        version,
                        &resolved.bytes,
                        &artifact.runtime,
                        artifact.gateway.is_some(),
                        runtime_manifest
                            .inference
                            .as_ref()
                            .and_then(|inference| inference.system_prompt_artifact.as_deref())
                            == Some(name.as_str()),
                    ),
                    None => LockVerdict::Ok { pulled_by: None },
                };

                match verdict {
                    LockVerdict::Ok { pulled_by } => {
                        let unserved = resolved_unserved(&resolved);
                        if unserved.is_empty() && process_driver_name == Some(name.as_str()) {
                            process_driver = Some(InstalledDriver {
                                reference: ref_str.clone(),
                                name: name.clone(),
                                version: version.clone(),
                                bytes: resolved.bytes.to_vec(),
                            });
                        }
                        match pulled_by {
                            // The bytes `stage_session` instantiates on the next `mur run`, and the
                            // host resolves one instance name per interface with no fallback, so
                            // a green line here would be one the launch contradicts.
                            _ if !unserved.is_empty() => {
                                let reason = unserved_reason(&unserved);
                                capsule_runtime::report_println!(
                                    "  \u{2717}  {ref_str:<col_width$}   \u{2014} {reason}"
                                );
                                findings.error(
                                    E_WIT_LEVEL_001,
                                    DoctorBlock::Artifacts,
                                    Some(ref_str.clone()),
                                    format!("{ref_str} \u{2014} {reason}"),
                                    format!(
                                        "{name}: pin {} in murmur.yaml, then run mur install",
                                        release_requirement(&unserved)
                                    ),
                                );
                                interface_failed.insert(ref_str.clone());
                            }
                            Some(session) => {
                                capsule_runtime::report_println!(
                                    "  \u{26A0}  {ref_str:<col_width$}   \u{2014} pulled at runtime by session {session}"
                                );
                                findings.warning(
                                    W_REG_004,
                                    DoctorBlock::Artifacts,
                                    Some(ref_str.clone()),
                                    format!(
                                        "{ref_str} \u{2014} pulled at runtime by session {session}"
                                    ),
                                    format!("mur install {ref_str}"),
                                );
                                total_pass += 1;
                            }
                            None => {
                                match artifact_attestation(
                                    name,
                                    version,
                                    &project_registry,
                                    &global_registry,
                                    &resolved,
                                    lock.as_ref(),
                                ) {
                                    None => capsule_runtime::report_println!(
                                        "  \u{2713}  {ref_str:<col_width$}   installed"
                                    ),
                                    Some(AttestationLine::Attested(signer)) => {
                                        capsule_runtime::report_println!(
                                            "  \u{2713}  {ref_str:<col_width$}   installed \u{00B7} attested by {signer}"
                                        );
                                    }
                                    Some(AttestationLine::Warn { code, status, fix }) => {
                                        capsule_runtime::report_println!(
                                            "  \u{26A0}  {ref_str:<col_width$}   installed \u{00B7} {status}"
                                        );
                                        findings.warning(
                                            code,
                                            DoctorBlock::Artifacts,
                                            Some(ref_str.clone()),
                                            format!("{ref_str} \u{2014} {status}"),
                                            fix,
                                        );
                                    }
                                }
                                total_pass += 1;
                            }
                        }
                    }
                    LockVerdict::RuntimeOriginNotDeclarable {
                        session,
                        declared_as,
                    } => {
                        let reason = format!(
                            "murmur.lock pins '{name}' from a runtime pull by session {session}; murmur.yaml declares it with {declared_as}"
                        );
                        capsule_runtime::report_println!(
                            "  \u{2717}  {ref_str:<col_width$}   \u{2014} {reason}"
                        );
                        findings.error(
                            E_RUN_043,
                            DoctorBlock::Artifacts,
                            Some(ref_str.clone()),
                            format!("{ref_str} \u{2014} {reason}"),
                            format!("mur install {ref_str}"),
                        );
                    }
                    LockVerdict::MissingEntry => {
                        let reason = format!("murmur.lock missing artifact entry for '{name}'");
                        capsule_runtime::report_println!(
                            "  \u{2717}  {ref_str:<col_width$}   \u{2014} {reason}"
                        );
                        findings.error(
                            E_RUN_003,
                            DoctorBlock::Artifacts,
                            Some(ref_str.clone()),
                            format!("{ref_str} \u{2014} {reason}"),
                            format!("mur install {ref_str}"),
                        );
                    }
                    LockVerdict::VersionMismatch { pinned } => {
                        let reason = format!(
                            "murmur.lock version mismatch for '{name}': manifest requested {version}, lock pinned {pinned}"
                        );
                        capsule_runtime::report_println!(
                            "  \u{2717}  {ref_str:<col_width$}   \u{2014} {reason}"
                        );
                        findings.error(
                            E_RUN_003,
                            DoctorBlock::Artifacts,
                            Some(ref_str.clone()),
                            format!("{ref_str} \u{2014} {reason}"),
                            format!(
                                "{name}: run mur install, which moves its murmur.lock entry to {version}"
                            ),
                        );
                    }
                    LockVerdict::HashMismatch { expected, actual } => {
                        let reason = format!("artifact integrity check failed for {ref_str}");
                        capsule_runtime::report_println!(
                            "  \u{2717}  {ref_str:<col_width$}   \u{2014} {reason}"
                        );
                        capsule_runtime::report_println!(
                            "        expected sha256 (murmur.lock): {expected}"
                        );
                        capsule_runtime::report_println!(
                            "        actual sha256 (on disk):       {actual}"
                        );
                        findings.error(
                            E_REG_002,
                            DoctorBlock::Artifacts,
                            Some(ref_str.clone()),
                            format!("{ref_str} \u{2014} {reason}"),
                            format!(
                                "{name}: artifact on disk does not match murmur.lock \u{2014} re-publish or delete the lock"
                            ),
                        );
                    }
                }
            }
            // The refusal `mur run` makes before staging, so it is one failure and gets no lock
            // line.
            ArtifactPresence::NativeTool { field } => {
                let refusal = crate::error::native_tool_launch_error(name, &field);
                capsule_runtime::report_println!(
                    "  \u{2717}  {ref_str:<col_width$}   \u{2014} error[{}]: {}",
                    refusal.code,
                    refusal.message
                );
                findings.coded(
                    refusal.code,
                    DoctorBlock::Artifacts,
                    Some(ref_str.clone()),
                    &refusal.message,
                    refusal.hint.unwrap_or_default(),
                );
            }
            // A missing artifact is already one failure; there is nothing on disk to
            // hash, so it never also gets a lock line.
            ArtifactPresence::Missing => {
                capsule_runtime::report_println!(
                    "  \u{2717}  {ref_str:<col_width$}   \u{2014} missing"
                );
                findings.error(
                    E_RUN_008,
                    DoctorBlock::Artifacts,
                    Some(ref_str.clone()),
                    format!("{ref_str} \u{2014} missing"),
                    format!("mur install {ref_str}"),
                );
            }
        }
    }

    report_model_limits(&runtime_manifest, &installed_manifests, &mut findings);
    report_process_harness(
        &runtime_manifest,
        process_driver.as_ref(),
        process_driver_local_source,
        &capability_policy,
        &project_root,
        &mut findings,
    );

    // `W-RUN-010` in the words a launch prints at staging, counted as a warning.
    if let Some(line) = capsule_runtime::resources::v8_address_space_warning(
        &capability_policy.shell_allow,
        capability_policy.declared_resources.memory_bytes,
        declared_floor == murmur_artifact::ContainmentClass::Unconfined,
    ) {
        capsule_runtime::report_eprintln!("[mur doctor] {line}");
        findings.warning(
            murmur_artifact::W_RUN_010,
            DoctorBlock::Resources,
            Some("capabilities.resources.memory_bytes".to_string()),
            without_code(&line, murmur_artifact::W_RUN_010),
            "set capabilities.resources.memory_bytes to 68719476736 (64 GiB) for node, and cap \
             RAM with capabilities.resources.cgroup_memory_bytes",
        );
    }

    // A malformed `programs:` block is reported once, by the declared-needs section below.
    let pinned_tools = crate::commands::pinned_programs::installed_tool_downloads(
        &runtime_manifest.artifacts,
        lock.as_ref(),
        &project_root,
    )
    .unwrap_or_default();
    let pinned = capsule_runtime::pinned_programs::derive_pinned_programs(
        &pinned_tools,
        &capability_policy.shell_allow,
    );
    report_declared_needs(
        &runtime_manifest,
        lock.as_ref(),
        &project_root,
        &capability_policy.shell_allow,
        &pinned,
        &mut findings,
    );
    report_pinned_programs(&pinned_tools, &capability_policy.shell_allow, &mut findings);

    // `mur run` stages only what murmur.yaml declares, so a pin a capsule pulled on its own is
    // reported but neither fails nor warns.
    if let Some(lock) = &lock {
        for entry in &lock.artifacts {
            let LockOrigin::Runtime { session } = &entry.origin else {
                continue;
            };
            if runtime_manifest
                .artifacts
                .iter()
                .any(|artifact| artifact.name == entry.name)
            {
                continue;
            }
            let ref_str = format!("{}@{}", entry.name, entry.resolved_version);
            capsule_runtime::report_println!(
                "  \u{00B7}  {ref_str:<col_width$}   pulled at runtime by session {session} \u{2014} not declared in murmur.yaml, so mur run does not stage it"
            );
        }
    }

    for artifact in &interface_findings.stale {
        let reference = format!("{}@{}", artifact.name, artifact.version);
        if !interface_failed.contains(&reference) {
            findings.warning(
                W_REG_003,
                DoctorBlock::WitLevel,
                Some(reference.clone()),
                format!(
                    "{reference} \u{2014} {}",
                    unserved_reason(&artifact.unserved)
                ),
                stale_install_fix(artifact),
            );
        }
    }

    // Whether a launch here gets its cgroup scope, asked of the host by creating and removing a
    // child cgroup whatever the capsule declares. A failed check only for a capsule that needs a
    // scope — a subprocess route — because only that launch is refused.
    let cgroup_probe = probe_scope(
        true,
        &capability_policy.resources,
        &project_root.join("workdir"),
    );
    capsule_runtime::report_println!();
    if let Some(fix) = report_cgroup_scope(&cgroup_probe.cgroup_scope, can_spawn_subprocess) {
        findings.coded(
            E_RUN_012,
            DoctorBlock::CgroupScope,
            None,
            "no cgroup scope can be created to bound this capsule's subprocesses",
            fix,
        );
    }

    // Where the session's runtime and records directories and its temporary folder lie and what
    // this host's mechanism makes of them, for the floor this capsule declares and the default workdir,
    // `<manifest dir>/workdir`. A `HOME` that cannot hold them is a failed check: no launch here
    // would start.
    capsule_runtime::report_println!();
    match doctor_runtime_dir(&host_probe, declared_floor, &project_root.join("workdir")) {
        Ok(reports) => {
            capsule_runtime::report_print!("{}", render_runtime_dir(&reports.runtime_dir));
            capsule_runtime::report_println!();
            capsule_runtime::report_print!("{}", render_records_dir(&reports.records_dir));
            capsule_runtime::report_println!();
            capsule_runtime::report_print!("{}", render_temp_dir(&reports.temp_dir));
            capsule_runtime::report_println!();
            capsule_runtime::report_print!(
                "{}",
                capsule_runtime::proc_view::render_process_view(&reports.process_view)
            );
            if reports.placement.inside_workdir() {
                capsule_runtime::report_eprintln!(
                    "[mur doctor] {}",
                    runtime_dir_inside_warning(&reports.placement)
                );
            }
        }
        Err(error) => {
            report_home_unusable(&error);
            let refusal = CliError::from(error);
            findings.coded(
                refusal.code,
                DoctorBlock::RuntimeDirectory,
                None,
                &refusal.message,
                HOME_UNUSABLE_HINT,
            );
        }
    }

    // The host folders the global config lends, checked as `mur run` checks them, for the default
    // workdir and the tier this capsule's floor applies here.
    capsule_runtime::report_println!();
    report_host_paths(
        effective_config.as_ref(),
        &project_root.join("workdir"),
        &capability_policy,
        applied_mechanism(&host_probe, declared_floor),
        &mut findings,
    );

    capsule_runtime::report_println!();

    Ok((total_pass, findings))
}

/// The `Host paths` section: each `host_paths` entry of the global config resolved, its access and
/// what `enforcement_tier` does with it, each `W-SEC-035`, and each refusal `mur run` would make
/// as an `E-CFG-004` error in `findings`.
fn report_host_paths(
    config: Option<&crate::config::MurConfig>,
    workdir: &Path,
    policy: &capsule_runtime::CapabilityPolicy,
    enforcement_tier: &str,
    findings: &mut Findings,
) {
    use capsule_runtime::host_paths::{
        check_policy_execute_roots, credential_warning, HostPathMechanism,
    };
    let mechanism = HostPathMechanism::for_enforcement_tier(enforcement_tier);
    capsule_runtime::report_println!("Host paths");
    let mut refuse = |error: CliError| {
        findings.predict(
            DoctorBlock::HostPaths,
            None,
            PredictedRefusal {
                code: error.code,
                message: error.message,
                consequence:
                    "`mur run` will refuse to launch until that entry is fixed or removed \
                          from ~/.murmur/config.yaml."
                        .to_string(),
                fix: "fix or remove that host_paths entry in ~/.murmur/config.yaml".to_string(),
            },
        );
    };
    let host_paths = match crate::config::launch_host_paths(config, workdir) {
        Ok(host_paths) => host_paths,
        Err(error) => {
            capsule_runtime::report_println!("  unavailable — an entry is refused (below)");
            refuse(error);
            return;
        }
    };
    if host_paths.is_empty() {
        capsule_runtime::report_println!("  <none>");
        return;
    }
    for host_path in &host_paths {
        capsule_runtime::report_println!(
            "  {} → {} ({}): {}",
            host_path.declared,
            host_path.path.display(),
            host_path.access,
            mechanism.describe(host_path.access)
        );
    }
    for host_path in &host_paths {
        if let Some(warning) = credential_warning(host_path) {
            capsule_runtime::report_eprintln!("[mur doctor] {warning}");
        }
    }
    let mut policy = policy.clone();
    policy.host_paths = host_paths;
    if let Err(refusal) = check_policy_execute_roots(&policy, mechanism) {
        refuse(crate::error::host_path_refusal_to_cli(&refusal));
    }
}

/// The containment stance `mur run` with no flags would resolve for this capsule, reported, and
/// the floor the rest of the checklist reads: `unconfined` when the global config chooses it,
/// `contained` otherwise. When the global config chooses `unconfined` and `mur` runs as root
/// (`E-RUN-063`) or a floor is declared too (`E-RUN-059`), `mur run` refuses, which is an error in
/// `findings`, and the checklist reads the contained floor a `--containment` launch would run at.
fn doctor_containment_stance(
    config: crate::config::ContainmentConfig,
    manifest: Option<murmur_artifact::ContainmentClass>,
    contained_floor: murmur_artifact::ContainmentClass,
    findings: &mut Findings,
) -> murmur_artifact::ContainmentClass {
    let stance = crate::stance::resolve_stance(crate::stance::StanceInputs {
        global: config.global,
        project: config.project,
        manifest,
        ..crate::stance::StanceInputs::default()
    });
    match stance {
        Ok(crate::stance::Stance::Unconfined(source)) => {
            capsule_runtime::report_println!("{}", unconfined_stance_line(source.describe()));
            murmur_artifact::ContainmentClass::Unconfined
        }
        Ok(crate::stance::Stance::Contained(floor)) => floor,
        Err(error) => {
            let hint = error.hint.unwrap_or_default();
            findings.predict(
                DoctorBlock::Containment,
                Some("capabilities.containment".to_string()),
                PredictedRefusal {
                    code: error.code,
                    message: error.message,
                    consequence: format!("`mur run` will refuse this capsule; {hint}"),
                    fix: hint,
                },
            );
            contained_floor
        }
    }
}

/// The `Containment stance` line for an unconfined launch chosen by `source`.
fn unconfined_stance_line(source: &str) -> String {
    format!(
        "Containment stance: unconfined ({source}) — shell commands, plan shell steps and the \
         process harness run as you, with your HOME, your whole environment, the \
         host filesystem and network, and sudo; no Landlock, seccomp, namespaces or \
         no_new_privs, and resource limits only where capabilities.resources declares them"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `W-RUN-011` names a package only for a need that resolves to nothing; a need that resolves
    /// somewhere no need may reach, or whose program is not on `PATH`, gives its reason instead.
    #[test]
    fn declared_need_warning_names_a_package_only_for_a_need_the_host_lacks() {
        use capsule_runtime::declared_needs::{MissingNeed, NOT_FOUND, PROGRAM_NOT_ON_PATH};
        let need = murmur_artifact::DeclaredNeed {
            form: murmur_artifact::NeedForm::Library("libx.so.1".to_string()),
            packages: std::collections::BTreeMap::from([(
                "debian".to_string(),
                "libx1".to_string(),
            )]),
        };
        let missing = |reason: &str| MissingNeed {
            need: need.clone(),
            reason: reason.to_string(),
        };
        let ids = ["ubuntu".to_string(), "debian".to_string()];
        assert_eq!(
            declared_need_warning("local/t", "cat", &missing(NOT_FOUND), &ids),
            (
                "warning[W-RUN-011]: local/t declares library libx.so.1 for cat, and this host \
                 has none; install libx1 (debian)"
                    .to_string(),
                "install libx1 for library libx.so.1 (cat, local/t)".to_string()
            )
        );
        assert_eq!(
            declared_need_warning("local/t", "cat", &missing(NOT_FOUND), &["arch".to_string()]).0,
            "warning[W-RUN-011]: local/t declares library libx.so.1 for cat, and this host has \
             none; the artifact names no package for arch"
        );
        let moved = "resolved to /home/u/lib/libx.so.1, beneath the home folder /home/u, which \
                     no need reaches";
        for reason in [moved, PROGRAM_NOT_ON_PATH] {
            let (line, fix) = declared_need_warning("local/t", "cat", &missing(reason), &ids);
            assert_eq!(
                line,
                format!(
                    "warning[W-RUN-011]: local/t declares library libx.so.1 for cat, and it is \
                     not granted: {reason}"
                )
            );
            assert!(!line.contains("install"), "{line}");
            assert!(fix.ends_with(reason), "{fix}");
        }
    }

    fn unserved(
        interface: &str,
        direction: ContractDirection,
        artifact_version: Option<&str>,
        served_version: Option<&str>,
    ) -> UnservedInterface {
        UnservedInterface {
            interface: interface.to_string(),
            direction,
            package: interface.split_once('/').unwrap().0.to_string(),
            artifact_version: artifact_version.map(str::to_string),
            served_version: served_version.map(str::to_string),
        }
    }

    /// A namespace creation refused because the kernel has no user namespaces left to give.
    fn kernel_support_missing() -> EgressNamespaceFailure {
        EgressNamespaceFailure {
            blocker: capsule_runtime::EgressNamespaceBlocker::KernelSupportMissing,
            failed_call: "unshare(CLONE_NEWUSER | CLONE_NEWNET)".to_string(),
            errno: Some(libc::ENOSPC),
            userns_grant: UsernsGrant::RestrictionDisabledHostWide,
            attachment: ProfileAttachment::Unconfined,
            profile_gap: ProfileGap::Undetermined,
        }
    }

    /// A capsule with a subprocess route gets the `E-CAP-005` error on a host that cannot create
    /// the namespace, and one with none gets nothing.
    #[test]
    fn a_subprocess_route_on_a_host_without_a_namespace_is_an_e_cap_005_error() {
        let failure = kernel_support_missing();
        let refusal = egress_namespace_refusal(true, Some(&failure)).expect("a refusal");
        let stderr = refusal.stderr_text();
        assert!(
            stderr.starts_with(&format!("[mur doctor] error[{E_CAP_005}]: ")),
            "{stderr}"
        );
        assert!(
            stderr.contains("`mur run` will refuse this capsule"),
            "{stderr}"
        );
        assert!(stderr.contains(&failure.reason()), "{stderr}");
        assert!(
            refusal.fix.contains("diagnostics/#e-cap-005"),
            "{}",
            refusal.fix
        );

        let mut findings = Findings::default();
        findings.predict(DoctorBlock::Host, None, refusal);
        assert_eq!(findings.errors.len(), 1);
        assert!(
            findings.errors[0]
                .headline()
                .starts_with(&format!("error[{E_CAP_005}]: ")),
            "{:?}",
            findings.errors
        );
        assert!(render_tally(0, &findings).fails);

        assert_eq!(egress_namespace_refusal(true, None), None);
        assert_eq!(egress_namespace_refusal(false, Some(&failure)), None);
    }

    /// An `install.tool` grant alone is no subprocess route for the pre-checklist refusal, as it
    /// is none for `mur run`: a pulled tool is a WASM component, which starts a program only
    /// through `murmur:process` and so only one `capabilities.shell.allow` names.
    #[test]
    fn egress_namespace_refusal_ignores_an_install_grant_alone() {
        let policy = |install: &str| {
            capability_policy_from_runtime_manifest(
                &murmur_artifact::RuntimeManifest::from_yaml_str(&format!(
                    "name: c\nversion: 0.1.0\nartifacts: []\ncapabilities:\n  install:\n{install}"
                ))
                .unwrap(),
            )
        };
        let tool = policy("    tool: [\"local/tool-*\"]\n");
        let skill = policy("    skill: [\"local/skill-*\"]\n");
        let failure = kernel_support_missing();
        assert!(
            egress_namespace_refusal(has_subprocess_route_at_staging(&tool), Some(&failure))
                .is_none()
        );
        assert!(
            egress_namespace_refusal(has_subprocess_route_at_staging(&skill), Some(&failure))
                .is_none()
        );
    }

    /// Records the checklist's runtime-pulled warning for `reference`, with `fix` as its `Fix:`.
    fn warn(findings: &mut Findings, fix: &str) {
        findings.warning(
            W_REG_004,
            DoctorBlock::Artifacts,
            Some("local/pulled@1.0.0".to_string()),
            "local/pulled@1.0.0 \u{2014} pulled at runtime by session ses_1",
            fix,
        );
    }

    /// Records the checklist's missing line for `reference`.
    fn missing(findings: &mut Findings, reference: &str) {
        findings.error(
            E_RUN_008,
            DoctorBlock::Artifacts,
            Some(reference.to_string()),
            format!("{reference} \u{2014} missing"),
            format!("mur install {reference}"),
        );
    }

    /// The document carries the tally's numbers and every finding in `Fix:` order, each message
    /// without the `error[<CODE>]: ` prefix the text form heads it with, and keys in the
    /// documented order.
    #[test]
    fn the_json_report_is_the_tally_and_the_findings_in_fix_order() {
        let mut findings = Findings::default();
        warn(&mut findings, "mur install local/pulled@1.0.0");
        findings.coded(
            E_CAP_005,
            DoctorBlock::Host,
            None,
            "creating a network namespace failed\nsecond line",
            "make this host able to create the capsule's network namespace",
        );
        missing(&mut findings, "local/a@1.0.0");
        let report = DoctorReport::from_findings(2, &findings);
        assert_eq!(report.verdict, Verdict::Refused);
        assert_eq!(report.exit_code, 1);
        assert_eq!(
            (report.checks_passed, report.errors, report.warnings),
            (2, 2, 1)
        );
        let fixes: Vec<&str> = report.findings.iter().map(|f| f.fix.as_str()).collect();
        let tally = render_tally(2, &findings).text;
        let tally_fixes: Vec<&str> = tally
            .lines()
            .filter_map(|line| line.strip_prefix("Fix: "))
            .collect();
        assert_eq!(fixes, tally_fixes);
        assert_eq!(
            report.findings[0].message,
            "creating a network namespace failed"
        );
        assert_eq!(report.findings[0].subject, None);
        assert_eq!(report.findings[2].severity, Severity::Warning);

        let rendered = report.render();
        assert!(rendered.ends_with("}\n"), "{rendered}");
        let keys: Vec<&str> = [
            "\"verdict\"",
            "\"exit_code\"",
            "\"checks_passed\"",
            "\"errors\"",
            "\"warnings\"",
            "\"findings\"",
            "\"severity\"",
            "\"code\"",
            "\"block\"",
            "\"subject\"",
            "\"message\"",
            "\"fix\"",
        ]
        .into_iter()
        .collect();
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| rendered.find(key).expect(key))
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{rendered}"
        );
        assert!(rendered.contains("\"block\": \"host\""), "{rendered}");
        assert!(rendered.contains("\"subject\": null"), "{rendered}");
    }

    /// Warnings alone are `ready` and exit `0`.
    #[test]
    fn the_json_report_with_warnings_only_is_ready() {
        let mut findings = Findings::default();
        warn(&mut findings, "mur install local/pulled@1.0.0");
        let report = DoctorReport::from_findings(1, &findings);
        assert_eq!(report.verdict, Verdict::Ready);
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.findings.len(), 1);
    }

    /// An error before the tally is one `refused` finding carrying its code and hint.
    #[test]
    fn the_json_report_for_an_early_error_is_one_refused_finding() {
        let error = CliError::with_hint(E_IO_003, "no murmur.yaml\nmore", "run it in a project");
        let report = DoctorReport::early(&error, DoctorBlock::Project);
        assert_eq!(report.verdict, Verdict::Refused);
        assert_eq!(report.exit_code, 1);
        assert_eq!(
            (report.checks_passed, report.errors, report.warnings),
            (0, 1, 0)
        );
        assert_eq!(report.findings[0].code, E_IO_003);
        assert_eq!(report.findings[0].block, DoctorBlock::Project);
        assert_eq!(report.findings[0].message, "no murmur.yaml");
        assert_eq!(report.findings[0].fix, "run it in a project");
        let bare = DoctorReport::early(&CliError::new(E_IO_003, "x"), DoctorBlock::Roster);
        assert_eq!(bare.findings[0].fix, "");
    }

    /// Every block serializes to its kebab-case name.
    #[test]
    fn doctor_blocks_serialize_kebab_case() {
        for (block, name) in [
            (DoctorBlock::Project, "project"),
            (DoctorBlock::ModelLimits, "model-limits"),
            (DoctorBlock::WitLevel, "wit-level"),
            (DoctorBlock::CgroupScope, "cgroup-scope"),
            (DoctorBlock::RuntimeDirectory, "runtime-directory"),
            (DoctorBlock::HostPaths, "host-paths"),
        ] {
            assert_eq!(serde_json::to_value(block).unwrap(), name);
        }
    }

    /// Nothing found closes with `All checks passed.` and does not fail.
    #[test]
    fn the_tally_with_nothing_found_passes() {
        let tally = render_tally(3, &Findings::default());
        assert_eq!(
            tally,
            Tally {
                text: "All checks passed.\n".to_string(),
                fails: false
            }
        );
    }

    /// Warnings alone keep the tally, blank line and `Fix:` layout, name no first refusal and do
    /// not fail.
    #[test]
    fn the_tally_with_warnings_only_names_no_refusal() {
        let mut findings = Findings::default();
        warn(&mut findings, "mur install local/a@1.0.0");
        let tally = render_tally(0, &findings);
        assert_eq!(
            tally.text,
            "0 checks passed, 0 errors found, 1 warning.\n\nFix: mur install local/a@1.0.0\n"
        );
        assert!(!tally.fails);

        warn(&mut findings, "second");
        let tally = render_tally(1, &findings);
        assert_eq!(
            tally.text,
            "1 check passed, 0 errors found, 2 warnings.\n\n\
             Fix: mur install local/a@1.0.0\nFix: second\n"
        );
        assert!(!tally.fails);
    }

    /// Errors fail the tally, the first one printed is named on the line after it, and each
    /// error's `Fix:` comes before every warning's.
    #[test]
    fn the_tally_names_the_first_refusal_and_lists_errors_before_warnings() {
        let mut findings = Findings::default();
        findings.coded(
            E_CAP_005,
            DoctorBlock::Host,
            None,
            "creating a network namespace failed\nsecond line",
            "make this host able to create the capsule's network namespace",
        );
        warn(&mut findings, "mur install local/pulled@1.0.0");
        missing(&mut findings, "local/a@1.0.0");
        let tally = render_tally(2, &findings);
        assert_eq!(
            tally.text,
            "2 checks passed, 2 errors found, 1 warning.\n\
             First refusal: error[E-CAP-005]: creating a network namespace failed\n\
             \n\
             Fix: make this host able to create the capsule's network namespace\n\
             Fix: mur install local/a@1.0.0\n\
             Fix: mur install local/pulled@1.0.0\n"
        );
        assert!(tally.fails);
    }

    /// One error and no warning keeps the tally's shape: no warning count.
    #[test]
    fn the_tally_with_one_error_has_no_warning_count() {
        let mut findings = Findings::default();
        missing(&mut findings, "local/a@1.0.0");
        let tally = render_tally(0, &findings);
        assert_eq!(
            tally.text,
            "0 checks passed, 1 error found.\n\
             First refusal: local/a@1.0.0 \u{2014} missing\n\
             \n\
             Fix: mur install local/a@1.0.0\n"
        );
        assert!(tally.fails);
    }

    /// The roost refusal is decided on `MURMUR_ROOST_URL` alone, as the launch's registration
    /// decides it; a missing `mur-roost` binary is advice inside a refusal, never one of its own.
    #[test]
    fn the_roost_refusal_follows_the_url_and_not_the_binary() {
        let binary = Path::new("/opt/mur/mur-roost");
        let unreachable = |_: &str| -> Result<(), String> { Err("connection refused".to_string()) };
        let healthy = |_: &str| -> Result<(), String> { Ok(()) };
        let probed = |_: &str| -> Result<(), String> { panic!("an unset URL is never probed") };

        for url in [None, Some(""), Some("  ")] {
            let refusal = roost_daemon_refusal(url, Some(binary), probed).expect("a refusal");
            assert_eq!(refusal.code, E_RUN_019);
            assert!(
                refusal.message.contains("is not set"),
                "{}",
                refusal.message
            );
            assert!(!refusal.stderr_text().contains("not found on PATH"));
            assert!(
                !refusal.fix.contains("install.murmur.rs"),
                "{}",
                refusal.fix
            );
        }

        let refusal = roost_daemon_refusal(None, None, probed).expect("a refusal");
        let stderr = refusal.stderr_text();
        assert!(stderr.starts_with(&format!("[mur doctor] error[{E_RUN_019}]: ")));
        assert!(
            stderr.contains("mur-roost was not found on PATH"),
            "{stderr}"
        );
        assert!(stderr.contains("install.murmur.rs"), "{stderr}");
        assert!(refusal.fix.contains("MURMUR_ROOST_URL"), "{}", refusal.fix);
        assert!(refusal.fix.contains("install.murmur.rs"), "{}", refusal.fix);

        let refusal =
            roost_daemon_refusal(Some("http://127.0.0.1:1"), None, unreachable).expect("a refusal");
        assert!(
            refusal
                .message
                .contains("no daemon answered at http://127.0.0.1:1: connection refused"),
            "{}",
            refusal.message
        );
        assert!(refusal.stderr_text().contains("install.murmur.rs"));

        assert_eq!(
            roost_daemon_refusal(Some("http://127.0.0.1:7700"), None, healthy),
            None
        );
        assert_eq!(
            roost_daemon_refusal(Some("http://127.0.0.1:7700"), Some(binary), healthy),
            None
        );
    }

    /// The `network namespace:` line reports the probe's answer, and names the failed call and
    /// its `errno` when it refused.
    #[test]
    fn the_network_namespace_line_reports_the_probe() {
        assert!(network_namespace_line(true, None).starts_with("created ("));
        assert_eq!(
            network_namespace_line(true, Some(&kernel_support_missing())),
            "refused \u{2014} unshare(CLONE_NEWUSER | CLONE_NEWNET) failed with ENOSPC"
        );
        assert!(network_namespace_line(false, None).starts_with("n/a"));
        assert!(network_namespace_line(false, Some(&kernel_support_missing())).starts_with("n/a"));
    }

    fn reading(
        network_ok: bool,
        mount: MountAttempt,
        sealed_root: MountAttempt,
    ) -> NamespaceReading {
        NamespaceReading {
            network: if network_ok {
                Ok(())
            } else {
                Err(capsule_runtime::NamespaceAttemptError::Step {
                    step: capsule_runtime::CreationStep::Unshare,
                    errno: libc::EPERM,
                })
            },
            mount,
            sealed_root,
            private_proc: MountAttempt::NotAttempted,
            userns_grant: UsernsGrant::Withheld,
            attachment: ProfileAttachment::Unconfined,
            profile_gap: ProfileGap::Undetermined,
        }
    }

    fn refused(step: capsule_runtime::MountStep, errno: i32) -> MountAttempt {
        MountAttempt::Failed(capsule_runtime::MountAttemptError::Step { step, errno })
    }

    /// The `mount namespace:` and `sealed root:` lines report each attempt in four shapes:
    /// created, refused with the call and its `errno`, not attempted, and n/a off Linux.
    #[test]
    fn the_mount_and_sealed_root_lines_report_each_attempt() {
        use capsule_runtime::MountStep;
        let created = reading(true, MountAttempt::Created, MountAttempt::Created);
        assert_eq!(
            mount_namespace_line(true, &created),
            "created (a private mount namespace inside the session's user namespace was created, \
             so /tmp can be mapped)"
        );
        assert!(sealed_root_line(true, &created).starts_with("created ("));

        let refusing = reading(
            true,
            refused(MountStep::MakePrivate, libc::EACCES),
            refused(MountStep::PivotRoot, libc::EPERM),
        );
        assert_eq!(
            mount_namespace_line(true, &refusing),
            "refused \u{2014} making / MS_REC|MS_PRIVATE failed with EACCES"
        );
        assert_eq!(
            sealed_root_line(true, &refusing),
            "refused \u{2014} pivot_root(2) failed with EPERM"
        );

        let no_network = reading(
            false,
            MountAttempt::NotAttempted,
            MountAttempt::NotAttempted,
        );
        for line in [
            mount_namespace_line(true, &no_network),
            sealed_root_line(true, &no_network),
        ] {
            assert_eq!(line, "not attempted (no network namespace to create it in)");
        }

        for reading in [created, refusing, no_network] {
            assert!(mount_namespace_line(false, &reading).starts_with("n/a ("));
            assert!(sealed_root_line(false, &reading).starts_with("n/a ("));
        }
    }

    /// The `private proc:` line reports the private-procfs rehearsal in the same shapes.
    #[test]
    fn the_private_proc_line_reports_the_rehearsal() {
        use capsule_runtime::MountStep;
        let mut created = reading(true, MountAttempt::Created, MountAttempt::Created);
        created.private_proc = MountAttempt::Created;
        assert_eq!(
            private_proc_line(true, &created),
            "created (a process in the session's PID namespace mounted its own procfs over /proc, \
             so commands see only the session's processes)"
        );
        let mut refusing = created.clone();
        refusing.private_proc = refused(MountStep::MountProc, libc::EPERM);
        assert_eq!(
            private_proc_line(true, &refusing),
            "refused \u{2014} mounting proc over /proc failed with EPERM"
        );
        assert!(private_proc_line(false, &created).starts_with("n/a ("));
    }

    /// The install-the-profile advice follows a failed network or sealed-root attempt, never the
    /// grant alone; a refused `/tmp` mapping does not call for it.
    #[test]
    fn the_profile_advice_follows_a_failed_attempt_not_the_reading() {
        use capsule_runtime::MountStep;
        assert!(!an_attempt_failed(&reading(
            true,
            MountAttempt::Created,
            MountAttempt::Created
        )));
        assert!(!an_attempt_failed(&reading(
            true,
            refused(MountStep::MakePrivate, libc::EACCES),
            MountAttempt::Created
        )));
        assert!(an_attempt_failed(&reading(
            true,
            MountAttempt::Created,
            refused(MountStep::Tmpfs, libc::EPERM)
        )));
        assert!(an_attempt_failed(&reading(
            false,
            MountAttempt::NotAttempted,
            MountAttempt::NotAttempted
        )));
    }

    /// The install-the-profile advice is for an unconfined `mur` whose attempt failed under a
    /// withheld grant; a container's profile, a created set of namespaces or another grant never
    /// prints it.
    #[test]
    fn the_profile_advice_is_for_an_unconfined_binary_only() {
        let docker = ProfileAttachment::Other {
            profile: "docker-default".to_string(),
        };
        let withheld = Some(UsernsGrant::Withheld);
        assert!(profile_advice_applies(
            withheld,
            &ProfileAttachment::Unconfined,
            true
        ));
        assert!(!profile_advice_applies(withheld, &docker, true));
        assert!(!profile_advice_applies(
            withheld,
            &ProfileAttachment::Unconfined,
            false
        ));
        assert!(!profile_advice_applies(
            Some(UsernsGrant::RestrictionDisabledHostWide),
            &ProfileAttachment::Unconfined,
            true
        ));
    }

    fn not_loaded_fixture() -> ProfileGap {
        ProfileGap::NotLoaded {
            exe: "/home/u/.local/bin/mur".into(),
            path: capsule_runtime::ProfiledPath::Home,
            file: capsule_runtime::ProfileFile::Installed,
        }
    }

    fn path_not_profiled_fixture(profile_loaded: bool) -> ProfileGap {
        ProfileGap::PathNotProfiled {
            exe: "/space/scratch/mur-0.11.0-dev".into(),
            link: Some("/home/u/.local/bin/mur".into()),
            profile_loaded,
            file: capsule_runtime::ProfileFile::Installed,
        }
    }

    /// A profile that is not loaded and a path no profile names get distinct advice, each with
    /// its own command and not the other's; only the path case says the path is why.
    #[test]
    fn the_profile_advice_tells_a_profile_not_loaded_from_a_path_not_profiled() {
        let load = "`sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`";
        let mv = "`mv -f /space/scratch/mur-0.11.0-dev /home/u/.local/bin/mur`";

        let not_loaded = profile_advice_lines(&not_loaded_fixture(), "0.10.0").join("\n");
        assert!(
            not_loaded
                .starts_with("No mur-sealed AppArmor profile naming this binary's path is loaded"),
            "{not_loaded}"
        );
        assert!(not_loaded.contains(load), "{not_loaded}");
        assert!(!not_loaded.contains("mv -f"), "{not_loaded}");
        assert!(!not_loaded.contains("This path is why"), "{not_loaded}");
        assert!(!not_loaded.contains("LAST RESORT"), "{not_loaded}");

        let linked = profile_advice_lines(&path_not_profiled_fixture(true), "0.10.0");
        let path = linked.join("\n");
        assert!(linked[0].starts_with("This path is why"), "{path}");
        assert!(
            path.contains("/home/u/.local/bin/mur is a symlink to /space/scratch/mur-0.11.0-dev"),
            "{path}"
        );
        assert!(path.contains(mv), "{path}");
        assert!(!path.contains("sudo mv"), "{path}");
        assert!(!path.contains(load), "{path}");
        assert!(!path.contains("is loaded either"), "{path}");
        assert!(
            path.contains("Adding this path to the profile is not the fix"),
            "{path}"
        );
        for attachment in SEALED_APPARMOR_ATTACHMENT_PATHS {
            assert!(linked.contains(&format!("  {attachment}")), "{path}");
        }
        assert_ne!(not_loaded, path);
    }

    /// With no profile loaded either, the path advice ends with the load step.
    #[test]
    fn the_path_advice_adds_the_load_step_only_when_no_profile_is_loaded() {
        let lines = profile_advice_lines(&path_not_profiled_fixture(false), "0.10.0");
        let last = lines.last().expect("advice");
        assert!(
            last.starts_with("No mur-sealed AppArmor profile is loaded either"),
            "{lines:?}"
        );
        assert!(
            last.contains("`sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`"),
            "{lines:?}"
        );
    }

    #[test]
    fn the_profile_advice_for_a_loaded_profile_that_does_not_attach_is_a_reload() {
        let lines = profile_advice_lines(
            &ProfileGap::NotAttached {
                exe: "/usr/local/bin/mur".into(),
                path: capsule_runtime::ProfiledPath::System,
            },
            "0.10.0",
        );
        let text = lines.join("\n");
        assert!(text.contains("out of date"), "{text}");
        assert!(
            text.contains("Reload it: `sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`"),
            "{text}"
        );
        assert!(!text.contains("This path is why"), "{text}");
    }

    /// Where the loaded profiles cannot be read, the advice is the combined block: the attachment
    /// list, the checkout script and the not-the-fix sentence.
    #[test]
    fn the_profile_advice_for_an_undetermined_gap_is_the_combined_block() {
        let lines = profile_advice_lines(&ProfileGap::Undetermined, "0.10.0");
        assert_eq!(
            lines[0],
            "This path is why the grant below is withheld: AppArmor attaches profiles by \
             executable path, and no mur-sealed profile attaches to this one."
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("scripts/install-dev-apparmor.sh")),
            "{lines:?}"
        );
        assert!(
            lines
                .last()
                .is_some_and(|line| line.starts_with("Adding this path to the profile is not")),
            "{lines:?}"
        );
    }

    #[test]
    fn the_memory_bound_names_both_cgroup_files_where_swap_can_be_bounded() {
        use capsule_runtime::SwapControl;
        assert_eq!(
            render_memory_bound(268_435_456, true, &SwapControl::Exposed, Some(1)),
            "  cgroup_memory_bytes: 268435456\n  memory.max 268435456\n  memory.swap.max 0\n"
        );
        let absent = render_memory_bound(268_435_456, true, &SwapControl::Absent, Some(1));
        assert!(absent.contains("  memory.max 268435456\n"), "{absent}");
        assert!(
            absent.contains("swap is not bounded on this host"),
            "{absent}"
        );
        assert!(
            absent.contains("swap is active (/proc/swaps lists 1 swap area)"),
            "{absent}"
        );
        let no_swap = render_memory_bound(268_435_456, true, &SwapControl::Absent, Some(0));
        assert!(no_swap.contains("no swap is active"), "{no_swap}");
        let elsewhere = render_memory_bound(268_435_456, false, &SwapControl::Unknown, None);
        assert!(
            elsewhere.contains("cgroup bounds are Linux-only"),
            "{elsewhere}"
        );
        assert!(!elsewhere.contains("memory.max"), "{elsewhere}");
    }

    #[test]
    fn unserved_line_names_both_versions() {
        let line = render_unserved_line(
            "old-hook@0.1.0",
            14,
            StoreScope::Global,
            &unserved(
                "murmur:hook/lifecycle@0.8.0",
                ContractDirection::Export,
                Some("0.8.0"),
                Some("0.9.0"),
            ),
        );
        assert_eq!(
            line,
            "  \u{26A0}  old-hook@0.1.0   global    exports murmur:hook/lifecycle@0.8.0 \u{2014} this mur serves murmur:hook@0.9.0"
        );
    }

    #[test]
    fn unserved_line_marks_an_unversioned_name() {
        let line = render_unserved_line(
            "bare-tool@1.0.0",
            15,
            StoreScope::Project,
            &unserved(
                "murmur:tool/run",
                ContractDirection::Export,
                None,
                Some("0.1.0"),
            ),
        );
        assert!(
            line.contains("project   exports murmur:tool/run (unversioned) \u{2014} this mur serves murmur:tool@0.1.0"),
            "{line}"
        );
    }

    #[test]
    fn unserved_line_names_a_package_this_mur_does_not_serve() {
        let line = render_unserved_line(
            "odd@0.1.0",
            9,
            StoreScope::Global,
            &unserved(
                "murmur:nonexistent/thing@0.1.0",
                ContractDirection::Import,
                Some("0.1.0"),
                None,
            ),
        );
        assert!(
            line.ends_with(
                "imports murmur:nonexistent/thing@0.1.0 \u{2014} this mur serves no version of murmur:nonexistent"
            ),
            "{line}"
        );
    }

    #[test]
    fn release_requirement_lists_each_package_once_in_first_seen_order() {
        let interfaces = [
            unserved(
                "murmur:driver/process@0.1.0",
                ContractDirection::Export,
                Some("0.1.0"),
                Some("0.4.0"),
            ),
            unserved(
                "murmur:text/chunks@0.1.0",
                ContractDirection::Import,
                Some("0.1.0"),
                None,
            ),
            unserved(
                "murmur:driver/other@0.1.0",
                ContractDirection::Import,
                Some("0.1.0"),
                Some("0.4.0"),
            ),
        ];
        assert_eq!(
            release_requirement(&interfaces),
            "a release built against murmur:driver@0.4.0, without murmur:text"
        );
    }

    #[test]
    fn release_requirement_with_only_unserved_packages_names_what_it_is_built_without() {
        let interfaces = [unserved(
            "murmur:nonexistent/thing@0.1.0",
            ContractDirection::Import,
            Some("0.1.0"),
            None,
        )];
        assert_eq!(
            release_requirement(&interfaces),
            "a release built without murmur:nonexistent"
        );
    }

    #[test]
    fn stale_install_fix_targets_the_store_and_never_the_installed_version() {
        let artifact = StaleArtifact {
            scope: StoreScope::Global,
            name: "old-hook".to_string(),
            version: "0.1.0".to_string(),
            unserved: vec![unserved(
                "murmur:hook/lifecycle@0.8.0",
                ContractDirection::Export,
                Some("0.8.0"),
                Some("0.9.0"),
            )],
        };
        assert_eq!(
            stale_install_fix(&artifact),
            "mur install -g old-hook@<a release built against murmur:hook@0.9.0>"
        );
        let project = StaleArtifact {
            scope: StoreScope::Project,
            ..artifact
        };
        assert_eq!(
            stale_install_fix(&project),
            "mur install old-hook@<a release built against murmur:hook@0.9.0>"
        );
    }
}
