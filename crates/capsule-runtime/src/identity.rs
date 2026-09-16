use std::sync::{Arc, Mutex};

use murmur_artifact::{ConversationMode, TaskAcceptance};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::a2a::{
    A2aMessage, A2aTask, CancelOutcome, IncomingTask, JsonRpcRequest, JsonRpcResponse,
    TaskRegistry, TaskState, TaskStatus,
};
use crate::cancel::{LiveDelegations, Residue};
use crate::delegation::{COMPLETION_SESSION_HEADER, DELEGATION_ID_HEADER};
use crate::detached::DetachedRegistry;
use crate::errors::RuntimeError;
use crate::origin::{self, TaskOrigin, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
use crate::peer_handoff::{handle_peer_request, is_peer_path, PeerPlane, AUDIENCE_HEADER};
use crate::resource_plane::{
    handle_resource_request, reason_phrase, ResourcePlane, ResourceResponse, RESOURCE_PATH_PREFIX,
};
use crate::streaming::{
    format_gap_event, format_sse_event, is_final_sse_event, ReplayResult, SseBroadcast,
    SseEventBuffer, StreamStatus, TaskStatusUpdateEvent, SSE_HEARTBEAT_COMMENT,
    SSE_HEARTBEAT_INTERVAL,
};
use crate::types::{CapabilityPolicy, InstalledArtifactSummary};

pub(crate) struct CapsuleIdentity {
    pub capsule_name: String,
    pub capsule_version: String,
    pub session_id: String,
    pub capsule_url: String,
}

/// Bind a TCP listener on the given address.
///
/// When `internal_port` is `Some(p)`, binds strictly to `{addr}:{p}` and returns
/// [`RuntimeError::PortInUse`] if that port is already taken.  When `None`, binds
/// to `{addr}:0` and lets the OS assign a port.
///
pub(crate) async fn bind_local_port(
    addr: &str,
    internal_port: Option<u16>,
) -> Result<(TcpListener, u16), RuntimeError> {
    let port = internal_port.unwrap_or(0);
    match TcpListener::bind(format!("{addr}:{port}")).await {
        Ok(listener) => {
            let bound_port = listener
                .local_addr()
                .map_err(|e| RuntimeError::Runtime(format!("failed to read bound port: {e}")))?
                .port();
            Ok((listener, bound_port))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && internal_port.is_some() => {
            Err(RuntimeError::PortInUse {
                port: internal_port.unwrap(),
            })
        }
        Err(e) => Err(RuntimeError::Runtime(format!(
            "failed to bind agent-card port: {e}"
        ))),
    }
}

/// A JSON-RPC method the door at `POST /` answers.
///
/// This is the door's whole method table: dispatch resolves a request's `method` through
/// [`DoorMethod::resolve`], and the agent card's `serves.methods` is [`served_methods`], which
/// asks the same resolver. Adding a method means adding a variant, its wire name and its `ALL`
/// entry; the handler arm is then demanded by the exhaustive matches in the dispatcher, and the
/// card lists it without further change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoorMethod {
    MessageSend,
    MessageStream,
    StreamWatch,
    TasksGet,
    TasksCancel,
    SessionStop,
}

impl DoorMethod {
    /// Every method, in the order the card lists them.
    pub(crate) const ALL: [DoorMethod; 6] = [
        DoorMethod::MessageSend,
        DoorMethod::MessageStream,
        DoorMethod::StreamWatch,
        DoorMethod::TasksGet,
        DoorMethod::TasksCancel,
        DoorMethod::SessionStop,
    ];

    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            DoorMethod::MessageSend => "message/send",
            DoorMethod::MessageStream => "message/stream",
            DoorMethod::StreamWatch => "stream/watch",
            DoorMethod::TasksGet => "tasks/get",
            DoorMethod::TasksCancel => "tasks/cancel",
            DoorMethod::SessionStop => "session/stop",
        }
    }

    /// The method this door answers for a request's `method` string, or `None` when it answers
    /// `-32601`.
    ///
    /// The only place a request's method is interpreted and the only place
    /// `lifecycle.task_acceptance` gates one: under `TaskAcceptance::None` neither task-starting
    /// method is served. The match is exact — no case folding, no trimming — so a name the card
    /// lists is the name to send.
    pub(crate) fn resolve(method: &str, acceptance: &TaskAcceptance) -> Option<DoorMethod> {
        let resolved = Self::ALL.into_iter().find(|m| m.wire_name() == method)?;
        match (resolved, acceptance) {
            (DoorMethod::MessageSend | DoorMethod::MessageStream, TaskAcceptance::None) => None,
            _ => Some(resolved),
        }
    }
}

