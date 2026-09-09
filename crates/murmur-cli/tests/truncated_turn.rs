//! What a turn the provider cut off at `inference.max_tokens` leaves behind, measured against what
//! a turn that finished leaves behind.
//!
//! Every case drives the real `mur` binary against a real Wasmtime driver fixture
//! (`truncation-driver`) that scripts each `stop_reason` the agent loop dispatches on and never
//! makes an HTTP call. `mur` runs as a subprocess rather than through `launch_session`, because
//! the `W-RUN-001` warning is a property of the session's stderr.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "truncation-driver";
const DRIVER_VERSION: &str = "0.1.0";
const TASK_TEXT: &str = "Name the three main causes.";
const CONTEXT_ID: &str = "ctx_truncation";

/// The cap the manifest declares, and the value every surface must name.
const CAP: u32 = 256;

/// The fragment the fixture returns, mirrored from
/// `tests/fixtures/truncation-driver/src/truncation-driver/src/lib.rs`.
const PARTIAL_TEXT: &str = "The three main causes are, first, the";

/// The marker `out/result.txt` ends on, mirrored from `capsule-runtime`'s `truncation_marker`.
const MARKER: &str =
    "[truncated at the inference.max_tokens output cap of 256 tokens — this is a fragment of the \
     reply, not a finished answer]";

fn driver_wasm() -> PathBuf {
    common::fixture_path("truncation-driver/tool/truncation-driver.wasm")
}

/// A capsule wired to the fixture, declaring `inference.max_tokens: 256`.
///
/// It declares no `capabilities.shell.allow` and no `capabilities.spawn`, so the session needs no
/// cgroup scope and runs unprivileged. The endpoint is a dummy: the fixture answers locally.
fn write_project(project_dir: &Path, config_mode: Option<&str>) -> PathBuf {
    let endpoint = "http://127.0.0.1:9";
    let config_section = match config_mode {
        Some(mode) => format!("    config:\n      mode: {mode}\n"),
        None => String::new(),
    };
    let manifest = format!(
        "name: truncation-capsule\n\
         version: 0.1.0\n\
         artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n\
         capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
         api_key: test-key\n  max_tokens: {CAP}\n  driver:\n    artifact: {DRIVER_NAME}\n\
         {config_section}"
    );
    let path = project_dir.join("murmur.yaml");
    fs::write(&path, manifest).unwrap();
    path
}

/// What one `mur run` left behind.
struct Run {
    home: TempDir,
    /// Held so the project directory — and the session workdir under it — outlives the assertions.
    _project: TempDir,
    output: std::process::Output,
    workdir: PathBuf,
}

impl Run {
    /// `mur run` exited `0`. Prints stderr on a failure, which is where the reason is.
    fn assert_success(&self) {
        assert!(
            self.output.status.success(),
            "mur run exited {:?}; stderr: {}",
            self.output.status.code(),
            self.stderr()
        );
    }

    fn stderr(&self) -> String {
        String::from_utf8(self.output.stderr.clone()).unwrap()
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir.join("out/result.txt")).expect("a result was written")
    }

    fn trace(&self) -> String {
        fs::read_to_string(self.workdir.join("trace.jsonl")).expect("a trace was written")
    }

    fn inference_events(&self) -> Vec<Value> {
        self.trace()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|ev| ev.get("event_type").and_then(Value::as_str) == Some("inference"))
            .collect()
    }

    fn sole_inference(&self) -> Value {
        let events = self.inference_events();
        assert_eq!(events.len(), 1, "expected one turn; got: {events:?}");
        events.into_iter().next().unwrap()
    }

    fn exit_status(&self) -> String {
        self.trace()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .rfind(|ev| ev.get("event_type").and_then(Value::as_str) == Some("session_end"))
            .and_then(|ev| {
                ev.get("exit_status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    /// The assistant messages the conversation record holds for this run's context.
    fn recorded_assistants(&self) -> Vec<Value> {
        let path = self
            .home
            .path()
            .join(".murmur/conversations/truncation-capsule")
            .join(CONTEXT_ID)
            .join("conversation.jsonl");
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|msg| msg.get("role").and_then(Value::as_str) == Some("assistant"))
            .collect()
    }

    /// `mur trace steps` over this run's trace — the turn-per-line rendering.
    fn trace_steps(&self) -> String {
        let out = Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .args([
                "trace",
                "steps",
                self.workdir.join("trace.jsonl").to_str().unwrap(),
            ])
            .assert()
            .success();
        String::from_utf8(out.get_output().stdout.clone()).unwrap()
    }
}

/// Publish the fixture driver, then run `mur run --context` against it under `config_mode`.
fn run(config_mode: Option<&str>) -> Run {
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &driver_wasm(),
    );
    common::publish_local(&home, &artifact).success();

    let manifest = write_project(project.path(), config_mode);
    let assertion = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            TASK_TEXT,
            "--context",
            CONTEXT_ID,
        ])
        .assert();

    let trace =
        common::find_file(project.path(), "trace.jsonl").expect("the session wrote a trace");
    let workdir = trace.parent().unwrap().to_path_buf();

    Run {
        home,
        _project: project,
        output: assertion.get_output().clone(),
        workdir,
    }
}

/// Every stderr line naming `W-RUN-001`.
fn warning_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("warning[W-RUN-001]"))
        .collect()
}

