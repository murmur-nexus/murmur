use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use capsule_runtime::precompile::{PrecompileItem, Precompiler};
use capsule_runtime::served_wit_level;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use murmur_artifact::{
    is_reserved_version, load_manifest_from_artifact_bytes, load_runtime_manifest, read_lockfile,
    resolve_manifest_path, sha256_hex, verify_sha256, version_fit, write_lockfile_atomic,
    ArtifactMeta, ArtifactName, ArtifactRef, ArtifactRefError, FitSubject, LocalRegistry,
    LockOrigin, LockedSha256, LockfileError, Manifest, MurmurLock, Registry, RegistryError,
    ResolvedArtifact, VersionFit, LOCAL_SCOPE, LOCK_VERSION, MANIFEST_FILENAME, MURMUR_SCOPE,
    W_REG_005, W_REG_006,
};
use rayon::prelude::*;

use crate::{
    attestation::{self, AttestationState, Expected},
    error::{
        artifact_name_arg, artifact_name_error, local_store_miss, native_payload_refusal,
        native_tool_install_error, CliError, E_IO_001, E_IO_003, E_REG_004, E_REG_005, E_REG_006,
    },
    source::{
        check_owner, check_stored_owner, github::GitHubSource, github_scope, newest_fitting,
        resolve, select_version, unanswered_hint, ArtifactSource, ExplicitVersion, Resolved,
        SourceError, Sources, MURMUR_OWNER_ID,
    },
    wit_level::{index_level_mismatch, level_mismatch},
};

use super::runtime_manifest_error_to_cli;

/// What an install holds a resolved payload to against its `murmur.lock` entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockCheck {
    /// The pinned version and, for that version, the pinned bytes: a named install from the
    /// global store, which takes no version the lock does not already pin.
    VersionAndPin,
    /// The pinned bytes of the pinned version only. A different version is a new pin, so the
    /// entry follows `murmur.yaml` on a manifest install and follows the reference on a named
    /// install from a source.
    PinOnly,
    /// `mur install --frozen`: the pinned version and bytes, since the lock is never written.
    Frozen,
}

/// The hint for a payload whose bytes disagree with the `murmur.lock` pin of the same version.
const CHANGED_BYTES_HINT: &str = "the bytes served for this version changed since murmur.lock \
                                  was written; to accept the new bytes, remove this entry from \
                                  murmur.lock and install again";

/// The hint for every `--frozen` refusal: nothing is pinned or re-pinned under it.
fn frozen_hint(what: &str) -> String {
    format!("--frozen does not change murmur.lock; run `mur install` without --frozen to {what}")
}

/// Compare a resolved `name@version` with pin `sha256` against its entry in `lock`, refusing
/// with `E-REG-005` when `check` says they disagree. No lock, or no entry for `name`, is not a
/// conflict: the artifact simply isn't pinned yet. `--frozen` refuses a missing entry before
/// anything is resolved (see [`check_frozen_lock`]).
fn check_lock_entry(
    lock: Option<&MurmurLock>,
    name: &str,
    version: &str,
    sha256: &LockedSha256,
    check: LockCheck,
) -> Result<(), CliError> {
    let Some(entry) = lock.and_then(|lock| lock.artifact_for(name)) else {
        return Ok(());
    };
    let conflict = |message: String, hint: String| {
        Err(CliError::with_hint(
            E_REG_005,
            format!("murmur.lock conflict for '{name}': {message}"),
            hint,
        ))
    };
    if let Some(message) = entry.same_version_conflict_with(version, sha256) {
        return conflict(message, CHANGED_BYTES_HINT.to_string());
    }
    if check != LockCheck::PinOnly {
        if let Some(message) = entry.conflict_with(version, sha256) {
            let hint = if check == LockCheck::Frozen {
                frozen_hint(&format!("take {version}"))
            } else {
                format!(
                    "to move {name} to {version}, change its version in murmur.yaml and run \
                     `mur install`"
                )
            };
            return conflict(message, hint);
        }
    }
    Ok(())
}

/// [`check_lock_entry`] against the `murmur.lock` at `lock_path`, read now.
fn check_lock_conflict(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: &LockedSha256,
    check: LockCheck,
) -> Result<(), CliError> {
    let lock = read_lock_if_any(lock_path)?;
    check_lock_entry(lock.as_ref(), name, version, sha256, check)
}

/// `E-REG-005` for the whole `mur install --frozen` before anything is fetched or written: there
/// is no `murmur.lock`, or a declared artifact has no entry in it, or its `murmur.yaml` version is
/// not the one its entry pins.
fn check_frozen_lock(
    lock: Option<&MurmurLock>,
    lock_path: &Path,
    names: &[ArtifactName],
    artifacts: &[&murmur_artifact::RuntimeArtifact],
) -> Result<(), CliError> {
    let Some(lock) = lock else {
        return Err(CliError::with_hint(
            E_REG_005,
            format!(
                "--frozen installs what murmur.lock pins, but there is no murmur.lock at {}",
                lock_path.display()
            ),
            frozen_hint("write it"),
        ));
    };
    for (name, artifact) in names.iter().zip(artifacts) {
        let Some(entry) = lock.artifact_for(name.as_str()) else {
            return Err(CliError::with_hint(
                E_REG_005,
                format!(
                    "murmur.lock has no entry for '{name}', which murmur.yaml declares at {}",
                    artifact.version
                ),
                frozen_hint(&format!("pin {name}@{}", artifact.version)),
            ));
        };
        if entry.resolved_version != artifact.version {
            return Err(CliError::with_hint(
                E_REG_005,
                format!(
                    "murmur.lock conflict for '{name}': pinned {name}@{}, but {name}@{} was \
                     resolved",
                    entry.resolved_version, artifact.version
                ),
                frozen_hint(&format!("take {}", artifact.version)),
            ));
        }
    }
    Ok(())
}

/// A store write or read of `name@version` refused, as the CLI error naming it: the native-tool
/// refusal is `E-REG-012` with the version in hand, every other error maps as usual.
fn store_error(error: RegistryError, name: &str, version: &str) -> CliError {
    match error {
        RegistryError::NativeTool { field, .. } => native_tool_install_error(name, version, &field),
        error => CliError::from(error),
    }
}

/// The project's `murmur.lock`, or `None` when there is none yet.
fn read_lock_if_any(lock_path: &Path) -> Result<Option<MurmurLock>, CliError> {
    match read_lockfile(lock_path) {
        Ok(lock) => Ok(Some(lock)),
        Err(LockfileError::NotFound(_)) => Ok(None),
        Err(err) => Err(super::lockfile_error_to_cli(err)),
    }
}

/// Upsert a single artifact's entry into `murmur.lock` at `lock_path` as operator-declared,
/// creating the lockfile if it doesn't exist yet and preserving every other pre-existing entry.
/// `owner_id`, the GitHub owner id of the source that served the payload, is recorded on the
/// entry when known; an entry keeps the id it has otherwise. `signer` replaces the entry's
/// `signer` either way: it describes the bytes now pinned, so `None` clears an earlier one.
///
/// Returns the line announcing the adoption when the entry was a `manage.pull()` pin, for the
/// caller to print once nothing else is drawing on the terminal.
fn upsert_lock_entry(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: LockedSha256,
    owner_id: Option<u64>,
    signer: Option<&str>,
) -> Result<Option<String>, CliError> {
    let mut lock = read_lock_if_any(lock_path)?.unwrap_or_else(|| MurmurLock {
        lock_version: LOCK_VERSION,
        artifacts: Vec::new(),
    });

    let previous = lock.upsert(name, version, sha256, LockOrigin::Operator);
    if let Some(owner_id) = owner_id {
        lock.set_owner_id(name, owner_id);
    }
    lock.set_signer(name, signer.map(str::to_string));

    write_lockfile_atomic(lock_path, &lock).map_err(super::lockfile_error_to_cli)?;
    Ok(match previous {
        Some(LockOrigin::Runtime { session }) => Some(format!(
            "Adopted {name}@{version} as operator-declared (it was pulled at runtime by session \
             {session})"
        )),
        Some(LockOrigin::Operator) | None => None,
    })
}

