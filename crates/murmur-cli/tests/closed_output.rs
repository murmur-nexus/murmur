//! `mur`'s short-lived commands with a standard stream whose reader has gone away.
//!
//! A closed stream is a pipe whose read end was dropped before the child started, so every write
//! to it fails with `EPIPE`. The `mur` binary leaves `SIGPIPE` ignored, so the write error is
//! the only thing the command sees. In the dev profile a print that panics on it exits 101 with
//! `panicked at`; in the release profile, `panic = "abort"` turns the same panic into `SIGABRT`.
//! [`assert_survived`] catches both shapes.

use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};

use tempfile::TempDir;

const SESSION_A: &str = "ses_aaaaaaaaaaaa4aaa8aaa000000000001";
const SESSION_B: &str = "ses_bbbbbbbbbbbb4bbb8bbb000000000002";

/// A two-turn session: one tool call, one shell call, ending `ok`.
const FIXTURE_A: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1000,",
    "\"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",",
    "\"max_turns\":10,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",
    "{\"event_type\":\"inference\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1100,",
    "\"turn\":1,\"input_tokens\":1000,\"output_tokens\":200,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",
    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1200,",
    "\"turn\":1,\"tool_name\":\"bash\",\"input_bytes\":50,\"output_bytes\":20,\"duration_ms\":100,\"status\":\"ok\"}\n",
    "{\"event_type\":\"shell\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1300,",
    "\"turn\":1,\"command\":\"echo hello\",\"exit_code\":0,\"stdout_bytes\":6,\"stderr_bytes\":0,\"duration_ms\":50}\n",
    "{\"event_type\":\"inference\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1400,",
    "\"turn\":2,\"input_tokens\":1200,\"output_tokens\":150,\"decision\":\"end_turn\",\"tool_name\":null}\n",
    "{\"event_type\":\"session_end\",\"session_id\":\"ses_aaaaaaaaaaaa4aaa8aaa000000000001\",\"timestamp\":1500,",
    "\"total_turns\":2,\"total_input_tokens\":2200,\"total_output_tokens\":350,",
    "\"total_tool_calls\":1,\"total_shell_calls\":1,\"duration_ms\":500,\"exit_status\":\"ok\"}\n"
);

/// A second session, ending `max_turns_reached`, so `mur trace diff` has a difference to print.
const FIXTURE_B: &str = concat!(
    "{\"event_type\":\"session_start\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2000,",
    "\"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",",
    "\"max_turns\":5,\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}\n",
    "{\"event_type\":\"inference\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2100,",
    "\"turn\":1,\"input_tokens\":1500,\"output_tokens\":300,\"decision\":\"tool_call\",\"tool_name\":\"bash\"}\n",
    "{\"event_type\":\"tool_call\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2200,",
    "\"turn\":1,\"tool_name\":\"bash\",\"input_bytes\":80,\"output_bytes\":30,\"duration_ms\":200,\"status\":\"ok\"}\n",
    "{\"event_type\":\"session_end\",\"session_id\":\"ses_bbbbbbbbbbbb4bbb8bbb000000000002\",\"timestamp\":2300,",
    "\"total_turns\":5,\"total_input_tokens\":1500,\"total_output_tokens\":300,",
    "\"total_tool_calls\":1,\"total_shell_calls\":0,\"duration_ms\":300,\"exit_status\":\"max_turns_reached\"}\n"
);

/// The `mur` binary this test run built.
fn mur_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("mur")
}

/// A scratch `HOME` and a scratch working directory for one invocation.
struct Scratch {
    home: TempDir,
    cwd: TempDir,
}

impl Scratch {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
            cwd: TempDir::new().unwrap(),
        }
    }

    fn mur(&self, args: &[&str]) -> Command {
        let mut command = Command::new(mur_path());
        command
            .args(args)
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .current_dir(self.cwd.path())
            .stdin(Stdio::null());
        command
    }

    /// A file under the scratch working directory holding `content`.
    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.cwd.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }
}