/// The wire names of every method [`DoorMethod::resolve`] serves under `acceptance`, in `ALL`
/// order.
pub(crate) fn served_methods(acceptance: &TaskAcceptance) -> Vec<&'static str> {
    DoorMethod::ALL
        .into_iter()
        .filter(|m| DoorMethod::resolve(m.wire_name(), acceptance).is_some())
        .map(DoorMethod::wire_name)
        .collect()
}

/// Which HTTP planes the manifest declares. An undeclared plane is still routed, but answers only
/// refusals, so the card lists a plane only when it is declared here.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DeclaredPlanes {
    /// `exports.files` is declared: the operator plane under `/resources/files` serves content.
    pub files: bool,
    /// `exports.peer_files` is declared: the peer plane under `/resources/peer/{handle}` serves
    /// content.
    pub peer_files: bool,
}

/// Build the Agent Card JSON derived from capsule identity and capability policy.
///
/// `session_id` is served alongside the rest because the card is how a caller confirms that the
/// capsule answering an address is the session it went looking for. A session id is already
/// non-secret here — the door names the addressed session when it refuses a completion meant for
/// another one.
///
/// `capabilities` is what this capsule may do; `serves` is what this door answers. `serves.methods`
/// is [`served_methods`] for the acceptance the door is given, so it cannot list a method the
/// dispatcher refuses or omit one it serves, and `capabilities.streaming` is `true` exactly when
/// it contains `message/stream`. `serves.planes` lists `files` then `peer_files`, each only when
/// declared.
pub(crate) fn build_agent_card(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
    task_acceptance: &TaskAcceptance,
    planes: DeclaredPlanes,
) -> serde_json::Value {
    let tools: Vec<&str> = installed_artifacts
        .iter()
        .filter(|a| a.runtime.is_llm_visible())
        .map(|a| a.name.as_str())
        .collect();

    let methods = served_methods(task_acceptance);
    let streaming = methods.contains(&DoorMethod::MessageStream.wire_name());
    let declared_planes: Vec<&str> = [(planes.files, "files"), (planes.peer_files, "peer_files")]
        .into_iter()
        .filter_map(|(declared, name)| declared.then_some(name))
        .collect();

    serde_json::json!({
        "name": identity.capsule_name,
        "version": identity.capsule_version,
        "url": identity.capsule_url,
        "session_id": identity.session_id,
        "capabilities": {
            "tools": tools,
            "shell": !capability_policy.shell_allow.is_empty(),
            "network": !capability_policy.network_allow.is_empty(),
            "streaming": streaming,
        },
        "serves": {
            "methods": methods,
            "planes": declared_planes,
        }
    })
}

/// Serve the agent-card endpoint and A2A JSON-RPC endpoints until shutdown.
///
/// Runs as a tokio task. Each accepted connection is handled in its own spawned task.
/// Shuts down cleanly when shutdown_rx fires or when accept returns an error.
// Everything after the listener and the shutdown channel is A2A server state that is cloned
// once per accepted connection and handed to `handle_connection` unchanged. A wrapper struct
// would name the argument count rather than a concept.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_http(
    listener: TcpListener,
    mut shutdown_rx: oneshot::Receiver<()>,
    card_json: String,
    task_registry: Arc<Mutex<TaskRegistry>>,
    task_tx: mpsc::Sender<IncomingTask>,
    task_acceptance: TaskAcceptance,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode: ConversationMode,
    resource_plane: Arc<ResourcePlane>,
    peer_plane: Arc<PeerPlane>,
    // This capsule's own session id. The door refuses a completion addressed to any other
    // session, which is what stops a child's outcome landing on whatever session answers the
    // parent's old address after a restart.
    session_id: String,
    // The two registries a cancel snapshots its residue from. `detached` is `None` for a launch
    // that demotes nothing, which contributes no shell items rather than an empty set.
    detached: Option<Arc<DetachedRegistry>>,
    live_delegations: Arc<LiveDelegations>,
) {
    let conversation_mode_str = match conversation_mode {
        ConversationMode::Stateless => "stateless",
        ConversationMode::Threaded => "threaded",
    };
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => break,
            result = listener.accept() => {
                match result {
                    Ok((stream, _)) => {
                        let card = card_json.clone();
                        let registry = Arc::clone(&task_registry);
                        let tx = task_tx.clone();
                        let acceptance = task_acceptance.clone();
                        let sse = sse_tx.clone();
                        let buf = Arc::clone(&sse_buffer);
                        let mode_str = conversation_mode_str.to_string();
                        let plane = Arc::clone(&resource_plane);
                        let peer = Arc::clone(&peer_plane);
                        let session = session_id.clone();
                        let detached_for_conn = detached.clone();
                        let live = Arc::clone(&live_delegations);
                        tokio::task::spawn_local(async move {
                            handle_connection(stream, card, registry, tx, acceptance, sse, buf, mode_str, plane, peer, session, detached_for_conn, live).await;
                        });
                    }
                    Err(e) => {
                        eprintln!("[capsule-runtime] HTTP accept error: {e}");
                        break;
                    }
                }
            }
        }
    }
}