/// Print each line announcing a `murmur.lock` change — an `Updated` line or an adoption line
/// [`upsert_lock_entry`] returned — once the progress bars are finished.
fn print_lock_changes(lines: &[String]) {
    for line in lines {
        capsule_runtime::report_println!("{line}");
    }
}

// ─── attestation ─────────────────────────────────────────────────────────────

/// The owner id the attestation of a stored `name` is held to: [`MURMUR_OWNER_ID`] for the
/// `murmur` scope, else the owner id the stored payload records, else the one its `murmur.lock`
/// entry records.
pub(crate) fn stored_owner_expectation(
    name: &ArtifactName,
    stored_owner: Option<u64>,
    lock: Option<&MurmurLock>,
) -> Option<u64> {
    if name.scope() == MURMUR_SCOPE {
        return Some(MURMUR_OWNER_ID);
    }
    stored_owner.or_else(|| {
        lock.and_then(|lock| lock.artifact_for(name.as_str()))
            .and_then(|entry| entry.owner_id)
    })
}

/// Verify, offline, the attestation `store` (named `store_label` in messages) keeps beside
/// `name@version` against the stored payload's `sha256`, with no repository to compare. Returns the
/// state and the bundle to keep, `Some` only when it verified; a bundle that does not verify is
/// `E-REG-016`. A `local/` artifact is never checked.
pub(crate) fn check_stored_attestation(
    store: &LocalRegistry,
    store_label: &str,
    name: &ArtifactName,
    version: &str,
    sha256: &str,
    stored_owner: Option<u64>,
    lock: Option<&MurmurLock>,
) -> Result<(AttestationState, Option<bytes::Bytes>), CliError> {
    if name.is_local() {
        return Ok((AttestationState::Absent, None));
    }
    let Some(bundle) = store
        .read_attestation(name, version)
        .map_err(|error| store_error(error, name.as_str(), version))?
    else {
        return Ok((AttestationState::Absent, None));
    };
    let root = attestation::trusted_root()?;
    let expected = Expected {
        sha256,
        owner_id: stored_owner_expectation(name, stored_owner, lock),
        repo: None,
    };
    match attestation::assess(Some(&bundle), &expected, root) {
        Ok(state) => {
            let keep = matches!(state, AttestationState::Verified { .. }).then_some(bundle);
            Ok((state, keep))
        }
        Err(reason) => Err(attestation::refusal(
            &format!("{name}@{version}"),
            store_label,
            &reason,
        )),
    }
}

/// The `warning[W-REG-005]` and `warning[W-REG-006]` lines, each followed by its link line, for
/// an install of `name@version` from `origin` whose attestation is `state`, given the `signer`
/// its `murmur.lock` entry recorded. None for a `local/` artifact.
pub(crate) fn attestation_warnings(
    name: &ArtifactName,
    version: &str,
    origin: &str,
    state: &AttestationState,
    recorded: Option<&str>,
) -> Vec<String> {
    if name.is_local() {
        return Vec::new();
    }
    let target = format!("{name}@{version}");
    let mut lines = Vec::new();
    let mut warn = |code: &str, text: String| {
        lines.push(format!("warning[{code}]: {text}"));
        lines.push(format!(
            "  \u{2192} {}",
            murmur_artifact::registry_warning_link(code)
        ));
    };
    match state {
        AttestationState::Absent => warn(
            W_REG_005,
            format!(
                "{target} from {origin} has no attestation; installed without proof of who \
                 published it"
            ),
        ),
        AttestationState::Unverifiable(reason) => warn(
            W_REG_005,
            format!(
                "{target} from {origin} has an attestation in a form this mur does not verify \
                 ({reason}); installed without proof of who published it"
            ),
        ),
        AttestationState::Verified { .. } => {}
    }
    match (state, recorded) {
        (AttestationState::Verified { signer }, Some(recorded)) if signer != recorded => warn(
            W_REG_006,
            format!("{target} is attested by {signer}; murmur.lock recorded {recorded}"),
        ),
        (AttestationState::Absent | AttestationState::Unverifiable(_), Some(recorded)) => warn(
            W_REG_006,
            format!(
                "{target} has no attestation; murmur.lock recorded it as attested by {recorded}"
            ),
        ),
        _ => {}
    }
    lines
}

/// Print warning lines to stderr.
pub(crate) fn print_warnings(lines: &[String]) {
    for line in lines {
        capsule_runtime::report_eprintln!("{line}");
    }
}

/// `, attested by <signer>` for a verified attestation, else nothing: the tail of an `Installed`
/// line.
fn attested_suffix(state: &AttestationState) -> String {
    state
        .signer()
        .map(|signer| format!(", attested by {signer}"))
        .unwrap_or_default()
}

/// The `signer` `lock_path`'s entry for `name` records, read now.
fn recorded_signer(lock_path: Option<&Path>, name: &str) -> Result<Option<String>, CliError> {
    let Some(lock_path) = lock_path else {
        return Ok(None);
    };
    Ok(read_lock_if_any(lock_path)?.and_then(|lock| {
        lock.artifact_for(name)
            .and_then(|entry| entry.signer.clone())
    }))
}

// ─── source helpers ──────────────────────────────────────────────────────────

/// The [`ArtifactMeta`] of a payload a source served as `name@version`, recording the source's
/// `owner_id`.
pub(crate) fn source_meta(
    name: &ArtifactName,
    version: &str,
    manifest: &Manifest,
    owner_id: u64,
) -> ArtifactMeta {
    ArtifactMeta {
        name: name.clone(),
        version: version.to_string(),
        runtime: manifest.registry_runtime(),
        artifact_runtime: manifest.runtime.clone(),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: Some(owner_id),
        built_at_level: None,
    }
}

/// Store a payload `name`'s source served and checked, and pin it in `lock_path` when given.
fn install_resolved(
    local_registry: &LocalRegistry,
    name: &ArtifactName,
    resolved: Resolved,
    lock_path: Option<&Path>,
) -> Result<Stored, CliError> {
    let meta = source_meta(
        name,
        &resolved.version,
        &resolved.manifest,
        resolved.owner_id,
    );
    let recorded = store_and_pin(
        local_registry,
        meta,
        &resolved.bytes,
        &resolved.sha256,
        lock_path,
        &resolved.attestation,
        resolved.bundle.as_deref(),
    )?;
    capsule_runtime::report_println!(
        "Installed {name}@{} from {}{}",
        resolved.version,
        resolved.source,
        attested_suffix(&resolved.attestation)
    );
    print_warnings(&attestation_warnings(
        name,
        &resolved.version,
        &resolved.source,
        &resolved.attestation,
        recorded.as_deref(),
    ));
    Ok(Stored {
        name: name.to_string(),
        version: resolved.version,
        payload: resolved.bytes,
    })
}

/// Write `meta`'s payload to `local_registry`, with `bundle` beside it when its attestation
/// verified, and, when `lock_path` is given, pin it there with the owner id `meta` records and the
/// signer `attestation` names. A payload of the version `lock_path` pins whose bytes disagree with
/// the pin is refused with `E-REG-005` before anything is stored; another version re-pins.
///
/// Returns the `signer` the lock entry recorded before this pin, for the signer-change warning.
fn store_and_pin(
    local_registry: &LocalRegistry,
    meta: ArtifactMeta,
    bytes: &[u8],
    sha256: &str,
    lock_path: Option<&Path>,
    attestation: &AttestationState,
    bundle: Option<&[u8]>,
) -> Result<Option<String>, CliError> {
    let pin = LockedSha256::any(sha256);
    let name = meta.name.to_string();
    let version = meta.version.clone();
    let owner_id = meta.owner_id;
    let recorded = recorded_signer(lock_path, &name)?;
    if let Some(lock_path) = lock_path {
        check_lock_conflict(lock_path, &name, &version, &pin, LockCheck::PinOnly)?;
    }
    local_registry
        .store_installed_attested(meta, bytes, sha256, bundle)
        .map_err(|error| store_error(error, &name, &version))?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) = upsert_lock_entry(
            lock_path,
            &name,
            &version,
            pin,
            owner_id,
            attestation.signer(),
        )? {
            capsule_runtime::report_println!("{adoption}");
        }
    }
    Ok(recorded)
}

