use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use crate::a2a::{
    murmur_state_name, send_message_member, user_message, A2A_PROTOCOL_VERSION, A2A_VERSION_HEADER,
};
use crate::origin::{stamp_for_peer, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};

pub(crate) struct OutgoingMessage {
    pub message_id: String,
    pub context_id: Option<String>,
    pub text: String,
}

/// The task a peer's door answered a [`send_a2a_message`] with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerTask {
    pub id: String,
    pub context_id: String,
    /// The task's state in murmur's word, mapped from the wire by [`murmur_state_name`]:
    /// `auth-required` among them, which a peer that is not murmur may answer.
    pub state: &'static str,
}

/// The guest's `task-result`, which keeps murmur's words: its `state` is the one the peer's wire
/// state was mapped to when it was read.
impl From<PeerTask> for crate::bindings::host::murmur::message::send::TaskResult {
    fn from(task: PeerTask) -> Self {
        Self {
            task_id: task.id,
            context_id: task.context_id,
            state: task.state.to_string(),
        }
    }
}

/// Send an A2A SendMessage JSON-RPC request to a peer capsule.
///
/// `peer_url` is in "localhost:{port}" or "http://localhost:{port}" format.
/// `traceparent` is the W3C traceparent header value (omitted if None).
///
/// `sender_task` is the sending capsule's own current task, `None` when no task is in scope.
/// Its trust class is stamped on the request so the receiver inherits it rather than reclassifying
/// the message as fresh — that is what keeps untrust from evaporating at the first hop. `None`
/// stamps `untrusted`, the safe class. The origin stamped is always `peer`; the guest supplies
/// neither header and has no field in `murmur:message/send` to supply one from.
///
/// `authorization` is the formation token the runtime presents for a call to a formation callee,
/// sent as `Authorization: Bearer <token>`. It is read only at the write, and `None` sends no
/// `Authorization` at all.
///
/// A peer that answers with a message rather than a task, or with a state that is not a
/// meaningful A2A v1.0 task state, is an `Err` naming what it sent.
pub(crate) async fn send_a2a_message(
    peer_url: &str,
    message: OutgoingMessage,
    traceparent: Option<String>,
    sender_task: Option<TaskProvenance>,
    authorization: Option<&crate::formation_credentials::FormationToken>,
) -> Result<PeerTask, String> {
    let addr = parse_host_port(peer_url)?;

    let request_id = format!("req_{}", uuid::Uuid::now_v7().simple());
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "SendMessage",
        "params": {
            "message": user_message(
                &message.message_id,
                message.context_id.as_deref(),
                &message.text,
            )
        }
    })
    .to_string();

    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("failed to connect to {peer_url}: {e}"))?;

    let mut request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{A2A_VERSION_HEADER}: {A2A_PROTOCOL_VERSION}\r\n",
        body.len()
    );
    if let Some(token) = authorization {
        request.push_str(&format!("Authorization: Bearer {}\r\n", token.expose()));
    }
    for (name, value) in peer_call_headers(sender_task, traceparent.as_deref()) {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(&body);

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("failed to write request to {peer_url}: {e}"))?;

    let raw = read_raw_response(BufReader::new(stream), peer_url).await?;
    if !(200..300).contains(&raw.status) {
        return Err(refused_message(peer_url, &raw));
    }

    let response: serde_json::Value = serde_json::from_slice(&raw.body)
        .map_err(|e| format!("failed to parse response from {peer_url}: {e}"))?;

    if let Some(error) = response.get("error") {
        return Err(format!("A2A error from {peer_url}: {error}"));
    }

    let result = response
        .get("result")
        .ok_or_else(|| format!("no result in A2A response from {peer_url}"))?;

    peer_task(result).map_err(|e| format!("failed to parse A2A task from {peer_url}: {e}"))
}

