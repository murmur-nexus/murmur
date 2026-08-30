use std::sync::{Arc, Mutex};

use murmur_artifact::{ConversationMode, TaskAcceptance};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::a2a::{
    A2aMessage, A2aTask, IncomingTask, JsonRpcRequest, JsonRpcResponse, TaskRegistry, TaskState,
    TaskStatus,
};
use crate::errors::RuntimeError;
use crate::origin::{self, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
use crate::peer_handoff::{handle_peer_request, is_peer_path, PeerPlane, AUDIENCE_HEADER};
use crate::resource_plane::{
    handle_resource_request, reason_phrase, ResourcePlane, ResourceResponse, RESOURCE_PATH_PREFIX,
};
use crate::streaming::{
    format_gap_event, format_sse_event, is_final_sse_event, ReplayResult, SseBroadcast,
    SseEventBuffer, StreamStatus, TaskStatusUpdateEvent,
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

/// Build the Agent Card JSON derived from capsule identity and capability policy.
pub(crate) fn build_agent_card(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
) -> serde_json::Value {
    let tools: Vec<&str> = installed_artifacts
        .iter()
        .filter(|a| a.runtime.is_llm_visible())
        .map(|a| a.name.as_str())
        .collect();

    serde_json::json!({
        "name": identity.capsule_name,
        "version": identity.capsule_version,
        "url": identity.capsule_url,
        "capabilities": {
            "tools": tools,
            "shell": !capability_policy.shell_allow.is_empty(),
            "network": !capability_policy.network_allow.is_empty(),
            "streaming": true,
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
                        tokio::task::spawn_local(async move {
                            handle_connection(stream, card, registry, tx, acceptance, sse, buf, mode_str, plane, peer).await;
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
        }
    }

    // Classified once at the door, so both task-starting paths below read the same rule rather
    // than each interpreting the headers for itself.
    let provenance = origin::from_wire(task_origin.as_deref(), task_trust.as_deref());

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

        // Peek at method to route SSE endpoints before full dispatch
        if let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&body_str) {
            if req.method == "message/stream" {
                handle_message_stream(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    &task_acceptance,
                    traceparent,
                    provenance,
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                )
                .await;
                return;
            }
            if req.method == "stream/watch" {
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
        }

        let response = handle_jsonrpc(
            &body_str,
            &task_registry,
            &task_tx,
            &task_acceptance,
            traceparent,
            provenance,
        );
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
    task_acceptance: &TaskAcceptance,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
) {
    use tokio::io::AsyncWriteExt;

    // task_acceptance: none — method is not available
    if matches!(task_acceptance, TaskAcceptance::None) {
        let error_body =
            JsonRpcResponse::err(req.id, -32601, "Method not found").into_http_response();
        let _ = writer.write_all(error_body.as_bytes()).await;
        return;
    }

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
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                if writer.write_all(b":heartbeat\n\n").await.is_err() {
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

    loop {
        match rx.recv().await {
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

fn handle_jsonrpc(
    body: &str,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    task_acceptance: &TaskAcceptance,
    traceparent: Option<String>,
    provenance: TaskProvenance,
) -> String {
    let req: JsonRpcRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(_) => {
            return JsonRpcResponse::err(Value::Null, -32700, "Parse error").into_http_response();
        }
    };

    let id = req.id.clone();
    match req.method.as_str() {
        "message/send" => handle_message_send(
            id,
            &req.params,
            task_registry,
            task_tx,
            task_acceptance,
            traceparent,
            provenance,
        ),
        "tasks/get" => handle_tasks_get(id, &req.params, task_registry),
        _ => JsonRpcResponse::err(id, -32601, "Method not found").into_http_response(),
    }
}

fn handle_message_send(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    task_acceptance: &TaskAcceptance,
    traceparent: Option<String>,
    provenance: TaskProvenance,
) -> String {
    // task_acceptance: none — method is not available
    if matches!(task_acceptance, TaskAcceptance::None) {
        return JsonRpcResponse::err(id, -32601, "Method not found").into_http_response();
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::RuntimeError;

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
}
