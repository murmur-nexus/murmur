//! End-to-end coverage for `shell-event.recipe`: what a policy hook is shown when the call it
//! gates names a build-tool recipe rather than a command.
//!
//! The hook denies, so nothing is executed and no `just` binary has to exist on the host. The
//! hook component is hand-authored WAT compiled in-test, so nothing here depends on a
//! `default-artifacts` checkout and no case is `#[ignore]`d.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use common::hook_wat::{create_hook_zip, shell_echo_deny_hook_wasm};
use serde_json::{json, Value};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The marker a justfile puts in the `build` recipe's body. A hook that echoes it back was
/// shown the file's contents and not just the name that selects them.
const MARKER_ALPHA: &str = "MURMUR-RECIPE-MARKER-ALPHA";
const MARKER_BETA: &str = "MURMUR-RECIPE-MARKER-BETA";

/// The file the `build` recipe would create. Its absence is what says the body was read and not
/// run.
const RECIPE_ARTIFACT: &str = "built.txt";

fn justfile(marker: &str) -> String {
    format!("build:\n  echo {marker}\n  : > {RECIPE_ARTIFACT}\n")
}

/// A capsule manifest declaring the driver, one denying `on-shell` hook, and `shell_binary` as
/// its only shell grant.
fn create_manifest(project_dir: &Path, endpoint: &str, shell_binary: &str) -> PathBuf {
    let manifest = format!(
        concat!(
            "name: recipe-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "  - name: echoer\n",
            "    version: 0.1.0\n",
            "    runtime: hook\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - {shell_binary}\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        shell_binary = shell_binary,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// The identity the echoing policy hook was handed, split back out of the reason it denied with.
struct Identity {
    binary: String,
    script: String,
    recipe: String,
    argv: String,
}

/// What one launched session left behind.
struct Session {
    trace: Vec<Value>,
    trace_raw: String,
    workdir: PathBuf,
    _project: tempfile::TempDir,
}

impl Session {
    fn events(&self, event_type: &str) -> Vec<&Value> {
        self.trace
            .iter()
            .filter(|e| e["event_type"] == event_type)
            .collect()
    }

    /// The one `call_denied` line, with the whole trace in the failure message.
    fn denial(&self) -> &Value {
        let denials = self.events("call_denied");
        assert_eq!(
            denials.len(),
            1,
            "expected exactly one call_denied line; trace was:\n{}",
            self.trace_raw
        );
        denials[0]
    }

    fn identity(&self) -> Identity {
        let reason = self.denial()["reason"].as_str().unwrap().to_string();
        let mut parts = reason.splitn(4, '|');
        Identity {
            binary: parts.next().unwrap().to_string(),
            script: parts.next().unwrap().to_string(),
            recipe: parts.next().unwrap().to_string(),
            argv: parts.next().unwrap().trim_end().to_string(),
        }
    }

    fn recipe_ran(&self) -> bool {
        self.workdir.join(RECIPE_ARTIFACT).exists()
    }
}

/// Publish the driver and the hook, stage the capsule, seed the workdir through `setup`, run the
/// session, and collect what it left.
fn run_session(command: &str, shell_binary: &str, setup: impl FnOnce(&Path)) -> Session {
    let responses = vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_recipe",
                "name": shell_binary,
                "input": {"command": command},
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "Understood."}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ];

    let server = common::ScriptedServer::start(responses);
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let hook_artifact = create_hook_zip(
        artifact_dir.path(),
        "echoer",
        "on-shell",
        "deny",
        &shell_echo_deny_hook_wasm(),
    );
    common::publish_local(&home, &hook_artifact).success();

    let manifest_path = create_manifest(project.path(), &server.endpoint, shell_binary);
    let staged = common::stage_agent_session(&home, project.path(), &manifest_path);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Do the thing.").unwrap();
    setup(&workdir);

    launch_session(staged, |_| {}).expect("the session must launch whatever the policy decides");

    let trace_raw = fs::read_to_string(workdir.join("trace.jsonl")).unwrap_or_default();
    let trace: Vec<Value> = trace_raw
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("every trace line must be valid JSON"))
        .collect();

    Session {
        trace,
        trace_raw,
        workdir,
        _project: project,
    }
}

// ── Scenarios ────────────────────────────────────────────────────────────────

/// The hook gating `just build` is shown the body the justfile gives `build`, and the call is
/// refused before anything in that body runs.
#[test]
fn the_hook_is_shown_the_recipe_body() {
    if common::skip_without_host_support("the_hook_is_shown_the_recipe_body") {
        return;
    }
    let session = run_session("build", "just", |workdir| {
        fs::write(workdir.join("justfile"), justfile(MARKER_ALPHA)).unwrap();
    });

    let identity = session.identity();
    assert!(
        identity.recipe.contains(MARKER_ALPHA),
        "the recipe body reaches the hook: {}",
        identity.recipe
    );
    assert_eq!(identity.binary, "just");
    assert_eq!(
        identity.script, "",
        "a build tool is not an interpreter form"
    );
    assert_eq!(identity.argv, "build");

    assert!(!session.recipe_ran(), "the denied recipe must not run");
    assert!(
        session.events("shell").is_empty(),
        "nothing ran, so nothing is recorded as having run:\n{}",
        session.trace_raw
    );
}

/// Rewriting the justfile changes what the hook decides on while the argv stays `["build"]` —
/// which is the substitution a policy gating a name alone cannot see.
#[test]
fn editing_the_recipe_changes_what_the_hook_sees() {
    if common::skip_without_host_support("editing_the_recipe_changes_what_the_hook_sees") {
        return;
    }
    let first = run_session("build", "just", |workdir| {
        fs::write(workdir.join("justfile"), justfile(MARKER_ALPHA)).unwrap();
    })
    .identity();
    let second = run_session("build", "just", |workdir| {
        fs::write(workdir.join("justfile"), justfile(MARKER_BETA)).unwrap();
    })
    .identity();

    assert!(first.recipe.contains(MARKER_ALPHA) && !first.recipe.contains(MARKER_BETA));
    assert!(second.recipe.contains(MARKER_BETA) && !second.recipe.contains(MARKER_ALPHA));
    assert_eq!(first.argv, "build");
    assert_eq!(second.argv, first.argv, "the argv is what did not move");
}

/// A workdir with no justfile leaves the recipe absent. The call is still refused and the
/// session still completes: an unresolved recipe is not an error.
#[test]
fn an_unresolvable_recipe_is_absent_not_an_error() {
    if common::skip_without_host_support("an_unresolvable_recipe_is_absent_not_an_error") {
        return;
    }
    let session = run_session("build", "just", |_| {});

    let identity = session.identity();
    assert_eq!(identity.recipe, "", "no justfile, no body");
    assert_eq!(identity.argv, "build");
    assert_eq!(session.denial()["event"], "on-shell");
    assert_eq!(session.denial()["hook_name"], "echoer");
}
