//! An agent capsule's turn cap, per-turn output cap, context window, input ceiling and input
//! budget: no turn cap unless the manifest sets `inference.max_turns`, the output cap and window
//! from the driver's own `models:` table unless the manifest sets `inference.max_tokens` and
//! `context.max_tokens`, the ceiling from the table alone, and the budget
//! `min(ceiling, window − cap)`. A malformed table is refused by `mur install`, `mur publish` and
//! the launch.
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
/// A driver table listing `test-model` with the input ceiling the OpenAI 1050000 window carries.
const CEILED_TABLE: &str = "models:\n  test-model:\n    max_output_tokens: 128000\n    \
                            context_window: 1050000\n    max_input_tokens: 922000\n";
/// A driver table whose `test-model` entry declares a ceiling above its window.
const CEILING_ABOVE_WINDOW: &str = "models:\n  test-model:\n    max_output_tokens: 128000\n    \
                                    context_window: 1050000\n    max_input_tokens: 1050001\n";
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

/// The anthropic driver fixture packed into `dir`, its manifest declaring `models` as its
/// `models:` table (empty for none).
fn driver_zip(dir: &TempDir, models: &str) -> PathBuf {
    common::create_driver_artifact_with_auth(
        dir.path(),
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        &format!("{}{models}", common::FIXTURE_DRIVER_AUTH),
    )
}

/// A scratch `HOME` with the driver published into it, its manifest declaring `models` as its
/// `models:` table (empty for none).
fn home_with_driver(models: &str) -> TempDir {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    common::publish_local(&home, &driver_zip(&artifacts, models)).success();
    home
}

