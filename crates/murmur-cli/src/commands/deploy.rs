use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use chrono::Utc;
use console::{measure_text_width, style};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use murmur_artifact::{load_runtime_manifest, LocalRegistry, RuntimeManifest, MANIFEST_FILENAME};
use rayon::prelude::*;
use uuid::Uuid;

use crate::{
    config::load_mur_config,
    error::{CliError, E_IO_001, E_IO_003},
    source::SourceChain,
};

use super::deploy_state::{append_deployment, DeploymentRecord};

// ─── error codes ─────────────────────────────────────────────────────────────

const E_DEPLOY_003: &str = "E-DEPLOY-003";
const E_DEPLOY_004: &str = "E-DEPLOY-004";
const E_DEPLOY_006: &str = "E-DEPLOY-006";

// ─── artifact staging for deploy ─────────────────────────────────────────────

/// An artifact that has been resolved locally or fetched from the source chain,
/// ready to upload to the remote VM.
#[derive(Debug)]
pub(crate) struct StagedArtifact {
    pub name: String,
    pub version: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

/// Resolve an artifact for `target_platform`, either from the local registry or by
/// fetching from the source chain.
///
/// Step A: check local registry for a platform-specific or WASM generic file.
/// Step B: if Step A misses, fetch from the source chain with the target platform.
/// Fetched artifacts are cached at the platform-specific local path.
pub(crate) fn ensure_artifact_for_deploy(
    name: &str,
    version: &str,
    target_platform: &str,
    local_registry: &LocalRegistry,
    chain: &crate::source::SourceChain,
) -> Result<StagedArtifact, crate::error::CliError> {
    use crate::error::{E_IO_003, E_REG_001};
    use murmur_artifact::{current_platform, Registry, RegistryError, RuntimeType};

    let platform_specific_path =
        local_registry.artifact_path_for_platform(name, version, target_platform);

    match local_registry.resolve_with_platform(name, version, Some(target_platform)) {
        Ok(resolved) => {
            let is_same_platform = target_platform == current_platform();
            let is_native = resolved.meta.runtime == RuntimeType::Native;

            if is_native && !is_same_platform && !platform_specific_path.exists() {
                // Different platform native — fall through to Step B to fetch the target variant
            } else {
                return Ok(StagedArtifact {
                    name: name.to_string(),
                    version: version.to_string(),
                    bytes: resolved.bytes.to_vec(),
                    sha256: resolved.sha256,
                });
            }
        }
        Err(RegistryError::NotFound { .. }) => {}
        Err(e) => return Err(crate::error::CliError::from(e)),
    }

    // Step B — fetch from source chain with explicit target platform
    match chain.resolve_bare_for_platform(name, version, target_platform) {
        Ok(bytes) => {
            let sha256 = murmur_artifact::sha256_hex(&bytes);

            let artifact_dir = platform_specific_path.parent().ok_or_else(|| {
                crate::error::CliError::new(
                    E_IO_003,
                    format!(
                        "unexpected artifact path (no parent): {}",
                        platform_specific_path.display()
                    ),
                )
            })?;
            let sha256_path =
                artifact_dir.join(format!("{name}-{version}-{target_platform}.sha256"));

            std::fs::create_dir_all(artifact_dir).map_err(|e| {
                crate::error::CliError::new(
                    E_IO_003,
                    format!("failed to create artifact cache dir: {e}"),
                )
            })?;
            std::fs::write(&platform_specific_path, &bytes).map_err(|e| {
                crate::error::CliError::new(E_IO_003, format!("failed to cache artifact: {e}"))
            })?;
            std::fs::write(&sha256_path, sha256.as_bytes()).map_err(|e| {
                crate::error::CliError::new(
                    E_IO_003,
                    format!("failed to cache artifact sha256: {e}"),
                )
            })?;

            Ok(StagedArtifact {
                name: name.to_string(),
                version: version.to_string(),
                bytes,
                sha256,
            })
        }
        Err(chain_err) => Err(crate::error::CliError::new(
            E_REG_001,
            format!(
                "artifact {name}@{version} not found locally or in any source\n  \
                 source chain: {chain_err}\n  \
                 hint: check your source config or run `mur doctor`"
            ),
        )),
    }
}

// ─── staging cleanup guard ────────────────────────────────────────────────────

struct StagingGuard(std::path::PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ─── display helpers ──────────────────────────────────────────────────────────

fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.0} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} B")
    }
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        let bits = ((chunk[0] as u32) << 16) | ((second as u32) << 8) | third as u32;
        let encode = |shift: u32| ALPHABET[((bits >> shift) & 63) as usize] as char;
        output.extend([
            encode(18),
            encode(12),
            if chunk.len() > 1 { encode(6) } else { '=' },
            if chunk.len() > 2 { encode(0) } else { '=' },
        ]);
    }
    output
}

// ─── SSH helpers ──────────────────────────────────────────────────────────────

fn ssh_args_base(key_path: Option<&str>, user: &str, ip: &str) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "StrictHostKeyChecking=no".into(),
        "-o".into(),
        "UserKnownHostsFile=/dev/null".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
    ];
    if let Some(key) = key_path {
        args.push("-i".into());
        args.push(key.to_string());
    }
    args.push(format!("{user}@{ip}"));
    args
}

