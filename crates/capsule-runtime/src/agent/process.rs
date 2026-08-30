//! Process transport for `transport: process` inference config.
//!
//! # Protocol (determined by direct inspection of `claude --help` and live testing)
//!
//! **Model:** Single-process-per-agent-run. Claude is spawned once; a single user message is
//! written as a JSON-line to stdin; stdin is closed; stdout is read line-by-line until the
//! `{"type":"result"}` event; the process exits naturally.
//!
//! **Flags used:**
//!   `--print`                    — non-interactive output mode
//!   `--output-format stream-json` — one JSON event per line on stdout
//!   `--verbose`                  — include system/metadata events
//!   `--input-format stream-json` — accept JSON-line messages on stdin
//!   `--model <model>`            — target model
//!   `--tools <names>`            — the bridge's `mcp__claude_bridge__<tool>` names when a bridge
//!                                  is bound; `--tools ""` when the capsule declares no tools, so
//!                                  claude's built-in tools are off either way
//!   `--mcp-config <json>`        — bridge server definition, with `--strict-mcp-config` so the
//!                                  bridge is the only tool server the CLI loads (bridge only)
//!   `--permission-mode bypassPermissions` — auto-approve bridge tool calls; `--print` cannot
//!                                  prompt for approval (bridge only)
//!   `--system-prompt <text>`     — inject system prompt when configured
//!
//! **Stdin message format:**
//!   `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<task>"}]}}`
//!
//! **Stdout event stream:**
//!   `{"type":"system","subtype":"init",...}`      — session header (ignored)
//!   `{"type":"assistant","message":{...}}`        — LLM response; content may include tool_use
//!   `{"type":"user","message":{...}}`             — tool result (claude executed internally)
//!   `{"type":"result","subtype":"success","result":"..."}`  — final result; signals end-of-turn
//!   `{"type":"result","subtype":"error_during_execution",...}` — terminal error
//!
//! **Turn counting:** each `{"type":"assistant"}` event = one LLM inference call = one turn.
//!
//! **Tool calls:** a capsule that declares tool artifacts gets a `claude_bridge` server bound on
//! loopback, and the CLI is pointed at it as its only tool server, restricted to the bridge's
//! `mcp__claude_bridge__<tool>` names with its own built-in tools off. Murmur executes each call
//! through the same `CapsuleStoreState` dispatch the `transport: http` path uses, under the
//! capsule's declared capabilities. Tool_use and tool_result events carry the qualified CLI names;
//! `strip_bridge_prefix` maps them back to the artifact tool names. A capsule that declares no
//! tools gets plain inference with the CLI's built-in tools disabled.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use murmur_artifact::InferenceConfig;
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

use crate::{
    agent::{AgentLoopExit, UNTRUSTED_CONTENT_NOTICE},
    errors::RuntimeError,
    hooks::{HookEvent, HookRuntime},
    murmur_md::MURMUR_MD_TRUST_NOTICE,
    otel::OtelEmitter,
    runtime::CapsuleStoreState,
    streaming::{SseBroadcast, SseEventBuffer},
    trace::TraceWriter,
};

use super::{claude_bridge, inventory::build_tool_inventory};

/// Loopback address the Claude Bridge binds on. Always host-local, independent of the
/// capsule server's bind address — the bridge is only ever reached by the local CLI
/// subprocess. Process-transport only.
const BRIDGE_BIND_ADDR: &str = "127.0.0.1";

/// Codex host-execution feature flags disabled on every `codex exec` run so that **murmur is
/// the sole tool executor** — the model can only reach the Claude Bridge (MCP) tools, never
/// Codex's own shell/exec/code tools. Verified against codex-cli 0.144.6 (`codex features list`).
/// Version-coupled: codex renames/removes these between releases (e.g. `js_repl` was removed by
/// 0.144, `code_mode_host` added), and disabling a non-existent flag is a hard error — so this
/// list must be revisited when the supported codex version changes. `-s read-only` is layered on
/// top as defence-in-depth (blocks file mutation from anything not covered here). Codex dialect
/// only; has no bearing on the Claude path.
const CODEX_DISABLED_FEATURES: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "shell_snapshot",
    "code_mode_host",
];

/// Prepended to the codex prompt when the Claude Bridge is active. Codex, unlike Claude, is
/// reluctant to use MCP tools for work it associates with its (now-disabled) native tools — with
/// a vague task it reasons "I can't write files, the sandbox is read-only" and gives up instead of
/// reaching for the equivalent bridge tool. This steers it to treat the provided MCP tools as its
/// only capabilities. Codex dialect only.
const CODEX_TOOL_GUIDANCE: &str = "\
You have NO shell, file, or built-in tools. Your ONLY capabilities are the MCP tools provided to \
you. For any file operation (read, write, edit, search) or other action, you MUST call the \
matching MCP tool — do not attempt native file access or report that the workspace is read-only. \
Inspect the available tools and use them to complete the task.";

