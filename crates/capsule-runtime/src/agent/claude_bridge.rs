//! The tool bridge — a loopback tool server for the `transport: process` inference path
//! **only**. Nothing here is used by the primary `transport: http` driver path; it exists solely
//! to give process-transport capsules the same artifact tool calling the HTTP path gets.
//!
//! # Why this exists
//!
//! With `transport: http`, murmur owns the agent loop: it sends the model `messages + tool
//! schemas`, the model returns a structured tool-call request, murmur executes the WASM tool,
//! and appends the result. The tool boundary sits between murmur and the model.
//!
//! With `transport: process` the runtime drives a harness CLI, which is a self-contained agent
//! that runs its *own* loop and executes its *own* tools on the host — the tool boundary sits inside the
//! subprocess, out of murmur's reach. So a plain process capsule could only do inference;
//! declared tool artifacts would be invisible to it.
//!
//! This bridge relocates the tool boundary back out to murmur. It is a tiny loopback HTTP
//! server that advertises the capsule's tool **schemas only** (no logic) over the protocol
//! harnesses speak to externally-hosted tool servers. When the model calls a tool, the request
//! comes here, and murmur executes it through the *same* `CapsuleStoreState` dispatch the HTTP
//! path uses — under the capsule's declared capabilities and sandbox. The model gets native,
//! structured tool calling; murmur keeps ownership of execution, capabilities, and the trace.
//!
//! # Scope / invariants (process transport only)
//!
//! - Bound to loopback with a per-run bearer token. The process driver is handed the URL, the
//!   token and the bare tool names, and is what writes them into whatever configuration its
//!   harness reads: the runtime builds no harness configuration and no harness tool name.
//! - Only the capsule's declared tools are advertised.
//! - Every call crosses the session's decision point first, over the gate channel, because the
//!   hook runtime and the trace writer belong to the task driving the harness and this bridge
//!   holds only the store. The decision point runs the required-field check on every call and
//!   then, only for a capsule with a policy hook or a `read_only` path, the manifest's
//!   `capabilities.filesystem.read_only` check and an `on-tool-call` policy hook.
//! - Request/response is plain JSON, so the transport is a minimal manual HTTP/1.1 handler
//!   mirroring `identity.rs`, not a full server stack.
//! - Tool calls run concurrently, up to [`MAX_IN_FLIGHT_CALLS`] at once: a shell command that
//!   runs for minutes does not hold up the harness's other calls.

use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedReadHalf, TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

use crate::{
    bindings::host::murmur::tool::run::{Status, ToolInput},
    detached::AbandonSignal,
    runtime::CapsuleStoreState,
};

/// How many `tools/call` dispatches the bridge runs at once.
///
/// Above what a harness fans out — one message's worth of parallel calls plus a few sub-agents
/// sharing the bridge — and small beside what each call can hold: a bridged shell call is one
/// host process under the scope's `cgroup_pids_max` and one thread of tokio's blocking pool. A
/// call beyond it is refused with [`at_capacity_text`] rather than queued, because a queued call
/// is a call that hangs behind a long one.
pub(super) const MAX_IN_FLIGHT_CALLS: usize = 16;

/// How many connections the bridge holds open at once. Further connections wait in the kernel's
/// accept backlog. At most [`MAX_IN_FLIGHT_CALLS`] of them can be long-running; every other one
/// is answered at once or dropped after [`REQUEST_READ_TIMEOUT`], so a waiting connection waits
/// about one read timeout, never for a tool call.
const MAX_OPEN_CONNECTIONS: usize = 64;

/// How long a connection has to deliver its whole request — request line, headers and body. A
/// connection that sends nothing in that time is closed without a response.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How many bytes a connection may send after its request while its call runs. They are read
/// and discarded so a close behind them can be seen; past this, the connection is no longer read.
const TRAILING_BYTES_CAP: usize = 64 * 1024;

/// The failing tool result's text for a call refused at [`MAX_IN_FLIGHT_CALLS`].
fn at_capacity_text(limit: usize) -> String {
    format!(
        "Refused: {limit} tool calls were already running, the most this session runs at once, \
         so nothing ran for this call. Call it again once one of the running calls has returned."
    )
}