/// `mur install github:<owner>/<repo>@<tag>`: every `.mur.zip` of that release, each filed under
/// its packed name in the owner's scope. The owner id is checked against `lock` and, for the
/// `murmur` scope, the compiled-in one before any payload is downloaded.
///
/// An asset that fails does not stop the ones after it. Each payload stored is pushed onto
/// `stored`, so the stored ones are there to compile when this returns an error; once every asset
/// was tried, the report names each failed asset and the error counts them.
fn install_github_release(
    owner: &str,
    repo: &str,
    tag: &str,
    store: &LocalRegistry,
    lock_path: Option<&Path>,
    stored: &mut Vec<Stored>,
) -> Result<(), CliError> {
    let scope = github_scope(owner, repo)?;
    let token = Sources::load()?.token_for(owner, repo);
    let source = GitHubSource::new(owner, repo, &scope, token);
    let label = source.label().to_string();
    let lock = match lock_path {
        Some(lock_path) => read_lock_if_any(lock_path)?,
        None => None,
    };
    let unanswered = |error: SourceError| match error {
        SourceError::RateLimited { .. } => CliError::with_hint(
            E_REG_006,
            format!("could not look up {label}: {}", error.reason()),
            unanswered_hint(&error),
        ),
        error => CliError::new(E_IO_003, format!("{label}: {}", error.reason())),
    };

    let owner_id = source.owner_id().map_err(unanswered)?;
    check_owner(
        &source,
        owner_id,
        lock.as_ref(),
        &format!("github:{owner}/{repo}@{tag}"),
    )
    .map_err(CliError::from)?;
    let payloads = source.release_payloads(tag).map_err(unanswered)?;
    let total = payloads.len();
    let mut failures = Vec::new();
    for payload in payloads {
        let install = || -> Result<Stored, CliError> {
            let manifest = load_manifest_from_artifact_bytes(&payload.bytes).map_err(|error| {
                native_payload_refusal(&error, &scope, &payload.bytes).unwrap_or_else(|| {
                    CliError::new(
                        E_IO_003,
                        format!(
                            "{} from {label} is not a valid .mur.zip artifact: {error}",
                            payload.file
                        ),
                    )
                })
            })?;
            let name = ArtifactName::from_parts(&scope, &manifest.name).map_err(|error| {
                CliError::new(
                    E_IO_003,
                    format!(
                        "an asset from {label} cannot be stored in the '{scope}' scope: {error}"
                    ),
                )
            })?;
            let version = manifest.version.clone();
            let sha256 = sha256_hex(&payload.bytes);
            let bundle_file = format!("{}{}", payload.file, attestation::BUNDLE_SUFFIX);
            let bundle = match source.fetch(tag, &bundle_file) {
                Ok(bundle) => Some(bundle),
                Err(SourceError::NotFound(_)) => None,
                Err(error) => return Err(unanswered(error)),
            };
            let state = match &bundle {
                None => AttestationState::Absent,
                Some(bundle) => {
                    let expected = Expected {
                        sha256: &sha256,
                        owner_id: Some(owner_id),
                        repo: Some(source.repo()),
                    };
                    attestation::assess(Some(bundle), &expected, attestation::trusted_root()?)
                        .map_err(|reason| {
                            attestation::refusal(&format!("{name}@{version}"), &label, &reason)
                        })?
                }
            };
            let keep = bundle.filter(|_| matches!(state, AttestationState::Verified { .. }));
            let meta = source_meta(&name, &version, &manifest, owner_id);
            let recorded = store_and_pin(
                store,
                meta,
                &payload.bytes,
                &sha256,
                lock_path,
                &state,
                keep.as_deref(),
            )?;
            capsule_runtime::report_println!(
                "Installed {name}@{version} from {label}{}",
                attested_suffix(&state)
            );
            print_warnings(&attestation_warnings(
                &name,
                &version,
                &label,
                &state,
                recorded.as_deref(),
            ));
            Ok(Stored {
                name: name.to_string(),
                version,
                payload: payload.bytes.clone(),
            })
        };
        match install() {
            Ok(installed) => stored.push(installed),
            Err(error) => failures.push((payload.file, error)),
        }
    }
    if failures.is_empty() {
        return Ok(());
    }

    capsule_runtime::report_println!();
    capsule_runtime::report_println!(
        "{} of {total} assets of github:{owner}/{repo}@{tag} failed to install:",
        failures.len()
    );
    for (file, error) in &failures {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("  {file}");
        for line in error.to_string().lines() {
            capsule_runtime::report_println!("    {line}");
        }
    }
    let (_, first) = &failures[0];
    Err(CliError::new(
        first.code,
        format!(
            "{} of {total} assets of github:{owner}/{repo}@{tag} failed to install",
            failures.len()
        ),
    ))
}

/// The one [`Precompiler`] an install command stores forms with, or `None` under
/// `--no-precompile` or when no engine can be built. `project_root` is the project whose store is
/// being installed into: its capsules' writable roots default to `<project_root>/workdir`, the
/// directory `stage_session` checks a form against, so forms are withheld from a murmur home that
/// directory could reach. `None` for the global store, which no one capsule stages from.
fn build_precompiler(no_precompile: bool, project_root: Option<&Path>) -> Option<Precompiler> {
    if no_precompile {
        return None;
    }
    let capsule_writable = project_root.map(|root| root.join("workdir"));
    Precompiler::new(capsule_writable.as_deref())
}

/// A payload an install stored as `name@version`, held until the command compiles everything it
/// stored as one batch.
pub(crate) struct Stored {
    name: String,
    version: String,
    payload: bytes::Bytes,
}

impl Stored {
    fn item(&self) -> PrecompileItem<'_> {
        PrecompileItem {
            name: &self.name,
            version: &self.version,
            payload: &self.payload,
        }
    }
}

/// Stores the compiled forms of every payload in `stored` as one batch, with the precompiler
/// [`build_precompiler`] gives for `no_precompile` and `project_root`. The outcomes are
/// deliberately dropped: a form that is not stored only means the first launch compiles, so it
/// never changes what install prints or how it exits. Builds no engine when `stored` is empty.
fn compile_stored<'a>(
    stored: impl IntoIterator<Item = PrecompileItem<'a>>,
    no_precompile: bool,
    project_root: Option<&Path>,
) {
    let items: Vec<PrecompileItem<'_>> = stored.into_iter().collect();
    if items.is_empty() {
        return;
    }
    if let Some(precompiler) = build_precompiler(no_precompile, project_root) {
        let _ = precompiler.precompile_all(&items);
    }
}

/// Fetches the program downloads each payload in `stored` pins for this host
/// ([`super::pinned_programs::fetch_pinned_downloads`]), after it is stored and pinned, printing
/// what each did. Returns each failure with the `name@version` it belongs to.
fn fetch_pinned_for(stored: &[Stored]) -> Vec<(String, CliError)> {
    let mut failures = Vec::new();
    for item in stored {
        let outcome = super::pinned_programs::fetch_pinned_downloads(
            &item.name,
            &item.version,
            &item.payload,
        );
        for line in &outcome.lines {
            capsule_runtime::report_println!("{line}");
        }
        if let Some(failure) = outcome.failure {
            failures.push((format!("{}@{}", item.name, item.version), failure));
        }
    }
    failures
}

/// Prints the pinned downloads that failed, each under the artifact it belongs to.
fn report_pinned_failures(failures: &[(String, CliError)]) {
    capsule_runtime::report_println!();
    capsule_runtime::report_println!(
        "{} pinned download{} failed:",
        failures.len(),
        if failures.len() == 1 { "" } else { "s" }
    );
    for (artifact, error) in failures {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("  {artifact}");
        for line in error.to_string().lines() {
            capsule_runtime::report_println!("    {line}");
        }
    }
}

