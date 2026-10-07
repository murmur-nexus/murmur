// The process driver the escape-conformance gate installs into every case's project store. It
// drives one harness, the gate's own `probe-driver`, which makes predetermined tool calls over the
// capsule's tool bridge and reports them on stdout in a word-prefixed line protocol. README.md
// lists what each call returns.
//
// The line protocol has a second implementation: `escape_conformance::harness_protocol` formats
// the lines `probe-driver` prints. `committed_driver_reads_every_line_the_probe_writes` in the
// gate package feeds those lines through the committed component, so the two cannot drift apart
// without `cargo test --workspace` failing.

wit_bindgen::generate!({
    path: "../../wit/process-driver",
    world: "process-driver",
    generate_all,
});

use exports::murmur::driver::process::{
    Description, Event, ExitStatus, FailureKind, Guest, InterruptMethod, LaunchPlan,
    LaunchRequest, ToolCallInfo, ToolResultInfo, TurnFailure, Usage,
};

/// The `probe-driver` version this driver reads. `probe-driver --version` prints it as
/// `escape-conformance-probe <version>`, and the gate's `PROBE_HARNESS_VERSION` holds the same
/// value.
const PROBE_HARNESS_VERSION: &str = "1.0.0";

/// The variables the launch plan hands `probe-driver`. The gate's `harness_protocol` names the
/// same three.
const ENV_BRIDGE_URL: &str = "MURMUR_EC_BRIDGE_URL";
const ENV_BRIDGE_TOKEN: &str = "MURMUR_EC_BRIDGE_TOKEN";
const ENV_PROBE: &str = "MURMUR_EC_PROBE";

struct ProbeDriver;

impl Guest for ProbeDriver {
    fn describe() -> Description {
        Description {
            harness: "escape-conformance-probe".to_string(),
            binary: "probe-driver".to_string(),
            version_args: vec!["--version".to_string()],
            tested_versions: vec![PROBE_HARNESS_VERSION.to_string()],
            interrupt: InterruptMethod::Unsupported,
            required_env: Vec::new(),
            streams_text: false,
            // `probe-driver` spends no tokens and says so with `usage in=0 out=0`. A driver that
            // reported nothing would be refused at staging on any host with a token ceiling.
            reports_usage: true,
        }
    }

    fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
        let Some(bridge) = request.bridge else {
            return Err("the capsule exposes no tools, so the probe has nothing to call".to_string());
        };
        let Some(config) = request.config else {
            return Err("inference.driver.config is required: it names the case".to_string());
        };
        Ok(LaunchPlan {
            args: Vec::new(),
            env_set: vec![
                (ENV_BRIDGE_URL.to_string(), bridge.url),
                (ENV_BRIDGE_TOKEN.to_string(), bridge.bearer_token),
                (ENV_PROBE.to_string(), config),
            ],
            files: Vec::new(),
            stdin: None,
            keep_stdin_open: false,
            interrupt_stdin: None,
        })
    }

    fn parse(lines: Vec<String>) -> Vec<Event> {
        lines.into_iter().map(parse_line).collect()
    }

    fn classify_exit(exit: ExitStatus) -> Event {
        if exit.interrupted {
            failed(FailureKind::Canceled, "interrupted")
        } else if exit.code == Some(0) {
            failed(
                FailureKind::HarnessError,
                "probe-driver exited without an end line",
            )
        } else {
            failed(FailureKind::HarnessError, &exit.stderr_tail)
        }
    }
}

fn failed(kind: FailureKind, message: &str) -> Event {
    Event::TurnFailed(TurnFailure {
        kind,
        message: message.to_string(),
    })
}

/// One stdout line into one event, read from its first word. A line that does not have the
/// shape its word promises is a `note`, so it reaches the trace rather than vanishing.
fn parse_line(line: String) -> Event {
    let (word, rest) = match line.split_once(' ') {
        Some((word, rest)) => (word, Some(rest)),
        None => (line.as_str(), None),
    };
    let event = match (word, rest) {
        ("tool", Some(rest)) => parse_tool_call(rest),
        ("result", Some(rest)) => parse_tool_result(rest),
        ("usage", Some(rest)) => parse_usage(rest),
        ("end", rest) => Some(Event::TurnEnd(rest.unwrap_or_default().to_string())),
        ("fail", rest) => Some(failed(FailureKind::HarnessError, rest.unwrap_or_default())),
        _ => None,
    };
    event.unwrap_or(Event::Note(line))
}