/// The whole of a capped turn, on every surface at once: the marked result, the trace's
/// `stop_reason`, one warning naming the setting and its value, and an `ok` session.
#[test]
fn a_capped_turn_is_marked_on_every_surface() {
    let run = run(None);
    run.assert_success();

    assert_eq!(
        run.result(),
        format!("{PARTIAL_TEXT}\n\n{MARKER}"),
        "the fragment verbatim, then the marker"
    );
    assert!(
        run.result().contains("inference.max_tokens") && run.result().contains("256"),
        "the marker names the setting and the value in force: {:?}",
        run.result()
    );

    let inference = run.sole_inference();
    assert_eq!(inference["stop_reason"], "max_tokens");
    assert_eq!(
        inference["decision"], "text",
        "`decision` keeps its existing vocabulary; `stop_reason` is what says the turn was cut off"
    );

    let stderr = run.stderr();
    let warnings = warning_lines(&stderr);
    assert_eq!(warnings.len(), 1, "exactly one warning; stderr: {stderr}");
    assert!(
        warnings[0].contains("inference.max_tokens") && warnings[0].contains("256"),
        "the warning names the setting and the value: {}",
        warnings[0]
    );
    assert!(
        warnings[0].contains("#w-run-001"),
        "the warning links its diagnostics anchor: {}",
        warnings[0]
    );

    // A capped turn is a result, not a failure.
    assert_eq!(run.exit_status(), "ok", "trace: {}", run.trace());
    assert!(
        !common::find_file(run.workdir.as_path(), "bootstrap.log")
            .map(|p| fs::read_to_string(p).unwrap_or_default())
            .unwrap_or_default()
            .contains("W-RUN-001"),
        "a mid-session warning goes to stderr alone"
    );
}

/// A turn that ended normally is what it has always been: the model's text alone, no marker, no
/// warning, no envelope key. The only addition anywhere is `stop_reason` on the trace event.
#[test]
fn a_completed_turn_is_untouched() {
    let run = run(Some("ENDTURN"));
    run.assert_success();

    assert_eq!(
        run.result(),
        PARTIAL_TEXT,
        "exactly the model's text: no marker, no trailing newline"
    );
    assert!(
        warning_lines(&run.stderr()).is_empty(),
        "stderr: {}",
        run.stderr()
    );
    assert_eq!(run.exit_status(), "ok");

    let inference = run.sole_inference();
    assert_eq!(inference["stop_reason"], "end_turn");

    let assistants = run.recorded_assistants();
    assert_eq!(assistants.len(), 1, "record: {assistants:?}");
    assert!(
        assistants[0].get("truncated").is_none(),
        "a completed turn carries no truncation mark: {}",
        assistants[0]
    );
}

/// The conversation record keeps the model's own bytes and carries the mark beside them, on the
/// same terms as cancellation — never by editing the content a later resume replays.
#[test]
fn the_record_marks_the_envelope_and_keeps_the_content() {
    let run = run(None);
    run.assert_success();

    let assistants = run.recorded_assistants();
    assert_eq!(assistants.len(), 1, "record: {assistants:?}");
    let assistant = &assistants[0];
    assert_eq!(
        assistant["truncated"], true,
        "the mark is a runtime-only envelope key: {assistant}"
    );
    assert_eq!(
        assistant["content"],
        serde_json::json!([{"type": "text", "text": PARTIAL_TEXT}]),
        "the content is byte-identical to what the model produced: {assistant}"
    );
    assert!(
        !assistant["content"].to_string().contains("truncated"),
        "no marker inside the content: {assistant}"
    );
}

/// `stop_reason` is on every turn, not only a capped one: a field that appears solely on failure
/// cannot be used to confirm success.
#[test]
fn stop_reason_is_written_for_every_turn() {
    let run = run(Some("TOOLTHENCAP"));
    run.assert_success();

    let events = run.inference_events();
    assert_eq!(events.len(), 2, "two turns expected; got: {events:?}");
    assert_eq!(events[0]["stop_reason"], "tool_call");
    assert_eq!(events[1]["stop_reason"], "max_tokens");
    assert_eq!(run.exit_status(), "ok", "trace: {}", run.trace());
}

/// A driver that reports no stop reason at all dispatches on the empty string and lands in the
/// catch-all arm. The trace records `""` — present and empty — because the event is written before
/// the match: that is the difference between "the driver reported nothing" and "the runtime
/// recorded nothing".
#[test]
fn a_missing_stop_reason_is_recorded_as_the_empty_string() {
    let run = run(Some("NOSTOP"));

    let inference = run.sole_inference();
    assert!(
        inference.get("stop_reason").is_some(),
        "the key is present: {inference}"
    );
    assert_eq!(inference["stop_reason"], "");
    assert_eq!(
        run.exit_status(),
        "failed",
        "an unsupported stop reason still fails the turn"
    );
    assert!(
        run.result().contains("unsupported stop_reason"),
        "result: {:?}",
        run.result()
    );
}

/// A turn cut off before it produced any text writes the marker alone.
#[test]
fn an_empty_capped_turn_writes_the_marker_alone() {
    let run = run(Some("EMPTY"));
    run.assert_success();

    assert_eq!(run.result(), MARKER, "no leading blank lines");
    assert_eq!(run.exit_status(), "ok");
}

/// `mur trace steps` names the truncation on the turn's own line, so a person sees it without
/// reaching for `jq`.
#[test]
fn trace_steps_renders_a_capped_turn_as_truncated() {
    let capped = run(None);
    capped.assert_success();
    let rendered = capped.trace_steps();
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("turn 0")
                && line.contains("[truncated at inference.max_tokens]")),
        "rendered: {rendered}"
    );

    let completed = run(Some("ENDTURN"));
    completed.assert_success();
    let rendered = completed.trace_steps();
    assert!(
        !rendered.contains("truncated"),
        "a normal turn's line is unchanged: {rendered}"
    );
}