/// Keeps the host facts of the `capabilities.shell.allow` binaries of the manifest at
/// `project_root`, as `mur run` without `--workdir` would derive them, so the project's first
/// launch keeps them instead of running each interpreter. Prints one line when at least one is
/// kept. Never fails and never writes to standard error: a manifest that does not load, an
/// unusable home or cache, or a binary that cannot be examined only means fewer entries, which
/// that launch works out itself. Nothing runs under `--no-precompile` or without a project.
fn keep_host_facts(no_precompile: bool, project_root: Option<&Path>) {
    let Some(root) = project_root.filter(|_| !no_precompile) else {
        return;
    };
    let Ok(manifest) = load_runtime_manifest(&resolve_manifest_path(root)) else {
        return;
    };
    let policy = capsule_runtime::capability_policy_from_runtime_manifest(&manifest);
    let Ok(placement) =
        capsule_runtime::runtime_dir_placement(&super::run::workdir_root(root, None))
    else {
        return;
    };
    let kept = capsule_runtime::keep_host_facts(
        &policy.shell_allow,
        &policy.shell_staged_runtime,
        &capsule_runtime::capsule_writable_roots(&placement),
        &capsule_runtime::HostFactsCache::in_murmur_home(),
    )
    .kept;
    if kept > 0 {
        let noun = if kept == 1 { "binary" } else { "binaries" };
        capsule_runtime::report_println!("Kept host facts for {kept} allowlisted {noun}");
    }
}

// ─── spinner helpers (mirrors deploy.rs) ─────────────────────────────────────

fn add_pending(
    multi: &MultiProgress,
    style: &ProgressStyle,
    msg: impl Into<String>,
) -> ProgressBar {
    let pb = multi.add(ProgressBar::new_spinner());
    pb.set_style(style.clone());
    pb.set_message(msg.into());
    pb
}

fn activate_step(pb: &ProgressBar, style: &ProgressStyle, msg: impl Into<String>) {
    pb.set_style(style.clone());
    pb.set_message(msg.into());
    pb.enable_steady_tick(Duration::from_millis(80));
}

fn finish_step(pb: &ProgressBar, style: &ProgressStyle, msg: impl Into<String>) {
    pb.set_style(style.clone());
    pb.finish_with_message(msg.into());
}

fn abandon_step(pb: &ProgressBar, style: &ProgressStyle, msg: impl Into<String>) {
    pb.set_style(style.clone());
    pb.abandon_with_message(msg.into());
}

pub(crate) fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.0} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

/// `mur install [<ref>...]`: the manifest's artifacts with no reference, else each reference in
/// the order given. `frozen` holds the manifest install to exactly what `murmur.lock` pins and
/// never writes the lock; clap accepts it only with no reference and no `-g`.
pub(crate) fn run_install(
    artifact_refs: &[String],
    global: bool,
    no_precompile: bool,
    frozen: bool,
) -> Result<(), CliError> {
    // Refusals that need no store come first, so a refused install creates nothing: every
    // reference is parsed before any is installed.
    let mut parsed = artifact_refs
        .iter()
        .map(|text| InstallRef::parse(text))
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() && !global {
        let manifest_path = resolve_manifest_path(&find_project_root()?);
        load_runtime_manifest(&manifest_path).map_err(runtime_manifest_error_to_cli)?;
    }
    let (store, project_root) = determine_store(global)?;
    let project_root_path = project_root.as_deref();

    match parsed.len() {
        0 => install_manifest_deps(&store, &project_root, no_precompile, frozen),
        1 => {
            let mut stored = Vec::new();
            let installed =
                install_single(parsed.remove(0), &store, project_root_path, &mut stored);
            compile_stored(
                stored.iter().map(Stored::item),
                no_precompile,
                project_root_path,
            );
            let mut pinned_failures = fetch_pinned_for(&stored);
            installed?;
            keep_host_facts(no_precompile, project_root_path);
            match pinned_failures.len() {
                0 => Ok(()),
                1 => Err(pinned_failures.remove(0).1),
                _ => {
                    report_pinned_failures(&pinned_failures);
                    std::process::exit(1);
                }
            }
        }
        _ => install_several(
            artifact_refs,
            parsed,
            &store,
            project_root_path,
            no_precompile,
        ),
    }
}

/// `mur install <ref> <ref>...`: installs each reference in argument order, one after another,
/// then compiles every payload they stored as one batch. A reference that fails does not stop the
/// ones after it; once every other is stored, pinned and compiled, the report names each failed
/// reference, in argument order, and the process exits 1.
fn install_several(
    artifact_refs: &[String],
    parsed: Vec<InstallRef>,
    store: &LocalRegistry,
    project_root: Option<&Path>,
    no_precompile: bool,
) -> Result<(), CliError> {
    let mut stored = Vec::new();
    let mut failures = Vec::new();
    for (text, parsed) in artifact_refs.iter().zip(parsed) {
        if let Err(error) = install_single(parsed, store, project_root, &mut stored) {
            failures.push((text, error));
        }
    }
    compile_stored(stored.iter().map(Stored::item), no_precompile, project_root);
    let pinned_failures = fetch_pinned_for(&stored);
    if failures.len() < artifact_refs.len() {
        keep_host_facts(no_precompile, project_root);
    }
    if failures.is_empty() && pinned_failures.is_empty() {
        return Ok(());
    }

    if !failures.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!(
            "{} of {} references failed to install:",
            failures.len(),
            artifact_refs.len()
        );
        for (text, error) in &failures {
            capsule_runtime::report_println!();
            capsule_runtime::report_println!("  {text}");
            for line in error.to_string().lines() {
                capsule_runtime::report_println!("    {line}");
            }
        }
    }
    if !pinned_failures.is_empty() {
        report_pinned_failures(&pinned_failures);
    }
    // One `CliError` cannot name every failed reference, so the report above does and the process
    // ends here, as the manifest install's does. All I/O is done.
    std::process::exit(1);
}

/// What `mur install <ref>` names, read from the reference's text alone.
enum InstallRef {
    /// A `.mur.zip` on this filesystem, installed as `local/<packed name>`.
    File(String),
    /// `github:<owner>/<repo>@<tag>`: every `.mur.zip` of that release, in the owner's scope.
    GitHubRelease {
        owner: String,
        repo: String,
        tag: String,
    },
    /// `<scope>/<name>[@<version>]`, resolved through the stores and the scope's source.
    Name {
        name: ArtifactName,
        version: Option<String>,
    },
}

impl InstallRef {
    /// `E-REG-007` for a reference that names an artifact by anything but a full name,
    /// `E-REG-004` for a reserved version, and the reserved-scope refusal of a `github:` owner.
    fn parse(text: &str) -> Result<Self, CliError> {
        if is_local_path(text) {
            return Ok(Self::File(text.to_string()));
        }
        if text.contains(':') {
            let parsed = ArtifactRef::parse(text).map_err(|error| match error {
                ArtifactRefError::Name(error) => artifact_name_error(error),
                other => CliError::new(E_IO_003, other.to_string()),
            })?;
            let ArtifactRef::GitHub { owner, repo, tag } = parsed else {
                return Err(CliError::new(
                    E_IO_003,
                    format!("unsupported artifact reference '{text}'"),
                ));
            };
            // Refused before any request: a reserved scope cannot be claimed by an owner.
            github_scope(&owner, &repo)?;
            return Ok(Self::GitHubRelease { owner, repo, tag });
        }
        let (name, version) = parse_name_ref(text)?;
        Ok(Self::Name {
            name,
            version: version.map(str::to_string),
        })
    }
}

/// Walk up from CWD to find the directory containing murmur.yaml.
pub(crate) fn find_project_root() -> Result<PathBuf, CliError> {
    let cwd = std::env::current_dir().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to determine current directory: {e}"),
        )
    })?;
    let mut current = cwd;
    loop {
        if resolve_manifest_path(&current).exists() {
            return Ok(current);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => {
                return Err(CliError::with_hint(
                    E_IO_001,
                    format!(
                        "no project root found (no {MANIFEST_FILENAME} in current or parent directories)"
                    ),
                    "use `mur install -g <ref>` to install into the global store instead",
                ));
            }
        }
    }
}

/// Returns (store, project_root). project_root is None when global=true. Creates nothing: the
/// store's first write creates its directory, so an install refused before it leaves the project
/// as it was.
fn determine_store(global: bool) -> Result<(LocalRegistry, Option<PathBuf>), CliError> {
    if global {
        let store = LocalRegistry::from_default_home().map_err(CliError::from)?;
        Ok((store, None))
    } else {
        let project_root = find_project_root()?;
        let store_path = project_root.join(".murmur").join("artifacts");
        Ok((LocalRegistry::new(store_path), Some(project_root)))
    }
}

