//! Every session has a private temporary folder, `tmp/` in its records directory, which a shell
//! call reaches as a literal `/tmp` wherever the host can give a command a private mount namespace.
//!
//! The model is scripted. What runs is real: the staged session, the kernel-tier sandbox, the
//! per-command mount namespace and the session's records directory.

#[path = "common/mod.rs"]
mod common;

use std::{fs, path::Path};

use assert_cmd::Command;
use capsule_runtime::launch_session;
use murmur_artifact::ContainmentClass;
use serde_json::{json, Value};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn tool_call(id: &str, tool_use_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_use_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(id: &str, text: &str) -> String {
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// A capsule that may run `bash`, declaring `containment` when given.
fn create_manifest(
    project_dir: &Path,
    endpoint: &str,
    containment: Option<&str>,
) -> std::path::PathBuf {
    let containment = containment
        .map(|class| format!("  containment: {class}\n"))
        .unwrap_or_default();
    let manifest = format!(
        concat!(
            "name: temp-folder-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "    gateway:\n",
            "      endpoint: {endpoint}\n",
            "      api_key: test-key\n",
            "capabilities:\n",
            "{containment}",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "  shell:\n",
            "    allow:\n",
            "      - bash\n",
            "      - cat\n",
            "inference:\n",
            "  transport: http\n",
            "  model: test-model\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        containment = containment,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

/// `mur run --explain-scope --json` for the manifest in `project_dir`, with `HOME` the process's
/// own — the home the staged session ran under.
fn explain_scope_json(project_dir: &Path) -> Value {
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", std::env::var_os("HOME").unwrap())
        .current_dir(project_dir)
        .args([
            "run",
            "--manifest",
            "murmur.yaml",
            "--explain-scope",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap()
}

/// One session that runs `echo x > /tmp/<name> && cat /tmp/<name>` and prints `$TMPDIR`, at
/// `floor` with the manifest declaring `containment`.
fn a_literal_tmp_write_lands_in_the_folder(
    test_name: &str,
    containment: Option<&str>,
    floor: ContainmentClass,
) {
    if common::skip_without_host_support(test_name) {
        return;
    }
    let name = format!("murmur-session-tmp-{}-{test_name}", std::process::id());
    let command = format!("echo x > /tmp/{name} && cat /tmp/{name} && printf '%s\\n' \"$TMPDIR\"");
    let server = common::ScriptedServer::start(vec![
        tool_call("msg_1", "toolu_tmp", "bash", json!({"command": command})),
        end_turn("msg_2", "Done."),
    ]);
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

    let manifest_path = create_manifest(project.path(), &server.endpoint, containment);
    let staged =
        common::stage_agent_session_at_floor(&home, project.path(), &manifest_path, floor, None);
    let records = staged.records_dir.clone();
    let session_id = staged.session_id.clone();
    fs::write(
        staged.accessible_workdir.join("task.md"),
        "Write to /tmp and read it back.",
    )
    .unwrap();

    launch_session(staged, |_| {}).expect("the session launches");

    let requests = server.requests();
    let reply = common::tool_result_text(&requests, "toolu_tmp")
        .expect("the bash call returned a tool_result");
    let trace_raw = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    let session_start: Value = trace_raw
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "session_start")
        .expect("trace carries session_start");
    let grants = &session_start["effective_grants"];
    let temp_dir = &grants["temp_dir"];
    let folder = records.join("tmp");
    println!("tool_result: {reply:?}\neffective_grants.temp_dir: {temp_dir}");

    if temp_dir["mapped_by"] != "mount-namespace" {
        // The scripted session ran; this host gave its commands no private mount namespace.
        capsule_runtime::runtime_err!(
            "[SKIP-HOST] {test_name}: this host maps no private /tmp: {temp_dir}"
        );
        return;
    }
    assert!(reply.contains("x\n/tmp"), "tool_result: {reply}");
    assert_eq!(fs::read_to_string(folder.join(&name)).unwrap(), "x\n");
    assert!(
        !Path::new("/tmp").join(&name).exists(),
        "the host's /tmp gained {name}"
    );
    let records_path = grants["records_dir"]["path"].as_str().unwrap();
    assert_eq!(
        temp_dir,
        &json!({
            "path": format!("{records_path}/tmp"),
            "tmpdir": "/tmp",
            "tmp_mapped": true,
            "mapped_by": "mount-namespace",
            "not_mapped_because": null,
        })
    );
    assert_eq!(
        records_path.replace("<session-id>", &session_id),
        records.to_string_lossy(),
        "the report's path is the session's records directory with the id left literal"
    );

    // The session recorded what `--explain-scope` reports for the same manifest and host.
    if containment.is_some() || floor == ContainmentClass::Advisory {
        assert_eq!(&explain_scope_json(project.path())["temp_dir"], temp_dir);
    }
}

#[test]
#[allow(clippy::print_stdout)]
fn a_literal_tmp_write_lands_in_the_folder_at_the_default_class() {
    a_literal_tmp_write_lands_in_the_folder(
        "a_literal_tmp_write_lands_in_the_folder_at_the_default_class",
        None,
        ContainmentClass::Advisory,
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn a_literal_tmp_write_lands_in_the_folder_at_scoped() {
    a_literal_tmp_write_lands_in_the_folder(
        "a_literal_tmp_write_lands_in_the_folder_at_scoped",
        Some("scoped"),
        ContainmentClass::Scoped,
    );
}

/// `mur run --explain-scope` prints `Temporary folder` after `Records directory`, at the default
/// class and at `--containment scoped`, `--json` carries `temp_dir`, and `mur doctor` prints the
/// same block.
#[test]
#[allow(clippy::print_stdout)]
fn explain_scope_and_doctor_print_the_temporary_folder() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    create_manifest(project.path(), "http://127.0.0.1:9", None);
    let mur = |args: &[&str]| {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", home.path())
            .current_dir(project.path())
            .args(args)
            .assert()
    };
    for containment in [None, Some("scoped")] {
        let mut args = vec!["run", "--manifest", "murmur.yaml", "--explain-scope"];
        if let Some(class) = containment {
            args.extend(["--containment", class]);
        }
        let rendered = mur(&args).success().get_output().stdout.clone();
        let rendered = String::from_utf8_lossy(&rendered);
        println!("{rendered}");
        let records = rendered
            .find("Records directory\n")
            .unwrap_or_else(|| panic!("{rendered}"));
        let temp = rendered
            .find("Temporary folder\n  path:      ")
            .unwrap_or_else(|| panic!("{rendered}"));
        assert!(records < temp, "{rendered}");
        let block = &rendered[temp..];
        assert!(block.contains("\n  TMPDIR:    "), "{rendered}");
        assert!(block.contains("\n  /tmp:      "), "{rendered}");

        args.push("--json");
        let json: Value =
            serde_json::from_slice(&mur(&args).success().get_output().stdout).unwrap();
        let temp_dir = &json["temp_dir"];
        let records_path = json["records_dir"]["path"].as_str().unwrap();
        assert_eq!(temp_dir["path"], format!("{records_path}/tmp"), "{json}");
        let mapped = temp_dir["tmp_mapped"].as_bool().unwrap();
        assert_eq!(temp_dir["not_mapped_because"].is_null(), mapped, "{json}");
        if mapped {
            assert_eq!(temp_dir["tmpdir"], "/tmp", "{json}");
            assert!(
                block.contains(&format!(
                    "/tmp:      mapped ({})\n",
                    temp_dir["mapped_by"].as_str().unwrap()
                )),
                "{rendered}"
            );
        } else {
            assert_eq!(temp_dir["tmpdir"], temp_dir["path"], "{json}");
            assert!(block.contains("/tmp:      not mapped — "), "{rendered}");
        }
        assert!(
            !home.path().join(".murmur/records").exists(),
            "--explain-scope creates nothing"
        );
    }

    let doctor = mur(&["doctor"]).get_output().stdout.clone();
    let doctor = String::from_utf8_lossy(&doctor);
    println!("{doctor}");
    let records = doctor
        .find("Records directory\n  path:      ")
        .unwrap_or_else(|| panic!("{doctor}"));
    let temp = doctor
        .find("Temporary folder\n  path:      ")
        .unwrap_or_else(|| panic!("{doctor}"));
    assert!(records < temp, "{doctor}");
}