/// Something the bridge saw that belongs in the trace as a `harness_note`.
///
/// The bridge holds no trace writer, so these go to the task driving the harness, which writes
/// [`BridgeNote::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BridgeNote {
    /// The harness closed a call's connection before its response was written.
    Abandoned {
        tool_name: String,
        elapsed_ms: u64,
        /// Whether the decision point had answered. Undecided means nothing was dispatched.
        decided: bool,
    },
    /// A call was refused because [`MAX_IN_FLIGHT_CALLS`] calls were already running.
    AtCapacity { tool_name: String, limit: usize },
}

impl BridgeNote {
    /// The `harness_note` text.
    pub(super) fn text(&self) -> String {
        match self {
            BridgeNote::Abandoned {
                tool_name,
                decided: false,
                ..
            } => format!(
                "The harness closed the connection for its call to {tool_name} before the call \
                 was decided, so nothing ran."
            ),
            BridgeNote::Abandoned {
                tool_name,
                elapsed_ms,
                decided: true,
            } => format!(
                "The harness closed the connection for its call to {tool_name} {elapsed_ms} ms \
                 into the call. A shell command still running is moved to the background as if \
                 it had outrun lifecycle.shell_grace_secs, or, with no task to deliver its \
                 completion to, runs to the end with its result discarded; anything else still \
                 running finishes unobserved."
            ),
            BridgeNote::AtCapacity { tool_name, limit } => format!(
                "The call to {tool_name} was refused because {limit} tool calls were already \
                 running."
            ),
        }
    }
}

/// One admitted `tools/call`, counted in `in_flight` until it is dropped — whether its response
/// was written or its future was.
struct InFlightSlot<'a>(&'a AtomicUsize);

impl<'a> InFlightSlot<'a> {
    fn claim(in_flight: &'a AtomicUsize, limit: usize) -> Option<Self> {
        in_flight
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < limit).then_some(n + 1)
            })
            .ok()
            .map(|_| Self(in_flight))
    }
}

impl Drop for InFlightSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What reading one request produced.
enum Incoming {
    /// The connection failed or ended before a whole request arrived. Nothing to answer.
    Gone,
    /// A request answered with a bare HTTP status.
    Reject(&'static str),
    /// An authorized JSON body.
    Rpc(Value),
}

/// The name this bridge registers itself under. Handed to the process driver as
/// `bridge.server-name`; how — and whether — a harness uses it to namespace tool names is the
/// driver's business, not the runtime's.
pub(super) const BRIDGE_SERVER_NAME: &str = "murmur";

/// Path the bridge listens on. Arbitrary; the driver is handed the full URL.
const BRIDGE_PATH: &str = "/bridge";

/// What a bridged tool call is refused with when the session's decision point cannot be reached
/// at all.
///
/// Fail closed: the alternative is a tool running because the thing that would have refused it
/// went away.
const GATE_UNREACHABLE: &str =
    "Refused: the session stopped answering policy checks, so this tool call was not dispatched.";

/// One bridged tool call, asking the session's decision point whether it may run.
///
/// The task driving the harness owns the hook runtime and the trace writer, and answers this
/// between two batches of harness output; the call is parked on `reply` in the meantime. Each call
/// carries its own `reply`, so a verdict reaches only the call that asked for it however many are
/// parked. A `reply` that is already closed belongs to a call whose harness went away, and is
/// skipped undecided.
pub(super) struct GateRequest {
    /// The tool's bare name, as the harness asked for it.
    pub(super) tool_name: String,
    /// The call's arguments, serialised exactly as dispatch is about to receive them.
    pub(super) input_json: String,
    /// The refusal text, or `None` to let the call run.
    pub(super) reply: oneshot::Sender<Option<String>>,
}

/// Put one call to the session's decision point and wait for its verdict.
///
/// Fail closed on both arms — a send onto a closed channel and a dropped reply alike — so a
/// session that has stopped answering cannot let a tool through ungated.
async fn ask_gate(
    gate: &mpsc::UnboundedSender<GateRequest>,
    tool_name: &str,
    input_json: &str,
) -> Option<String> {
    let (reply, verdict) = oneshot::channel();
    if gate
        .send(GateRequest {
            tool_name: tool_name.to_string(),
            input_json: input_json.to_string(),
            reply,
        })
        .is_err()
    {
        return Some(GATE_UNREACHABLE.to_string());
    }
    verdict
        .await
        .unwrap_or_else(|_| Some(GATE_UNREACHABLE.to_string()))
}

/// A tool-server result carrying `text` as a failure.
fn error_result(text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true
    })
}