/// Which CLI wire protocol the process transport speaks. Selected from `inference.command`:
/// `codex` → the codex-exec dialect, anything else → the Claude Code dialect (the default and
/// original path). The two CLIs diverge in flags, how the task is delivered, and their stdout
/// event streams, but share the Claude Bridge and the WASM tool executor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessDialect {
    Claude,
    Codex,
}

impl ProcessDialect {
    fn from_command(command: &str) -> Self {
        let base = Path::new(command)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(command);
        if base.eq_ignore_ascii_case("codex") {
            Self::Codex
        } else {
            Self::Claude
        }
    }
}

/// Overall wall-clock timeout for one claude subprocess run (all turns + tool calls).
/// Prevents the agent loop from hanging indefinitely if claude stalls.
const PROCESS_TIMEOUT_SECS: u64 = 600; // 10 minutes

/// Max bytes of subprocess stderr retained for error diagnostics. The subprocess's
/// stderr is otherwise invisible to the user; when a spawn produces no result (e.g. the
/// CLI rejected an argument and exited), its tail is folded into the RuntimeError so the
/// failure is legible instead of a bare "closed stdout without producing a result".
const STDERR_TAIL_CAP: usize = 4096;

/// Formats a trailing "subprocess stderr" block for error messages, or "" when the
/// subprocess wrote nothing to stderr. Best-effort: the concurrent drain task may not have
/// flushed the final bytes, but for the common "CLI rejected a flag and exited" case the
/// process has already terminated and its stderr is fully captured by the time we format.
fn format_stderr_tail(buf: &Arc<Mutex<String>>) -> String {
    let captured = buf.lock().map(|g| g.clone()).unwrap_or_default();
    let trimmed = captured.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("\n--- subprocess stderr (tail) ---\n{trimmed}")
    }
}

/// Read `task.md` from the workdir, returning empty string if absent.
fn read_task_from_workdir(workdir: &Path) -> String {
    std::fs::read_to_string(workdir.join("task.md")).unwrap_or_default()
}

/// Builds the `--system-prompt` argument value for the claude subprocess. Always carries
/// `MURMUR_MD_TRUST_NOTICE` and `UNTRUSTED_CONTENT_NOTICE`, even when no `inference.system_prompt`
/// is configured, so the subprocess never receives MURMUR.md-adjacent context or runs tools
/// without the not-instructions / untrusted-content notices.
///
/// It carries no `[Capsule]` block, unlike the http transport's
/// `agent::build_augmented_system_prompt`: on this transport murmur does not render the system
/// prompt at all — it hands the CLI one argument and the CLI frames everything around it, so
/// the two unconditional notices are the only part that is murmur's to inject. That is why
/// `run_process_inference_loop` takes the capsule identity as `_name` / `_version` and ignores
/// both. The value must nonetheless stay launch-invariant, for the same prompt caching reason
/// as the http path: the CLI puts it at the head of the prompt, and every provider matches its
/// cache on an exact prefix, so a per-launch value here would miss the cache on every request.
pub(super) fn build_process_system_prompt(system_prompt: Option<&str>) -> String {
    let notices = format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}");
    match system_prompt.filter(|sp| !sp.is_empty()) {
        Some(sp) => format!("{notices}\n\n{sp}"),
        None => notices,
    }
}

