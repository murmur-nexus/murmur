use capsule_runtime::{
    capability_policy_from_runtime_manifest, check_egress_namespace,
    check_interpreted_entrypoints_reachable, check_staged_runtime_floor,
    detect_egress_namespace_blocker, detect_userns_grant, inspect_installed_profile,
    preopen_reports, warn_on_interpreter_runtime_grants, warn_on_unreachable_toolchain_helpers,
    warn_on_userns_restriction_disabled_host_wide, warn_on_workdir_exec, ArtifactRequest,
    InstalledProfileState, SEALED_APPARMOR_PROFILE_PATH, SEALED_APPARMOR_PROFILE_SHA256,
};
use murmur_artifact::{
    current_platform, effective_containment_floor, load_runtime_manifest, native_binary_verdict,
    parse_tool_implementation_from_yaml, read_lockfile, resolve_manifest_path, sha256_hex,
    ArtifactImplementation, ArtifactRuntime, LocalRegistry, LockfileError, MurmurLock,
    NativeBinaryVerdict,
};

use crate::commands::install::find_project_root;
use crate::commands::run::{artifact_presence, ArtifactPresence};
use crate::commands::{lockfile_error_to_cli, runtime_manifest_error_to_cli};
use crate::config::load_effective_mur_config_if_any_exists;
use crate::error::{CliError, E_CAP_002, E_CAP_004, E_CAP_005, E_CAP_006};

/// Whether an installed artifact's payload can run on the host doctor is checking for.
///
/// Separate from [`LockVerdict`] because it asks a different question of the same bytes: the lock
/// checks that the artifact on disk is the one that was pinned, this checks that the machine can
/// execute it. Only a native tool has an answer beyond `Independent`.
enum PlatformVerdict {
    /// Nothing about this artifact's payload is platform-specific.
    Independent,
    /// A native binary identified as this host's platform.
    Matches,
    /// A native binary whose format this check does not recognise.
    Unverified,
    /// A native binary built for another platform.
    Mismatch { binary_platform: String },
}

/// Classify one installed artifact's payload against `platform`, the way `stage_session` does
/// before it writes a native binary into a session workdir.
///
/// An artifact is native when the capsule manifest declares it a tool *and* its own packed
/// `murmur.yaml` says `implementation: native`. Neither `artifacts-index.json` nor
/// `ArtifactMeta.runtime` can stand in: the index tags every non-skill artifact with both
/// platforms, and `ArtifactMeta.runtime` is `Wasm` for anything installed through the normal path.
///
/// Any failure to open the archive or read `bin/<name>` is `Unverified`, never `Mismatch`. The
/// lock and hash checks above already report a corrupt or truncated artifact, and reporting the
/// same read failure a second time as a platform failure would send the operator after the wrong
/// thing.
fn check_artifact_platform(
    name: &str,
    version: &str,
    runtime: &ArtifactRuntime,
    artifact_bytes: &[u8],
    platform: &str,
) -> PlatformVerdict {
    if !matches!(runtime, ArtifactRuntime::Tool) {
        return PlatformVerdict::Independent;
    }

    let Ok(manifest_yaml) =
        capsule_runtime::artifact::extract_manifest_yaml(name, version, artifact_bytes)
    else {
        return PlatformVerdict::Unverified;
    };
    if parse_tool_implementation_from_yaml(&manifest_yaml) != ArtifactImplementation::Native {
        return PlatformVerdict::Independent;
    }

    let Ok(binary) =
        capsule_runtime::artifact::extract_native_binary(name, version, artifact_bytes)
    else {
        return PlatformVerdict::Unverified;
    };

    match native_binary_verdict(&binary, platform) {
        NativeBinaryVerdict::Runnable => PlatformVerdict::Matches,
        NativeBinaryVerdict::Indeterminate => PlatformVerdict::Unverified,
        NativeBinaryVerdict::Mismatch { binary_platform } => PlatformVerdict::Mismatch {
            binary_platform: binary_platform.to_string(),
        },
    }
}