/// Everything the process runner needs to hand a freshly-bound bridge to the driver.
pub(super) struct BridgeHandle {
    pub(super) listener: TcpListener,
    /// e.g. `http://127.0.0.1:52344/bridge`
    pub(super) url: String,
    /// Per-run bearer token every request must present.
    pub(super) token: String,
    /// Session id echoed back on `initialize`.
    pub(super) session_id: String,
    /// The capsule's tool names, bare. The driver names them the way its harness needs.
    pub(super) tool_names: Vec<String>,
    /// MCP-shaped tool schemas advertised on `tools/list`.
    mcp_tools: Vec<Value>,
    /// The in-flight bound, [`MAX_IN_FLIGHT_CALLS`]. A field only so this module's tests can
    /// reach it with a handful of calls.
    max_in_flight_calls: usize,
}

/// Convert murmur's tool inventory (`{name, parameters, description?}`) into the tool-server
/// schema shape (`{name, description, inputSchema}`) `tools/list` answers with.
fn inventory_to_mcp_tools(inventory: &[Value]) -> Vec<Value> {
    inventory
        .iter()
        .filter_map(|t| {
            let name = t.get("name").and_then(Value::as_str)?;
            let schema = t
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let mut tool = json!({ "name": name, "inputSchema": schema });
            if let Some(desc) = t.get("description").and_then(Value::as_str) {
                tool["description"] = Value::String(desc.to_string());
            }
            Some(tool)
        })
        .collect()
}

/// Bind the bridge on loopback (ephemeral port) and precompute what the driver is told about it.
/// Returns `None` when the capsule declares no tools — the harness then runs with none.
pub(super) async fn bind_bridge(bind_addr: &str, inventory: &[Value]) -> Option<BridgeHandle> {
    let mcp_tools = inventory_to_mcp_tools(inventory);
    if mcp_tools.is_empty() {
        return None;
    }

    // Port 0 = let the OS pick a free ephemeral port.
    let listener = TcpListener::bind(format!("{bind_addr}:0")).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    let host = if bind_addr.is_empty() {
        "127.0.0.1"
    } else {
        bind_addr
    };

    let tool_names = mcp_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    Some(BridgeHandle {
        listener,
        url: format!("http://{host}:{port}{BRIDGE_PATH}"),
        token: Uuid::new_v4().simple().to_string(),
        session_id: Uuid::new_v4().simple().to_string(),
        tool_names,
        mcp_tools,
        max_in_flight_calls: MAX_IN_FLIGHT_CALLS,
    })
}

