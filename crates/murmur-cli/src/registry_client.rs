use bytes::Bytes;
use murmur_artifact::{
    ArtifactMeta, LocalRegistry, PublishResult, Registry, RegistryError, ResolvedArtifact, RuntimeType,
};
use serde::Deserialize;
use ureq::http::StatusCode;

pub struct RemoteRegistry {
    base_url: String,
    api_key: String,
    client: ureq::Agent,
}

/// Blocking HTTP agent matching the previous reqwest::blocking behavior:
/// total request timeout, redirects followed, non-2xx statuses returned as
/// responses (not errors) so call sites can match on them.
pub(crate) fn blocking_agent(timeout: std::time::Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

impl RemoteRegistry {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            client: blocking_agent(std::time::Duration::from_secs(30)),
        }
    }

    fn artifacts_url(&self) -> String {
        format!("{}/v1/artifacts", self.base_url)
    }

    fn artifact_url(&self, name: &str, version: &str) -> String {
        format!("{}/v1/artifacts/{}/{}", self.base_url, name, version)
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn resolve_impl(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        // Platform strings are assumed to be URL-safe (canonical form: os-arch, e.g.
        // "darwin-aarch64"). Do not pass arbitrary user strings without encoding.
        let url = match platform {
            Some(p) => format!("{}/v1/artifacts/{}/{}?platform={}", self.base_url, name, version, p),
            None => self.artifact_url(name, version),
        };

        let mut response = self
            .client
            .get(&url)
            .header("authorization", self.bearer())
            .call()
            .map_err(|error| {
                RegistryError::InvalidInput(format!(
                    "failed to download {name}@{version} from {}: {error}",
                    self.base_url
                ))
            })?;

        match response.status() {
            StatusCode::OK => {
                let expected_sha = response
                    .headers()
                    .get("x-murmur-sha256")
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        RegistryError::InvalidInput(
                            "registry response missing x-murmur-sha256 header".to_string(),
                        )
                    })?
                    .to_string();

                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(u64::MAX)
                    .read_to_vec()
                    .map(Bytes::from)
                    .map_err(|error| {
                        RegistryError::InvalidInput(format!(
                            "failed to read artifact bytes for {name}@{version}: {error}"
                        ))
                    })?;

                // meta.runtime is not returned by the remote registry endpoint; read it
                // from murmur.yaml inside the zip. RuntimeType::Wasm here is a placeholder.
                Ok(ResolvedArtifact {
                    meta: ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime: RuntimeType::Wasm,
                        artifact_runtime: String::new(),
                        platforms: Vec::new(),
                        description: None,
                        tags: Vec::new(),
                    },
                    bytes,
                    sha256: expected_sha,
                })
            }
            StatusCode::NOT_FOUND => Err(RegistryError::NotFound {
                name: name.to_string(),
                version: version.to_string(),
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(auth_error(&self.base_url))
            }
            status => Err(RegistryError::InvalidInput(format!(
                "download failed with HTTP {}: {}",
                status,
                extract_server_error(response)
                    .unwrap_or_else(|| "unknown registry error".to_string())
            ))),
        }
    }
}

impl Registry for RemoteRegistry {
    fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError> {
        self.resolve_impl(name, version, None)
    }

    fn resolve_with_platform(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        self.resolve_impl(name, version, platform)
    }

