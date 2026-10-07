//! `probe-driver` — the scripted harness each conformance case runs behind.
//!
//! # What this stands in for
//!
//! `mur run` with `inference.transport: process` loads the manifest's process driver, stands up
//! the tool bridge (a loopback JSON-RPC server advertising the capsule's declared tools, including
//! every `capabilities.shell.allow` binary), and spawns the harness binary with the environment
//! the driver's launch plan sets. Every generated manifest names `escape-conformance-driver` as
//! the driver and this binary as `inference.command`. A subscription CLI would decide *which* tool
//! to call by asking a model. This binary decides by reading the case out of its environment.
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
//! The harness environment starts empty and holds only what the launch plan sets, named in
//! [`escape_conformance::harness_protocol`]:
//!
//! | variable | meaning |
//! |---|---|
//! | `ENV_BRIDGE_URL` | the bridge's `http://127.0.0.1:<port>/<path>` URL |
//! | `ENV_BRIDGE_TOKEN` | the bearer token every bridge request carries |
//! | `ENV_PROBE` | the case, as a [`ProbeConfig`] JSON object |
//!
//! `--version` as the first argument prints the version and exits before reading any of them.
//!
//! # Output
//!
//! stdout carries the `harness_protocol` lines the driver's `parse` reads: a `tool` line before
//! each call, its `result` line after, then `usage` and `end <summary>`. A failure of the probe
//! itself is one `fail` line in place of whatever had not been printed yet, which ends the run
//! with `E-RUN-033`. The exit status is always 0: the driver classifies an exit with no `end` or
//! `fail` line as a failure, so a non-zero status would only replace the probe's own account of
//! what went wrong with a stderr tail.

use std::env;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::Duration;

use escape_conformance::harness_protocol::{
    end_line, fail_line, result_line, tool_line, usage_line, ProbeConfig, ENV_BRIDGE_TOKEN,
    ENV_BRIDGE_URL, ENV_PROBE, PROBE_HARNESS_VERSION,
};

/// Bridge coordinates, read out of the environment the launch plan set.
struct Bridge {
    host: String,
    port: u16,
    path: String,
    token: String,
}

fn read_bridge() -> Result<Bridge, String> {
    let url = env::var(ENV_BRIDGE_URL)
        .map_err(|_| format!("{ENV_BRIDGE_URL} is not set; was this spawned by `mur run`?"))?;
    let token = env::var(ENV_BRIDGE_TOKEN).map_err(|_| format!("{ENV_BRIDGE_TOKEN} is not set"))?;

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
        token,
    })
}

fn read_config() -> Result<ProbeConfig, String> {
    let text =
        env::var(ENV_PROBE).map_err(|_| format!("{ENV_PROBE} is not set; nothing to run"))?;
    ProbeConfig::from_json(&text)
}

/// One JSON-RPC round trip. The bridge answers `connection: close`, so each call gets its own
/// socket — which is exactly what it expects.
fn call(bridge: &Bridge, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let payload = body.to_string();
    let request = format!(
        "POST {} HTTP/1.1\r\nhost: {}:{}\r\nauthorization: Bearer {}\r\ncontent-type: application/json\r\n\
         accept: application/json, text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        bridge.path,
        bridge.host,
        bridge.port,
        bridge.token,
        payload.len(),
        payload
    );

    let mut stream = TcpStream::connect((bridge.host.as_str(), bridge.port))
        .map_err(|err| format!("could not reach the bridge: {err}"))?;
    // Generous: a boundary case's tool call is a whole capsule subprocess doing real work, and a
    // resource-exhaustion case is trying to hit a ceiling first.
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

/// One line on stdout. A write error is ignored: the driver then sees no `end` line and
/// classifies the exit as a failure, which is the truthful outcome.
fn emit(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn run(config: &ProbeConfig) -> Result<String, String> {
    let bridge = read_bridge()?;
    let ProbeConfig {
        tool, case, script, ..
    } = config;

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
    let first = tool_call(&bridge, 2, "c1", tool, script)?;
    summary.push_str(&format!(" :: {first}"));

    // An optional second call, for the one case that needs a *later* spawn to exist. The workdir
    // ceiling is not a session kill: `ShellEnforcement::check_workdir_budget` refuses the **next**
    // subprocess after the periodic check latches a breach, so a case that makes a single tool
    // call gives the latch nothing to refuse and cannot observe the mechanism at all.
    if let Some(second) = &config.script2 {
        let result = tool_call(&bridge, 3, "c2", tool, second)?;
        summary.push_str(&format!(" || SECOND-CALL {result}"));
    }
    Ok(summary)
}

/// One `tools/call`, reported on stdout as a `tool`/`result` pair under `call_id` and returned
/// rendered as `isError=<bool> :: <text>` — the form the runner grades on.
fn tool_call(
    bridge: &Bridge,
    rpc_id: u32,
    call_id: &str,
    tool: &str,
    script: &str,
) -> Result<String, String> {
    let arguments = serde_json::json!({ "command": script });
    emit(&tool_line(call_id, tool, &arguments.to_string()));
    let result = call(
        bridge,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": rpc_id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
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

    emit(&result_line(call_id, is_error, text));
    Ok(format!("isError={is_error} :: {}", text.replace('\n', " ")))
}

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some("--version") {
        println!("escape-conformance-probe {PROBE_HARNESS_VERSION}");
        return ExitCode::SUCCESS;
    }

    let config = match read_config() {
        Ok(config) => config,
        Err(err) => {
            let message = format!("probe-driver failed: {err}");
            eprintln!("[probe-driver] {message}");
            emit(&fail_line(&message));
            return ExitCode::SUCCESS;
        }
    };

    let outcome = run(&config);
    let summary = match &outcome {
        Ok(summary) => summary.clone(),
        Err(err) => format!("probe-driver failed: {err}"),
    };
    eprintln!("[probe-driver] {summary}");
    // When a tool call is refused before it runs — a cgroup join that cannot happen, an execve
    // the sandbox denies — that refusal text is the only explanation for a missing probe file.
    // The runner reads this file and folds it into the case's DETAIL, and grades the
    // `SecondSpawnRefused` and `ShellExit` cases on it.
    let _ = std::fs::write(&config.log, format!("{summary}\n"));

    match outcome {
        Ok(summary) => {
            emit(&usage_line());
            emit(&end_line(&summary));
        }
        Err(_) => emit(&fail_line(&summary)),
    }
    ExitCode::SUCCESS
}