impl BridgeHandle {
    /// Accept-and-serve loop. Never returns on its own; the process runner drops this future once
    /// the run ends, which drops every connection still being served and every call still in
    /// flight with it.
    ///
    /// Connections are served concurrently, on the caller's task: every accepted connection's
    /// handler joins one set polled beside the accept, and nothing is spawned, which is what lets
    /// `store` stay a borrow rather than something `'static`. A shell command that runs for
    /// minutes holds one handler while `initialize`, `tools/list` and other calls are answered.
    ///
    /// Two fixed bounds:
    ///
    /// - At most [`MAX_IN_FLIGHT_CALLS`] `tools/call` dispatches at once, counted from the
    ///   decision point until the response is written or the call is dropped. A call beyond that
    ///   is answered at once with a failing tool result saying nothing ran, and never reaches the
    ///   decision point or dispatch. Nothing else is refused by it.
    /// - At most [`MAX_OPEN_CONNECTIONS`] connections, each given [`REQUEST_READ_TIMEOUT`] to
    ///   deliver its request. While that many are open, no connection is accepted and new ones
    ///   wait in the kernel's backlog.
    ///
    /// A connection the harness closes while its call runs abandons the call: the call's future is
    /// dropped, a shell command still in the foreground is demoted to the background, and a
    /// [`BridgeNote::Abandoned`] goes to `notes`.
    ///
    /// `on_request` is called once per accepted connection, before it is read — a refused one
    /// included. A harness working through a long tool call writes nothing to stdout, so this is
    /// the other half of the evidence that it is still alive — see the runner's inactivity clock.
    ///
    /// `gate` is where every admitted `tools/call` asks whether it may run. The answering side
    /// runs the required-field check on each call and then, only for a capsule with a policy hook
    /// or a `read_only` path, the manifest's `read_only` check and the policy hook.
    pub(super) async fn serve(
        &self,
        store: &CapsuleStoreState,
        gate: &mpsc::UnboundedSender<GateRequest>,
        notes: &mpsc::UnboundedSender<BridgeNote>,
        on_request: &dyn Fn(),
    ) {
        let in_flight = AtomicUsize::new(0);
        let mut open = FuturesUnordered::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept(), if open.len() < MAX_OPEN_CONNECTIONS => {
                    let Ok((stream, _)) = accepted else {
                        return;
                    };
                    on_request();
                    open.push(self.handle_connection(stream, store, gate, notes, &in_flight));
                }
                Some(()) = open.next(), if !open.is_empty() => {}
            }
        }
    }

    /// Manual HTTP/1.1 request handler (mirrors `identity.rs`), routing the tool-server
    /// protocol: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.
    async fn handle_connection(
        &self,
        stream: TcpStream,
        store: &CapsuleStoreState,
        gate: &mpsc::UnboundedSender<GateRequest>,
        notes: &mpsc::UnboundedSender<BridgeNote>,
        in_flight: &AtomicUsize,
    ) {
        let (reader_half, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader_half);

        let request = match tokio::time::timeout(
            REQUEST_READ_TIMEOUT,
            self.read_request(&mut reader),
        )
        .await
        {
            Ok(Incoming::Rpc(request)) => request,
            Ok(Incoming::Reject(status)) => {
                write_response(&mut writer, status, None, None).await;
                return;
            }
            Ok(Incoming::Gone) | Err(_) => return,
        };

        let rpc_method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();

        // Notifications carry no id and expect no body.
        if id.is_none() {
            write_response(&mut writer, "202 Accepted", None, None).await;
            return;
        }
        let id = id.unwrap();

        match rpc_method {
            "initialize" => {
                let protocol = request
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or("2024-11-05");
                let result = json!({
                    "protocolVersion": protocol,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "murmur-bridge", "version": env!("CARGO_PKG_VERSION") }
                });
                let session_header = format!("Mcp-Session-Id: {}", self.session_id);
                write_json_rpc(&mut writer, &id, result, Some(&session_header)).await;
            }
            "tools/list" => {
                write_json_rpc(&mut writer, &id, json!({ "tools": self.mcp_tools }), None).await;
            }
            "tools/call" => {
                let (tool_name, input_json) = call_target(request.get("params"));
                let Some(_slot) = InFlightSlot::claim(in_flight, self.max_in_flight_calls) else {
                    let limit = self.max_in_flight_calls;
                    let _ = notes.send(BridgeNote::AtCapacity { tool_name, limit });
                    write_json_rpc(
                        &mut writer,
                        &id,
                        error_result(at_capacity_text(limit)),
                        None,
                    )
                    .await;
                    return;
                };

                let abandon = AbandonSignal::new();
                let decided = AtomicBool::new(false);
                let started = Instant::now();
                let dispatch = self.dispatch_tool_call(
                    &tool_name,
                    input_json,
                    store,
                    gate,
                    abandon.clone(),
                    &decided,
                );
                // A client that half-closes its write side after sending its request reads here
                // as one that went away, and its call is abandoned: a client must keep the
                // connection open until the response arrives.
                let result = tokio::select! {
                    result = dispatch => Some(result),
                    () = peer_closed(&mut reader) => None,
                };
                match result {
                    Some(result) => write_json_rpc(&mut writer, &id, result, None).await,
                    None => {
                        // The dispatch future is already dropped. A shell command it left on the
                        // blocking pool reads this at its next poll and demotes.
                        abandon.raise();
                        let _ = notes.send(BridgeNote::Abandoned {
                            tool_name,
                            elapsed_ms: started
                                .elapsed()
                                .as_millis()
                                .try_into()
                                .unwrap_or(u64::MAX),
                            decided: decided.load(Ordering::SeqCst),
                        });
                    }
                }
            }
            _ => {
                let err = json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                });
                write_response(
                    &mut writer,
                    "200 OK",
                    Some("application/json"),
                    Some(&err.to_string()),
                )
                .await;
            }
        }
    }

    /// Read one request: request line, headers and body. The caller bounds the whole read by
    /// [`REQUEST_READ_TIMEOUT`].
    async fn read_request(&self, reader: &mut BufReader<OwnedReadHalf>) -> Incoming {
        // Request line: "<METHOD> <PATH> HTTP/1.1"
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            return Incoming::Gone;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();

        // Headers
        let mut content_length = 0usize;
        let mut authorized = false;
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
            } else if let Some(rest) = lower.strip_prefix("authorization:") {
                authorized = rest.trim() == format!("bearer {}", self.token);
            }
        }

        // A client may open a GET stream for server->client messages; this bridge needs none.
        if method == "GET" {
            return Incoming::Reject("405 Method Not Allowed");
        }

        if !authorized {
            return Incoming::Reject("401 Unauthorized");
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
            return Incoming::Gone;
        }
        match serde_json::from_slice(&body) {
            Ok(request) => Incoming::Rpc(request),
            Err(_) => Incoming::Reject("400 Bad Request"),
        }
    }

    /// Execute one `tools/call` through murmur's WASM tool dispatch — the same executor the
    /// HTTP transport uses — and shape the outcome as a tool-server result. Tool execution
    /// stays entirely in murmur's sandbox under the capsule's declared capabilities; the harness
    /// never runs anything itself.
    ///
    /// The session's decision point comes first, over `gate`, and `decided` is set once it has
    /// answered. A refusal is returned as the failing tool result the harness reports back to its
    /// model, carrying the refusal's own text unaltered — its no-retry sentence is what stops the
    /// model asking again.
    ///
    /// `abandon` is raised by the caller if the harness goes away; see
    /// [`CapsuleStoreState::dispatch_bridged_tool_async`].
    async fn dispatch_tool_call(
        &self,
        name: &str,
        input_json: String,
        store: &CapsuleStoreState,
        gate: &mpsc::UnboundedSender<GateRequest>,
        abandon: AbandonSignal,
        decided: &AtomicBool,
    ) -> Value {
        // A refusal means nothing ran, and its trace line is written on the answering side. The
        // harness still reports the failure as a tool result of its own, which the event sink
        // records as a `tool_call` with `status: error`: this request carries no id tying it to
        // that result, so suppressing the pair would mean guessing.
        let refusal = ask_gate(gate, name, &input_json).await;
        decided.store(true, Ordering::SeqCst);
        if let Some(refusal) = refusal {
            return error_result(refusal);
        }

        match store
            .dispatch_bridged_tool_async(
                name,
                ToolInput {
                    data: Some(input_json),
                    log_path: None,
                },
                abandon,
            )
            .await
        {
            Ok(outcome) => {
                let is_error = !matches!(outcome.result.status, Status::Passed);
                // `outcome.fatal` is deliberately not acted on here: this bridge is a tool server
                // for an external harness process and owns no murmur session to end. The
                // failure still reaches the caller in full — `result.data` carries the same named
                // text (`RuntimeError`'s Display) that the agent loop would end the session with.
                let text = outcome
                    .result
                    .data
                    .or(outcome.result.summary)
                    .unwrap_or_else(|| "tool returned no data".to_string());
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error
                })
            }
            Err(err) => error_result(format!("tool '{name}' failed: {err}")),
        }
    }
}

