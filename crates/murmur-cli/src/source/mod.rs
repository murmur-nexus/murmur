pub(crate) mod github;

use std::fmt;

use bytes::Bytes;

use crate::config::{MurConfig, SourceConfig, SourceType};

use self::github::GitHubSource;

/// What one source returned for one artifact.
///
/// [`Self::platform`] is the tag on the asset name that was actually selected, not the platform
/// that was asked for: an install has to record which platform the bytes it holds are for, and a
/// source that answered with an untagged asset did not say.
#[derive(Debug, Clone)]
pub struct SourceResolution {
    pub bytes: Bytes,
    /// The release tag the asset came from — not a platform.
    pub resolved_version: String,
    /// Platform tag split off the selected asset name, or `None` when the asset carries none.
    pub platform: Option<String>,
}

pub trait ArtifactSource: Send + Sync {
    fn name(&self) -> &str;
    fn resolve_bare(&self, name: &str) -> Result<SourceResolution, SourceError>;
    fn resolve_bare_with_version(
        &self,
        name: &str,
        version: &str,
    ) -> Result<SourceResolution, SourceError> {
        let _ = version;
        self.resolve_bare(name)
    }
    /// Resolve a specific version of an artifact for the given target platform.
    ///
    /// Implementations should prefer `<name>-<version>-<platform>.mur.zip` and fall back to
    /// the generic `<name>-<version>.mur.zip` (covers WASM artifacts). The default delegates
    /// to `resolve_bare_with_version`, ignoring platform, which preserves all existing impls.
    fn resolve_bare_with_version_for_platform(
        &self,
        name: &str,
        version: &str,
        platform: &str,
    ) -> Result<SourceResolution, SourceError> {
        let _ = platform;
        self.resolve_bare_with_version(name, version)
    }
}

