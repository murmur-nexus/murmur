use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use capsule_runtime::precompile::Precompiler;
use capsule_runtime::served_wit_level;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use murmur_artifact::{
    current_platform, is_reserved_version, level_fit, load_manifest_from_artifact_bytes,
    load_runtime_manifest, read_lockfile, registry_warning_link, resolve_manifest_path, sha256_hex,
    split_platform_suffix, split_platform_tag, verify_sha256, wit_contracts_from_artifact_bytes,
    write_lockfile_atomic, ArtifactMeta, ArtifactName, ArtifactRef, ArtifactRefError, LevelFit,
    LocalRegistry, LockOrigin, LockedSha256, LockfileError, Manifest, MurmurLock, Platform,
    Registry, RegistryError, ResolvedArtifact, RuntimeType, VersionFit, LOCAL_SCOPE, LOCK_VERSION,
    MANIFEST_FILENAME, SUPPORTED_PLATFORMS, W_REG_001,
};
use rayon::prelude::*;

use crate::{
    error::{
        artifact_name_arg, artifact_name_error, local_store_miss, CliError, E_IO_001, E_IO_003,
        E_REG_004, E_REG_005, E_REG_006,
    },
    source::{
        check_owner, check_stored_owner, github::GitHubSource, github_scope, newest_fitting,
        resolve, select_version, unanswered_hint, ArtifactSource, ExplicitVersion, ResolveError,
        Resolved, SourceError, Sources,
    },
    wit_level::{index_level_mismatch, level_mismatch},
};

use super::runtime_manifest_error_to_cli;

/// Compare a registry-resolved artifact against any existing `murmur.lock` pin at
/// `lock_path`, refusing to install when they disagree. A missing lockfile is not a
/// conflict — the artifact simply isn't pinned yet.
fn check_lock_conflict(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: &LockedSha256,
) -> Result<(), CliError> {
    match read_lockfile(lock_path) {
        Ok(lock) => {
            if let Some(entry) = lock.artifact_for(name) {
                if let Some(conflict) = entry.conflict_with(version, sha256) {
                    return Err(CliError::with_hint(
                        E_REG_005,
                        format!("murmur.lock conflict for '{name}': {conflict}"),
                        "if this is an intentional upgrade, remove the stale entry from \
                         murmur.lock before installing again",
                    ));
                }
            }
            Ok(())
        }
        Err(LockfileError::NotFound(_)) => Ok(()),
        Err(err) => Err(super::lockfile_error_to_cli(err)),
    }
}

/// The `murmur.lock` pin for a payload the store has just recorded.
///
/// A native binary is different bytes per platform, so it is pinned under the platform it was
/// installed for; a WASM component or a static skill is the same bytes everywhere and is pinned
/// once under `any`. A native payload whose platform nothing recorded (see [`W_REG_001`]) is
/// still pinned per-platform, under this host's — the bytes this host resolved are the bytes
/// this hash verifies.
fn lock_pin(meta: &ArtifactMeta, sha256: &str) -> LockedSha256 {
    if meta.runtime != RuntimeType::Native {
        return LockedSha256::any(sha256);
    }
    let platform = meta.platforms.first().map_or_else(
        || current_platform().to_string(),
        |(os, arch)| format!("{os}-{arch}"),
    );
    LockedSha256::for_one_platform(&platform, sha256)
}

/// The `platforms` list an [`ArtifactMeta`] records for a payload.
///
/// Only a native payload carries one: a WASM component or a static skill is filed at the generic
/// path so every host resolves it, which is the invariant `stage_session`'s registry fallback
/// depends on. `platform` is what the source reported for the payload it handed back — the tag
/// on the asset name it selected, or an explicitly requested target platform — never the
/// manifest's aspiration.
fn recorded_platforms(
    name: &str,
    version: &str,
    runtime: RuntimeType,
    platform: Option<&str>,
) -> Vec<Platform> {
    if runtime != RuntimeType::Native {
        return Vec::new();
    }
    match platform.and_then(split_platform_tag) {
        Some((os, arch)) => vec![(os.to_string(), arch.to_string())],
        None => {
            warn_untagged_native(name, version);
            Vec::new()
        }
    }
}