/// The task a peer's `SendMessage` `result` holds, or why it holds none murmur can read.
fn peer_task(result: &serde_json::Value) -> Result<PeerTask, String> {
    let task = match send_message_member(result)? {
        ("task", task) => task,
        (_, message) => {
            return Err(format!(
                "it answered a message and started no task: {message}"
            ))
        }
    };
    let field = |pointer: &str| {
        task.pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("its task has no {}: {task}", &pointer[1..]))
    };
    let (id, context_id, wire) = (field("/id")?, field("/contextId")?, field("/status/state")?);
    let state =
        murmur_state_name(wire).ok_or_else(|| format!("its task is in no known state: {wire}"))?;
    Ok(PeerTask {
        id: id.to_string(),
        context_id: context_id.to_string(),
        state,
    })
}

/// The headers every request to a peer presents besides `Authorization`: the provenance stamped
/// for the sending task — see [`stamp_for_peer`] — and the trace context it continues.
pub(crate) fn peer_call_headers(
    sender_task: Option<TaskProvenance>,
    traceparent: Option<&str>,
) -> Vec<(&'static str, String)> {
    let stamped = stamp_for_peer(sender_task);
    let mut headers = vec![
        (PEER_ORIGIN_HEADER, stamped.origin().as_str().to_string()),
        (PEER_TRUST_HEADER, stamped.trust().as_str().to_string()),
    ];
    if let Some(traceparent) = traceparent {
        headers.push(("traceparent", traceparent.to_string()));
    }
    headers
}