fn wait_for_ssh(
    ip: &str,
    key_path: Option<&str>,
    user: &str,
    timeout: Duration,
) -> Result<(), CliError> {
    let start = Instant::now();
    let mut delay = Duration::from_secs(2);

    loop {
        let result = Command::new("ssh")
            .args(ssh_args_base(key_path, user, ip))
            .arg("true")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();

        match result {
            Ok(o) if o.status.success() => return Ok(()),
            _ => {}
        }

        if start.elapsed() >= timeout {
            return Err(CliError::new(
                E_DEPLOY_003,
                format!("SSH not available on {ip} after {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}

fn ssh_exec(
    ip: &str,
    key_path: Option<&str>,
    user: &str,
    command: &str,
) -> Result<String, CliError> {
    let mut args = ssh_args_base(key_path, user, ip);
    args.push(command.to_string());

    let output = Command::new("ssh")
        .args(&args)
        .output()
        .map_err(|e| CliError::new(E_IO_003, format!("ssh exec: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::new(
            E_DEPLOY_003,
            format!("SSH command failed (exit {}): {stderr}", output.status),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn scp_upload(
    ip: &str,
    key_path: Option<&str>,
    user: &str,
    local: &str,
    remote: &str,
    recursive: bool,
) -> Result<(), CliError> {
    let mut args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=no".to_string(),
        "-o".to_string(),
        "UserKnownHostsFile=/dev/null".to_string(),
        "-o".to_string(),
        "LogLevel=ERROR".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ];
    if let Some(key) = key_path {
        args.push("-i".to_string());
        args.push(key.to_string());
    }
    if recursive {
        args.push("-r".to_string());
    }
    args.push(local.to_string());
    args.push(format!("{user}@{ip}:{remote}"));

    let output = Command::new("scp")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CliError::new(E_IO_003, format!("scp: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::new(
            E_DEPLOY_003,
            format!(
                "scp upload to {user}@{ip}:{remote} failed: {}",
                stderr.trim()
            ),
        ));
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}

// ─── artifact helpers ─────────────────────────────────────────────────────────

/// Collect (name, version) pairs for every artifact in the manifest.
///
/// `RuntimeManifest.artifacts` is `Vec<RuntimeArtifact> { name, version, runtime }`.
/// `InferenceDriver.artifact` is a bare name string (no version). Drivers that appear in
/// `manifest.artifacts` are already collected; drivers only in `inference.driver` (no version
/// available) are not stageable and are omitted here.
fn collect_deploy_artifacts(manifest: &RuntimeManifest) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for a in &manifest.artifacts {
        let key = (a.name.clone(), a.version.clone());
        if seen.insert(key.clone()) {
            result.push(key);
        }
    }
    result
}

/// Returns `(local_absolute_path, remote_filename)` for every file that is
/// referenced by path in the manifest and must be uploaded alongside it.
///
/// Fields checked in `RuntimeManifest`:
///   - `inference.system_prompt_file` — path to a local file for the system prompt
///   - `inference.compaction.system_prompt_file` — path to a local file for the
///     compaction system prompt
///
/// Fields NOT checked (not local file paths):
///   - `inference.system_prompt` — inline string content
///   - `inference.compaction.system_prompt` — inline string content
///   - `inference.api_key` — literal value or `${ENV_VAR}` reference
///   - `inference.driver.config` — inline JSON object
///   - `observability.otel_endpoint` — HTTP endpoint URL
///   - All other fields — scalars, version strings, or nested configs
///
/// All paths are resolved relative to `manifest_dir`. Absolute paths are used as-is.
fn collect_manifest_files(
    manifest: &RuntimeManifest,
    manifest_dir: &Path,
) -> Result<Vec<(std::path::PathBuf, String)>, CliError> {
    let mut files = Vec::new();

    if let Some(ref inference) = manifest.inference {
        if let Some(ref spf) = inference.system_prompt_file {
            files.push(resolve_manifest_file(
                "inference.system_prompt_file",
                spf,
                manifest_dir,
            )?);
        }

        if let Some(spf) = inference
            .compaction
            .as_ref()
            .and_then(|c| c.system_prompt_file.as_ref())
        {
            files.push(resolve_manifest_file(
                "inference.compaction.system_prompt_file",
                spf,
                manifest_dir,
            )?);
        }
    }

    Ok(files)
}

/// Resolves one manifest-referenced path into the `(local_absolute_path, remote_filename)`
/// pair [`collect_manifest_files`] uploads, erroring if the file is missing or the path has
/// no filename component. `field` is the dotted manifest field name, used verbatim in both
/// error messages so an author can tell which reference failed.
fn resolve_manifest_file(
    field: &str,
    raw_path: &str,
    manifest_dir: &Path,
) -> Result<(std::path::PathBuf, String), CliError> {
    let local_path = if std::path::Path::new(raw_path).is_absolute() {
        std::path::PathBuf::from(raw_path)
    } else {
        manifest_dir.join(raw_path)
    };

    std::fs::metadata(&local_path).map_err(|_| {
        CliError::new(
            E_IO_001,
            format!(
                "manifest references {field} at {} but the file does not exist",
                local_path.display()
            ),
        )
    })?;

    let filename = local_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            CliError::new(
                E_IO_001,
                format!("{field} path has no filename: {}", local_path.display()),
            )
        })?
        .to_string();

    Ok((local_path, filename))
}

/// Inner implementation of mur binary resolution; takes an explicit home directory so
/// unit tests can exercise cache-hit logic without mutating process environment variables.
/// `dl_progress`: optional ProgressBar to show download progress (pass `None` on cache hit
/// or in tests).
fn resolve_mur_binary_impl(
    explicit: Option<&Path>,
    manifest_version: Option<&str>,
    platform: &str,
    home: &Path,
    dl_progress: Option<&ProgressBar>,
) -> Result<std::path::PathBuf, CliError> {
    // Honour an explicitly supplied binary first.
    if let Some(p) = explicit {
        if !p.exists() {
            return Err(CliError::new(
                E_IO_001,
                format!("--mur-binary not found: {}", p.display()),
            ));
        }
        return Ok(p.to_path_buf());
    }

    // Version priority: manifest.mur_version > running binary version.
    let version = manifest_version.unwrap_or(env!("CARGO_PKG_VERSION"));

    let cache_dir = home.join(".murmur").join("bin");
    let cache_path = cache_dir.join(format!("mur-{version}-{platform}"));

    // Return cached copy if it exists and is non-empty.
    if cache_path.exists() && cache_path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(cache_path);
    }

    let client = crate::registry_client::blocking_agent(std::time::Duration::from_secs(60));

    // Read GitHub token from the environment so private-repo release assets can be
    // fetched.  GITHUB_TOKEN is the standard name set by GitHub Actions and gh-auth;
    // GH_TOKEN is the gh CLI convention.
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty());

    // Must use the GitHub API asset endpoint, NOT the direct browser_download_url.
    // For private repos, GitHub redirects browser_download_url to a CDN and strips the
    // Bearer token on the cross-host redirect, causing 404.  The API endpoint with
    // Accept: application/octet-stream handles auth correctly (same pattern used by
    // GitHubSource::download_asset for artifact downloads).
    //
    // Step 1: fetch release JSON to find the asset ID.
    let api_base = std::env::var("MUR_GITHUB_API_BASE")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string());

    let release_url = format!("{api_base}/repos/murmur-nexus/murmur/releases/tags/v{version}");
    let asset_name = format!("mur-{version}-{platform}");

    let mut release_req = client
        .get(&release_url)
        .header("User-Agent", "murmur-cli")
        .header("Accept", "application/vnd.github+json");
    if let Some(t) = &token {
        release_req = release_req.header("Authorization", format!("Bearer {t}"));
    }

    let mut release_resp = release_req.call().map_err(|e| {
        CliError::new(
            E_DEPLOY_006,
            format!(
                "could not fetch release v{version} from GitHub: {e}\n  \
                 Build from source or pass --mur-binary <path> to provide one manually."
            ),
        )
    })?;

    if !release_resp.status().is_success() {
        let status = release_resp.status();
        return Err(CliError::new(
            E_DEPLOY_006,
            format!(
                "could not fetch release v{version} from GitHub: HTTP {status}\n  \
                 Ensure GITHUB_TOKEN is exported and has repo read access.\n  \
                 Build from source or pass --mur-binary <path> to provide one manually."
            ),
        ));
    }

    let release_json: serde_json::Value = release_resp.body_mut().read_json().map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to parse GitHub release JSON: {e}"),
        )
    })?;

    let asset_id = release_json["assets"]
        .as_array()
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a["name"].as_str() == Some(asset_name.as_str()))
                .and_then(|a| a["id"].as_u64())
        })
        .ok_or_else(|| {
            CliError::new(
                E_DEPLOY_006,
                format!(
                    "asset '{asset_name}' not found in release v{version}\n  \
                     Build from source or pass --mur-binary <path> to provide one manually."
                ),
            )
        })?;

    // Step 2: download the asset via the API with Accept: application/octet-stream.
    let asset_url = format!("{api_base}/repos/murmur-nexus/murmur/releases/assets/{asset_id}");

    let mut asset_req = client
        .get(&asset_url)
        .header("User-Agent", "murmur-cli")
        .header("Accept", "application/octet-stream");
    if let Some(t) = &token {
        asset_req = asset_req.header("Authorization", format!("Bearer {t}"));
    }

    let mut asset_resp = asset_req.call().map_err(|e| {
        CliError::new(
            E_DEPLOY_006,
            format!(
                "could not download mur binary for {platform} v{version}: {e}\n  \
                 Build from source or pass --mur-binary <path> to provide one manually."
            ),
        )
    })?;

    if !asset_resp.status().is_success() {
        let status = asset_resp.status();
        return Err(CliError::new(
            E_DEPLOY_006,
            format!(
                "could not download mur binary for {platform} v{version}: HTTP {status}\n  \
                 Build from source or pass --mur-binary <path> to provide one manually."
            ),
        ));
    }

    let content_length = asset_resp.body().content_length();
    let bytes: bytes::Bytes = if let Some(pb) = dl_progress {
        if let Some(len) = content_length {
            pb.set_length(len);
        }
        let mut reader = pb.wrap_read(asset_resp.into_body().into_reader());
        let mut buf = Vec::with_capacity(content_length.unwrap_or(0) as usize);
        use std::io::Read;
        reader
            .read_to_end(&mut buf)
            .map(|_| bytes::Bytes::from(buf))
            .map_err(|e| {
                CliError::new(
                    E_IO_003,
                    format!("failed to read downloaded mur binary: {e}"),
                )
            })?
    } else {
        asset_resp
            .body_mut()
            .with_config()
            .limit(u64::MAX)
            .read_to_vec()
            .map(bytes::Bytes::from)
            .map_err(|e| {
                CliError::new(
                    E_IO_003,
                    format!("failed to read downloaded mur binary: {e}"),
                )
            })?
    };

    if bytes.is_empty() {
        return Err(CliError::new(
            E_DEPLOY_006,
            format!(
                "downloaded mur binary is empty — check that v{version} is released\n  \
                 Build from source or pass --mur-binary <path> to provide one manually."
            ),
        ));
    }

    std::fs::create_dir_all(&cache_dir).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to create binary cache directory: {e}"),
        )
    })?;
    std::fs::write(&cache_path, &bytes)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to write cached mur binary: {e}")))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&cache_path)
            .map_err(|e| {
                CliError::new(E_IO_003, format!("failed to read binary permissions: {e}"))
            })?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&cache_path, perms).map_err(|e| {
            CliError::new(
                E_IO_003,
                format!("failed to set binary executable permission: {e}"),
            )
        })?;
    }

    Ok(cache_path)
}

// ─── env var helpers ──────────────────────────────────────────────────────────

fn parse_env_var(s: &str) -> Result<(&str, &str), CliError> {
    s.split_once('=')
        .filter(|(k, _)| !k.is_empty())
        .ok_or_else(|| {
            CliError::new(
                "E-DEPLOY-001",
                format!("--env value {s:?} must be KEY=VALUE"),
            )
        })
}

/// Load a .env file and return lines as KEY=VALUE strings.
/// Handles both plain `KEY=VALUE` and shell `export KEY=VALUE` formats.
/// Blank lines and lines starting with `#` are skipped.
fn load_env_file(path: &Path) -> Result<Vec<String>, CliError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        CliError::new(
            E_IO_001,
            format!("cannot read env file {}: {e}", path.display()),
        )
    })?;
    let entries = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.strip_prefix("export ")
                .unwrap_or(line)
                .trim()
                .to_string()
        })
        .collect();
    Ok(entries)
}

// ─── mur binary cache check ───────────────────────────────────────────────────