/// A pipe's write end whose read end is already gone: every write to it fails with `EPIPE`.
fn closed() -> Stdio {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Stdio::from(writer)
}

/// Run `command` with stdout and stderr as given; a piped stream is read to its end.
fn run(mut command: Command, stdout: Stdio, stderr: Stdio) -> Output {
    command.stdout(stdout).stderr(stderr).output().unwrap()
}

/// No panic, no signal, and the expected exit code.
fn assert_survived(status: ExitStatus, stderr: &str, expected: i32, what: &str) {
    assert!(
        !stderr.contains("panicked at"),
        "{what} panicked:\n{stderr}"
    );
    assert!(
        !stderr.contains("failed printing to"),
        "{what} failed a print:\n{stderr}"
    );
    assert_eq!(status.signal(), None, "{what} died by signal:\n{stderr}");
    assert_eq!(status.code(), Some(expected), "{what}:\n{stderr}");
}

/// Run `args` with stdout closed and stderr readable, and assert it survived with `expected`.
fn assert_survives_closed_stdout(scratch: &Scratch, args: &[&str], expected: i32) {
    let output = run(scratch.mur(args), closed(), Stdio::piped());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_survived(output.status, &stderr, expected, &format!("mur {args:?}"));
}

#[test]
fn trace_show_survives_a_closed_stdout() {
    let scratch = Scratch::new();
    let trace = scratch.file("trace-a.jsonl", FIXTURE_A);
    assert_survives_closed_stdout(&scratch, &["trace", "show", trace.to_str().unwrap()], 0);
}

#[test]
fn trace_steps_survives_a_closed_stdout() {
    let scratch = Scratch::new();
    let trace = scratch.file("trace-a.jsonl", FIXTURE_A);
    assert_survives_closed_stdout(&scratch, &["trace", "steps", trace.to_str().unwrap()], 0);
}

#[test]
fn ps_survives_a_closed_stdout() {
    let scratch = Scratch::new();
    assert_survives_closed_stdout(&scratch, &["ps"], 0);
}

/// A session of `turns` tool-calling turns: each an `inference`, a `tool_call` and a `shell`
/// event shaped like [`FIXTURE_A`]'s.
fn big_trace(turns: u32) -> String {
    let session = "ses_cccccccccccc4ccc8ccc000000000003";
    let mut trace = format!(
        "{{\"event_type\":\"session_start\",\"session_id\":\"{session}\",\"timestamp\":1000,\
         \"capsule_name\":\"test-capsule\",\"capsule_version\":\"0.1.0\",\"model\":\"claude-3-5-sonnet\",\
         \"max_turns\":{turns},\"capabilities\":[\"shell\"],\"tools_declared\":[\"bash\"]}}\n"
    );
    let mut timestamp = 1000u64;
    for turn in 1..=turns {
        timestamp += 10;
        trace.push_str(&format!(
            "{{\"event_type\":\"inference\",\"session_id\":\"{session}\",\"timestamp\":{timestamp},\
             \"turn\":{turn},\"input_tokens\":1000,\"output_tokens\":200,\"decision\":\"tool_call\",\
             \"tool_name\":\"bash\"}}\n"
        ));
        timestamp += 10;
        trace.push_str(&format!(
            "{{\"event_type\":\"tool_call\",\"session_id\":\"{session}\",\"timestamp\":{timestamp},\
             \"turn\":{turn},\"tool_name\":\"bash\",\"input_bytes\":50,\"output_bytes\":20,\
             \"duration_ms\":100,\"status\":\"ok\"}}\n"
        ));
        timestamp += 10;
        trace.push_str(&format!(
            "{{\"event_type\":\"shell\",\"session_id\":\"{session}\",\"timestamp\":{timestamp},\
             \"turn\":{turn},\"command\":\"echo hello {turn}\",\"exit_code\":0,\"stdout_bytes\":6,\
             \"stderr_bytes\":0,\"duration_ms\":50}}\n"
        ));
    }
    timestamp += 10;
    trace.push_str(&format!(
        "{{\"event_type\":\"session_end\",\"session_id\":\"{session}\",\"timestamp\":{timestamp},\
         \"total_turns\":{turns},\"total_input_tokens\":{},\"total_output_tokens\":{},\
         \"total_tool_calls\":{turns},\"total_shell_calls\":{turns},\"duration_ms\":{},\
         \"exit_status\":\"ok\"}}\n",
        1000 * u64::from(turns),
        200 * u64::from(turns),
        timestamp - 1000
    ));
    trace
}