/// Build the CLI argv for the selected dialect. Both dialects carry the murmur system prompt
/// (trust/untrusted notices) and, when the Claude Bridge is active, the flags that point the CLI
/// at it while disabling the CLI's own host tools — but they diverge in every specific.
fn build_process_args(
    dialect: ProcessDialect,
    inference: &InferenceConfig,
    bridge: &Option<claude_bridge::BridgeHandle>,
    system_prompt: Option<&str>,
    task: &str,
) -> Vec<String> {
    let system = build_process_system_prompt(system_prompt);
    match dialect {
        ProcessDialect::Claude => {
            let mut args: Vec<String> = vec![
                "--print".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--input-format".into(),
                "stream-json".into(),
            ];
            // model is optional: omit `--model` when empty so claude uses its CLI-configured
            // default. Passing `--model ""` is a hard 400 from the API, so it must be absent, not
            // empty (same reason codex omits `-m`).
            if !inference.model.trim().is_empty() {
                args.push("--model".into());
                args.push(inference.model.clone());
            }
            match bridge {
                // Restrict the CLI to exactly the bridge tools (its own host tools stay off),
                // point it at the bridge, forbid any other tool server, and auto-approve
                // (--print cannot prompt). The task arrives separately on stdin.
                Some(b) => {
                    args.push("--tools".into());
                    args.push(b.allowed_tool_names.join(","));
                    args.push("--mcp-config".into());
                    args.push(b.mcp_config_json());
                    args.push("--strict-mcp-config".into());
                    args.push("--permission-mode".into());
                    args.push("bypassPermissions".into());
                }
                // No tools declared → pure inference, unchanged from the original process path.
                None => {
                    args.push("--tools".into());
                    args.push(String::new()); // disable all built-in claude tools
                }
            }
            args.push("--system-prompt".into());
            args.push(system);
            args
        }
        ProcessDialect::Codex => {
            // codex exec: JSON event stream, non-git dirs allowed, read-only sandbox, and the
            // host-execution features disabled so murmur (via the bridge) is the sole executor.
            // codex has no `--system-prompt`, so the notices are prepended to the positional
            // prompt. The model is `-m`; omitted when empty so the account default is used.
            let mut args: Vec<String> = vec![
                "exec".into(),
                "--json".into(),
                "--skip-git-repo-check".into(),
                "-s".into(),
                "read-only".into(),
                // apply_patch is codex's built-in file editor — not a `--disable` feature but a
                // config toggle. Off, so codex can't edit files itself and instead routes file
                // work to the bridge's tools (otherwise codex prefers apply_patch for file ops
                // and its writes are silently blocked by the read-only sandbox).
                "-c".into(),
                "include_apply_patch_tool=false".into(),
            ];
            for feature in CODEX_DISABLED_FEATURES {
                args.push("--disable".into());
                args.push((*feature).into());
            }
            if !inference.model.trim().is_empty() {
                args.push("-m".into());
                args.push(inference.model.clone());
            }
            // Register the bridge as an MCP server for this run (only when tools are declared),
            // and steer codex toward those tools (it otherwise under-uses them — see the const).
            let guidance = if let Some(b) = bridge {
                args.extend(b.codex_config_args());
                format!("{CODEX_TOOL_GUIDANCE}\n\n")
            } else {
                String::new()
            };
            // Positional prompt = system notices + (tool guidance) + task.
            args.push(format!("{system}\n\n{guidance}{task}"));
            args
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_process_inference_loop(
    // `store_state` (shared &) is process-transport-specific: it lets the Claude Bridge
    // execute declared tool artifacts through the same dispatch the HTTP path uses. When the
    // capsule declares no tools it is unused beyond building an empty inventory.
    store_state: &CapsuleStoreState,
    workdir: &Path,
    inference: &InferenceConfig,
    system_prompt: Option<String>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    task_id: Option<String>,
    _sse: Option<(SseBroadcast, Arc<Mutex<SseEventBuffer>>)>,
    accessible_workdir: &Path,
    _name: &str,
    _version: &str,
) -> Result<AgentLoopExit, RuntimeError> {
    let command_name = inference
        .command
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("claude");

    let max_turns = inference.max_turns;

    // Claude Bridge (process transport only): if the capsule declares tool artifacts, stand
    // up a loopback tool server so the CLI can call them natively while murmur executes them.
    // Returns None when there are no tools, in which case the CLI runs as pure inference
    // exactly as before. See agent/claude_bridge.rs for the full rationale.
    let inventory = build_tool_inventory(workdir, inference.system_prompt_artifact.as_deref());
    let bridge = claude_bridge::bind_bridge(BRIDGE_BIND_ADDR, &inventory).await;

    // The trace's `session_start`/`session_end` frame and the `on-session-start`/
    // `on-session-end` hook dispatch both fire once per launch, from runtime.rs around the
    // task loop. This function runs one attempt of one task and writes neither.
    otel.begin_session(None);

    // Same fix as the http-transport path in agent.rs: task.md lives in accessible_workdir
    // (where the agent's own tools are preopened), not the internal session workdir.
    // Fenced on the same condition as the http path, from the same function, so the transport
    // a capsule runs on does not decide whether an untrusted payload is marked.
    let task = super::fence_task_payload(
        store_state.current_task_provenance,
        read_task_from_workdir(accessible_workdir),
    );

    // Build subprocess arguments for the selected CLI dialect. Claude takes the task on stdin
    // as a JSON message; codex takes it as a positional prompt arg (stdin stays closed).
    let dialect = ProcessDialect::from_command(command_name);
    let args = build_process_args(dialect, inference, &bridge, system_prompt.as_deref(), &task);

    let child_stdin = match dialect {
        ProcessDialect::Claude => std::process::Stdio::piped(),
        // codex reads the prompt from argv; a piped-but-unwritten stdin makes it hang waiting
        // for "additional input", so close it outright.
        ProcessDialect::Codex => std::process::Stdio::null(),
    };
    let install_hint = match dialect {
        ProcessDialect::Claude => "install the claude CLI from https://claude.ai/download",
        ProcessDialect::Codex => {
            "install the codex CLI from https://developers.openai.com/codex/cli"
        }
    };
    let mut child = Command::new(command_name)
        .args(&args)
        .stdin(child_stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::AgentLoopFailed(format!(
                    "'{command_name}': binary not found on PATH\n\
                     hint: {install_hint}"
                ))
            } else {
                RuntimeError::AgentLoopFailed(format!("failed to spawn '{command_name}': {e}"))
            }
        })?;

    // Drain the subprocess's stderr concurrently into a bounded buffer. Reading it on a
    // separate task (rather than after stdout) avoids a pipe-buffer deadlock if the CLI
    // writes a large diagnostic to stderr before exiting, and gives us its tail to fold
    // into any failure below.
    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let buf = Arc::clone(&stderr_buf);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut b) = buf.lock() {
                    b.push_str(&line);
                    b.push('\n');
                    // Trim whole leading lines to stay under the cap without splitting a
                    // UTF-8 char (newlines are ASCII, so slicing after one is always safe).
                    while b.len() > STDERR_TAIL_CAP {
                        match b.find('\n') {
                            Some(nl) => *b = b[nl + 1..].to_string(),
                            None => break,
                        }
                    }
                }
            }
        });
    }

    // Claude receives the task as a JSON-line user message on stdin (codex already has it on
    // argv and its stdin was closed at spawn).
    if dialect == ProcessDialect::Claude {
        let user_msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": task}]
            }
        });
        let mut stdin = child.stdin.take().expect("stdin should be piped");
        let line = format!("{}\n", serde_json::to_string(&user_msg).unwrap_or_default());
        stdin.write_all(line.as_bytes()).await.map_err(|e| {
            RuntimeError::AgentLoopFailed(format!("failed to write to subprocess stdin: {e}"))
        })?;
        // drop closes stdin → signals EOF to the subprocess
    }

    // Read stdout with an overall timeout. When the Claude Bridge is active, serve it
    // concurrently on the same task: tool dispatch is `&self`, connections are served one at a
    // time, and the read loop borrows disjoint state (child/hooks/trace), so a shared
    // `&CapsuleStoreState` is all the bridge needs. `select!` drops the never-completing bridge
    // future the moment the CLI produces its result. Each dialect parses its own stdout stream
    // but both drive the same bridge and share the timeout/kill/session-end plumbing below.
    let result = tokio::time::timeout(Duration::from_secs(PROCESS_TIMEOUT_SECS), async {
        // The reader future depends on the dialect; the bridge future is identical.
        let read = async {
            match dialect {
                ProcessDialect::Claude => {
                    read_process_output(
                        &mut child,
                        workdir,
                        inference,
                        max_turns,
                        task_id.as_deref(),
                        hooks,
                        trace,
                        otel,
                        &stderr_buf,
                    )
                    .await
                }
                ProcessDialect::Codex => {
                    read_codex_output(
                        &mut child,
                        workdir,
                        max_turns,
                        hooks,
                        trace,
                        otel,
                        &stderr_buf,
                    )
                    .await
                }
            }
        };
        match &bridge {
            Some(b) => {
                tokio::select! {
                    r = read => r,
                    () = b.serve(store_state) => Err(RuntimeError::AgentLoopFailed(
                        "Claude Bridge server exited unexpectedly".into(),
                    )),
                }
            }
            None => read.await,
        }
    })
    .await;

    // Kill the subprocess regardless of how we exit.
    let _ = child.kill().await;

    match result {
        Err(_elapsed) => {
            otel.emit_session_end("failed").await;
            Err(RuntimeError::AgentLoopFailed(format!(
                "inference subprocess timed out after {PROCESS_TIMEOUT_SECS}s{}",
                format_stderr_tail(&stderr_buf)
            )))
        }
        Ok(inner) => inner,
    }
}

