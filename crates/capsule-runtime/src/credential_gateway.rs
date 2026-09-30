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
//! so a key rotated in the global config, or injected by a controller, reaches the next request.
//! The request body is buffered so that a `401` can be answered by re-reading the credential and
//! resending once; the response never is.
//!
//! The gateway is not a listener. A WASM guest runs inside the `mur` process, so there is no
//! socket to guard and nothing another process can connect to and spend the key through.
//!
//! No error this module returns carries the key or the rendered header.

use std::{collections::HashMap, future::Future, sync::Arc};

use bytes::Bytes;
use http::{
    header::{self, HeaderName, HeaderValue},
    HeaderMap, Method, StatusCode, Uri, Version,
};
use http_body_util::{BodyExt, Full};
use murmur_artifact::UpstreamAuth;
use wasmtime_wasi_http::{Error as WasiHttpError, RequestOptions, WasiBody};

use crate::{errors::RuntimeError, gateway_credential::GatewayCredential, spend::SpendMeter};

/// The future a sent request's response comes with. It drives the rest of the connection, so the
/// response body arrives only while it runs.
pub(crate) type ConnectionIo = Box<dyn Future<Output = Result<(), WasiHttpError>> + Send>;

/// Sends `request` to the authority its URI names, over a connection of its own and with TLS for
/// an `https` URI, and returns once the response head has arrived.
pub(crate) async fn send_direct(
    request: http::Request<WasiBody>,
    options: Option<RequestOptions>,
) -> Result<(http::Response<WasiBody>, ConnectionIo), WasiHttpError> {
    let (response, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
    Ok((response.map(BodyExt::boxed_unsync), Box::new(io)))
}

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
    /// A driver choice's gateway: the configured `transport: http` driver's, or an
    /// `inference.alternates` driver's. It sends nothing unless the meter holds an open admission.
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
    auth: UpstreamAuth,
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
    /// The metered gateways of `inference.alternates` drivers other than the primary's own.
    alternates: HashMap<String, Arc<CredentialGateway>>,
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

    /// Adds `gateway`, which is metered, as an alternate driver's: reachable only through
    /// [`Self::inference_for`], never through [`Self::inference`] or [`Self::for_artifact`].
    pub(crate) fn insert_alternate(&mut self, gateway: CredentialGateway) {
        debug_assert!(
            gateway.is_metered(),
            "an alternate driver's gateway is metered"
        );
        self.alternates
            .insert(gateway.artifact.clone(), Arc::new(gateway));
    }

    /// The configured inference driver's gateway, attached on the agent loop's primary-choice
    /// dispatch, a hook's `run-inference` and compaction.
    pub(crate) fn inference(&self) -> Option<&Arc<CredentialGateway>> {
        self.inference.as_ref()
    }

    /// The metered gateway of the driver choice on artifact `driver`: the primary's when `driver`
    /// is the configured driver, an alternate's otherwise. `None` for a driver no choice names, or
    /// whose credential could not be staged. Attached only on the agent loop's driver dispatch.
    pub(crate) fn inference_for(&self, driver: &str) -> Option<&Arc<CredentialGateway>> {
        self.inference
            .as_ref()
            .filter(|gateway| gateway.artifact == driver)
            .or_else(|| self.alternates.get(driver))
    }

    /// The gateway attached on every dispatch or instantiation of artifact `name`. Never the
    /// inference gateway, even when `name` is the inference driver: a driver reached by name
    /// through tool dispatch is not metered, so it gets none.
    pub(crate) fn for_artifact(&self, name: &str) -> Option<&Arc<CredentialGateway>> {
        self.by_artifact.get(name)
    }

    /// Every gateway: the inference gateway first, then the alternates' by artifact name, then
    /// the rest by artifact name.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<CredentialGateway>> {
        let mut alternates: Vec<_> = self.alternates.values().collect();
        alternates.sort_by(|a, b| a.artifact.cmp(&b.artifact));
        let mut rest: Vec<_> = self.by_artifact.values().collect();
        rest.sort_by(|a, b| a.artifact.cmp(&b.artifact));
        self.inference.iter().chain(alternates).chain(rest)
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
    fn request(&self, body: Bytes) -> http::Request<WasiBody> {
        let mut request = http::Request::new(
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
        auth: UpstreamAuth,
        credential: Option<Arc<GatewayCredential>>,
        metering: GatewayMetering,
    ) -> Result<Self, RuntimeError> {
        let artifact = artifact.into();
        // The endpoint is not quoted: a refused one may carry a password in its userinfo.
        let refuse = |message: &str| {
            RuntimeError::Runtime(format!(
                "gateway.endpoint on artifact '{artifact}' {message}"
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
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
    ) -> Result<(http::Response<WasiBody>, ConnectionIo), WasiHttpError> {
        let Some(credential) = self.credential.clone() else {
            return send_direct(self.rewrite(request, None)?, options).await;
        };

        let (parts, body) = request.into_parts();
        let body = body.collect().await?.to_bytes();
        let head = RequestHead {
            method: parts.method,
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
        };

        // An injected credential no controller has supplied: refused here, so the upstream never
        // sees a keyless request it would answer with a rejection of its own.
        let Some(sent) = credential.current().await else {
            return Err(WasiHttpError::InternalError(Some(
                credential.not_injected_message(),
            )));
        };
        let response = send_direct(
            self.rewrite(head.request(body.clone()), Some(&sent))?,
            options,
        )
        .await?;
        if response.0.status() != StatusCode::UNAUTHORIZED {
            credential.clear_rejection();
            return Ok(response);
        }

        let reread = credential.reread_after_rejection().await;
        let Some(reread) = reread.filter(|reread| *reread != sent) else {
            credential
                .record_rejection(StatusCode::UNAUTHORIZED.as_u16(), false)
                .await;
            return Ok(response);
        };
        drop(response);

        let resent = send_direct(self.rewrite(head.request(body), Some(&reread))?, options).await?;
        if resent.0.status() == StatusCode::UNAUTHORIZED {
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
    /// upstream's scheme and authority, so the upstream scheme decides TLS. The body is moved
    /// through untouched.
    ///
    /// `host` must be replaced, not left alone: wasi-http sets it from the authority the guest
    /// addressed, which is the gateway's, and nothing downstream corrects it.
    fn rewrite(
        &self,
        request: http::Request<WasiBody>,
        key: Option<&str>,
    ) -> Result<http::Request<WasiBody>, WasiHttpError> {
        let (mut parts, body) = request.into_parts();

        let header = HeaderName::from_bytes(self.auth.header.as_bytes())
            .map_err(|_| WasiHttpError::InternalError(None))?;
        parts.headers.remove(&header);
        if let Some(key) = key {
            let mut value = HeaderValue::from_str(&self.auth.render(key))
                .map_err(|_| WasiHttpError::InternalError(None))?;
            value.set_sensitive(true);
            parts.headers.insert(header, value);
        }

        let mut uri = Uri::builder()
            .scheme(self.upstream.scheme())
            .authority(self.upstream_authority.as_str());
        if let Some(path_and_query) = parts.uri.path_and_query() {
            uri = uri.path_and_query(path_and_query.clone());
        }
        parts.uri = uri
            .build()
            .map_err(|_| WasiHttpError::HttpRequestUriInvalid)?;
        parts.headers.insert(
            header::HOST,
            HeaderValue::from_str(&self.upstream_authority)
                .map_err(|_| WasiHttpError::HttpRequestUriInvalid)?,
        );

        Ok(http::Request::from_parts(parts, body))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use http_body_util::Empty;
    use murmur_artifact::ApiKeyReference;

    use super::*;

    const KEY: &str = "sk-unit-gateway-marker";

    fn auth(header: &str, value: &str) -> UpstreamAuth {
        UpstreamAuth {
            header: header.to_string(),
            value: value.to_string(),
        }
    }

    fn gateway(endpoint: &str, auth: UpstreamAuth, key: Option<&str>) -> CredentialGateway {
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

    /// A guest request as wasi-http hands it to the `send_request` hook: `host` already set to the
    /// authority the guest addressed.
    fn request(uri: &str, headers: &[(&str, &str)]) -> http::Request<WasiBody> {
        let host = uri.parse::<Uri>().unwrap().authority().unwrap().to_string();
        let mut builder = http::Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::HOST, host);
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
        let rewritten = gateway.rewrite(forged, Some(KEY)).unwrap();
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
        let rewritten = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/chat/completions", &[]),
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
        let rewritten = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/chat/completions?x=1", &[]),
                Some(KEY),
            )
            .unwrap();
        assert_eq!(
            rewritten.uri().to_string(),
            "https://api.moonshot.ai/v1/chat/completions?x=1"
        );
        let hosts: Vec<_> = rewritten.headers().get_all("host").iter().collect();
        assert_eq!(
            hosts,
            vec!["api.moonshot.ai"],
            "the gateway's host is replaced"
        );
    }

    #[test]
    fn credential_gateway_sends_plain_http_to_an_http_upstream_with_its_port() {
        let gateway = gateway(
            "http://localhost:11434",
            auth("x-api-key", "{key}"),
            Some(KEY),
        );
        let rewritten = gateway
            .rewrite(request("http://127.0.0.1:9/api/chat", &[]), Some(KEY))
            .unwrap();
        assert_eq!(
            rewritten.uri().to_string(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(rewritten.headers()["host"], "localhost:11434");
    }

    #[test]
    fn credential_gateway_without_a_key_attaches_nothing_and_still_strips_the_forged_header() {
        let gateway = gateway(
            "https://api.anthropic.com",
            auth("x-api-key", "{key}"),
            None,
        );
        assert!(gateway.credential().is_none());
        let rewritten = gateway
            .rewrite(
                request("http://127.0.0.1:9/v1/messages", &[("X-Api-Key", "forged")]),
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

        let upstream_authority = endpoint.trim_start_matches("http://").to_string();
        let gateway = Arc::new(gateway(&endpoint, auth("x-api-key", "{key}"), Some(KEY)));
        let options = RequestOptions {
            connect_timeout: Some(Duration::from_secs(5)),
            first_byte_timeout: Some(Duration::from_secs(5)),
            between_bytes_timeout: Some(Duration::from_secs(5)),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let status = rt.block_on(async {
            let (response, _io) = gateway
                .send(request("http://127.0.0.1:9/v1/probe", &[]), Some(options))
                .await
                .unwrap();
            response.status()
        });
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);

        let head = first.join().unwrap();
        assert!(head.contains(&format!("x-api-key: {KEY}")), "{head}");
        assert!(
            head.contains(&format!("host: {upstream_authority}\r\n")),
            "{head}"
        );
        assert!(!head.contains("127.0.0.1:9\r\n"), "{head}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            elsewhere.accept().is_err(),
            "the redirect target must receive no connection"
        );
    }

    #[test]
    fn credential_gateway_refuses_userinfo() {
        for endpoint in [
            "https://user:sk-in-url@api.example.com",
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
            assert!(!err.to_string().contains("sk-in-url"), "{err}");
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
