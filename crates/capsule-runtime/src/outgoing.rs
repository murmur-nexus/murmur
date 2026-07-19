use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::a2a::A2aTask;

pub(crate) struct OutgoingMessage {
    pub message_id: String,
    pub context_id: Option<String>,
    pub text: String,
}

/// Send an A2A message/send JSON-RPC request to a peer capsule.
///
/// `peer_url` is in "localhost:{port}" or "http://localhost:{port}" format.
/// `traceparent` is the W3C traceparent header value (omitted if None).
pub(crate) async fn send_a2a_message(
    peer_url: &str,
    message: OutgoingMessage,
    traceparent: Option<String>,
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
    request.push_str("\r\n");
    request.push_str(&body);

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("failed to write request to {peer_url}: {e}"))?;

    let mut reader = BufReader::new(stream);

    // Read and discard status line
    let mut status_line = String::new();
    reader
        .read_line(&mut status_line)
        .await
        .map_err(|e| format!("failed to read status from {peer_url}: {e}"))?;

    // Parse headers for Content-Length
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
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().ok();
        }
    }

    // Read body
    let body_bytes = if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("failed to read response body from {peer_url}: {e}"))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .await
            .map_err(|e| format!("failed to read response body from {peer_url}: {e}"))?;
        buf
    };

    let response: serde_json::Value = serde_json::from_slice(&body_bytes)
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
