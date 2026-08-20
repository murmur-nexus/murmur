//! Claude Bridge — a loopback tool server for the `transport: process` (subscription CLI)
//! inference path **only**. Nothing here is used by the primary `transport: http` driver
//! path; it exists solely to give process-transport capsules the same artifact tool calling
//! the HTTP path gets.
//!
//! # Why this exists
//!
//! With `transport: http`, murmur owns the agent loop: it sends the model `messages + tool
//! schemas`, the model returns a structured tool-call request, murmur executes the WASM tool,
//! and appends the result. The tool boundary sits between murmur and the model.
//!
//! With `transport: process` we drive the `claude` CLI, which is a self-contained agent that
//! runs its *own* loop and executes its *own* tools on the host — the tool boundary sits
//! inside the subprocess, out of murmur's reach. So a plain process capsule can only do
//! inference; declared tool artifacts are invisible to it.
//!
//! This bridge relocates the tool boundary back out to murmur. It is a tiny loopback HTTP
//! server that advertises the capsule's tool **schemas only** (no logic) over the protocol
//! the CLI speaks to externally-hosted tool servers. When the model calls a tool, the request
//! comes here, and murmur executes it through the *same* `CapsuleStoreState` dispatch the HTTP
//! path uses — under the capsule's declared capabilities and sandbox. The model gets native,
//! structured tool calling; murmur keeps ownership of execution, capabilities, and the trace.
//!
//! # Scope / invariants (process transport only)
//!
//! - Bound to loopback with a per-run bearer token; the CLI is pointed at it with a strict
//!   config so it uses *only* this server and none of the operator's own tool servers.
//! - Only the capsule's declared tools are advertised; the CLI's built-in host tools stay off.
//! - Request/response is plain JSON (the CLI's client does not require a streaming channel for
//!   tool calls — verified against the real CLI), so the transport is a minimal manual HTTP/1.1
//!   handler mirroring `identity.rs`, not a full server stack.

use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

use crate::{
    bindings::host::murmur::tool::run::{Status, ToolInput},
    runtime::CapsuleStoreState,
};

/// The server key the CLI uses to namespace this bridge's tools. Tools are addressed by the
/// CLI as `mcp__<server_key>__<tool_name>`, so this also forms the `--tools` allowlist names.
pub(super) const BRIDGE_SERVER_KEY: &str = "claude_bridge";

/// Path the bridge listens on. Arbitrary; the CLI is told the full URL in its config.
const BRIDGE_PATH: &str = "/bridge";

/// Everything the process loop needs to point the CLI at a freshly-bound bridge.
pub(super) struct BridgeHandle {
    pub(super) listener: TcpListener,
    /// e.g. `http://127.0.0.1:52344/bridge`
    pub(super) url: String,
    /// Per-run bearer token the CLI must present on every request.
    pub(super) token: String,
    /// Session id echoed back to the CLI on `initialize`.
    pub(super) session_id: String,
    /// Fully-qualified tool names for the CLI's `--tools` allowlist
    /// (`mcp__claude_bridge__<tool>`), restricting it to exactly the capsule's tools.
    pub(super) allowed_tool_names: Vec<String>,
    /// MCP-shaped tool schemas advertised on `tools/list`.
    mcp_tools: Vec<Value>,
}

/// Convert murmur's tool inventory (`{name, parameters, description?}`) into the tool-server
/// schema shape (`{name, description, inputSchema}`) the CLI expects on `tools/list`.
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

/// Bind the bridge on loopback (ephemeral port) and precompute its config. Returns `None`
/// when the capsule declares no tools — the process loop then keeps its plain inference path.
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

    let allowed_tool_names = mcp_tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(|n| format!("mcp__{BRIDGE_SERVER_KEY}__{n}"))
        .collect();

    Some(BridgeHandle {
        listener,
        url: format!("http://{host}:{port}{BRIDGE_PATH}"),
        token: Uuid::new_v4().simple().to_string(),
        session_id: Uuid::new_v4().simple().to_string(),
        allowed_tool_names,
        mcp_tools,
    })
}

impl BridgeHandle {
    /// The `--mcp-config` JSON value the Claude CLI loads to reach this bridge. Kept internal to
    /// the process transport; never surfaced in the manifest or to the user.
    pub(super) fn mcp_config_json(&self) -> String {
        json!({
            "mcpServers": {
                BRIDGE_SERVER_KEY: {
                    "type": "http",
                    "url": self.url,
                    "headers": { "Authorization": format!("Bearer {}", self.token) }
                }
            }
        })
        .to_string()
    }