/// A scratch `HOME` whose store holds a driver shipping `models`, a table `mur publish` refuses:
/// a well-formed driver is published, then its stored payload replaced.
fn home_with_stored_driver(models: &str) -> TempDir {
    let home = home_with_driver(TABLE);
    let artifacts = TempDir::new().unwrap();
    common::overwrite_stored_payload(
        &home,
        DRIVER,
        DRIVER_VERSION,
        &driver_zip(&artifacts, models),
    );
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

/// `session_start.model_limits` for a driver entry with no input ceiling: the budget is the
/// window less the cap, or none with no window.
fn limits(cap: u32, cap_source: &str, window: Option<u32>, window_source: &str) -> Value {
    json!({
        "max_output_tokens": cap,
        "max_output_tokens_source": cap_source,
        "context_window": window,
        "context_window_source": window_source,
        "max_input_tokens": null,
        "max_input_tokens_source": "none",
        "input_budget": window.map(|window| window - cap),
        "input_budget_basis": if window.is_some() {
            "context_window_less_output_cap"
        } else {
            "none"
        },
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

/// A stored driver shipping a malformed table refuses the launch, naming the driver and the
/// field, before any request leaves: an out-of-range limit, and a ceiling above the window.
#[test]
fn malformed_table_is_refused() {
    for (models, field) in [
        (
            "models:\n  test-model:\n    max_output_tokens: 0\n    context_window: 1\n",
            "models.test-model.max_output_tokens",
        ),
        (CEILING_ABOVE_WINDOW, "models.test-model.max_input_tokens"),
    ] {
        let home = home_with_stored_driver(models);
        let server = common::ScriptedServer::start(script(0));
        let project = project(&manifest(&server.endpoint, "  max_tokens: 4096\n", ""));
        let run = run(&home, &project);
        assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
        let refusal = run.lines("error[E-RUN-052]");
        assert_eq!(refusal.len(), 1, "{}", run.context());
        assert!(
            refusal[0].contains(
                "driver local/murmur-driver-anthropic@0.1.0 ships a malformed models: table"
            ) && refusal[0].contains(field),
            "{}",
            run.context()
        );
        assert!(server.requests().is_empty(), "{}", run.context());
    }
}

// ── The store refuses a malformed table ───────────────────────────────────────

/// The `E-REG-011` line `mur install` and `mur publish` print for [`CEILING_ABOVE_WINDOW`].
const STORE_REFUSAL: &str = "error[E-REG-011]: local/murmur-driver-anthropic@0.1.0 cannot be \
                             stored: its murmur.yaml ships a malformed models: table: murmur.yaml: \
                             field 'models.test-model.max_input_tokens' is 1050001, above this \
                             entry's context_window of 1050000; an input ceiling is at most the \
                             window";

/// Whether `home`'s global store, or `project`'s, holds any file for the driver.
fn stored(store_root: &std::path::Path) -> bool {
    let registry =
        murmur_artifact::LocalRegistry::new(store_root.join(".murmur").join("artifacts"));
    let name = murmur_artifact::ArtifactName::parse(DRIVER).unwrap();
    registry.artifact_path_for(&name, DRIVER_VERSION).exists()
        || registry.sha256_path_for(&name, DRIVER_VERSION).exists()
}

/// `mur install <zip>` of a driver whose ceiling is above its window is refused, `E-REG-011`, and
/// neither the store nor `murmur.lock` is written.
#[test]
fn install_refuses_a_ceiling_above_the_window() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let zip = driver_zip(&artifacts, CEILING_ABOVE_WINDOW);
    let project = project("name: p\nversion: 0.1.0\nartifacts: []\n");
    let output = common::install_artifact_to_project_with_home(
        project.dir.path(),
        &home,
        &zip,
        &["--no-precompile"],
    )
    .failure()
    .get_output()
    .clone();
    let refused = Run::of(output);
    assert_eq!(
        refused.lines("error[E-REG-011]"),
        [STORE_REFUSAL],
        "{}",
        refused.context()
    );
    assert!(
        refused
            .stderr
            .contains("hint: the driver's own murmur.yaml is at fault"),
        "{}",
        refused.context()
    );
    assert!(!stored(project.dir.path()), "{}", refused.context());
    assert!(!stored(home.path()), "{}", refused.context());
    assert!(
        !project.dir.path().join("murmur.lock").exists()
            || !fs::read_to_string(project.dir.path().join("murmur.lock"))
                .unwrap()
                .contains("murmur-driver-anthropic"),
        "{}",
        refused.context()
    );
}

/// `mur publish` refuses the same payload the same way, and writes nothing.
#[test]
fn install_by_publish_refuses_a_ceiling_above_the_window() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let output = common::publish_local(&home, &driver_zip(&artifacts, CEILING_ABOVE_WINDOW))
        .failure()
        .get_output()
        .clone();
    let refused = Run::of(output);
    assert_eq!(
        refused.lines("error[E-REG-011]"),
        [STORE_REFUSAL],
        "{}",
        refused.context()
    );
    assert!(!stored(home.path()), "{}", refused.context());
}

/// A well-formed driver declaring a ceiling installs and publishes.
#[test]
fn install_accepts_a_well_formed_ceiling() {
    let home = TempDir::new().unwrap();
    let artifacts = TempDir::new().unwrap();
    let zip = driver_zip(&artifacts, CEILED_TABLE);
    let project = project("name: p\nversion: 0.1.0\nartifacts: []\n");
    common::install_artifact_to_project_with_home(
        project.dir.path(),
        &home,
        &zip,
        &["--no-precompile"],
    )
    .success();
    assert!(stored(project.dir.path()));
    common::publish_local(&home, &zip).success();
    assert!(stored(home.path()));
}

// ── The input budget ──────────────────────────────────────────────────────────

/// The input ceiling bounds the budget when it is the smaller term, and `session_start` and
/// every agent-loop `inference` line carry it.
#[test]
fn budget_from_the_input_ceiling() {
    let home = home_with_driver(CEILED_TABLE);
    let server = common::ScriptedServer::start(script(1));
    let project = project(&manifest(&server.endpoint, "", ""));
    let run = run(&home, &project);
    assert_eq!(run.output.status.code(), Some(0), "{}", run.context());
    let trace = run.trace();
    assert_eq!(
        session_start(&trace)["model_limits"],
        json!({
            "max_output_tokens": 128000,
            "max_output_tokens_source": "driver",
            "context_window": 1050000,
            "context_window_source": "driver",
            "max_input_tokens": 922000,
            "max_input_tokens_source": "driver",
            "input_budget": 922000,
            "input_budget_basis": "max_input_tokens",
        })
    );
    let lines = events(&trace, "inference");
    assert_eq!(lines.len(), 2, "{trace:?}");
    for line in lines {
        assert_eq!(line["max_output_tokens"], 128000, "{line}");
        assert_eq!(line["input_budget"], 922000, "{line}");
    }
}

/// An output cap that fills the window is refused before any request leaves, `E-RUN-055`, with
/// every number and its source; `mur doctor` reports the same refusal as a fix and exits 1.
#[test]
fn budget_refused_when_the_cap_fills_the_window() {
    let home = home_with_driver(common::FIXTURE_DRIVER_MODELS);
    let server = common::ScriptedServer::start(script(0));
    let inference_extra = "  max_tokens: 200000\n";
    let top_extra = "context:\n  max_tokens: 200000\n";
    let project = project(&manifest(&server.endpoint, inference_extra, top_extra));
    let run = run(&home, &project);
    assert_ne!(run.output.status.code(), Some(0), "{}", run.context());
    let expected = "error[E-RUN-055]: driver choice 'primary' has no input budget: its context \
                    window of 200000 (manifest) less its output cap of 200000 (manifest) leaves \
                    no input tokens; input ceiling none (model 'test-model', driver \
                    local/murmur-driver-anthropic@0.1.0)";
    assert_eq!(
        run.lines("error[E-RUN-055]"),
        [expected],
        "{}",
        run.context()
    );
    assert!(
        run.stderr.contains(
            "hint: lower inference.max_tokens below the context window, or raise \
             context.max_tokens above the output cap"
        ),
        "{}",
        run.context()
    );
    assert!(server.requests().is_empty(), "{}", run.context());

    let doctor_project = self::project(&manifest("http://127.0.0.1:9", inference_extra, top_extra));
    let doctor = doctor(&home, &doctor_project);
    assert_eq!(doctor.output.status.code(), Some(1), "{}", doctor.context());
    assert!(
        doctor
            .stdout
            .lines()
            .any(|line| line == format!("  \u{2717}  {expected}").as_str()),
        "{}",
        doctor.context()
    );
    assert!(
        doctor
            .stdout
            .lines()
            .any(|line| line.contains("Fix:") && line.contains("lower inference.max_tokens")),
        "{}",
        doctor.context()
    );
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
             (driver), context window 400000 (driver), input ceiling none (none), input budget \
             334464 (context_window_less_output_cap)"
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
             (manifest), context window none (none), input ceiling none (none), input budget \
             none (none)"
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

/// The line for a driver declaring an input ceiling, exactly.
#[test]
fn doctor_prints_the_input_ceiling_and_budget() {
    let home = home_with_driver(CEILED_TABLE);
    let run = doctor(&home, &doctor_project(""));
    assert!(
        run.stdout.lines().any(|line| line
            == "  \u{2713}  primary (local/murmur-driver-anthropic@0.1.0, model test-model): \
                output cap 128000 (driver), context window 1050000 (driver), input ceiling 922000 \
                (driver), input budget 922000 (max_input_tokens)"),
        "{}",
        run.context()
    );
}