// Receives `serve_http`'s state verbatim and splits it across the three request handlers; see
// the note on `serve_http` for why it is not bundled.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: tokio::net::TcpStream,
    card_json: String,
    task_registry: Arc<Mutex<TaskRegistry>>,
    task_tx: mpsc::Sender<IncomingTask>,
    task_acceptance: TaskAcceptance,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode_str: String,
    resource_plane: Arc<ResourcePlane>,
    peer_plane: Arc<PeerPlane>,
    session_id: String,
    detached: Option<Arc<DetachedRegistry>>,
    live_delegations: Arc<LiveDelegations>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Read request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() {
        return;
    }

    let mut parts_iter = request_line.split_whitespace();
    let method = parts_iter.next().unwrap_or("").to_string();
    let path = parts_iter.next().unwrap_or("").to_string();

    // Read all headers
    let mut content_length: usize = 0;
    let mut is_json = false;
    let mut traceparent: Option<String> = None;
    let mut last_event_id: Option<u64> = None;
    let mut audience: Option<String> = None;
    let mut task_origin: Option<String> = None;
    let mut task_trust: Option<String> = None;
    let mut delegation_id: Option<String> = None;
    let mut completion_session: Option<String> = None;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        let lower = lower.trim_end();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        } else if lower.starts_with("content-type:") && lower.contains("application/json") {
            is_json = true;
        } else if let Some(rest) = lower.strip_prefix("traceparent:") {
            traceparent = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix("last-event-id:") {
            last_event_id = rest.trim().parse().ok();
        } else if let Some(rest) = lower.strip_prefix(&format!("{AUDIENCE_HEADER}:")) {
            // Already lowercased with the rest of the line, which is exactly the form an audience
            // takes: both sides build it with `to_lowercase`.
            audience = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{PEER_ORIGIN_HEADER}:")) {
            // Lowercased with the rest of the line, which is the only spelling
            // `origin::from_wire` accepts — a peer that shouts `PEER` is read as `peer`.
            task_origin = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{PEER_TRUST_HEADER}:")) {
            task_trust = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{DELEGATION_ID_HEADER}:")) {
            // Both id spellings are lowercase hex, so lowercasing the whole line loses nothing.
            delegation_id = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{COMPLETION_SESSION_HEADER}:")) {
            completion_session = Some(rest.trim().to_string());
        }
    }

    // Classified once at the door, so both task-starting paths below read the same rule rather
    // than each interpreting the headers for itself.
    let provenance = origin::from_wire(task_origin.as_deref(), task_trust.as_deref());

    // Both delegation headers mean something only on the completion path. On every other path
    // they are ignored rather than carried: a `peer` message claiming a delegation id would put a
    // value on the receiver's `task_start` that no delegation of its own produced.
    let is_completion = provenance.origin() == TaskOrigin::Completion;
    let delegation_id = if is_completion { delegation_id } else { None };

    // Routed ahead of the operator plane on its own segment, and answering every method under it
    // including the ones it refuses: a `PUT` that fell through would leave no record of somebody
    // trying to write, and a peer request that fell through to `/resources/` would be answered by
    // the wrong authoriser.
    if is_peer_path(&path) {
        let response = handle_peer_request(&peer_plane, &method, &path, audience.as_deref()).await;
        let _ = writer_half.write_all(&framed_bytes(&response)).await;
        return;
    }

    // The resource plane is routed on its prefix alone and answers every method under it,
    // including the ones it refuses: a `PUT` that fell through to the bare 404 below would leave
    // no trace record of somebody trying to write.
    if path.starts_with(RESOURCE_PATH_PREFIX) {
        let response = handle_resource_request(&resource_plane, &method, &path).await;
        let _ = writer_half.write_all(&framed_bytes(&response)).await;
        return;
    }

    if method == "GET" && path == "/.well-known/agent-card.json" {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            card_json.len(),
            card_json,
        );
        let _ = writer_half.write_all(response.as_bytes()).await;
        return;
    }

    if method == "POST" && (path == "/" || path.is_empty()) && is_json && content_length > 0 {
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let body_str = String::from_utf8_lossy(&body).to_string();

        // A completion is addressed to one session, and this door answers for one session. An
        // address that has outlived the session that made the delegation — a parent that
        // restarted onto the same port — is refused here rather than delivered to whoever
        // answers now. The refusal names the addressed session and not this one: a caller that
        // guessed wrong learns nothing about who is actually here.
        if is_completion && completion_session.as_deref() != Some(session_id.as_str()) {
            let id = serde_json::from_str::<JsonRpcRequest>(&body_str)
                .map(|req| req.id)
                .unwrap_or(Value::Null);
            let addressed = completion_session.as_deref().unwrap_or("<unaddressed>");
            let response = JsonRpcResponse::err(
                id,
                -32004,
                &format!(
                    "completion is addressed to session {addressed}, which is not the session \
                     running here"
                ),
            )
            .into_http_response();
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        let req = match serde_json::from_str::<JsonRpcRequest>(&body_str) {
            Ok(req) => req,
            Err(_) => {
                let response =
                    JsonRpcResponse::err(Value::Null, -32700, "Parse error").into_http_response();
                let _ = writer_half.write_all(response.as_bytes()).await;
                return;
            }
        };

        // The streaming methods own the connection; every other method answers one JSON body.
        let response = match DoorMethod::resolve(&req.method, &task_acceptance) {
            Some(DoorMethod::MessageStream) => {
                handle_message_stream(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    traceparent,
                    provenance,
                    delegation_id,
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                )
                .await;
                return;
            }
            Some(DoorMethod::StreamWatch) => {
                handle_stream_watch(
                    writer_half,
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                    conversation_mode_str,
                )
                .await;
                return;
            }
            Some(door_method) => handle_jsonrpc(
                door_method,
                req,
                &task_registry,
                &task_tx,
                traceparent,
                provenance,
                delegation_id,
                detached.as_ref(),
                &live_delegations,
                &session_id,
            ),
            None => JsonRpcResponse::err(req.id, -32601, "Method not found").into_http_response(),
        };
        let _ = writer_half.write_all(response.as_bytes()).await;
        return;
    }

    let response =
        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string();
    let _ = writer_half.write_all(response.as_bytes()).await;
}

