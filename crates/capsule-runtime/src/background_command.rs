//! The runtime-provided `background-command` tool: the state of a shell command that was moved
//! to the background, a wait for it, or a stop of it, named by the work id its shell call
//! returned.
//!
//! The commands themselves are kept by [`crate::detached::DetachedRegistry`]; this module holds
//! what the model reads: the description, the input schema, the input parser and the result.

use std::time::Duration;

use crate::{
    bindings::host::murmur::tool::run::{Status, ToolResult},
    detached::{CommandSnapshot, CommandState, WORK_ID_PREFIX},
    shell::MAX_SHELL_OUTPUT_BYTES,
};

/// The runtime-provided tool an agent capsule has when it can run a shell command.
pub const BACKGROUND_COMMAND_TOOL: &str = "background-command";

/// The longest one `wait` blocks, in seconds, and what it blocks for when `wait_secs` is absent.
pub const MAX_WAIT_SECS: u64 = 600;

/// How long `stop` waits after `SIGTERM` before it sends `SIGKILL`.
pub(crate) const STOP_TERM_GRACE: Duration = Duration::from_secs(2);

/// How long `stop` waits after `SIGKILL` for the command to finish.
pub(crate) const STOP_KILL_WAIT: Duration = Duration::from_secs(3);

/// `background-command`'s input schema.
pub const INPUT_SCHEMA: &str = r#"{"type":"object","properties":{"operation":{"type":"string","enum":["status","wait","stop"]},"work_id":{"type":"string"},"wait_secs":{"type":"integer","minimum":0}},"required":["operation","work_id"]}"#;

/// `background-command`'s description, the same on both transports, for what the capsule has:
/// `shell`, shell commands demoted on a clock, and `code_mode_tool`, the code mode tool whose calls
/// can be moved to the background. A capsule with shell demotion alone is described by
/// [`shell_description`].
pub fn description(shell: bool, code_mode_tool: Option<&str>) -> String {
    match (shell, code_mode_tool) {
        (_, None) => shell_description(),
        (false, Some(tool)) => code_mode_description(tool),
        (true, Some(tool)) => format!(
            "{} It also reports on, waits for, or stops a call to the '{tool}' tool that was \
             moved to the background, named by the work id that call returned: `wait` returns \
             once the call finishes, and `stop` ends the code mode instance the call runs in, \
             which stops the call; every value the instance holds is lost, and the next call to \
             '{tool}' starts a new instance. For such a call each operation returns its state \
             (running, finished or stopped), the seconds since it started and, once finished, \
             the call's own status and result; a call still running at the session's end is \
             stopped.",
            shell_description()
        ),
    }
}

/// The description of a capsule whose calls to the code mode tool `tool` can be moved to the
/// background, and which demotes no shell command.
fn code_mode_description(tool: &str) -> String {
    format!(
        "Reports on, waits for, or stops a call to the '{tool}' tool that was moved to the \
         background, named by the work id ({WORK_ID_PREFIX}…) that call returned. `operation` is \
         `status`, `wait` or `stop`. `status` returns at once. `wait` returns once the call \
         finishes or after `wait_secs` seconds, whichever is first; `wait_secs` defaults to \
         {MAX_WAIT_SECS}, which is also the most it can be. `stop` ends the code mode instance \
         the call runs in, which stops the call; every value the instance holds is lost, and the \
         next call to '{tool}' starts a new instance. Each operation returns the call's state \
         (running, finished or stopped) and the seconds since it started; a finished call's \
         status and result follow. A work id is valid for the rest of the session that issued \
         it; a call still running at the session's end is stopped."
    )
}

