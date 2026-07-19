use std::cell::RefCell;
use std::env;

use bytes::Bytes;
use indicatif::ProgressBar;
use murmur_artifact::current_platform;
use serde::Deserialize;
use ureq::Agent;

use crate::registry_client::blocking_agent;

use super::{ArtifactSource, SourceError};

// ── Per-thread download progress ─────────────────────────────────────────────
//
// deploy.rs registers a ProgressBar on the current thread before calling into
// ensure_artifact_for_deploy.  download_asset picks it up, sets the content-
// length, and wraps the response body so indicatif ticks the bar automatically
// as bytes stream in.  Rayon's par_iter gives each artifact its own OS thread,
// so every artifact gets a separate ProgressBar with no locking needed.

thread_local! {
    static DL_PROGRESS: RefCell<Option<ProgressBar>> = const { RefCell::new(None) };
}

/// Register a progress bar for artifact downloads on the current thread.
/// The bar should already have the download style set (bytes template + bar).
/// Call before the download; clear with `pop_download_progress` after.
pub fn push_download_progress(pb: ProgressBar) {
    DL_PROGRESS.with(|cell| *cell.borrow_mut() = Some(pb));
}

/// Remove the thread-local download progress bar after a download completes or fails.
pub fn pop_download_progress() {
    DL_PROGRESS.with(|cell| *cell.borrow_mut() = None);
}

pub struct GitHubSource {
    source_name: String,
    owner: String,
    repo: String,
    token: Option<String>,
    explicit_tag: Option<String>,
    client: Agent,
    api_base: String,
}

pub struct GitHubDirectResolution {
    pub bytes: Bytes,
    pub tag: String,
}

impl GitHubSource {
    pub fn from_config(name: &str, repo: &str, token: Option<String>) -> Result<Self, SourceError> {
        let Some((owner, repository)) = repo.split_once('/') else {
            return Err(SourceError::Config(format!(
                "invalid github repo '{}' (expected owner/repo)",
                repo
            )));
        };

        if owner.trim().is_empty() || repository.trim().is_empty() {
            return Err(SourceError::Config(format!(
                "invalid github repo '{}' (expected owner/repo)",
                repo
            )));
        }

        let _ = name;

        Ok(Self {
            source_name: format!("github:{owner}/{repository}"),
            owner: owner.to_string(),
            repo: repository.to_string(),
            token,
            explicit_tag: None,
            client: blocking_agent(std::time::Duration::from_secs(30)),
            api_base: github_api_base(),
        })
    }

    pub fn explicit(owner: &str, repo: &str, tag: &str, token: Option<String>) -> Self {
        Self {
            source_name: format!("github:{owner}/{repo}"),
            owner: owner.to_string(),
            repo: repo.to_string(),
            token,
            explicit_tag: Some(tag.to_string()),
            client: blocking_agent(std::time::Duration::from_secs(30)),
            api_base: github_api_base(),
        }
    }

    pub fn resolve_all_release_assets_by_tag(
        &self,
    ) -> Result<Vec<GitHubDirectResolution>, SourceError> {
        let Some(explicit_tag) = self.explicit_tag.as_deref() else {
            return Err(SourceError::Config(
                "github explicit resolution requires a tag".to_string(),
            ));
        };

        let release = self.fetch_release_by_exact_tag(explicit_tag)?;
        let zip_assets: Vec<_> = release
            .assets
            .iter()
            .filter(|a| a.name.ends_with(".mur.zip"))
            .cloned()
            .collect();

        if zip_assets.is_empty() {
            return Err(SourceError::NotFound(format!(
                "no .mur.zip assets found in github release {}@{}",
                self.repo_label(),
                release.tag_name
            )));
        }

        let tag = if release.tag_name.is_empty() {
            explicit_tag.to_string()
        } else {
            release.tag_name.clone()
        };

        zip_assets
            .iter()
            .map(|asset| {
                let bytes = self.download_asset(asset)?;
                Ok(GitHubDirectResolution {
                    bytes,
                    tag: tag.clone(),
                })
            })
            .collect()
    }