/// A capsule tool call seen in an `assistant` event, awaiting its matching `tool_result` so a
/// `tool_call` trace event can be written with the real status/duration. Process transport only:
/// bridge tools execute out-of-band (in the Claude Bridge), so the read loop reconstructs the
/// trace record by pairing the CLI's tool_use → tool_result events.
struct PendingToolCall {
    name: String,
    input: Value,
    input_bytes: u64,
    turn: u32,
    started: Instant,
}

/// Strip the Claude Bridge namespace (`mcp__claude_bridge__`) the CLI prepends to bridge tool
/// names, so the trace shows the bare artifact name (e.g. `murmur-tool-editor`) — matching how
/// the http transport records it. Non-bridge names pass through unchanged.
fn strip_bridge_prefix(name: &str) -> &str {
    // Kept in sync with claude_bridge::BRIDGE_SERVER_KEY.
    name.strip_prefix(&format!("mcp__{}__", claude_bridge::BRIDGE_SERVER_KEY))
        .unwrap_or(name)
}

/// Extract a `tool_result` block's textual content, whether the CLI sends it as a bare string
/// or as an array of `{type:"text",text}` blocks.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// True when an assistant event carries only reasoning (`thinking` / `redacted_thinking`) and
/// no actionable block (`tool_use` / `text`). The process CLI emits extended-thinking as its
/// own standalone assistant events; these are not logical turns and are skipped so process
/// `max_turns` aligns with the http transport. Empty/absent content counts as thinking-only
/// (nothing actionable to count). Process transport only.
fn is_thinking_only(content: Option<&Value>) -> bool {
    match content.and_then(Value::as_array) {
        Some(arr) if !arr.is_empty() => arr.iter().all(|b| {
            matches!(
                b.get("type").and_then(Value::as_str),
                Some("thinking") | Some("redacted_thinking")
            )
        }),
        _ => true,
    }
}

