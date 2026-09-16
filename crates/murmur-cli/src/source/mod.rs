pub(crate) mod github;

use std::fmt;

use bytes::Bytes;

use crate::config::{MurConfig, SourceConfig, SourceType};
use crate::error::{E_IO_003, E_REG_001, E_REG_006};

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

/// One source's failure to resolve an artifact, kept typed so the chain can tell a source that
/// answered "no such artifact" from one that did not answer at all.
#[derive(Debug, Clone)]
pub struct SourceAttempt {
    pub source: String,
    pub error: SourceError,
}

impl SourceAttempt {
    /// The one-line reason printed beside the source's name.
    pub(crate) fn reason(&self) -> String {
        self.error.reason()
    }
}

#[derive(Debug, Clone)]
pub enum SourceChainError {
    /// No source in the chain produced the artifact. `attempts` holds every source's error in
    /// chain order; whether this means "absent" or "unknown" depends on those errors, which is
    /// what [`SourceChainError::diagnosis`] decides.
    Unresolved {
        target: String,
        attempts: Vec<SourceAttempt>,
    },
    /// An explicit `github:<owner>/<repo>@<tag>` lookup failed; `source` is `github:<owner>/<repo>`.
    SourceFailure { source: String, error: SourceError },
}

/// The code, message and hint every rendering of a [`SourceChainError`] shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceChainDiagnosis {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

const NOT_FOUND_HINT: &str = "check the name and version; the sources searched are the \
     registry.sources entries of the effective config, or name one directly with \
     mur install github:<owner>/<repo>@<tag>";

const ANONYMOUS_RATE_LIMIT_HINT: &str = "GitHub allows 60 unauthenticated API requests an hour \
     and each artifact lookup can spend four; authenticate with \
     `export GITHUB_TOKEN=$(gh auth token)` (or any GitHub token), or set \
     `token: ${GITHUB_TOKEN}` on the source in config.yaml to keep it";

const TOKEN_RATE_LIMIT_HINT: &str = "the token sent with these requests has exhausted its own \
     GitHub rate limit; retry after the reset shown above";

const UNANSWERED_HINT: &str = "each line above is what that source returned instead of an \
     answer; fix the source or its credentials and retry";

impl SourceChainError {
    /// Picks the code and hint from the typed attempt errors.
    ///
    /// `E-REG-001` only when every attempt is [`SourceError::NotFound`] — an empty chain included —
    /// because only then did every source answer. Any other attempt makes it `E-REG-006`. The
    /// token hint is given only when some rate-limited request carried no token.
    pub(crate) fn diagnosis(&self) -> SourceChainDiagnosis {
        match self {
            SourceChainError::Unresolved { target, attempts } => {
                let answered = attempts
                    .iter()
                    .all(|attempt| matches!(attempt.error, SourceError::NotFound(_)));
                let mut message = if answered {
                    format!("could not resolve '{target}'")
                } else {
                    format!(
                        "could not look up '{target}': a source did not answer, so whether it \
                         publishes the artifact is not known"
                    )
                };
                for attempt in attempts {
                    message.push_str(&format!("\n  {} — {}", attempt.source, attempt.reason()));
                }
                if answered {
                    SourceChainDiagnosis {
                        code: E_REG_001,
                        message,
                        hint: Some(NOT_FOUND_HINT.to_string()),
                    }
                } else {
                    let errors: Vec<&SourceError> =
                        attempts.iter().map(|attempt| &attempt.error).collect();
                    SourceChainDiagnosis {
                        code: E_REG_006,
                        message,
                        hint: Some(
                            rate_limit_hint(&errors)
                                .unwrap_or(UNANSWERED_HINT)
                                .to_string(),
                        ),
                    }
                }
            }
            SourceChainError::SourceFailure { source, error } => match error {
                SourceError::RateLimited { .. } => SourceChainDiagnosis {
                    code: E_REG_006,
                    message: format!("could not look up {source}: {}", error.reason()),
                    hint: rate_limit_hint(&[error]).map(str::to_string),
                },
                _ => SourceChainDiagnosis {
                    code: E_IO_003,
                    message: error.reason(),
                    hint: None,
                },
            },
        }
    }
}