    fn resolve_bare_internal(
        &self,
        artifact_name: &str,
        version_hint: Option<&str>,
    ) -> Result<(Bytes, String), SourceError> {
        if let Some(version) = version_hint {
            // Primary: look in the release whose tag matches the artifact version.
            // Use select_versioned_asset (exact name match) so that a workspace-level release
            // tag (e.g. v0.3.33) that contains an older artifact zip (e.g. name-0.3.31.mur.zip)
            // does not silently shadow the correct version from the latest release.
            let primary = match self.fetch_release_by_version_hint(version) {
                Ok(release) => {
                    match select_versioned_asset(&release.assets, artifact_name, version, current_platform()) {
                        Some(asset) => {
                            let bytes = self.download_asset(&asset)?;
                            return Ok((bytes, release.tag_name));
                        }
                        None => Some(release.tag_name.clone()),  // release found, artifact absent
                    }
                }
                Err(SourceError::NotFound(_)) => None,  // no release with this tag
                Err(e) => return Err(e),
            };

            // Fallback: the repo uses a monotonic release tag independent of artifact
            // versions (e.g. v0.3.41 contains murmur-driver-openai@0.3.33).  Search the
            // latest release for an asset whose name pins the requested version exactly:
            //   {name}-{version}.mur.zip  or  {name}-{version}-{platform}.mur.zip
            let latest = self.fetch_latest_release()?;
            match select_versioned_asset(&latest.assets, artifact_name, version, current_platform()) {
                Some(asset) => {
                    let bytes = self.download_asset(&asset)?;
                    Ok((bytes, latest.tag_name))
                }
                None => Err(SourceError::NotFound(format!(
                    "asset '{artifact_name}.mur.zip' not found in release '{}'{} or latest '{}'",
                    primary.as_deref().unwrap_or(version),
                    if primary.is_none() { " (no such release)" } else { "" },
                    latest.tag_name,
                ))),
            }
        } else {
            let release = self.fetch_latest_release()?;
            let asset =
                select_asset_for_artifact(&release.assets, artifact_name, current_platform())
                    .ok_or_else(|| {
                        SourceError::NotFound("asset not found in latest release".to_string())
                    })?;
            let bytes = self.download_asset(&asset)?;
            Ok((bytes, release.tag_name))
        }
    }

    fn fetch_latest_release(&self) -> Result<GitHubRelease, SourceError> {
        self.fetch_release_json(&format!(
            "{}/repos/{}/{}/releases/latest",
            self.api_base, self.owner, self.repo
        ))
    }

    fn fetch_release_by_version_hint(&self, version: &str) -> Result<GitHubRelease, SourceError> {
        let mut tags = vec![version.to_string()];
        if !version.starts_with('v') {
            tags.push(format!("v{version}"));
        }

        let mut last_not_found = None;

        for tag in tags {
            match self.fetch_release_by_exact_tag(&tag) {
                Ok(release) => return Ok(release),
                Err(SourceError::NotFound(message)) => last_not_found = Some(message),
                Err(other) => return Err(other),
            }
        }

        Err(SourceError::NotFound(last_not_found.unwrap_or_else(|| {
            format!("release tag '{version}' not found")
        })))
    }

