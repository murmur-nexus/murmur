//! An agent capsule's turn cap, per-turn output cap and context window: no turn cap unless the
//! manifest sets `inference.max_turns`, and the output cap and window from the driver's own
//! `models:` table unless the manifest sets `inference.max_tokens` and `context.max_tokens`.
//!
//! Driven through the real `mur` binary and the real anthropic driver fixture against a scripted
//! provider, because the properties are what reaches the provider, what `session_start` records,
//! what stderr says and what `mur doctor` reports.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::PathBuf};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const TASK: &str = "Say hello.";

/// A driver table listing `test-model` with limits unlike any default.
const TABLE: &str = "models:\n  test-model:\n    max_output_tokens: 65536\n    context_window: \
                     400000\n";
/// A driver table that lists a model, but not `test-model`.
const OTHER_TABLE: &str = "models:\n  other-model:\n    max_output_tokens: 1000\n    \
                           context_window: 2000\n";

// ── Scripted provider responses ───────────────────────────────────────────────

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A turn that asks for a tool the capsule does not have, so the loop wants another turn.
fn tool_call(index: usize) -> String {
    common::tool_use_response(&format!("toolu_{index}"), "no-such-tool", json!({}))
}

/// `tool_calls` tool-calling turns, then one that ends the task.
fn script(tool_calls: usize) -> Vec<String> {
    (0..tool_calls)
        .map(tool_call)
        .chain(std::iter::once(end_turn("msg_end", "done")))
        .collect()
}

// ── Homes, projects and `mur run` ─────────────────────────────────────────────

/// A scratch `HOME` with the driver published into it, its manifest declaring `models` as its
/// `models:` table (empty for none).
fn home_with_driver(models: &str) -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let artifact = common::create_driver_artifact_with_auth(
        artifacts.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        &format!("{}{models}", common::FIXTURE_DRIVER_AUTH),
    );
    common::publish_local(&home, &artifact).success();
    home
}

/// A queue capsule on `test-model` that exits after its task, reaching `endpoint` through the
/// driver's gateway. `inference_extra` is spliced into the `inference:` block and `top_extra` at
/// the top level.
fn manifest(endpoint: &str, inference_extra: &str, top_extra: &str) -> String {
    format!(
        "name: limits\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER}\n    version: \
         {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
         api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         lifecycle:\n  task_acceptance: queue\n  after_task: exit\n{top_extra}\
         inference:\n  transport: http\n  model: test-model\n{inference_extra}  driver:\n    \
         artifact: {DRIVER}\n"
    )
}

struct Project {
    dir: TempDir,
    manifest: PathBuf,
}

fn project(manifest_yaml: &str) -> Project {
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(&manifest, manifest_yaml).unwrap();
    Project { dir, manifest }
}

/// What one `mur run` or `mur doctor` left behind.
struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn context(&self) -> String {
        format!(
            "exit {:?}\nstdout:\n{}\nstderr:\n{}",
            self.output.status.code(),
            self.stdout,
            self.stderr
        )
    }

    fn trace(&self) -> Vec<Value> {
        common::read_whole_trace(
            &common::parse_records_from_stdout(&self.stdout).join("trace.jsonl"),
        )
    }

    /// Every stderr line naming `code`.
    fn lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(code))
            .collect()
    }
}

/// `mur run --task … --lifecycle-after-task exit` on `project`.
fn run(home: &TempDir, project: &Project) -> Run {
    Run::of(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .env("MURMUR_A2A_TIMEOUT_SECS", "1")
            .current_dir(project.dir.path())
            .args([
                "run",
                "--manifest",
                project.manifest.to_str().unwrap(),
                "--task",
                TASK,
                "--lifecycle-after-task",
                "exit",
                "--verbose",
            ])
            .output()
            .unwrap(),
    )
}

/// `mur doctor` in `project`'s directory.
fn doctor(home: &TempDir, project: &Project) -> Run {
    Run::of(
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .current_dir(project.dir.path())
            .arg("doctor")
            .output()
            .unwrap(),
    )
}

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn session_start(trace: &[Value]) -> &Value {
    events(trace, "session_start")
        .first()
        .copied()
        .expect("session_start is written")
}

/// The `max_tokens` every request the provider received carried.
fn sent_caps(requests: &[Value]) -> Vec<Value> {
    requests
        .iter()
        .map(|request| request["max_tokens"].clone())
        .collect()
}

fn limits(cap: u32, cap_source: &str, window: Option<u32>, window_source: &str) -> Value {
    json!({
        "max_output_tokens": cap,
        "max_output_tokens_source": cap_source,
        "context_window": window,
        "context_window_source": window_source,
    })
}

// ── The turn cap ──────────────────────────────────────────────────────────────