/// The hint for a set of errors that includes a rate limit, or `None` when none is rate limited.
fn rate_limit_hint(errors: &[&SourceError]) -> Option<&'static str> {
    let mut rate_limited = errors.iter().filter_map(|error| match error {
        SourceError::RateLimited { authenticated, .. } => Some(*authenticated),
        _ => None,
    });
    let first = rate_limited.next()?;
    if !first || rate_limited.any(|authenticated| !authenticated) {
        Some(ANONYMOUS_RATE_LIMIT_HINT)
    } else {
        Some(TOKEN_RATE_LIMIT_HINT)
    }
}

impl fmt::Display for SourceChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diagnosis = self.diagnosis();
        write!(f, "{}", diagnosis.message)?;
        if let Some(hint) = diagnosis.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SourceChainError {}

#[derive(Debug, Clone)]
pub enum SourceError {
    /// The source answered: it has no such release or asset.
    NotFound(String),
    /// The source refused or failed the request for a reason other than a rate limit.
    Http {
        status: u16,
        message: String,
    },
    /// The source refused the request because its API rate limit is exhausted.
    RateLimited {
        status: u16,
        message: String,
        /// Seconds until the limit resets, or `None` when the response did not say.
        resets_in_secs: Option<u64>,
        /// Whether the refused request carried a token.
        authenticated: bool,
    },
    Config(String),
    Other(String),
}

