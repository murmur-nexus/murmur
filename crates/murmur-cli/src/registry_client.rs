use bytes::Bytes;
use murmur_artifact::{
    declared_runtime_from_artifact_bytes, split_platform_tag, ArtifactMeta, LocalRegistry,
    PlatformMatch, PublishResult, Registry, RegistryError, ResolvedArtifact, RuntimeType,
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
            Some(p) => format!(
                "{}/v1/artifacts/{}/{}?platform={}",
                self.base_url, name, version, p
            ),
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

                // The endpoint returns no packaging type, so it is read from the murmur.yaml
                // inside the bytes it did return: what the artifact says it is decides the
                // recorded value, on this install path as on every other.
                let declared = declared_runtime_from_artifact_bytes(&bytes).ok_or_else(|| {
                    RegistryError::InvalidInput(format!(
                        "artifact {name}@{version} downloaded from {} carries no readable \
                         murmur.yaml: the download is corrupt, or the registry served something \
                         that is not a .mur.zip",
                        self.base_url
                    ))
                })?;
                let runtime = declared.runtime;
                // The requested platform is the only platform information this exchange
                // carries: serving different bytes for an explicit `?platform=` would be a
                // server bug, and this is exactly what the local store will serve for that
                // platform once these bytes are installed.
                let platforms = match (runtime, platform.and_then(split_platform_tag)) {
                    (RuntimeType::Native, Some((os, arch))) => {
                        vec![(os.to_string(), arch.to_string())]
                    }
                    _ => Vec::new(),
                };
                Ok(ResolvedArtifact {
                    meta: ArtifactMeta {
                        name: name.to_string(),
                        version: version.to_string(),
                        runtime,
                        artifact_runtime: declared.artifact_runtime,
                        platforms,
                        description: None,
                        tags: Vec::new(),
                        wit_contracts: None,
                    },
                    bytes,
                    sha256: expected_sha,
                    // The endpoint serves one payload per request and never falls back to an
                    // untagged one, so there is no fallback for this to report.
                    platform_match: PlatformMatch::NotApplicable,
                })
            }
            StatusCode::NOT_FOUND => Err(RegistryError::NotFound {
                name: name.to_string(),
                version: version.to_string(),
            }),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(auth_error(&self.base_url)),
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
            ("runtime".to_string(), meta.runtime.as_str().to_string()),
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

                let published =
                    response
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
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(auth_error(&self.base_url)),
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
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(auth_error(&self.base_url)),
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
            Err(RegistryError::NotFound { .. }) => self
                .secondary
                .resolve_with_platform(name, version, platform),
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

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use murmur_artifact::{sha256_hex, Registry, RegistryError, RuntimeType};
    use tempfile::tempdir;
    use zip::{write::SimpleFileOptions, ZipWriter};

    use super::RemoteRegistry;

    /// A loopback HTTP server answering exactly one request with `body` and the headers an
    /// artifact download carries. It binds port 0, so concurrent tests never contend for a
    /// port, and its thread is left detached so a test that never made its request cannot
    /// block the runner in `accept()`.
    struct OneShotServer {
        url: String,
    }

    impl OneShotServer {
        /// `sha256` is sent as `x-murmur-sha256`, the header the download path requires.
        fn serving(body: Vec<u8>, sha256: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());

            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                // A GET carries no body, so the request head is the whole request.
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);

                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                     content-length: {}\r\nx-murmur-sha256: {sha256}\r\n\
                     connection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
                stream.flush().unwrap();
            });

            Self { url }
        }
    }

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

    fn recorded_meta(path: &std::path::Path) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(path).unwrap()).unwrap()
            ["meta"]
            .clone()
    }

    /// The same bytes record the same packaging type whichever install path stored them.
    #[test]
    fn a_remote_install_records_what_a_local_file_install_records() {
        let bytes = native_zip("my-tool", "1.0.0");
        let server = OneShotServer::serving(bytes.clone(), sha256_hex(&bytes));

        let resolved = RemoteRegistry::new(&server.url, "test-key")
            .resolve("my-tool", "1.0.0")
            .unwrap();
        assert_eq!(resolved.meta.runtime, RuntimeType::Native);

        let remote_dir = tempdir().unwrap();
        let remote_store = murmur_artifact::LocalRegistry::new(remote_dir.path());
        remote_store
            .store_installed_overwrite(resolved.meta.clone(), &bytes, &resolved.sha256)
            .unwrap();

        // The local-file path reads the platform off the file name, so the file is named the
        // way a published platform-tagged asset is.
        let platform = murmur_artifact::SUPPORTED_PLATFORMS[0];
        let source_dir = tempdir().unwrap();
        let file = source_dir
            .path()
            .join(format!("my-tool-1.0.0-{platform}.mur.zip"));
        std::fs::write(&file, &bytes).unwrap();

        let local_dir = tempdir().unwrap();
        let local_store = murmur_artifact::LocalRegistry::new(local_dir.path());
        crate::commands::install::install_from_local_file(file.to_str().unwrap(), &local_store)
            .unwrap();

        let from_remote = recorded_meta(&remote_store.metadata_path_for("my-tool", "1.0.0", None));
        let from_local =
            recorded_meta(&local_store.metadata_path_for("my-tool", "1.0.0", Some(platform)));

        assert_eq!(from_remote["runtime"], "native");
        assert_eq!(from_local["runtime"], "native");
        assert_eq!(from_remote["artifact_runtime"], "tool");
        assert_eq!(
            from_remote["artifact_runtime"],
            from_local["artifact_runtime"]
        );
    }

    /// A payload that is not a `.mur.zip` says nothing about itself, and is refused rather
    /// than recorded as a guess.
    #[test]
    fn a_download_carrying_no_manifest_is_refused() {
        let body = b"not-an-archive".to_vec();
        let server = OneShotServer::serving(body.clone(), sha256_hex(&body));

        let error = RemoteRegistry::new(&server.url, "test-key")
            .resolve("my-tool", "1.0.0")
            .unwrap_err();

        assert!(
            matches!(error, RegistryError::InvalidInput(_)),
            "unreadable bytes are rejected input, not a transport failure: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("my-tool") && message.contains("1.0.0"),
            "error must name the artifact: {message}"
        );
        assert!(
            message.contains("murmur.yaml"),
            "error must say what was missing: {message}"
        );
    }
}