/// One plane response as bytes on the wire. `connection: close` is appended here rather than by
/// either plane: framing is the transport's business, and both planes already carry their own
/// `content-length`.
fn framed_bytes(response: &ResourceResponse) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason_phrase(response.status)
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&response.body);
    bytes
}

#[allow(clippy::too_many_arguments)]
async fn handle_message_stream(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe to broadcast BEFORE writing headers so we don't miss events
    // emitted between enqueue and the start of our receive loop.
    let mut rx = sse_tx.subscribe();

    // Write SSE response headers immediately
    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    if writer.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    // Replay buffered events for reconnecting clients
    if let Some(last_id) = last_event_id {
        let replay = sse_buffer.lock().unwrap().replay_from(last_id);
        let replay_events = match replay {
            ReplayResult::Complete(events) => events,
            ReplayResult::WithGap {
                first_available_id,
                events,
            } => {
                let gap = format_gap_event(first_available_id);
                if writer.write_all(gap.as_bytes()).await.is_err() {
                    return;
                }
                events
            }
        };
        for event in replay_events {
            if writer.write_all(event.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    // Parse the A2A message from params
    let msg_value = req.params.get("message").unwrap_or(&req.params);
    let message: A2aMessage = match serde_json::from_value(msg_value.clone()) {
        Ok(m) => m,
        Err(e) => {
            let error_data = format!("{{\"error\":\"Invalid params: {e}\"}}");
            let event_text = format!("event: error\ndata: {error_data}\n\n");
            let _ = writer.write_all(event_text.as_bytes()).await;
            return;
        }
    };

    let text = message.extract_text();
    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("ctx_{}", uuid::Uuid::now_v7().simple()));

    // Capacity check and enqueue — release lock before any await
    let accepted = {
        let mut reg = task_registry.lock().unwrap();
        if reg.can_accept() {
            reg.enqueue(&task_id, &context_id);
            true
        } else {
            false
        }
    };
    if !accepted {
        let rejected_event = TaskStatusUpdateEvent {
            id: task_id.clone(),
            context_id: Some(context_id.clone()),
            status: StreamStatus {
                state: "rejected".into(),
                message: "task rejected: capsule is busy".into(),
                response: None,
            },
            r#final: true,
        };
        let data = serde_json::to_string(&rejected_event).unwrap_or_default();
        let event_text = format_sse_event(0, "status", &data);
        let _ = writer.write_all(event_text.as_bytes()).await;
        return;
    }

    // Send to agent loop via mpsc
    let incoming = IncomingTask {
        task_id: task_id.clone(),
        context_id,
        message_id: message.message_id.clone(),
        message_text: text,
        traceparent,
        provenance,
        source: crate::a2a::SOURCE_A2A,
        delegation_id,
    };
    if task_tx.try_send(incoming).is_err() {
        {
            let mut reg = task_registry.lock().unwrap();
            reg.pending_count -= 1;
            reg.history.remove(&task_id);
        } // lock dropped before await
        let event_text =
            "event: error\ndata: {\"error\":\"internal error: queue send failed\"}\n\n";
        let _ = writer.write_all(event_text.as_bytes()).await;
        return;
    }

    // Forward broadcast events to the SSE stream
    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                if writer.write_all(SSE_HEARTBEAT_COMMENT).await.is_err() {
                    return;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event_arc) => {
                        let is_final = is_final_sse_event(&event_arc);
                        if writer.write_all(event_arc.as_bytes()).await.is_err() {
                            return;
                        }
                        if is_final {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[capsule-runtime] SSE broadcast lagged by {n} events");
                        continue;
                    }
                }
            }
        }
    }
}

