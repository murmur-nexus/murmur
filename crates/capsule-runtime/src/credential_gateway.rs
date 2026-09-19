//! The credential gateway: how an artifact reaches a third-party API without holding its key.
//!
//! An artifact whose entry carries `gateway:` is handed [`CredentialGateway::guest_endpoint`] as
//! `MURMUR_GATEWAY_ENDPOINT` — and, for the configured inference driver, as
//! `MURMUR_INFERENCE_ENDPOINT` too — and builds its request URL from it as it would from the real
//! upstream. Its request leaves the guest through `wasi:http/outgoing-handler` into the store's
//! `NetworkPolicyHooks::send_request`, which hands a request addressed to [`GATEWAY_AUTHORITY`] to
//! [`CredentialGateway::send`]. The runtime therefore originates the upstream connection — with
//! TLS for an `https` endpoint — and the response future, body included, goes back to the guest
//! exactly as wasi-http produced it.
//!
//! The key attached to each request is the credential's value at the moment the request is sent,
//! so a key rotated in the global config reaches the next request. The request body is buffered so
//! that a `401` can be answered by re-reading the credential and resending once; the response
//! never is.
//!
//! The gateway is not a listener. A WASM guest runs inside the `mur` process, so there is no
//! socket to guard and nothing another process can connect to and spend the key through.
//!
//! No error this module returns carries the key or the rendered header.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use http::{
    header::{HeaderName, HeaderValue},
    HeaderMap, Method, StatusCode, Uri, Version,
};
use http_body_util::{BodyExt, Full};
use murmur_artifact::InferenceAuth;
use wasmtime_wasi_http::p2::{
    bindings::http::types::ErrorCode,
    body::HyperOutgoingBody,
    default_send_request_handler,
    types::{IncomingResponse, OutgoingRequestConfig},
};

use crate::{errors::RuntimeError, gateway_credential::GatewayCredential, spend::SpendMeter};

/// The authority `MURMUR_GATEWAY_ENDPOINT` names.
///
/// Never bound: nothing listens on it. It is recognised only inside the `send_request` hook of a
/// store that holds a gateway, where a request to it is rewritten at that gateway's upstream. A
/// request to it from any other store is an ordinary allow-list-checked request, and carries no
/// key.
pub(crate) const GATEWAY_AUTHORITY: &str = "127.0.0.1:9";

/// Whether a gateway's requests are admitted against the session's spend meter.
#[derive(Clone)]
pub(crate) enum GatewayMetering {
    /// The configured `transport: http` driver's gateway. It sends nothing unless the meter holds
    /// an open admission.
    Inference(Arc<SpendMeter>),
    /// Every other gateway. It never reads the meter, and what its calls spend is not counted.
    Unmetered,
}

/// One artifact's credential gateway for one session.
pub(crate) struct CredentialGateway {
    /// The artifact whose store gets this gateway. No other store does.
    pub(crate) artifact: String,
    /// `gateway.endpoint`, parsed. Only its scheme, host and explicit port reach a request.
    upstream: url::Url,
    /// `upstream`'s authority as it goes on the wire: host plus the port only when one was written.
    upstream_authority: String,
    /// The artifact's own declaration of how its upstream takes the key.
    auth: InferenceAuth,
    /// `gateway.api_key`, resolved at staging. `None` attaches nothing.
    credential: Option<Arc<GatewayCredential>>,
    pub(crate) metering: GatewayMetering,
}

impl std::fmt::Debug for CredentialGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialGateway")
            .field("artifact", &self.artifact)
            .field("upstream", &self.upstream.as_str())
            .field("auth", &self.auth)
            .field("credential", &self.credential)
            .field("metered", &self.is_metered())
            .finish()
    }
}

/// Every gateway a session holds, keyed so a store can be handed only its own artifact's.
#[derive(Debug, Default, Clone)]
pub(crate) struct GatewayTable {
    inference: Option<Arc<CredentialGateway>>,
    by_artifact: HashMap<String, Arc<CredentialGateway>>,
}

impl GatewayTable {
    /// Adds `gateway`: as the inference gateway when it is metered, by its artifact otherwise.
    pub(crate) fn insert(&mut self, gateway: CredentialGateway) {
        let gateway = Arc::new(gateway);
        if gateway.is_metered() {
            self.inference = Some(gateway);
        } else {
            self.by_artifact.insert(gateway.artifact.clone(), gateway);
        }
    }