/// Installs the one reference `parsed` into `store`, printing an `Installed` line for each
/// payload it stores and pushing that payload onto `stored` for the caller to compile.
fn install_single(
    parsed: InstallRef,
    store: &LocalRegistry,
    project_root: Option<&Path>,
    stored: &mut Vec<Stored>,
) -> Result<(), CliError> {
    let lock_path = project_root.map(|r| r.join("murmur.lock"));
    let (name, version) = match parsed {
        InstallRef::File(path) => {
            stored.push(install_from_local_file(&path, store, lock_path.as_deref())?);
            return Ok(());
        }
        InstallRef::GitHubRelease { owner, repo, tag } => {
            return install_github_release(
                &owner,
                &repo,
                &tag,
                store,
                lock_path.as_deref(),
                stored,
            );
        }
        InstallRef::Name { name, version } => (name, version),
    };
    let version = version.as_deref();
    let lock = match &lock_path {
        Some(lock_path) => read_lock_if_any(lock_path)?,
        None => None,
    };

    // scope/name@version — the global store first, the scope's source on a miss
    if let Some(version) = version {
        let global_store = LocalRegistry::from_default_home().map_err(CliError::from)?;
        match global_store.resolve(&name, version) {
            Ok(resolved) => match version_fit(
                &FitSubject::stored(&resolved.meta, Some(&resolved.bytes)),
                served_wit_level(),
            ) {
                VersionFit::Fits => {
                    stored.push(install_from_global_store(
                        &name,
                        version,
                        resolved,
                        &global_store,
                        store,
                        lock_path.as_deref(),
                        lock.as_ref(),
                    )?);
                    return Ok(());
                }
                VersionFit::Unserved(mismatched) => {
                    return Err(level_mismatch(name.as_str(), version, mismatched));
                }
            },
            Err(RegistryError::NotFound { .. }) if name.is_local() => {
                return Err(local_store_miss(&name, Some(version)));
            }
            Err(RegistryError::NotFound { .. }) => {}
            Err(error) => return Err(store_error(error, name.as_str(), version)),
        }
    }

    let sources = Sources::load()?;
    let source = sources.source_for(&name, version)?;
    let resolved = resolve(
        &*source,
        &name,
        version,
        ExplicitVersion::FitChecked,
        lock.as_ref(),
    )
    .map_err(CliError::from)?;
    stored.push(install_resolved(
        store,
        &name,
        resolved,
        lock_path.as_deref(),
    )?);
    Ok(())
}

/// Copy `name@version`, which `global_store` holds as `resolved`, into `store` with its
/// attestation and pin it. No source is contacted; the owner id the stored payload records is
/// checked against `lock`, and the stored attestation is verified offline.
fn install_from_global_store(
    name: &ArtifactName,
    version: &str,
    resolved: ResolvedArtifact,
    global_store: &LocalRegistry,
    store: &LocalRegistry,
    lock_path: Option<&Path>,
    lock: Option<&MurmurLock>,
) -> Result<Stored, CliError> {
    verify_sha256(name.as_str(), version, &resolved.bytes, &resolved.sha256)
        .map_err(CliError::from)?;
    check_stored_owner(
        name,
        version,
        "the global store",
        resolved.meta.owner_id,
        lock,
    )?;
    let pin = LockedSha256::any(&resolved.sha256);
    if let Some(lock_path) = lock_path {
        check_lock_conflict(
            lock_path,
            name.as_str(),
            version,
            &pin,
            LockCheck::VersionAndPin,
        )?;
    }
    let owner_id = resolved.meta.owner_id;
    let (state, bundle) = check_stored_attestation(
        global_store,
        "the global store",
        name,
        version,
        &resolved.sha256,
        owner_id,
        lock,
    )?;
    let recorded = lock
        .and_then(|lock| lock.artifact_for(name.as_str()))
        .and_then(|entry| entry.signer.clone());
    store
        .store_installed_attested(
            resolved.meta,
            &resolved.bytes,
            &resolved.sha256,
            bundle.as_deref(),
        )
        .map_err(|error| store_error(error, name.as_str(), version))?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) = upsert_lock_entry(
            lock_path,
            name.as_str(),
            version,
            pin,
            owner_id,
            state.signer(),
        )? {
            capsule_runtime::report_println!("{adoption}");
        }
    }

    let display = store_display(store);
    capsule_runtime::report_println!(
        "Installed {name}@{version} \u{2192} {display}/{}/{}/{version}/{}-{version}.mur.zip{}",
        name.scope(),
        name.name(),
        name.name(),
        attested_suffix(&state)
    );
    print_warnings(&attestation_warnings(
        name,
        version,
        "the global store",
        &state,
        recorded.as_deref(),
    ));
    Ok(Stored {
        name: name.to_string(),
        version: version.to_string(),
        payload: resolved.bytes,
    })
}