/// Read and process JSON-line events from the subprocess stdout.
// Decoding the stdout stream is what feeds every session-wide sink, so this takes the same set
// its caller `run_process_inference_loop` was handed — hooks, trace, otel, stderr tail — rather
// than a struct that exists only to carry them one level down.
#[allow(clippy::too_many_arguments)]
async fn read_process_output(
    child: &mut tokio::process::Child,
    workdir: &Path,
    _inference: &InferenceConfig,
    max_turns: u32,
    task_id: Option<&str>,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    stderr_buf: &Arc<Mutex<String>>,
) -> Result<AgentLoopExit, RuntimeError> {
    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut lines = BufReader::new(stdout).lines();

    let mut turns: u32 = 0;
    let mut result_text = String::new();
    let mut found_result = false;
    // Bridge tools execute out-of-band, so pair each assistant `tool_use` with its later
    // `tool_result` to emit a `tool_call` trace event (what `mur trace show` counts). Keyed by
    // tool_use id. Process transport only.
    let mut pending_tool_calls: HashMap<String, PendingToolCall> = HashMap::new();

    while let Some(line) = lines.next_line().await.map_err(|e| {
        RuntimeError::AgentLoopFailed(format!("failed to read subprocess stdout: {e}"))
    })? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let event: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                return Err(RuntimeError::AgentLoopFailed(format!(
                    "malformed JSON on subprocess stdout: {e}: {line}{}",
                    format_stderr_tail(stderr_buf)
                )));
            }
        };

        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "assistant" => {
                let content = event.get("message").and_then(|m| m.get("content"));

                // The CLI streams the model's extended-thinking blocks as their own assistant
                // events, separate from the event carrying the actual action (tool_use / text).
                // In transport: http one turn is one inference (thinking + action together), so
                // counting these standalone thinking events would inflate the turn count — a
                // single tool call would burn several `max_turns`, and even a plain reply would
                // cost two. Skip them entirely (no turn increment, no trace/inference event) so
                // process `max_turns` counts the same logical turns as http. Process transport
                // only; the CLI's turn loop is what produces these split events.
                if is_thinking_only(content) {
                    continue;
                }

                turns += 1;
                if turns > max_turns {
                    otel.emit_session_end("failed").await;
                    return Err(RuntimeError::AgentLoopFailed(format!(
                        "max_turns ({max_turns}) exceeded"
                    )));
                }

                let turn_idx = turns - 1;
                let decision = if content
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
                    })
                    .unwrap_or(false)
                {
                    "tool_call"
                } else {
                    "end_turn"
                };

                let _ = trace
                    .write_inference(
                        turn_idx,
                        0,
                        0,
                        decision.to_string(),
                        None,
                        None,
                        None,
                        // The CLI owns its own conversation; the runtime minted no message
                        // for this request and has no id to name — and it built no payload to
                        // hash either.
                        Vec::new(),
                        None,
                    )
                    .await;

                otel.emit_inference(turn_idx, 0, 0, decision, None, 0, None, None)
                    .await;

                hooks
                    .emit(
                        workdir,
                        HookEvent::Inference {
                            turn: turn_idx,
                            input_tokens: 0,
                            output_tokens: 0,
                            decision: decision.to_string(),
                            tool_name: None,
                            prompt: None,
                            output: content.map(|c| c.to_string()),
                            tools: None,
                        },
                    )
                    .await;

                // Record any tool_use blocks so the matching tool_result can be traced as a
                // `tool_call` event (bridge tools run out-of-band, so we reconstruct the record
                // here). Process transport only.
                if let Some(blocks) = content.and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let Some(id) = block.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        let name = strip_bridge_prefix(
                            block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("<unknown>"),
                        )
                        .to_string();
                        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                        let input_bytes = serde_json::to_string(&input)
                            .map(|s| s.len() as u64)
                            .unwrap_or(0);
                        pending_tool_calls.insert(
                            id.to_string(),
                            PendingToolCall {
                                name,
                                input,
                                input_bytes,
                                turn: turn_idx,
                                started: Instant::now(),
                            },
                        );
                    }
                }

                if let Some(tid) = task_id {
                    // Emit working status for A2A clients.
                    let _ = tid; // used for future SSE emission
                }
            }

            "user" => {
                // Tool result event. Pair it with the pending tool_use to emit a `tool_call`
                // trace event (what `mur trace show` counts) plus the ToolCall hook, with the
                // real tool name, byte sizes, duration, and status. Process transport only.
                let content = event
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array);
                if let Some(blocks) = content {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                            continue;
                        };
                        let Some(pending) = pending_tool_calls.remove(id) else {
                            continue; // result for a tool we didn't record (e.g. no id) — skip
                        };

                        let is_error = block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let status = if is_error { "error" } else { "ok" };
                        let output = tool_result_text(block);
                        let output_bytes = output.len() as u64;
                        let duration_ms = pending
                            .started
                            .elapsed()
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX);

                        let _ = trace
                            .write_tool_call(
                                pending.turn,
                                pending.name.clone(),
                                Some(id.to_string()),
                                pending.input.clone(),
                                pending.input_bytes,
                                &output,
                                output_bytes,
                                duration_ms,
                                status.to_string(),
                                None,
                                None,
                            )
                            .await;

                        hooks
                            .emit(
                                workdir,
                                HookEvent::ToolCall {
                                    turn: pending.turn,
                                    tool_name: pending.name,
                                    input_bytes: pending.input_bytes,
                                    output_bytes,
                                    duration_ms,
                                    status: status.to_string(),
                                },
                            )
                            .await;
                    }
                }
            }

            "result" => {
                let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
                if subtype == "success" {
                    result_text = event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    found_result = true;
                } else {
                    // Error result from claude.
                    let err_msg = event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or(subtype);
                    otel.emit_session_end("failed").await;
                    return Err(RuntimeError::AgentLoopFailed(format!(
                        "claude returned error: {err_msg}"
                    )));
                }
                break;
            }

            // Ignore: system/init, rate_limit_event, post_turn_summary, etc.
            _ => {}
        }
    }

    if !found_result {
        otel.emit_session_end("failed").await;
        return Err(RuntimeError::AgentLoopFailed(format!(
            "inference subprocess closed stdout without producing a result{}",
            format_stderr_tail(stderr_buf)
        )));
    }

    super::record_result(hooks, workdir, &result_text).map_err(RuntimeError::AgentLoopFailed)?;

    otel.emit_session_end("ok").await;

    Ok(AgentLoopExit::Ok)
}