    /// Codex-dialect equivalent of [`Self::mcp_config_json`]: the `-c key=value` overrides that
    /// register this bridge as a codex MCP server for one `codex exec` run (no persistent config
    /// change) and auto-approve its tool calls (codex exec is non-interactive and cannot prompt).
    /// Values are TOML literals — hence the embedded quotes. Process transport / codex dialect only.
    pub(super) fn codex_config_args(&self) -> Vec<String> {
        let key = BRIDGE_SERVER_KEY;
        vec![
            "-c".into(),
            format!("mcp_servers.{key}.url=\"{}\"", self.url),
            "-c".into(),
            format!(
                "mcp_servers.{key}.http_headers.Authorization=\"Bearer {}\"",
                self.token
            ),
            "-c".into(),
            format!("mcp_servers.{key}.default_tools_approval_mode=\"approve\""),
        ]
    }

    /// Accept-and-serve loop. Runs concurrently with the CLI's stdout read loop (same task,
    /// via `select!`), so it shares an immutable `&CapsuleStoreState` — tool dispatch is
    /// `&self`, and connections are served one at a time (the CLI issues tool calls
    /// sequentially), so no locking or `&mut` is needed. Never returns on its own; the process
    /// loop drops this future once the CLI produces its result.
    pub(super) async fn serve(&self, store: &CapsuleStoreState) {
        // Serve inline (not spawned): keeps the borrow of `store` non-'static and serializes
        // tool execution, which is what we want for a single CLI client.
        while let Ok((stream, _)) = self.listener.accept().await {
            self.handle_connection(stream, store).await;
        }
    }

    /// Manual HTTP/1.1 request handler (mirrors `identity.rs`), routing the tool-server
    /// protocol: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.
    async fn handle_connection(&self, stream: TcpStream, store: &CapsuleStoreState) {
        use tokio::io::AsyncBufReadExt;

        let (reader_half, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader_half);

        // Request line: "<METHOD> <PATH> HTTP/1.1"
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await.is_err() {
            return;
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

        // The CLI may open a GET stream for server->client messages; we don't need one.
        if method == "GET" {
            write_response(&mut writer, "405 Method Not Allowed", None, None).await;
            return;
        }

        if !authorized {
            write_response(&mut writer, "401 Unauthorized", None, None).await;
            return;
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let request: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                write_response(&mut writer, "400 Bad Request", None, None).await;
                return;
            }
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
                    "serverInfo": { "name": "murmur-claude-bridge", "version": env!("CARGO_PKG_VERSION") }
                });
                let session_header = format!("Mcp-Session-Id: {}", self.session_id);
                write_json_rpc(&mut writer, &id, result, Some(&session_header)).await;
            }
            "tools/list" => {
                write_json_rpc(&mut writer, &id, json!({ "tools": self.mcp_tools }), None).await;
            }
            "tools/call" => {
                let result = self.dispatch_tool_call(request.get("params"), store).await;
                write_json_rpc(&mut writer, &id, result, None).await;
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

    /// Execute one `tools/call` through murmur's WASM tool dispatch — the same executor the
    /// HTTP transport uses — and shape the outcome as a tool-server result. Tool execution
    /// stays entirely in murmur's sandbox under the capsule's declared capabilities; the CLI
    /// never runs anything itself.
    async fn dispatch_tool_call(&self, params: Option<&Value>, store: &CapsuleStoreState) -> Value {
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // The tool's arguments object is forwarded verbatim as the tool input payload, exactly
        // like the HTTP path forwards `tool_use.input`.
        let arguments = params
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let input_json = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string());

        match store
            .dispatch_agent_tool_async(
                &name,
                ToolInput {
                    data: Some(input_json),
                    log_path: None,
                },
            )
            .await
        {
            Ok(outcome) => {
                let is_error = !matches!(outcome.result.status, Status::Passed);
                // `outcome.fatal` is deliberately not acted on here: this bridge is a tool server
                // for an external Claude Code process and owns no murmur session to end. The
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
            Err(err) => json!({
                "content": [{ "type": "text", "text": format!("tool '{name}' failed: {err}") }],
                "isError": true
            }),
        }
    }
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

/// Build and write a minimal HTTP/1.1 response with `connection: close` (so the CLI opens a
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

    #[tokio::test]
    async fn bind_bridge_is_none_without_tools() {
        // No tools declared → no bridge, so the process path stays pure inference.
        assert!(bind_bridge("127.0.0.1", &[]).await.is_none());
    }

    #[tokio::test]
    async fn bind_bridge_builds_config_and_allowlist() {
        let inventory = vec![json!({"name": "editor", "parameters": {"type": "object"}})];
        let handle = bind_bridge("127.0.0.1", &inventory)
            .await
            .expect("bridge should bind when tools are declared");
        assert_eq!(
            handle.allowed_tool_names,
            vec!["mcp__claude_bridge__editor"]
        );
        let cfg = handle.mcp_config_json();
        assert!(cfg.contains("\"type\":\"http\""));
        assert!(cfg.contains(&handle.url));
        assert!(cfg.contains(&format!("Bearer {}", handle.token)));
    }
}