    /// The configured inference driver's gateway, attached only on the agent loop's driver
    /// dispatch and a hook's `run-inference`.
    pub(crate) fn inference(&self) -> Option<&Arc<CredentialGateway>> {
        self.inference.as_ref()
    }

    /// The gateway attached on every dispatch or instantiation of artifact `name`. Never the
    /// inference gateway, even when `name` is the inference driver: a driver reached by name
    /// through tool dispatch is not metered, so it gets none.
    pub(crate) fn for_artifact(&self, name: &str) -> Option<&Arc<CredentialGateway>> {
        self.by_artifact.get(name)
    }

    /// Every gateway, the inference gateway first, then the rest by artifact name.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<CredentialGateway>> {
        let mut rest: Vec<_> = self.by_artifact.values().collect();
        rest.sort_by(|a, b| a.artifact.cmp(&b.artifact));
        self.inference.iter().chain(rest)
    }
}

/// Everything of a guest request but its body, kept so the request can be rebuilt for a resend.
struct RequestHead {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
}

impl RequestHead {
    fn request(&self, body: Bytes) -> hyper::Request<HyperOutgoingBody> {
        let mut request = hyper::Request::new(
            Full::new(body)
                .map_err(|never| match never {})
                .boxed_unsync(),
        );
        *request.method_mut() = self.method.clone();
        *request.uri_mut() = self.uri.clone();
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers.clone();
        request
    }
}

fn copy_config(config: &OutgoingRequestConfig) -> OutgoingRequestConfig {
    OutgoingRequestConfig {
        use_tls: config.use_tls,
        connect_timeout: config.connect_timeout,
        first_byte_timeout: config.first_byte_timeout,
        between_bytes_timeout: config.between_bytes_timeout,
    }
}

impl CredentialGateway {
    /// Builds the gateway for `artifact` against `endpoint`.
    ///
    /// Refuses an endpoint that is not an absolute `http`/`https` URL with a host, or that carries
    /// a query or fragment: the guest is handed only the endpoint's path, so either would be
    /// silently dropped. Also refuses `${` and userinfo, the two shapes under which the text of
    /// the endpoint and the host the key is sent to can differ. The manifest parser applies the
    /// same rules and the plain-`http`-to-loopback rule; this is the last check before a key has
    /// a destination.
    pub(crate) fn new(
        artifact: impl Into<String>,
        endpoint: &str,
        auth: InferenceAuth,
        credential: Option<Arc<GatewayCredential>>,
        metering: GatewayMetering,
    ) -> Result<Self, RuntimeError> {
        let artifact = artifact.into();
        let refuse = |message: &str| {
            RuntimeError::Runtime(format!(
                "gateway.endpoint '{endpoint}' on artifact '{artifact}' {message}"
            ))
        };
        if endpoint.contains("${") {
            return Err(refuse("must be written literally; it is not interpolated"));
        }
        let upstream = url::Url::parse(endpoint).map_err(|_| refuse("is not a valid URL"))?;
        if !matches!(upstream.scheme(), "http" | "https") {
            return Err(refuse("must use http or https"));
        }
        if !upstream.username().is_empty() || upstream.password().is_some() {
            return Err(refuse("must not carry userinfo before '@'"));
        }
        if upstream.query().is_some() || upstream.fragment().is_some() {
            return Err(refuse("must not carry a query or fragment"));
        }
        let host = upstream
            .host_str()
            .ok_or_else(|| refuse("must name a host"))?;
        let upstream_authority = match upstream.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        Ok(Self {
            artifact,
            upstream,
            upstream_authority,
            auth,
            credential,
            metering,
        })
    }

    /// The artifact's credential, when `gateway.api_key` is set.
    pub(crate) fn credential(&self) -> Option<&Arc<GatewayCredential>> {
        self.credential.as_ref()
    }

    /// Whether this is the inference gateway, admitted against the spend meter.
    pub(crate) fn is_metered(&self) -> bool {
        matches!(self.metering, GatewayMetering::Inference(_))
    }

    /// The value of `MURMUR_GATEWAY_ENDPOINT` (and, for the inference gateway,
    /// `MURMUR_INFERENCE_ENDPOINT`): plain `http` to the gateway authority, carrying the upstream's
    /// path without a trailing `/`.
    pub(crate) fn guest_endpoint(&self) -> String {
        let path = self.upstream.path().trim_end_matches('/');
        format!("http://{GATEWAY_AUTHORITY}{path}")
    }

