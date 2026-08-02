//! `probe-driver` — the scripted inference CLI each conformance case runs behind.
//!
//! # What this stands in for
//!
//! `mur run` with `inference.transport: process` spawns whatever `inference.command` names and
//! drives it as a self-contained agent: it stands up the Claude Bridge (a loopback JSON-RPC tool
//! server advertising the capsule's declared tools, including every
//! `capabilities.shell.allow` binary), points the CLI at it with `--mcp-config`, and reads
//! JSON-lines back on stdout. A subscription CLI would decide *which* tool to call by asking a
//! model. This binary decides by reading two environment variables.
//!
//! Everything downstream of that decision is untouched: the bridge dispatches through
//! `CapsuleStoreState::dispatch_agent_tool_async`, which is the same executor the HTTP transport
//! uses, so the tool runs under the capsule's declared capabilities, inside the real Landlock
//! ruleset and the real seccomp filter, and lands in the same trace. Only the choice is scripted.
//!
//! # Why that is the right trade for a release gate
//!
//! A gate whose verdicts depend on a model choosing to run the exact command it was handed is
//! flaky in the one direction that matters. A case the model skipped produces no probe file, and
//! "no evidence" must never be read as "contained" — so a flaky driver would convert model
//! variance directly into false assurance about a security property. Scripting the call also
//! means the suite needs no API key and makes no network request of its own, so anyone with the
//! repository and a Linux host can run it.
//!
//! # Inputs
//!
//! | variable | meaning |
//! |---|---|
//! | `MURMUR_EC_TOOL` | tool to invoke; the harness always sets `python3` |
//! | `MURMUR_EC_SCRIPT` | the tool's `command` argument — the staged probe's filename |
//! | `MURMUR_EC_CASE` | case id, echoed into the result line for legibility only |
//!
//! Everything else it needs — the bridge URL and its per-run bearer token — arrives in argv,
//! inside the `--mcp-config` JSON.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

/// Bridge coordinates, lifted out of the `--mcp-config` JSON `mur run` passes in argv.
struct Bridge {
    host: String,
    port: u16,
    path: String,
    authorization: String,
}

fn parse_bridge(args: &[String]) -> Result<Bridge, String> {
    let index = args
        .iter()
        .position(|arg| arg == "--mcp-config")
        .ok_or("no --mcp-config in argv; was this spawned by `mur run`?")?;
    let raw = args
        .get(index + 1)
        .ok_or("--mcp-config had no value")?;
    let config: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| format!("--mcp-config is not JSON: {err}"))?;

    // `mur run` names the server `claude_bridge`, but read whichever single server is configured
    // rather than hardcoding the key — the name is an internal detail of the process transport.
    let servers = config
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .ok_or("--mcp-config has no mcpServers object")?;
    let server = servers
        .values()
        .next()
        .ok_or("--mcp-config declares no server")?;

    let url = server
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("the bridge server has no url")?;
    let authorization = server
        .get("headers")
        .and_then(|h| h.get("Authorization"))
        .and_then(|v| v.as_str())
        .ok_or("the bridge server has no Authorization header")?
        .to_string();

    // `http://127.0.0.1:PORT/path` — parsed by hand rather than with a URL crate, since the
    // shape is fixed by `claude_bridge::bind_bridge` and this keeps the dependency set at one.
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("bridge url is not plain http: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], rest[at..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| format!("bridge authority has no port: {authority}"))?;

    Ok(Bridge {
        host: host.to_string(),
        port: port
            .parse()
            .map_err(|_| format!("bridge port is not a number: {port}"))?,
        path,
        authorization,
    })
}

/// One JSON-RPC round trip. The bridge answers `connection: close`, so each call gets its own
/// socket — which is exactly what it expects.
fn call(bridge: &Bridge, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let payload = body.to_string();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: {}:{}\r\nauthorization: {}\r\ncontent-type: application/json\r\n\
         accept: application/json, text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        bridge.path,
        bridge.host,
        bridge.port,
        bridge.authorization,
        payload.len(),
        payload
    );

    let mut stream = TcpStream::connect((bridge.host.as_str(), bridge.port))
        .map_err(|err| format!("could not reach the bridge: {err}"))?;
    // Generous: a boundary case's tool call is a whole capsule subprocess doing real work, and a
    // resource-exhaustion case is deliberately trying to hit a ceiling first.
    stream
        .set_read_timeout(Some(Duration::from_secs(900)))
        .map_err(|err| format!("could not set a read timeout: {err}"))?;
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("could not send to the bridge: {err}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| format!("could not read the bridge's reply: {err}"))?;
    let text = String::from_utf8_lossy(&response);
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or("the bridge's reply had no body")?;
    serde_json::from_str(body).map_err(|err| format!("the bridge's reply is not JSON: {err}"))
}

