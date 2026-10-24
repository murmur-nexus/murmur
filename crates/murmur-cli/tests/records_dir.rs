//! Every record the runtime writes about a session — `trace.jsonl`, `blobs/`, `logs/`, a delegated
//! child's `completion.json` and a records-granted hook's `hooks/` — lives in a per-session
//! records directory under `~/.murmur/records`, which the capsule's commands can read and cannot
//! change, and which outlives the session.
//!
//! The model is scripted against the anthropic driver fixture. What runs is real: the staged
//! session, the kernel tier this host reaches, the native fixture tool, the `records-hook` WASM
//! fixture and the `mur` binary. Every `scoped` and `sealed` case runs on this host or prints a
//! `[SKIP-HOST]` line saying why it cannot.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::launch_session;
use murmur_artifact::ContainmentClass;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const FIXTURE_ARTIFACT: &str = "local/tool-fixture";
const FIXTURE_TOOL: &str = common::FIXTURE_NATIVE_TOOL_NAME;
const FIXTURE_VERSION: &str = "0.1.0";
const HOOK_ARTIFACT: &str = "local/records-hook";
const HOOK_VERSION: &str = "0.1.0";
const TASK: &str = "Probe the records directory and report.";
/// What the `records-hook` fixture scores every session, and nothing a command plants says.
const HOOK_SCORE: &str = "0.75";

/// What every command a test runs is allowed to execute.
const SHELL_ALLOW: &[&str] = &[
    "bash", "cat", "cp", "mv", "ln", "rm", "truncate", "mkdir", "head", "ls",
];

// ── provider replies ─────────────────────────────────────────────────────────

