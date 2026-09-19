//! A loopback HTTP/1.1 upstream that records every request it reads and answers each with the
//! reply a per-request handler returns, for tests that assert on bytes a gateway put on a socket.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

/// One request as the upstream read it off the socket.
#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub target: String,
    /// Names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
    /// Dechunked when the request was sent `transfer-encoding: chunked`.
    pub body: Vec<u8>,
    /// When the request head was complete.
    pub arrived: Instant,
}

impl RecordedRequest {
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What the upstream answers one request with. Always sent with `content-length` and
/// `connection: close`.
pub struct Reply {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

impl Reply {
    /// `200` with a JSON body.
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.into(),
        }
    }
}

type Handler = dyn Fn(usize, &RecordedRequest) -> Reply + Send + Sync;

pub struct RecordingUpstream {
    /// `http://127.0.0.1:<port>`, no trailing `/`.
    pub endpoint: String,
    pub port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingUpstream {
    /// Answers every request with [`Reply::json`] of `body`.
    pub fn replying(body: &'static str) -> Self {
        Self::with_handler(move |_, _| Reply::json(body))
    }

    /// Answers the n-th request (from 0) with what `handler` returns for it, called after the
    /// request is recorded, so a handler that reads [`Self::requests`] sees it.
    pub fn with_handler(
        handler: impl Fn(usize, &RecordedRequest) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let handler: Arc<Handler> = Arc::new(handler);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                let index = {
                    let mut recorded = recorded.lock().unwrap();
                    recorded.push(request.clone());
                    recorded.len() - 1
                };
                let reply = handler(index, &request);
                let reason = if reply.status == 200 { "OK" } else { "Reply" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{}",
                    reply.status,
                    reply.content_type,
                    reply.body.len(),
                    reply.body
                );
                let _ = stream.flush();
            }
        });
        Self {
            endpoint: format!("http://127.0.0.1:{port}"),
            port,
            requests,
        }
    }

    /// Every request recorded so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

/// Reads one request head and its body, sized by `content-length` or `transfer-encoding:
/// chunked`. `None` when the peer closes before a complete head.
pub fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let arrived = Instant::now();
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut body = buffer[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let content_length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    if let Some(length) = content_length {
        while body.len() < length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
    } else if chunked {
        while !body.windows(5).any(|w| w == b"0\r\n\r\n") {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body = dechunk(&body);
    }
    Some(RecordedRequest {
        method,
        target,
        headers,
        body,
        arrived,
    })
}

fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&rest[..pos]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = pos + 2;
        out.extend_from_slice(&rest[start..start + size]);
        rest = &rest[start + size + 2..];
    }
    out
}
