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
    admit_live_frame, format_gap_event, format_lagged_event, format_unnumbered_sse_event, frame_id,
    is_final_sse_event, ReplayResult, SseBroadcast, SseEventBuffer, StreamStatus,
    TaskStatusUpdateEvent, SSE_HEARTBEAT_COMMENT, SSE_HEARTBEAT_INTERVAL,
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
/// Spawned onto the session runtime's worker pool, and each accepted connection is handled in
/// its own task on that pool, so the door answers while the thread running the agent loop is
/// held by synchronous work inside a turn. No handler waits on the agent loop.
///
/// Returns once `shutdown_rx` fires or its sender is dropped, having closed the listener and
/// ended every connection still open, so the door closes with the session rather than with the
/// runtime. An accept error closes the listener but leaves open connections served until then.
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
    let mut connections = tokio::task::JoinSet::new();
    let accept_failed = loop {
        tokio::select! {
            _ = &mut shutdown_rx => break false,
            // Reaps finished connections, which a `JoinSet` otherwise keeps until joined.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
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
                        connections.spawn(async move {
                            handle_connection(stream, card, registry, tx, acceptance, sse, buf, mode_str, plane, peer, session, detached_for_conn, live).await;
                        });
                    }
                    Err(e) => {
                        crate::runtime_err!("[capsule-runtime] HTTP accept error: {e}");
                        break true;
                    }
                }
            }
        }
    };
    drop(listener);
    if accept_failed {
        let _ = shutdown_rx.await;
    }
    // A `stream/watch` or `message/stream` connection otherwise outlives the session: its
    // handler holds a broadcast sender, so it never sees the channel close.
    connections.shutdown().await;
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

    // Replay buffered events for reconnecting clients. The highest id written is where the live
    // loop picks up, so a frame both replayed and received live is written once.
    let mut last_written = None;
    if let Some(last_id) = last_event_id {
        match write_replay(&mut writer, &sse_buffer, last_id).await {
            Ok(written) => last_written = written,
            Err(()) => return,
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
        let _ = writer
            .write_all(format_rejected_event(&rejected_event).as_bytes())
            .await;
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
                        if !admit_live_frame(&mut last_written, &event_arc) {
                            continue;
                        }
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
                        crate::runtime_err!("[capsule-runtime] SSE broadcast lagged by {n} events");
                        if writer
                            .write_all(format_lagged_event(n).as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                }
            }
        }
    }
}

/// Write the replay of the frames after `last_id` — preceded by a `gap` frame when the buffer
/// cannot supply all of them — and return the highest id written, or `None` when the replay was
/// empty. `Err` means the client is gone.
async fn write_replay(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    sse_buffer: &Mutex<SseEventBuffer>,
    last_id: u64,
) -> Result<Option<u64>, ()> {
    use tokio::io::AsyncWriteExt;

    let replay = sse_buffer.lock().unwrap().replay_from(last_id);
    let events = match replay {
        ReplayResult::Complete(events) => events,
        ReplayResult::WithGap {
            first_available_id,
            events,
        } => {
            let gap = format_gap_event(first_available_id);
            writer.write_all(gap.as_bytes()).await.map_err(|_| ())?;
            events
        }
    };
    let mut last_written = None;
    for event in events {
        writer.write_all(event.as_bytes()).await.map_err(|_| ())?;
        last_written = frame_id(&event).or(last_written);
    }
    Ok(last_written)
}

/// The `rejected` status written to a `message/stream` connection the capsule is too busy to
/// accept. It goes to that one connection and is never buffered, so it carries no `id:` line:
/// it has no place in the session's sequence, and an id would move a client's resume cursor.
fn format_rejected_event(event: &TaskStatusUpdateEvent) -> String {
    let data = serde_json::to_string(event).unwrap_or_default();
    format_unnumbered_sse_event("status", &data)
}

