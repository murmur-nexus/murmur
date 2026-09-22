// Test-only process driver for `tests/process_driver.rs`, `tests/process_runner.rs` and the
// process driver runner's own tests. It drives a harness that does not exist: every answer is
// fixed, so a test can assert on each field the runtime reads. The README in the fixture root
// lists what each call returns.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/process-driver",
    world: "process-driver",
    generate_all,
});

use std::cell::RefCell;

use exports::murmur::driver::process::{
    Description, DriverFile, Event, ExitStatus, FailureKind, Guest, InterruptMethod, LaunchPlan,
    LaunchRequest, RetryInfo, SessionInfo, SessionMode, ToolCallInfo, ToolResultInfo, TurnFailure,
    Usage,
};

thread_local! {
    /// The prefix this driver told its harness to address bridge tools by, remembered from
    /// `launch`. The runtime keeps one instance for a whole run, so `parse` can strip again what
    /// `launch` prepended — proving the runtime never builds or reads a harness tool name.
    static TOOL_PREFIX: RefCell<Option<String>> = const { RefCell::new(None) };
}

struct Fixture;

impl Guest for Fixture {
    fn describe() -> Description {
        Description {
            harness: "fixture-harness".to_string(),
            binary: "fixture-cli".to_string(),
            version_args: vec!["--version".to_string()],
            tested_versions: vec!["1.0.0".to_string()],
            interrupt: interrupt_method(),
            required_env: vec!["HOME".to_string(), "FIXTURE_HARNESS_PROFILE".to_string()],
            streams_text: true,
            reports_usage: reports_usage(),
        }
    }

    fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
        if request.task.is_empty() {
            return Err("empty task".to_string());
        }
        if request
            .config
            .as_deref()
            .is_some_and(|config| config.contains("refuse-launch"))
        {
            return Err("launch refused by config".to_string());
        }
        let mode = match request.session.mode {
            SessionMode::New => "new",
            SessionMode::Resume => "resume",
        };
        let mut args = vec![
            "--session".to_string(),
            request.session.id,
            "--mode".to_string(),
            mode.to_string(),
        ];
        if let Some(model) = request.model {
            args.push("--model".to_string());
            args.push(model);
        }
        let mut env_set = vec![("FIXTURE_FILES".to_string(), "{files_dir}".to_string())];
        if let Some(bridge) = request.bridge {
            let prefix = format!("{}__", bridge.server_name);
            let qualified: Vec<String> = bridge
                .tool_names
                .iter()
                .map(|name| format!("{prefix}{name}"))
                .collect();
            TOOL_PREFIX.with(|slot| *slot.borrow_mut() = Some(prefix));
            env_set.push(("FIXTURE_BRIDGE_URL".to_string(), bridge.url));
            env_set.push(("FIXTURE_BRIDGE_TOKEN".to_string(), bridge.bearer_token));
            env_set.push(("FIXTURE_BRIDGE_TOOLS".to_string(), qualified.join(",")));
        }
        let mut stdin = request.task.into_bytes();
        stdin.push(b'\n');
        Ok(LaunchPlan {
            args,
            env_set,
            files: vec![DriverFile {
                name: "config.json".to_string(),
                contents: request.config.unwrap_or_else(|| "{}".to_string()),
            }],
            stdin: Some(stdin),
            keep_stdin_open: true,
            interrupt_stdin: interrupt_stdin(),
        })
    }

    fn parse(lines: Vec<String>) -> Vec<Event> {
        lines.into_iter().map(parse_line).collect()
    }

    fn classify_exit(exit: ExitStatus) -> Event {
        if exit.interrupted {
            Event::TurnFailed(TurnFailure {
                kind: FailureKind::Canceled,
                message: "interrupted".to_string(),
            })
        } else if exit.code == Some(0) {
            Event::TurnEnd(String::new())
        } else {
            Event::TurnFailed(TurnFailure {
                kind: FailureKind::HarnessError,
                message: exit.stderr_tail,
            })
        }
    }
}

/// What this build declares about interrupting its harness, chosen by cargo feature. The three
/// components built from this source are otherwise identical, so a test drives each of the
/// runtime's three interrupt paths against the same driver.
fn interrupt_method() -> InterruptMethod {
    if cfg!(feature = "signal-int") {
        InterruptMethod::SignalInt
    } else if cfg!(feature = "unsupported") {
        InterruptMethod::Unsupported
    } else {
        InterruptMethod::StdinMessage
    }
}

/// Whether this build reads `usage` lines. The `no-usage` build reports `false` and drops every
/// `usage` line into a `note`, which is what a harness that reports no token counts at all looks
/// like to the runtime.
fn reports_usage() -> bool {
    !cfg!(feature = "no-usage")
}

