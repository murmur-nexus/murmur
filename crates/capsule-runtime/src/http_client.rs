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
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;
use url::Url;

/// One request, one response, one connection.
///
/// `extra_headers` are appended verbatim after the framing headers. `body` is sent as
/// `application/json` on `POST`, and ignored otherwise.
pub(crate) fn http_json(
    method: &str,
    url: &str,
    body: Option<&str>,
    extra_headers: &[(&str, &str)],
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
    let mut stream = TcpStream::connect(&addr)
        .map_err(|error| format!("failed to connect to {addr}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
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
