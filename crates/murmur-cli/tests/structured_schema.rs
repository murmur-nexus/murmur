//! What `mur run` actually delivers to a capsule, and what it leaves behind.
//!
//! A task reaches a capsule as text and comes back as text: `--task` writes `task.md`,
//! `read_task` reads it (then falls back to `input.txt`), the text reaches the model verbatim,
//! and `write_result` writes `out/result.txt` and nothing else. There is no structured task
//! envelope, and there is not going to be one — an envelope has no reader on this side, so it
//! would arrive as prose for the model to decode. A delegating parent obeys the same contract:
//! `plan::capsule_task_text` refuses a capsule step whose input is anything but task text.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::launch_session;
use predicates::prelude::*;
use serde_json::json;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// `--task <text>` reaches the model as the user message, verbatim.
#[test]
fn task_flag_text_reaches_the_model_verbatim() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    run_mur(&home, &manifest_path, &["--task", "Build X"]);

    assert_eq!(first_user_message(&server), "Build X");
}

/// `--task <path>` copies the file's contents instead of passing the path along.
///
/// The trigger is the path existing, not an `@` sigil — no `@`-prefix expansion exists on this
/// flag, so `@/some/path` would be delivered as that literal string.
#[test]
fn task_flag_path_delivers_file_contents() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let task_file = tempfile::NamedTempFile::new().unwrap();
    fs::write(task_file.path(), "Build from file").unwrap();

    let arg = task_file.path().display().to_string();
    run_mur(&home, &manifest_path, &["--task", &arg]);

    assert_eq!(first_user_message(&server), "Build from file");
}

/// With no `--task`, an existing `task.md` in the workdir is what the agent runs.
///
/// `read_task` looks in the workdir, not the project directory, and nothing stages a
/// project-level `task.md` into it — so that is where the file has to be.
#[test]
fn no_flags_falls_back_to_task_md() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let workdir = workdir_of(&manifest_path);
    fs::create_dir_all(&workdir).unwrap();
    fs::write(workdir.join("task.md"), "Legacy markdown task").unwrap();

    run_mur(&home, &manifest_path, &[]);

    assert_eq!(first_user_message(&server), "Legacy markdown task");
}

/// A clean turn leaves the assistant's final text in `out/result.txt`.
#[test]
fn result_txt_written_on_clean_completion() {
    let server = end_turn_server("structured output");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let session_id = run_mur(&home, &manifest_path, &["--task", "Finish cleanly"]);

    assert_eq!(
        fs::read_to_string(session_dir(&manifest_path, &session_id).join("out/result.txt"))
            .unwrap(),
        "structured output"
    );
}

/// A driver-side error response still terminates cleanly and still leaves `out/result.txt`
/// rather than trapping the session.
#[test]
fn result_txt_written_on_error_response() {
    let server = common::ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [],
        "stop_reason": "error",
        "error": "driver-side failure",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged =
        common::stage_agent_session(&home, manifest_path.parent().unwrap(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Trigger an error").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent error response should not trap");
    assert!(launched.workdir.join("out/result.txt").exists());
}

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    fs::write(
        project.path().join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: structured-agent\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()])
}

/// The text of the first user message the driver was asked to send.
fn first_user_message(server: &common::ScriptedServer) -> String {
    let requests = server.requests();
    requests[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no user message text in first request: {:?}", requests[0]))
        .to_string()
}

/// Run `mur run` against the pinned workdir, returning the session id it reports.
fn run_mur(home: &TempDir, manifest_path: &Path, extra_args: &[&str]) -> String {
    let workdir = workdir_of(manifest_path);
    fs::create_dir_all(&workdir).unwrap();

    let mut cmd = Command::cargo_bin("mur").unwrap();
    let workdir_arg = workdir.to_str().unwrap().to_string();
    let mut args = vec![
        "run",
        "--manifest",
        manifest_path.to_str().unwrap(),
        "--workdir",
        &workdir_arg,
    ];
    args.extend_from_slice(extra_args);

    let output = cmd
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(args)
        .assert()
        .success()
        .stdout(predicate::str::contains("status:  ok"))
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(output).unwrap();
    stdout
        .split("session: ")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .unwrap_or_else(|| panic!("no session id in stdout: {stdout}"))
        .trim()
        .to_string()
}

/// Where a run's own files land, given the id it reported.
///
/// A pinned `--workdir` is the *accessible* directory — what the agent's tools see and where
/// `task.md` is read from. The session's own tree, including `out/`, hangs off it under
/// `.murmur/<session id>` (see `runtime::stage_session`).
fn session_dir(manifest_path: &Path, session_id: &str) -> PathBuf {
    workdir_of(manifest_path).join(".murmur").join(session_id)
}

/// The workdir every run in this file is pinned to via `--workdir`.
///
/// Pinned rather than discovered: `mur run` prints url/session/status and no longer prints a
/// `workdir:` line to scrape, and the effective workdir is whatever `stage_session` settles on,
/// so naming it up front is the only way for a test to look inside afterwards.
fn workdir_of(manifest_path: &Path) -> PathBuf {
    manifest_path.parent().unwrap().join("workdir")
}