/// Passive observer handler for `stream/watch`.
///
/// Does not submit a task or call `can_accept()`. Subscribes to the broadcast channel,
/// replays buffered events, then forwards live events until the capsule shuts down or
/// the client disconnects. A `final: true` status event ends one task turn but does NOT
/// close this connection — the observer stays alive across turns.
///
/// While no events flow, writes [`SSE_HEARTBEAT_COMMENT`] every [`SSE_HEARTBEAT_INTERVAL`] so
/// an observer can tell an idle capsule from a dead socket. The handler runs on the session's
/// `LocalSet`, so the heartbeat only fires when that thread reaches an await point: synchronous
/// work inside a turn stalls it until the thread is released.
async fn handle_stream_watch(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode_str: String,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before replaying so we don't miss events emitted between replay and loop start.
    let mut rx = sse_tx.subscribe();

    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    if writer.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    // Emit connection-ack so observers know the capsule's conversation mode without reading manifest.
    let ack = format!(
        "event: connection-ack\ndata: {{\"role\":\"observer\",\"conversation_mode\":\"{conversation_mode_str}\"}}\n\n"
    );
    if writer.write_all(ack.as_bytes()).await.is_err() {
        return;
    }

    let last_id = last_event_id.unwrap_or(0);
    let replay = sse_buffer.lock().unwrap().replay_from(last_id);
    let replay_events = match replay {
        ReplayResult::Complete(events) => events,
        ReplayResult::WithGap {
            first_available_id,
            events,
        } => {
            let gap = format_gap_event(first_available_id);
            if writer.write_all(gap.as_bytes()).await.is_err() {
                return;
            }
            events
        }
    };
    for event in replay_events {
        if writer.write_all(event.as_bytes()).await.is_err() {
            return;
        }
    }

    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                // A failed write means the observer is gone; there is nothing left to serve.
                if writer.write_all(SSE_HEARTBEAT_COMMENT).await.is_err() {
                    return;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event_arc) => {
                        if writer.write_all(event_arc.as_bytes()).await.is_err() {
                            return;
                        }
                        // Do NOT exit on is_final_sse_event — final ends one task turn, not the capsule.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let _ = writer
                            .write_all(b"event: capsule-closed\ndata: {}\n\n")
                            .await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("[capsule-runtime] stream/watch: SSE broadcast lagged by {n} events");
                        continue;
                    }
                }
            }
        }
    }
}

/// Answers a resolved method that replies with one JSON-RPC body.
#[allow(clippy::too_many_arguments)]
fn handle_jsonrpc(
    method: DoorMethod,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let id = req.id;
    match method {
        DoorMethod::MessageSend => handle_message_send(
            id,
            &req.params,
            task_registry,
            task_tx,
            traceparent,
            provenance,
            delegation_id,
        ),
        DoorMethod::TasksGet => handle_tasks_get(id, &req.params, task_registry),
        DoorMethod::TasksCancel => {
            handle_tasks_cancel(id, &req.params, task_registry, detached, live_delegations)
        }
        DoorMethod::SessionStop => {
            handle_session_stop(id, task_registry, detached, live_delegations, session_id)
        }
        DoorMethod::MessageStream | DoorMethod::StreamWatch => {
            unreachable!("handle_connection answers the streaming methods before this point")
        }
    }
}