/// A `tools/call`'s tool name and its arguments serialised for dispatch.
///
/// The arguments object is forwarded verbatim as the tool input payload, exactly like the HTTP
/// path forwards `tool_use.input`.
fn call_target(params: Option<&Value>) -> (String, String) {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let input_json = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string());
    (name, input_json)
}

/// Resolves when the peer has gone away: a read on the request's read half reports end of stream
/// or fails.
///
/// Bytes sent after the request are read and discarded, up to [`TRAILING_BYTES_CAP`]; past that
/// the connection is no longer watched and this never resolves.
async fn peer_closed(reader: &mut BufReader<OwnedReadHalf>) {
    let mut scratch = [0u8; 1024];
    let mut discarded = 0usize;
    while discarded <= TRAILING_BYTES_CAP {
        match reader.read(&mut scratch).await {
            Ok(0) | Err(_) => return,
            Ok(read) => discarded += read,
        }
    }
    std::future::pending().await
}

/// Write a JSON-RPC 2.0 success response as an `application/json` HTTP reply.
async fn write_json_rpc(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    id: &Value,
    result: Value,
    extra_header: Option<&str>,
) {
    let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
    write_response_with_header(
        writer,
        "200 OK",
        Some("application/json"),
        Some(&payload),
        extra_header,
    )
    .await;
}

async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    content_type: Option<&str>,
    body: Option<&str>,
) {
    write_response_with_header(writer, status, content_type, body, None).await;
}