impl SourceError {
    /// One line: the chain prints each attempt's reason on a single indented line.
    pub(crate) fn reason(&self) -> String {
        match self {
            SourceError::NotFound(message)
            | SourceError::Config(message)
            | SourceError::Other(message) => message.clone(),
            SourceError::Http { status, message } => format!("HTTP {status}: {message}"),
            SourceError::RateLimited {
                status,
                message,
                resets_in_secs,
                ..
            } => {
                let mut reason = format!("rate limited by GitHub (HTTP {status}): {message}");
                if let Some(secs) = resets_in_secs {
                    let minutes = secs.div_ceil(60).max(1);
                    let unit = if minutes == 1 { "minute" } else { "minutes" };
                    reason.push_str(&format!(" — resets in about {minutes} {unit}"));
                }
                reason
            }
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
                    error,
                }),
            }
        }

        Err(SourceChainError::Unresolved {
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
            .map_err(|error| SourceChainError::SourceFailure {
                source: format!("github:{owner}/{repo}"),
                error,
            })
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
                    error,
                }),
            }
        }

        Err(SourceChainError::Unresolved {
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

    fn failing(name: &str, error: SourceError) -> Box<dyn ArtifactSource> {
        Box::new(MockSource {
            name: name.to_string(),
            calls: Arc::new(AtomicUsize::new(0)),
            result: Err(error),
        })
    }

    fn rate_limited(authenticated: bool) -> SourceError {
        SourceError::RateLimited {
            status: 403,
            message: "API rate limit exceeded for 127.0.0.1.".to_string(),
            resets_in_secs: Some(2460),
            authenticated,
        }
    }

    fn diagnose(sources: Vec<Box<dyn ArtifactSource>>) -> SourceChainDiagnosis {
        let chain = SourceChain::from_sources_for_test(sources, Vec::new());
        let error = chain
            .resolve_bare("murmur-driver-anthropic", None)
            .unwrap_err();
        let diagnosis = error.diagnosis();
        let rendered = error.to_string();
        assert!(!rendered.contains("doctor"), "{rendered}");
        diagnosis
    }

    fn hint(diagnosis: &SourceChainDiagnosis) -> &str {
        diagnosis.hint.as_deref().unwrap_or_default()
    }

    #[test]
    fn every_source_answering_not_found_is_e_reg_001() {
        let diagnosis = diagnose(vec![
            failing(
                "first",
                SourceError::NotFound("release not found".to_string()),
            ),
            failing("second", SourceError::NotFound("no asset".to_string())),
        ]);
        assert_eq!(diagnosis.code, E_REG_001);
        assert!(diagnosis
            .message
            .starts_with("could not resolve 'murmur-driver-anthropic'"));
        assert!(diagnosis.message.contains("\n  first — release not found"));
        assert!(diagnosis.message.contains("\n  second — no asset"));
        assert!(!hint(&diagnosis).contains("doctor"));
        assert!(!hint(&diagnosis).contains("GITHUB_TOKEN"));
    }

    #[test]
    fn an_empty_chain_is_e_reg_001() {
        assert_eq!(diagnose(Vec::new()).code, E_REG_001);
    }

    #[test]
    fn a_source_that_failed_to_answer_makes_it_e_reg_006() {
        let diagnosis = diagnose(vec![
            failing(
                "first",
                SourceError::NotFound("release not found".to_string()),
            ),
            failing(
                "second",
                SourceError::Http {
                    status: 502,
                    message: "Bad Gateway".to_string(),
                },
            ),
        ]);
        assert_eq!(diagnosis.code, E_REG_006);
        assert!(diagnosis.message.starts_with(
            "could not look up 'murmur-driver-anthropic': a source did not answer, so whether it publishes the artifact is not known"
        ));
        assert!(diagnosis.message.contains("\n  first — release not found"));
        assert!(diagnosis
            .message
            .contains("\n  second — HTTP 502: Bad Gateway"));
        assert_eq!(hint(&diagnosis), UNANSWERED_HINT);
    }

    #[test]
    fn an_unauthenticated_rate_limit_gets_the_token_hint() {
        let diagnosis = diagnose(vec![
            failing("first", rate_limited(true)),
            failing("second", rate_limited(false)),
        ]);
        assert_eq!(diagnosis.code, E_REG_006);
        assert!(diagnosis.message.contains(
            "\n  second — rate limited by GitHub (HTTP 403): API rate limit exceeded for 127.0.0.1. — resets in about 41 minutes"
        ));
        let hint = hint(&diagnosis);
        for needle in [
            "GITHUB_TOKEN",
            "gh auth token",
            "token: ${GITHUB_TOKEN}",
            "60",
        ] {
            assert!(hint.contains(needle), "{needle} missing from {hint}");
        }
    }

    #[test]
    fn an_authenticated_rate_limit_says_the_token_is_exhausted() {
        let diagnosis = diagnose(vec![
            failing("first", rate_limited(true)),
            failing(
                "second",
                SourceError::NotFound("release not found".to_string()),
            ),
        ]);
        assert_eq!(diagnosis.code, E_REG_006);
        let hint = hint(&diagnosis);
        assert!(hint.contains("exhausted"), "{hint}");
        assert!(
            !hint.contains("GITHUB_TOKEN") && !hint.contains("gh auth token"),
            "{hint}"
        );
    }

    #[test]
    fn a_refused_request_gets_no_token_advice() {
        let diagnosis = diagnose(vec![failing(
            "first",
            SourceError::Http {
                status: 403,
                message: "Resource protected by organization SAML enforcement.".to_string(),
            },
        )]);
        assert_eq!(diagnosis.code, E_REG_006);
        let hint = hint(&diagnosis);
        assert!(
            !hint.contains("GITHUB_TOKEN") && !hint.contains("token:"),
            "{hint}"
        );
        assert!(!diagnosis.message.contains("rate limited"));
    }

    #[test]
    fn a_rate_limited_explicit_lookup_is_e_reg_006_and_any_other_failure_e_io_003() {
        let limited = SourceChainError::SourceFailure {
            source: "github:acme/artifacts".to_string(),
            error: rate_limited(false),
        }
        .diagnosis();
        assert_eq!(limited.code, E_REG_006);
        assert!(limited
            .message
            .starts_with("could not look up github:acme/artifacts: rate limited by GitHub"));
        assert_eq!(limited.hint.as_deref(), Some(ANONYMOUS_RATE_LIMIT_HINT));

        let other = SourceChainError::SourceFailure {
            source: "github:acme/artifacts".to_string(),
            error: SourceError::Http {
                status: 500,
                message: "boom".to_string(),
            },
        };
        let diagnosis = other.diagnosis();
        assert_eq!(diagnosis.code, E_IO_003);
        assert_eq!(diagnosis.message, "HTTP 500: boom");
        assert_eq!(diagnosis.hint, None);
        assert!(!other.to_string().contains("doctor"));
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