/// `mur install` with no reference: every artifact `murmur.yaml` declares, at the version it
/// declares.
///
/// An entry of `murmur.lock` whose version the manifest changed is replaced, and announced with an
/// `Updated murmur.lock:` line; every other entry stays as it was. Under `frozen` the install
/// takes only what the lock pins, refusing a missing lock, a missing entry or a changed version
/// before anything is fetched, and the lock is never written.
fn install_manifest_deps(
    store: &LocalRegistry,
    project_root: &Option<PathBuf>,
    no_precompile: bool,
    frozen: bool,
) -> Result<(), CliError> {
    let root = match project_root {
        Some(r) => r.clone(),
        None => find_project_root()?,
    };
    let lock_path = root.join("murmur.lock");
    // A lock this build cannot read is refused before anything is fetched or stored.
    let lock = read_lock_if_any(&lock_path)?;

    let manifest_path = resolve_manifest_path(&root);
    let runtime_manifest =
        load_runtime_manifest(&manifest_path).map_err(runtime_manifest_error_to_cli)?;

    // The artifacts to install, by the full name each resolves under; inline-source entries are
    // skipped, and the manifest parser has already refused any other form.
    let artifacts: Vec<_> = runtime_manifest
        .artifacts
        .iter()
        .filter(|a| a.source.is_none())
        .collect();
    let full_names = artifacts
        .iter()
        .map(|a| {
            a.artifact_name()
                .map_err(|error| CliError::new(E_IO_003, error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if artifacts.is_empty() {
        capsule_runtime::report_println!("No artifacts in manifest.");
        return Ok(());
    }
    let check = if frozen {
        check_frozen_lock(lock.as_ref(), &lock_path, &full_names, &artifacts)?;
        LockCheck::Frozen
    } else {
        LockCheck::PinOnly
    };

    let global_store = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let sources = Sources::load()?;

    // Pre-check which artifacts are already in the project store.
    let cached_flags: Vec<bool> = artifacts
        .iter()
        .zip(&full_names)
        .map(|(a, name)| store.artifact_path_for(name, &a.version).exists())
        .collect();

    // Where each artifact comes from, and every owner id that decides whether it may, settled
    // before anything is fetched or written.
    let held: Vec<Result<Held, CliError>> = artifacts
        .par_iter()
        .zip(&full_names)
        .map(|(a, name)| held_payload(name, &a.version, store, &global_store))
        .collect();
    check_owners(&full_names, &artifacts, &held, &sources, lock.as_ref())?;
    check_fits(&full_names, &artifacts, &held, &sources)?;

    // ── Progress styles (matches deploy.rs exactly) ───────────────────────────
    let multi = MultiProgress::new();
    let tick_chars = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "];

    let pending_style = ProgressStyle::with_template("  {msg}").expect("valid style");
    let spinner_style = ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .expect("valid style")
        .tick_strings(tick_chars);
    let done_style = ProgressStyle::with_template("  {msg}").expect("valid style");

    let pending_l2 = ProgressStyle::with_template("      {msg}").expect("valid style");
    let spinner_l2 = ProgressStyle::with_template("      {spinner:.cyan} {msg}")
        .expect("valid style")
        .tick_strings(tick_chars);
    let download_l2 = ProgressStyle::with_template(
        "      {spinner:.cyan} {msg}  {bytes}/{total_bytes}  [{bar:12.cyan/dim}]",
    )
    .expect("valid style")
    .tick_strings(tick_chars)
    .progress_chars("█▒░");
    let done_l2 = ProgressStyle::with_template("      {msg}").expect("valid style");

    // ── Create ALL spinners upfront (pending) ─────────────────────────────────
    let n = artifacts.len();
    let s = |n: usize| if n == 1 { "" } else { "s" };

    let cached_count_pre = cached_flags.iter().filter(|&&c| c).count();
    let fetch_count_pre = n - cached_count_pre;
    let dl_hint = if fetch_count_pre == 0 {
        "  all cached".to_string()
    } else if cached_count_pre == 0 {
        format!("  {} to fetch", fetch_count_pre)
    } else {
        format!("  {}↓  {} cached", fetch_count_pre, cached_count_pre)
    };
    let header_pb = add_pending(
        &multi,
        &pending_style,
        format!(
            "{} ↓ {} artifact{}{}",
            style("·").dim(),
            n,
            s(n),
            style(&dl_hint).dim()
        ),
    );

    let spinners: Vec<ProgressBar> = artifacts
        .iter()
        .map(|a| {
            add_pending(
                &multi,
                &pending_l2,
                format!("{} ↓ {}@{}", style("·").dim(), a.name, a.version),
            )
        })
        .collect();

    // ── Activate header, then parallel-install all artifacts ─────────────────
    activate_step(
        &header_pb,
        &spinner_style,
        format!("↓ {} artifact{}", n, s(n)),
    );

    let results: Vec<Result<FetchOutcome, CliError>> = held
        .into_par_iter()
        .enumerate()
        .map(|(i, held)| {
            let artifact = artifacts[i];
            let pb = &spinners[i];
            let is_cached = cached_flags[i];
            let use_download_bar = !is_cached;

            if use_download_bar {
                pb.set_style(download_l2.clone());
                pb.set_message(format!("↓ {}@{}", artifact.name, artifact.version));
                pb.enable_steady_tick(Duration::from_millis(80));
                crate::source::github::push_download_progress(pb.clone());
            } else {
                activate_step(
                    pb,
                    &spinner_l2,
                    format!("↓ {}@{}", artifact.name, artifact.version),
                );
            }

            let result = fetch_and_store(
                &full_names[i],
                artifact.version.as_str(),
                held,
                &sources,
                lock.as_ref(),
                store,
                &global_store,
                check,
            );

            if use_download_bar {
                crate::source::github::pop_download_progress();
            }

            match &result {
                Ok(outcome) => {
                    let info = if is_cached {
                        "cached".to_string()
                    } else {
                        format_bytes(outcome.bytes_len)
                    };
                    finish_step(
                        pb,
                        &done_l2,
                        format!(
                            "{} ↓ {}@{}  {info}",
                            style("✓").green().bold(),
                            artifact.name,
                            artifact.version
                        ),
                    );
                }
                Err(_) => {
                    abandon_step(
                        pb,
                        &done_l2,
                        format!(
                            "{} ↓ {}@{}  failed",
                            style("✗").red().bold(),
                            artifact.name,
                            artifact.version
                        ),
                    );
                }
            }
            result
        })
        .collect();

    // Collapse level-2 bars, then finish the group header with summary.
    for pb in &spinners {
        multi.remove(pb);
    }

    // Partition by artifact index instead of `.collect::<Result<_, _>>()`: collecting into a
    // Result short-circuits on the first Err, throwing away every other failure *and* every
    // success — including successes whose bytes are already on disk but whose murmur.lock
    // entry would then never be written (`mur run` later fails with E-RUN-003).
    let mut successes: Vec<(usize, FetchOutcome)> = Vec::new();
    let mut failures: Vec<(usize, CliError)> = Vec::new();
    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(outcome) => successes.push((i, outcome)),
            Err(err) => failures.push((i, err)),
        }
    }

    // Upsert murmur.lock sequentially (not inside the parallel fetch loop) so concurrent
    // fetches never race on the same lockfile write. Every success is pinned regardless of
    // how many other artifacts failed; `--frozen` pins nothing. Each entry whose version the
    // manifest moved gets its `Updated` line, judged against the lock as it was read above.
    let mut lock_changes = Vec::new();
    for (_, outcome) in successes.iter().filter(|_| !frozen) {
        if let Some(LockUpsert {
            name,
            version,
            sha256,
            owner_id,
            signer,
        }) = &outcome.lock_upsert
        {
            let previous = lock
                .as_ref()
                .and_then(|lock| lock.artifact_for(name))
                .map(|entry| entry.resolved_version.as_str())
                .filter(|previous| previous != version);
            let adoption = upsert_lock_entry(
                &lock_path,
                name,
                version,
                sha256.clone(),
                *owner_id,
                signer.as_deref(),
            )?;
            if let Some(previous) = previous {
                lock_changes.push(format!(
                    "Updated murmur.lock: {name} {previous} \u{2192} {version}"
                ));
            }
            lock_changes.extend(adoption);
        }
    }

    // After the lock is written, so an interrupted compile never costs a pin, and for the cached
    // artifacts too, so forms a new `mur` build keys differently are filled in. The header
    // spinner keeps ticking meanwhile.
    compile_stored(
        successes.iter().map(|(i, outcome)| PrecompileItem {
            name: &artifacts[*i].name,
            version: &artifacts[*i].version,
            payload: &outcome.payload,
        }),
        no_precompile,
        Some(&root),
    );

    // Each stored tool's pinned program downloads, after the lock is written, cached artifacts
    // included: a present store entry costs a verification and no request. A failed download is
    // reported under its artifact, which stays stored and pinned so the next install retries it.
    // The lines wait for the header spinner to finish.
    let mut pinned_lines = Vec::new();
    for (i, outcome) in &successes {
        let pinned = super::pinned_programs::fetch_pinned_downloads(
            &artifacts[*i].name,
            &artifacts[*i].version,
            &outcome.payload,
        );
        pinned_lines.extend(pinned.lines);
        if let Some(failure) = pinned.failure {
            failures.push((*i, failure));
        }
    }
    failures.sort_by_key(|(i, _)| *i);

    // Byte/cache accounting covers the successes only — a failed artifact contributed no
    // bytes and is neither "fetched" nor "cached".
    let fetched_bytes: u64 = successes
        .iter()
        .filter(|(i, _)| !cached_flags[*i])
        .map(|(_, o)| o.bytes_len)
        .sum();
    let cached_n = successes.iter().filter(|(i, _)| cached_flags[*i]).count();
    let summary_tail = install_summary_tail(successes.len(), cached_n, fetched_bytes);
    // Who published each payload, once the bars are finished: a line per verified attestation,
    // the warnings to stderr.
    let attested_lines: Vec<String> = successes
        .iter()
        .filter_map(|(i, outcome)| {
            outcome.attestation.signer().map(|signer| {
                format!(
                    "{}@{} attested by {signer}",
                    full_names[*i], artifacts[*i].version
                )
            })
        })
        .collect();
    let attestation_warning_lines: Vec<String> = successes
        .iter()
        .flat_map(|(_, outcome)| outcome.warnings.iter().cloned())
        .collect();

    if failures.is_empty() {
        finish_step(
            &header_pb,
            &done_style,
            format!("{} ↓ {}", style("✓").green().bold(), summary_tail),
        );
        print_lines(&attested_lines);
        print_warnings(&attestation_warning_lines);
        print_lock_changes(&lock_changes);
        print_lines(&pinned_lines);
        keep_host_facts(no_precompile, Some(&root));
        return Ok(());
    }

    abandon_step(
        &header_pb,
        &done_style,
        format!("{} ↓ artifacts  failed", style("✗").red().bold()),
    );
    print_lines(&attested_lines);
    print_warnings(&attestation_warning_lines);
    print_lock_changes(&lock_changes);
    print_lines(&pinned_lines);
    if !successes.is_empty() {
        keep_host_facts(no_precompile, Some(&root));
    }

    // Plain report lines rather than indicatif bar text: MultiProgress writes nothing at all
    // when stdout/stderr is not a terminal, so bar messages are cosmetic-only and would make
    // the failure report invisible in CI logs and pipes.
    capsule_runtime::report_println!();
    capsule_runtime::report_println!(
        "{} of {} artifact{} failed to install:",
        failures.len(),
        n,
        s(n)
    );
    for (i, err) in &failures {
        let artifact = artifacts[*i];
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("  {}@{}", artifact.name, artifact.version);
        // CliError's Display renders `error[CODE]: message` plus its hint line; keep every
        // failure's full text, indented under the artifact that produced it.
        for line in err.to_string().lines() {
            capsule_runtime::report_println!("    {line}");
        }
    }
    if !successes.is_empty() {
        capsule_runtime::report_println!();
        capsule_runtime::report_println!("installed {summary_tail}");
    }

    // A CliError carries a single code/message/hint and cannot represent "K of N artifacts
    // failed", so the report is printed above and the process terminates here — same pattern
    // as `mur doctor`. No destructors run, which is fine: all I/O is done.
    std::process::exit(1);
}

fn print_lines(lines: &[String]) {
    for line in lines {
        capsule_runtime::report_println!("{line}");
    }
}

/// Render the shared `{count} artifact{s}  {bytes-or-cache-note}` tail used by both the
/// green indicatif roll-up and the plain-text success roll-up, over an artifact subset.
fn install_summary_tail(count: usize, cached_n: usize, fetched_bytes: u64) -> String {
    let s = if count == 1 { "" } else { "s" };
    if cached_n == count {
        format!("{count} artifact{s}  all cached")
    } else {
        let note = if cached_n > 0 {
            format!("  {cached_n} cached")
        } else {
            String::new()
        };
        format!("{count} artifact{s}  {}{note}", format_bytes(fetched_bytes))
    }
}

/// Result of a single [`fetch_and_store`] call: the byte length of the installed artifact,
/// plus the entry to upsert into `murmur.lock` once every parallel fetch has completed. Set for
/// store-held and source-served artifacts alike so every install path pins the lockfile.
struct FetchOutcome {
    bytes_len: u64,
    lock_upsert: Option<LockUpsert>,
    /// The payload bytes written to the store, which are the bytes precompiled.
    payload: bytes::Bytes,
    /// What the payload's attestation proved.
    attestation: AttestationState,
    /// The attestation warning lines to print once the progress bars are finished.
    warnings: Vec<String>,
}

/// One `murmur.lock` entry a [`fetch_and_store`] pins.
struct LockUpsert {
    name: String,
    version: String,
    sha256: LockedSha256,
    owner_id: Option<u64>,
    /// The signer of a verified attestation; `None` clears the entry's `signer`.
    signer: Option<String>,
}

impl FetchOutcome {
    /// The outcome of pinning `payload` as `name@version` with `attestation`, its warnings judged
    /// against the signer `lock` records for `name`.
    #[allow(clippy::too_many_arguments)]
    fn pinned(
        name: &ArtifactName,
        version: &str,
        sha256: LockedSha256,
        owner_id: Option<u64>,
        payload: bytes::Bytes,
        attestation: AttestationState,
        origin: &str,
        lock: Option<&MurmurLock>,
    ) -> Self {
        let recorded = lock
            .and_then(|lock| lock.artifact_for(name.as_str()))
            .and_then(|entry| entry.signer.as_deref());
        let warnings = attestation_warnings(name, version, origin, &attestation, recorded);
        Self {
            bytes_len: payload.len() as u64,
            lock_upsert: Some(LockUpsert {
                name: name.to_string(),
                version: version.to_string(),
                sha256,
                owner_id,
                signer: attestation.signer().map(str::to_string),
            }),
            payload,
            attestation,
            warnings,
        }
    }
}

/// Where a declared artifact's payload is: already in the project store, in the global store, or
/// in neither, so its scope's source serves it.
enum Held {
    Project(ResolvedArtifact),
    Global(ResolvedArtifact),
    Source,
}

/// Look `name@version` up in the project store, then the global store. Reads no source.
fn held_payload(
    name: &ArtifactName,
    version: &str,
    store: &LocalRegistry,
    global_store: &LocalRegistry,
) -> Result<Held, CliError> {
    // What the project store holds already — an artifact installed from a file, notably, which no
    // other place has — is pinned where it is.
    if let Ok(held) = store.resolve(name, version) {
        verify_sha256(name.as_str(), version, &held.bytes, &held.sha256).map_err(CliError::from)?;
        return Ok(Held::Project(held));
    }
    match global_store.resolve(name, version) {
        Ok(resolved) => {
            verify_sha256(name.as_str(), version, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            Ok(Held::Global(resolved))
        }
        Err(RegistryError::NotFound { .. }) if name.is_local() => {
            Err(local_store_miss(name, Some(version)))
        }
        Err(RegistryError::NotFound { .. }) => Ok(Held::Source),
        Err(error) => Err(store_error(error, name.as_str(), version)),
    }
}

/// `E-REG-010` for the whole install when a stored payload, or the source a missing one would come
/// from, has an owner id `lock` (or the built-in `murmur` owner) does not hold its scope to. Asks
/// each scope's source for its owner once; a source that does not answer is left for its
/// artifacts' own fetch to report.
fn check_owners(
    names: &[ArtifactName],
    artifacts: &[&murmur_artifact::RuntimeArtifact],
    held: &[Result<Held, CliError>],
    sources: &Sources,
    lock: Option<&MurmurLock>,
) -> Result<(), CliError> {
    let mut asked = std::collections::BTreeSet::new();
    for ((name, artifact), held) in names.iter().zip(artifacts).zip(held) {
        match held {
            Ok(Held::Project(held)) => check_stored_owner(
                name,
                &artifact.version,
                "the project store",
                held.meta.owner_id,
                lock,
            )?,
            Ok(Held::Global(held)) => check_stored_owner(
                name,
                &artifact.version,
                "the global store",
                held.meta.owner_id,
                lock,
            )?,
            Ok(Held::Source) => {
                if !asked.insert(name.scope().to_string()) {
                    continue;
                }
                let Some(source) = sources.for_scope(name.scope()) else {
                    continue;
                };
                if let Ok(owner_id) = source.owner_id() {
                    check_owner(
                        &*source,
                        owner_id,
                        lock,
                        &format!("{name}@{}", artifact.version),
                    )
                    .map_err(CliError::from)?;
                }
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// `E-WIT-LEVEL-001` for the whole install when a pinned version does not fit the WIT level this
/// mur serves, wherever its payload would come from: a store's payload is read for its contracts,
/// and a source's index for the version's `interfaces`, before anything is fetched or written. A
/// version a source's index cannot be read for is left for its own fetch to report.
fn check_fits(
    names: &[ArtifactName],
    artifacts: &[&murmur_artifact::RuntimeArtifact],
    held: &[Result<Held, CliError>],
    sources: &Sources,
) -> Result<(), CliError> {
    for ((name, artifact), held) in names.iter().zip(artifacts).zip(held) {
        match held {
            Ok(Held::Project(held) | Held::Global(held)) => {
                let subject = FitSubject::stored(&held.meta, Some(&held.bytes));
                if let VersionFit::Unserved(mismatched) = version_fit(&subject, served_wit_level())
                {
                    return Err(level_mismatch(name.as_str(), &artifact.version, mismatched));
                }
            }
            Ok(Held::Source) => {
                let Some(source) = sources.for_scope(name.scope()) else {
                    continue;
                };
                let Ok(published) = source.index() else {
                    continue;
                };
                let Some(listed) = published
                    .index
                    .artifacts
                    .iter()
                    .find(|listed| listed.name == name.name())
                else {
                    continue;
                };
                let Some(version) = select_version(listed, &artifact.version) else {
                    continue;
                };
                if let VersionFit::Unserved(mismatched) =
                    version_fit(&FitSubject::indexed(version), served_wit_level())
                {
                    return Err(index_level_mismatch(
                        name.as_str(),
                        &artifact.version,
                        mismatched,
                        source.label(),
                        newest_fitting(listed).map(|fits| fits.version.as_str()),
                    ));
                }
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// Pin one artifact `store` already holds, or copy it from the global store, or fetch it from its
/// scope's source, and write it to `store`. Each payload is held to its entry in `lock` as `check`
/// says before it is stored.
#[allow(clippy::too_many_arguments)]
fn fetch_and_store(
    name: &ArtifactName,
    version: &str,
    held: Result<Held, CliError>,
    sources: &Sources,
    lock: Option<&MurmurLock>,
    store: &LocalRegistry,
    global_store: &LocalRegistry,
    check: LockCheck,
) -> Result<FetchOutcome, CliError> {
    match held? {
        Held::Project(held) => {
            let pin = LockedSha256::any(&held.sha256);
            check_lock_entry(lock, name.as_str(), version, &pin, check)?;
            let (state, _) = check_stored_attestation(
                store,
                "the project store",
                name,
                version,
                &held.sha256,
                held.meta.owner_id,
                lock,
            )?;
            Ok(FetchOutcome::pinned(
                name,
                version,
                pin,
                held.meta.owner_id,
                held.bytes,
                state,
                "the project store",
                lock,
            ))
        }
        Held::Global(resolved) => {
            let pin = LockedSha256::any(&resolved.sha256);
            check_lock_entry(lock, name.as_str(), version, &pin, check)?;
            let owner_id = resolved.meta.owner_id;
            let (state, bundle) = check_stored_attestation(
                global_store,
                "the global store",
                name,
                version,
                &resolved.sha256,
                owner_id,
                lock,
            )?;
            store
                .store_installed_attested(
                    resolved.meta,
                    &resolved.bytes,
                    &resolved.sha256,
                    bundle.as_deref(),
                )
                .map_err(|error| store_error(error, name.as_str(), version))?;
            Ok(FetchOutcome::pinned(
                name,
                version,
                pin,
                owner_id,
                resolved.bytes,
                state,
                "the global store",
                lock,
            ))
        }
        Held::Source => {
            let source = sources.source_for(name, Some(version))?;
            let resolved = resolve(
                &*source,
                name,
                Some(version),
                ExplicitVersion::FitChecked,
                lock,
            )
            .map_err(CliError::from)?;
            let meta = source_meta(name, version, &resolved.manifest, resolved.owner_id);
            let pin = LockedSha256::any(&resolved.sha256);
            check_lock_entry(lock, name.as_str(), version, &pin, check)?;
            store
                .store_installed_attested(
                    meta,
                    &resolved.bytes,
                    &resolved.sha256,
                    resolved.bundle.as_deref(),
                )
                .map_err(|error| store_error(error, name.as_str(), version))?;
            Ok(FetchOutcome::pinned(
                name,
                version,
                pin,
                Some(resolved.owner_id),
                resolved.bytes,
                resolved.attestation,
                &resolved.source,
                lock,
            ))
        }
    }
}

/// Install a `.mur.zip` sitting on this filesystem into `store`, as `local/<packed name>`, and pin
/// it in the project's `murmur.lock` when `lock_path` is given.
///
/// A file can claim no other scope: a scope says whose artifact it is, and nothing in a file on
/// disk shows that it came from the owner it would name.
pub(crate) fn install_from_local_file(
    path_str: &str,
    store: &LocalRegistry,
    lock_path: Option<&Path>,
) -> Result<Stored, CliError> {
    let bytes = std::fs::read(path_str)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read {path_str}: {e}")))?;

    let manifest = load_manifest_from_artifact_bytes(&bytes).map_err(|error| {
        native_payload_refusal(&error, LOCAL_SCOPE, &bytes).unwrap_or_else(|| {
            CliError::new(
                E_IO_003,
                format!("{path_str} is not a valid .mur.zip: {error}"),
            )
        })
    })?;

    let name = ArtifactName::from_parts(LOCAL_SCOPE, &manifest.name).map_err(|error| {
        CliError::new(
            E_IO_003,
            format!("{path_str} declares a name that cannot be installed: {error}"),
        )
    })?;
    let sha256 = sha256_hex(&bytes);
    let meta = ArtifactMeta {
        name: name.clone(),
        version: manifest.version.clone(),
        runtime: manifest.registry_runtime(),
        artifact_runtime: manifest.runtime.clone(),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: None,
        built_at_level: None,
    };
    let pin = LockedSha256::any(&sha256);
    store
        .store_installed_overwrite(meta, &bytes, &sha256)
        .map_err(|error| store_error(error, name.as_str(), &manifest.version))?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) =
            upsert_lock_entry(lock_path, name.as_str(), &manifest.version, pin, None, None)?
        {
            capsule_runtime::report_println!("{adoption}");
        }
    }

    capsule_runtime::report_println!("Installed {name}@{} from {path_str}", manifest.version);
    Ok(Stored {
        name: name.to_string(),
        version: manifest.version,
        payload: bytes.into(),
    })
}

fn is_local_path(s: &str) -> bool {
    s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || (s.contains('/') && s.ends_with(".mur.zip"))
}

/// `<scope>/<name>[@<version>]`. The name is refused with `E-REG-007` when it is not a full name,
/// and a reserved version with `E-REG-004`: the index lists no such version, and leaving the
/// version out already asks for the newest that fits.
fn parse_name_ref(input: &str) -> Result<(ArtifactName, Option<&str>), CliError> {
    let mut segments = input.split('@');
    let name = segments.next().unwrap_or_default();
    let version = segments.next();
    if name.is_empty() || version == Some("") || segments.next().is_some() {
        return Err(CliError::new(
            E_IO_003,
            "artifact reference must be in the format <scope/name[@version]>",
        ));
    }
    let name = artifact_name_arg(name)?;
    if let Some(version) = version {
        if is_reserved_version(version) {
            return Err(CliError::with_hint(
                E_REG_004,
                format!("version '{version}' is reserved and names no single version of {name}"),
                format!(
                    "omit the version to install the newest version that fits this mur: mur \
                     install {name}"
                ),
            ));
        }
    }
    Ok((name, version))
}

fn store_display(store: &LocalRegistry) -> String {
    let root = store.root();
    if let Ok(home) = std::env::var("HOME") {
        let home = Path::new(&home);
        if let Ok(rel) = root.strip_prefix(home) {
            return format!("~/{}", rel.display());
        }
    }
    root.display().to_string()
}

#[cfg(test)]
mod tests {
    use murmur_artifact::{LockOrigin, LockedSha256, MurmurLock, LOCK_VERSION};

    use super::{check_lock_entry, LockCheck, CHANGED_BYTES_HINT};

    fn pinned_lock() -> MurmurLock {
        let mut lock = MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: Vec::new(),
        };
        lock.upsert(
            "ana/tool",
            "1.0.0",
            LockedSha256::any("pinned-hash"),
            LockOrigin::Operator,
        );
        lock
    }

    /// Each lock check against an entry pinning `ana/tool@1.0.0` at `pinned-hash`.
    #[test]
    fn each_lock_check_refuses_only_what_it_holds_the_entry_to() {
        let lock = pinned_lock();
        let check = |version: &str, hash: &str, mode: LockCheck| {
            check_lock_entry(
                Some(&lock),
                "ana/tool",
                version,
                &LockedSha256::any(hash),
                mode,
            )
            .err()
        };
        let all = [
            LockCheck::VersionAndPin,
            LockCheck::PinOnly,
            LockCheck::Frozen,
        ];

        for mode in all {
            assert!(check("1.0.0", "pinned-hash", mode).is_none(), "{mode:?}");
            let changed = check("1.0.0", "other-hash", mode).unwrap();
            assert_eq!(changed.code, "E-REG-005");
            assert_eq!(
                changed.message,
                "murmur.lock conflict for 'ana/tool': 'ana/tool' is pinned at sha256 pinned-hash, \
                 but other-hash was resolved"
            );
            assert_eq!(changed.hint.as_deref(), Some(CHANGED_BYTES_HINT));
            assert!(
                check_lock_entry(None, "ana/tool", "2.0.0", &LockedSha256::any("x"), mode).is_ok()
            );
        }

        assert!(check("1.1.0", "new-hash", LockCheck::PinOnly).is_none());
        let moved = check("1.1.0", "new-hash", LockCheck::VersionAndPin).unwrap();
        assert_eq!(
            moved.message,
            "murmur.lock conflict for 'ana/tool': pinned ana/tool@1.0.0, but ana/tool@1.1.0 was \
             resolved"
        );
        assert_eq!(
            moved.hint.as_deref(),
            Some("to move ana/tool to 1.1.0, change its version in murmur.yaml and run `mur install`")
        );
        let frozen = check("1.1.0", "new-hash", LockCheck::Frozen).unwrap();
        assert_eq!(frozen.message, moved.message);
        assert_eq!(
            frozen.hint.as_deref(),
            Some(
                "--frozen does not change murmur.lock; run `mur install` without --frozen to take \
                 1.1.0"
            )
        );
    }
}