/// Build and write a minimal HTTP/1.1 response with `connection: close` (so the client opens a
/// fresh connection per request — the simplest correct behaviour for this short-lived server).
async fn write_response_with_header(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    content_type: Option<&str>,
    body: Option<&str>,
    extra_header: Option<&str>,
) {
    let body = body.unwrap_or("");
    let mut response = format!("HTTP/1.1 {status}\r\n");
    if let Some(ct) = content_type {
        response.push_str(&format!("content-type: {ct}\r\n"));
    }
    if let Some(h) = extra_header {
        response.push_str(h);
        response.push_str("\r\n");
    }
    response.push_str(&format!("content-length: {}\r\n", body.len()));
    response.push_str("connection: close\r\n\r\n");
    response.push_str(body);
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_maps_to_mcp_tool_shape() {
        // murmur inventory uses `parameters`; the tool-server protocol expects `inputSchema`.
        let inventory = vec![json!({
            "name": "murmur-tool-editor",
            "description": "edits files",
            "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
        })];
        let mcp = inventory_to_mcp_tools(&inventory);
        assert_eq!(mcp.len(), 1);
        assert_eq!(mcp[0]["name"], "murmur-tool-editor");
        assert_eq!(mcp[0]["description"], "edits files");
        assert_eq!(mcp[0]["inputSchema"]["type"], "object");
        assert!(mcp[0].get("parameters").is_none());
    }

    #[test]
    fn inventory_without_schema_defaults_to_object() {
        let inventory = vec![json!({ "name": "t" })];
        let mcp = inventory_to_mcp_tools(&inventory);
        assert_eq!(mcp[0]["inputSchema"]["type"], "object");
    }

    use std::{net::SocketAddr, sync::Arc};

    use tokio::sync::mpsc::error::TryRecvError;

    /// A call must be answered inside this for the bridge to count as not holding it up.
    const PROMPT: Duration = Duration::from_secs(1);

    /// A bridge serving one `echo-tool`, with a store behind it that no test dispatches into: the
    /// test answers the decision point itself, always with a refusal.
    struct Fixture {
        _dir: tempfile::TempDir,
        store: CapsuleStoreState,
        handle: BridgeHandle,
        addr: SocketAddr,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = crate::runtime::build_test_state(
                Arc::new(murmur_artifact::LocalRegistry::new(
                    dir.path().join("registry"),
                )),
                dir.path().to_path_buf(),
                dir.path().join("murmur.lock"),
            );
            let handle = bind_bridge(
                "127.0.0.1",
                &[json!({"name": "echo-tool", "parameters": {"type": "object"}})],
            )
            .await
            .expect("bridge binds");
            let addr = handle.listener.local_addr().unwrap();
            Self {
                _dir: dir,
                store,
                handle,
                addr,
            }
        }

        fn client(&self) -> Client {
            Client {
                addr: self.addr,
                token: self.handle.token.clone(),
            }
        }
    }

    /// A raw HTTP/1.1 client that speaks to the bridge the way a harness does: one request per
    /// connection, read to EOF.
    #[derive(Clone)]
    struct Client {
        addr: SocketAddr,
        token: String,
    }

    impl Client {
        fn request(&self, body: &Value) -> String {
            let body = body.to_string();
            format!(
                "POST {BRIDGE_PATH} HTTP/1.1\r\nhost: {}\r\nauthorization: Bearer {}\r\n\
                 content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                self.addr,
                self.token,
                body.len()
            )
        }

        /// Open a connection and send one JSON-RPC request on it, leaving it open.
        async fn send(&self, body: Value) -> TcpStream {
            let mut stream = TcpStream::connect(self.addr).await.unwrap();
            stream
                .write_all(self.request(&body).as_bytes())
                .await
                .unwrap();
            stream
        }

        async fn post(&self, body: Value) -> Value {
            let mut raw = Vec::new();
            let mut stream = self.send(body).await;
            stream.read_to_end(&mut raw).await.unwrap();
            response_body(raw)
        }

        /// A `tools/call` on a plain thread of its own, so the test can go on while it is parked.
        /// The answer arrives on the returned receiver.
        fn call_in_background(&self, msg: &str) -> oneshot::Receiver<Value> {
            use std::io::{Read, Write};

            let (answer, answered) = oneshot::channel();
            let request = self.request(&tool_call(msg));
            let addr = self.addr;
            std::thread::spawn(move || {
                let mut stream = std::net::TcpStream::connect(addr).unwrap();
                stream.write_all(request.as_bytes()).unwrap();
                let mut raw = Vec::new();
                stream.read_to_end(&mut raw).unwrap();
                let _ = answer.send(response_body(raw));
            });
            answered
        }
    }

    /// The JSON body of a whole HTTP response.
    fn response_body(raw: Vec<u8>) -> Value {
        let raw = String::from_utf8(raw).unwrap();
        let (_, body) = raw.split_once("\r\n\r\n").expect("a whole response");
        serde_json::from_str(body).unwrap_or_else(|_| panic!("a JSON body, got {raw:?}"))
    }

    fn tool_call(msg: &str) -> Value {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "echo-tool", "arguments": {"msg": msg}}
        })
    }

    fn text_of(response: &Value) -> &str {
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
    }

    async fn next_gate(gate: &mut mpsc::UnboundedReceiver<GateRequest>) -> GateRequest {
        tokio::time::timeout(PROMPT, gate.recv())
            .await
            .expect("the call reached the decision point in time")
            .expect("the gate channel is open")
    }

    async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(PROMPT, future)
            .await
            .unwrap_or_else(|_| panic!("{what} was not answered within {PROMPT:?}"))
    }

    #[tokio::test]
    async fn a_parked_call_does_not_block_the_next_connection() {
        let fixture = Fixture::new().await;
        let client = fixture.client();
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
        let (note_tx, _note_rx) = mpsc::unbounded_channel();
        let accepted = AtomicUsize::new(0);
        let on_request = || {
            accepted.fetch_add(1, Ordering::SeqCst);
        };
        let serve = fixture
            .handle
            .serve(&fixture.store, &gate_tx, &note_tx, &on_request);

        let body = async {
            let parked = client.call_in_background("A");
            let a = next_gate(&mut gate_rx).await;

            let initialized = within(
                "initialize",
                client.post(json!({
                    "jsonrpc": "2.0", "id": 2, "method": "initialize",
                    "params": {"protocolVersion": "2025-03-26"}
                })),
            )
            .await;
            assert_eq!(initialized["result"]["protocolVersion"], "2025-03-26");

            let listed = within(
                "tools/list",
                client.post(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})),
            )
            .await;
            assert_eq!(listed["result"]["tools"][0]["name"], "echo-tool");

            let second = client.call_in_background("B");
            let b = next_gate(&mut gate_rx).await;
            assert!(b.input_json.contains("\"B\""));

            let _ = a.reply.send(Some("refused".to_string()));
            let _ = b.reply.send(Some("refused".to_string()));
            within("call A", parked).await.unwrap();
            within("call B", second).await.unwrap();
        };
        tokio::select! {
            () = serve => panic!("serve returned"),
            () = body => {}
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            4,
            "one per connection opened"
        );
    }

    #[tokio::test]
    async fn each_parked_call_gets_its_own_verdict() {
        let fixture = Fixture::new().await;
        let client = fixture.client();
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
        let (note_tx, _note_rx) = mpsc::unbounded_channel();
        let serve = fixture
            .handle
            .serve(&fixture.store, &gate_tx, &note_tx, &|| {});

        let body = async {
            let call_a = client.call_in_background("A");
            let a = next_gate(&mut gate_rx).await;
            let call_b = client.call_in_background("B");
            let b = next_gate(&mut gate_rx).await;
            assert!(a.input_json.contains("\"A\"") && b.input_json.contains("\"B\""));

            // Answered in the reverse of arrival order.
            let _ = b.reply.send(Some("refused-B".to_string()));
            let _ = a.reply.send(Some("refused-A".to_string()));

            let answer_a = within("call A", call_a).await.unwrap();
            let answer_b = within("call B", call_b).await.unwrap();
            assert_eq!(text_of(&answer_a), "refused-A");
            assert_eq!(text_of(&answer_b), "refused-B");
            assert_eq!(answer_a["result"]["isError"], true);
            assert_eq!(answer_b["result"]["isError"], true);
        };
        tokio::select! {
            () = serve => panic!("serve returned"),
            () = body => {}
        }
    }

    #[tokio::test]
    async fn a_call_above_the_bound_is_refused_at_once() {
        let mut fixture = Fixture::new().await;
        fixture.handle.max_in_flight_calls = 2;
        let client = fixture.client();
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
        let (note_tx, mut note_rx) = mpsc::unbounded_channel();
        let serve = fixture
            .handle
            .serve(&fixture.store, &gate_tx, &note_tx, &|| {});

        let body = async {
            let call_a = client.call_in_background("A");
            let a = next_gate(&mut gate_rx).await;
            let call_b = client.call_in_background("B");
            let b = next_gate(&mut gate_rx).await;

            let refused = within("the third call", client.post(tool_call("C"))).await;
            assert_eq!(refused["result"]["isError"], true);
            let text = text_of(&refused);
            assert!(text.contains('2'), "names the limit: {text}");
            assert!(text.contains("nothing ran"), "says nothing ran: {text}");

            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(
                matches!(gate_rx.try_recv(), Err(TryRecvError::Empty)),
                "a call refused at the bound never reaches the decision point"
            );
            assert_eq!(
                note_rx.try_recv().ok(),
                Some(BridgeNote::AtCapacity {
                    tool_name: "echo-tool".to_string(),
                    limit: 2
                })
            );

            let listed = within(
                "tools/list",
                client.post(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"})),
            )
            .await;
            assert_eq!(listed["result"]["tools"][0]["name"], "echo-tool");

            let _ = a.reply.send(Some("refused-A".to_string()));
            within("call A", call_a).await.unwrap();

            let call_d = client.call_in_background("D");
            let d = next_gate(&mut gate_rx).await;
            assert!(
                d.input_json.contains("\"D\""),
                "a freed slot admits the next call"
            );

            let _ = b.reply.send(Some("refused".to_string()));
            let _ = d.reply.send(Some("refused".to_string()));
            within("call B", call_b).await.unwrap();
            within("call D", call_d).await.unwrap();
        };
        tokio::select! {
            () = serve => panic!("serve returned"),
            () = body => {}
        }
    }

    #[tokio::test]
    async fn a_closed_connection_abandons_its_call() {
        let fixture = Fixture::new().await;
        let client = fixture.client();
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
        let (note_tx, mut note_rx) = mpsc::unbounded_channel();
        let serve = fixture
            .handle
            .serve(&fixture.store, &gate_tx, &note_tx, &|| {});

        let body = async {
            let connection = client.send(tool_call("A")).await;
            let mut a = next_gate(&mut gate_rx).await;
            drop(connection);

            within("the closed reply", a.reply.closed()).await;
            let note = within("the abandonment note", note_rx.recv())
                .await
                .expect("the note channel is open");
            match note {
                BridgeNote::Abandoned {
                    tool_name, decided, ..
                } => {
                    assert_eq!(tool_name, "echo-tool");
                    assert!(!decided, "the decision point had not answered");
                }
                other => panic!("expected an abandonment note, got {other:?}"),
            }
        };
        tokio::select! {
            () = serve => panic!("serve returned"),
            () = body => {}
        }
    }

    #[tokio::test]
    async fn dropping_serve_cancels_parked_calls() {
        let fixture = Fixture::new().await;
        let client = fixture.client();
        let (gate_tx, mut gate_rx) = mpsc::unbounded_channel();
        let (note_tx, _note_rx) = mpsc::unbounded_channel();
        let mut serve = Box::pin(
            fixture
                .handle
                .serve(&fixture.store, &gate_tx, &note_tx, &|| {}),
        );

        let _connection = client.send(tool_call("A")).await;
        let a = tokio::select! {
            () = &mut serve => panic!("serve returned"),
            a = next_gate(&mut gate_rx) => a,
        };
        assert!(!a.reply.is_closed());
        drop(serve);
        assert!(
            a.reply.is_closed(),
            "dropping serve drops the call waiting on this verdict"
        );
    }

    #[tokio::test]
    async fn bind_bridge_is_none_without_tools() {
        // No tools declared → no bridge, so the harness runs with no tools.
        assert!(bind_bridge("127.0.0.1", &[]).await.is_none());
    }

    /// The driver is handed bare names and builds whatever its harness needs from them: the
    /// runtime never spells a harness's tool naming.
    #[tokio::test]
    async fn bind_bridge_names_tools_bare() {
        let inventory = vec![json!({"name": "editor", "parameters": {"type": "object"}})];
        let handle = bind_bridge("127.0.0.1", &inventory)
            .await
            .expect("bridge should bind when tools are declared");
        assert_eq!(handle.tool_names, vec!["editor"]);
        assert!(handle.url.starts_with("http://127.0.0.1:"));
        assert!(!handle.token.is_empty());
    }
}