/// More than a pipe buffer holds, so the command is still writing when `head` has gone.
const PAST_THE_PIPE_BUFFER: usize = 256 * 1024;

/// `<pipeline>` under `bash -o pipefail`: exits 0 with exactly one line and no panic.
fn assert_head_takes_one_line(scratch: &Scratch, pipeline: &str) {
    let output = Command::new("bash")
        .args(["-c", &format!("set -o pipefail; {pipeline}")])
        .env("HOME", scratch.home.path())
        .env_remove("NEXUS_API_KEY")
        .current_dir(scratch.cwd.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked at"), "{pipeline}:\n{stderr}");
    assert!(!stderr.contains("Broken pipe"), "{pipeline}:\n{stderr}");
    assert_eq!(output.status.code(), Some(0), "{pipeline}:\n{stderr}");
    assert_eq!(stdout.lines().count(), 1, "{pipeline}:\n{stdout}");
    assert!(stdout.ends_with('\n'), "{pipeline}:\n{stdout}");
}

#[test]
fn head_takes_one_line_from_a_long_report() {
    let scratch = Scratch::new();
    let trace = scratch.file("big.jsonl", &big_trace(12_000));
    let mur = mur_path();
    let (mur, trace) = (mur.to_str().unwrap(), trace.to_str().unwrap());

    for subcommand in ["show", "steps"] {
        let full = scratch.mur(&["trace", subcommand, trace]).output().unwrap();
        assert!(
            full.status.success(),
            "mur trace {subcommand} on the big trace"
        );
        assert!(
            full.stdout.len() > PAST_THE_PIPE_BUFFER,
            "mur trace {subcommand} rendered only {} bytes",
            full.stdout.len()
        );
        assert_head_takes_one_line(
            &scratch,
            &format!("'{mur}' trace {subcommand} '{trace}' | head -1"),
        );
    }
    assert_head_takes_one_line(&scratch, &format!("'{mur}' ps | head -1"));
}

/// Commands whose exit status with stdout closed must equal the one with stdout open.
#[test]
fn a_closed_stdout_leaves_the_exit_status_alone() {
    let scratch = Scratch::new();
    let sessions = scratch.cwd.path().join("sessions");
    for (id, trace) in [(SESSION_A, FIXTURE_A), (SESSION_B, FIXTURE_B)] {
        fs::create_dir_all(sessions.join(id)).unwrap();
        fs::write(sessions.join(id).join("trace.jsonl"), trace).unwrap();
    }
    let a = scratch.file("trace-a.jsonl", FIXTURE_A);
    let b = scratch.file("trace-b.jsonl", FIXTURE_B);
    let (sessions, a, b) = (
        sessions.to_str().unwrap(),
        a.to_str().unwrap(),
        b.to_str().unwrap(),
    );

    let cases: [&[&str]; 9] = [
        &["--version"],
        &["--help"],
        &["list"],
        &["list", "--all"],
        &["doctor"],
        &["trace", "report", "--workdir", sessions],
        &["trace", "diff", a, b],
        &["conversation", "ls"],
        &["beta", "list"],
    ];
    for args in cases {
        let open = run(scratch.mur(args), Stdio::piped(), Stdio::piped());
        let expected = open
            .status
            .code()
            .unwrap_or_else(|| panic!("mur {args:?} with stdout open: {:?}", open.status));
        assert_survives_closed_stdout(&scratch, args, expected);
    }
}

/// `mur ps` prunes a dead record, writing `pruned:` to stderr, with both streams closed. The
/// record file is gone afterwards: ending the output does not end the work.
#[test]
fn ps_finishes_pruning_with_both_streams_closed() {
    let scratch = Scratch::new();
    let mut exited = Command::new("true").spawn().unwrap();
    let pid = exited.id();
    exited.wait().unwrap();

    let session_id = format!("ses_{:0>32}", "dead");
    let running = scratch.home.path().join(".murmur").join("running");
    fs::create_dir_all(&running).unwrap();
    let record = running.join(format!("{session_id}.json"));
    fs::write(
        &record,
        serde_json::to_vec_pretty(&serde_json::json!({
            "session_id": session_id,
            "url": "127.0.0.1:1",
            "pid": pid,
            "process_start": "1",
            "capsule_name": "fabricated",
            "capsule_version": "0.1.0",
            "workdir": "/tmp/fabricated",
            "outlives_launcher": true,
            "started_at": "2026-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();

    let output = run(scratch.mur(&["ps"]), closed(), closed());
    assert_eq!(output.status.signal(), None, "mur ps died by signal");
    assert_eq!(output.status.code(), Some(0));
    assert!(!record.exists(), "mur ps left the dead record in place");
}

/// A command that fails keeps its own exit status when neither stream can carry its output.
#[test]
fn a_failed_command_keeps_exit_one_with_both_streams_closed() {
    let scratch = Scratch::new();
    let output = run(
        scratch.mur(&["trace", "show", "/nonexistent/trace.jsonl"]),
        closed(),
        closed(),
    );
    assert_eq!(
        output.status.signal(),
        None,
        "mur trace show died by signal"
    );
    assert_eq!(output.status.code(), Some(1));
}

/// A stdout that refuses writes for a reason other than a gone reader fails a successful command.
#[cfg(target_os = "linux")]
#[test]
fn a_full_disk_on_stdout_fails_the_command_with_e_io_003() {
    use std::io::Read;

    let scratch = Scratch::new();
    let trace = scratch.file("trace-a.jsonl", FIXTURE_A);
    let full = fs::File::options().write(true).open("/dev/full").unwrap();
    let mut child = scratch
        .mur(&["trace", "show", trace.to_str().unwrap()])
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!stderr.contains("panicked at"), "{stderr}");
    assert_eq!(status.signal(), None, "{stderr}");
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("E-IO-003"), "{stderr}");
    assert!(
        stderr.contains("failed to write to standard output"),
        "{stderr}"
    );
}

/// `mur doctor` on a manifest with an unrecognized key, which makes it write a `W-SEC-019`
/// warning to stderr, keeps its own exit status when neither stream can carry its output.
#[test]
fn doctor_warns_into_closed_streams_and_keeps_its_exit_status() {
    let scratch = Scratch::new();
    scratch.file(
        "murmur.yaml",
        "name: warned-capsule\nversion: 0.1.0\nnot_a_manifest_key: 1\n",
    );
    scratch.file("capsule.wasm", "\0asm\x01\0\0\0");

    let open = run(scratch.mur(&["doctor"]), Stdio::piped(), Stdio::piped());
    let open_stderr = String::from_utf8_lossy(&open.stderr);
    assert!(open_stderr.contains("W-SEC-019"), "{open_stderr}");
    let expected = open.status.code().expect("mur doctor with streams open");

    let closed_run = run(scratch.mur(&["doctor"]), closed(), closed());
    assert_eq!(
        closed_run.status.signal(),
        None,
        "mur doctor died by signal"
    );
    assert_eq!(closed_run.status.code(), Some(expected));
}