/// With no `inference.max_turns`, a task runs as many turns as it needs: twelve tool-calling turns
/// and the turn that ends it.
#[test]
fn no_turn_cap() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(script(12));
    let project = project(&manifest(&server.endpoint, "", ""));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());

    let trace = run.trace();
    assert_eq!(session_start(&trace)["max_turns"], Value::Null);
    let task_end = events(&trace, "task_end");
    assert_eq!(task_end.len(), 1, "{trace:?}");
    assert_eq!(task_end[0]["exit_status"], "ok", "{trace:?}");
    assert_eq!(task_end[0]["turns"], 13, "{trace:?}");
    assert_eq!(events(&trace, "inference").len(), 13, "{trace:?}");
    assert!(
        !trace
            .iter()
            .any(|event| event.to_string().contains("max_turns_reached")),
        "{trace:?}"
    );
    assert_eq!(server.requests().len(), 13);

    let show = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .args(["trace", "show"])
        .arg(common::parse_records_from_stdout(&run.stdout).join("trace.jsonl"))
        .output()
        .unwrap();
    let show = String::from_utf8_lossy(&show.stdout);
    assert!(show.contains("count:      13  (max: no cap)"), "{show}");
}

/// A set `inference.max_turns` caps the task exactly as written.
#[test]
fn max_turns_still_caps() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(script(12));
    let project = project(&manifest(&server.endpoint, "  max_turns: 3\n", ""));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());
    let refusal = run.lines("error[E-RUN-040]");
    assert_eq!(refusal.len(), 1, "{}", run.context());
    assert!(
        refusal[0].contains("inference.max_turns"),
        "{}",
        run.context()
    );

    let trace = run.trace();
    assert_eq!(session_start(&trace)["max_turns"], 3);
    assert_eq!(events(&trace, "inference").len(), 3, "{trace:?}");
    assert_eq!(
        events(&trace, "task_end")[0]["exit_status"],
        "max_turns_reached"
    );
    assert_eq!(server.requests().len(), 3);
}

// ── The output cap and the context window ─────────────────────────────────────

/// With neither key set, every request carries the cap the driver's table declares, and
/// `session_start` records both limits as the driver's.
#[test]
fn limits_from_driver_table() {
    let home = home_with_driver(TABLE);
    let server = common::ScriptedServer::start(script(2));
    let project = project(&manifest(&server.endpoint, "", ""));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.lines("W-RUN-009").is_empty(), "{}", run.context());

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(sent_caps(&requests), vec![json!(65536); 3]);
    let trace = run.trace();
    let start = session_start(&trace);
    assert_eq!(start["max_turns"], Value::Null);
    assert_eq!(
        start["model_limits"],
        limits(65536, "driver", Some(400_000), "driver")
    );
}

/// Each manifest key overrides its own limit as written and leaves the other the driver's.
#[test]
fn manifest_overrides() {
    for (inference_extra, top_extra, expected) in [
        (
            "  max_tokens: 4096\n",
            "",
            limits(4096, "manifest", Some(400_000), "driver"),
        ),
        (
            "",
            "context:\n  max_tokens: 120000\n",
            limits(65536, "driver", Some(120_000), "manifest"),
        ),
        (
            "  max_tokens: 4096\n",
            "context:\n  max_tokens: 120000\n",
            limits(4096, "manifest", Some(120_000), "manifest"),
        ),
    ] {
        let home = home_with_driver(TABLE);
        let server = common::ScriptedServer::start(script(0));
        let project = project(&manifest(&server.endpoint, inference_extra, top_extra));
        let run = run(&home, &project);
        assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
        assert!(run.lines("W-RUN-009").is_empty(), "{}", run.context());
        assert_eq!(
            sent_caps(&server.requests()),
            vec![expected["max_output_tokens"].clone()]
        );
        assert_eq!(session_start(&run.trace())["model_limits"], expected);
    }
}

/// A model the driver's table does not list runs on the manifest's cap with no window, and says so
/// once; with `context.max_tokens` also set, it has nothing to say.
#[test]
fn unknown_model_warns_once() {
    let home = home_with_driver(OTHER_TABLE);
    let server = common::ScriptedServer::start(script(1));
    let project = project(&manifest(&server.endpoint, "  max_tokens: 4096\n", ""));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let warnings = run.lines("warning[W-RUN-009]");
    assert_eq!(warnings.len(), 1, "{}", run.context());
    for expected in [
        "driver local/murmur-driver-anthropic@0.1.0 declares no limits for model 'test-model'",
        "inference.max_tokens (4096) as its output cap",
        "knows no context window: compaction does not run",
        "set context.max_tokens",
        "https://docs.murmur.nexus/reference/diagnostics/#w-run-009",
    ] {
        assert!(warnings[0].contains(expected), "{}", warnings[0]);
    }
    assert_eq!(sent_caps(&server.requests()), vec![json!(4096); 2]);
    assert_eq!(
        session_start(&run.trace())["model_limits"],
        limits(4096, "manifest", None, "none")
    );

    let server = common::ScriptedServer::start(script(0));
    let project = self::project(&manifest(
        &server.endpoint,
        "  max_tokens: 4096\n",
        "context:\n  max_tokens: 9000\n",
    ));
    let run = self::run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    assert!(run.lines("W-RUN-009").is_empty(), "{}", run.context());
}

