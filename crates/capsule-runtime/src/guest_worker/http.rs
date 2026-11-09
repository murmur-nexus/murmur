//! `wasi:http` across the worker connection: the worker's hooks send each outgoing request to the
//! supervisor, which applies the call's network policy, gateway and formation routes exactly as an
//! in-process store does, and streams the response back.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use wasmtime_wasi_http::{Error as WasiHttpError, RequestOptions, WasiBody};

use super::wire::{HttpError, HttpRequestHead, HttpResponseHead};
use super::RemoteLink;
use crate::credential_gateway::ConnectionIo;

/// What `send_request` returns: the response head and its connection, or the guest's error.
type SendResponse = Box<
    dyn Future<Output = Result<(http::Response<WasiBody>, ConnectionIo), WasiHttpError>> + Send,
>;

/// A worker store's `wasi:http` hooks: every request goes to the supervisor.
pub(crate) struct RemoteHttp {
    link: Arc<RemoteLink>,
}

impl RemoteHttp {
    pub(crate) fn new(link: Arc<RemoteLink>) -> Self {
        Self { link }
    }

    pub(crate) fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _response_errors: Box<dyn Future<Output = Result<(), WasiHttpError>> + Send>,
    ) -> SendResponse {
        let link = Arc::clone(&self.link);
        Box::new(async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await?.to_bytes().to_vec();
            let head = HttpRequestHead {
                method: parts.method.as_str().to_string(),
                uri: parts.uri.to_string(),
                headers: parts
                    .headers
                    .iter()
                    .map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
                    .collect(),
                connect_timeout: options.as_ref().and_then(|o| o.connect_timeout),
                first_byte_timeout: options.as_ref().and_then(|o| o.first_byte_timeout),
                between_bytes_timeout: options.as_ref().and_then(|o| o.between_bytes_timeout),
                has_options: options.is_some(),
                body,
            };
            let (head, chunks) = link.http(head).await.map_err(from_wire)?;
            let response = response_from_head(head, ChannelBody { chunks }.boxed_unsync())?;
            let io: ConnectionIo = Box::new(std::future::ready(Ok(())));
            Ok((response, io))
        })
    }
}

/// A response body fed by the supervisor's `BodyChunk`/`BodyEnd` frames.
pub(crate) struct ChannelBody {
    pub(crate) chunks: mpsc::UnboundedReceiver<Result<Bytes, HttpError>>,
}

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = WasiHttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, WasiHttpError>>> {
        match self.chunks.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(from_wire(error)))),
        }
    }
}

fn response_from_head(
    head: HttpResponseHead,
    body: WasiBody,
) -> Result<http::Response<WasiBody>, WasiHttpError> {
    let mut builder = http::Response::builder().status(head.status);
    for (name, value) in head.headers {
        builder = builder.header(name, value);
    }
    builder
        .body(body)
        .map_err(|error| WasiHttpError::InternalError(Some(error.to_string())))
}

/// The request `head` describes, for the supervisor to send through the call's policy.
pub(crate) fn request_from_head(
    head: HttpRequestHead,
) -> Result<(http::Request<WasiBody>, Option<RequestOptions>), WasiHttpError> {
    let mut builder = http::Request::builder()
        .method(head.method.as_str())
        .uri(head.uri.as_str());
    for (name, value) in head.headers {
        builder = builder.header(name, value);
    }
    let body = http_body_util::Full::new(Bytes::from(head.body))
        .map_err(|never| match never {})
        .boxed_unsync();
    let request = builder
        .body(body)
        .map_err(|_| WasiHttpError::HttpRequestUriInvalid)?;
    let options = head.has_options.then_some(RequestOptions {
        connect_timeout: head.connect_timeout,
        first_byte_timeout: head.first_byte_timeout,
        between_bytes_timeout: head.between_bytes_timeout,
    });
    Ok((request, options))
}

/// The head of `response`, to send the worker.
pub(crate) fn head_of(response: &http::Response<WasiBody>) -> HttpResponseHead {
    HttpResponseHead {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_string(), value.as_bytes().to_vec()))
            .collect(),
    }
}

