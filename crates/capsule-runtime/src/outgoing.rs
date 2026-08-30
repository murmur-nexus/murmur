use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::a2a::A2aTask;
use crate::origin::{stamp_for_peer, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};

pub(crate) struct OutgoingMessage {
    pub message_id: String,
    pub context_id: Option<String>,
    pub text: String,
}

/// Send an A2A message/send JSON-RPC request to a peer capsule.
///
/// `peer_url` is in "localhost:{port}" or "http://localhost:{port}" format.
/// `traceparent` is the W3C traceparent header value (omitted if None).
///
/// `sender_task` is the sending capsule's own current task, `None` when no task is in scope.
/// Its trust class is stamped on the request so the receiver inherits it rather than reclassifying
/// the message as fresh — that is what keeps untrust from evaporating at the first hop. `None`
/// stamps `untrusted`, the safe class. The origin stamped is always `peer`; the guest supplies
/// neither header and has no field in `murmur:message/send` to supply one from.
pub(crate) async fn send_a2a_message(
    peer_url: &str,
    message: OutgoingMessage,
    traceparent: Option<String>,
    sender_task: Option<TaskProvenance>,
) -> Result<A2aTask, String> {
    let addr = parse_host_port(peer_url)?;

    let request_id = format!("req_{}", uuid::Uuid::now_v7().simple());
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "message/send",
        "params": {
            "message": {
                "messageId": message.message_id,
                "contextId": message.context_id,
                "role": "user",
                "parts": [{"text": message.text}]
            }
        }
    })
    .to_string();

    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("failed to connect to {peer_url}: {e}"))?;

    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(ref tp) = traceparent {
        request.push_str(&format!("traceparent: {tp}\r\n"));
    }
    let stamped = stamp_for_peer(sender_task);
    request.push_str(&format!(
        "{PEER_ORIGIN_HEADER}: {}\r\n{PEER_TRUST_HEADER}: {}\r\n",
        stamped.origin().as_str(),
        stamped.trust().as_str(),
    ));
    request.push_str("\r\n");
    request.push_str(&body);

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("failed to write request to {peer_url}: {e}"))?;

    let raw = read_raw_response(BufReader::new(stream), peer_url).await?;

    let response: serde_json::Value = serde_json::from_slice(&raw.body)
        .map_err(|e| format!("failed to parse response from {peer_url}: {e}"))?;

    if let Some(error) = response.get("error") {
        return Err(format!("A2A error from {peer_url}: {error}"));
    }

    let result = response
        .get("result")
        .ok_or_else(|| format!("no result in A2A response from {peer_url}"))?;

    serde_json::from_value(result.clone())
        .map_err(|e| format!("failed to parse A2A task from {peer_url}: {e}"))
}

pub(crate) fn parse_host_port(peer_url: &str) -> Result<String, String> {
    let stripped = peer_url
        .strip_prefix("http://")
        .or_else(|| peer_url.strip_prefix("https://"))
        .unwrap_or(peer_url);
    // Drop any path component
    let host_port = stripped.split('/').next().unwrap_or(stripped);
    if host_port.is_empty() {
        return Err(format!("invalid peer URL: {peer_url}"));
    }
    Ok(host_port.to_string())
}

/// One HTTP response, as the hand-rolled client below reads it back.
pub(crate) struct RawHttpResponse {
    pub status: u16,
    /// Header names lowercased; values trimmed and otherwise verbatim.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawHttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Fetches a peer's agent card.
///
/// The minting side needs the peer's own `name` and `url` to derive the audience a handle is
/// scoped to; both come from the card the peer already publishes, so neither side has to be told
/// the audience string by the other. The caller enforces `capabilities.network.allow` *before*
/// this is reached — **minting grants no new outbound authority**.
pub(crate) async fn fetch_agent_card(peer_url: &str) -> Result<serde_json::Value, String> {
    let response = raw_get(peer_url, "/.well-known/agent-card.json", &[]).await?;
    if response.status != 200 {
        return Err(format!(
            "peer {peer_url} answered {} for its agent card",
            response.status
        ));
    }
    serde_json::from_slice(&response.body)
        .map_err(|error| format!("peer {peer_url} returned an unparseable agent card: {error}"))
}

/// Redeems a handle against a peer's `/resources/peer/<handle>` endpoint, asserting `audience`.
///
/// Returns the response whatever its status: the caller reports the peer's own refusal code
/// rather than flattening every non-200 into one message.
pub(crate) async fn redeem_peer_handle(
    peer_url: &str,
    token: &str,
    audience: &str,
) -> Result<RawHttpResponse, String> {
    raw_get(
        peer_url,
        &format!("{}/{token}", crate::peer_handoff::PEER_PATH_PREFIX),
        &[(crate::peer_handoff::AUDIENCE_HEADER, audience)],
    )
    .await
}

/// A single `GET` over a fresh connection, in the same hand-rolled HTTP/1.1 style as
/// [`send_a2a_message`].
///
/// `Connection: close` on every request and no keep-alive, so the response is complete when the
/// socket is: a peer that omits `content-length` still delimits its body, and one that sends it
/// is read to exactly that length.
async fn raw_get(
    peer_url: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
) -> Result<RawHttpResponse, String> {
    let addr = parse_host_port(peer_url)?;

    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("failed to connect to {peer_url}: {e}"))?;

    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("failed to write request to {peer_url}: {e}"))?;

    read_raw_response(BufReader::new(stream), peer_url).await
}

