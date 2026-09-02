use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use murmur_artifact::{
    load_manifest_from_artifact_bytes, load_runtime_manifest, read_lockfile, resolve_manifest_path,
    sha256_hex, verify_sha256, write_lockfile_atomic, ArtifactMeta, ArtifactRef, LocalRegistry,
    LockedArtifact, LockedSha256, LockfileError, MurmurLock, RegistryError, LOCK_VERSION,
    MANIFEST_FILENAME,
};
use rayon::prelude::*;

use crate::{
    config::{
        load_effective_mur_config, load_effective_mur_config_if_any_exists, resolve_registry,
    },
    error::{CliError, E_IO_001, E_IO_003, E_REG_001, E_REG_005},
    source::{SourceChain, SourceChainError},
};

/// Compare a registry-resolved artifact against any existing `murmur.lock` pin at
/// `lock_path`, refusing to install when they disagree. A missing lockfile is not a
/// conflict — the artifact simply isn't pinned yet.
fn check_lock_conflict(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: &str,
) -> Result<(), CliError> {
    match read_lockfile(lock_path) {
        Ok(lock) => {
            if let Some(entry) = lock.artifact_for(name) {
                if entry.resolved_version != version || entry.sha256.wasm != sha256 {
                    return Err(CliError::with_hint(
                        E_REG_005,
                        format!(
                            "murmur.lock conflict for '{name}': pinned {}@{} (sha256 {}), but \
                             the registry now resolves {name}@{version} (sha256 {sha256})",
                            entry.name, entry.resolved_version, entry.sha256.wasm
                        ),
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

/// Upsert a single artifact's entry into `murmur.lock` at `lock_path`, creating the lockfile
/// if it doesn't exist yet and preserving every other pre-existing entry.
fn upsert_lock_entry(
    lock_path: &Path,
    name: &str,
    version: &str,
    sha256: &str,
) -> Result<(), CliError> {
    let mut lock = match read_lockfile(lock_path) {
        Ok(lock) => lock,
        Err(LockfileError::NotFound(_)) => MurmurLock {
            lock_version: LOCK_VERSION,
            artifacts: Vec::new(),
        },
        Err(err) => return Err(super::lockfile_error_to_cli(err)),
    };

    if let Some(entry) = lock.artifacts.iter_mut().find(|entry| entry.name == name) {
        entry.resolved_version = version.to_string();
        entry.sha256 = LockedSha256 {
            wasm: sha256.to_string(),
        };
    } else {
        lock.artifacts.push(LockedArtifact {
            name: name.to_string(),
            resolved_version: version.to_string(),
            sha256: LockedSha256 {
                wasm: sha256.to_string(),
            },
        });
    }

    write_lockfile_atomic(lock_path, &lock).map_err(super::lockfile_error_to_cli)
}

// ─── source-chain helpers ─────────────────────────────────────────────────────

pub(crate) fn install_resolved(
    local_registry: &LocalRegistry,
    resolved: crate::source::ResolvedSource,
    lock_path: Option<&Path>,
) -> Result<(), CliError> {
    use murmur_artifact::load_manifest_from_artifact_bytes;
    let resolved_version_hint = resolved.resolved_version.clone();
    let manifest = load_manifest_from_artifact_bytes(&resolved.bytes).map_err(|error| {
        CliError::new(
            E_IO_003,
            format!(
                "resolved bytes from {} are not a valid .mur.zip artifact: {error}",
                resolved.source
            ),
        )
    })?;
    let installed_version = if manifest.version.trim().is_empty() {
        resolved_version_hint.unwrap_or_else(|| "unknown".to_string())
    } else {
        manifest.version.clone()
    };
    let sha256 = sha256_hex(&resolved.bytes);
    local_registry
        .store_installed_overwrite(
            ArtifactMeta {
                name: manifest.name.clone(),
                version: installed_version.clone(),
                runtime: manifest.registry_runtime(),
                artifact_runtime: manifest.runtime.clone(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &resolved.bytes,
            &sha256,
        )
        .map_err(CliError::from)?;
    // Source-chain resolutions must still pin the lockfile; without this a github-sourced
    // install stores the artifact but leaves murmur.lock without an entry, so `mur run`
    // later fails with E-RUN-003.
    if let Some(lock_path) = lock_path {
        upsert_lock_entry(lock_path, &manifest.name, &installed_version, &sha256)?;
    }
    println!(
        "Installed {}@{} from {}",
        manifest.name, installed_version, resolved.source
    );
    Ok(())
}

pub(crate) fn source_chain_error_to_cli(target: &str, error: SourceChainError) -> CliError {
    match error {
        SourceChainError::NotFound { attempts, .. } => {
            let mut message = format!("could not resolve '{target}'");
            for attempt in attempts {
                message.push_str(&format!("\n  {} — {}", attempt.source, attempt.reason));
            }
            message.push_str("\n  hint: run `mur doctor` to check your source configuration");
            CliError::new(E_REG_001, message)
        }
        SourceChainError::SourceFailure(message) => CliError::new(E_IO_003, message),
    }
}

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
    registry_override: Option<&str>,
    global: bool,
    all_platforms: bool,
) -> Result<(), CliError> {
    if all_platforms {
        return run_install_all_platforms(artifact_ref);
    }

    let (store, project_root) = determine_store(global)?;

    match artifact_ref {
        None => install_manifest_deps(&store, &project_root, registry_override),
        Some(ref_str) => {
            install_single(ref_str, registry_override, &store, project_root.as_deref())
        }
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

/// Returns (store, project_root). project_root is None when global=true.
fn determine_store(global: bool) -> Result<(LocalRegistry, Option<PathBuf>), CliError> {
    if global {
        let store = LocalRegistry::from_default_home().map_err(CliError::from)?;
        Ok((store, None))
    } else {
        let project_root = find_project_root()?;
        let store_path = project_root.join(".murmur").join("artifacts");
        std::fs::create_dir_all(&store_path).map_err(|e| {
            CliError::new(
                E_IO_003,
                format!(
                    "failed to create project store at {}: {e}",
                    store_path.display()
                ),
            )
        })?;
        Ok((LocalRegistry::new(store_path), Some(project_root)))
    }
}

fn install_single(
    artifact_ref: &str,
    registry_override: Option<&str>,
    store: &LocalRegistry,
    project_root: Option<&Path>,
) -> Result<(), CliError> {
    if is_local_path(artifact_ref) {
        return install_from_local_file(artifact_ref, store);
    }

    if is_source_chain_ref(artifact_ref) {
        let config = load_effective_mur_config()?;
        let chain = SourceChain::from_config(&config);
        let parsed =
            ArtifactRef::parse(artifact_ref).map_err(|e| CliError::new(E_IO_003, e.to_string()))?;
        let resolved_list = match &parsed {
            ArtifactRef::BareName(name) => vec![chain
                .resolve_bare(name, None)
                .map_err(|e| source_chain_error_to_cli(artifact_ref, e))?],
            ArtifactRef::GitHub { owner, repo, tag } => chain
                .resolve_github_all(owner, repo, tag)
                .map_err(|e| source_chain_error_to_cli(artifact_ref, e))?,
        };
        let lock_path = project_root.map(|r| r.join("murmur.lock"));
        for resolved in resolved_list {
            install_resolved(store, resolved, lock_path.as_deref())?;
        }
        return Ok(());
    }

    // name@version — registry first, source chain fallback on NotFound
    let (name, version) = parse_versioned_ref(artifact_ref)?;
    let registry = resolve_registry(registry_override)?;
    match registry.resolve(name, version) {
        Ok(resolved) => {
            verify_sha256(name, version, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            if let Some(root) = project_root {
                check_lock_conflict(&root.join("murmur.lock"), name, version, &resolved.sha256)?;
            }
            store
                .store_installed_overwrite(resolved.meta, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            if let Some(root) = project_root {
                upsert_lock_entry(&root.join("murmur.lock"), name, version, &resolved.sha256)?;
            }
        }
        Err(RegistryError::NotFound { .. }) => {
            let source_chain = match load_effective_mur_config_if_any_exists()? {
                Some(config) => {
                    let chain = SourceChain::from_config(&config);
                    if chain.is_empty() {
                        None
                    } else {
                        Some(chain)
                    }
                }
                None => None,
            };
            match source_chain {
                Some(chain) => {
                    let resolved = chain
                        .resolve_bare(name, Some(version))
                        .map_err(|e| source_chain_error_to_cli(name, e))?;
                    let lock_path = project_root.map(|r| r.join("murmur.lock"));
                    install_resolved(store, resolved, lock_path.as_deref())?;
                    return Ok(());
                }
                None => {
                    return Err(CliError::with_hint(
                        E_REG_001,
                        format!("artifact {name}@{version} not found in registry"),
                        "configure a registry.sources entry in ~/.murmur/config.yaml",
                    ));
                }
            }
        }
        Err(e) => return Err(CliError::from(e)),
    }

    let display = store_display(store);
    println!("Installed {name}@{version} → {display}/{name}/{version}/{name}-{version}.mur.zip");
    Ok(())
}

fn install_manifest_deps(
    store: &LocalRegistry,
    project_root: &Option<PathBuf>,
    registry_override: Option<&str>,
) -> Result<(), CliError> {
    let root = match project_root {
        Some(r) => r.clone(),
        None => find_project_root()?,
    };
    let lock_path = root.join("murmur.lock");

    let manifest_path = resolve_manifest_path(&root);
    let runtime_manifest = load_runtime_manifest(&manifest_path)
        .map_err(|err| CliError::new(E_IO_003, format!("failed to load manifest: {err:?}")))?;

    // Collect only the artifacts we need to install (skip inline-source ones).
    let artifacts: Vec<_> = runtime_manifest
        .artifacts
        .iter()
        .filter(|a| a.source.is_none())
        .collect();

    if artifacts.is_empty() {
        println!("No artifacts in manifest.");
        return Ok(());
    }

    let registry = resolve_registry(registry_override)?;
    let source_chain = match load_effective_mur_config_if_any_exists()? {
        Some(config) => {
            let chain = SourceChain::from_config(&config);
            if chain.is_empty() {
                None
            } else {
                Some(chain)
            }
        }
        None => None,
    };

    // Pre-check which artifacts are already in the project store.
    let cached_flags: Vec<bool> = artifacts
        .iter()
        .map(|a| store.artifact_path_for(&a.name, &a.version).exists())
        .collect();

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

    let results: Vec<Result<FetchOutcome, CliError>> = artifacts
        .par_iter()
        .enumerate()
        .map(|(i, artifact)| {
            let pb = &spinners[i];
            let is_cached = cached_flags[i];
            let use_download_bar = !is_cached && source_chain.is_some();

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
                artifact.name.as_str(),
                artifact.version.as_str(),
                &*registry,
                source_chain.as_ref(),
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
    for (_, outcome) in &successes {
        if let Some((name, version, sha256)) = &outcome.lock_upsert {
            upsert_lock_entry(&lock_path, name, version, sha256)?;
        }
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
        return Ok(());
    }

    abandon_step(
        &header_pb,
        &done_style,
        format!("{} ↓ artifacts  failed", style("✗").red().bold()),
    );

    // Plain `println!` rather than indicatif bar text: MultiProgress writes nothing at all
    // when stdout/stderr is not a terminal, so bar messages are cosmetic-only and would make
    // the failure report invisible in CI logs and pipes.
    println!();
    println!(
        "{} of {} artifact{} failed to install:",
        failures.len(),
        n,
        s(n)
    );
    for (i, err) in &failures {
        let artifact = artifacts[*i];
        println!();
        println!("  {}@{}", artifact.name, artifact.version);
        // CliError's Display renders `error[CODE]: message` plus its hint line; keep every
        // failure's full text, indented under the artifact that produced it.
        for line in err.to_string().lines() {
            println!("    {line}");
        }
    }
    if !successes.is_empty() {
        println!();
        println!("installed {summary_tail}");
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
/// plus the `(name, resolved_version, sha256)` to upsert into `murmur.lock` once every
/// parallel fetch has completed. Set for both registry-resolved and source-chain-resolved
/// artifacts so every install path pins the lockfile.
struct FetchOutcome {
    bytes_len: u64,
    lock_upsert: Option<(String, String, String)>,
}

/// Fetch one artifact from the registry (or source chain fallback) and write it
/// to the store.
fn fetch_and_store(
    name: &str,
    version: &str,
    registry: &dyn murmur_artifact::Registry,
    source_chain: Option<&SourceChain>,
    store: &LocalRegistry,
    lock_path: &Path,
) -> Result<FetchOutcome, CliError> {
    match registry.resolve(name, version) {
        Ok(resolved) => {
            verify_sha256(name, version, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            check_lock_conflict(lock_path, name, version, &resolved.sha256)?;
            let len = resolved.bytes.len() as u64;
            store
                .store_installed_overwrite(resolved.meta, &resolved.bytes, &resolved.sha256)
                .map_err(CliError::from)?;
            Ok(FetchOutcome {
                bytes_len: len,
                lock_upsert: Some((name.to_string(), version.to_string(), resolved.sha256)),
            })
        }
        Err(RegistryError::NotFound { .. }) => match source_chain {
            Some(chain) => {
                let resolved = chain
                    .resolve_bare(name, Some(version))
                    .map_err(|e| source_chain_error_to_cli(name, e))?;
                let sha256 = sha256_hex(&resolved.bytes);
                let len = resolved.bytes.len() as u64;
                let inner = load_manifest_from_artifact_bytes(&resolved.bytes).map_err(|e| {
                    CliError::new(E_IO_003, format!("failed to parse artifact {name}: {e}"))
                })?;
                if !inner.version.trim().is_empty() && inner.version.trim() != version {
                    return Err(CliError::new(
                        E_IO_003,
                        format!(
                            "artifact {name}@{version}: downloaded zip reports internal version '{}' — \
                             the release was likely built before the version bump was committed; \
                             bump the artifact version in artifacts.toml, run apply-versions.sh, commit, then re-push the tag",
                            inner.version
                        ),
                    ));
                }
                store
                    .store_installed_overwrite(
                        ArtifactMeta {
                            name: inner.name.clone(),
                            version: version.to_string(),
                            runtime: inner.registry_runtime(),
                            artifact_runtime: inner.runtime.clone(),
                            platforms: Vec::new(),
                            description: None,
                            tags: Vec::new(),
                            wit_contracts: None,
                        },
                        &resolved.bytes,
                        &sha256,
                    )
                    .map_err(CliError::from)?;
                // Pin the source-chain resolution in murmur.lock too — otherwise the
                // artifact stores but `mur run` fails with E-RUN-003 (missing lock entry).
                Ok(FetchOutcome {
                    bytes_len: len,
                    lock_upsert: Some((name.to_string(), version.to_string(), sha256)),
                })
            }
            None => Err(CliError::with_hint(
                E_REG_001,
                format!("artifact {name}@{version} not found in registry"),
                "configure a registry.sources entry in ~/.murmur/config.yaml",
            )),
        },
        Err(e) => Err(CliError::from(e)),
    }
}

fn install_from_local_file(path_str: &str, store: &LocalRegistry) -> Result<(), CliError> {
    let bytes = std::fs::read(path_str)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to read {path_str}: {e}")))?;

    let manifest = load_manifest_from_artifact_bytes(&bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("{path_str} is not a valid .mur.zip: {e}")))?;

    let sha256 = sha256_hex(&bytes);
    store
        .store_installed_overwrite(
            ArtifactMeta {
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                runtime: manifest.registry_runtime(),
                artifact_runtime: manifest.runtime.clone(),
                platforms: Vec::new(),
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
            },
            &bytes,
            &sha256,
        )
        .map_err(CliError::from)?;

    println!(
        "Installed {}@{} from {}",
        manifest.name, manifest.version, path_str
    );
    Ok(())
}

fn run_install_all_platforms(artifact_ref: Option<&str>) -> Result<(), CliError> {
    let Some(ref_str) = artifact_ref else {
        return Err(CliError::new(
            E_IO_003,
            "--all-platforms requires an artifact reference (name@version)",
        ));
    };

    let (name, version) = parse_versioned_ref(ref_str)?;

    let config = load_effective_mur_config()?;
    let chain = SourceChain::from_config(&config);
    if chain.is_empty() {
        return Err(CliError::with_hint(
            E_REG_001,
            "--all-platforms requires a configured source chain",
            "add a registry.sources entry to ~/.murmur/config.yaml",
        ));
    }

    let global_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;

    for platform in ALL_PLATFORMS {
        match chain.resolve_bare_for_platform(name, version, platform) {
            Ok(bytes) => {
                let sha256 = sha256_hex(&bytes);
                let artifact_path =
                    global_registry.artifact_path_for_platform(name, version, platform);
                let artifact_dir = artifact_path.parent().ok_or_else(|| {
                    CliError::new(E_IO_003, "unexpected artifact path (no parent)")
                })?;
                std::fs::create_dir_all(artifact_dir).map_err(|e| {
                    CliError::new(E_IO_003, format!("failed to create artifact dir: {e}"))
                })?;
                std::fs::write(&artifact_path, &bytes).map_err(|e| {
                    CliError::new(E_IO_003, format!("failed to write artifact: {e}"))
                })?;
                let sha256_path = artifact_dir.join(format!("{name}-{version}-{platform}.sha256"));
                std::fs::write(&sha256_path, sha256.as_bytes())
                    .map_err(|e| CliError::new(E_IO_003, format!("failed to write sha256: {e}")))?;
                println!(
                    "Installed {name}@{version} ({platform}) → ~/.murmur/artifacts/{name}/{version}/{name}-{version}-{platform}.mur.zip"
                );
            }
            Err(e) => {
                eprintln!(
                    "warning: could not install {name}@{version} for {platform}: {}",
                    source_chain_err_display(&e)
                );
            }
        }
    }

    Ok(())
}

fn source_chain_err_display(e: &SourceChainError) -> String {
    match e {
        SourceChainError::NotFound { target, attempts } => {
            let mut msg = format!("could not resolve '{target}'");
            for a in attempts {
                msg.push_str(&format!("\n  {} — {}", a.source, a.reason));
            }
            msg
        }
        SourceChainError::SourceFailure(m) => m.clone(),
    }
}

fn is_local_path(s: &str) -> bool {
    s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || (s.contains('/') && s.ends_with(".mur.zip"))
}

fn is_source_chain_ref(s: &str) -> bool {
    // github: prefix or bare name (no @)
    s.contains(':') || !s.contains('@')
}

pub(crate) fn parse_versioned_ref(input: &str) -> Result<(&str, &str), CliError> {
    let mut segments = input.split('@');
    let name = segments.next().unwrap_or_default();
    let version = segments.next().unwrap_or_default();

    if name.is_empty() || version.is_empty() || segments.next().is_some() {
        return Err(CliError::new(
            E_IO_003,
            "artifact reference must be in the format <name@version>",
        ));
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