fn handle_message_send(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
) -> String {
    let msg_value = params.get("message").unwrap_or(params);
    let message: A2aMessage = match serde_json::from_value(msg_value.clone()) {
        Ok(m) => m,
        Err(e) => {
            return JsonRpcResponse::err(id, -32602, &format!("Invalid params: {e}"))
                .into_http_response();
        }
    };

    let text = message.extract_text();

    // Check if the active task is waiting for input — deliver to it instead of enqueuing.
    {
        let mut reg = task_registry.lock().unwrap();
        if let Some(active_task_id) = reg.active_input_required_task_id() {
            if reg.deliver_input(&active_task_id, text.clone()).is_ok() {
                if let Some(task) = reg.get_task(&active_task_id) {
                    return JsonRpcResponse::ok(id, task).into_http_response();
                }
            }
        }
    }

    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("ctx_{}", uuid::Uuid::now_v7().simple()));

    // Capacity check and enqueue under lock
    {
        let mut reg = task_registry.lock().unwrap();
        if !reg.can_accept() {
            let task = A2aTask {
                id: task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Rejected,
                },
                artifacts: None,
            };
            return JsonRpcResponse::ok(id, task).into_http_response();
        }
        reg.enqueue(&task_id, &context_id);
    }

    // Send to mpsc (should always succeed — capacity was checked under the same lock)
    let incoming = IncomingTask {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        message_id: message.message_id.clone(),
        message_text: text,
        traceparent,
        provenance,
        source: crate::a2a::SOURCE_A2A,
        delegation_id,
    };
    if task_tx.try_send(incoming).is_err() {
        // Unexpected path — roll back pending count
        let mut reg = task_registry.lock().unwrap();
        reg.pending_count -= 1;
        reg.history.remove(&task_id);
        return JsonRpcResponse::err(id, -32603, "internal error: queue send failed")
            .into_http_response();
    }

    let task = A2aTask {
        id: task_id,
        context_id,
        status: TaskStatus {
            state: TaskState::Submitted,
        },
        artifacts: None,
    };
    JsonRpcResponse::ok(id, task).into_http_response()
}

fn handle_tasks_get(id: Value, params: &Value, task_registry: &Arc<Mutex<TaskRegistry>>) -> String {
    let requested_id = params.get("id").and_then(Value::as_str).map(str::to_string);

    let Some(task_id) = requested_id else {
        // Backward compat: if no id provided, return the active slot's task if any
        let reg = task_registry.lock().unwrap();
        return match &reg.active_slot {
            crate::a2a::TaskSlotState::Empty => {
                JsonRpcResponse::err(id, -32001, "Task not found").into_http_response()
            }
            _ => {
                // Use get_task on the active slot's task_id
                let active_id = match &reg.active_slot {
                    crate::a2a::TaskSlotState::Running { task_id, .. }
                    | crate::a2a::TaskSlotState::Done { task_id, .. } => task_id.clone(),
                    crate::a2a::TaskSlotState::Empty => unreachable!(),
                };
                match reg.get_task(&active_id) {
                    Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
                    None => JsonRpcResponse::err(id, -32001, "Task not found").into_http_response(),
                }
            }
        };
    };

    let reg = task_registry.lock().unwrap();
    match reg.get_task(&task_id) {
        Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
        None => JsonRpcResponse::err(id, -32001, "Task not found").into_http_response(),
    }
}

/// `tasks/cancel`: stop one task and report what is still running.
///
/// Runs on the connection task and never waits for the agent loop: it takes the registry lock,
/// records the cancellation, snapshots the two work registries and answers. Whether the loop has
/// noticed yet is not the caller's question — the recorded state is already `canceled`.
///
/// Every outcome but one is a JSON-RPC `result`. A task that had already ended is returned
/// unchanged, because "do no more work on this" is already true of it. Only an id this capsule
/// never held is an error, and it is the same `-32001` `tasks/get` answers with.
fn handle_tasks_cancel(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
) -> String {
    let Some(task_id) = params.get("id").and_then(Value::as_str).map(str::to_string) else {
        return JsonRpcResponse::err(id, -32602, "Invalid params: tasks/cancel requires an id")
            .into_http_response();
    };

    let (outcome, task) = {
        let mut reg = task_registry.lock().unwrap();
        let outcome = reg.request_cancel(&task_id);
        (outcome, reg.get_task(&task_id))
    };

    match (outcome, task) {
        (CancelOutcome::Unknown, _) | (_, None) => {
            JsonRpcResponse::err(id, -32001, "Task not found").into_http_response()
        }
        (CancelOutcome::AlreadyTerminal, Some(task)) => {
            JsonRpcResponse::ok(id, task).into_http_response()
        }
        (CancelOutcome::Accepted, Some(mut task)) => {
            // Read after the state is recorded, so nothing this snapshot names can have been
            // started by the cancelled task afterwards.
            task.artifacts = Residue::snapshot(detached, live_delegations)
                .into_artifact()
                .map(|artifact| vec![artifact]);
            JsonRpcResponse::ok(id, task).into_http_response()
        }
    }
}