    /// The upstream's authority — host, and port when one was written — for the trace and for
    /// warnings. Never carries a path, query or key.
    pub(crate) fn upstream_host(&self) -> &str {
        &self.upstream_authority
    }

    /// Whether `uri` is addressed to the gateway.
    pub(crate) fn is_addressed_to_gateway(uri: &Uri) -> bool {
        uri.scheme_str() == Some("http")
            && uri.authority().map(|authority| authority.as_str()) == Some(GATEWAY_AUTHORITY)
    }

    /// Sends one guest request to the upstream and returns the upstream's response unbuffered.
    ///
    /// The request carries the credential's current value. When the provider answers `401`, the
    /// credential is read again, since its source may have changed after the request read it; a
    /// value different from the one sent is attached to a single resend of the same bytes, and that
    /// response is returned whatever its status. An unchanged value is not resent — the same key
    /// cannot get a different answer — and the `401` is recorded as a rejection, as is a `401` to
    /// the resend. No other status is looked at; a rejection's response goes back to the guest as
    /// it is.
    pub(crate) async fn send(
        self: Arc<Self>,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> Result<IncomingResponse, ErrorCode> {
        let Some(credential) = self.credential.clone() else {
            let (request, config) = self.rewrite(request, config, None)?;
            return default_send_request_handler(request, config).await;
        };

        let (parts, body) = request.into_parts();
        let body = body.collect().await?.to_bytes();
        let head = RequestHead {
            method: parts.method,
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
        };

        let sent = credential.current().await;
        let (request, first_config) = self.rewrite(
            head.request(body.clone()),
            copy_config(&config),
            Some(&sent),
        )?;
        let response = default_send_request_handler(request, first_config).await?;
        if response.resp.status() != StatusCode::UNAUTHORIZED {
            credential.clear_rejection();
            return Ok(response);
        }

        let reread = credential.reread_after_rejection().await;
        if reread == sent {
            credential
                .record_rejection(StatusCode::UNAUTHORIZED.as_u16(), false)
                .await;
            return Ok(response);
        }
        drop(response);

        let (request, config) = self.rewrite(head.request(body), config, Some(&reread))?;
        let resent = default_send_request_handler(request, config).await?;
        if resent.resp.status() == StatusCode::UNAUTHORIZED {
            credential
                .record_rejection(StatusCode::UNAUTHORIZED.as_u16(), true)
                .await;
        } else {
            credential.clear_rejection();
        }
        Ok(resent)
    }

    /// Readdresses a guest request at the upstream and attaches `key`.
    ///
    /// Every header named like `auth.header` is removed and, when a key is given, exactly one is
    /// inserted, marked sensitive. The URI keeps the request's own path and query and takes the
    /// upstream's scheme and authority; `use_tls` follows the upstream scheme. The body is moved
    /// through untouched. `host` is left unset — guests cannot set it — so wasi-http's sender fills
    /// it from the rewritten authority.
    fn rewrite(
        &self,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
        key: Option<&str>,
    ) -> Result<(hyper::Request<HyperOutgoingBody>, OutgoingRequestConfig), ErrorCode> {
        let (mut parts, body) = request.into_parts();

        let header = HeaderName::from_bytes(self.auth.header.as_bytes())
            .map_err(|_| ErrorCode::InternalError(None))?;
        parts.headers.remove(&header);
        if let Some(key) = key {
            let mut value = HeaderValue::from_str(&self.auth.render(key))
                .map_err(|_| ErrorCode::InternalError(None))?;
            value.set_sensitive(true);
            parts.headers.insert(header, value);
        }

        let mut uri = Uri::builder()
            .scheme(self.upstream.scheme())
            .authority(self.upstream_authority.as_str());
        if let Some(path_and_query) = parts.uri.path_and_query() {
            uri = uri.path_and_query(path_and_query.clone());
        }
        parts.uri = uri.build().map_err(|_| ErrorCode::HttpRequestUriInvalid)?;

        let config = OutgoingRequestConfig {
            use_tls: self.upstream.scheme() == "https",
            ..config
        };
        Ok((hyper::Request::from_parts(parts, body), config))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http_body_util::Empty;
    use murmur_artifact::ApiKeyReference;

    use super::*;

    const KEY: &str = "sk-unit-gateway-marker";

    fn auth(header: &str, value: &str) -> InferenceAuth {
        InferenceAuth {
            header: header.to_string(),
            value: value.to_string(),
        }
    }

    fn gateway(endpoint: &str, auth: InferenceAuth, key: Option<&str>) -> CredentialGateway {
        let credential = key.map(|key| {
            Arc::new(
                GatewayCredential::resolve(
                    "driver",
                    &ApiKeyReference::Literal(key.to_string()),
                    None,
                )
                .unwrap(),
            )
        });
        CredentialGateway::new(
            "driver",
            endpoint,
            auth,
            credential,
            GatewayMetering::Inference(Arc::new(SpendMeter::unlimited())),
        )
        .unwrap()
    }

    fn request(uri: &str, headers: &[(&str, &str)]) -> hyper::Request<HyperOutgoingBody> {
        let mut builder = hyper::Request::builder().method("POST").uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder
            .body(
                Empty::<bytes::Bytes>::new()
                    .map_err(|err| match err {})
                    .boxed_unsync(),
            )
            .unwrap()
    }

    fn config() -> OutgoingRequestConfig {
        OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: Duration::from_millis(11),
            first_byte_timeout: Duration::from_millis(22),
            between_bytes_timeout: Duration::from_millis(33),
        }
    }

    #[test]
    fn credential_gateway_replaces_a_forged_header_of_any_case_with_one_sensitive_header() {
        let gateway = gateway(
            "https://api.anthropic.com",
            auth("x-api-key", "{key}"),
            Some(KEY),
        );
        let forged = request(
            "http://127.0.0.1:9/v1/messages",
            &[
                ("X-API-KEY", "forged-upper"),
                ("x-api-key", "forged-lower"),
                ("content-type", "application/json"),
            ],
        );
        let (rewritten, _) = gateway.rewrite(forged, config(), Some(KEY)).unwrap();
        let values: Vec<_> = rewritten.headers().get_all("x-api-key").iter().collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], KEY);
        assert!(values[0].is_sensitive());
        assert_eq!(rewritten.headers()["content-type"], "application/json");
    }

    #[test]
    fn credential_gateway_renders_a_bearer_template() {
        let gateway = gateway(
            "https://api.openai.com/v1",
            auth("Authorization", "Bearer {key}"),
            Some(KEY),
        );
        let (rewritten, _) = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/chat/completions", &[]),
                config(),
                Some(KEY),
            )
            .unwrap();
        assert_eq!(
            rewritten.headers()["authorization"],
            format!("Bearer {KEY}").as_str()
        );
    }

    #[test]
    fn credential_gateway_keeps_the_request_path_and_query_and_takes_the_upstream_scheme() {
        let gateway = gateway(
            "https://api.moonshot.ai/v1",
            auth("Authorization", "Bearer {key}"),
            Some(KEY),
        );
        let (rewritten, config) = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/chat/completions?x=1", &[]),
                config(),
                Some(KEY),
            )
            .unwrap();
        assert_eq!(
            rewritten.uri().to_string(),
            "https://api.moonshot.ai/v1/chat/completions?x=1"
        );
        assert!(config.use_tls);
        assert_eq!(config.connect_timeout, Duration::from_millis(11));
        assert_eq!(config.first_byte_timeout, Duration::from_millis(22));
        assert_eq!(config.between_bytes_timeout, Duration::from_millis(33));
    }

    #[test]
    fn credential_gateway_sends_plain_http_to_an_http_upstream_with_its_port() {
        let gateway = gateway(
            "http://localhost:11434",
            auth("x-api-key", "{key}"),
            Some(KEY),
        );
        let (rewritten, config) = gateway
            .rewrite(
                request("http://127.0.0.1:9/api/chat", &[]),
                config(),
                Some(KEY),
            )
            .unwrap();
        assert_eq!(
            rewritten.uri().to_string(),
            "http://localhost:11434/api/chat"
        );
        assert!(!config.use_tls);
    }

    #[test]
    fn credential_gateway_without_a_key_attaches_nothing_and_still_strips_the_forged_header() {
        let gateway = gateway(
            "https://api.anthropic.com",
            auth("x-api-key", "{key}"),
            None,
        );
        assert!(gateway.credential().is_none());
        let (rewritten, _) = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/messages", &[("X-Api-Key", "forged")]),
                config(),
                None,
            )
            .unwrap();
        assert!(rewritten.headers().get("x-api-key").is_none());
    }

    #[test]
    fn credential_gateway_guest_endpoint_carries_the_upstream_path() {
        let bare = gateway(
            "https://api.anthropic.com",
            auth("x-api-key", "{key}"),
            None,
        );
        assert_eq!(bare.guest_endpoint(), "http://127.0.0.1:9");
        let pathed = gateway(
            "https://api.moonshot.ai/v1",
            auth("x-api-key", "{key}"),
            None,
        );
        assert_eq!(pathed.guest_endpoint(), "http://127.0.0.1:9/v1");
        let trailing = gateway(
            "https://api.moonshot.ai/v1/",
            auth("x-api-key", "{key}"),
            None,
        );
        assert_eq!(trailing.guest_endpoint(), "http://127.0.0.1:9/v1");
    }

    #[test]
    fn credential_gateway_refuses_an_endpoint_with_a_query_or_fragment() {
        for endpoint in [
            "https://api.example.com/v1?x=1",
            "https://api.example.com/#f",
        ] {
            let err = CredentialGateway::new(
                "web-search",
                endpoint,
                auth("x-api-key", "{key}"),
                None,
                GatewayMetering::Unmetered,
            )
            .unwrap_err();
            assert!(err.to_string().contains("gateway.endpoint"), "{err}");
            assert!(err.to_string().contains("artifact 'web-search'"), "{err}");
        }
    }

    /// wasi-http's sender issues exactly one request: a `3xx` goes back to the guest as the
    /// response, and the key never follows its `Location`.
    #[test]
    fn gateway_does_not_follow_a_redirect() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let location = format!("http://{}/stolen", elsewhere.local_addr().unwrap());

        let redirecting = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", redirecting.local_addr().unwrap());
        let first = std::thread::spawn(move || {
            let (mut stream, _) = redirecting.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                head.push(byte[0]);
            }
            write!(
                stream,
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            )
            .unwrap();
            String::from_utf8_lossy(&head).to_lowercase()
        });

        let gateway = Arc::new(gateway(&endpoint, auth("x-api-key", "{key}"), Some(KEY)));
        let config = OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: Duration::from_secs(5),
            first_byte_timeout: Duration::from_secs(5),
            between_bytes_timeout: Duration::from_secs(5),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let response = gateway
                .send(request("http://127.0.0.1:9/v1/probe", &[]), config)
                .await
                .unwrap();
            response.resp.status()
        });
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);

        let head = first.join().unwrap();
        assert!(head.contains(&format!("x-api-key: {KEY}")), "{head}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            elsewhere.accept().is_err(),
            "the redirect target must receive no connection"
        );
    }

    #[test]
    fn credential_gateway_refuses_userinfo() {
        for endpoint in [
            "https://user:pass@api.example.com",
            "https://user@api.example.com",
            "https://api.example.com@127.0.0.1:8443",
            "https://${PROVIDER_HOST}/v1",
        ] {
            let err = CredentialGateway::new(
                "web-search",
                endpoint,
                auth("x-api-key", "{key}"),
                None,
                GatewayMetering::Unmetered,
            )
            .unwrap_err();
            assert!(err.to_string().contains("gateway.endpoint"), "{err}");
            assert!(err.to_string().contains("artifact 'web-search'"), "{err}");
        }
    }

    #[test]
    fn credential_gateway_debug_redacts_the_key() {
        let gateway = gateway(
            "https://api.anthropic.com",
            auth("x-api-key", "{key}"),
            Some(KEY),
        );
        let debug = format!("{gateway:?}");
        assert!(!debug.contains(KEY), "{debug}");
        assert!(debug.contains("value: \"<redacted>\""), "{debug}");
    }

    #[test]
    fn credential_gateway_recognises_only_its_own_plain_http_authority() {
        let uri = |s: &str| s.parse::<Uri>().unwrap();
        assert!(CredentialGateway::is_addressed_to_gateway(&uri(
            "http://127.0.0.1:9/v1"
        )));
        assert!(!CredentialGateway::is_addressed_to_gateway(&uri(
            "https://127.0.0.1:9/v1"
        )));
        assert!(!CredentialGateway::is_addressed_to_gateway(&uri(
            "http://127.0.0.1:90/v1"
        )));
        assert!(!CredentialGateway::is_addressed_to_gateway(&uri(
            "http://localhost:9/v1"
        )));
    }
}