    fn fetch_release_by_exact_tag(&self, tag: &str) -> Result<GitHubRelease, SourceError> {
        self.fetch_release_json(&format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.api_base, self.owner, self.repo, tag
        ))
    }

    fn fetch_release_json(&self, url: &str) -> Result<GitHubRelease, SourceError> {
        let mut request = self
            .client
            .get(url)
            .header("user-agent", "murmur-cli")
            .header("accept", "application/vnd.github+json");

        if let Some(token) = self.effective_token() {
            request = request.header("authorization", format!("Bearer {token}"));
        }

        let mut response = request
            .call()
            .map_err(|error| SourceError::Other(format!("request failed: {error}")))?;

        let status = response.status();
        if status == ureq::http::StatusCode::NOT_FOUND {
            return Err(SourceError::NotFound("release not found".to_string()));
        }

        if !status.is_success() {
            let message = response
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| "request failed".to_string());
            return Err(SourceError::Http {
                status: status.as_u16(),
                message,
            });
        }

        response.body_mut().read_json::<GitHubRelease>().map_err(|error| {
            SourceError::Other(format!("invalid github release response: {error}"))
        })
    }

    fn download_asset(&self, asset: &GitHubReleaseAsset) -> Result<Bytes, SourceError> {
        // Use the GitHub API asset endpoint with Accept: application/octet-stream so that
        // auth is handled correctly for private repos. browser_download_url redirects to a
        // CDN and the Bearer token is stripped on the cross-host redirect.
        let url = format!(
            "{}/repos/{}/{}/releases/assets/{}",
            self.api_base, self.owner, self.repo, asset.id
        );

        let mut request = self
            .client
            .get(&url)
            .header("user-agent", "murmur-cli")
            .header("accept", "application/octet-stream");

        if let Some(token) = self.effective_token() {
            request = request.header("authorization", format!("Bearer {token}"));
        }

        let mut response = request
            .call()
            .map_err(|error| SourceError::Other(format!("asset download failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let message = response
                .body_mut()
                .read_to_string()
                .unwrap_or_else(|_| "asset download failed".to_string());
            return Err(SourceError::Http {
                status: status.as_u16(),
                message,
            });
        }

        // If a progress bar is registered for this thread, stream the response body through
        // it so indicatif can show live bytes/total and a progress bar.
        // ureq's BodyReader implements std::io::Read, and ProgressBar::wrap_read
        // wraps any Read to call pb.inc(n) for every chunk — no manual polling needed.
        let pb: Option<ProgressBar> = DL_PROGRESS.with(|cell| cell.borrow().clone());
        let content_length = response.body().content_length();
        if let Some(pb) = pb {
            if let Some(len) = content_length {
                pb.set_length(len);
            }
            let mut reader = pb.wrap_read(response.into_body().into_reader());
            let mut buf = Vec::with_capacity(content_length.unwrap_or(0) as usize);
            use std::io::Read;
            reader
                .read_to_end(&mut buf)
                .map(|_| Bytes::from(buf))
                .map_err(|error| SourceError::Other(format!("failed to read asset bytes: {error}")))
        } else {
            response
                .body_mut()
                .with_config()
                .limit(u64::MAX)
                .read_to_vec()
                .map(Bytes::from)
                .map_err(|error| SourceError::Other(format!("failed to read asset bytes: {error}")))
        }
    }

    fn effective_token(&self) -> Option<String> {
        self.token.clone().or_else(|| {
            env::var("GITHUB_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    }

    fn repo_label(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

impl ArtifactSource for GitHubSource {
    fn name(&self) -> &str {
        &self.source_name
    }

    fn resolve_bare(&self, name: &str) -> Result<(Bytes, String), SourceError> {
        self.resolve_bare_internal(name, None)
    }

    fn resolve_bare_with_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<(Bytes, String), SourceError> {
        if version == "latest" {
            return self.resolve_bare_internal(name, None);
        }
        self.resolve_bare_internal(name, Some(version))
    }

    fn resolve_bare_with_version_for_platform(
        &self,
        name: &str,
        version: &str,
        platform: &str,
    ) -> Result<Vec<u8>, SourceError> {
        if version == "latest" {
            let release = self.fetch_latest_release()?;
            let asset = select_asset_for_artifact(&release.assets, name, platform)
                .ok_or_else(|| {
                    SourceError::NotFound(format!(
                        "no asset for '{name}' (platform: {platform}) in latest release '{}'",
                        release.tag_name
                    ))
                })?;
            return self.download_asset(&asset).map(|b| b.to_vec());
        }

        // Primary: release tagged with the artifact version.
        let primary_tag = match self.fetch_release_by_version_hint(version) {
            Ok(release) => {
                if let Some(asset) = select_versioned_asset(&release.assets, name, version, platform) {
                    return self.download_asset(&asset).map(|b| b.to_vec());
                }
                Some(release.tag_name)  // release found, artifact absent
            }
            Err(SourceError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };

        // Fallback: latest release with version-pinned asset name.
        let latest = self.fetch_latest_release()?;
        match select_versioned_asset(&latest.assets, name, version, platform) {
            Some(asset) => self.download_asset(&asset).map(|b| b.to_vec()),
            None => Err(SourceError::NotFound(format!(
                "no asset for '{name}' v{version} (platform: {platform}) in release '{}'{} or latest '{}'",
                primary_tag.as_deref().unwrap_or(version),
                if primary_tag.is_none() { " (no such release)" } else { "" },
                latest.tag_name,
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubReleaseAsset {
    id: u64,
    name: String,
}

fn select_asset_for_artifact(
    assets: &[GitHubReleaseAsset],
    artifact_name: &str,
    platform: &str,
) -> Option<GitHubReleaseAsset> {
    // Prefer a platform-tagged variant so native tool artifacts resolve to the correct binary.
    // Pattern: <name>-<version>-<platform>.mur.zip  (e.g. murmur-tool-git-0.3.20-darwin-aarch64.mur.zip)
    let platform_suffix = format!("-{platform}.mur.zip");
    if let Some(asset) = assets.iter().find(|asset| {
        asset.name.starts_with(&format!("{artifact_name}-")) && asset.name.ends_with(&platform_suffix)
    }) {
        return Some(asset.clone());
    }

    // Fall back to an exact unplatformed name (WASM artifacts), then any versioned variant.
    let exact = format!("{artifact_name}.mur.zip");
    if let Some(asset) = assets.iter().find(|asset| asset.name == exact) {
        return Some(asset.clone());
    }

    assets
        .iter()
        .find(|asset| {
            asset.name.starts_with(&format!("{artifact_name}-")) && asset.name.ends_with(".mur.zip")
        })
        .cloned()
}

/// Find an asset that names a specific artifact version exactly.
/// Used when falling back to the latest release in repos where the release tag
/// is a monotonic build counter independent of individual artifact versions.
///
/// Matches (in order):
///   {name}-{version}-{platform}.mur.zip   (native, platform-specific)
///   {name}-{version}.mur.zip              (WASM, platform-independent)
fn select_versioned_asset(
    assets: &[GitHubReleaseAsset],
    artifact_name: &str,
    version: &str,
    platform: &str,
) -> Option<GitHubReleaseAsset> {
    let platform_name = format!("{artifact_name}-{version}-{platform}.mur.zip");
    if let Some(a) = assets.iter().find(|a| a.name == platform_name) {
        return Some(a.clone());
    }
    let generic_name = format!("{artifact_name}-{version}.mur.zip");
    assets.iter().find(|a| a.name == generic_name).cloned()
}

fn github_api_base() -> String {
    env::var("MUR_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}