/// Read and process `codex exec --json` events (the codex dialect of the process transport).
/// Codex's newline-delimited stream is entirely different from Claude's: `thread.started` /
/// `turn.started` / `item.started` / `item.completed` (each item a `reasoning`, `agent_message`,
/// or `mcp_tool_call`) / `turn.completed` / `turn.failed`. Bridge tools execute through murmur, and
/// each `mcp_tool_call` item carries the call *and* its result inline — so, unlike the Claude path,
/// no tool_use→tool_result pairing is needed. `reasoning` items are skipped for turn counting so
/// `max_turns` matches the http/Claude paths. The final `agent_message` text is the result.
/// Codex dialect only.
async fn read_codex_output(
    child: &mut tokio::process::Child,
    workdir: &Path,
    max_turns: u32,
    hooks: &mut HookRuntime,
    trace: &mut TraceWriter,
    otel: &mut OtelEmitter,
    stderr_buf: &Arc<Mutex<String>>,
) -> Result<AgentLoopExit, RuntimeError> {
    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut lines = BufReader::new(stdout).lines();

    let mut turns: u32 = 0;
    let mut result_text = String::new();
    let mut found_result = false;

    while let Some(line) = lines.next_line().await.map_err(|e| {
        RuntimeError::AgentLoopFailed(format!("failed to read subprocess stdout: {e}"))
    })? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        // codex occasionally prints non-JSON progress lines to stdout; skip them rather than
        // fail. A genuinely empty/broken stream still surfaces via the no-result error below.
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "item.completed" => {
                let Some(item) = event.get("item") else {
                    continue;
                };
                match item.get("type").and_then(Value::as_str).unwrap_or("") {
                    // Model reasoning — not a logical turn (mirrors the Claude thinking-skip).
                    "reasoning" => {}
                    "agent_message" => {
                        turns += 1;
                        if turns > max_turns {
                            return codex_max_turns_error(max_turns, otel).await;
                        }
                        result_text = item
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = trace
                            .write_inference(
                                turns - 1,
                                0,
                                0,
                                "end_turn".into(),
                                None,
                                None,
                                None,
                                Vec::new(),
                                None,
                            )
                            .await;
                        otel.emit_inference(turns - 1, 0, 0, "end_turn", None, 0, None, None)
                            .await;
                    }
                    "mcp_tool_call" => {
                        turns += 1;
                        if turns > max_turns {
                            return codex_max_turns_error(max_turns, otel).await;
                        }
                        let tool_name = item
                            .get("tool")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>")
                            .to_string();
                        let input = item.get("arguments").cloned().unwrap_or_else(|| json!({}));
                        let input_bytes = serde_json::to_string(&input)
                            .map(|s| s.len() as u64)
                            .unwrap_or(0);
                        let is_error = item.get("error").map(|e| !e.is_null()).unwrap_or(false);
                        let status = if is_error { "error" } else { "ok" };
                        let output = tool_result_text(item.get("result").unwrap_or(&Value::Null));
                        let output_bytes = output.len() as u64;

                        let _ = trace
                            .write_inference(
                                turns - 1,
                                0,
                                0,
                                "tool_call".into(),
                                Some(tool_name.clone()),
                                None,
                                None,
                                Vec::new(),
                                None,
                            )
                            .await;
                        otel.emit_inference(
                            turns - 1,
                            0,
                            0,
                            "tool_call",
                            Some(tool_name.as_str()),
                            0,
                            None,
                            None,
                        )
                        .await;
                        let _ = trace
                            .write_tool_call(
                                turns - 1,
                                tool_name.clone(),
                                // codex reports each `mcp_tool_call` item inline, call and
                                // result together, and names no id for the pair.
                                None,
                                input,
                                input_bytes,
                                &output,
                                output_bytes,
                                0,
                                status.to_string(),
                                None,
                                None,
                            )
                            .await;
                        hooks
                            .emit(
                                workdir,
                                HookEvent::ToolCall {
                                    turn: turns - 1,
                                    tool_name,
                                    input_bytes,
                                    output_bytes,
                                    duration_ms: 0,
                                    status: status.to_string(),
                                },
                            )
                            .await;
                    }
                    // A host command executed despite the disable set (see CODEX_DISABLED_FEATURES).
                    // Should not happen; record it so the trace shows murmur was NOT the sole
                    // executor rather than hiding the leak.
                    "command_execution" => {
                        let cmd = item.get("command").and_then(Value::as_str).unwrap_or("");
                        let status = if item.get("exit_code").and_then(Value::as_i64) == Some(0) {
                            "ok"
                        } else {
                            "error"
                        };
                        let _ = trace
                            .write_tool_call(
                                turns.saturating_sub(1),
                                format!("codex-shell: {cmd}"),
                                None,
                                json!({}),
                                0,
                                "",
                                0,
                                0,
                                status.to_string(),
                                None,
                                None,
                            )
                            .await;
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                found_result = true;
                break;
            }
            "turn.failed" | "error" => {
                let msg = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| event.get("message").and_then(Value::as_str))
                    .unwrap_or("codex turn failed");
                otel.emit_session_end("failed").await;
                return Err(RuntimeError::AgentLoopFailed(format!(
                    "codex returned error: {msg}{}",
                    format_stderr_tail(stderr_buf)
                )));
            }
            _ => {}
        }
    }

    if !found_result {
        otel.emit_session_end("failed").await;
        return Err(RuntimeError::AgentLoopFailed(format!(
            "inference subprocess closed stdout without producing a result{}",
            format_stderr_tail(stderr_buf)
        )));
    }

    super::record_result(hooks, workdir, &result_text).map_err(RuntimeError::AgentLoopFailed)?;
    otel.emit_session_end("ok").await;
    Ok(AgentLoopExit::Ok)
}