/// Reads one complete HTTP/1.1 response off a connection the caller has already written to.
///
/// Shared by every outbound request this module makes, so a peer's framing is interpreted one
/// way rather than several. `content-length` bounds the read but never sizes an allocation up
/// front: the length is the peer's claim, and a peer that claims a gigabyte must actually send
/// one before this grows to hold it.
async fn read_raw_response(
    mut reader: BufReader<TcpStream>,
    peer_url: &str,
) -> Result<RawHttpResponse, String> {
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| format!("failed to read status from {peer_url}: {e}"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("unparseable status line from {peer_url}: {status_line:?}"))?;

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = value.parse().ok();
        }
        headers.push((name, value));
    }

    let mut body = Vec::new();
    match content_length {
        Some(len) => {
            reader
                .take(len as u64)
                .read_to_end(&mut body)
                .await
                .map_err(|e| format!("failed to read response body from {peer_url}: {e}"))?;
            if body.len() != len {
                return Err(format!(
                    "peer {peer_url} declared {len} bytes and sent {}",
                    body.len()
                ));
            }
        }
        None => {
            reader
                .read_to_end(&mut body)
                .await
                .map_err(|e| format!("failed to read response body from {peer_url}: {e}"))?;
        }
    }

    Ok(RawHttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::{TaskOrigin, TrustClass};

    /// Accept one connection, read the request head, and answer a minimal `message/send` result.
    async fn capture_one_request(listener: tokio::net::TcpListener) -> String {
        let (mut stream, _) = listener.accept().await.expect("peer should connect");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read_exact(&mut byte).await {
                Ok(_) => head.push(byte[0]),
                Err(_) => break,
            }
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req_1",
            "result": {
                "id": "tsk_peer",
                "contextId": "ctx_peer",
                "status": {"state": "submitted"}
            }
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        String::from_utf8_lossy(&head).to_string()
    }

    async fn request_head_for(sender_task: Option<TaskProvenance>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(capture_one_request(listener));

        let message = OutgoingMessage {
            message_id: "msg_1".to_string(),
            context_id: None,
            text: "hello".to_string(),
        };
        send_a2a_message(&addr, message, None, sender_task)
            .await
            .expect("peer should answer with a task");
        server.await.expect("capture task should not panic")
    }

    /// The sending runtime stamps the class of its own current task, and the guest has no say:
    /// `murmur:message/send` carries no origin or trust field for a capsule author to set.
    #[tokio::test]
    async fn outbound_send_stamps_the_senders_own_trust_class() {
        let cases = [
            (
                Some(TaskProvenance::derive(
                    TaskOrigin::Event,
                    Some(TrustClass::Untrusted),
                )),
                "untrusted",
            ),
            (
                Some(TaskProvenance::derive(TaskOrigin::User, None)),
                "trusted",
            ),
            (None, "untrusted"),
        ];
        for (sender_task, expected_trust) in cases {
            let head = request_head_for(sender_task).await;
            assert!(
                head.contains(&format!("{PEER_ORIGIN_HEADER}: peer\r\n")),
                "outbound origin must always be peer; head was:\n{head}"
            );
            assert!(
                head.contains(&format!("{PEER_TRUST_HEADER}: {expected_trust}\r\n")),
                "expected {expected_trust} for {sender_task:?}; head was:\n{head}"
            );
        }
    }
}
