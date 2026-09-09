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
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;
use url::Url;

use crate::dns_resolver::Resolution;

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
    let mut stream = connect_within(host, port, timeout)?;
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

/// The first address `host` resolves to that accepts a connection on `port` within `timeout`.
///
/// The name goes through [`crate::dns_resolver`], the same in-process resolver a capsule's own
/// lookups use, so the runtime's outbound requests are bounded by the same deadline and report the
/// same three outcomes. `TcpStream::connect` has no deadline of its own, so a host that neither
/// accepts nor refuses would hold the caller for as long as the OS retries.
fn connect_within(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, String> {
    let addr = format!("{host}:{port}");
    let resolution = crate::dns_resolver::resolve(host);
    let Resolution::Resolved(addresses) = &resolution else {
        return Err(unresolved(&addr, &resolution));
    };
    if addresses.is_empty() {
        return Err(unresolved(&addr, &resolution));
    }

    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&SocketAddr::new(*address, port), timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(error) => format!("failed to connect to {addr}: {error}"),
        None => format!("failed to connect to {addr}: it resolved to no address"),
    })
}

/// The `Err` text a lookup that produced no address to dial turns into.
///
/// A host that does not exist and a resolver that did not answer are different failures with
/// different remedies — check the name, versus try again — so they read differently here. Neither
/// text carries anything the caller supplied beyond the host and port, which is what keeps a
/// spawn credential in `x-murmur-spawn-credential` out of every error this module returns.
fn unresolved(addr: &str, resolution: &Resolution) -> String {
    match resolution {
        Resolution::DoesNotExist => format!("failed to resolve {addr}: the host does not exist"),
        Resolution::DidNotAnswer(reason) => format!("failed to resolve {addr}: {reason}"),
        Resolution::Resolved(_) => {
            format!("failed to connect to {addr}: it resolved to no address")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::{Ipv4Addr, TcpListener};

    use super::{http_json, unresolved};
    use crate::dns_resolver::{NoAnswer, Resolution};

    /// A listener that answers exactly one request with one JSON body, then stops.
    fn one_shot_json_server() -> u16 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" {
                        break;
                    }
                    line.clear();
                }
                let body = r#"{"ok":true}"#;
                let _ = std::io::Write::write_all(
                    &mut stream,
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        port
    }

    #[test]
    fn an_address_literal_and_a_hosts_file_name_both_reach_the_same_listener() {
        let port = one_shot_json_server();

        let literal = http_json("GET", &format!("http://127.0.0.1:{port}/"), None, &[]).unwrap();
        assert_eq!(literal["ok"], true);

        let named = http_json("GET", &format!("http://localhost:{port}/"), None, &[]).unwrap();
        assert_eq!(named["ok"], true);
    }

    #[test]
    fn a_host_that_does_not_exist_and_a_resolver_that_did_not_answer_read_differently() {
        let missing = unresolved("api.example.test:80", &Resolution::DoesNotExist);
        let silent = unresolved(
            "api.example.test:80",
            &Resolution::DidNotAnswer(NoAnswer::DeadlineElapsed),
        );

        assert!(missing.contains("the host does not exist"), "{missing}");
        assert!(
            silent.contains("the resolver did not answer within 5s"),
            "{silent}"
        );
        assert_ne!(missing, silent);
    }

    #[test]
    fn a_failed_request_never_carries_a_request_header_or_body() {
        let error = http_json(
            "POST",
            "http://name.no.capsule.will.ever.have.invalid:9/",
            Some(r#"{"secret":"body-value"}"#),
            &[("x-murmur-spawn-credential", "credential-value")],
        )
        .unwrap_err();

        assert!(!error.contains("credential-value"), "{error}");
        assert!(!error.contains("body-value"), "{error}");
        assert!(!error.contains("x-murmur-spawn-credential"), "{error}");
    }
}
