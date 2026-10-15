use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use indicatif::ProgressBar;
use murmur_artifact::{ArtifactIndex, ARTIFACT_FILE_SUFFIX, ARTIFACT_INDEX_FILE_NAME};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use ureq::Agent;

use crate::http::blocking_agent;

use super::{ArtifactSource, PublishedIndex, RateLimitSignal, SourceError};

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

/// One GitHub repository as the source of `scope`, through the REST API at
/// `MUR_GITHUB_API_BASE` (default `https://api.github.com`).
///
/// The repository's owner, its latest release, its index and every other release looked up are
/// each requested at most once and kept for the life of the command, so threads fetching
/// artifacts of one scope at once share them.
pub(crate) struct GitHubSource {
    label: String,
    owner: String,
    repo_name: String,
    /// `<owner>/<repo>`.
    repo: String,
    scope: String,
    token: Option<String>,
    client: Agent,
    api_base: String,
    owner_id: OnceLock<Result<u64, SourceError>>,
    latest: OnceLock<Result<GitHubRelease, SourceError>>,
    index: OnceLock<Result<Arc<PublishedIndex>, SourceError>>,
    /// Releases looked up by tag. Held across the request, so two threads asking for one tag make
    /// one request.
    tagged: Mutex<HashMap<String, Result<GitHubRelease, SourceError>>>,
}

/// One `.mur.zip` asset of a release, as a `github:<owner>/<repo>@<tag>` install takes it.
pub(crate) struct ReleasePayload {
    pub file: String,
    pub bytes: Bytes,
}

impl GitHubSource {
    /// The source `owner/repo` for `scope`. Makes no request.
    pub(crate) fn new(owner: &str, repo: &str, scope: &str, token: Option<String>) -> Self {
        Self {
            label: format!("github:{owner}/{repo}"),
            owner: owner.to_string(),
            repo_name: repo.to_string(),
            repo: format!("{owner}/{repo}"),
            scope: scope.to_string(),
            token,
            client: blocking_agent(std::time::Duration::from_secs(30)),
            api_base: github_api_base(),
            owner_id: OnceLock::new(),
            latest: OnceLock::new(),
            index: OnceLock::new(),
            tagged: Mutex::new(HashMap::new()),
        }
    }

    /// Every `.mur.zip` asset of release `tag`, downloaded.
    pub(crate) fn release_payloads(&self, tag: &str) -> Result<Vec<ReleasePayload>, SourceError> {
        let release = self.release(tag)?;
        let assets: Vec<&GitHubReleaseAsset> = release
            .assets
            .iter()
            .filter(|asset| asset.name.ends_with(ARTIFACT_FILE_SUFFIX))
            .collect();
        if assets.is_empty() {
            return Err(SourceError::NotFound(format!(
                "no {ARTIFACT_FILE_SUFFIX} assets found in github release {}@{tag}",
                self.repo
            )));
        }
        assets
            .into_iter()
            .map(|asset| {
                Ok(ReleasePayload {
                    file: asset.name.clone(),
                    bytes: self.download_asset(asset, true)?,
                })
            })
            .collect()
    }

