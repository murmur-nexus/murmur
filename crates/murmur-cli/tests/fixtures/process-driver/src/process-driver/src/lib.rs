// Test-only process driver for `tests/process_driver.rs` and the process driver runner. It drives
// a harness that does not exist: every answer is fixed, so a test can assert on each field the
// runtime reads. The README in the fixture root lists what each call returns.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/process-driver",
    world: "process-driver",
    generate_all,
});

use exports::murmur::driver::process::{
    Description, DriverFile, Event, ExitStatus, FailureKind, Guest, InterruptMethod, LaunchPlan,
    LaunchRequest, SessionInfo, SessionMode, TurnFailure,
};

struct Fixture;

impl Guest for Fixture {
    fn describe() -> Description {
        Description {
            harness: "fixture-harness".to_string(),
            binary: "fixture-cli".to_string(),
            version_args: vec!["--version".to_string()],
            tested_versions: vec!["1.0.0".to_string()],
            interrupt: InterruptMethod::StdinMessage,
            required_env: vec!["HOME".to_string(), "FIXTURE_HARNESS_PROFILE".to_string()],
            streams_text: true,
        }
    }

    fn launch(request: LaunchRequest) -> Result<LaunchPlan, String> {
        if request.task.is_empty() {
            return Err("empty task".to_string());
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
        let mut stdin = request.task.into_bytes();
        stdin.push(b'\n');
        Ok(LaunchPlan {
            args,
            env_set: vec![("FIXTURE_FILES".to_string(), "{files_dir}".to_string())],
            files: vec![DriverFile {
                name: "config.json".to_string(),
                contents: request.config.unwrap_or_else(|| "{}".to_string()),
            }],
            stdin: Some(stdin),
            keep_stdin_open: true,
            interrupt_stdin: Some(b"interrupt\n".to_vec()),
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

fn parse_line(line: String) -> Event {
    let Some((word, rest)) = line.split_once(' ') else {
        return Event::Note(line);
    };
    match word {
        "started" => Event::SessionStarted(SessionInfo {
            id: rest.to_string(),
            auth: "subscription".to_string(),
            model: None,
        }),
        "delta" => Event::TextDelta(rest.to_string()),
        "text" => Event::Text(rest.to_string()),
        "end" => Event::TurnEnd(rest.to_string()),
        "fail" => Event::TurnFailed(TurnFailure {
            kind: FailureKind::Other,
            message: rest.to_string(),
        }),
        _ => Event::Note(line),
    }
}

export!(Fixture);
