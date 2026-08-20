use capsule_runtime::{
    capability_policy_from_runtime_manifest, check_egress_namespace,
    check_interpreted_entrypoints_reachable, check_staged_runtime_floor,
    detect_egress_namespace_blocker, warn_on_interpreter_runtime_grants,
    warn_on_unreachable_toolchain_helpers, warn_on_workdir_exec, ArtifactRequest,
};
use murmur_artifact::{
    current_platform, effective_containment_floor, load_runtime_manifest, read_lockfile,
    resolve_manifest_path, sha256_hex, LocalRegistry, LockfileError, MurmurLock,
};

use crate::commands::install::find_project_root;
use crate::commands::run::{artifact_presence, ArtifactPresence};
use crate::commands::{lockfile_error_to_cli, runtime_manifest_error_to_cli};
use crate::config::load_effective_mur_config_if_any_exists;
use crate::error::{CliError, E_CAP_004, E_CAP_005, E_CAP_006};

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
                    LockVerdict::Ok => {
                        println!("  \u{2713}  {ref_str:<col_width$}   {platform}");
                        total_pass += 1;
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