/// `session/stop`: cancel everything this session still holds and report what it leaves running.
///
/// The one moment anything can ask a capsule for that account. A detached shell command keeps its
/// own lifecycle and a delegated child is still going, and once the process is gone so is the
/// registry that knew about either — so the question is asked while the door is still up, and
/// ending the process is left to the signal that follows.
///
/// Nothing here shuts the door down. A method that killed its own process would be racing its
/// response out of it.
///
/// Three keys, all three always present. `canceled` lists only the tasks this call moved to
/// `canceled`, so a second stop answers `[]` rather than an error. `residue` is `[]` when nothing
/// is running — unlike `tasks/cancel`, which omits the key: a session stop has to be able to say
/// "nothing" as a positive fact, because that is the whole answer the operator asked for.
fn handle_session_stop(
    id: Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let canceled = task_registry.lock().unwrap().cancel_every_live();

    // Read after every cancellation is recorded, so nothing this snapshot names can have been
    // started by a task afterwards.
    let residue = Residue::snapshot(detached, live_delegations).into_json_items();

    JsonRpcResponse::ok(
        id,
        serde_json::json!({
            "session_id": session_id,
            "canceled": canceled,
            "residue": residue,
        }),
    )
    .into_http_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::RuntimeError;

    const ACCEPTANCES: [TaskAcceptance; 3] = [
        TaskAcceptance::None,
        TaskAcceptance::Single,
        TaskAcceptance::Queue,
    ];

    fn card_for(acceptance: &TaskAcceptance, planes: DeclaredPlanes) -> Value {
        let identity = CapsuleIdentity {
            capsule_name: "probe".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_probe".to_string(),
            capsule_url: "http://127.0.0.1:1".to_string(),
        };
        build_agent_card(
            &identity,
            &[],
            &CapabilityPolicy::default(),
            acceptance,
            planes,
        )
    }

    #[test]
    fn door_method_served_methods_are_exactly_what_resolve_serves() {
        for acceptance in &ACCEPTANCES {
            let served = served_methods(acceptance);
            for method in DoorMethod::ALL {
                let resolved = DoorMethod::resolve(method.wire_name(), acceptance);
                assert_eq!(
                    served.contains(&method.wire_name()),
                    resolved.is_some(),
                    "{} under {acceptance:?}",
                    method.wire_name()
                );
                if let Some(resolved) = resolved {
                    assert_eq!(resolved, method, "{acceptance:?}");
                }
            }
            let listed_in_all_order: Vec<&str> = DoorMethod::ALL
                .into_iter()
                .map(DoorMethod::wire_name)
                .filter(|name| served.contains(name))
                .collect();
            assert_eq!(served, listed_in_all_order, "{acceptance:?}");
        }
    }

    #[test]
    fn door_method_acceptance_none_serves_no_task_starting_method() {
        assert_eq!(
            served_methods(&TaskAcceptance::None),
            ["stream/watch", "tasks/get", "tasks/cancel", "session/stop"]
        );
        for acceptance in [TaskAcceptance::Single, TaskAcceptance::Queue] {
            assert_eq!(
                served_methods(&acceptance),
                [
                    "message/send",
                    "message/stream",
                    "stream/watch",
                    "tasks/get",
                    "tasks/cancel",
                    "session/stop"
                ]
            );
        }
    }

    #[test]
    fn door_method_resolve_matches_names_exactly() {
        for acceptance in &ACCEPTANCES {
            for name in [
                "tasks/list",
                "",
                "Tasks/Get",
                "TASKS/GET",
                " tasks/get",
                "tasks/get ",
                "tasks/get\n",
                "session/stop/",
            ] {
                assert_eq!(
                    DoorMethod::resolve(name, acceptance),
                    None,
                    "{name:?} under {acceptance:?}"
                );
            }
        }
    }

    #[test]
    fn door_method_card_advertises_what_the_resolver_serves() {
        for acceptance in &ACCEPTANCES {
            let card = card_for(acceptance, DeclaredPlanes::default());
            let methods: Vec<&str> = card["serves"]["methods"]
                .as_array()
                .expect("serves.methods is an array")
                .iter()
                .map(|m| m.as_str().expect("each method is a string"))
                .collect();
            assert_eq!(methods, served_methods(acceptance), "{acceptance:?}");
            assert_eq!(
                card["capabilities"]["streaming"],
                methods.contains(&"message/stream"),
                "{acceptance:?}"
            );
        }
    }

    #[test]
    fn door_method_card_lists_declared_planes_in_order() {
        let cases = [
            (false, false, serde_json::json!([])),
            (true, false, serde_json::json!(["files"])),
            (false, true, serde_json::json!(["peer_files"])),
            (true, true, serde_json::json!(["files", "peer_files"])),
        ];
        for (files, peer_files, expected) in cases {
            let card = card_for(
                &TaskAcceptance::Single,
                DeclaredPlanes { files, peer_files },
            );
            assert_eq!(card["serves"]["planes"], expected);
        }
    }

    #[tokio::test]
    async fn bind_local_port_os_assigned_when_none() {
        let (listener, port) = bind_local_port("127.0.0.1", None).await.unwrap();
        assert!(port > 0);
        // Listener is bound — a second bind on the same port should fail.
        let err = bind_local_port("127.0.0.1", Some(port)).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PortInUse { port: p } if p == port),
            "expected PortInUse, got {err}"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn bind_local_port_uses_specified_port() {
        // Grab a free port from the OS, release it, then explicitly request it.
        let port = {
            let (l, p) = bind_local_port("127.0.0.1", None).await.unwrap();
            drop(l);
            p
        };
        let (_, bound_port) = bind_local_port("127.0.0.1", Some(port)).await.unwrap();
        assert_eq!(bound_port, port);
    }

    #[tokio::test]
    async fn bind_local_port_returns_port_in_use_when_taken() {
        let (_listener, port) = bind_local_port("127.0.0.1", None).await.unwrap();
        // _listener is still alive — the port is occupied.
        let err = bind_local_port("127.0.0.1", Some(port)).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PortInUse { port: p } if p == port),
            "expected PortInUse {{ port: {port} }}, got {err}"
        );
    }

    /// The heartbeat shares the session's `LocalSet` with the agent loop, so it can only fire
    /// when that thread reaches an await point. This reproduces that shape — a `spawn_local`ed
    /// `handle_stream_watch` alongside a `spawn_local`ed task that holds the thread with
    /// `std::thread::sleep` across a due tick — and asserts what a client actually reads:
    /// nothing while the thread is held, and a heartbeat once it is released.
    ///
    /// Moving the door off the session thread is a separate concern; this test records the
    /// current behaviour rather than a desired one.
    #[tokio::test]
    async fn stream_watch_heartbeat_stalls_while_session_thread_is_blocked() {
        use std::io::{BufRead, BufReader};
        use std::time::{Duration, Instant};

        // Longer than one heartbeat interval, so a tick falls due inside the blocked stretch.
        const BLOCK: Duration = Duration::from_secs(20);
        // How long to keep reading after the thread is released, to see whether ticks resume.
        const OBSERVE_AFTER_RELEASE: Duration = Duration::from_secs(8);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Read the client side on its own OS thread: the runtime thread is about to be
        // unavailable, which is the whole point.
        let (line_tx, line_rx) = std::sync::mpsc::channel::<(Instant, String)>();
        let client = std::thread::spawn(move || {
            let sock = std::net::TcpStream::connect(addr).unwrap();
            sock.set_read_timeout(Some(
                BLOCK + OBSERVE_AFTER_RELEASE + Duration::from_secs(10),
            ))
            .unwrap();
            let mut reader = BufReader::new(sock);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line_tx.send((Instant::now(), line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(16);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let local = tokio::task::LocalSet::new();
        let (connected_at, blocked_at) = local
            .run_until(async {
                let (sock, _peer) = listener.accept().await.unwrap();
                let connected_at = Instant::now();
                // Hold the read half so the socket stays open for the whole measurement.
                let (_read_half, write_half) = sock.into_split();
                tokio::task::spawn_local(handle_stream_watch(
                    write_half,
                    Some(0),
                    sse_tx.clone(),
                    Arc::clone(&sse_buffer),
                    "stateless".to_string(),
                ));

                // Let the handler write its preamble and settle into the receive loop before
                // the thread is taken away from it.
                tokio::time::sleep(Duration::from_millis(250)).await;

                let blocked_at = Instant::now();
                tokio::task::spawn_local(async { std::thread::sleep(BLOCK) })
                    .await
                    .unwrap();

                tokio::time::sleep(OBSERVE_AFTER_RELEASE).await;
                (connected_at, blocked_at)
            })
            .await;

        // Dropping the LocalSet drops the handler, closing the socket and ending the reader.
        drop(local);
        drop(sse_tx);
        client.join().unwrap();

        let released_at = blocked_at + BLOCK;
        let heartbeats: Vec<Instant> = line_rx
            .try_iter()
            .filter(|(_, line)| line.trim_end() == ":heartbeat")
            .map(|(at, _)| at)
            .collect();

        let arrivals: Vec<Duration> = heartbeats
            .iter()
            .map(|at| at.duration_since(connected_at))
            .collect();
        // Reported so the measurement can be re-read rather than re-derived.
        eprintln!(
            "[heartbeat] thread held from {:?} to {:?} after connect; heartbeats arrived at {arrivals:?}",
            blocked_at.duration_since(connected_at),
            released_at.duration_since(connected_at)
        );
        assert!(
            !heartbeats.is_empty(),
            "heartbeat never resumed after the session thread was released; arrivals: {arrivals:?}"
        );

        let first = heartbeats[0];
        assert!(
            first + Duration::from_millis(100) >= released_at,
            "a heartbeat arrived while the session thread was blocked: first at {:?} after \
             connect, thread released at {:?} after connect",
            first.duration_since(connected_at),
            released_at.duration_since(connected_at)
        );
        assert!(
            first.duration_since(connected_at) > SSE_HEARTBEAT_INTERVAL,
            "the tick due at {SSE_HEARTBEAT_INTERVAL:?} was expected to be stalled past its \
             deadline, but the first heartbeat arrived at {:?}",
            first.duration_since(connected_at)
        );
    }
}