fn run() -> Result<String, String> {
    let args: Vec<String> = env::args().collect();
    let bridge = parse_bridge(&args)?;

    let tool = env::var("MURMUR_EC_TOOL").unwrap_or_else(|_| "python3".to_string());
    let script = env::var("MURMUR_EC_SCRIPT")
        .map_err(|_| "MURMUR_EC_SCRIPT is not set; nothing to run".to_string())?;
    let case = env::var("MURMUR_EC_CASE").unwrap_or_else(|_| "unnamed".to_string());

    // Protocol handshake. The bridge answers `initialize` for any protocol version it is handed
    // and does not require the follow-up notification, but sending it keeps this an ordinary
    // client rather than one that happens to work.
    call(
        &bridge,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "murmur-escape-conformance-probe-driver", "version": "0"}
            }
        }),
    )?;

    // `dispatch_shell_tool` reads its command out of the arguments object's `command` key; for a
    // non-interpreter binary such as `python3` the value is split into argv words, so the staged
    // probe's filename is the whole argument list.
    let mut summary = format!("case={case} tool={tool}");
    let first = tool_call(&bridge, 2, &tool, &script)?;
    summary.push_str(&format!(" :: {first}"));

    // An optional second call, for the one case that needs a *later* spawn to exist. The workdir
    // ceiling is not a session kill: `ShellEnforcement::check_workdir_budget` refuses the **next**
    // subprocess after the periodic check latches a breach, so a case that makes a single tool
    // call gives the latch nothing to refuse and cannot observe the mechanism at all.
    if let Ok(second) = env::var("MURMUR_EC_SCRIPT2") {
        if !second.is_empty() {
            let result = tool_call(&bridge, 3, &tool, &second)?;
            summary.push_str(&format!(" || SECOND-CALL {result}"));
        }
    }
    Ok(summary)
}

/// One `tools/call`, rendered as `isError=<bool> :: <text>`.
fn tool_call(bridge: &Bridge, id: u32, tool: &str, script: &str) -> Result<String, String> {
    let result = call(
        bridge,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": { "command": script } }
        }),
    )?;

    let text = result
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("<the bridge returned no text>");
    let is_error = result
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(format!("isError={is_error} :: {}", text.replace('\n', " ")))
}

fn main() -> ExitCode {
    // `mur run`'s process loop reads JSON-lines on stdout and wants a terminal `result` record.
    // The harness never grades a case on this line — the probe file and the trace are the
    // evidence — but emitting a well-formed one keeps a clean case from ending in `E-RUN-007`
    // and burying the real result under a spurious agent-loop failure.
    let (summary, is_error) = match run() {
        Ok(summary) => (summary, false),
        Err(err) => (format!("probe-driver failed: {err}"), true),
    };
    eprintln!("[probe-driver] {summary}");
    // `mur run` drains the inference subprocess's stderr into a bounded buffer and only surfaces
    // it when the agent loop itself fails, so on a *successful* session the line above is
    // invisible. When a tool call is refused before it runs — a cgroup join that cannot happen,
    // an execve the supervisor denies — that refusal text is the only explanation for a missing
    // probe file, and losing it turns a diagnosable infrastructure fault into an unexplained
    // INCONCLUSIVE. The runner sets this path and folds the contents into the case's DETAIL.
    if let Ok(path) = env::var("MURMUR_EC_DRIVER_LOG") {
        let _ = std::fs::write(&path, format!("{summary}\n"));
    }

    let line = serde_json::json!({
        "type": "result",
        "subtype": if is_error { "error_during_execution" } else { "success" },
        "is_error": is_error,
        "result": summary,
    });
    println!("{line}");
    let _ = std::io::stdout().flush();

    // Exit 0 even on failure: a non-zero exit makes `mur run` report a spawn problem, which would
    // mask whatever the case actually observed. The failure is on stderr and in the result line.
    ExitCode::SUCCESS
}