pub struct SourceChain {
    sources: Vec<Box<dyn ArtifactSource>>,
    source_configs: Vec<SourceConfig>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub bytes: Bytes,
    pub source: String,
    pub resolved_version: Option<String>,
    /// Platform tag of the asset name the source selected, or `None` when it carries none.
    /// What an install records as the payload's platform, for a native payload.
    pub platform: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceAttempt {
    pub source: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum SourceChainError {
    NotFound {
        target: String,
        attempts: Vec<SourceAttempt>,
    },
    SourceFailure(String),
}

impl fmt::Display for SourceChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceChainError::NotFound { target, attempts } => {
                writeln!(f, "could not resolve '{target}'")?;
                for attempt in attempts {
                    writeln!(f, "  {} — {}", attempt.source, attempt.reason)?;
                }
                write!(
                    f,
                    "  hint: run `mur doctor` to check your source configuration\n  hint: use an explicit source URI: mur install github:<owner>/<repo>@<tag>"
                )
            }
            SourceChainError::SourceFailure(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for SourceChainError {}

#[derive(Debug, Clone)]
pub enum SourceError {
    NotFound(String),
    Http { status: u16, message: String },
    Config(String),
    Other(String),
}

impl SourceError {
    fn reason(&self) -> String {
        match self {
            SourceError::NotFound(message)
            | SourceError::Config(message)
            | SourceError::Other(message) => message.clone(),
            SourceError::Http { status, message } => format!("HTTP {status}: {message}"),
        }
    }
}

impl SourceChain {
    pub fn from_config(config: &MurConfig) -> Self {
        let mut source_configs = config.registry.sources.clone();
        if let Some(default_name) = config.registry.default.as_deref() {
            if let Some(index) = source_configs
                .iter()
                .position(|source| source.name == default_name)
            {
                let default_source = source_configs.remove(index);
                source_configs.insert(0, default_source);
            }
        }

        let mut sources: Vec<Box<dyn ArtifactSource>> = Vec::with_capacity(source_configs.len());

        for source in &source_configs {
            match source.r#type {
                SourceType::GitHub => {
                    let Some(repo) = source.repo.as_deref() else {
                        sources.push(Box::new(BrokenSource::new(
                            source.name.clone(),
                            SourceError::Config(
                                "missing `repo` for github source in ~/.murmur/config.yaml"
                                    .to_string(),
                            ),
                        )));
                        continue;
                    };

                    match GitHubSource::from_config(&source.name, repo, source.resolved_token()) {
                        Ok(github) => sources.push(Box::new(github)),
                        Err(error) => {
                            sources.push(Box::new(BrokenSource::new(source.name.clone(), error)))
                        }
                    }
                }
            }
        }

        Self {
            sources,
            source_configs,
        }
    }

    pub fn resolve_bare(
        &self,
        name: &str,
        version_hint: Option<&str>,
    ) -> Result<ResolvedSource, SourceChainError> {
        let mut attempts = Vec::new();

        for source in &self.sources {
            let result = if let Some(version) = version_hint {
                source.resolve_bare_with_version(name, version)
            } else {
                source.resolve_bare(name)
            };

            match result {
                Ok(resolution) => {
                    return Ok(ResolvedSource {
                        bytes: resolution.bytes,
                        source: source.name().to_string(),
                        resolved_version: Some(resolution.resolved_version),
                        platform: resolution.platform,
                    });
                }
                Err(error) => attempts.push(SourceAttempt {
                    source: source.name().to_string(),
                    reason: error.reason(),
                }),
            }
        }

        Err(SourceChainError::NotFound {
            target: name.to_string(),
            attempts,
        })
    }

    pub fn resolve_github_all(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<Vec<ResolvedSource>, SourceChainError> {
        let repo_key = format!("{owner}/{repo}");
        let matched = self.source_configs.iter().find(|source| {
            source.r#type == SourceType::GitHub && source.repo.as_deref() == Some(repo_key.as_str())
        });

        let token = matched.and_then(SourceConfig::resolved_token);

        let github = GitHubSource::explicit(owner, repo, tag, token);
        github
            .resolve_all_release_assets_by_tag()
            .map_err(|error| SourceChainError::SourceFailure(error.reason()))
            .map(|all| {
                all.into_iter()
                    .map(|r| ResolvedSource {
                        bytes: r.bytes,
                        source: format!("github:{owner}/{repo}"),
                        resolved_version: Some(r.tag),
                        platform: r.platform,
                    })
                    .collect()
            })
    }

    /// Resolve a specific version of an artifact for the given target platform, trying each
    /// source in order. Returns the first success, carrying the platform tag of the asset that
    /// was selected — which may be `None` when the source answered with an untagged asset.
    pub(crate) fn resolve_bare_for_platform(
        &self,
        name: &str,
        version: &str,
        platform: &str,
    ) -> Result<ResolvedSource, SourceChainError> {
        let mut attempts = Vec::new();

        for source in &self.sources {
            match source.resolve_bare_with_version_for_platform(name, version, platform) {
                Ok(resolution) => {
                    return Ok(ResolvedSource {
                        bytes: resolution.bytes,
                        source: source.name().to_string(),
                        resolved_version: Some(resolution.resolved_version),
                        platform: resolution.platform,
                    })
                }
                Err(error) => attempts.push(SourceAttempt {
                    source: source.name().to_string(),
                    reason: error.reason(),
                }),
            }
        }

        Err(SourceChainError::NotFound {
            target: format!("{name}@{version} (platform: {platform})"),
            attempts,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn from_sources_for_test(
        sources: Vec<Box<dyn ArtifactSource>>,
        source_configs: Vec<SourceConfig>,
    ) -> Self {
        Self {
            sources,
            source_configs,
        }
    }
}

struct BrokenSource {
    name: String,
    error: SourceError,
}

impl BrokenSource {
    fn new(name: String, error: SourceError) -> Self {
        Self { name, error }
    }
}

impl ArtifactSource for BrokenSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn resolve_bare(&self, _name: &str) -> Result<SourceResolution, SourceError> {
        Err(self.error.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        thread,
    };

    use zip::{
        write::{FileOptions, SimpleFileOptions},
        CompressionMethod, ZipWriter,
    };

    use super::*;

    struct MockSource {
        name: String,
        calls: Arc<AtomicUsize>,
        result: Result<SourceResolution, SourceError>,
    }

    impl ArtifactSource for MockSource {
        fn name(&self) -> &str {
            &self.name
        }

        fn resolve_bare(&self, _name: &str) -> Result<SourceResolution, SourceError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn untagged(bytes: &'static [u8], version: &str) -> SourceResolution {
        SourceResolution {
            bytes: Bytes::from_static(bytes),
            resolved_version: version.to_string(),
            platform: None,
        }
    }

    #[test]
    fn source_chain_routes_bare_name_to_chain() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));

        let chain = SourceChain::from_sources_for_test(
            vec![
                Box::new(MockSource {
                    name: "first".to_string(),
                    calls: Arc::clone(&first_calls),
                    result: Err(SourceError::NotFound("missing".to_string())),
                }),
                Box::new(MockSource {
                    name: "second".to_string(),
                    calls: Arc::clone(&second_calls),
                    result: Ok(untagged(b"artifact", "0.1.0")),
                }),
            ],
            Vec::new(),
        );

        let resolved = chain.resolve_bare("murmur-driver-anthropic", None).unwrap();

        assert_eq!(resolved.bytes, Bytes::from_static(b"artifact"));
        assert_eq!(resolved.resolved_version.as_deref(), Some("0.1.0"));
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn source_chain_routes_github_uri_directly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let artifact_bytes = artifact_zip_bytes("murmur-driver-anthropic", "0.1.1");
        let server = MockGitHubServer::start(artifact_bytes);

        std::env::set_var("MUR_GITHUB_API_BASE", server.api_base());

        let chain = SourceChain::from_sources_for_test(
            vec![Box::new(MockSource {
                name: "chain-source".to_string(),
                calls: Arc::clone(&calls),
                result: Ok(untagged(b"from-chain", "1.0.0")),
            })],
            Vec::new(),
        );

        let resolved_list = chain
            .resolve_github_all("acme", "artifacts", "v0.1.1")
            .unwrap();

        std::env::remove_var("MUR_GITHUB_API_BASE");

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(resolved_list.len(), 1);
        let resolved = &resolved_list[0];
        assert_eq!(resolved.source, "github:acme/artifacts");
        assert_eq!(resolved.resolved_version.as_deref(), Some("v0.1.1"));
        assert!(!resolved.bytes.is_empty());
    }

    fn artifact_zip_bytes(name: &str, version: &str) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut zip = ZipWriter::new(&mut cursor);
            let options: SimpleFileOptions =
                FileOptions::default().compression_method(CompressionMethod::Deflated);

            zip.start_file("murmur.yaml", options).unwrap();
            writeln!(zip, "name: {name}").unwrap();
            writeln!(zip, "version: {version}").unwrap();

            zip.start_file("tool.wasm", options).unwrap();
            zip.write_all(b"fake-wasm").unwrap();

            zip.finish().unwrap();
        }

        cursor.into_inner()
    }

    struct MockGitHubServer {
        address: std::net::SocketAddr,
        _join: thread::JoinHandle<()>,
    }

    impl MockGitHubServer {
        fn start(asset_bytes: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();

            let join = thread::spawn(move || {
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request = read_request(&mut stream).unwrap();

                    if request.path == "/repos/acme/artifacts/releases/tags/v0.1.1" {
                        let body = format!(
                            "{{\"tag_name\":\"v0.1.1\",\"assets\":[{{\"id\":1,\"name\":\"murmur-driver-anthropic.mur.zip\",\"browser_download_url\":\"http://{address}/download/murmur-driver-anthropic.mur.zip\"}}]}}"
                        );
                        write_response(&mut stream, 200, "application/json", body.as_bytes())
                            .unwrap();
                    } else if request.path == "/repos/acme/artifacts/releases/assets/1" {
                        write_response(&mut stream, 200, "application/octet-stream", &asset_bytes)
                            .unwrap();
                    } else {
                        write_response(&mut stream, 404, "text/plain", b"not found").unwrap();
                    }
                }
            });

            Self {
                address,
                _join: join,
            }
        }

        fn api_base(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    struct RequestLine {
        path: String,
    }

    fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<RequestLine> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];

        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let first_line = request.lines().next().unwrap_or_default();
        let path = first_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();

        Ok(RequestLine { path })
    }

    fn write_response(
        stream: &mut std::net::TcpStream,
        status: u16,
        content_type: &str,
        body: &[u8],
    ) -> std::io::Result<()> {
        let reason = match status {
            200 => "OK",
            404 => "Not Found",
            _ => "Error",
        };

        let headers = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );

        stream.write_all(headers.as_bytes())?;
        stream.write_all(body)?;
        stream.flush()
    }
}