/// Report a native payload filed where every host will resolve it, because nothing in the
/// install said which platform it is for.
fn warn_untagged_native(name: &str, version: &str) {
    capsule_runtime::report_eprintln!(
        "warning[{W_REG_001}]: {name}@{version} is a native artifact with no recorded platform \u{2014} \
         it was filed at the generic path, where every host resolves it\n  {}",
        registry_warning_link(W_REG_001)
    );
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
/// entry when known; an entry keeps the id it has otherwise.
///
/// Returns the line announcing the adoption when the entry was a `manage.pull()` pin, for the
/// caller to print once nothing else is drawing on the terminal.
fn upsert_lock_entry(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: LockedSha256,
    owner_id: Option<u64>,
) -> Result<Option<String>, CliError> {
    let mut lock = read_lock_if_any(lock_path)?.unwrap_or_else(|| MurmurLock {
        lock_version: LOCK_VERSION,
        artifacts: Vec::new(),
    });

    let previous = lock.upsert(name, version, sha256, LockOrigin::Operator);
    if let Some(owner_id) = owner_id {
        lock.set_owner_id(name, owner_id);
    }

    write_lockfile_atomic(lock_path, &lock).map_err(super::lockfile_error_to_cli)?;
    Ok(match previous {
        Some(LockOrigin::Runtime { session }) => Some(format!(
            "Adopted {name}@{version} as operator-declared (it was pulled at runtime by session \
             {session})"
        )),
        Some(LockOrigin::Operator) | None => None,
    })
}

/// Print each adoption line [`upsert_lock_entry`] returned, once the progress bars are finished.
fn print_adoptions(adoptions: &[String]) {
    for adoption in adoptions {
        capsule_runtime::report_println!("{adoption}");
    }
}

// ─── source helpers ──────────────────────────────────────────────────────────

/// The [`ArtifactMeta`] of a payload a source served as `name@version`, recording the asset's
/// `platform` and the source's `owner_id`.
pub(crate) fn source_meta(
    name: &ArtifactName,
    version: &str,
    manifest: &Manifest,
    platform: Option<&str>,
    owner_id: u64,
) -> ArtifactMeta {
    let runtime = manifest.registry_runtime();
    ArtifactMeta {
        name: name.clone(),
        version: version.to_string(),
        runtime,
        artifact_runtime: manifest.runtime.clone(),
        // The platform of the asset the source served, not of this host: a `--all-platforms` or
        // deploy fetch installs a payload for a platform that is not the one running the install.
        platforms: recorded_platforms(name.as_str(), version, runtime, platform),
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
    precompiler: Option<&Precompiler>,
) -> Result<(), CliError> {
    let meta = source_meta(
        name,
        &resolved.version,
        &resolved.manifest,
        resolved.platform.as_deref(),
        resolved.owner_id,
    );
    store_and_pin(
        local_registry,
        meta,
        &resolved.bytes,
        &resolved.sha256,
        lock_path,
    )?;
    capsule_runtime::report_println!(
        "Installed {name}@{} from {}",
        resolved.version,
        resolved.source
    );
    precompile(
        precompiler,
        name.as_str(),
        &resolved.version,
        &resolved.bytes,
    );
    Ok(())
}

/// Write `meta`'s payload to `local_registry` and, when `lock_path` is given, pin it there with
/// the owner id `meta` records.
fn store_and_pin(
    local_registry: &LocalRegistry,
    meta: ArtifactMeta,
    bytes: &[u8],
    sha256: &str,
    lock_path: Option<&Path>,
) -> Result<(), CliError> {
    let pin = lock_pin(&meta, sha256);
    let name = meta.name.to_string();
    let version = meta.version.clone();
    let owner_id = meta.owner_id;
    local_registry
        .store_installed_overwrite(meta, bytes, sha256)
        .map_err(CliError::from)?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) = upsert_lock_entry(lock_path, &name, &version, pin, owner_id)? {
            capsule_runtime::report_println!("{adoption}");
        }
    }
    Ok(())
}