fn tool_call(index: usize, id: &str, name: &str, input: Value) -> String {
    json!({
        "id": format!("msg_{index}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_end",
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

fn bash(command: &str) -> Value {
    json!({ "command": command })
}

fn probe(operation: &str, path: &Path) -> Value {
    json!({"operation": operation, "path": path.to_str().unwrap(), "text": "pwned"})
}

/// A server answering each request with the next of the calls `calls` will hold, then `done`.
fn scripted_server() -> (
    common::ScriptedServer,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let replies = std::sync::Arc::clone(&calls);
    let server = common::ScriptedServer::start_answering(64, move |_| {
        let mut replies = replies.lock().unwrap();
        if replies.is_empty() {
            end_turn("done")
        } else {
            replies.remove(0)
        }
    });
    (server, calls)
}

fn script(calls: &std::sync::Mutex<Vec<String>>, script: &[(String, String, Value)]) {
    *calls.lock().unwrap() = script
        .iter()
        .enumerate()
        .map(|(index, (id, name, input))| tool_call(index + 1, id, name, input.clone()))
        .collect();
}

// ── host gate ────────────────────────────────────────────────────────────────

/// Whether this host can run a session at `floor` with a kernel tier and a network namespace,
/// printing a `[SKIP-HOST]` line when it cannot.
fn skip_host(test: &str, floor: ContainmentClass) -> bool {
    if common::skip_without_host_support(test) {
        return true;
    }
    let achieved = capsule_runtime::detect_containment_decision(floor)
        .0
        .achieved();
    if achieved < floor {
        eprintln!("[SKIP-HOST] {test}: this host reaches {achieved}, the test needs {floor}");
        return true;
    }
    false
}

/// The Landlock ABI a `scoped` session's ruleset is built at here.
fn landlock_abi() -> u8 {
    capsule_runtime::detect_landlock_abi()
        .map(|abi| abi.min(6))
        .unwrap_or(0)
}

// ── capsule ──────────────────────────────────────────────────────────────────

/// A project: the scripted driver, the native fixture, and the `records-hook` fixture declared
/// with `hook_capabilities` (YAML under its `capabilities:`), or no hook when `None`.
struct Project {
    home: TempDir,
    _staging: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

/// The `records-hook` fixture packed with its own bundled manifest.
fn records_hook_zip(dir: &Path) -> PathBuf {
    let path = dir.join(format!("records-hook-{HOOK_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("records-hook/murmur.yaml")).unwrap())
        .unwrap();
    zip.start_file("hook.wasm", options).unwrap();
    zip.write_all(&fs::read(common::fixture_path("records-hook/hook/records-hook.wasm")).unwrap())
        .unwrap();
    zip.finish().unwrap();
    path
}

fn project(endpoint: &str, hook_capabilities: Option<&str>, native: bool) -> Project {
    let home = TempDir::new().unwrap();
    let staging = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let driver = common::create_driver_artifact(
        staging.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();
    let mut artifacts = format!(
        "  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n"
    );
    if native {
        let binary = common::fixture_native_tool_binary()
            .expect("the native fixture tool builds on a host that passes the host check");
        let manifest_bytes = fs::read(common::fixture_native_tool_manifest()).unwrap();
        let fixture = common::create_native_tool_zip(
            staging.path(),
            FIXTURE_TOOL,
            FIXTURE_VERSION,
            &manifest_bytes,
            &binary,
        );
        common::publish_local(&home, &fixture).success();
        artifacts.push_str(&format!(
            "  - name: {FIXTURE_ARTIFACT}\n    version: {FIXTURE_VERSION}\n    runtime: tool\n"
        ));
    }
    if let Some(capabilities) = hook_capabilities {
        common::publish_local(&home, &records_hook_zip(staging.path())).success();
        artifacts.push_str(&format!(
            "  - name: {HOOK_ARTIFACT}\n    version: {HOOK_VERSION}\n    runtime: hook\n    \
             capabilities:\n{capabilities}"
        ));
    }
    let shell = SHELL_ALLOW
        .iter()
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();
    let manifest = format!(
        "name: records-capsule\nversion: 0.1.0\nartifacts:\n{artifacts}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\n  shell:\n    allow:\n{shell}inference:\n  \
         transport: http\n  model: test-model\n  max_turns: 40\n  driver:\n    artifact: \
         {DRIVER_NAME}\n"
    );
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    Project {
        home,
        _staging: staging,
        project,
        manifest: manifest_path,
    }
}

const RECORDS_GRANT: &str = "      filesystem:\n        records: true\n";
const SCOPE_GRANT: &str = "      filesystem:\n        scope: .\n";

/// The text the model was handed for call `id`.
fn result_text(server: &common::ScriptedServer, id: &str) -> String {
    let text = common::tool_result_text(&server.requests(), id)
        .unwrap_or_else(|| panic!("no tool_result for {id}"));
    println!("── {id}\n{text}");
    text
}

/// A native fixture probe's `data`.
fn probe_data(server: &common::ScriptedServer, id: &str) -> Value {
    let text = result_text(server, id);
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("{id} returned no probe: {text}"))
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home);
    command
}

// ── commands and native tools cannot change the records ──────────────────────

/// One `RD|<label>|<status>|<stderr>` line per attempt.
fn attempt(label: &str, command: &str) -> String {
    format!("out=$( {{ {command}; }} 2>&1 ); echo \"RD|{label}|$?|$out\"\n")
}

/// The shell script the scoped and sealed cases run: every attempt on the trace, the records a
/// command could forge, then the read-back and the plants in the workdir.
fn attack_script(session_id: &str) -> String {
    let trace = "$MURMUR_RECORDS_DIR/trace.jsonl";
    let mut script = String::from("cd \"$PWD\"\n");
    script.push_str(&attempt(
        "trace:append",
        &format!("printf pwned >> \"{trace}\""),
    ));
    script.push_str(&attempt(
        "trace:overwrite",
        &format!("printf pwned > \"{trace}\""),
    ));
    script.push_str(&attempt(
        "trace:truncate",
        &format!("truncate -s 0 \"{trace}\""),
    ));
    script.push_str(&attempt("trace:remove", &format!("rm -f \"{trace}\"")));
    script.push_str(&attempt(
        "trace:rename-over",
        &format!("printf pwned > ./rd-src && mv -f ./rd-src \"{trace}\""),
    ));
    script.push_str(&attempt(
        "trace:link-out",
        &format!("ln \"{trace}\" ./rd-linked-trace"),
    ));
    script.push_str(&attempt(
        "eval:create",
        "printf '{}' > \"$MURMUR_RECORDS_DIR/hooks/eval.jsonl\"",
    ));
    script.push_str(&attempt(
        "completion:create",
        "printf '{}' > \"$MURMUR_RECORDS_DIR/completion.json\"",
    ));
    script.push_str(&attempt(
        "bootstrap:overwrite",
        "printf pwned > \"$MURMUR_RECORDS_DIR/logs/bootstrap.log\"",
    ));
    script.push_str(&attempt(
        "blobs:create",
        "mkdir -p \"$MURMUR_RECORDS_DIR/blobs\" && printf x > \"$MURMUR_RECORDS_DIR/blobs/x\"",
    ));
    script.push_str("echo \"RD-TRACE|$(head -n 1 \"$MURMUR_RECORDS_DIR/trace.jsonl\")\"\n");
    script.push_str("echo \"RD-DIR|$MURMUR_RECORDS_DIR\"\n");
    // The plants: files of the same names in the workdir, where every earlier reader looked.
    script.push_str(&format!(
        "printf '%s\\n' '{{\"record_type\":\"dataset_run\",\"ts\":9,\"dataset_id\":null,\
         \"case_id\":null,\"overall\":\"pass\",\"scores\":{{\"planted\":1.0}}}}' > ./eval.jsonl\n\
         printf '{{}}' > ./completion.json\n\
         mkdir -p ./.murmur/{session_id} && printf planted > ./.murmur/{session_id}/trace.jsonl\n\
         echo RD-PLANTED\n"
    ));
    script
}

/// Runs a session at `floor` and asserts every attempt to change a record failed, the runtime's
/// own records are whole afterwards, and every reader reads them rather than the plants.
fn commands_cannot_change_the_records(test: &str, floor: ContainmentClass) {
    if skip_host(test, floor) {
        return;
    }
    let (server, calls) = scripted_server();
    let p = project(&server.endpoint, Some(RECORDS_GRANT), true);
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        floor,
        Some(workdir.path()),
    );
    let records = staged.records_dir.clone();
    let session_id = staged.session_id.clone();
    let home = common::process_home();
    assert_eq!(
        records,
        common::session_records_dir(&home, workdir.path(), &session_id)
    );
    fs::write(workdir.path().join("task.md"), TASK).unwrap();

    let mut plan: Vec<(String, String, Value)> = vec![(
        "toolu_attack".to_string(),
        "bash".to_string(),
        bash(&attack_script(&session_id)),
    )];
    for (label, rel) in [
        ("trace", "trace.jsonl"),
        ("eval", "hooks/eval.jsonl"),
        ("completion", "completion.json"),
    ] {
        plan.push((
            format!("toolu_write_{label}"),
            FIXTURE_TOOL.to_string(),
            probe("write", &records.join(rel)),
        ));
    }
    plan.push((
        "toolu_read_trace".to_string(),
        FIXTURE_TOOL.to_string(),
        probe("read", &records.join("trace.jsonl")),
    ));
    script(&calls, &plan);

    launch_session(staged, |_| {}).expect("the session launches");

    // Every shell attempt failed, with the tier's own refusal.
    let attack = result_text(&server, "toolu_attack");
    let truncate_mediated = floor == ContainmentClass::Sealed || landlock_abi() >= 3;
    let mut attempts = 0;
    for line in attack.lines().filter(|line| line.starts_with("RD|")) {
        let fields: Vec<&str> = line.splitn(4, '|').collect();
        let (label, status, error) = (fields[1], fields[2], fields[3]);
        attempts += 1;
        if label == "trace:truncate" && !truncate_mediated {
            assert_eq!(
                status, "0",
                "below ABI 3 truncate(2) is not mediated: {line}"
            );
            continue;
        }
        assert_ne!(status, "0", "the attempt must fail: {line}");
        let expected: &[&str] = if label.ends_with(":link-out") {
            &["Invalid cross-device link"]
        } else {
            write_refusals(floor, &records)
        };
        assert!(
            expected.iter().any(|text| error.contains(text))
                || (label.ends_with(":rename-over")
                    && (error.contains("Permission denied")
                        || error.contains("Read-only file system"))),
            "{label} must fail with {expected:?}: {line}"
        );
    }
    assert_eq!(attempts, 10, "{attack}");
    assert!(attack.contains("RD-PLANTED"), "{attack}");
    let line = |prefix: &str| {
        attack
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("no {prefix} in {attack}"))
            .to_string()
    };
    if truncate_mediated {
        let first: Value = serde_json::from_str(&line("RD-TRACE|")).unwrap();
        assert_eq!(first["event_type"], "session_start");
        assert_eq!(first["session_id"], session_id.as_str());
    }
    assert_eq!(line("RD-DIR|"), records.display().to_string());

    // The native tool's writes fail with the text the kernel gave; its read returns the trace.
    let native_refusals = write_refusals(floor, &records);
    for label in ["trace", "eval", "completion"] {
        let data = probe_data(&server, &format!("toolu_write_{label}"));
        assert_eq!(data["ok"], false, "{label}: {data}");
        assert!(
            native_refusals
                .iter()
                .any(|text| data["error"] == format!("{text} (os error {})", errno_of(text))),
            "{label}: {data}"
        );
    }
    if truncate_mediated {
        let read = probe_data(&server, "toolu_read_trace");
        assert_eq!(read["ok"], true, "{read}");
        assert!(
            read["contents"]
                .as_str()
                .unwrap()
                .contains("\"session_start\""),
            "{read}"
        );
    }

    // After the session the records are the runtime's, whole, and kept.
    assert!(
        records.is_dir(),
        "the records directory outlived the session"
    );
    let trace_text = fs::read_to_string(records.join("trace.jsonl")).unwrap();
    if truncate_mediated {
        // An append that landed would leave a line that is no event of the runtime's.
        for line in trace_text.lines() {
            let event: Value = serde_json::from_str(line)
                .unwrap_or_else(|_| panic!("a line no runtime wrote: {line}"));
            assert!(event["event_type"].is_string(), "{line}");
        }
        let events = common::read_whole_trace(&records.join("trace.jsonl"));
        assert_eq!(events[0]["event_type"], "session_start");
        // The session's own record of what it ran under: the records directory protected, and
        // the native tool reaching the runtime directory through its environment.
        let grants = &events[0]["effective_grants"];
        assert_eq!(
            grants["records_dir"]["read_only_to_commands"], true,
            "{grants}"
        );
        assert_eq!(
            grants["records_dir"]["path"],
            records
                .parent()
                .unwrap()
                .join("<session-id>")
                .display()
                .to_string()
        );
        let native = grants["preopens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|preopen| preopen["artifact"] == FIXTURE_ARTIFACT)
            .unwrap_or_else(|| panic!("{grants}"));
        assert_eq!(native["runtime_dir_reach"], "environment", "{native}");
        assert_eq!(
            events.last().unwrap()["event_type"],
            "session_end",
            "{trace_text}"
        );
        assert!(
            !trace_text.contains("\"workdir_path_refused\""),
            "no runtime read or write was refused: {trace_text}"
        );
    }
    let eval = fs::read_to_string(records.join("hooks/eval.jsonl")).unwrap();
    assert!(eval.contains("\"dataset_run\""), "{eval}");
    assert!(eval.contains("\"records-hook\":0.75"), "{eval}");
    assert!(
        records.join("hooks/logs/hook-records-hook.log").is_file(),
        "the hook's own log is beside its eval.jsonl"
    );
    assert!(!records.join("completion.json").exists());
    for name in ["blobs", "logs"] {
        assert!(
            !workdir.path().join(name).exists(),
            "the work folder gained {name}"
        );
    }
    assert_eq!(
        fs::read_to_string(
            workdir
                .path()
                .join(".murmur")
                .join(&session_id)
                .join("trace.jsonl")
        )
        .unwrap(),
        "planted",
        "the runtime wrote nothing into the planted trace.jsonl"
    );

    // Every reader reads the records directory, never the plants.
    let shown = mur(&home)
        .args([
            "trace",
            "show",
            &session_id,
            "--workdir",
            workdir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown = String::from_utf8_lossy(&shown);
    println!("── mur trace show\n{shown}");
    assert!(shown.contains(&session_id), "{shown}");
    let scored = mur(&home)
        .args([
            "eval",
            "show",
            &session_id,
            "--workdir",
            workdir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let scored = String::from_utf8_lossy(&scored);
    println!("── mur eval show\n{scored}");
    assert!(scored.contains("records-hook"), "{scored}");
    assert!(scored.contains(HOOK_SCORE), "{scored}");
    assert!(!scored.contains("planted"), "{scored}");
}

#[test]
#[allow(clippy::print_stdout)]
fn scoped_commands_cannot_change_the_records() {
    commands_cannot_change_the_records(
        "scoped_commands_cannot_change_the_records",
        ContainmentClass::Scoped,
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn sealed_commands_cannot_change_the_records() {
    commands_cannot_change_the_records(
        "sealed_commands_cannot_change_the_records",
        ContainmentClass::Sealed,
    );
}

// ── a records-granted hook writes beneath hooks/, a scope stays in the workdir ──

/// One advisory session with the `records-hook` fixture declared `capabilities`: its work folder
/// and its records directory.
fn hook_run(capabilities: &str) -> (TempDir, PathBuf) {
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = project(&server.endpoint, Some(capabilities), false);
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let records = staged.records_dir.clone();
    fs::write(workdir.path().join("task.md"), TASK).unwrap();
    launch_session(staged, |_| {}).expect("the session runs");
    (workdir, records)
}

#[test]
fn a_hook_granted_the_records_dir_writes_beneath_hooks() {
    if common::skip_without_host_support("a_hook_granted_the_records_dir_writes_beneath_hooks") {
        return;
    }
    let (workdir, records) = hook_run(RECORDS_GRANT);
    let eval = fs::read_to_string(records.join("hooks/eval.jsonl")).unwrap();
    assert!(eval.contains("\"records-hook\":0.75"), "{eval}");
    assert!(records.join("hooks/logs/hook-records-hook.log").is_file());
    let dir = workdir.path();
    assert!(!dir.join("eval.jsonl").exists(), "{}", dir.display());
    assert!(
        !dir.join("logs/hook-records-hook.log").exists(),
        "{}",
        dir.display()
    );

    // The same hook declared `scope: .` writes into the workdir exactly as before, and its
    // records directory holds no `hooks/`.
    let (workdir, records) = hook_run(SCOPE_GRANT);
    let eval = fs::read_to_string(workdir.path().join("eval.jsonl")).unwrap();
    assert!(eval.contains("\"records-hook\":0.75"), "{eval}");
    assert!(!records.join("hooks").exists());
}

/// `mur eval run` scores each case from the records directory's `hooks/eval.jsonl`: a scoring
/// hook granted `records: true` is read there, and one declared `scope: .` writes nowhere `mur
/// eval` reads, which the run says, naming the grant that would fix it.
#[test]
fn eval_run_reads_the_records_granted_hook_s_score_and_names_the_grant_otherwise() {
    if common::skip_without_host_support(
        "eval_run_reads_the_records_granted_hook_s_score_and_names_the_grant_otherwise",
    ) {
        return;
    }
    for (grant, records) in [(RECORDS_GRANT, true), (SCOPE_GRANT, false)] {
        let server = common::ScriptedServer::start(vec![end_turn("done")]);
        let p = project(&server.endpoint, Some(grant), false);
        let task = p.project.path().join("case.md");
        fs::write(&task, TASK).unwrap();
        let dataset = p.project.path().join("dataset.jsonl");
        fs::write(
            &dataset,
            format!(
                "{}\n",
                json!({"case_id": "case-1", "task_path": task.to_str().unwrap()})
            ),
        )
        .unwrap();
        let output = mur(p.home.path())
            .current_dir(p.project.path())
            .args(["eval", "run", "--dataset", dataset.to_str().unwrap()])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("── records: {records}\n{stdout}{stderr}");
        let warned = stderr.contains("warning: no hooks/eval.jsonl in")
            && stderr.contains("capabilities.filesystem.records: true");
        if records {
            assert!(stdout.contains("result: pass"), "{stdout}{stderr}");
            assert!(!warned, "{stderr}");
        } else {
            assert!(stdout.contains("result: no_scores"), "{stdout}{stderr}");
            assert!(warned, "{stderr}");
        }
    }
}

/// `records: true` with a `scope`, on a tool, and in the capsule-wide block are each refused at
/// parse with `E-MAN-003` naming the field.
#[test]
fn records_is_refused_beside_a_scope_off_a_hook_and_capsule_wide() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let cases = [
        (
            "  - name: local/records-hook\n    version: 0.1.0\n    runtime: hook\n    \
             capabilities:\n      filesystem:\n        records: true\n        scope: .\n",
            "",
        ),
        (
            "  - name: local/echo-tool\n    version: 0.1.0\n    runtime: tool\n    \
             capabilities:\n      filesystem:\n        records: true\n",
            "",
        ),
        ("", "capabilities:\n  filesystem:\n    records: true\n"),
    ];
    for (artifacts, capabilities) in cases {
        let manifest = dir.path().join("murmur.yaml");
        let artifacts = if artifacts.is_empty() {
            "artifacts: []\n".to_string()
        } else {
            format!("artifacts:\n{artifacts}")
        };
        fs::write(
            &manifest,
            format!("name: c\nversion: 0.1.0\n{artifacts}{capabilities}"),
        )
        .unwrap();
        let output = mur(home.path())
            .args([
                "run",
                "--manifest",
                manifest.to_str().unwrap(),
                "--explain-scope",
            ])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("E-MAN-003"), "{stderr}");
        assert!(
            stderr.contains("capabilities.filesystem.records"),
            "{stderr}"
        );
    }
}

/// `--explain-scope` names a records-granted hook's surface as the records directory's
/// `hooks/`, in both renderings.
#[test]
fn explain_scope_names_a_records_granted_hook_s_surface() {
    let home = TempDir::new().unwrap();
    let dir = TempDir::new().unwrap();
    let manifest = dir.path().join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: c\nversion: 0.1.0\nartifacts:\n  - name: {HOOK_ARTIFACT}\n    version: \
             {HOOK_VERSION}\n    runtime: hook\n    capabilities:\n{RECORDS_GRANT}"
        ),
    )
    .unwrap();
    let rendered = mur(home.path())
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--explain-scope",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rendered = String::from_utf8_lossy(&rendered);
    assert!(
        rendered.contains("local/records-hook (hook): the records directory's hooks/"),
        "{rendered}"
    );
    let json = common::explain_scope_json(&home, &manifest);
    let preopen = &json["preopens"][0];
    assert_eq!(preopen["surface"], "records-hooks", "{json}");
    assert_eq!(preopen["runtime_dir_reach"], Value::Null, "{json}");
}

// ── the records directory reports and floors like the runtime directory ──────

#[test]
#[allow(clippy::print_stdout)]
fn records_dir_explain_scope_and_doctor_name_the_records_directory() {
    for (floor, mechanism) in [
        (ContainmentClass::Scoped, "landlock"),
        (ContainmentClass::Sealed, "mountns+landlock"),
    ] {
        let test = "records_dir_explain_scope_and_doctor_name_the_records_directory";
        if skip_host(test, floor) {
            return;
        }
        let server = common::ScriptedServer::start(vec![]);
        let p = project(&server.endpoint, None, false);
        let rendered = mur(p.home.path())
            .args([
                "run",
                "--manifest",
                p.manifest.to_str().unwrap(),
                "--containment",
                &floor.to_string(),
                "--explain-scope",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let rendered = String::from_utf8_lossy(&rendered);
        println!("{rendered}");
        let expected_path = common::records_group(p.home.path(), &p.project.path().join("workdir"))
            .join("<session-id>")
            .display()
            .to_string();
        let runtime_block = rendered.find("Runtime directory\n").unwrap();
        let records_block = rendered
            .find(&format!(
                "Records directory\n  path:      {expected_path}\n  commands:  read only ({mechanism})\n"
            ))
            .unwrap_or_else(|| panic!("{rendered}"));
        assert!(runtime_block < records_block, "{rendered}");
        let json: Value = serde_json::from_slice(
            &mur(p.home.path())
                .args([
                    "run",
                    "--manifest",
                    p.manifest.to_str().unwrap(),
                    "--containment",
                    &floor.to_string(),
                    "--explain-scope",
                    "--json",
                ])
                .assert()
                .success()
                .get_output()
                .stdout,
        )
        .unwrap();
        assert_eq!(
            json["records_dir"],
            json!({
                "path": expected_path,
                "read_only_to_commands": true,
                "enforced_by": mechanism,
                "not_protected_because": null,
            })
        );
        assert!(
            !p.home.path().join(".murmur/records").exists(),
            "--explain-scope creates nothing"
        );
    }

    let server = common::ScriptedServer::start(vec![]);
    let p = project(&server.endpoint, None, false);
    let doctor = mur(p.home.path())
        .current_dir(p.project.path())
        .arg("doctor")
        .assert()
        .get_output()
        .stdout
        .clone();
    let doctor = String::from_utf8_lossy(&doctor);
    println!("{doctor}");
    let runtime_block = doctor.find("Runtime directory\n  path:      ").unwrap();
    let records_block = doctor
        .find("Records directory\n  path:      ")
        .unwrap_or_else(|| panic!("{doctor}"));
    assert!(runtime_block < records_block, "{doctor}");
}

/// With `--workdir "$HOME"` both directories lie inside the workdir: `scoped` and `sealed` are
/// refused naming both, with the hint that fits the cause, and nothing is created; `advisory`
/// runs, warns once and reports the records directory unprotected.
#[test]
#[allow(clippy::print_stdout)]
fn records_dir_inside_the_workdir_refuses_a_kernel_floor() {
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = project(&server.endpoint, None, false);
    let home = p.home.path();
    for floor in ["scoped", "sealed"] {
        let output = mur(home)
            .args([
                "run",
                "--manifest",
                p.manifest.to_str().unwrap(),
                "--workdir",
                home.to_str().unwrap(),
                "--containment",
                floor,
                "--task",
                "x",
            ])
            .assert()
            .failure()
            .get_output()
            .clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("{floor}: {stderr}");
        assert!(stderr.contains("error[E-CAP-003]"), "{stderr}");
        assert!(stderr.contains("the records directory "), "{stderr}");
        assert!(
            stderr.contains(&common::records_group(home, home).display().to_string()),
            "{stderr}"
        );
        assert!(
            stderr.contains(
                "hint: run with a --workdir that does not contain ~/.murmur, or lower the \
                 declared floor to 'advisory' (capabilities.containment in murmur.yaml, \
                 containment in .murmur/config.yaml, or --containment)"
            ),
            "{stderr}"
        );
        assert!(!stderr.contains("run on a host that provides"), "{stderr}");
        assert!(!home.join(".murmur/records").exists());
        assert!(!home.join(".murmur/sessions").exists());
    }

    if common::skip_without_host_support("records_dir_inside_the_workdir_refuses_a_kernel_floor") {
        return;
    }
    let output = mur(home)
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            home.to_str().unwrap(),
            "--task",
            "x",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("W-SEC-033").count(), 1, "{stderr}");
    assert!(stderr.contains("the records directory"), "{stderr}");
    let records = common::newest_session_records(home, home).expect("a records directory");
    let events = common::read_whole_trace(&records.join("trace.jsonl"));
    let start = &events[0];
    assert_eq!(start["event_type"], "session_start");
    assert_eq!(
        start["effective_grants"]["achieved_containment"],
        "advisory"
    );
    assert_eq!(
        start["effective_grants"]["records_dir"]["read_only_to_commands"],
        false
    );
}

/// `capabilities.filesystem.workdir_exec: true` under a `scoped` floor is refused with its own
/// hint, which names the declaration and no host.
#[test]
fn workdir_exec_under_a_kernel_floor_names_its_own_fix() {
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let p = project(&server.endpoint, None, false);
    let manifest = fs::read_to_string(&p.manifest).unwrap().replace(
        "capabilities:\n",
        "capabilities:\n  filesystem:\n    workdir_exec: true\n",
    );
    fs::write(&p.manifest, manifest).unwrap();
    let output = mur(p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--containment",
            "scoped",
            "--task",
            "x",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error[E-CAP-003]"), "{stderr}");
    assert!(
        stderr.contains(
            "hint: set capabilities.filesystem.workdir_exec to false, or lower the declared \
             floor to 'advisory' (capabilities.containment in murmur.yaml, containment in \
             .murmur/config.yaml, or --containment)"
        ),
        "{stderr}"
    );
}

// ── the manifest copy is the one that was launched ───────────────────────────

#[test]
#[allow(clippy::print_stdout)]
fn the_runtime_dir_copies_the_launched_manifest() {
    if common::skip_without_host_support("the_runtime_dir_copies_the_launched_manifest") {
        return;
    }
    for launched in ["murmur-scoped.yaml", "murmur.yaml"] {
        let server = common::ScriptedServer::start(vec![
            tool_call(
                1,
                "toolu_cat",
                "bash",
                bash(&format!(
                    "cat \"$MURMUR_RUNTIME_DIR/{launched}\"; echo; ls \"$MURMUR_RUNTIME_DIR\""
                )),
            ),
            end_turn("done"),
        ]);
        let p = project(&server.endpoint, None, false);
        let scoped = fs::read_to_string(&p.manifest)
            .unwrap()
            .replace("name: records-capsule", "name: local/b");
        fs::write(p.project.path().join("murmur-scoped.yaml"), &scoped).unwrap();
        let project_manifest = fs::read_to_string(&p.manifest)
            .unwrap()
            .replace("name: records-capsule", "name: local/a");
        fs::write(&p.manifest, &project_manifest).unwrap();
        let manifest = p.project.path().join(launched);
        let workdir = TempDir::new().unwrap();
        let staged = common::stage_agent_session_at_floor(
            &p.home,
            p.project.path(),
            &manifest,
            ContainmentClass::Advisory,
            Some(workdir.path()),
        );
        let runtime_dir = staged.runtime_dir.clone();
        let murmur_md = fs::read_to_string(runtime_dir.join("MURMUR.md")).unwrap();
        fs::write(workdir.path().join("task.md"), TASK).unwrap();
        launch_session(staged, |_| {}).expect("the session runs");

        let listed = result_text(&server, "toolu_cat");
        let expected = if launched == "murmur.yaml" {
            "name: local/a"
        } else {
            "name: local/b"
        };
        assert!(listed.contains(expected), "{listed}");
        let other = if launched == "murmur.yaml" {
            "murmur-scoped.yaml"
        } else {
            "murmur.yaml"
        };
        assert!(
            !listed.lines().any(|line| line.trim() == other),
            "only the launched manifest is copied: {listed}"
        );
        let system = common::system_text(&server.requests()[0]["system"]).unwrap();
        assert!(
            system.contains(&format!(
                "Runtime files: MURMUR.md, {launched} and the installed tools are"
            )),
            "{system}"
        );
        assert!(
            murmur_md.contains(&format!("├── {launched}")),
            "{murmur_md}"
        );
    }
}

/// A capsule launched by name read no manifest, so its runtime directory holds none and neither
/// the `[Capsule]` notice nor `MURMUR.md` names one.
#[test]
#[allow(clippy::print_stdout)]
fn an_installed_capsule_s_runtime_dir_holds_and_names_no_manifest() {
    if common::skip_without_host_support(
        "an_installed_capsule_s_runtime_dir_holds_and_names_no_manifest",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_ls",
            "bash",
            bash("ls -A \"$MURMUR_RUNTIME_DIR\"; echo ===; cat \"$MURMUR_RUNTIME_DIR/MURMUR.md\""),
        ),
        end_turn("done"),
    ]);
    let p = project(&server.endpoint, None, false);
    let manifest = fs::read_to_string(&p.manifest).unwrap();
    let body = manifest
        .strip_prefix("name: records-capsule\nversion: 0.1.0\n")
        .expect("the project manifest opens with its name and version");
    common::publish_to_store(
        &p.home.path().join(".murmur").join("artifacts"),
        "local/records-capsule",
        "0.1.0",
        "capsule",
        body,
        None,
    );
    let workdir = TempDir::new().unwrap();

    mur(p.home.path())
        .args([
            "run",
            "--capsule",
            "local/records-capsule",
            "--capsule-version",
            "0.1.0",
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--task",
            TASK,
            "--no-env-file",
        ])
        .assert()
        .success();

    let listed = result_text(&server, "toolu_ls");
    let system = common::system_text(&server.requests()[0]["system"]).unwrap();
    let notice = system
        .lines()
        .find(|line| line.starts_with("Runtime files:"))
        .unwrap_or_default();
    println!("── [Capsule] notice\n{notice}");
    assert!(
        system.contains(
            "Runtime files: MURMUR.md and the installed tools are in a read-only directory"
        ),
        "{system}"
    );
    let (listing, murmur_md) = listed
        .split_once("\n===\n")
        .unwrap_or_else(|| panic!("no listing separator: {listed}"));
    let entries: Vec<&str> = listing.lines().map(str::trim).collect();
    assert!(entries.contains(&"MURMUR.md"), "{listing}");
    assert!(entries.contains(&"task.md"), "{listing}");
    assert!(
        !entries.iter().any(|entry| entry.ends_with(".yaml")),
        "an installed capsule's runtime directory holds no manifest: {listing}"
    );
    assert!(!murmur_md.contains(".yaml"), "{murmur_md}");
    assert!(!workdir.path().join("murmur.yaml").exists());
}

/// The refusal texts a write into `dir` may fail with at `floor`: a sealed root's read-only bind,
/// Landlock's, or, with `dir` under a `/tmp` the session maps onto its temporary folder, the
/// read-only bind that re-exposes it.
fn write_refusals(floor: ContainmentClass, dir: &Path) -> &'static [&'static str] {
    let tmp = fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let under_tmp = dir.starts_with(&tmp) || dir.starts_with("/tmp");
    if floor == ContainmentClass::Sealed {
        &["Read-only file system"]
    } else if under_tmp {
        &["Permission denied", "Read-only file system"]
    } else {
        &["Permission denied"]
    }
}

/// The `errno` the kernel's message `text` is the `strerror` of.
fn errno_of(text: &str) -> i32 {
    match text {
        "Read-only file system" => 30,
        _ => 13,
    }
}