/// How a door that refused a request is named: `status`, followed by the refusal's own `error`
/// code when `body` is a JSON object carrying one, and the body's `message` when it has one.
pub(crate) fn refusal_status(status: &str, body: &[u8]) -> (String, Option<String>) {
    let body: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let field = |name: &str| {
        body.as_ref()
            .and_then(|body| body.get(name))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let status = match field("error") {
        Some(code) => format!("{status} {code}"),
        None => status.to_string(),
    };
    (status, field("message"))
}

/// What a non-2xx answer to [`send_a2a_message`] is reported as: the status, and the receiver's
/// own `error` code and `message` when its body is a JSON object carrying them, so a guest reads
/// why the peer refused rather than a bare status.
fn refused_message(peer_url: &str, raw: &RawHttpResponse) -> String {
    match refusal_status(&raw.status.to_string(), &raw.body) {
        (status, Some(message)) => {
            format!("peer at {peer_url} refused the message ({status}): {message}")
        }
        (status, None) => format!("peer at {peer_url} refused the message ({status})"),
    }
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
/// The minting side needs the peer's own `name` and `JSONRPC` interface `url` to derive the
/// audience a handle is scoped to; both come from the card the peer already publishes, so neither
/// side has to be told the audience string by the other. The caller enforces
/// `capabilities.network.allow` *before* this is reached — **minting grants no new outbound
/// authority**.
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

    /// Accept one connection, read the request head and body, and answer a minimal `SendMessage`
    /// result. Returns the head followed by the body.
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
        let length = String::from_utf8_lossy(&head)
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|length| length.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut request_body = vec![0u8; length];
        let _ = stream.read_exact(&mut request_body).await;
        head.extend_from_slice(&request_body);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "req_1",
            "result": {"task": {
                "id": "tsk_peer",
                "contextId": "ctx_peer",
                "status": {"state": "TASK_STATE_SUBMITTED"}
            }}
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
        send_a2a_message(&addr, message, None, sender_task, None)
            .await
            .expect("peer should answer with a task");
        server.await.expect("capture task should not panic")
    }

    fn hello() -> OutgoingMessage {
        OutgoingMessage {
            message_id: "msg_1".to_string(),
            context_id: None,
            text: "hello".to_string(),
        }
    }

    /// The guest's message goes out as an A2A 1.0 `SendMessage`, naming its version.
    #[tokio::test]
    async fn outbound_send_is_a_v1_send_message_naming_its_version() {
        let request = request_head_for(None).await;
        let (head, body) = request.split_once("\r\n\r\n").expect("a head and a body");
        assert!(
            head.contains("\r\nA2A-Version: 1.0\r\n"),
            "head was:\n{head}"
        );
        let body: serde_json::Value = serde_json::from_str(body).expect("a JSON body");
        assert_eq!(body["method"], "SendMessage");
        let message = &body["params"]["message"];
        assert_eq!(message["role"], "ROLE_USER");
        assert_eq!(
            message["parts"],
            serde_json::json!([{"text": "hello", "mediaType": "text/plain"}])
        );
        assert!(message.get("contextId").is_none(), "{message}");
    }

    /// A door that does not consent answers this runtime's peer message with `403`, and the
    /// sender reports the status and the receiver's own reason rather than a bare code.
    #[tokio::test]
    async fn a_refused_peer_message_names_the_status_and_the_receivers_reason() {
        let (addr, _shutdown, mut task_rx) = crate::identity::serve_test_door(false).await;
        let error = send_a2a_message(&addr, hello(), None, None, None)
            .await
            .expect_err("a door that does not consent refuses the message");
        assert_eq!(
            error,
            format!(
                "peer at {addr} refused the message (403 peer_not_accepted): this capsule does \
                 not accept tasks from peers"
            )
        );
        assert!(task_rx.try_recv().is_err(), "nothing reached the loop");
    }

    /// The same door, consenting, takes the message as a task.
    #[tokio::test]
    async fn a_consenting_door_takes_the_peer_message() {
        let (addr, _shutdown, mut task_rx) = crate::identity::serve_test_door(true).await;
        let task = send_a2a_message(&addr, hello(), None, None, None)
            .await
            .expect("a consenting door takes the message");
        assert_eq!(task.state, "submitted");
        assert!(task.id.starts_with("tsk_"), "{task:?}");
        let result = crate::bindings::host::murmur::message::send::TaskResult::from(task);
        assert_eq!(result.state, "submitted");
        let incoming = task_rx.recv().await.expect("the task reached the loop");
        assert_eq!(incoming.provenance.origin(), TaskOrigin::Peer);
    }

    /// A non-2xx answer whose body is not the JSON shape still names the status.
    #[test]
    fn a_refusal_without_a_json_reason_names_the_status_alone() {
        let raw = RawHttpResponse {
            status: 502,
            headers: Vec::new(),
            body: b"bad gateway".to_vec(),
        };
        assert_eq!(
            refused_message("localhost:1", &raw),
            "peer at localhost:1 refused the message (502)"
        );
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

    /// A peer's answer as `send_a2a_message` reads it: `result`, served once on a local port.
    async fn answered(result: serde_json::Value) -> Result<PeerTask, String> {
        let (addr, sent) = crate::http_client::capture::answer_one(
            serde_json::json!({"jsonrpc": "2.0", "id": "req_1", "result": result}).to_string(),
        );
        let task = send_a2a_message(&addr, hello(), None, None, None).await;
        sent.join().unwrap();
        task
    }

    fn peer_answer(state: &str) -> serde_json::Value {
        serde_json::json!({"task": {"id": "tsk_p", "contextId": "ctx_p", "status": {"state": state}}})
    }

    /// A peer that is not murmur may answer a state this door never produces; the guest reads it
    /// in murmur's word.
    #[tokio::test]
    async fn a_peer_answering_auth_required_reads_as_auth_required() {
        let task = answered(peer_answer("TASK_STATE_AUTH_REQUIRED"))
            .await
            .unwrap();
        assert_eq!(
            task,
            PeerTask {
                id: "tsk_p".to_string(),
                context_id: "ctx_p".to_string(),
                state: "auth-required",
            }
        );
    }

    /// A message rather than a task, or a state that is no task state, is an error naming what
    /// the peer sent.
    #[tokio::test]
    async fn a_peer_answering_no_readable_task_is_an_error_naming_it() {
        let error = answered(serde_json::json!({"message": {"messageId": "msg_p"}}))
            .await
            .unwrap_err();
        assert!(error.contains("answered a message"), "{error}");
        assert!(error.contains("msg_p"), "{error}");
        for state in ["TASK_STATE_UNSPECIFIED", "submitted"] {
            let error = answered(peer_answer(state)).await.unwrap_err();
            assert!(error.contains(state), "{error}");
        }
    }
}