/// The verdict for one installed artifact when `murmur.lock` is present. Mirrors the
/// three ways `mur run` rejects a locked artifact (`stage_session`'s lock enforcement),
/// so a green doctor line means a session would accept the same artifact.
enum LockVerdict {
    /// Lock agrees with the manifest pin and with the bytes on disk.
    Ok,
    /// No entry for this artifact — `mur run` fails with E-RUN-003.
    MissingEntry,
    /// The lock pins a different version than the manifest declares.
    VersionMismatch { pinned: String },
    /// The bytes on disk hash to something other than the pinned `sha256.wasm` —
    /// `mur run` fails with E-REG-002.
    HashMismatch { expected: String, actual: String },
}

/// Check one installed artifact against the lockfile the way `stage_session` does:
/// entry must exist, its `resolved_version` must equal the manifest pin, and its
/// `sha256.wasm` must equal the hash of the bytes that were actually resolved.
///
/// A version mismatch short-circuits the hash comparison — hashing bytes for a version
/// already known to be wrong would report one drifted artifact as two failures.
fn check_lock_entry(
    lock: &MurmurLock,
    name: &str,
    version: &str,
    artifact_bytes: &[u8],
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
    if actual != entry.sha256.wasm {
        return LockVerdict::HashMismatch {
            expected: entry.sha256.wasm.clone(),
            actual,
        };
    }

    LockVerdict::Ok
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
/// `run_doctor`'s exit status is driven by the `fixes` vector alone.
fn report_userns_grant() {
    let grant = detect_userns_grant();
    println!("AppArmor / user namespaces");
    match grant {
        Some(grant) => {
            println!("  userns grant: {}", grant.wire_name());
            println!("    {}", grant.summary());
        }
        None => println!("  userns grant: n/a (AppArmor is a Linux mechanism)"),
    }

    match inspect_installed_profile() {
        InstalledProfileState::Matches => {
            println!("  {SEALED_APPARMOR_PROFILE_PATH}: matches the profile this build ships");
        }
        InstalledProfileState::Drifted { installed_sha256 } => {
            println!(
                "  {SEALED_APPARMOR_PROFILE_PATH}: does NOT match the profile this build ships"
            );
            println!("    installed sha256: {installed_sha256}");
            println!("    shipped sha256:   {SEALED_APPARMOR_PROFILE_SHA256}");
            println!(
                "    This compares file contents only. It does not establish what the kernel has \
                 loaded — a file can be edited without `apparmor_parser -r` ever running. The \
                 `userns grant` line above is what the kernel actually did. Local customisation \
                 belongs in /etc/apparmor.d/local/mur-sealed, which is included inside the shipped \
                 profiles and is not hashed here."
            );
        }
        InstalledProfileState::Absent => {
            println!("  {SEALED_APPARMOR_PROFILE_PATH}: not installed");
            println!(
                "    Expected on a host without AppArmor, and on a checkout build using \
                 scripts/install-dev-apparmor.sh, which writes its own separate file. The `userns \
                 grant` line above is what decides whether sealed containment works here."
            );
        }
        InstalledProfileState::Unreadable { error } => {
            println!("  {SEALED_APPARMOR_PROFILE_PATH}: present but unreadable ({error})");
            println!(
                "    Not the same as absent, and it changes nothing: the `userns grant` line above \
                 is what the kernel actually did."
            );
        }
    }

    println!();

    // Stderr, in the same words `mur run` uses at staging, so the two cannot state it differently.
    warn_on_userns_restriction_disabled_host_wide(grant);
}

/// Prints the filesystem surface every guest-bearing artifact in `runtime_manifest` will be
/// preopened into, one line per artifact, resolved through the same `preopen_reports` that
/// `mur run --explain-scope` and `stage_session` use.
///
/// A report, never a verdict: a whole-workdir preopen is the default for a `runtime: tool` or
/// `runtime: driver` entry that declares no `capabilities.filesystem.scope`, so counting it as a
/// failure would fail essentially every capsule that exists, and it trades away no enforcement
/// property the way `workdir_exec` does. Nothing here reaches `run_doctor`'s `fixes` vector, which
/// is what drives its exit status.
///
/// A scope `mur run` would refuse is reported as a warning rather than a failure, on the
/// `E-CAP-004` precedent above: doctor launches nothing, so it has nothing to refuse, and aborting
/// the checklist over one manifest line would hide every artifact problem behind it.
fn report_preopens(runtime_manifest: &murmur_artifact::RuntimeManifest) {
    println!("Filesystem preopens");
    match preopen_reports(runtime_manifest.artifacts.iter().map(|artifact| {
        (
            artifact.name.as_str(),
            &artifact.runtime,
            artifact.capabilities.as_ref(),
        )
    })) {
        Ok(preopens) if preopens.is_empty() => {
            println!("  <none> (this capsule declares no tool, driver or hook artifact)");
        }
        Ok(preopens) => {
            for preopen in &preopens {
                println!("  - {}", preopen.render());
            }
        }
        Err(error) => {
            println!("  <unresolved>");
            eprintln!(
                "[mur doctor] warning[{E_CAP_002}]: {error}\n  \
                 `mur run` will refuse this capsule until that entry's \
                 capabilities.filesystem.scope names a path inside the workdir."
            );
        }
    }

    println!();
}

/// Check every artifact the current project declares against the stores a session
/// resolves from. The checklist is the manifest — editing `murmur.yaml` changes what
/// is checked, with no change here.
pub(crate) fn run_doctor() -> Result<(), CliError> {
    let project_root = find_project_root().map_err(|mut error| {
        error.hint = Some("run `mur doctor` from inside a project directory".to_string());
        error
    })?;
    let manifest_path = resolve_manifest_path(&project_root);
    let runtime_manifest =
        load_runtime_manifest(&manifest_path).map_err(runtime_manifest_error_to_cli)?;

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

    // Same reasoning for `staged_runtime`, but it is a refusal rather than a posture warning: a
    // capsule declaring one without an effective `sealed` floor is rejected at staging with
    // E-CAP-004, and finding that out from `mur doctor` beats finding it out from a failed run.
    //
    // Doctor sees two of the three floor sources — the manifest and the workspace config. It has
    // no `--containment` flag of its own, and since the effective floor is the *strongest* of the
    // three, a flag could only ever raise what is computed here. So this reports a warning rather
    // than a failure, and says so: the run it is predicting may still be launched with
    // `--containment sealed`.
    //
    // Computed once here rather than inside the `staged_runtime` branch it used to live in,
    // because the two reachability checks below need it for *every* capsule, not only one
    // declaring a grant.
    let declared_floor = effective_containment_floor(
        load_effective_mur_config_if_any_exists()?.and_then(|config| config.containment),
        runtime_manifest
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.containment),
        None,
    );
    let staged_runtime = runtime_manifest
        .capabilities
        .as_ref()
        .and_then(|caps| caps.shell.as_ref())
        .map(|shell| shell.staged_runtime.as_slice())
        .unwrap_or_default();
    if !staged_runtime.is_empty() {
        if let Err(error) = check_staged_runtime_floor(staged_runtime, declared_floor) {
            eprintln!(
                "[mur doctor] warning[{E_CAP_004}]: {error}\n  \
                 `mur run` will refuse this capsule unless the floor is raised — set \
                 `capabilities.containment: sealed` in murmur.yaml, set `containment: sealed` in \
                 .murmur/config.yaml, or pass `--containment sealed`."
            );
        }
    }

    // The two stage-time reachability checks, surfaced here for the same reason as everything
    // above: finding out from `mur doctor` that a `sealed` capsule's `pip` can never import its
    // own package beats finding it out from a `ModuleNotFoundError` several agent turns into a
    // run. Both resolve `shell.allow` against this host's real `PATH` and read real files, but
    // neither creates a session workdir nor contacts a registry, so they belong in doctor's
    // manifest-only prologue rather than in its artifact loop below.
    //
    // The first is an `Err` at `mur run` and a warning here, following the `E-CAP-004` precedent
    // three lines up: doctor never launches, so it has nothing to refuse, and aborting the rest of
    // the checklist over it would hide every artifact problem behind one capability problem.
    let capability_policy = capability_policy_from_runtime_manifest(&runtime_manifest);
    if let Err(error) = check_interpreted_entrypoints_reachable(&capability_policy, declared_floor)
    {
        eprintln!(
            "[mur doctor] warning[{E_CAP_006}]: {error}\n  \
             `mur run` will refuse this capsule at the declared floor — declare the grant above, \
             or lower `capabilities.containment` if this capsule does not need a composed root."
        );
    }
    // The second already prints its own `W-SEC-012` line, in the same words `mur run` uses, so
    // there is nothing to reformat here — the return value is only for callers that want to
    // inspect what was warned about.
    let _ = warn_on_unreachable_toolchain_helpers(&capability_policy, declared_floor);

    // Same reasoning again, for the mechanism that replaced the seccomp connect/sendto
    // interception: a capsule that can spawn a native subprocess needs a network namespace to put
    // it in, and a host that cannot create one refuses the run with E-CAP-005. Unlike the two
    // checks above this is a pure *host* question — no manifest floor is involved and no flag can
    // change the answer — so doctor asks it whenever the capsule declares a subprocess capability
    // at all, which is exactly what `mur run` does.
    //
    // Deliberately narrower than `stage_session`'s own refusal ought to be: it does not count a
    // native-implementation artifact with no `shell.allow`/`spawn.allow` declared, because that
    // would need each declared artifact's own bundled implementation metadata resolved from the
    // registry — doctor's artifact loop below does that, but only per-artifact and after this
    // point, and duplicating it earlier just for this warning is not worth the restructuring. A
    // native-artifact-only capsule on a host that can't build the namespace will not get this
    // warning from `mur doctor`; it will only surface as a run-time failure from `mur run`.
    let capabilities = runtime_manifest.capabilities.as_ref();
    let shell_allows = capabilities
        .and_then(|caps| caps.shell.as_ref())
        .is_some_and(|shell| !shell.allow.is_empty());
    let spawn_allows = capabilities
        .and_then(|caps| caps.spawn.as_ref())
        .is_some_and(|spawn| !spawn.allow.is_empty());
    let can_spawn_subprocess = shell_allows || spawn_allows;
    if let Err(error) =
        check_egress_namespace(can_spawn_subprocess, detect_egress_namespace_blocker())
    {
        eprintln!(
            "[mur doctor] warning[{E_CAP_005}]: {error}\n  \
             `mur run` will refuse this capsule on this host until that is resolved. Nothing in \
             murmur.yaml can change it — the refusal is about this machine, not the manifest."
        );
    }

    // Where this host's permission to create an unprivileged user namespace comes from, and how
    // the installed AppArmor profile compares to the one this build ships. Both are pure host
    // questions with no manifest input, printed for every project — an operator reading `achieved:
    // sealed` needs to know whether that came from the profile murmur ships or from the host's
    // unprivileged-userns hardening being switched off for every binary on the machine.
    report_userns_grant();

    // What each guest will actually be preopened into, printed before the artifact checklist for
    // the same reason the block above is: it is a property of the manifest, not of any one
    // artifact's presence in a store, and it is the question `capabilities.filesystem.scope` at
    // the capsule level does not answer.
    report_preopens(&runtime_manifest);

    // A lockfile is optional. When one is present it is what `mur run` enforces, so
    // doctor checks against it too; when it is absent doctor reports presence only,
    // exactly as before. A lockfile that exists but cannot be read is a hard failure
    // before any checklist line prints — same as a malformed murmur.yaml.
    let lock = match read_lockfile(&project_root.join("murmur.lock")) {
        Ok(lock) => Some(lock),
        Err(LockfileError::NotFound(_)) => None,
        Err(error) => return Err(lockfile_error_to_cli(error)),
    };

    let project_registry = LocalRegistry::new(project_root.join(".murmur").join("artifacts"));
    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let platform = current_platform();

    println!("Checking {} for {platform}...", manifest_path.display());

    // Align every check line on the widest "name@version" reference string.
    let col_width = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.name.len() + 1 + artifact.version.len()) // +1 for '@'
        .max()
        .unwrap_or(0);

    let mut total_pass: u32 = 0;
    let mut fixes: Vec<String> = Vec::new();

    for artifact in &runtime_manifest.artifacts {
        let request = ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            capabilities: artifact.capabilities.clone(),
        };
        let name = &artifact.name;
        let version = &artifact.version;
        let ref_str = format!("{name}@{version}");

        match artifact_presence(&project_registry, &global_registry, &request, platform) {
            // Local-source artifacts are never registry-resolved and never locked, so
            // they are exempt from every lock check — the same exemption `mur run` makes.
            ArtifactPresence::LocalSource => {
                println!("  \u{2713}  {ref_str:<col_width$}   local source");
                total_pass += 1;
            }
            ArtifactPresence::Installed(resolved) => {
                // No lockfile: presence is the whole check, as it has always been.
                let verdict = match &lock {
                    Some(lock) => check_lock_entry(lock, name, version, &resolved.bytes),
                    None => LockVerdict::Ok,
                };

                match verdict {
                    // A green line has to say what it actually verified. Before this check
                    // every passing artifact printed the host platform, including artifacts
                    // whose payload doctor had never opened — an attestation it could not make.
                    LockVerdict::Ok => {
                        match check_artifact_platform(
                            name,
                            version,
                            &artifact.runtime,
                            &resolved.bytes,
                            platform,
                        ) {
                            PlatformVerdict::Independent => {
                                println!(
                                    "  \u{2713}  {ref_str:<col_width$}   platform-independent"
                                );
                                total_pass += 1;
                            }
                            PlatformVerdict::Matches => {
                                println!("  \u{2713}  {ref_str:<col_width$}   {platform}");
                                total_pass += 1;
                            }
                            PlatformVerdict::Unverified => {
                                println!("  \u{2713}  {ref_str:<col_width$}   platform unverified");
                                total_pass += 1;
                            }
                            PlatformVerdict::Mismatch { binary_platform } => {
                                println!(
                                    "  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} native binary is built for {binary_platform}, this host is {platform}"
                                );
                                fixes.push(format!(
                                    "{name}: native binary is built for {binary_platform} \u{2014} reinstall {ref_str} on this host"
                                ));
                            }
                        }
                    }
                    LockVerdict::MissingEntry => {
                        println!(
                            "  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} murmur.lock missing artifact entry for '{name}'"
                        );
                        fixes.push(format!("mur install {ref_str}"));
                    }
                    LockVerdict::VersionMismatch { pinned } => {
                        println!(
                            "  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} murmur.lock version mismatch for '{name}': manifest requested {version}, lock pinned {pinned}"
                        );
                        fixes.push(format!(
                            "{name}: remove the stale murmur.lock entry, then run mur install {ref_str}"
                        ));
                    }
                    LockVerdict::HashMismatch { expected, actual } => {
                        println!(
                            "  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} artifact integrity check failed for {ref_str}"
                        );
                        println!("        expected sha256 (murmur.lock): {expected}");
                        println!("        actual sha256 (on disk):       {actual}");
                        fixes.push(format!(
                            "{name}: artifact on disk does not match murmur.lock \u{2014} re-publish or delete the lock"
                        ));
                    }
                }
            }
            // A missing artifact is already one failure; there is nothing on disk to
            // hash, so it never also gets a lock line.
            ArtifactPresence::Missing => {
                println!("  \u{2717}  {ref_str:<col_width$}   {platform}   \u{2014} missing");
                fixes.push(format!("mur install {ref_str}"));
            }
        }
    }

    println!();

    if fixes.is_empty() {
        println!("All checks passed.");
        return Ok(());
    }

    let total_fail = fixes.len();
    let ps = if total_pass == 1 { "" } else { "s" };
    let es = if total_fail == 1 { "" } else { "s" };
    println!("{total_pass} check{ps} passed, {total_fail} error{es} found.");
    println!();

    for fix in &fixes {
        println!("Fix: {fix}");
    }

    // Exit non-zero so `mur doctor` can be used in CI pre-flight checks.
    // std::process::exit terminates the process immediately; no destructors run,
    // which is acceptable here because we are done with all I/O.
    std::process::exit(1);
}