/// `mur install github:<owner>/<repo>@<tag>`: every `.mur.zip` of that release, each filed under
/// its packed name in the owner's scope. The owner id is checked against `lock` and, for the
/// `murmur` scope, the compiled-in one before any payload is downloaded.
fn install_github_release(
    owner: &str,
    repo: &str,
    tag: &str,
    store: &LocalRegistry,
    lock_path: Option<&Path>,
    precompiler: Option<&Precompiler>,
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
    for payload in payloads {
        let manifest = load_manifest_from_artifact_bytes(&payload.bytes).map_err(|error| {
            CliError::new(
                E_IO_003,
                format!(
                    "{} from {label} is not a valid .mur.zip artifact: {error}",
                    payload.file
                ),
            )
        })?;
        let name = ArtifactName::from_parts(&scope, &manifest.name).map_err(|error| {
            CliError::new(
                E_IO_003,
                format!("an asset from {label} cannot be stored in the '{scope}' scope: {error}"),
            )
        })?;
        let version = manifest.version.clone();
        let platform = split_platform_suffix(&payload.file).map(|(_, platform)| platform);
        let sha256 = sha256_hex(&payload.bytes);
        let meta = source_meta(&name, &version, &manifest, platform, owner_id);
        store_and_pin(store, meta, &payload.bytes, &sha256, lock_path)?;
        capsule_runtime::report_println!("Installed {name}@{version} from {label}");
        precompile(precompiler, name.as_str(), &version, &payload.bytes);
    }
    Ok(())
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

/// Stores the compiled form of the payload `bytes` just stored as `name@version`, when a
/// precompiler is given. The outcome is deliberately dropped: a form that is not stored only means
/// the first launch compiles, so it never changes what install prints or how it exits.
fn precompile(precompiler: Option<&Precompiler>, name: &str, version: &str, bytes: &[u8]) {
    if let Some(precompiler) = precompiler {
        let _ = precompiler.precompile(name, version, bytes);
    }
}

/// The platforms `mur install --all-platforms` downloads for.
///
/// Deliberately a subset of [`SUPPORTED_PLATFORMS`]: which platforms CI seeds a store with is a
/// release-engineering decision, separate from which platform strings this build recognises in a
/// store path, an asset name or a lockfile key. Collapsing the two lists would make adding a
/// recognised platform silently add a download.
const ALL_PLATFORMS: &[&str] = &["linux-x86_64", "darwin-aarch64"];

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

fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.0} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