/// A model the driver's table does not list, with no `inference.max_tokens`, is refused before any
/// request leaves, naming the choice, the model, the driver and the key to set.
#[test]
fn unknown_model_without_cap_is_refused() {
    for table in ["", OTHER_TABLE] {
        let home = home_with_driver(table);
        let server = common::ScriptedServer::start(script(0));
        let project = project(&manifest(&server.endpoint, "", ""));
        let run = run(&home, &project);
        assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
        let refusal = run.lines("error[E-RUN-052]");
        assert_eq!(refusal.len(), 1, "{}", run.context());
        assert_eq!(
            refusal[0],
            "error[E-RUN-052]: driver choice 'primary' has no output cap: driver \
             local/murmur-driver-anthropic@0.1.0 declares no limits for model 'test-model' and \
             the manifest sets no inference.max_tokens",
            "{}",
            run.context()
        );
        assert!(
            run.stderr.contains("hint: set inference.max_tokens"),
            "{}",
            run.context()
        );
        assert!(server.requests().is_empty(), "{}", run.context());
    }
}

/// A driver shipping a malformed table refuses the launch, naming the driver and the field.
#[test]
fn malformed_table_is_refused() {
    let home = home_with_driver(
        "models:\n  test-model:\n    max_output_tokens: 0\n    context_window: 1\n",
    );
    let server = common::ScriptedServer::start(script(0));
    let project = project(&manifest(&server.endpoint, "  max_tokens: 4096\n", ""));
    let run = run(&home, &project);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let refusal = run.lines("error[E-RUN-052]");
    assert_eq!(refusal.len(), 1, "{}", run.context());
    assert!(
        refusal[0]
            .contains("driver local/murmur-driver-anthropic@0.1.0 ships a malformed models: table")
            && refusal[0].contains("models.test-model.max_output_tokens"),
        "{}",
        run.context()
    );
    assert!(server.requests().is_empty(), "{}", run.context());
}

// ── mur doctor ────────────────────────────────────────────────────────────────

/// The project directory and manifest `mur doctor` reads; the endpoint is never reached.
fn doctor_project(inference_extra: &str) -> Project {
    project(&manifest("http://127.0.0.1:9", inference_extra, ""))
}

#[test]
fn doctor_prints_the_limits_a_known_model_resolves_to() {
    let home = home_with_driver(TABLE);
    let run = doctor(&home, &doctor_project(""));
    assert!(
        run.stdout.contains(
            "primary (local/murmur-driver-anthropic@0.1.0, model test-model): output cap 65536 \
             (driver), context window 400000 (driver)"
        ),
        "{}",
        run.context()
    );
    assert!(run.lines("W-RUN-009").is_empty(), "{}", run.context());
    assert!(!run.stdout.contains("E-RUN-052"), "{}", run.context());
}

/// `mur doctor` prints the `W-RUN-009` a launch prints, in the same words.
#[test]
fn doctor_warns_for_an_unknown_model_with_a_cap() {
    let home = home_with_driver(OTHER_TABLE);
    let project = doctor_project("  max_tokens: 4096\n");
    let doctor = doctor(&home, &project);
    assert!(
        doctor.stdout.contains(
            "primary (local/murmur-driver-anthropic@0.1.0, model test-model): output cap 4096 \
             (manifest), context window none (none)"
        ),
        "{}",
        doctor.context()
    );
    let warned = doctor.lines("warning[W-RUN-009]");
    assert_eq!(warned.len(), 1, "{}", doctor.context());
    // A warning, not a fix: counted in the tally, and doctor still exits 0.
    assert_eq!(doctor.output.status.code(), Some(0), "{}", doctor.context());
    assert!(
        doctor.stdout.contains("0 errors found, 1 warning."),
        "{}",
        doctor.context()
    );
    assert!(
        doctor.stdout.lines().any(|line| line.starts_with(
            "Fix: set context.max_tokens for model 'test-model' (driver choice 'primary')"
        )),
        "{}",
        doctor.context()
    );

    let server = common::ScriptedServer::start(script(0));
    let launched = run(
        &home,
        &self::project(&manifest(&server.endpoint, "  max_tokens: 4096\n", "")),
    );
    let at_launch = launched.lines("warning[W-RUN-009]");
    assert_eq!(at_launch.len(), 1, "{}", launched.context());
    let text = |line: &str| {
        line.split_once("warning[W-RUN-009]")
            .map(|(_, text)| text.to_string())
            .unwrap()
    };
    assert_eq!(text(warned[0]), text(at_launch[0]));
}

/// `mur doctor` reports the `E-RUN-052` refusal as a fix naming `inference.max_tokens`, and exits
/// non-zero.
#[test]
fn doctor_fixes_an_unknown_model_without_a_cap() {
    let home = home_with_driver(OTHER_TABLE);
    let run = doctor(&home, &doctor_project(""));
    assert_eq!(run.output.status.code(), Some(1), "{}", run.context());
    assert!(
        run.stdout.contains(
            "error[E-RUN-052]: driver choice 'primary' has no output cap: driver \
             local/murmur-driver-anthropic@0.1.0 declares no limits for model 'test-model'"
        ),
        "{}",
        run.context()
    );
    assert!(
        run.stdout
            .lines()
            .any(|line| line.contains("Fix:") && line.contains("set inference.max_tokens")),
        "{}",
        run.context()
    );
}