/// `error` as the code a guest would be shown for it in-process, the conversion
/// `wasmtime_wasi_http`'s default hooks apply to the I/O-bearing variants included.
pub(crate) fn to_wire(error: &WasiHttpError) -> HttpError {
    use std::error::Error as _;
    match error {
        WasiHttpError::Hyper(err) => {
            if let Some(code) = err.source().and_then(|cause| {
                cause.downcast_ref::<wasmtime_wasi_http::p2::bindings::http::types::ErrorCode>()
            }) {
                return to_wire(&WasiHttpError::from(code.clone()));
            }
            HttpError::HttpProtocolError
        }
        WasiHttpError::Connect(err) => {
            if err.kind() == std::io::ErrorKind::AddrNotAvailable
                || err
                    .to_string()
                    .starts_with("failed to lookup address information")
            {
                return HttpError::DnsError {
                    rcode: Some("address not available".to_string()),
                    info_code: None,
                };
            }
            HttpError::ConnectionRefused
        }
        WasiHttpError::Tls(_) => HttpError::TlsProtocolError,
        WasiHttpError::InvalidDnsNameError(_) => HttpError::DnsError {
            rcode: Some("invalid dns name".to_string()),
            info_code: None,
        },
        WasiHttpError::HttpRequestBodySize(size) => HttpError::HttpRequestBodySize(*size),
        WasiHttpError::DnsTimeout => HttpError::DnsTimeout,
        WasiHttpError::DnsError { rcode, info_code } => HttpError::DnsError {
            rcode: rcode.clone(),
            info_code: *info_code,
        },
        WasiHttpError::DestinationNotFound => HttpError::DestinationNotFound,
        WasiHttpError::DestinationUnavailable => HttpError::DestinationUnavailable,
        WasiHttpError::DestinationIpProhibited => HttpError::DestinationIpProhibited,
        WasiHttpError::DestinationIpUnroutable => HttpError::DestinationIpUnroutable,
        WasiHttpError::ConnectionRefused => HttpError::ConnectionRefused,
        WasiHttpError::ConnectionTerminated => HttpError::ConnectionTerminated,
        WasiHttpError::ConnectionTimeout => HttpError::ConnectionTimeout,
        WasiHttpError::ConnectionReadTimeout => HttpError::ConnectionReadTimeout,
        WasiHttpError::ConnectionWriteTimeout => HttpError::ConnectionWriteTimeout,
        WasiHttpError::ConnectionLimitReached => HttpError::ConnectionLimitReached,
        WasiHttpError::TlsProtocolError => HttpError::TlsProtocolError,
        WasiHttpError::TlsCertificateError => HttpError::TlsCertificateError,
        WasiHttpError::TlsAlertReceived {
            alert_id,
            alert_message,
        } => HttpError::TlsAlertReceived {
            alert_id: *alert_id,
            alert_message: alert_message.clone(),
        },
        WasiHttpError::HttpRequestDenied => HttpError::HttpRequestDenied,
        WasiHttpError::HttpRequestLengthRequired => HttpError::HttpRequestLengthRequired,
        WasiHttpError::HttpRequestMethodInvalid => HttpError::HttpRequestMethodInvalid,
        WasiHttpError::HttpRequestUriInvalid => HttpError::HttpRequestUriInvalid,
        WasiHttpError::HttpRequestUriTooLong => HttpError::HttpRequestUriTooLong,
        WasiHttpError::HttpRequestHeaderSectionSize(size) => {
            HttpError::HttpRequestHeaderSectionSize(*size)
        }
        WasiHttpError::HttpRequestHeaderSize {
            field_name,
            field_size,
        } => HttpError::HttpRequestHeaderSize {
            field_name: field_name.clone(),
            field_size: *field_size,
        },
        WasiHttpError::HttpRequestTrailerSectionSize(size) => {
            HttpError::HttpRequestTrailerSectionSize(*size)
        }
        WasiHttpError::HttpRequestTrailerSize {
            field_name,
            field_size,
        } => HttpError::HttpRequestTrailerSize {
            field_name: field_name.clone(),
            field_size: *field_size,
        },
        WasiHttpError::HttpResponseIncomplete => HttpError::HttpResponseIncomplete,
        WasiHttpError::HttpResponseHeaderSectionSize(size) => {
            HttpError::HttpResponseHeaderSectionSize(*size)
        }
        WasiHttpError::HttpResponseHeaderSize {
            field_name,
            field_size,
        } => HttpError::HttpResponseHeaderSize {
            field_name: field_name.clone(),
            field_size: *field_size,
        },
        WasiHttpError::HttpResponseBodySize(size) => HttpError::HttpResponseBodySize(*size),
        WasiHttpError::HttpResponseTrailerSectionSize(size) => {
            HttpError::HttpResponseTrailerSectionSize(*size)
        }
        WasiHttpError::HttpResponseTrailerSize {
            field_name,
            field_size,
        } => HttpError::HttpResponseTrailerSize {
            field_name: field_name.clone(),
            field_size: *field_size,
        },
        WasiHttpError::HttpResponseTransferCoding(coding) => {
            HttpError::HttpResponseTransferCoding(coding.clone())
        }
        WasiHttpError::HttpResponseContentCoding(coding) => {
            HttpError::HttpResponseContentCoding(coding.clone())
        }
        WasiHttpError::HttpResponseTimeout => HttpError::HttpResponseTimeout,
        WasiHttpError::HttpUpgradeFailed => HttpError::HttpUpgradeFailed,
        WasiHttpError::HttpProtocolError => HttpError::HttpProtocolError,
        WasiHttpError::LoopDetected => HttpError::LoopDetected,
        WasiHttpError::ConfigurationError => HttpError::ConfigurationError,
        WasiHttpError::InternalError(message) => HttpError::InternalError(message.clone()),
        other => HttpError::InternalError(Some(other.to_string())),
    }
}