/// Passive observer handler for `stream/watch`.
///
/// Does not submit a task or call `can_accept()`. Subscribes to the broadcast channel,
/// replays buffered events, then forwards live events until the capsule shuts down or
/// the client disconnects. A `final: true` status event ends one task turn but does NOT
/// close this connection — the observer stays alive across turns.
///
/// Writes [`SSE_HEARTBEAT_COMMENT`] every [`SSE_HEARTBEAT_INTERVAL`], whether or not events
/// flowed in between, so an observer can tell an idle capsule from a dead socket. The handler
/// runs on the runtime's worker pool, off the thread running the agent loop, so synchronous work
/// inside a turn does not delay it.
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

    let Ok(mut last_written) =
        write_replay(&mut writer, &sse_buffer, last_event_id.unwrap_or(0)).await
    else {
        return;
    };

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
                        if !admit_live_frame(&mut last_written, &event_arc) {
                            continue;
                        }
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
                        crate::runtime_err!("[capsule-runtime] stream/watch: SSE broadcast lagged by {n} events");
                        if writer
                            .write_all(format_lagged_event(n).as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
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
#[allow(clippy::print_stdout, clippy::print_stderr)]
mod tests {
    use super::*;
    use crate::errors::RuntimeError;
    use crate::streaming::format_sse_event;

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
    fn rejected_status_carries_no_event_id() {
        let frame = format_rejected_event(&TaskStatusUpdateEvent {
            id: "tsk_1".into(),
            context_id: Some("ctx_1".into()),
            status: StreamStatus {
                state: "rejected".into(),
                message: "task rejected: capsule is busy".into(),
                response: None,
            },
            r#final: true,
        });
        assert!(frame.starts_with("event: status\ndata: {"), "{frame}");
        assert!(!frame.contains("id: "), "{frame}");
        assert!(frame.contains(r#""state":"rejected""#), "{frame}");
        assert!(frame.ends_with("\n\n"), "{frame}");
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

    /// The door runs on the runtime's worker pool while the agent loop holds its own thread. This
    /// reproduces that shape — `handle_stream_watch` spawned onto the pool, as `serve_http` spawns
    /// every connection, beside a `LocalSet` task that holds the calling thread with
    /// `std::thread::sleep` across a due tick — and asserts what a client actually reads: a
    /// heartbeat while the thread is still held.
    ///
    /// `protocol_page_says_the_heartbeat_keeps_its_cadence_during_a_turn` holds the protocol page to
    /// this behaviour and must change with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_watch_heartbeat_continues_while_session_thread_is_blocked() {
        use std::time::{Duration, Instant};

        // Longer than one heartbeat interval, so a tick falls due inside the blocked stretch.
        const BLOCK: Duration = Duration::from_secs(20);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Read on its own OS thread, so what the client records does not depend on the thread
        // about to be held.
        let (client, line_rx) = spawn_sse_line_reader(addr, BLOCK + Duration::from_secs(10));

        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(16);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let local = tokio::task::LocalSet::new();
        let (connected_at, blocked_at, watcher) = local
            .run_until(async {
                let (sock, _peer) = listener.accept().await.unwrap();
                let connected_at = Instant::now();
                // Hold the read half so the socket stays open for the whole measurement.
                let (read_half, write_half) = sock.into_split();
                let sse = sse_tx.clone();
                let buffer = Arc::clone(&sse_buffer);
                let watcher = tokio::spawn(async move {
                    let _read_half = read_half;
                    handle_stream_watch(write_half, Some(0), sse, buffer, "stateless".to_string())
                        .await;
                });

                // Let the handler write its preamble and settle into the receive loop before
                // the thread is held.
                tokio::time::sleep(Duration::from_millis(250)).await;

                let blocked_at = Instant::now();
                tokio::task::spawn_local(async { std::thread::sleep(BLOCK) })
                    .await
                    .unwrap();
                (connected_at, blocked_at, watcher)
            })
            .await;

        // Ending the handler closes the socket, which ends the reader.
        watcher.abort();
        let _ = watcher.await;
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
        let Some(&first) = heartbeats.first() else {
            panic!("no heartbeat arrived while the session thread was blocked");
        };
        assert!(
            first < released_at,
            "the first heartbeat waited for the session thread: it arrived at {:?} after \
             connect, and the thread was released at {:?} after connect",
            first.duration_since(connected_at),
            released_at.duration_since(connected_at)
        );
        assert!(
            first.duration_since(connected_at) < SSE_HEARTBEAT_INTERVAL + Duration::from_secs(2),
            "the tick due at {SSE_HEARTBEAT_INTERVAL:?} was late: the first heartbeat arrived at \
             {:?} after connect",
            first.duration_since(connected_at)
        );
    }

    /// Reads an SSE connection on its own OS thread, forwarding each line with the moment it
    /// arrived. The sender drops when the server closes the socket or a read times out.
    fn spawn_sse_line_reader(
        addr: std::net::SocketAddr,
        read_timeout: std::time::Duration,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<(std::time::Instant, String)>,
    ) {
        use std::io::{BufRead, BufReader};

        let (line_tx, line_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let sock = std::net::TcpStream::connect(addr).unwrap();
            sock.set_read_timeout(Some(read_timeout)).unwrap();
            let mut reader = BufReader::new(sock);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line_tx.send((std::time::Instant::now(), line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (client, line_rx)
    }

    /// Appends received lines to `received` until it contains `needle`, or, with `None`, until the
    /// server closes the connection. Sleeps between polls so the handler under test can run.
    async fn collect_sse_lines(
        lines: &std::sync::mpsc::Receiver<(std::time::Instant, String)>,
        received: &mut String,
        needle: Option<&str>,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            loop {
                match lines.try_recv() {
                    Ok((_, line)) => received.push_str(&line),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        assert!(
                            needle.is_none_or(|n| received.contains(n)),
                            "the connection closed before {needle:?} arrived; received:\n{received}"
                        );
                        return;
                    }
                }
            }
            if needle.is_some_and(|n| received.contains(n)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {needle:?}; received:\n{received}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Waits until the handler under test has subscribed, so it is the channel's only receiver.
    async fn wait_for_subscriber(sse_tx: &SseBroadcast) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sse_tx.receiver_count() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the handler never subscribed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The body of an HTTP response: everything after the header block.
    fn sse_body(received: &str) -> &str {
        let at = received
            .find("\r\n\r\n")
            .unwrap_or_else(|| panic!("no header terminator in:\n{received}"));
        &received[at + 4..]
    }

    /// Capacity of the broadcast channel in the lag tests; each handler's queue holds this many.
    const LAG_QUEUE: usize = 4;
    /// Frames beyond [`LAG_QUEUE`] in the burst, and so the count the `lagged` frame must carry.
    const LAG_MISSED: usize = 3;

    /// The burst sent to a lagging connection: `LAG_QUEUE + LAG_MISSED` non-final frames with ids
    /// from 1.
    fn lag_burst() -> Vec<String> {
        (1..=(LAG_QUEUE + LAG_MISSED) as u64)
            .map(|id| format_sse_event(id, "text", &format!("{{\"n\":{id},\"final\":false}}")))
            .collect()
    }

    fn assert_single_lagged_frame(body: &str) {
        let expected = format!("event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n");
        assert_eq!(
            body.matches("event: lagged").count(),
            1,
            "expected exactly one lagged frame in:\n{body}"
        );
        assert!(
            body.contains(&expected),
            "the lagged frame is not byte-exact `{expected:?}` in:\n{body}"
        );
        let before = &body[..body.find(&expected).unwrap()];
        assert!(
            before.is_empty() || before.ends_with("\n\n"),
            "the lagged frame does not start its own block, so it carries an id line:\n{body}"
        );
    }

    fn assert_buffer_has_no_lagged_frame(sse_buffer: &Arc<Mutex<SseEventBuffer>>) {
        let events = match sse_buffer.lock().unwrap().replay_from(0) {
            ReplayResult::Complete(events) => events,
            ReplayResult::WithGap { events, .. } => events,
        };
        assert!(
            events.iter().all(|e| !e.contains("event: lagged")),
            "a lagged frame entered the replay buffer"
        );
    }

    /// A `stream/watch` connection that falls behind is told how many live frames it lost, in one
    /// id-less `lagged` frame written before the oldest retained frame, and stays open.
    ///
    /// Current-thread flavour: the burst is sent with no await between sends, so the handler cannot
    /// drain its queue part-way through and the lost count is exact.
    #[tokio::test]
    async fn stream_watch_writes_lagged_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_stream_watch(write_half, Some(0), sse, buffer, "stateless".to_string()).await;
        });
        wait_for_subscriber(&sse_tx).await;

        let burst = lag_burst();
        for frame in &burst {
            sse_tx.send(Arc::new(frame.clone())).unwrap();
        }
        let retained = &burst[LAG_MISSED..];
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some(retained.last().unwrap())).await;

        let last = format_sse_event(100, "text", "{\"n\":\"last\",\"final\":false}");
        sse_tx.send(Arc::new(last.clone())).unwrap();
        collect_sse_lines(&lines, &mut received, Some(&last)).await;

        watcher.abort();
        let _ = watcher.await;
        drop(sse_tx);
        client.join().unwrap();

        let body = sse_body(&received);
        assert_single_lagged_frame(body);
        let expected = format!(
            "event: connection-ack\ndata: {{\"role\":\"observer\",\"conversation_mode\":\"stateless\"}}\n\n\
             event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n{}{last}",
            retained.concat()
        );
        assert_eq!(body, expected);
        assert_buffer_has_no_lagged_frame(&sse_buffer);
    }

    /// A `message/stream` connection that falls behind is told how many live frames it lost, keeps
    /// receiving, and still closes on the first live `final` status it is delivered.
    ///
    /// Current-thread flavour for the same reason as `stream_watch_writes_lagged_frame`.
    #[tokio::test]
    async fn message_stream_writes_lagged_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "message/stream".to_string(),
            params: serde_json::json!({
                "message": {
                    "messageId": "msg_lagged",
                    "role": "user",
                    "parts": [{"text": "hello"}]
                }
            }),
        };

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let handler = tokio::spawn(async move {
            let _read_half = read_half;
            handle_message_stream(
                write_half,
                req,
                &task_registry,
                &task_tx,
                None,
                TaskProvenance::derive(TaskOrigin::User, None),
                None,
                None,
                sse,
                buffer,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        let burst = lag_burst();
        for frame in &burst {
            sse_tx.send(Arc::new(frame.clone())).unwrap();
        }
        let retained = &burst[LAG_MISSED..];
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some(retained.last().unwrap())).await;

        let final_status =
            format_sse_event(100, "status", "{\"id\":\"tsk_lagged\",\"final\":true}");
        sse_tx.send(Arc::new(final_status.clone())).unwrap();
        collect_sse_lines(&lines, &mut received, None).await;

        handler.await.unwrap();
        client.join().unwrap();
        assert!(task_rx.try_recv().is_ok(), "the task was not submitted");

        let body = sse_body(&received);
        assert_single_lagged_frame(body);
        let expected = format!(
            "event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n{}{final_status}",
            retained.concat()
        );
        assert_eq!(body, expected);
        assert_buffer_has_no_lagged_frame(&sse_buffer);
    }

    const PROTOCOL_PAGE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/content/reference/streaming-protocol.md"
    );

    fn protocol_page() -> String {
        std::fs::read_to_string(PROTOCOL_PAGE_PATH)
            .unwrap_or_else(|e| panic!("cannot read {PROTOCOL_PAGE_PATH}: {e}"))
    }

    /// The section of the protocol page whose heading carries `anchor`, from the start of that
    /// heading line up to the next line consisting only of `---`.
    fn protocol_page_section<'a>(page: &'a str, anchor: &str) -> &'a str {
        let marker = format!("{{ #{anchor} }}");
        let at = page
            .find(&marker)
            .unwrap_or_else(|| panic!("{PROTOCOL_PAGE_PATH} has no heading with anchor {marker}"));
        let start = page[..at].rfind('\n').map_or(0, |i| i + 1);
        let len = page[start..].find("\n---\n").unwrap_or_else(|| {
            panic!("the {marker} section of {PROTOCOL_PAGE_PATH} has no closing `---` line")
        });
        &page[start..start + len]
    }

    /// Every number written immediately before the word `seconds`, in order.
    fn numbers_before_seconds(text: &str) -> Vec<u64> {
        let words: Vec<&str> = text.split_whitespace().collect();
        words
            .windows(2)
            .filter(|pair| {
                pair[1]
                    .trim_end_matches(|c: char| !c.is_alphanumeric())
                    .eq("seconds")
            })
            .filter_map(|pair| {
                pair[0]
                    .trim_start_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok()
            })
            .collect()
    }

    /// The Heartbeat section states `SSE_HEARTBEAT_INTERVAL` in its Interval row, and every
    /// duration in seconds anywhere in the section is that interval.
    #[test]
    fn protocol_page_heartbeat_interval_is_the_runtime_interval() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "heartbeat");
        let interval = SSE_HEARTBEAT_INTERVAL.as_secs();
        let row = format!("| Interval | {interval} seconds");
        assert!(
            section.contains(&row),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} has no `{row}` row; \
             SSE_HEARTBEAT_INTERVAL is {interval} seconds"
        );
        let stated = numbers_before_seconds(section);
        let wrong: Vec<u64> = stated.iter().copied().filter(|&n| n != interval).collect();
        assert!(
            wrong.is_empty(),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} states {wrong:?} \
             seconds; SSE_HEARTBEAT_INTERVAL is {interval} seconds"
        );
    }

    /// The Frames-a-connection-misses section states `SSE_BROADCAST_CAPACITY` as the size of each
    /// connection's live queue.
    #[test]
    fn protocol_page_broadcast_queue_is_the_runtime_capacity() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "lagged");
        let capacity = crate::runtime::SSE_BROADCAST_CAPACITY;
        for phrase in [
            format!("queue of {capacity}"),
            format!("more than {capacity}"),
        ] {
            assert!(
                section.contains(&phrase),
                "the Frames a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} \
                 does not say `{phrase}`; SSE_BROADCAST_CAPACITY is {capacity}"
            );
        }
    }

    /// The page documents the frame `stream_watch_writes_lagged_frame` and
    /// `message_stream_writes_lagged_frame` hold the handlers to: its key, its place in the
    /// endpoint and event-id tables, and its absence from replay.
    #[test]
    fn protocol_page_documents_the_lagged_frame() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "event-lagged");
        for required in ["`missed`", "{\"missed\":"] {
            assert!(
                section.contains(required),
                "the `lagged` section ({{ #event-lagged }}) of {PROTOCOL_PAGE_PATH} does not \
                 contain `{required}`"
            );
        }
        for row in [
            "| [`lagged`](#event-lagged) | yes | yes |",
            "| `lagged` | no | — |",
        ] {
            assert!(
                page.contains(row),
                "{PROTOCOL_PAGE_PATH} has no `{row}` row"
            );
        }
        assert!(
            protocol_page_section(&page, "replay").contains("`lagged`"),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not list `lagged`"
        );
        assert!(
            !protocol_page_section(&page, "lagged").contains("Nothing is written in their place"),
            "the Frames a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} says \
             nothing is written for lost frames"
        );
    }

    /// The Event ids section states the id a session's first frame takes, and that a replay from
    /// `0` of an unevicted buffer starts there.
    #[test]
    fn protocol_page_first_event_id_is_the_runtimes() {
        let page = protocol_page();
        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        let buffer = std::sync::Arc::new(Mutex::new(SseEventBuffer::new(1)));
        let first = crate::streaming::emit_frame(&tx, &buffer, "status", "{}");

        let phrase = format!("| First id | `{first}` |");
        assert!(
            protocol_page_section(&page, "event-ids").contains(&phrase),
            "the Event ids section ({{ #event-ids }}) of {PROTOCOL_PAGE_PATH} does not say \
             `{phrase}`; a session's first frame takes id {first}"
        );
        let phrase = format!("starting at id `{first}`");
        assert!(
            protocol_page_section(&page, "replay").contains(&phrase),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not say `{phrase}`"
        );
    }

    /// The Replay section states `SSE_REPLAY_CAPACITY` as the number of frames a session keeps.
    #[test]
    fn protocol_page_replay_capacity_is_the_runtime_capacity() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "replay");
        let capacity = crate::runtime::SSE_REPLAY_CAPACITY;
        let phrase = format!("most recent {capacity} frames");
        assert!(
            section.contains(&phrase),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not say `{phrase}`; \
             SSE_REPLAY_CAPACITY is {capacity}"
        );
    }

    /// The Heartbeat section's liveness claim is held against
    /// `stream_watch_heartbeat_continues_while_session_thread_is_blocked`: the heartbeat keeps its
    /// cadence while the thread running the agent loop is held.
    #[test]
    fn protocol_page_says_the_heartbeat_keeps_its_cadence_during_a_turn() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "heartbeat");
        let required = "keeps its cadence while a turn is running";
        assert!(
            section.contains(required),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} does not say \
             `{required}`"
        );
        for forbidden in ["same thread", "holds the heartbeat"] {
            assert!(
                !section.contains(forbidden),
                "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} says \
                 `{forbidden}`, but the heartbeat is served off the agent loop's thread"
            );
        }
    }
}