    fn publish(
        &self,
        meta: ArtifactMeta,
        bytes: &[u8],
    ) -> Result<murmur_artifact::PublishResult, RegistryError> {
        let mut query: Vec<(String, String)> = vec![
            ("name".to_string(), meta.name.clone()),
            ("version".to_string(), meta.version.clone()),
            (
                "runtime".to_string(),
                meta.runtime.as_str().to_string(),
            ),
        ];

        for (os, arch) in &meta.platforms {
            query.push(("platform".to_string(), format!("{os}-{arch}")));
        }

        let mut response = self
            .client
            .post(self.artifacts_url())
            .header("authorization", self.bearer())
            .query_pairs(query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .send(bytes)
            .map_err(|error| {
                RegistryError::InvalidInput(format!(
                    "failed to publish {}@{} via registry {}: {}",
                    meta.name, meta.version, self.base_url, error
                ))
            })?;

        match response.status() {
            StatusCode::CREATED => {
                #[derive(Deserialize)]
                struct PublishBody {
                    artifact_id: String,
                    sha256: String,
                }

                let published = response
                    .body_mut()
                    .read_json::<PublishBody>()
                    .map_err(|error| {
                        RegistryError::InvalidInput(format!(
                            "invalid publish response from {}: {}",
                            self.base_url, error
                        ))
                    })?;

                Ok(murmur_artifact::PublishResult {
                    artifact_id: published.artifact_id,
                    sha256: published.sha256,
                })
            }
            StatusCode::CONFLICT => Err(RegistryError::Conflict {
                name: meta.name,
                version: meta.version,
            }),
            StatusCode::UNPROCESSABLE_ENTITY => {
                let server_message = extract_server_error(response)
                    .unwrap_or_else(|| "artifact rejected by registry".to_string());
                if let Some(version) = parse_reserved_version_message(&server_message) {
                    Err(RegistryError::ReservedVersion(version.to_string()))
                } else {
                    Err(RegistryError::InvalidInput(server_message))
                }
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(auth_error(&self.base_url))
            }
            status => Err(RegistryError::InvalidInput(format!(
                "publish failed with HTTP {}: {}",
                status,
                extract_server_error(response)
                    .unwrap_or_else(|| "unknown registry error".to_string())
            ))),
        }
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        let mut response = self
            .client
            .get(&self.artifacts_url())
            .header("authorization", self.bearer())
            .call()
            .map_err(|error| {
                RegistryError::InvalidInput(format!(
                    "failed to list artifacts from {}: {}",
                    self.base_url, error
                ))
            })?;

        match response.status() {
            StatusCode::OK => response.body_mut().read_json().map_err(|error| {
                RegistryError::InvalidInput(format!(
                    "invalid index response from {}: {}",
                    self.base_url, error
                ))
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(auth_error(&self.base_url))
            }
            status => Err(RegistryError::InvalidInput(format!(
                "list failed with HTTP {}: {}",
                status,
                extract_server_error(response)
                    .unwrap_or_else(|| "unknown registry error".to_string())
            ))),
        }
    }
}

fn auth_error(base_url: &str) -> RegistryError {
    RegistryError::InvalidInput(format!(
        "registry authentication failed for {base_url}. Check NEXUS_API_KEY and try again."
    ))
}

#[derive(Debug, Deserialize)]
struct RegistryErrorBody {
    error: String,
}

fn extract_server_error(mut response: ureq::http::Response<ureq::Body>) -> Option<String> {
    let body = response.body_mut().read_to_string().ok()?;
    if body.trim().is_empty() {
        return None;
    }

    serde_json::from_str::<RegistryErrorBody>(&body)
        .map(|err| err.error)
        .ok()
        .or(Some(body))
}

fn parse_reserved_version_message(message: &str) -> Option<&str> {
    let prefix = "reserved artifact version '";
    let suffix = "' is not allowed";
    let version = message.strip_prefix(prefix)?.strip_suffix(suffix)?;
    Some(version)
}

/// Resolves against `primary` first and, on `NotFound` only, falls back to `secondary`.
///
/// This is how a session is meant to find its artifacts: the project store first, then the
/// global one. Both `mur run` and `mur eval` stage against it, so an artifact reachable by the
/// pre-flight check in `commands::run::artifact_presence` is also reachable by the staging that
/// follows — the two disagreeing is what let `mur publish` + `mur run` report an artifact as
/// present and then fail to stage it.
///
/// Only `NotFound` falls through. Any other error is a real failure of the primary store and is
/// returned as such rather than being masked by a lookup somewhere else.
pub(crate) struct FallbackRegistry {
    pub(crate) primary: LocalRegistry,
    pub(crate) secondary: LocalRegistry,
}

impl Registry for FallbackRegistry {
    fn resolve(&self, name: &str, version: &str) -> Result<ResolvedArtifact, RegistryError> {
        match self.primary.resolve(name, version) {
            Err(RegistryError::NotFound { .. }) => self.secondary.resolve(name, version),
            other => other,
        }
    }

    fn resolve_with_platform(
        &self,
        name: &str,
        version: &str,
        platform: Option<&str>,
    ) -> Result<ResolvedArtifact, RegistryError> {
        match self.primary.resolve_with_platform(name, version, platform) {
            Err(RegistryError::NotFound { .. }) => {
                self.secondary.resolve_with_platform(name, version, platform)
            }
            other => other,
        }
    }

    fn publish(&self, meta: ArtifactMeta, bytes: &[u8]) -> Result<PublishResult, RegistryError> {
        self.primary.publish(meta, bytes)
    }

    fn list_index(&self) -> Result<Vec<ArtifactMeta>, RegistryError> {
        self.primary.list_index()
    }
}