/// Returns true if the mur binary is already available without downloading.
fn check_mur_binary_cached(
    explicit: Option<&Path>,
    version: &str,
    platform: &str,
    home: &Path,
) -> bool {
    if let Some(p) = explicit {
        return p.exists();
    }
    let cache_path = home
        .join(".murmur")
        .join("bin")
        .join(format!("mur-{version}-{platform}"));
    cache_path.exists() && cache_path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

// ─── spinner helpers ──────────────────────────────────────────────────────────

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

/// Build the shell script that starts the capsule on the remote host.
///
/// The flags here must stay a subset of what `mur run` actually accepts — an
/// unknown flag makes the remote `mur run` exit on argument parsing, which
/// deploy only discovers as a 120s `tail -f` timeout. Artifacts are pre-staged
/// by the upload step before this runs, so no fetch flag belongs here.
fn build_start_script(remote_deploy_dir: &str, remote_manifest: &str) -> String {
    format!(
        "[ -f {remote_deploy_dir}/.env ] && set -a && . {remote_deploy_dir}/.env && set +a; \
         > /tmp/mur-start.json; \
         nohup /usr/local/bin/mur run --manifest {remote_manifest} \
         --workdir {remote_deploy_dir} \
         --json \
         >/tmp/mur-start.json 2>/tmp/mur-start.err </dev/null & \
         timeout 120 tail -f /tmp/mur-start.json | head -n 1"
    )
}

// ─── run_deploy ───────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_deploy(
    host: &str,
    ssh_key_arg: Option<&Path>,
    ssh_user: &str,
    manifest_arg: &Path,
    workdir_arg: Option<&Path>,
    mur_binary_arg: Option<&Path>,
    env_vars: &[String],
    env_file_arg: Option<&Path>,
    target_platform: &str,
) -> Result<(), CliError> {
    let deploy_start = Instant::now();

    // ── 0. Validate arguments ─────────────────────────────────────────────────
    if host.is_empty() {
        return Err(CliError::new(
            "E-DEPLOY-001",
            "no host specified; create a VM first, then pass its IP via --host",
        ));
    }

    let manifest_path = if manifest_arg.is_absolute() {
        manifest_arg.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| CliError::new(E_IO_003, format!("current_dir: {e}")))?
            .join(manifest_arg)
    };
    if !manifest_path.exists() {
        return Err(CliError::new(
            E_IO_001,
            format!("manifest not found: {}", manifest_path.display()),
        ));
    }

    if let Some(wd) = workdir_arg {
        if !wd.exists() {
            return Err(CliError::new(
                E_IO_001,
                format!("workdir not found: {}", wd.display()),
            ));
        }
    }

    let ssh_key = ssh_key_arg.and_then(|p| p.to_str()).map(|s| s.to_string());
    let key_ref = ssh_key.as_deref();

    // ── 0.1. Env vars — explicit flags, --env-file, or auto-detect .env ───────
    // Priority: --env flags > --env-file > .env next to manifest (auto)
    let env_entries_owned: Vec<String> = if !env_vars.is_empty() {
        env_vars.to_vec()
    } else if let Some(file) = env_file_arg {
        load_env_file(file)?
    } else {
        let manifest_dir_path = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let auto_dotenv = manifest_dir_path.join(".env");
        if auto_dotenv.exists() {
            load_env_file(&auto_dotenv)?
        } else {
            vec![]
        }
    };
    let parsed_env: Vec<(&str, &str)> = env_entries_owned
        .iter()
        .map(|s| parse_env_var(s))
        .collect::<Result<Vec<_>, _>>()?;

    // ── 0.2. IDs ──────────────────────────────────────────────────────────────
    // dep_ prefix + UUID v7 simple (32 hex, no dashes) — consistent with ses_/tsk_/ctx_.
    // This names a *deployment* (a VM and its keys), not a capsule session; one deployment
    // outlives the sessions that run on it, which is why it gets an id of its own.
    let deployment_id = format!("dep_{}", Uuid::now_v7().simple());
    // Short 6-char hex ID for the remote deploy directory (skip the 4-char prefix).
    let short_id: String = deployment_id[4..].chars().take(6).collect();

    // ── 0.3. Home dir + staging dir ───────────────────────────────────────────
    let home_os = std::env::var_os("HOME")
        .ok_or_else(|| CliError::new(E_IO_001, "could not determine home directory"))?;
    let home = std::path::PathBuf::from(home_os);
    let staging_dir = home
        .join(".murmur")
        .join("deploy_staging")
        .join(&deployment_id);
    std::fs::create_dir_all(&staging_dir)
        .map_err(|e| CliError::new(E_IO_003, format!("failed to create staging dir: {e}")))?;
    let _staging_guard = StagingGuard(staging_dir.clone());

    // ── 0.4. Load manifest, artifacts, sources ────────────────────────────────
    let runtime_manifest = load_runtime_manifest(&manifest_path).map_err(|e| {
        CliError::new(
            E_IO_003,
            format!("failed to parse manifest for staging: {e}"),
        )
    })?;
    let artifact_refs = collect_deploy_artifacts(&runtime_manifest);
    let local_registry = LocalRegistry::from_default_home().map_err(CliError::from)?;
    let mur_config = load_mur_config()?;
    let chain = SourceChain::from_config(&mur_config);
    let manifest_dir = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let manifest_files = collect_manifest_files(&runtime_manifest, manifest_dir)?;

    // ── 0.5. Pre-compute artifact cache flags ────────────────────────────────
    // Done before creating spinners so the group header can show accurate counts.
    let cached_flags: Vec<bool> = artifact_refs
        .iter()
        .map(|(name, version)| {
            local_registry
                .artifact_path_for_platform(name, version, target_platform)
                .exists()
                || local_registry.artifact_path_for(name, version).exists()
        })
        .collect();

    // ── 0.6. Pre-check mur binary cache ──────────────────────────────────────
    let mur_version = runtime_manifest
        .mur_version
        .as_deref()
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let is_mur_cached =
        check_mur_binary_cached(mur_binary_arg, mur_version, target_platform, &home);
    let is_mur_explicit = mur_binary_arg.is_some();

    // ── Progress styles ───────────────────────────────────────────────────────
    let multi = MultiProgress::new();
    let tick_chars = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "];

    // Level-1 styles (main steps, 2-space indent)
    let pending_style = ProgressStyle::with_template("  {msg}").expect("valid pending style");
    let spinner_style = ProgressStyle::with_template("  {spinner:.cyan} {msg}")
        .expect("valid spinner style")
        .tick_strings(tick_chars);
    let done_style = ProgressStyle::with_template("  {msg}").expect("valid done style");

    // Level-2 styles (sub-items, 6-space indent for visual hierarchy)
    let pending_l2 = ProgressStyle::with_template("      {msg}").expect("valid l2 pending style");
    let spinner_l2 = ProgressStyle::with_template("      {spinner:.cyan} {msg}")
        .expect("valid l2 spinner style")
        .tick_strings(tick_chars);
    let download_l2 = ProgressStyle::with_template(
        "      {spinner:.cyan} {msg}  {bytes}/{total_bytes}  [{bar:12.cyan/dim}]",
    )
    .expect("valid l2 download style")
    .tick_strings(tick_chars)
    .progress_chars("█▒░");
    // mur binary download uses level-1 width (no indentation, bigger bar)
    let download_l1 = ProgressStyle::with_template(
        "  {spinner:.cyan} {msg}  {bytes}/{total_bytes}  [{bar:16.cyan/dim}]",
    )
    .expect("valid l1 download style")
    .tick_strings(tick_chars)
    .progress_chars("█▒░");
    let done_l2 = ProgressStyle::with_template("      {msg}").expect("valid l2 done style");

    // ── Create ALL spinners upfront (pending) ─────────────────────────────────
    // Two-level hierarchy:
    //   Level 1 (2 spaces): group headers + single steps — always visible
    //   Level 2 (6 spaces): per-artifact items — removed (collapsed) when group finishes
    //
    // This shows the whole plan before any work starts; the user can see all upcoming
    // steps even while downloads are running.

    let n_arts = artifact_refs.len();
    let s = |n: usize| if n == 1 { "" } else { "s" }; // plural suffix

    // ─ Download group ─────────────────────────────────────────────────────────
    let cached_count_pre = cached_flags.iter().filter(|&&c| c).count();
    let fetch_count_pre = n_arts - cached_count_pre;
    let dl_hint = if fetch_count_pre == 0 {
        "  all cached".to_string()
    } else if cached_count_pre == 0 {
        format!("  {} to fetch", fetch_count_pre)
    } else {
        format!("  {}↓  {} cached", fetch_count_pre, cached_count_pre)
    };
    let dl_header_pb: Option<ProgressBar> = if n_arts > 0 {
        Some(add_pending(
            &multi,
            &pending_style,
            format!(
                "{} ↓ {} artifact{}{}",
                style("·").dim(),
                n_arts,
                s(n_arts),
                style(&dl_hint).dim()
            ),
        ))
    } else {
        None
    };

    let dl_spinners: Vec<ProgressBar> = artifact_refs
        .iter()
        .map(|(name, version)| {
            add_pending(
                &multi,
                &pending_l2,
                format!("{} ↓ {name}@{version}", style("·").dim()),
            )
        })
        .collect();

    // ─ Single step spinners ───────────────────────────────────────────────────
    let mur_bin_pb = add_pending(
        &multi,
        &pending_style,
        if is_mur_explicit {
            format!("{} ↑ mur binary", style("·").dim())
        } else {
            format!("{} ↓ mur v{mur_version}", style("·").dim())
        },
    );
    let ssh_pb = add_pending(&multi, &pending_style, format!("{} SSH", style("·").dim()));
    let binary_pb = add_pending(
        &multi,
        &pending_style,
        format!("{} ↑ mur binary", style("·").dim()),
    );
    let files_pb = add_pending(
        &multi,
        &pending_style,
        format!("{} ↑ files", style("·").dim()),
    );

    // ─ Upload group ───────────────────────────────────────────────────────────
    let ul_header_pb: Option<ProgressBar> = if n_arts > 0 {
        Some(add_pending(
            &multi,
            &pending_style,
            format!("{} ↑ {} artifact{}", style("·").dim(), n_arts, s(n_arts)),
        ))
    } else {
        None
    };

    let ul_spinners: Vec<ProgressBar> = artifact_refs
        .iter()
        .map(|(name, version)| {
            add_pending(
                &multi,
                &pending_l2,
                format!("{} ↑ {name}@{version}", style("·").dim()),
            )
        })
        .collect();

    // ─ Start capsule ─────────────────────────────────────────────────────────
    let start_pb = add_pending(
        &multi,
        &pending_style,
        format!("{} → start capsule", style("·").dim()),
    );

    // ── 1. Parallel artifact downloads ───────────────────────────────────────
    // Activate the download group header.
    if let Some(ref pb) = dl_header_pb {
        activate_step(
            pb,
            &spinner_style,
            format!("↓ {} artifact{}", n_arts, s(n_arts)),
        );
    }

    // LocalRegistry and SourceChain are Send + Sync; rayon par_iter collects
    // results in original order — staged[i] == artifact_refs[i] == dl_spinners[i].
    let dl_raw: Vec<Result<StagedArtifact, CliError>> = artifact_refs
        .par_iter()
        .enumerate()
        .map(|(i, (name, version))| {
            let pb = &dl_spinners[i];
            let is_cached = cached_flags[i];

            if is_cached {
                activate_step(pb, &spinner_l2, format!("{name}@{version}"));
            } else {
                pb.set_style(download_l2.clone());
                pb.set_message(format!("↓ {name}@{version}"));
                pb.enable_steady_tick(Duration::from_millis(80));
                crate::source::github::push_download_progress(pb.clone());
            }

            let result =
                ensure_artifact_for_deploy(name, version, target_platform, &local_registry, &chain);

            if !is_cached {
                crate::source::github::pop_download_progress();
            }

            match &result {
                Ok(sa) => {
                    let info = if is_cached {
                        "cached".to_string()
                    } else {
                        format_bytes(sa.bytes.len() as u64)
                    };
                    finish_step(
                        pb,
                        &done_l2,
                        format!("{} ↓ {name}@{version}  {info}", style("✓").green().bold()),
                    );
                }
                Err(_) => {
                    abandon_step(
                        pb,
                        &done_l2,
                        format!("{} ↓ {name}@{version}  failed", style("✗").red().bold()),
                    );
                }
            }
            result
        })
        .collect();

    // Collapse level-2 download bars, then finish the group header with summary.
    for pb in &dl_spinners {
        multi.remove(pb);
    }

    let staged: Vec<StagedArtifact> = match dl_raw.into_iter().collect::<Result<Vec<_>, _>>() {
        Ok(staged) => {
            if let Some(ref pb) = dl_header_pb {
                let fetched_bytes: u64 = staged
                    .iter()
                    .zip(cached_flags.iter())
                    .filter(|(_, &c)| !c)
                    .map(|(sa, _)| sa.bytes.len() as u64)
                    .sum();
                let cached_n = cached_flags.iter().filter(|&&c| c).count();
                let summary = if cached_n == n_arts {
                    format!(
                        "{} ↓ {} artifact{}  all cached",
                        style("✓").green().bold(),
                        n_arts,
                        s(n_arts)
                    )
                } else {
                    let note = if cached_n > 0 {
                        format!("  {} cached", cached_n)
                    } else {
                        String::new()
                    };
                    format!(
                        "{} ↓ {} artifact{}  {}{}",
                        style("✓").green().bold(),
                        n_arts,
                        s(n_arts),
                        format_bytes(fetched_bytes),
                        note
                    )
                };
                finish_step(pb, &done_style, summary);
            }
            staged
        }
        Err(e) => {
            if let Some(ref pb) = dl_header_pb {
                abandon_step(
                    pb,
                    &done_style,
                    format!("{} ↓ artifacts  failed", style("✗").red().bold()),
                );
            }
            return Err(e);
        }
    };

    // ── 2. Resolve mur binary ─────────────────────────────────────────────────
    let local_mur_binary = if is_mur_cached || is_mur_explicit {
        let path = resolve_mur_binary_impl(
            mur_binary_arg,
            runtime_manifest.mur_version.as_deref(),
            target_platform,
            &home,
            None,
        )?;
        let done_label = if is_mur_explicit {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            format!(
                "{} ↑ mur binary  {}  (explicit)",
                style("✓").green().bold(),
                format_bytes(size)
            )
        } else {
            format!("{} ↓ mur v{mur_version}  cached", style("✓").green().bold())
        };
        finish_step(&mur_bin_pb, &done_style, done_label);
        path
    } else {
        // Not cached — activate with level-1 download style and stream progress.
        mur_bin_pb.set_style(download_l1.clone());
        mur_bin_pb.set_message(format!("↓ mur v{mur_version}"));
        mur_bin_pb.enable_steady_tick(Duration::from_millis(80));
        let path = resolve_mur_binary_impl(
            mur_binary_arg,
            runtime_manifest.mur_version.as_deref(),
            target_platform,
            &home,
            Some(&mur_bin_pb),
        )?;
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        finish_step(
            &mur_bin_pb,
            &done_style,
            format!(
                "{} ↓ mur v{mur_version}  {}",
                style("✓").green().bold(),
                format_bytes(size)
            ),
        );
        path
    };

    // ── 3. Wait for SSH ───────────────────────────────────────────────────────
    activate_step(&ssh_pb, &spinner_style, "SSH  connecting...");
    wait_for_ssh(host, key_ref, ssh_user, Duration::from_secs(30))?;
    finish_step(
        &ssh_pb,
        &done_style,
        format!("{} SSH  ready", style("✓").green().bold()),
    );
    multi.remove(&ssh_pb); // collapse SSH once ready — next step appears in its place

    // ── 4. Upload mur binary ──────────────────────────────────────────────────
    // Upload to /tmp first; then move into /usr/local/bin.
    // Direct scp to /usr/local/bin/mur fails on some VPS with "dest open: Failure"
    // because SFTP-mode scp has restricted access to system dirs even as root.
    let binary_size = std::fs::metadata(&local_mur_binary)
        .map(|m| m.len())
        .unwrap_or(0);

    // Level-1 upload bar with fake ticker (same pattern as artifact uploads).
    binary_pb.set_style(download_l1.clone());
    binary_pb.set_length(binary_size);
    binary_pb.set_message("↑ mur binary");
    binary_pb.enable_steady_tick(Duration::from_millis(80));
    let bin_running = Arc::new(AtomicBool::new(true));
    let bin_running2 = bin_running.clone();
    let bin_pb_tick = binary_pb.clone();
    let bin_ticker = std::thread::spawn(move || {
        let target = (binary_size as f64 * 0.85) as u64;
        let chunk = (target / 50).max(1024);
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if !bin_running2.load(Ordering::Relaxed) {
                break;
            }
            let pos = bin_pb_tick.position();
            if pos < target {
                bin_pb_tick.set_position((pos + chunk).min(target));
            }
        }
    });

    let bin_result = (|| -> Result<(), CliError> {
        scp_upload(
            host,
            key_ref,
            ssh_user,
            &local_mur_binary.to_string_lossy(),
            "/tmp/mur-upload",
            false,
        )?;
        ssh_exec(host, key_ref, ssh_user,
            "mkdir -p /usr/local/bin && mv /tmp/mur-upload /usr/local/bin/mur && chmod +x /usr/local/bin/mur")?;
        Ok(())
    })();

    bin_running.store(false, Ordering::Relaxed);
    let _ = bin_ticker.join();

    match bin_result {
        Ok(()) => {
            binary_pb.set_position(binary_size);
            finish_step(
                &binary_pb,
                &done_style,
                format!(
                    "{} ↑ mur binary  {}",
                    style("✓").green().bold(),
                    format_bytes(binary_size)
                ),
            );
        }
        Err(e) => {
            abandon_step(
                &binary_pb,
                &done_style,
                format!("{} ↑ mur binary  failed", style("✗").red().bold()),
            );
            return Err(e);
        }
    }

    // ── 5. Upload capsule manifest and workdir ────────────────────────────────
    let remote_deploy_dir = format!("/root/mur-{short_id}");
    ssh_exec(
        host,
        key_ref,
        ssh_user,
        &format!("mkdir -p {remote_deploy_dir}"),
    )?;

    let manifest_filename = manifest_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(MANIFEST_FILENAME);
    let remote_manifest = format!("{remote_deploy_dir}/{manifest_filename}");

    activate_step(
        &files_pb,
        &spinner_style,
        format!("↑ {manifest_filename}  uploading..."),
    );

    scp_upload(
        host,
        key_ref,
        ssh_user,
        &manifest_path.to_string_lossy(),
        &remote_manifest,
        false,
    )?;

    for (local_path, remote_name) in &manifest_files {
        scp_upload(
            host,
            key_ref,
            ssh_user,
            &local_path.to_string_lossy(),
            &format!("{remote_deploy_dir}/{remote_name}"),
            false,
        )?;
    }

    if let Some(wd) = workdir_arg {
        scp_upload(
            host,
            key_ref,
            ssh_user,
            &wd.to_string_lossy(),
            &format!("{remote_deploy_dir}/workdir"),
            true,
        )?;
    }

    let mut capsule_file_names: Vec<String> = vec![manifest_filename.to_string()];
    for (_, name) in &manifest_files {
        capsule_file_names.push(name.clone());
    }
    finish_step(
        &files_pb,
        &done_style,
        format!(
            "{} ↑ {}",
            style("✓").green().bold(),
            capsule_file_names.join(" · ")
        ),
    );

    // ── 6. Upload pre-staged artifacts (parallel) ─────────────────────────────
    // Level-1 header activates; level-2 upload spinners run in parallel; collapse
    // the level-2 bars when all uploads finish, leaving only the header summary.
    if !staged.is_empty() {
        if let Some(ref pb) = ul_header_pb {
            activate_step(
                pb,
                &spinner_style,
                format!("↑ {} artifact{}", staged.len(), s(staged.len())),
            );
        }

        // Write all staging files to disk first (fast, sequential).
        for artifact in &staged {
            let stem = format!("{}-{}", artifact.name, artifact.version);
            std::fs::write(staging_dir.join(format!("{stem}.mur.zip")), &artifact.bytes)
                .map_err(|e| CliError::new(E_IO_003, format!("staging {}: {e}", artifact.name)))?;
            std::fs::write(
                staging_dir.join(format!("{stem}.sha256")),
                artifact.sha256.as_bytes(),
            )
            .map_err(|e| {
                CliError::new(E_IO_003, format!("staging sha256 {}: {e}", artifact.name))
            })?;
        }

        // Parallel scp uploads — each artifact drives its own level-2 progress bar.
        // scp is a subprocess so we can't stream bytes; a background ticker fills
        // the bar 0→85% while scp runs, then jumps to 100% on completion.
        // Returns Ok(bytes_uploaded) on success for the group summary.
        let ul_raw: Vec<Result<u64, CliError>> = staged
            .par_iter()
            .enumerate()
            .map(|(i, artifact)| -> Result<u64, CliError> {
                let pb = &ul_spinners[i];
                let size = artifact.bytes.len() as u64;

                // Set up determinate bar (same template as downloads).
                pb.set_style(download_l2.clone());
                pb.set_length(size);
                pb.set_message(format!("↑ {}@{}", artifact.name, artifact.version));
                pb.enable_steady_tick(Duration::from_millis(80));

                // Fake ticker: fills 0 → 85% of file size over ~10s.
                let running = Arc::new(AtomicBool::new(true));
                let running2 = running.clone();
                let pb_tick = pb.clone();
                let ticker = std::thread::spawn(move || {
                    let target = (size as f64 * 0.85) as u64;
                    let chunk = (target / 50).max(1024); // ~50 ticks × 200ms = 10s to reach target
                    loop {
                        std::thread::sleep(Duration::from_millis(200));
                        if !running2.load(Ordering::Relaxed) {
                            break;
                        }
                        let pos = pb_tick.position();
                        if pos < target {
                            pb_tick.set_position((pos + chunk).min(target));
                        }
                    }
                });

                let result = (|| {
                    let stem = format!("{}-{}", artifact.name, artifact.version);
                    let staged_zip = staging_dir.join(format!("{stem}.mur.zip"));
                    let staged_sha = staging_dir.join(format!("{stem}.sha256"));
                    let remote_dir = format!(
                        "/root/.murmur/artifacts/{}/{}",
                        artifact.name, artifact.version
                    );
                    ssh_exec(host, key_ref, ssh_user, &format!("mkdir -p {remote_dir}"))?;
                    scp_upload(
                        host,
                        key_ref,
                        ssh_user,
                        &staged_zip.to_string_lossy(),
                        &format!("{remote_dir}/{stem}.mur.zip"),
                        false,
                    )?;
                    scp_upload(
                        host,
                        key_ref,
                        ssh_user,
                        &staged_sha.to_string_lossy(),
                        &format!("{remote_dir}/{stem}.sha256"),
                        false,
                    )?;
                    Ok(())
                })();

                // Stop ticker (waits at most one 200ms sleep cycle).
                running.store(false, Ordering::Relaxed);
                let _ = ticker.join();

                match result {
                    Ok(()) => {
                        pb.set_position(size); // fill bar to 100%
                        finish_step(
                            pb,
                            &done_l2,
                            format!(
                                "{} ↑ {}@{}  {}",
                                style("✓").green().bold(),
                                artifact.name,
                                artifact.version,
                                format_bytes(size)
                            ),
                        );
                        Ok(size)
                    }
                    Err(e) => {
                        abandon_step(
                            pb,
                            &done_l2,
                            format!(
                                "{} ↑ {}@{}  failed",
                                style("✗").red().bold(),
                                artifact.name,
                                artifact.version
                            ),
                        );
                        Err(e)
                    }
                }
            })
            .collect();

        // Collapse level-2 upload bars regardless of success/failure.
        for pb in &ul_spinners {
            multi.remove(pb);
        }

        match ul_raw.into_iter().collect::<Result<Vec<u64>, _>>() {
            Ok(sizes) => {
                if let Some(ref pb) = ul_header_pb {
                    let total: u64 = sizes.iter().sum();
                    finish_step(
                        pb,
                        &done_style,
                        format!(
                            "{} ↑ {} artifact{}  {}",
                            style("✓").green().bold(),
                            staged.len(),
                            s(staged.len()),
                            format_bytes(total)
                        ),
                    );
                }
            }
            Err(e) => {
                if let Some(ref pb) = ul_header_pb {
                    abandon_step(
                        pb,
                        &done_style,
                        format!("{} ↑ artifacts  upload failed", style("✗").red().bold()),
                    );
                }
                return Err(e);
            }
        }
    }

    // ── 7. Write .env file to VM ──────────────────────────────────────────────
    if !parsed_env.is_empty() {
        let env_file_content = parsed_env
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect::<String>();
        let encoded = base64_encode(env_file_content.as_bytes());
        let cmd = format!(
            "printf '%s' '{encoded}' | base64 -d > {remote_deploy_dir}/.env && chmod 600 {remote_deploy_dir}/.env"
        );
        ssh_exec(host, key_ref, ssh_user, &cmd)?;
    }

    // ── 8. Start capsule ──────────────────────────────────────────────────────
    activate_step(&start_pb, &spinner_style, "→ starting capsule...");

    let start_script = build_start_script(&remote_deploy_dir, &remote_manifest);
    let raw_output = ssh_exec(host, key_ref, ssh_user, &start_script)?;
    let json_line = raw_output.trim().to_string();

    if json_line.is_empty() {
        let stderr = ssh_exec(
            host,
            key_ref,
            ssh_user,
            "cat /tmp/mur-start.err 2>/dev/null || true",
        )
        .unwrap_or_default();
        let still_running = ssh_exec(
            host,
            key_ref,
            ssh_user,
            "pgrep -x mur >/dev/null 2>&1 && echo yes || echo no",
        )
        .unwrap_or_default();
        let still_running = still_running.trim() == "yes";
        return Err(CliError::new(
            E_DEPLOY_004,
            format!(
                "capsule did not emit startup JSON within 120s{}\n  mur stderr: {}",
                if still_running {
                    " (mur process is still running — VM may be slow to compile on first start)"
                } else {
                    " (mur process has exited)"
                },
                if stderr.trim().is_empty() {
                    "(empty)"
                } else {
                    truncate(stderr.trim(), 500)
                }
            ),
        ));
    }

    // ── 9. Parse JSON and construct public URL ────────────────────────────────
    let start_info: serde_json::Value = serde_json::from_str(&json_line).map_err(|e| {
        CliError::new(
            E_DEPLOY_004,
            format!("could not parse capsule startup JSON '{json_line}': {e}"),
        )
    })?;

    let url_field = start_info["url"]
        .as_str()
        .ok_or_else(|| CliError::new(E_DEPLOY_004, "startup JSON missing 'url' field"))?;

    let port = url_field.rsplit(':').next().ok_or_else(|| {
        CliError::new(E_DEPLOY_004, format!("unexpected url shape '{url_field}'"))
    })?;
    // Make the capsule reachable from outside the VM.
    // mur run binds to 127.0.0.1 (loopback); iptables DNAT redirects external traffic
    // to that loopback address. route_localnet is required for DNAT to loopback to work.
    // ufw allow opens the port in the kernel firewall.
    // All three commands are silent/non-fatal — if a mechanism isn't available the others still apply.
    let _ = ssh_exec(host, key_ref, ssh_user, &format!(
        "sysctl -w net.ipv4.conf.all.route_localnet=1 >/dev/null 2>&1; \
         iptables -t nat -A PREROUTING -p tcp --dport {port} -j DNAT --to-destination 127.0.0.1:{port} >/dev/null 2>&1; \
         ufw allow {port}/tcp >/dev/null 2>&1; \
         true"
    ));

    let public_url = format!("http://{host}:{port}");

    finish_step(
        &start_pb,
        &done_style,
        format!(
            "{} → capsule  started · port {port}",
            style("✓").green().bold()
        ),
    );

    // ── 10. Persist deployment record ─────────────────────────────────────────
    let record = DeploymentRecord {
        deployment_id: deployment_id.clone(),
        provider: "manual".to_string(),
        provider_vm_id: String::new(),
        provider_key_id: String::new(),
        region: String::new(),
        ip: host.to_string(),
        url: public_url.clone(),
        manifest_path: manifest_path.to_string_lossy().into_owned(),
        started_at: Utc::now().to_rfc3339(),
        status: "running".to_string(),
    };
    append_deployment(record)?;

    // ── 11. Success summary box ────────────────────────────────────────────────
    let elapsed = deploy_start.elapsed().as_secs();
    let capsule_name = &runtime_manifest.name;

    // Build content lines; measure_text_width strips ANSI codes for accurate width.
    let title = format!(
        "{}  {}",
        style("∞").green().bold(),
        style(capsule_name).cyan().bold()
    );
    let blank = String::new();
    let row_url = format!(
        "{}  {}",
        style("url ").dim(),
        style(&public_url).underlined()
    );
    // Show "dep_" prefix + first 8 hex chars: "dep_01954a3b" (12 chars, self-describing)
    let row_job = format!("{}  {}", style("dep ").dim(), &deployment_id[..12]);
    let row_time = format!("{}  {}s", style("time").dim(), elapsed);
    let rows: &[&str] = &[&title, &blank, &row_url, &row_job, &row_time];

    let max_vis = rows
        .iter()
        .map(|l| measure_text_width(l))
        .max()
        .unwrap_or(30);
    let h_bar = "─".repeat(max_vis + 4); // 2-space padding each side
    multi.println(format!("\n  ┌{h_bar}┐"))?;
    for row in rows {
        let vis = measure_text_width(row);
        let pad = " ".repeat(max_vis - vis);
        multi.println(format!("  │  {row}{pad}  │"))?;
    }
    multi.println(format!("  └{h_bar}┘\n"))?;
    drop(multi); // ensure all bars finalize before returning
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use murmur_artifact::RuntimeManifest;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn base64_encode_matches_rfc_4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
        }
    }

    // ─── build_start_script ───────────────────────────────────────────────────

    #[test]
    fn start_script_passes_no_auto_pull_flag() {
        let script = build_start_script("/root/mur-abc123", "/root/mur-abc123/murmur.yaml");
        assert!(
            !script.contains("--auto-pull"),
            "`mur run` has no --auto-pull flag; sending it makes the remote process \
             exit on argument parsing: {script}"
        );
    }

    #[test]
    fn start_script_uses_only_flags_mur_run_accepts() {
        let script = build_start_script("/root/mur-abc123", "/root/mur-abc123/murmur.yaml");
        assert!(
            script.contains("mur run --manifest /root/mur-abc123/murmur.yaml"),
            "{script}"
        );
        assert!(script.contains("--workdir /root/mur-abc123"), "{script}");
        assert!(script.contains("--json"), "{script}");
    }

    // ─── parse_env_var ────────────────────────────────────────────────────────

    #[test]
    fn parse_env_var_splits_simple_key_value() {
        let (k, v) = parse_env_var("KEY=value").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "value");
    }

    #[test]
    fn parse_env_var_preserves_equals_in_value() {
        let (k, v) = parse_env_var("KEY=val=with=equals").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "val=with=equals");
    }

    #[test]
    fn parse_env_var_rejects_no_equals() {
        assert!(parse_env_var("NOEQUALS").is_err());
    }

    #[test]
    fn parse_env_var_rejects_empty_key() {
        assert!(parse_env_var("=value").is_err());
    }

    #[test]
    fn parse_env_var_accepts_empty_value() {
        let (k, v) = parse_env_var("KEY=").unwrap();
        assert_eq!(k, "KEY");
        assert_eq!(v, "");
    }

    // ─── collect_manifest_files ───────────────────────────────────────────────

    #[test]
    fn collect_manifest_files_returns_empty_for_no_file_path_fields() {
        let manifest =
            RuntimeManifest::from_yaml_str("name: cap\nversion: 0.1.0\nartifacts: []\n").unwrap();
        let dir = tempdir().unwrap();
        let files = collect_manifest_files(&manifest, dir.path()).unwrap();
        assert!(
            files.is_empty(),
            "expected no files for a manifest with no file-path fields"
        );
    }

    #[test]
    fn collect_manifest_files_includes_existing_system_prompt_file() {
        let dir = tempdir().unwrap();
        let instructions = dir.path().join("instructions.md");
        fs::write(&instructions, "You are an assistant.").unwrap();

        let yaml = format!(
            "name: cap\nversion: 0.1.0\nartifacts: []\n\
             inference:\n  endpoint: http://localhost:8080\n  model: test\n  \
             system_prompt_file: instructions.md\n  \
             driver:\n    artifact: murmur-driver-anthropic\n"
        );
        let manifest = RuntimeManifest::from_yaml_str(&yaml).unwrap();

        let files = collect_manifest_files(&manifest, dir.path()).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, instructions);
        assert_eq!(files[0].1, "instructions.md");
    }

    #[test]
    fn collect_manifest_files_errors_for_missing_system_prompt_file() {
        let dir = tempdir().unwrap();
        // Do NOT create instructions.md

        let yaml = "name: cap\nversion: 0.1.0\nartifacts: []\n\
                    inference:\n  endpoint: http://localhost:8080\n  model: test\n  \
                    system_prompt_file: instructions.md\n  \
                    driver:\n    artifact: murmur-driver-anthropic\n";
        let manifest = RuntimeManifest::from_yaml_str(yaml).unwrap();

        let err = collect_manifest_files(&manifest, dir.path()).unwrap_err();
        assert!(
            err.message.contains("instructions.md"),
            "error should mention the missing filename, got: {}",
            err.message
        );
    }

    #[test]
    fn collect_manifest_files_includes_existing_compaction_system_prompt_file() {
        let dir = tempdir().unwrap();
        let instructions = dir.path().join("compaction-instructions.md");
        fs::write(&instructions, "Summarize aggressively.").unwrap();

        let yaml = "name: cap\nversion: 0.1.0\nartifacts: []\n\
                    inference:\n  endpoint: http://localhost:8080\n  model: test\n  \
                    compaction:\n    system_prompt_file: compaction-instructions.md\n  \
                    driver:\n    artifact: murmur-driver-anthropic\n";
        let manifest = RuntimeManifest::from_yaml_str(yaml).unwrap();

        let files = collect_manifest_files(&manifest, dir.path()).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, instructions);
        assert_eq!(files[0].1, "compaction-instructions.md");
    }

    #[test]
    fn collect_manifest_files_errors_for_missing_compaction_system_prompt_file() {
        let dir = tempdir().unwrap();
        // Do NOT create compaction-instructions.md

        let yaml = "name: cap\nversion: 0.1.0\nartifacts: []\n\
                    inference:\n  endpoint: http://localhost:8080\n  model: test\n  \
                    compaction:\n    system_prompt_file: compaction-instructions.md\n  \
                    driver:\n    artifact: murmur-driver-anthropic\n";
        let manifest = RuntimeManifest::from_yaml_str(yaml).unwrap();

        let err = collect_manifest_files(&manifest, dir.path()).unwrap_err();
        assert!(
            err.message.contains("compaction-instructions.md")
                && err
                    .message
                    .contains("inference.compaction.system_prompt_file"),
            "error should name the field and the missing path, got: {}",
            err.message
        );
    }

    /// Both prompt-file fields set means both files ship — the primary one first.
    #[test]
    fn collect_manifest_files_includes_both_prompt_files() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("instructions.md"), "You are an assistant.").unwrap();
        fs::write(dir.path().join("compaction.md"), "Summarize.").unwrap();

        let yaml = "name: cap\nversion: 0.1.0\nartifacts: []\n\
                    inference:\n  endpoint: http://localhost:8080\n  model: test\n  \
                    system_prompt_file: instructions.md\n  \
                    compaction:\n    system_prompt_file: compaction.md\n  \
                    driver:\n    artifact: murmur-driver-anthropic\n";
        let manifest = RuntimeManifest::from_yaml_str(yaml).unwrap();

        let files = collect_manifest_files(&manifest, dir.path()).unwrap();

        let names: Vec<&str> = files.iter().map(|(_, n)| n.as_str()).collect();
        assert_eq!(names, vec!["instructions.md", "compaction.md"]);
    }

    // ─── resolve_mur_binary_impl ─────────────────────────────────────────────

    #[test]
    fn resolve_mur_binary_returns_explicit_path_when_valid() {
        let dir = tempdir().unwrap();
        let binary = dir.path().join("my-mur");
        fs::write(&binary, b"fake-binary").unwrap();

        let result =
            resolve_mur_binary_impl(Some(&binary), None, "linux-x86_64", dir.path(), None).unwrap();
        assert_eq!(result, binary);
    }

    #[test]
    fn resolve_mur_binary_errors_for_missing_explicit_path() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent-mur");

        let err = resolve_mur_binary_impl(Some(&missing), None, "linux-x86_64", dir.path(), None)
            .unwrap_err();
        assert!(
            err.message.contains("--mur-binary not found"),
            "expected mur-binary-not-found error, got: {}",
            err.message
        );
    }

    #[test]
    fn resolve_mur_binary_uses_manifest_version_for_cache_lookup() {
        let dir = tempdir().unwrap();
        let cache_dir = dir.path().join(".murmur").join("bin");
        fs::create_dir_all(&cache_dir).unwrap();
        let cached = cache_dir.join("mur-0.4.5-linux-x86_64");
        fs::write(&cached, b"fake-cached-binary").unwrap();

        let result =
            resolve_mur_binary_impl(None, Some("0.4.5"), "linux-x86_64", dir.path(), None).unwrap();
        assert_eq!(
            result, cached,
            "should return cached path for manifest version 0.4.5"
        );
    }

    #[test]
    fn resolve_mur_binary_uses_running_version_when_no_manifest_version() {
        let dir = tempdir().unwrap();
        let running_version = env!("CARGO_PKG_VERSION");
        let cache_dir = dir.path().join(".murmur").join("bin");
        fs::create_dir_all(&cache_dir).unwrap();
        let cached = cache_dir.join(format!("mur-{running_version}-linux-x86_64"));
        fs::write(&cached, b"fake-cached-binary").unwrap();

        let result = resolve_mur_binary_impl(None, None, "linux-x86_64", dir.path(), None).unwrap();
        assert_eq!(
            result, cached,
            "should return cached path for running version"
        );
    }

    // ─── staging path ─────────────────────────────────────────────────────────

    #[test]
    fn staging_path_uses_deployment_dir_with_clean_filename() {
        let home = tempdir().unwrap();
        let deployment_id = "abc123-test-deployment-id";
        let staging_dir = home
            .path()
            .join(".murmur")
            .join("deploy_staging")
            .join(deployment_id);

        let stem = "murmur-driver-openai-0.3.33";
        let zip_path = staging_dir.join(format!("{stem}.mur.zip"));
        let sha_path = staging_dir.join(format!("{stem}.sha256"));

        let zip_name = zip_path.file_name().unwrap().to_str().unwrap();
        let sha_name = sha_path.file_name().unwrap().to_str().unwrap();

        assert_eq!(zip_name, "murmur-driver-openai-0.3.33.mur.zip");
        assert_eq!(sha_name, "murmur-driver-openai-0.3.33.sha256");
        assert!(
            !zip_name.contains(deployment_id),
            "filename must not contain deployment UUID: {zip_name}"
        );
        assert!(
            !sha_name.contains(deployment_id),
            "filename must not contain deployment UUID: {sha_name}"
        );
    }

    #[test]
    fn staging_guard_removes_dir_on_drop() {
        let home = tempdir().unwrap();
        let staging_dir = home.path().join("to-clean");
        fs::create_dir_all(&staging_dir).unwrap();
        fs::write(staging_dir.join("file.txt"), b"data").unwrap();
        assert!(staging_dir.exists());

        {
            let _guard = StagingGuard(staging_dir.clone());
        }

        assert!(
            !staging_dir.exists(),
            "StagingGuard must remove the dir on drop"
        );
    }

    // ─── parallel artifact resolution ─────────────────────────────────────────

    #[test]
    fn parallel_resolution_returns_all_six_artifacts() {
        use crate::config::SourceConfig;
        use crate::source::{ArtifactSource, SourceChain, SourceError};
        use bytes::Bytes;
        use murmur_artifact::{sha256_hex, ArtifactMeta, LocalRegistry, RuntimeType};
        use std::io::Write;
        use zip::{
            write::{FileOptions, SimpleFileOptions},
            CompressionMethod, ZipWriter,
        };

        fn make_zip_bytes(name: &str, version: &str) -> Vec<u8> {
            let mut cursor = std::io::Cursor::new(Vec::new());
            let mut zip = ZipWriter::new(&mut cursor);
            let opts: SimpleFileOptions =
                FileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("murmur.yaml", opts).unwrap();
            writeln!(zip, "name: {name}").unwrap();
            writeln!(zip, "version: {version}").unwrap();
            zip.start_file("tool.wasm", opts).unwrap();
            zip.write_all(b"fake-wasm").unwrap();
            zip.finish().unwrap();
            cursor.into_inner()
        }

        struct NeverCalled;
        impl ArtifactSource for NeverCalled {
            fn name(&self) -> &str {
                "never-called"
            }
            fn resolve_bare(&self, _: &str) -> Result<(Bytes, String), SourceError> {
                panic!("source chain must not be called when all artifacts are cached")
            }
        }

        let dir = tempdir().unwrap();
        let registry = LocalRegistry::new(dir.path());

        let pairs: Vec<(&str, &str)> = vec![
            ("art-1", "1.0.0"),
            ("art-2", "1.0.0"),
            ("art-3", "1.0.0"),
            ("art-4", "1.0.0"),
            ("art-5", "1.0.0"),
            ("art-6", "1.0.0"),
        ];

        for (name, version) in &pairs {
            let bytes = make_zip_bytes(name, version);
            let sha256 = sha256_hex(&bytes);
            registry
                .store_installed_overwrite(
                    ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Wasm,
                        artifact_runtime: "wasm".to_string(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    &bytes,
                    &sha256,
                )
                .unwrap();
        }

        let chain = SourceChain::from_sources_for_test(
            vec![Box::new(NeverCalled)],
            Vec::<SourceConfig>::new(),
        );

        let results: Vec<Result<StagedArtifact, CliError>> = pairs
            .par_iter()
            .map(|(name, version)| {
                ensure_artifact_for_deploy(name, version, "linux-x86_64", &registry, &chain)
            })
            .collect();

        let staged: Vec<StagedArtifact> =
            results.into_iter().collect::<Result<Vec<_>, _>>().unwrap();

        assert_eq!(staged.len(), 6, "all 6 artifacts must be returned");
        for artifact in &staged {
            assert!(
                pairs
                    .iter()
                    .any(|(n, v)| *n == artifact.name && *v == artifact.version),
                "unexpected artifact in results: {}@{}",
                artifact.name,
                artifact.version
            );
        }
    }

    // ─── load_env_file ────────────────────────────────────────────────────────

    #[test]
    fn load_env_file_parses_key_value_entries() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".env");
        fs::write(&file, "FOO=bar\nBAZ=qux\n").unwrap();
        let entries = load_env_file(&file).unwrap();
        assert_eq!(entries, vec!["FOO=bar", "BAZ=qux"]);
    }

    #[test]
    fn load_env_file_skips_comments_and_blank_lines() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".env");
        fs::write(&file, "# comment\n\nKEY=val\n# another comment\n\n").unwrap();
        let entries = load_env_file(&file).unwrap();
        assert_eq!(entries, vec!["KEY=val"]);
    }

    #[test]
    fn load_env_file_strips_export_prefix() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".env");
        fs::write(
            &file,
            "export OPENAI_API_KEY=sk-xxx\nexport DB_URL=postgres://\n",
        )
        .unwrap();
        let entries = load_env_file(&file).unwrap();
        assert_eq!(entries, vec!["OPENAI_API_KEY=sk-xxx", "DB_URL=postgres://"]);
    }

    #[test]
    fn load_env_file_preserves_equals_in_value() {
        let dir = tempdir().unwrap();
        let file = dir.path().join(".env");
        fs::write(&file, "TOKEN=abc=def==\n").unwrap();
        let entries = load_env_file(&file).unwrap();
        assert_eq!(entries, vec!["TOKEN=abc=def=="]);
    }

    #[test]
    fn load_env_file_returns_error_for_missing_file() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent.env");
        assert!(load_env_file(&missing).is_err());
    }

    // ─── check_mur_binary_cached ──────────────────────────────────────────────

    #[test]
    fn check_mur_binary_cached_returns_true_for_explicit_existing_path() {
        let dir = tempdir().unwrap();
        let binary = dir.path().join("mur");
        fs::write(&binary, b"fake").unwrap();
        assert!(check_mur_binary_cached(
            Some(&binary),
            "0.4.9",
            "linux-x86_64",
            dir.path()
        ));
    }

    #[test]
    fn check_mur_binary_cached_returns_false_for_missing_explicit_path() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("no-mur");
        assert!(!check_mur_binary_cached(
            Some(&missing),
            "0.4.9",
            "linux-x86_64",
            dir.path()
        ));
    }

    #[test]
    fn check_mur_binary_cached_returns_true_for_cached_version() {
        let dir = tempdir().unwrap();
        let bin_dir = dir.path().join(".murmur").join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("mur-0.4.9-linux-x86_64"), b"fake").unwrap();
        assert!(check_mur_binary_cached(
            None,
            "0.4.9",
            "linux-x86_64",
            dir.path()
        ));
    }

    #[test]
    fn check_mur_binary_cached_returns_false_for_uncached_version() {
        let dir = tempdir().unwrap();
        assert!(!check_mur_binary_cached(
            None,
            "0.4.9",
            "linux-x86_64",
            dir.path()
        ));
    }

    // ─── remote deploy dir name ───────────────────────────────────────────────

    #[test]
    fn short_id_is_six_hex_chars_after_the_four_char_prefix() {
        // deployment_id format: "dep_" + 32 hex chars (UUID v7 simple)
        let deployment_id = "dep_018f4b2c1234567890abcdef12345678";
        // skip the 4-char prefix, take first 6 hex chars
        let short_id: String = deployment_id[4..].chars().take(6).collect();
        assert_eq!(short_id, "018f4b");
        assert_eq!(short_id.len(), 6);
        assert!(short_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() && (c.is_ascii_lowercase() || c.is_ascii_digit())));
        let remote_dir = format!("/root/mur-{short_id}");
        assert_eq!(remote_dir, "/root/mur-018f4b");
        assert!(
            !remote_dir.contains("deploy"),
            "dir must not contain 'deploy': {remote_dir}"
        );
    }

    // ─── scp stderr capture ───────────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn scp_failure_error_contains_captured_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let fake_scp = dir.path().join("scp");
        fs::write(
            &fake_scp,
            "#!/bin/sh\nprintf '%s' 'scp-stderr-sentinel: connection refused' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = fs::metadata(&fake_scp).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_scp, perms).unwrap();

        let local_file = dir.path().join("upload.txt");
        fs::write(&local_file, b"data").unwrap();

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let patched = format!("{}:{orig_path}", dir.path().display());
        // SAFETY: PATH modification is test-only. This test must not run concurrently
        // with other tests that also modify PATH. Within a single test binary, tests
        // run sequentially unless --test-threads > 1. The fake `scp` only exists for
        // the duration of this test.
        unsafe { std::env::set_var("PATH", &patched) };

        let result = scp_upload(
            "127.0.0.1",
            None,
            "root",
            &local_file.to_string_lossy(),
            "/tmp/sentinel-test.txt",
            false,
        );

        unsafe { std::env::set_var("PATH", &orig_path) };

        let err = result.expect_err("scp_upload must fail when scp exits non-zero");
        assert!(
            err.message.contains("scp-stderr-sentinel"),
            "error must include captured scp stderr; got: {}",
            err.message
        );
    }

    // ─── ensure_artifact_for_deploy ───────────────────────────────────────────

    #[cfg(feature = "beta-mur-deploy")]
    mod ensure_artifact {
        use std::{
            io::Write,
            sync::{
                atomic::{AtomicBool, Ordering},
                Arc,
            },
        };

        use bytes::Bytes;
        use murmur_artifact::{current_platform, ArtifactMeta, LocalRegistry, RuntimeType};
        use tempfile::tempdir;
        use zip::{
            write::{FileOptions, SimpleFileOptions},
            CompressionMethod, ZipWriter,
        };

        use crate::{
            config::SourceConfig,
            source::{ArtifactSource, SourceChain, SourceError},
        };

        use super::super::{ensure_artifact_for_deploy, StagedArtifact};
        use murmur_artifact::sha256_hex;

        fn make_zip(name: &str, version: &str) -> Vec<u8> {
            let mut cursor = std::io::Cursor::new(Vec::new());
            {
                let mut zip = ZipWriter::new(&mut cursor);
                let opts: SimpleFileOptions =
                    FileOptions::default().compression_method(CompressionMethod::Deflated);
                zip.start_file("murmur.yaml", opts).unwrap();
                writeln!(zip, "name: {name}").unwrap();
                writeln!(zip, "version: {version}").unwrap();
                zip.start_file("tool.wasm", opts).unwrap();
                zip.write_all(b"fake-wasm").unwrap();
                zip.finish().unwrap();
            }
            cursor.into_inner()
        }

        fn store_wasm(registry: &LocalRegistry, name: &str, version: &str, bytes: &[u8]) {
            let sha256 = sha256_hex(bytes);
            registry
                .store_installed_overwrite(
                    ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Wasm,
                        artifact_runtime: "wasm".to_string(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    bytes,
                    &sha256,
                )
                .unwrap();
        }

        fn store_native_at_platform_path(
            registry: &LocalRegistry,
            name: &str,
            version: &str,
            platform: &str,
            bytes: &[u8],
        ) {
            let artifact_path = registry.artifact_path_for_platform(name, version, platform);
            std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
            std::fs::write(&artifact_path, bytes).unwrap();
            let sha256 = sha256_hex(bytes);
            let sha256_path = artifact_path
                .parent()
                .unwrap()
                .join(format!("{name}-{version}-{platform}.sha256"));
            std::fs::write(&sha256_path, sha256.as_bytes()).unwrap();
            let meta_path = artifact_path
                .parent()
                .unwrap()
                .join(format!("{name}-{version}.meta.json"));
            let meta = serde_json::json!({
                "meta": {
                    "name": name, "version": version,
                    "runtime": "native", "artifact_runtime": "native",
                    "platforms": [], "description": null, "tags": []
                }
            });
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        }

        fn store_native_at_generic_path(
            registry: &LocalRegistry,
            name: &str,
            version: &str,
            bytes: &[u8],
        ) {
            let artifact_path = registry.artifact_path_for(name, version);
            std::fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
            std::fs::write(&artifact_path, bytes).unwrap();
            let sha256 = sha256_hex(bytes);
            let sha256_path = artifact_path
                .parent()
                .unwrap()
                .join(format!("{name}-{version}.sha256"));
            std::fs::write(&sha256_path, sha256.as_bytes()).unwrap();
            let meta_path = artifact_path
                .parent()
                .unwrap()
                .join(format!("{name}-{version}.meta.json"));
            let meta = serde_json::json!({
                "meta": {
                    "name": name, "version": version,
                    "runtime": "native", "artifact_runtime": "native",
                    "platforms": [], "description": null, "tags": []
                }
            });
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap()).unwrap();
        }

        struct MockSource {
            result: Result<Vec<u8>, SourceError>,
            called: Arc<AtomicBool>,
        }

        impl ArtifactSource for MockSource {
            fn name(&self) -> &str {
                "mock"
            }

            fn resolve_bare(&self, _name: &str) -> Result<(Bytes, String), SourceError> {
                self.called.store(true, Ordering::SeqCst);
                self.result
                    .clone()
                    .map(|v| (Bytes::from(v), "0.0.0".to_string()))
            }

            fn resolve_bare_with_version_for_platform(
                &self,
                _name: &str,
                _version: &str,
                _platform: &str,
            ) -> Result<Vec<u8>, SourceError> {
                self.called.store(true, Ordering::SeqCst);
                self.result.clone()
            }
        }

        fn mock_chain(result: Result<Vec<u8>, SourceError>) -> (SourceChain, Arc<AtomicBool>) {
            let called = Arc::new(AtomicBool::new(false));
            let chain = SourceChain::from_sources_for_test(
                vec![Box::new(MockSource {
                    result,
                    called: called.clone(),
                })],
                Vec::<SourceConfig>::new(),
            );
            (chain, called)
        }

        #[test]
        fn wasm_artifact_present_locally_staged_without_pull() {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());
            let bytes = make_zip("my-tool", "1.0.0");
            store_wasm(&registry, "my-tool", "1.0.0", &bytes);

            let (chain, called) = mock_chain(Err(SourceError::NotFound(
                "should not be called".to_string(),
            )));

            let staged =
                ensure_artifact_for_deploy("my-tool", "1.0.0", "linux-x86_64", &registry, &chain)
                    .unwrap();

            assert_eq!(staged.name, "my-tool");
            assert_eq!(staged.version, "1.0.0");
            assert_eq!(staged.bytes, bytes);
            assert!(
                !called.load(Ordering::SeqCst),
                "source chain must not be called for local WASM"
            );
        }

        #[test]
        fn wasm_artifact_missing_locally_pulled_and_staged() {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());
            let bytes = make_zip("my-tool", "1.0.0");

            let (chain, called) = mock_chain(Ok(bytes.clone()));

            let staged =
                ensure_artifact_for_deploy("my-tool", "1.0.0", "linux-x86_64", &registry, &chain)
                    .unwrap();

            assert_eq!(staged.bytes, bytes);
            assert!(
                called.load(Ordering::SeqCst),
                "source chain must be called when artifact is missing"
            );
        }

        #[test]
        fn native_artifact_different_platform_linux_variant_pulled() {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());

            let darwin_bytes = b"darwin-binary";
            store_native_at_platform_path(
                &registry,
                "my-tool",
                "1.0.0",
                "darwin-aarch64",
                darwin_bytes,
            );

            let linux_bytes = b"linux-binary".to_vec();
            let (chain, called) = mock_chain(Ok(linux_bytes.clone()));

            let staged =
                ensure_artifact_for_deploy("my-tool", "1.0.0", "linux-x86_64", &registry, &chain)
                    .unwrap();

            assert_eq!(staged.bytes, linux_bytes);
            assert!(
                called.load(Ordering::SeqCst),
                "chain must be called to fetch the linux variant"
            );

            let cached = registry.artifact_path_for_platform("my-tool", "1.0.0", "linux-x86_64");
            assert!(cached.exists(), "linux binary should be cached locally");
        }

        #[test]
        fn native_artifact_same_platform_local_generic_used() {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());

            let bytes = b"native-binary";
            store_native_at_generic_path(&registry, "my-tool", "1.0.0", bytes);

            let (chain, called) = mock_chain(Err(SourceError::NotFound(
                "should not be called".to_string(),
            )));

            let target = current_platform();
            let staged =
                ensure_artifact_for_deploy("my-tool", "1.0.0", target, &registry, &chain).unwrap();

            assert_eq!(&staged.bytes[..], bytes);
            assert!(
                !called.load(Ordering::SeqCst),
                "chain must not be called for same-platform native"
            );
        }

        #[test]
        fn pull_failure_returns_cli_error_with_artifact_name_and_version() {
            let dir = tempdir().unwrap();
            let registry = LocalRegistry::new(dir.path());

            let (chain, _) = mock_chain(Err(SourceError::NotFound("404".to_string())));

            let err = ensure_artifact_for_deploy(
                "missing-tool",
                "9.9.9",
                "linux-x86_64",
                &registry,
                &chain,
            )
            .unwrap_err();

            assert!(
                err.message.contains("missing-tool"),
                "error must name the artifact: {}",
                err.message
            );
            assert!(
                err.message.contains("9.9.9"),
                "error must include the version: {}",
                err.message
            );
        }
    }
}