/// The description of a capsule whose shell commands are demoted on a clock and which has no
/// code mode tool.
fn shell_description() -> String {
    format!(
        "Reports on, waits for, or stops a shell command that was moved to the background, named \
         by the work id ({WORK_ID_PREFIX}…) its shell call returned. `operation` is `status`, \
         `wait` or `stop`. `status` returns at once. `wait` returns once the command exits or \
         after `wait_secs` seconds, whichever is first; `wait_secs` defaults to {MAX_WAIT_SECS}, \
         which is also the most it can be. `stop` sends SIGTERM to the command and every process \
         in its process group, SIGKILL {term} seconds later to any still running, and returns \
         once the command has exited; a process that left the group is not reached and ends \
         with the session. Each operation returns the command's state (running, exited or \
         stopped), its exit code once it has one, the seconds since it started, and the last \
         {MAX_SHELL_OUTPUT_BYTES} bytes of its combined stdout and stderr, with the path of the \
         file in the session's records that holds all of it. A work id is valid for the rest of \
         the session that issued it; a command still running at the session's end is killed.",
        term = STOP_TERM_GRACE.as_secs(),
    )
}

/// What a call asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Status,
    Wait,
    Stop,
}

impl Operation {
    const ALL: [Self; 3] = [Self::Status, Self::Wait, Self::Stop];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Wait => "wait",
            Self::Stop => "stop",
        }
    }
}

/// One parsed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub operation: Operation,
    pub work_id: String,
    /// Read only by [`Operation::Wait`].
    pub wait_secs: Option<u64>,
}

/// The accepted operations, as a refusal names them.
fn accepted_operations() -> String {
    Operation::ALL
        .map(|operation| format!("`{}`", operation.as_str()))
        .join(", ")
}

/// Parses a call's input. The refusal names the offending field and value.
pub(crate) fn parse_request(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<Request, String> {
    let operation = match args.get("operation") {
        None | Some(serde_json::Value::Null) => {
            return Err(format!(
                "'{BACKGROUND_COMMAND_TOOL}' requires `operation`, one of {}",
                accepted_operations()
            ))
        }
        Some(value) => Operation::ALL
            .into_iter()
            .find(|operation| value.as_str() == Some(operation.as_str()))
            .ok_or_else(|| {
                format!(
                    "'{BACKGROUND_COMMAND_TOOL}' has no operation {value}; `operation` is one of {}",
                    accepted_operations()
                )
            })?,
    };
    let work_id = match args.get("work_id") {
        None | Some(serde_json::Value::Null) => {
            return Err(format!(
                "'{BACKGROUND_COMMAND_TOOL}' requires `work_id`, the {WORK_ID_PREFIX}<hex> id a \
                 shell call returned"
            ))
        }
        Some(serde_json::Value::String(work_id)) => work_id.clone(),
        Some(value) => {
            return Err(format!(
                "'{BACKGROUND_COMMAND_TOOL}' got `work_id` {value}; a work id is a string of the \
                 form {WORK_ID_PREFIX}<hex>"
            ))
        }
    };
    if !is_work_id(&work_id) {
        return Err(format!(
            "`{work_id}` is not a work id: a work id has the form {WORK_ID_PREFIX}<hex>"
        ));
    }
    let wait_secs = match args.get("wait_secs") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            format!(
                "'{BACKGROUND_COMMAND_TOOL}' got `wait_secs` {value}; `wait_secs` is a whole \
                 number of seconds, 0 or more"
            )
        })?),
    };
    Ok(Request {
        operation,
        work_id,
        wait_secs,
    })
}

