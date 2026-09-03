//! The one blocking HTTP/1.1 client this crate speaks to `mur-roost` and to a peer capsule with.
//!
//! Kept deliberately small and shared rather than re-implemented per caller: the plan scheduler's
//! `capsule` step, session registration and the child launcher all address the same loopback
//! daemon, and a second client would be a second place for a request header — including one
//! carrying a spawn token — to be formatted into an error string.
//!
//! [`http_json`] never puts a request header, or the request body, into the `Err` it returns. Only
//! the response status line and the response body reach an error message, so a token presented in
//! `x-murmur-spawn-credential` cannot travel back out through a failure.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::Value;
use url::Url;

/// Deadline every request gets unless the caller states a shorter one: long enough for a daemon
/// that is staging a child, short enough that a session cannot block on it forever.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// One request, one response, one connection, under [`DEFAULT_TIMEOUT`].
///
/// `extra_headers` are appended verbatim after the framing headers. `body` is sent as
/// `application/json` on `POST`, and ignored otherwise.
pub(crate) fn http_json(
    method: &str,
    url: &str,
    body: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> Result<Value, String> {
    http_json_with_timeout(method, url, body, extra_headers, DEFAULT_TIMEOUT)
}

/// [`http_json`] with the connect, write and read deadline named by the caller.
///
/// `timeout` bounds each of the three separately, not the call as a whole: an interactive caller
/// asking an address nothing answers waits for the connect refusal or the deadline, whichever
/// comes first.
pub(crate) fn http_json_with_timeout(
    method: &str,
    url: &str,
    body: Option<&str>,
    extra_headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<Value, String> {
    let url = Url::parse(url).map_err(|error| format!("invalid URL '{url}': {error}"))?;
    if url.scheme() != "http" {
        return Err(format!("unsupported URL scheme '{}'", url.scheme()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("URL '{url}' has no host"))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");
    let mut stream = connect_within(&addr, timeout)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| error.to_string())?;

    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let body = body.unwrap_or("");
    let extra: String = extra_headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    let request = if method == "POST" {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
            body.len()
        )
    } else {
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n")
    };
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("failed to write HTTP request: {error}"))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| format!("failed to read HTTP response: {error}"))?;
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Err("invalid HTTP response".to_string());
    };
    if !headers.starts_with("HTTP/1.1 2") && !headers.starts_with("HTTP/1.0 2") {
        return Err(format!("HTTP request failed: {headers}; body: {body}"));
    }
    serde_json::from_str(body).map_err(|error| format!("failed to parse HTTP JSON: {error}"))
}

/// The first address `addr` resolves to that accepts a connection within `timeout`.
///
/// `TcpStream::connect` has no deadline of its own, so a host that neither accepts nor refuses
/// would hold the caller for as long as the OS retries.
fn connect_within(addr: &str, timeout: Duration) -> Result<TcpStream, String> {
    let resolved = addr
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve {addr}: {error}"))?;
    let mut last_error = None;
    for socket_addr in resolved {
        match TcpStream::connect_timeout(&socket_addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(error) => format!("failed to connect to {addr}: {error}"),
        None => format!("failed to connect to {addr}: it resolved to no address"),
    })
}