/// The bytes the fake harness treats as an interrupt. A driver that declares `unsupported` names
/// none: there is nothing the runtime could write that would stop its harness.
fn interrupt_stdin() -> Option<Vec<u8>> {
    if cfg!(feature = "unsupported") {
        None
    } else {
        Some(b"interrupt\n".to_vec())
    }
}

fn parse_line(line: String) -> Event {
    let Some((word, rest)) = line.split_once(' ') else {
        return Event::Note(line);
    };
    match word {
        "started" => {
            let (id, auth) = match rest.split_once(' ') {
                Some((id, auth)) => (id, auth),
                None => (rest, "subscription"),
            };
            Event::SessionStarted(SessionInfo {
                id: id.to_string(),
                auth: auth.to_string(),
                model: None,
            })
        }
        "delta" => Event::TextDelta(rest.to_string()),
        "text" => Event::Text(rest.to_string()),
        "tdelta" => Event::ThinkingDelta(rest.to_string()),
        "thinking" => Event::Thinking(rest.to_string()),
        "tool" => parse_tool_call(rest, &line),
        "result" => parse_tool_result(rest, &line),
        "retry" => match rest.split_once(' ') {
            Some((attempt, reason)) => match attempt.parse::<u32>() {
                Ok(attempt) => Event::Retry(RetryInfo {
                    attempt,
                    reason: reason.to_string(),
                }),
                Err(_) => Event::Note(line),
            },
            None => Event::Note(line),
        },
        "usage" if reports_usage() => parse_usage(rest, &line),
        "end" => Event::TurnEnd(rest.to_string()),
        "fail" => match rest.split_once(' ') {
            Some((kind, message)) => match failure_kind(kind) {
                Some(kind) => Event::TurnFailed(TurnFailure {
                    kind,
                    message: message.to_string(),
                }),
                None => Event::Note(line),
            },
            None => Event::Note(line),
        },
        _ => Event::Note(line),
    }
}

/// `usage <member>=<n> ...`, with `<member>` one of `in`, `out`, `cache-read`, `cache-creation`
/// or `thinking`. Every value is the cumulative total for the harness run so far, which is what
/// the interface asks a driver to report; members the line omits are left `none`.
fn parse_usage(rest: &str, line: &str) -> Event {
    let mut usage = Usage {
        input: None,
        output: None,
        cache_read: None,
        cache_creation: None,
        thinking: None,
    };
    let mut named = false;
    for field in rest.split_whitespace() {
        let Some((member, value)) = field.split_once('=') else {
            return Event::Note(line.to_string());
        };
        let Ok(value) = value.parse::<u64>() else {
            return Event::Note(line.to_string());
        };
        let slot = match member {
            "in" => &mut usage.input,
            "out" => &mut usage.output,
            "cache-read" => &mut usage.cache_read,
            "cache-creation" => &mut usage.cache_creation,
            "thinking" => &mut usage.thinking,
            _ => return Event::Note(line.to_string()),
        };
        *slot = Some(value);
        named = true;
    }
    if named {
        Event::Usage(usage)
    } else {
        Event::Note(line.to_string())
    }
}

/// `tool <id> <name> <json>`. The name arrives the way this driver told its harness to spell it,
/// so the prefix `launch` added comes back off here.
fn parse_tool_call(rest: &str, line: &str) -> Event {
    let mut parts = rest.splitn(3, ' ');
    let (Some(id), Some(name), Some(input)) = (parts.next(), parts.next(), parts.next()) else {
        return Event::Note(line.to_string());
    };
    let bare = TOOL_PREFIX.with(|slot| match slot.borrow().as_deref() {
        Some(prefix) => name.strip_prefix(prefix).unwrap_or(name).to_string(),
        None => name.to_string(),
    });
    Event::ToolCall(ToolCallInfo {
        id: id.to_string(),
        name: bare,
        input: input.to_string(),
    })
}

/// `result <id> ok|error <output>`.
fn parse_tool_result(rest: &str, line: &str) -> Event {
    let mut parts = rest.splitn(3, ' ');
    let (Some(id), Some(status), Some(output)) = (parts.next(), parts.next(), parts.next()) else {
        return Event::Note(line.to_string());
    };
    let is_error = match status {
        "ok" => false,
        "error" => true,
        _ => return Event::Note(line.to_string()),
    };
    Event::ToolResult(ToolResultInfo {
        id: id.to_string(),
        output: output.to_string(),
        is_error,
    })
}

fn failure_kind(word: &str) -> Option<FailureKind> {
    match word {
        "auth" => Some(FailureKind::Auth),
        "quota" => Some(FailureKind::Quota),
        "max-turns" => Some(FailureKind::MaxTurns),
        "canceled" => Some(FailureKind::Canceled),
        "harness-error" => Some(FailureKind::HarnessError),
        "other" => Some(FailureKind::Other),
        _ => None,
    }
}

export!(Fixture);