pub(crate) fn run_install(
    artifact_ref: Option<&str>,
    global: bool,
    all_platforms: bool,
    no_precompile: bool,
) -> Result<(), CliError> {
    // Refusals that need no store come first, so a refused install creates nothing.
    let parsed = artifact_ref.map(InstallRef::parse).transpose()?;
    if all_platforms {
        return run_install_all_platforms(parsed, no_precompile);
    }
    if parsed.is_none() && !global {
        let manifest_path = resolve_manifest_path(&find_project_root()?);
        load_runtime_manifest(&manifest_path).map_err(runtime_manifest_error_to_cli)?;
    }
    let (store, project_root) = determine_store(global)?;

    match parsed {
        None => install_manifest_deps(&store, &project_root, no_precompile),
        Some(parsed) => install_single(parsed, &store, project_root.as_deref(), no_precompile),
    }
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

fn install_single(
    parsed: InstallRef,
    store: &LocalRegistry,
    project_root: Option<&Path>,
    no_precompile: bool,
) -> Result<(), CliError> {
    let lock_path = project_root.map(|r| r.join("murmur.lock"));
    let (name, version) = match parsed {
        InstallRef::File(path) => {
            let precompiler = build_precompiler(no_precompile, project_root);
            return install_from_local_file(
                &path,
                store,
                lock_path.as_deref(),
                precompiler.as_ref(),
            );
        }
        InstallRef::GitHubRelease { owner, repo, tag } => {
            let precompiler = build_precompiler(no_precompile, project_root);
            return install_github_release(
                &owner,
                &repo,
                &tag,
                store,
                lock_path.as_deref(),
                precompiler.as_ref(),
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
        match global_store.resolve_with_platform(&name, version, Some(current_platform())) {
            Ok(resolved) => {
                check_stored_fit(&name, version, &resolved)?;
                return install_from_global_store(
                    &name,
                    version,
                    resolved,
                    store,
                    lock_path.as_deref(),
                    lock.as_ref(),
                    build_precompiler(no_precompile, project_root).as_ref(),
                );
            }
            Err(RegistryError::NotFound { .. }) if name.is_local() => {
                return Err(local_store_miss(&name, Some(version)));
            }
            Err(RegistryError::NotFound { .. }) => {}
            Err(e) => return Err(CliError::from(e)),
        }
    }

    let sources = Sources::load()?;
    let source = sources.source_for(&name, version)?;
    let resolved = resolve(
        &*source,
        &name,
        version,
        ExplicitVersion::FitChecked,
        current_platform(),
        lock.as_ref(),
    )
    .map_err(CliError::from)?;
    let precompiler = build_precompiler(no_precompile, project_root);
    install_resolved(
        store,
        &name,
        resolved,
        lock_path.as_deref(),
        precompiler.as_ref(),
    )
}

/// `E-WIT-LEVEL-001` when the payload a store holds as `name@version` does not fit the WIT level
/// this mur serves, read from the contracts its sidecar records, else from its component. A
/// payload with no component (a native tool, a skill) fits.
fn check_stored_fit(
    name: &ArtifactName,
    version: &str,
    held: &ResolvedArtifact,
) -> Result<(), CliError> {
    let contracts = held
        .meta
        .wit_contracts
        .clone()
        .or_else(|| wit_contracts_from_artifact_bytes(&held.bytes));
    let Some(contracts) = contracts else {
        return Ok(());
    };
    match level_fit(&contracts, served_wit_level()) {
        LevelFit::Fits => Ok(()),
        LevelFit::Mismatched(mismatched) => Err(level_mismatch(name.as_str(), version, mismatched)),
    }
}

/// Copy `name@version`, which the global store holds as `resolved`, into `store` and pin it. No
/// source is contacted; the owner id the stored payload records is checked against `lock`.
fn install_from_global_store(
    name: &ArtifactName,
    version: &str,
    resolved: ResolvedArtifact,
    store: &LocalRegistry,
    lock_path: Option<&Path>,
    lock: Option<&MurmurLock>,
    precompiler: Option<&Precompiler>,
) -> Result<(), CliError> {
    verify_sha256(name.as_str(), version, &resolved.bytes, &resolved.sha256)
        .map_err(CliError::from)?;
    check_stored_owner(
        name,
        version,
        "the global store",
        resolved.meta.owner_id,
        lock,
    )?;
    let pin = lock_pin(&resolved.meta, &resolved.sha256);
    // The name the payload is filed under, which for a native artifact carries its platform tag.
    let file_stem = match resolved.meta.platforms.first() {
        Some((os, arch)) => format!("{}-{version}-{os}-{arch}", name.name()),
        None => format!("{}-{version}", name.name()),
    };
    if let Some(lock_path) = lock_path {
        check_lock_conflict(lock_path, name.as_str(), version, &pin)?;
    }
    let owner_id = resolved.meta.owner_id;
    store
        .store_installed_overwrite(resolved.meta, &resolved.bytes, &resolved.sha256)
        .map_err(CliError::from)?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) = upsert_lock_entry(lock_path, name.as_str(), version, pin, owner_id)?
        {
            capsule_runtime::report_println!("{adoption}");
        }
    }

    let display = store_display(store);
    capsule_runtime::report_println!(
        "Installed {name}@{version} \u{2192} {display}/{}/{}/{version}/{file_stem}.mur.zip",
        name.scope(),
        name.name()
    );
    precompile(precompiler, name.as_str(), version, &resolved.bytes);
    Ok(())
}

fn install_manifest_deps(
    store: &LocalRegistry,
    project_root: &Option<PathBuf>,
    no_precompile: bool,
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
                &lock_path,
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
    // how many other artifacts failed.
    let mut adoptions = Vec::new();
    for (_, outcome) in &successes {
        if let Some((name, version, sha256, owner_id)) = &outcome.lock_upsert {
            adoptions.extend(upsert_lock_entry(
                &lock_path,
                name,
                version,
                sha256.clone(),
                *owner_id,
            )?);
        }
    }

    // After the lock is written, so an interrupted compile never costs a pin, and for the cached
    // artifacts too, so forms a new `mur` build keys differently are filled in. The header
    // spinner keeps ticking meanwhile.
    if let Some(precompiler) = build_precompiler(no_precompile, Some(&root)) {
        successes.par_iter().for_each(|(i, outcome)| {
            let artifact = artifacts[*i];
            precompile(
                Some(&precompiler),
                &artifact.name,
                &artifact.version,
                &outcome.payload,
            );
        });
    }

    // Byte/cache accounting covers the successes only — a failed artifact contributed no
    // bytes and is neither "fetched" nor "cached".
    let fetched_bytes: u64 = successes
        .iter()
        .filter(|(i, _)| !cached_flags[*i])
        .map(|(_, o)| o.bytes_len)
        .sum();
    let cached_n = successes.iter().filter(|(i, _)| cached_flags[*i]).count();
    let summary_tail = install_summary_tail(successes.len(), cached_n, fetched_bytes);

    if failures.is_empty() {
        finish_step(
            &header_pb,
            &done_style,
            format!("{} ↓ {}", style("✓").green().bold(), summary_tail),
        );
        print_adoptions(&adoptions);
        return Ok(());
    }

    abandon_step(
        &header_pb,
        &done_style,
        format!("{} ↓ artifacts  failed", style("✗").red().bold()),
    );
    print_adoptions(&adoptions);

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
/// plus the `(name, resolved_version, sha256, owner_id)` to upsert into `murmur.lock` once every
/// parallel fetch has completed. Set for store-held and source-served artifacts alike so every
/// install path pins the lockfile.
struct FetchOutcome {
    bytes_len: u64,
    lock_upsert: Option<(String, String, LockedSha256, Option<u64>)>,
    /// The payload bytes written to the store, which are the bytes precompiled.
    payload: bytes::Bytes,
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
    if let Ok(held) = store.resolve_with_platform(name, version, Some(current_platform())) {
        verify_sha256(name.as_str(), version, &held.bytes, &held.sha256).map_err(CliError::from)?;
        return Ok(Held::Project(held));
    }
    match global_store.resolve_with_platform(name, version, Some(current_platform())) {
        Ok(resolved) => {
            verify_sha256(name.as_str(), version, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            Ok(Held::Global(resolved))
        }
        Err(RegistryError::NotFound { .. }) if name.is_local() => {
            Err(local_store_miss(name, Some(version)))
        }
        Err(RegistryError::NotFound { .. }) => Ok(Held::Source),
        Err(e) => Err(CliError::from(e)),
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
                check_stored_fit(name, &artifact.version, held)?;
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
                    version.fit(served_wit_level(), current_platform())
                {
                    return Err(index_level_mismatch(
                        name.as_str(),
                        &artifact.version,
                        mismatched,
                        source.label(),
                        newest_fitting(listed, current_platform())
                            .map(|fits| fits.version.as_str()),
                    ));
                }
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// Pin one artifact `store` already holds, or copy it from the global store, or fetch it from its
/// scope's source, and write it to `store`.
fn fetch_and_store(
    name: &ArtifactName,
    version: &str,
    held: Result<Held, CliError>,
    sources: &Sources,
    lock: Option<&MurmurLock>,
    store: &LocalRegistry,
    lock_path: &Path,
) -> Result<FetchOutcome, CliError> {
    match held? {
        Held::Project(held) => {
            let pin = lock_pin(&held.meta, &held.sha256);
            check_lock_conflict(lock_path, name.as_str(), version, &pin)?;
            Ok(FetchOutcome {
                bytes_len: held.bytes.len() as u64,
                lock_upsert: Some((
                    name.to_string(),
                    version.to_string(),
                    pin,
                    held.meta.owner_id,
                )),
                payload: held.bytes,
            })
        }
        Held::Global(resolved) => {
            let pin = lock_pin(&resolved.meta, &resolved.sha256);
            check_lock_conflict(lock_path, name.as_str(), version, &pin)?;
            let len = resolved.bytes.len() as u64;
            let owner_id = resolved.meta.owner_id;
            store
                .store_installed_overwrite(resolved.meta, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            Ok(FetchOutcome {
                bytes_len: len,
                lock_upsert: Some((name.to_string(), version.to_string(), pin, owner_id)),
                payload: resolved.bytes,
            })
        }
        Held::Source => {
            let source = sources.source_for(name, Some(version))?;
            let resolved = resolve(
                &*source,
                name,
                Some(version),
                ExplicitVersion::FitChecked,
                current_platform(),
                lock,
            )
            .map_err(CliError::from)?;
            let meta = source_meta(
                name,
                version,
                &resolved.manifest,
                resolved.platform.as_deref(),
                resolved.owner_id,
            );
            let pin = lock_pin(&meta, &resolved.sha256);
            store
                .store_installed_overwrite(meta, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            Ok(FetchOutcome {
                bytes_len: resolved.bytes.len() as u64,
                lock_upsert: Some((
                    name.to_string(),
                    version.to_string(),
                    pin,
                    Some(resolved.owner_id),
                )),
                payload: resolved.bytes,
            })
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
    precompiler: Option<&Precompiler>,
) -> Result<(), CliError> {
    let bytes = std::fs::read(path_str)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read {path_str}: {e}")))?;

    let manifest = load_manifest_from_artifact_bytes(&bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("{path_str} is not a valid .mur.zip: {e}")))?;

    let runtime = manifest.registry_runtime();
    let platform = if runtime == RuntimeType::Native {
        Some(local_file_platform(path_str)?)
    } else {
        None
    };

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
        runtime,
        artifact_runtime: manifest.runtime.clone(),
        platforms: recorded_platforms(
            name.as_str(),
            &manifest.version,
            runtime,
            platform.as_deref(),
        ),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
        owner_id: None,
        built_at_level: None,
    };
    let pin = lock_pin(&meta, &sha256);
    store
        .store_installed_overwrite(meta, &bytes, &sha256)
        .map_err(CliError::from)?;
    if let Some(lock_path) = lock_path {
        if let Some(adoption) =
            upsert_lock_entry(lock_path, name.as_str(), &manifest.version, pin, None)?
        {
            capsule_runtime::report_println!("{adoption}");
        }
    }

    capsule_runtime::report_println!("Installed {name}@{} from {path_str}", manifest.version);
    precompile(precompiler, name.as_str(), &manifest.version, &bytes);
    Ok(())
}

/// Which platform a native payload installed from a local file is for.
///
/// The file name is asked first — a `{name}-{version}-{platform}.mur.zip` built for another
/// platform must file under that platform, not under this host's. Only a name carrying no
/// recognised tag falls back to auto-detection, the same detection and the same printed line
/// `mur publish` uses. A host outside [`SUPPORTED_PLATFORMS`] has nothing to detect, so the
/// install is refused rather than filed under a platform nobody can resolve.
fn local_file_platform(path_str: &str) -> Result<String, CliError> {
    let file_name = Path::new(path_str)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path_str);

    if let Some((_, platform)) = split_platform_suffix(file_name) {
        return Ok(platform.to_string());
    }

    if current_platform() == "unknown" {
        return Err(CliError::with_hint(
            E_IO_003,
            format!(
                "cannot determine which platform the native artifact in {path_str} is for: its \
                 file name carries no platform tag and this host is not one of {}",
                SUPPORTED_PLATFORMS.join(", ")
            ),
            "install from a source that publishes platform-tagged assets, or rename the file to \
             <name>-<version>-<platform>.mur.zip",
        ));
    }

    capsule_runtime::report_println!("Platform: {} (auto-detected)", current_platform());
    Ok(current_platform().to_string())
}

fn run_install_all_platforms(
    parsed: Option<InstallRef>,
    no_precompile: bool,
) -> Result<(), CliError> {
    let (name, version) = match parsed {
        None => {
            return Err(CliError::new(
                E_IO_003,
                "--all-platforms requires an artifact reference (scope/name@version)",
            ))
        }
        Some(InstallRef::Name {
            name,
            version: Some(version),
        }) => (name, version),
        Some(_) => {
            return Err(CliError::new(
                E_IO_003,
                "artifact reference must be in the format <scope/name@version>",
            ))
        }
    };
    let version = version.as_str();
    let sources = Sources::load()?;
    let source = sources.source_for(&name, Some(version))?;

    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    for platform in ALL_PLATFORMS {
        let resolved = match resolve(
            &*source,
            &name,
            Some(version),
            ExplicitVersion::FitChecked,
            platform,
            None,
        ) {
            Ok(resolved) => resolved,
            Err(error) if matches!(*error, ResolveError::NoPayloadFor { .. }) => {
                capsule_runtime::report_eprintln!(
                    "warning: could not install {name}@{version} for {platform}: {error}"
                );
                continue;
            }
            Err(error) => return Err(CliError::from(error)),
        };
        // The loop's target platform, not the asset's own tag: this arm asked for one platform's
        // payload, and an untagged asset answers for every platform.
        let meta = source_meta(
            &name,
            version,
            &resolved.manifest,
            Some(platform),
            resolved.owner_id,
        );
        // Through the same write every other install path uses, so the store never holds a
        // payload with no metadata beside it — without a sidecar a native payload reads back as
        // `runtime: wasm`.
        global_registry
            .store_installed_overwrite(meta, &resolved.bytes, &resolved.sha256)
            .map_err(CliError::from)?;
        capsule_runtime::report_println!(
            "Installed {name}@{version} ({platform}) → ~/.murmur/artifacts/{}/{}/{version}/{}-{version}-{platform}.mur.zip",
            name.scope(),
            name.name(),
            name.name()
        );
        // Only this host's payload: a form is native code for the machine that compiled
        // it, so another platform's payload has nothing to gain from one here.
        if *platform == current_platform() {
            precompile(
                build_precompiler(no_precompile, None).as_ref(),
                name.as_str(),
                version,
                &resolved.bytes,
            );
        }
    }

    Ok(())
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
    use std::io::Write;

    use tempfile::tempdir;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn native_zip(name: &str, version: &str) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let opts = SimpleFileOptions::default();
            zip.start_file("murmur.yaml", opts).unwrap();
            write!(
                zip,
                "name: {name}\nversion: {version}\nruntime: tool\nimplementation: native\n"
            )
            .unwrap();
            zip.start_file(format!("bin/{name}"), opts).unwrap();
            zip.write_all(b"binary").unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    }

    /// A native `.mur.zip` installed from a file records the packaging type it declares.
    #[test]
    fn a_local_file_install_records_the_declared_packaging_type() {
        let bytes = native_zip("my-tool", "1.0.0");

        // The local-file path reads the platform off the file name, so the file is named the
        // way a published platform-tagged asset is.
        let platform = murmur_artifact::SUPPORTED_PLATFORMS[0];
        let source_dir = tempdir().unwrap();
        let file = source_dir
            .path()
            .join(format!("my-tool-1.0.0-{platform}.mur.zip"));
        std::fs::write(&file, &bytes).unwrap();

        let store_dir = tempdir().unwrap();
        let store = murmur_artifact::LocalRegistry::new(store_dir.path());
        super::install_from_local_file(file.to_str().unwrap(), &store, None, None).unwrap();

        let name = murmur_artifact::ArtifactName::parse("local/my-tool").unwrap();
        let meta_path = store.metadata_path_for(&name, "1.0.0", Some(platform));
        let recorded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
        assert_eq!(recorded["meta"]["runtime"], "native");
        assert_eq!(recorded["meta"]["artifact_runtime"], "tool");
    }
}