    fn latest_release(&self) -> Result<&GitHubRelease, SourceError> {
        self.latest
            .get_or_init(|| self.get_json(&self.repo_url("releases/latest")))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Release `tag`: the latest release when it is that tag, else one lookup by tag.
    fn release(&self, tag: &str) -> Result<GitHubRelease, SourceError> {
        if let Some(Ok(latest)) = self.latest.get() {
            if latest.tag_name == tag {
                return Ok(latest.clone());
            }
        }
        let mut tagged = self.tagged.lock().unwrap_or_else(|e| e.into_inner());
        tagged
            .entry(tag.to_string())
            .or_insert_with(|| {
                self.get_json(&self.repo_url(&format!("releases/tags/{tag}")))
                    .map_err(|error| match error {
                        SourceError::NotFound(_) => {
                            SourceError::NotFound(format!("{} has no release {tag}", self.label))
                        }
                        error => error,
                    })
            })
            .clone()
    }

    fn read_index(&self) -> Result<Arc<PublishedIndex>, SourceError> {
        let latest = self.latest_release().map_err(|error| match error {
            SourceError::NotFound(_) => SourceError::NoIndex { release: None },
            error => error,
        })?;
        let release = latest.tag_name.clone();
        let Some(asset) = latest
            .assets
            .iter()
            .find(|asset| asset.name == ARTIFACT_INDEX_FILE_NAME)
        else {
            return Err(SourceError::NoIndex {
                release: Some(release),
            });
        };
        let bytes = self.download_asset(asset, false)?;
        let text = String::from_utf8_lossy(&bytes);
        let index = ArtifactIndex::from_json(&text).map_err(|error| SourceError::BadIndex {
            release: release.clone(),
            error,
        })?;
        Ok(Arc::new(PublishedIndex { release, index }))
    }

    fn repo_url(&self, path: &str) -> String {
        format!(
            "{}/repos/{}/{}/{path}",
            self.api_base, self.owner, self.repo_name
        )
    }

    /// A JSON answer. A 404 is [`SourceError::NotFound`].
    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, SourceError> {
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
            return Err(SourceError::NotFound(format!("{url} not found")));
        }

        if !status.is_success() {
            return Err(self.http_failure(&mut response, "request failed"));
        }

        response
            .body_mut()
            .read_json::<T>()
            .map_err(|error| SourceError::Other(format!("invalid GitHub API response: {error}")))
    }

    /// `progress` streams the body through the thread's download progress bar, if one is
    /// registered: a payload's, never the index's, so the bar counts one file.
    fn download_asset(
        &self,
        asset: &GitHubReleaseAsset,
        progress: bool,
    ) -> Result<Bytes, SourceError> {
        // Use the GitHub API asset endpoint with Accept: application/octet-stream so that
        // auth is handled correctly for private repos. browser_download_url redirects to a
        // CDN and the Bearer token is stripped on the cross-host redirect.
        let url = self.repo_url(&format!("releases/assets/{}", asset.id));

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
            return Err(self.http_failure(&mut response, "asset download failed"));
        }

        // If a progress bar is registered for this thread, stream the response body through
        // it so indicatif can show live bytes/total and a progress bar.
        // ureq's BodyReader implements std::io::Read, and ProgressBar::wrap_read
        // wraps any Read to call pb.inc(n) for every chunk — no manual polling needed.
        let pb: Option<ProgressBar> = if progress {
            DL_PROGRESS.with(|cell| cell.borrow().clone())
        } else {
            None
        };
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

    /// The error for a non-success response other than a 404 of a JSON lookup. `unreadable` is
    /// the message used when the body cannot be read.
    fn http_failure(
        &self,
        response: &mut ureq::http::Response<ureq::Body>,
        unreadable: &str,
    ) -> SourceError {
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let remaining = header("x-ratelimit-remaining");
        let reset = header("x-ratelimit-reset");
        let retry_after = header("retry-after");
        let body = response
            .body_mut()
            .read_to_string()
            .unwrap_or_else(|_| unreadable.to_string());
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        classify_http_failure(
            status,
            RateLimitHeaders {
                remaining: remaining.as_deref(),
                reset: reset.as_deref(),
                retry_after: retry_after.as_deref(),
            },
            body,
            self.effective_token().is_some(),
            now_unix,
        )
    }