/// Shared max-turns failure path for the codex reader.
async fn codex_max_turns_error(
    max_turns: u32,
    otel: &mut OtelEmitter,
) -> Result<AgentLoopExit, RuntimeError> {
    otel.emit_session_end("failed").await;
    Err(RuntimeError::AgentLoopFailed(format!(
        "max_turns ({max_turns}) exceeded"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Launch-invariance: the `--system-prompt` value is a pure function of its argument, with
    /// no path, session id or other per-launch value anywhere in it, so two launches of the same
    /// capsule hand the CLI a byte-identical prefix. See the builder's own doc comment.
    #[test]
    fn process_system_prompt_is_launch_invariant() {
        for arg in [None, Some("You are a helpful assistant.")] {
            let first = build_process_system_prompt(arg);
            let second = build_process_system_prompt(arg);
            assert_eq!(first, second, "same input must yield the same string");
            assert!(first.contains(MURMUR_MD_TRUST_NOTICE));
            assert!(first.contains(UNTRUSTED_CONTENT_NOTICE));
            assert!(
                !first.contains("[Capsule]"),
                "the process transport injects no [Capsule] block; got:\n{first}"
            );
            assert!(
                !first.split_whitespace().any(|t| t.starts_with('/')),
                "no host path may appear in the prompt; got:\n{first}"
            );
            assert!(
                !first.contains("ses_"),
                "no session id may appear in the prompt; got:\n{first}"
            );
        }
    }

    #[test]
    fn process_system_prompt_carries_trust_notice_with_no_custom_prompt() {
        // Behavior change: previously this asserted equality with just
        // MURMUR_MD_TRUST_NOTICE. The `--system-prompt` value now always also carries
        // UNTRUSTED_CONTENT_NOTICE (C-4/C-7 posture), so the exact-equality assertion is
        // rewritten to check for both notices rather than deleted.
        let prompt = build_process_system_prompt(None);
        assert_eq!(
            prompt,
            format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}")
        );
    }

    #[test]
    fn process_system_prompt_carries_trust_notice_alongside_custom_prompt() {
        let prompt = build_process_system_prompt(Some("You are a helpful assistant."));
        assert!(prompt.contains(MURMUR_MD_TRUST_NOTICE));
        assert!(prompt.contains(UNTRUSTED_CONTENT_NOTICE));
        assert!(prompt.contains("You are a helpful assistant."));
        let notice_pos = prompt.find(MURMUR_MD_TRUST_NOTICE).unwrap();
        let untrusted_pos = prompt.find(UNTRUSTED_CONTENT_NOTICE).unwrap();
        let custom_pos = prompt.find("You are a helpful assistant.").unwrap();
        assert!(notice_pos < custom_pos);
        assert!(untrusted_pos < custom_pos);
    }

    fn test_inference(command: &str, model: &str) -> InferenceConfig {
        InferenceConfig {
            transport: "process".into(),
            endpoint: None,
            model: model.into(),
            api_key: None,
            driver: None,
            command: Some(command.into()),
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
        }
    }

    #[test]
    fn dialect_selected_from_command_basename() {
        assert!(ProcessDialect::from_command("codex") == ProcessDialect::Codex);
        assert!(ProcessDialect::from_command("/usr/local/bin/codex") == ProcessDialect::Codex);
        assert!(ProcessDialect::from_command("claude") == ProcessDialect::Claude);
        assert!(ProcessDialect::from_command("anything-else") == ProcessDialect::Claude);
    }

    #[test]
    fn codex_args_disable_host_tools_and_carry_task_as_positional() {
        let inf = test_inference("codex", "gpt-5.5");
        let args = build_process_args(ProcessDialect::Codex, &inf, &None, None, "do the thing");
        assert_eq!(args[0], "exec");
        assert!(args.iter().any(|a| a == "--json"));
        // host-execution tools disabled + apply_patch off → murmur is the sole executor
        for f in CODEX_DISABLED_FEATURES {
            assert!(
                args.windows(2).any(|w| w[0] == "--disable" && w[1] == *f),
                "missing --disable {f}"
            );
        }
        assert!(args.iter().any(|a| a == "include_apply_patch_tool=false"));
        // model passed via -m; task is the final positional arg
        assert!(args.windows(2).any(|w| w[0] == "-m" && w[1] == "gpt-5.5"));
        assert!(args.last().unwrap().ends_with("do the thing"));
    }

    #[test]
    fn codex_args_omit_model_when_empty() {
        let inf = test_inference("codex", "");
        let args = build_process_args(ProcessDialect::Codex, &inf, &None, None, "t");
        assert!(
            !args.iter().any(|a| a == "-m"),
            "empty model must not pass -m (uses account default)"
        );
    }

    #[test]
    fn claude_args_unchanged_shape() {
        let inf = test_inference("claude", "claude-opus-4-8");
        let args = build_process_args(ProcessDialect::Claude, &inf, &None, None, "t");
        assert_eq!(args[0], "--print");
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--model" && w[1] == "claude-opus-4-8"));
        // no-tools claude still disables built-ins via empty --tools
        assert!(args
            .windows(2)
            .any(|w| w[0] == "--tools" && w[1].is_empty()));
        assert!(args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn claude_args_omit_model_when_empty() {
        let inf = test_inference("claude", "");
        let args = build_process_args(ProcessDialect::Claude, &inf, &None, None, "t");
        // `--model ""` is a hard 400; empty model must omit the flag so claude uses its default.
        assert!(
            !args.iter().any(|a| a == "--model"),
            "empty model must not pass --model"
        );
    }

    #[test]
    fn strips_bridge_prefix_to_bare_tool_name() {
        assert_eq!(
            strip_bridge_prefix("mcp__claude_bridge__murmur-tool-editor"),
            "murmur-tool-editor"
        );
        // Non-bridge names pass through unchanged.
        assert_eq!(strip_bridge_prefix("some-other-tool"), "some-other-tool");
        assert_eq!(strip_bridge_prefix("mcp__other__t"), "mcp__other__t");
    }

    #[test]
    fn tool_result_text_handles_string_and_array_content() {
        use serde_json::json;
        assert_eq!(tool_result_text(&json!({"content": "hi"})), "hi");
        assert_eq!(
            tool_result_text(
                &json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]})
            ),
            "ab"
        );
        assert_eq!(tool_result_text(&json!({})), "");
    }

    #[test]
    fn thinking_only_events_are_skipped_but_actions_count() {
        use serde_json::json;
        // thinking-only → skipped
        assert!(is_thinking_only(Some(
            &json!([{"type": "thinking", "thinking": "…"}])
        )));
        assert!(is_thinking_only(Some(
            &json!([{"type": "redacted_thinking"}])
        )));
        // empty / absent → nothing actionable → skipped
        assert!(is_thinking_only(Some(&json!([]))));
        assert!(is_thinking_only(None));
        // any actionable block → counted (not thinking-only)
        assert!(!is_thinking_only(Some(
            &json!([{"type": "text", "text": "hi"}])
        )));
        assert!(!is_thinking_only(Some(
            &json!([{"type": "tool_use", "name": "editor"}])
        )));
        // thinking + action in the same event → counted
        assert!(!is_thinking_only(Some(&json!([
            {"type": "thinking"}, {"type": "tool_use", "name": "editor"}
        ]))));
    }

    #[test]
    fn process_system_prompt_treats_empty_string_as_absent() {
        // Behavior change: same rewrite as above — empty custom prompt still yields
        // both always-present notices, not just MURMUR_MD_TRUST_NOTICE.
        let prompt = build_process_system_prompt(Some(""));
        assert_eq!(
            prompt,
            format!("{MURMUR_MD_TRUST_NOTICE}\n{UNTRUSTED_CONTENT_NOTICE}")
        );
    }
}