/// Whether `text` matches `^wrk_[0-9a-f]+$`.
fn is_work_id(text: &str) -> bool {
    text.strip_prefix(WORK_ID_PREFIX).is_some_and(|hex| {
        !hex.is_empty()
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// The refusal for a well-formed work id this session never issued.
pub(crate) fn unknown_work_id(work_id: &str) -> String {
    format!("no background command with work id `{work_id}` in this session")
}

/// The refusal for a session that keeps no background commands.
pub(crate) fn not_available() -> String {
    format!("'{BACKGROUND_COMMAND_TOOL}': background commands are not available in this session")
}

/// How long a `wait` blocks: `wait_secs`, at most [`MAX_WAIT_SECS`], which is also what an
/// absent `wait_secs` gives.
pub fn effective_wait(wait_secs: Option<u64>) -> Duration {
    Duration::from_secs(wait_secs.unwrap_or(MAX_WAIT_SECS).min(MAX_WAIT_SECS))
}

/// The tool result for `snapshot`, and the runtime note that goes after the fence.
///
/// The lines are `work_id`, `state`, `elapsed_secs`, `waited_secs` (a `wait` only), `exit_code`,
/// `status` and `wait_error` (a finished command only, `wait_error` when the wait failed),
/// `output`, `output_bytes` and `output_shown`, then a blank line and the output tail. The command
/// is not among them: the work id names it, and the tool call that started it holds it.
///
/// `waited` is the time a `wait` blocked, and `None` for the other operations. A found command is
/// `Passed` whatever its exit code. The note carries an attributed resource limit and the output
/// holders a foreground call would report, and is `None` when there is neither.
pub(crate) fn render(
    snapshot: &CommandSnapshot,
    waited: Option<Duration>,
) -> (ToolResult, Option<String>) {
    let mut lines = vec![
        format!("work_id: {}", snapshot.work_id),
        format!("state: {}", snapshot.state.as_str()),
        format!("elapsed_secs: {}", snapshot.elapsed_ms / 1000),
    ];
    if let Some(waited) = waited {
        lines.push(format!("waited_secs: {}", waited.as_secs()));
    }
    let mut metadata = vec![
        ("work_id".to_string(), snapshot.work_id.clone()),
        ("state".to_string(), snapshot.state.as_str().to_string()),
    ];
    let mut notes = Vec::new();
    if let Some(completion) = &snapshot.finished {
        lines.push(format!("exit_code: {}", completion.exit_code));
        lines.push(format!("status: {}", completion.status()));
        if let Some(error) = &completion.error {
            lines.push(format!("wait_error: {error}"));
        }
        metadata.push(("exit_code".to_string(), completion.exit_code.to_string()));
        notes.extend(
            completion
                .resource_limit
                .as_deref()
                .map(crate::resources::resource_limit_line),
        );
        notes.extend(completion.output_holders.note());
    }
    let truncated = snapshot.output_bytes > snapshot.tail.len() as u64;
    lines.push(format!("output: {}", snapshot.output_path));
    lines.push(format!("output_bytes: {}", snapshot.output_bytes));
    lines.push(if truncated {
        format!("output_shown: last {MAX_SHELL_OUTPUT_BYTES} bytes")
    } else {
        "output_shown: all".to_string()
    });

    let shown = if truncated {
        from_char_boundary(&snapshot.tail)
    } else {
        &snapshot.tail
    };
    let mut data = lines.join("\n");
    if !shown.is_empty() {
        data.push_str("\n\n");
        data.push_str(&String::from_utf8_lossy(shown));
    }
    let summary = match &snapshot.finished {
        Some(completion) if snapshot.state != CommandState::Running => format!(
            "{} {}, exit code {}",
            snapshot.work_id,
            snapshot.state.as_str(),
            completion.exit_code
        ),
        _ => format!("{} {}", snapshot.work_id, snapshot.state.as_str()),
    };
    let result = ToolResult {
        status: Status::Passed,
        summary: Some(summary),
        data: Some(data),
        data_path: None,
        truncated,
        metadata,
        attachments: Vec::new(),
    };
    (result, (!notes.is_empty()).then(|| notes.join("\n")))
}

/// The tool result for `snapshot`, a call to the code mode tool moved to the background.
///
/// The lines are `work_id`, `tool`, `state`, `elapsed_secs`, `waited_secs` (a `wait` only) and
/// `call_status` (a finished call only), then a blank line and the call's own data for a finished
/// call, or the stop sentence for a stopped one. `status` is the call's own for a finished call and
/// `Passed` otherwise; a finished call's `summary`, `truncated` and `attachments` are carried over.
pub(crate) fn render_code_mode(
    snapshot: &crate::code_mode::CallSnapshot,
    waited: Option<Duration>,
) -> ToolResult {
    use crate::code_mode::CallState;

    let mut lines = vec![
        format!("work_id: {}", snapshot.work_id),
        format!("tool: {}", snapshot.tool),
        format!("state: {}", snapshot.state.as_str()),
        format!("elapsed_secs: {}", snapshot.elapsed.as_secs()),
    ];
    if let Some(waited) = waited {
        lines.push(format!("waited_secs: {}", waited.as_secs()));
    }
    let metadata = vec![
        ("work_id".to_string(), snapshot.work_id.clone()),
        ("state".to_string(), snapshot.state.as_str().to_string()),
    ];
    let running_summary = || format!("{} {}", snapshot.work_id, snapshot.state.as_str());
    match &snapshot.state {
        CallState::Running => ToolResult {
            status: Status::Passed,
            summary: Some(running_summary()),
            data: Some(lines.join("\n")),
            data_path: None,
            truncated: false,
            metadata,
            attachments: Vec::new(),
        },
        CallState::Stopped => ToolResult {
            status: Status::Passed,
            summary: Some(running_summary()),
            data: Some(format!(
                "{}\n\n{}",
                lines.join("\n"),
                crate::code_mode::stopped_sentence(&snapshot.tool)
            )),
            data_path: None,
            truncated: false,
            metadata,
            attachments: Vec::new(),
        },
        CallState::Finished(Ok(result)) => {
            lines.push(format!("call_status: {}", status_name(&result.status)));
            let own = result.data.as_deref().or(result.summary.as_deref());
            let data = match own {
                Some(own) => format!("{}\n\n{own}", lines.join("\n")),
                None => lines.join("\n"),
            };
            ToolResult {
                status: result.status,
                summary: result.summary.clone().or_else(|| Some(running_summary())),
                data: Some(data),
                data_path: None,
                truncated: result.truncated,
                metadata,
                attachments: result.attachments.clone(),
            }
        }
        CallState::Finished(Err(error)) => {
            lines.push("call_status: error".to_string());
            ToolResult {
                status: Status::Error,
                summary: Some(running_summary()),
                data: Some(format!("{}\n\n{error}", lines.join("\n"))),
                data_path: None,
                truncated: false,
                metadata,
                attachments: Vec::new(),
            }
        }
    }
}

/// The bytes of a finished code mode call's own data, as `background_command.output_bytes`
/// records it; `0` for a call that has not finished.
pub(crate) fn code_mode_output_bytes(state: &crate::code_mode::CallState) -> u64 {
    match state {
        crate::code_mode::CallState::Finished(Ok(result)) => result
            .data
            .as_deref()
            .or(result.summary.as_deref())
            .map_or(0, |data| data.len() as u64),
        crate::code_mode::CallState::Finished(Err(error)) => error.len() as u64,
        crate::code_mode::CallState::Running | crate::code_mode::CallState::Stopped => 0,
    }
}

fn status_name(status: &Status) -> &'static str {
    match status {
        Status::Passed => "passed",
        Status::Failed => "failed",
        Status::Error => "error",
    }
}

/// `tail` from its first byte that starts a UTF-8 character: a tail cut from a longer output can
/// begin inside one.
fn from_char_boundary(tail: &[u8]) -> &[u8] {
    let skip = tail
        .iter()
        .take(3)
        .take_while(|byte| (**byte & 0b1100_0000) == 0b1000_0000)
        .count();
    &tail[skip..]
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    /// The description states what each operation does and its limits, and nothing about when or
    /// why to use it.
    /// The text a capsule with shell demotion and no code mode is offered, byte for byte.
    const SHELL_ONLY_DESCRIPTION: &str = "Reports on, waits for, or stops a shell command that was \
        moved to the background, named by the work id (wrk_…) its shell call returned. \
        `operation` is `status`, `wait` or `stop`. `status` returns at once. `wait` returns once \
        the command exits or after `wait_secs` seconds, whichever is first; `wait_secs` defaults \
        to 600, which is also the most it can be. `stop` sends SIGTERM to the command and every \
        process in its process group, SIGKILL 2 seconds later to any still running, and returns \
        once the command has exited; a process that left the group is not reached and ends with \
        the session. Each operation returns the command's state (running, exited or stopped), \
        its exit code once it has one, the seconds since it started, and the last 16384 bytes of \
        its combined stdout and stderr, with the path of the file in the session's records that \
        holds all of it. A work id is valid for the rest of the session that issued it; a \
        command still running at the session's end is killed.";

    #[test]
    fn background_command_shell_only_description_is_unchanged() {
        assert_eq!(description(true, None), SHELL_ONLY_DESCRIPTION);
    }

    #[test]
    fn background_command_shell_refusals_are_unchanged() {
        assert_eq!(
            unknown_work_id("wrk_0a"),
            "no background command with work id `wrk_0a` in this session"
        );
        assert_eq!(
            not_available(),
            "'background-command': background commands are not available in this session"
        );
    }

    /// The code mode and combined descriptions state what each operation does to a code mode
    /// call, and nothing about when or why to use it.
    #[test]
    fn background_command_code_mode_descriptions_state_facts_only() {
        let code_mode = description(false, Some("counter-tool"));
        let combined = description(true, Some("counter-tool"));
        assert!(combined.starts_with(SHELL_ONLY_DESCRIPTION), "{combined}");
        for text in [&code_mode, &combined] {
            for fact in [
                "'counter-tool'",
                "`stop` ends the code mode instance",
                "every value the instance holds is lost",
                "starts a new instance",
                "running, finished or stopped",
            ] {
                assert!(text.contains(fact), "{fact} missing: {text}");
            }
            let lower = text.to_lowercase();
            for advice in ["when", "should", "call this", "use this"] {
                assert!(!lower.contains(advice), "{advice}: {text}");
            }
        }
        assert!(!code_mode.contains("SIGTERM"), "{code_mode}");
        assert!(code_mode.contains("`wait_secs` defaults to 600"), "{code_mode}");
    }

    fn code_mode_snapshot(state: crate::code_mode::CallState) -> crate::code_mode::CallSnapshot {
        crate::code_mode::CallSnapshot {
            work_id: "wrk_0a1b".to_string(),
            tool: "counter-tool".to_string(),
            state,
            elapsed: Duration::from_millis(4_200),
        }
    }

    #[test]
    fn background_command_code_mode_results_render_their_lines() {
        use crate::code_mode::CallState;

        let running = render_code_mode(&code_mode_snapshot(CallState::Running), None);
        assert_eq!(
            running.data.as_deref(),
            Some("work_id: wrk_0a1b\ntool: counter-tool\nstate: running\nelapsed_secs: 4")
        );
        assert!(matches!(running.status, Status::Passed));

        let finished = render_code_mode(
            &code_mode_snapshot(CallState::Finished(Ok(ToolResult {
                status: Status::Passed,
                summary: Some("counted".to_string()),
                data: Some("count=1".to_string()),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: Vec::new(),
            }))),
            Some(Duration::from_secs(3)),
        );
        assert_eq!(
            finished.data.as_deref(),
            Some(
                "work_id: wrk_0a1b\ntool: counter-tool\nstate: finished\nelapsed_secs: 4\n\
                 waited_secs: 3\ncall_status: passed\n\ncount=1"
            )
        );
        assert_eq!(finished.summary.as_deref(), Some("counted"));

        let stopped = render_code_mode(&code_mode_snapshot(CallState::Stopped), None);
        assert_eq!(
            stopped.data.as_deref(),
            Some(
                "work_id: wrk_0a1b\ntool: counter-tool\nstate: stopped\nelapsed_secs: 4\n\n\
                 The call was stopped by ending the code mode instance; every value the instance \
                 held is lost, and the next call to 'counter-tool' starts a new instance."
            )
        );

        let failed = render_code_mode(
            &code_mode_snapshot(CallState::Finished(Err("tool 'counter-tool' trapped".into()))),
            None,
        );
        assert!(matches!(failed.status, Status::Error));
        assert!(
            failed.data.as_deref().unwrap().contains("call_status: error\n\ntool"),
            "{failed:?}"
        );
    }

    #[test]
    fn the_description_states_operations_and_limits_only() {
        let description = description(true, None);
        for fact in [
            "`status`",
            "`wait`",
            "`stop`",
            "wrk_",
            "600",
            "16384",
            "SIGTERM",
            "SIGKILL",
            "2 seconds",
            "process group",
            "running, exited or stopped",
            "killed",
        ] {
            assert!(description.contains(fact), "{fact} missing: {description}");
        }
        let lower = description.to_lowercase();
        for advice in ["when", "should", "call this", "use this"] {
            assert!(!lower.contains(advice), "{advice}: {description}");
        }
    }

    /// The schema parses, offers the three operations and requires `operation` and `work_id`.
    #[test]
    fn the_input_schema_names_the_three_operations() {
        let schema: serde_json::Value = serde_json::from_str(INPUT_SCHEMA).unwrap();
        assert_eq!(
            schema["properties"]["operation"]["enum"],
            json!(["status", "wait", "stop"])
        );
        assert_eq!(schema["required"], json!(["operation", "work_id"]));
        assert_eq!(schema["properties"]["wait_secs"]["minimum"], json!(0));
    }

    #[test]
    fn wait_secs_is_capped_at_the_runtime_limit() {
        assert_eq!(effective_wait(None), Duration::from_secs(600));
        assert_eq!(effective_wait(Some(0)), Duration::ZERO);
        assert_eq!(effective_wait(Some(30)), Duration::from_secs(30));
        assert_eq!(effective_wait(Some(100_000)), Duration::from_secs(600));
    }

    fn parse(args: serde_json::Value) -> Result<Request, String> {
        parse_request(args.as_object().expect("a JSON object"))
    }

    #[test]
    fn a_request_is_parsed_and_each_refusal_names_its_value() {
        assert_eq!(
            parse(json!({"operation": "wait", "work_id": "wrk_0a1b", "wait_secs": 5})),
            Ok(Request {
                operation: Operation::Wait,
                work_id: "wrk_0a1b".to_string(),
                wait_secs: Some(5),
            })
        );
        let refused = |args: serde_json::Value| parse(args).unwrap_err();

        let malformed = refused(json!({"operation": "status", "work_id": "abc"}));
        assert!(
            malformed.contains("`abc`") && malformed.contains("wrk_<hex>"),
            "{malformed}"
        );
        for not_hex in ["wrk_", "wrk_0A1B", "wrk_xyz", "WRK_0a1b"] {
            let refusal = refused(json!({"operation": "status", "work_id": not_hex}));
            assert!(refusal.contains(not_hex), "{refusal}");
        }
        let missing = refused(json!({"operation": "status"}));
        assert!(missing.contains("`work_id`"), "{missing}");
        let restart = refused(json!({"operation": "restart", "work_id": "wrk_0a1b"}));
        assert!(
            restart.contains("\"restart\"") && restart.contains("`status`, `wait`, `stop`"),
            "{restart}"
        );
        let negative =
            refused(json!({"operation": "wait", "work_id": "wrk_0a1b", "wait_secs": -1}));
        assert!(negative.contains("-1"), "{negative}");
    }

    fn snapshot(tail: &[u8], output_bytes: u64) -> CommandSnapshot {
        CommandSnapshot {
            work_id: "wrk_0a1b".to_string(),
            state: CommandState::Running,
            elapsed_ms: 12_345,
            finished: None,
            output_path: "/records/logs/wrk_0a1b.log".to_string(),
            output_bytes,
            tail: tail.to_vec(),
            delivered_now: false,
        }
    }

    /// A running command's result has the lines a running state has, in order, and no exit
    /// lines.
    #[test]
    fn a_running_command_renders_without_exit_lines() {
        let (result, note) = render(&snapshot(b"tick 1\n", 7), None);
        assert_eq!(
            result.data.as_deref(),
            Some(
                "work_id: wrk_0a1b\nstate: running\n\
                 elapsed_secs: 12\noutput: /records/logs/wrk_0a1b.log\noutput_bytes: 7\n\
                 output_shown: all\n\ntick 1\n"
            )
        );
        assert!(note.is_none());
        assert!(!result.truncated);
        assert!(matches!(result.status, Status::Passed));
    }

    fn finished(state: CommandState, exit_code: i32) -> CommandSnapshot {
        CommandSnapshot {
            state,
            finished: Some(crate::detached::DetachedCompletion {
                work_id: "wrk_0a1b".to_string(),
                binary: "/usr/bin/bash".to_string(),
                command: "make test".to_string(),
                exit_code,
                duration_ms: 12_345,
                exited_at_ms: 1_000,
                output_path: "/records/logs/wrk_0a1b.log".to_string(),
                output_bytes: 7,
                resource_limit: None,
                output_holders: crate::shell::OutputHolders::Closed,
                context_id: "ctx_1".to_string(),
                provenance: crate::origin::TaskProvenance::derive(
                    crate::origin::TaskOrigin::Completion,
                    None,
                ),
                error: None,
            }),
            ..snapshot(b"tick 1\n", 7)
        }
    }

    /// An exited and a stopped command render their exit lines and, like a running one, name the
    /// command by its work id alone.
    #[test]
    fn a_finished_command_renders_its_exit_lines_and_no_command() {
        let (exited, _) = render(&finished(CommandState::Exited, 0), None);
        assert_eq!(
            exited.data.as_deref(),
            Some(
                "work_id: wrk_0a1b\nstate: exited\nelapsed_secs: 12\nexit_code: 0\nstatus: ok\n\
                 output: /records/logs/wrk_0a1b.log\noutput_bytes: 7\noutput_shown: all\n\n\
                 tick 1\n"
            )
        );
        let (stopped, _) = render(
            &finished(CommandState::Stopped, 143),
            Some(Duration::from_secs(2)),
        );
        let stopped = stopped.data.unwrap();
        assert!(
            stopped.lines().any(|line| line == "state: stopped"),
            "{stopped}"
        );
        assert!(
            stopped.lines().any(|line| line == "exit_code: 143"),
            "{stopped}"
        );
        for data in [exited.data.as_deref().unwrap(), stopped.as_str()] {
            assert!(
                !data.lines().any(|line| line.starts_with("command:")),
                "{data}"
            );
            assert!(!data.contains("make test"), "{data}");
            assert!(!data.contains("/usr/bin/bash"), "{data}");
        }
    }

    /// A tail cut inside a multi-byte character starts at the next character.
    #[test]
    fn a_cut_tail_starts_on_a_character() {
        let mut tail = "é".as_bytes()[1..].to_vec();
        tail.extend_from_slice(b"rest");
        let (result, _) = render(
            &snapshot(&tail, 100_000),
            Some(Duration::from_millis(1_900)),
        );
        let data = result.data.unwrap();
        assert!(data.contains("waited_secs: 1\n"), "{data}");
        assert!(data.contains("output_shown: last 16384 bytes"), "{data}");
        assert!(data.ends_with("\n\nrest"), "{data}");
        assert!(result.truncated);
    }
}