    fn effective_token(&self) -> Option<String> {
        self.token.clone().or_else(|| {
            env::var("GITHUB_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    }
}

impl ArtifactSource for GitHubSource {
    fn label(&self) -> &str {
        &self.label
    }

    fn repo(&self) -> &str {
        &self.repo
    }

    fn scope(&self) -> &str {
        &self.scope
    }

    /// `owner.id` of `GET /repos/<owner>/<repo>`, which a public repository answers without a
    /// token.
    fn owner_id(&self) -> Result<u64, SourceError> {
        self.owner_id
            .get_or_init(|| {
                let url = format!("{}/repos/{}", self.api_base, self.repo);
                self.get_json::<GitHubRepository>(&url)
                    .map(|repository| repository.owner.id)
            })
            .clone()
    }

    fn index(&self) -> Result<Arc<PublishedIndex>, SourceError> {
        self.index.get_or_init(|| self.read_index()).clone()
    }

    fn fetch(&self, release: &str, file: &str) -> Result<Bytes, SourceError> {
        let found = self.release(release)?;
        let asset = found
            .assets
            .iter()
            .find(|asset| asset.name == file)
            .ok_or_else(|| {
                SourceError::NotFound(format!("release {release} has no asset {file}"))
            })?;
        self.download_asset(asset, true)
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRepository {
    owner: GitHubOwner,
}

#[derive(Debug, Deserialize)]
struct GitHubOwner {
    id: u64,
}

#[derive(Debug, Clone, Deserialize)]
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

/// The raw rate-limit header values of a GitHub response, `None` for a header that is absent.
#[derive(Debug, Clone, Copy, Default)]
struct RateLimitHeaders<'a> {
    /// `x-ratelimit-remaining`: requests left in the current window.
    remaining: Option<&'a str>,
    /// `x-ratelimit-reset`: unix seconds at which the window resets.
    reset: Option<&'a str>,
    /// `retry-after`: seconds to wait, sent with GitHub's secondary rate limits.
    retry_after: Option<&'a str>,
}

/// Classifies a non-success GitHub response as [`SourceError::RateLimited`] or
/// [`SourceError::Http`].
///
/// A 429 is always a rate limit. A 403 is one when `x-ratelimit-remaining` is `0`, `retry-after`
/// is present, or — checked last, because it is prose GitHub may reword — the body mentions a rate
/// limit. GitHub also answers 403 for SAML enforcement and blocked tokens, which stay `Http` and
/// must never get rate-limit advice. The message is the body's JSON `message` when it has
/// one, else the trimmed body, flattened to one line.
fn classify_http_failure(
    status: u16,
    headers: RateLimitHeaders<'_>,
    body: String,
    authenticated: bool,
    now_unix: u64,
) -> SourceError {
    let message = one_line_message(&body);

    let signal = if status == 429 {
        RateLimitSignal::Status
    } else if status != 403 {
        return SourceError::Http { status, message };
    } else if headers.remaining.map(str::trim) == Some("0") {
        RateLimitSignal::RemainingHeader
    } else if headers.retry_after.is_some() {
        RateLimitSignal::RetryAfterHeader
    } else if body.to_ascii_lowercase().contains("rate limit") {
        RateLimitSignal::Body
    } else {
        return SourceError::Http { status, message };
    };

    let resets_in_secs = headers
        .retry_after
        .and_then(|retry_after| retry_after.trim().parse::<u64>().ok())
        .or_else(|| {
            headers
                .reset
                .and_then(|reset| reset.trim().parse::<u64>().ok())
                .map(|reset| reset.saturating_sub(now_unix))
        });

    SourceError::RateLimited {
        status,
        message,
        signal,
        resets_in_secs,
        authenticated,
    }
}

fn one_line_message(body: &str) -> String {
    let extracted = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("message")?.as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().to_string());
    extracted.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn github_api_base() -> String {
    env::var("MUR_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.github.com".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    const RATE_LIMIT_BODY: &str = "{\"message\":\"API rate limit exceeded for 127.0.0.1.\",\"documentation_url\":\"https://docs.github.com/rest/overview/resources-in-the-rest-api#rate-limiting\"}";

    fn classify(status: u16, headers: RateLimitHeaders<'_>, body: &str) -> SourceError {
        classify_http_failure(status, headers, body.to_string(), false, NOW)
    }

    #[test]
    fn a_403_with_no_requests_remaining_is_a_rate_limit_reset_from_the_header() {
        let reset = (NOW + 600).to_string();
        let error = classify(
            403,
            RateLimitHeaders {
                remaining: Some("0"),
                reset: Some(&reset),
                retry_after: None,
            },
            RATE_LIMIT_BODY,
        );
        match error {
            SourceError::RateLimited {
                status,
                message,
                signal,
                resets_in_secs,
                authenticated,
            } => {
                assert_eq!(status, 403);
                assert_eq!(signal, RateLimitSignal::RemainingHeader);
                assert_eq!(message, "API rate limit exceeded for 127.0.0.1.");
                assert_eq!(resets_in_secs, Some(600));
                assert!(!authenticated);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn a_403_whose_body_names_a_rate_limit_is_one_without_headers() {
        let error = classify(
            403,
            RateLimitHeaders::default(),
            "{\"message\":\"You have exceeded a secondary Rate Limit.\"}",
        );
        assert!(
            matches!(
                error,
                SourceError::RateLimited {
                    signal: RateLimitSignal::Body,
                    resets_in_secs: None,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error
                .reason()
                .starts_with("rate limited by GitHub (HTTP 403, response body says rate limit): "),
            "{}",
            error.reason()
        );
    }

    #[test]
    fn retry_after_gives_the_reset() {
        let error = classify(
            403,
            RateLimitHeaders {
                retry_after: Some("30"),
                ..RateLimitHeaders::default()
            },
            "{\"message\":\"slow down\"}",
        );
        assert!(
            matches!(
                error,
                SourceError::RateLimited {
                    signal: RateLimitSignal::RetryAfterHeader,
                    resets_in_secs: Some(30),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error.reason().ends_with("resets in about 1 minute"),
            "{}",
            error.reason()
        );
    }

    #[test]
    fn a_429_is_always_a_rate_limit() {
        let error = classify_http_failure(
            429,
            RateLimitHeaders::default(),
            "too many".to_string(),
            true,
            NOW,
        );
        assert!(
            matches!(
                error,
                SourceError::RateLimited {
                    status: 429,
                    signal: RateLimitSignal::Status,
                    authenticated: true,
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_saml_403_with_requests_remaining_is_not_a_rate_limit() {
        let error = classify(
            403,
            RateLimitHeaders {
                remaining: Some("4999"),
                ..RateLimitHeaders::default()
            },
            "{\"message\":\"Resource protected by organization SAML enforcement. You must grant your Personal Access token access to this organization.\"}",
        );
        match error {
            SourceError::Http { status, message } => {
                assert_eq!(status, 403);
                assert!(message.starts_with("Resource protected by organization SAML"));
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn a_500_is_an_http_failure() {
        let error = classify(500, RateLimitHeaders::default(), "{\"message\":\"boom\"}");
        assert!(
            matches!(&error, SourceError::Http { status: 500, message } if message == "boom"),
            "{error:?}"
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_used_trimmed() {
        let error = classify(502, RateLimitHeaders::default(), "  Bad Gateway \n");
        assert!(
            matches!(&error, SourceError::Http { message, .. } if message == "Bad Gateway"),
            "{error:?}"
        );
    }

    #[test]
    fn a_multi_line_body_becomes_one_line() {
        let error = classify(
            403,
            RateLimitHeaders {
                remaining: Some("0"),
                ..RateLimitHeaders::default()
            },
            "<html>\n<body>\r\nrate limit\n</body>\n</html>",
        );
        let reason = error.reason();
        assert!(
            !reason.contains('\n') && !reason.contains('\r'),
            "{reason:?}"
        );
        assert!(
            reason.contains("<html> <body> rate limit </body> </html>"),
            "{reason}"
        );
    }

    #[test]
    fn a_reset_in_the_past_is_zero_seconds_away() {
        let reset = (NOW - 5).to_string();
        let error = classify(
            403,
            RateLimitHeaders {
                remaining: Some("0"),
                reset: Some(&reset),
                retry_after: None,
            },
            RATE_LIMIT_BODY,
        );
        assert!(
            matches!(
                error,
                SourceError::RateLimited {
                    resets_in_secs: Some(0),
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(
            error.reason().ends_with("resets in about 1 minute"),
            "{}",
            error.reason()
        );
    }
}