/// The error a worker hands its guest for `error`.
pub(crate) fn from_wire(error: HttpError) -> WasiHttpError {
    match error {
        HttpError::HttpRequestBodySize(size) => WasiHttpError::HttpRequestBodySize(size),
        HttpError::DnsTimeout => WasiHttpError::DnsTimeout,
        HttpError::DnsError { rcode, info_code } => WasiHttpError::DnsError { rcode, info_code },
        HttpError::DestinationNotFound => WasiHttpError::DestinationNotFound,
        HttpError::DestinationUnavailable => WasiHttpError::DestinationUnavailable,
        HttpError::DestinationIpProhibited => WasiHttpError::DestinationIpProhibited,
        HttpError::DestinationIpUnroutable => WasiHttpError::DestinationIpUnroutable,
        HttpError::ConnectionRefused => WasiHttpError::ConnectionRefused,
        HttpError::ConnectionTerminated => WasiHttpError::ConnectionTerminated,
        HttpError::ConnectionTimeout => WasiHttpError::ConnectionTimeout,
        HttpError::ConnectionReadTimeout => WasiHttpError::ConnectionReadTimeout,
        HttpError::ConnectionWriteTimeout => WasiHttpError::ConnectionWriteTimeout,
        HttpError::ConnectionLimitReached => WasiHttpError::ConnectionLimitReached,
        HttpError::TlsProtocolError => WasiHttpError::TlsProtocolError,
        HttpError::TlsCertificateError => WasiHttpError::TlsCertificateError,
        HttpError::TlsAlertReceived {
            alert_id,
            alert_message,
        } => WasiHttpError::TlsAlertReceived {
            alert_id,
            alert_message,
        },
        HttpError::HttpRequestDenied => WasiHttpError::HttpRequestDenied,
        HttpError::HttpRequestLengthRequired => WasiHttpError::HttpRequestLengthRequired,
        HttpError::HttpRequestMethodInvalid => WasiHttpError::HttpRequestMethodInvalid,
        HttpError::HttpRequestUriInvalid => WasiHttpError::HttpRequestUriInvalid,
        HttpError::HttpRequestUriTooLong => WasiHttpError::HttpRequestUriTooLong,
        HttpError::HttpRequestHeaderSectionSize(size) => {
            WasiHttpError::HttpRequestHeaderSectionSize(size)
        }
        HttpError::HttpRequestHeaderSize {
            field_name,
            field_size,
        } => WasiHttpError::HttpRequestHeaderSize {
            field_name,
            field_size,
        },
        HttpError::HttpRequestTrailerSectionSize(size) => {
            WasiHttpError::HttpRequestTrailerSectionSize(size)
        }
        HttpError::HttpRequestTrailerSize {
            field_name,
            field_size,
        } => WasiHttpError::HttpRequestTrailerSize {
            field_name,
            field_size,
        },
        HttpError::HttpResponseIncomplete => WasiHttpError::HttpResponseIncomplete,
        HttpError::HttpResponseHeaderSectionSize(size) => {
            WasiHttpError::HttpResponseHeaderSectionSize(size)
        }
        HttpError::HttpResponseHeaderSize {
            field_name,
            field_size,
        } => WasiHttpError::HttpResponseHeaderSize {
            field_name,
            field_size,
        },
        HttpError::HttpResponseBodySize(size) => WasiHttpError::HttpResponseBodySize(size),
        HttpError::HttpResponseTrailerSectionSize(size) => {
            WasiHttpError::HttpResponseTrailerSectionSize(size)
        }
        HttpError::HttpResponseTrailerSize {
            field_name,
            field_size,
        } => WasiHttpError::HttpResponseTrailerSize {
            field_name,
            field_size,
        },
        HttpError::HttpResponseTransferCoding(coding) => {
            WasiHttpError::HttpResponseTransferCoding(coding)
        }
        HttpError::HttpResponseContentCoding(coding) => {
            WasiHttpError::HttpResponseContentCoding(coding)
        }
        HttpError::HttpResponseTimeout => WasiHttpError::HttpResponseTimeout,
        HttpError::HttpUpgradeFailed => WasiHttpError::HttpUpgradeFailed,
        HttpError::HttpProtocolError => WasiHttpError::HttpProtocolError,
        HttpError::LoopDetected => WasiHttpError::LoopDetected,
        HttpError::ConfigurationError => WasiHttpError::ConfigurationError,
        HttpError::InternalError(message) => WasiHttpError::InternalError(message),
    }
}