/// `tool <id> <name> <json>`. Bridge tool names are bare and `probe-driver` uses them as they
/// are, so there is no prefix to strip.
fn parse_tool_call(rest: &str) -> Option<Event> {
    let mut parts = rest.splitn(3, ' ');
    let (id, name, input) = (parts.next()?, parts.next()?, parts.next()?);
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(Event::ToolCall(ToolCallInfo {
        id: id.to_string(),
        name: name.to_string(),
        input: input.to_string(),
    }))
}

/// `result <id> ok|error [<text>]`. A tool that printed nothing comes back as `result <id> ok`,
/// which is a result with empty output.
fn parse_tool_result(rest: &str) -> Option<Event> {
    let mut parts = rest.splitn(3, ' ');
    let id = parts.next().filter(|id| !id.is_empty())?;
    let is_error = match parts.next()? {
        "ok" => false,
        "error" => true,
        _ => return None,
    };
    Some(Event::ToolResult(ToolResultInfo {
        id: id.to_string(),
        output: parts.next().unwrap_or_default().to_string(),
        is_error,
    }))
}

/// `usage in=<n> out=<n>`, cumulative for the run as the interface asks.
fn parse_usage(rest: &str) -> Option<Event> {
    let mut fields = rest.split(' ');
    let input = fields.next()?.strip_prefix("in=")?.parse::<u64>().ok()?;
    let output = fields.next()?.strip_prefix("out=")?.parse::<u64>().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(Event::Usage(Usage {
        input: Some(input),
        output: Some(output),
        cache_read: None,
        cache_creation: None,
        thinking: None,
    }))
}

export!(ProbeDriver);

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Event {
        parse_line(line.to_string())
    }

    #[test]
    fn a_tool_line_is_a_tool_call_with_its_json_verbatim() {
        match parse(r#"tool c1 python3 {"command":"ec-probe.py"}"#) {
            Event::ToolCall(call) => {
                assert_eq!(call.id, "c1");
                assert_eq!(call.name, "python3");
                assert_eq!(call.input, r#"{"command":"ec-probe.py"}"#);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_result_line_carries_its_status_and_text() {
        match parse("result c2 error isError=true :: Exit code: 1") {
            Event::ToolResult(result) => {
                assert_eq!(result.id, "c2");
                assert!(result.is_error);
                assert_eq!(result.output, "isError=true :: Exit code: 1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_result_with_no_text_is_an_empty_result() {
        for line in ["result c1 ok", "result c1 ok "] {
            match parse(line) {
                Event::ToolResult(result) => {
                    assert!(!result.is_error);
                    assert_eq!(result.output, "");
                }
                other => panic!("{line:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn usage_end_and_fail_lines() {
        assert!(matches!(
            parse("usage in=0 out=0"),
            Event::Usage(Usage {
                input: Some(0),
                output: Some(0),
                cache_read: None,
                cache_creation: None,
                thinking: None,
            })
        ));
        assert!(matches!(parse("end case=x :: done"), Event::TurnEnd(s) if s == "case=x :: done"));
        assert!(matches!(
            parse("fail probe-driver failed: no bridge"),
            Event::TurnFailed(TurnFailure { kind: FailureKind::HarnessError, message })
                if message == "probe-driver failed: no bridge"
        ));
    }

    #[test]
    fn malformed_lines_are_notes() {
        for line in [
            "",
            "text hello",
            "tool c1 python3",
            "result c1",
            "result c1 maybe out",
            "usage in=0",
            "usage in=x out=0",
            "usage in=0 out=0 extra",
            "usage out=0 in=0",
        ] {
            assert!(
                matches!(parse(line), Event::Note(ref note) if note == line),
                "{line:?} should be a note"
            );
        }
    }

    #[test]
    fn classify_exit_never_reports_success() {
        let exit = |code, interrupted| ExitStatus {
            code,
            signal: None,
            stderr_tail: "boom".to_string(),
            interrupted,
            saw_terminal: false,
        };
        assert!(matches!(
            ProbeDriver::classify_exit(exit(Some(0), true)),
            Event::TurnFailed(TurnFailure { kind: FailureKind::Canceled, .. })
        ));
        assert!(matches!(
            ProbeDriver::classify_exit(exit(Some(0), false)),
            Event::TurnFailed(TurnFailure { kind: FailureKind::HarnessError, ref message })
                if message == "probe-driver exited without an end line"
        ));
        assert!(matches!(
            ProbeDriver::classify_exit(exit(Some(3), false)),
            Event::TurnFailed(TurnFailure { kind: FailureKind::HarnessError, ref message })
                if message == "boom"
        ));
    }
}
