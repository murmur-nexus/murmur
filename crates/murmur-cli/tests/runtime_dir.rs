//! The files the runtime writes for its own use — `MURMUR.md`, `task.md`, the manifest copy,
//! installed tools and submitted plans — live in a per-session runtime directory outside the
//! workdir, which the capsule's commands can read and cannot change.
//!
//! The model is scripted against the anthropic driver fixture. What runs is real: the staged
//! session, the kernel tier this host reaches, the native fixture tool, the WASM probe fixture and
//! the `mur` binary. Every `scoped` and `sealed` case runs on this host or prints a `[SKIP-HOST]`
//! line saying why it cannot.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use capsule_runtime::launch_session;
use murmur_artifact::ContainmentClass;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const FIXTURE_ARTIFACT: &str = "local/tool-fixture";
const FIXTURE_TOOL: &str = common::FIXTURE_NATIVE_TOOL_NAME;
const FIXTURE_VERSION: &str = "0.1.0";
const SKILL: &str = "rd-skill";
const SKILL_BODY: &str = "# rd skill\n\nThe original guidance of the runtime-directory skill.\n";
const TASK: &str = "Probe the runtime directory and report.";
const PROBE_TOOL: &str = "runtime-dir-probe";
/// The native fixture published under a second name, only to the test store, for a pull.
const PULLED_ARTIFACT: &str = "local/tool-pulled";
const PULLED_TOOL: &str = "tool-pulled";
const PROBE_ARTIFACT: &str = "local/runtime-dir-probe";
/// The runtime-files line of a [`project`] declaring no WASM tool: `bash` is its one tool that
/// opens the runtime directory by a fixed name.
const NOTICE: &str = "Runtime files: MURMUR.md, murmur.yaml and the installed tools are in a \
                      read-only directory: $MURMUR_RUNTIME_DIR in a shell command.";

/// What every command a test runs is allowed to execute.
const SHELL_ALLOW: &[&str] = &[
    "bash", "cat", "cp", "mv", "ln", "rm", "truncate", "wc", "stat", "head", "tail", "ls",
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

/// A project: the scripted driver, the native fixture, a local skill, and `extra` YAML under
/// `artifacts:` and `capabilities:`.
struct Project {
    home: TempDir,
    _staging: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

#[derive(Default)]
struct Spec<'a> {
    native_fixture: bool,
    plan: bool,
    extra_artifacts: &'a str,
    extra_capabilities: &'a str,
    /// Binaries allowed beside [`SHELL_ALLOW`].
    extra_shell: &'a [&'a str],
    env_allow: &'a [&'a str],
}

fn project(endpoint: &str, spec: &Spec<'_>) -> Option<Project> {
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
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\n  - name: {SKILL}\n    \
         source: ./skills/{SKILL}/skill.md\n    runtime: skill\n"
    );
    if spec.native_fixture {
        let binary = common::fixture_native_tool_binary()?;
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
    artifacts.push_str(spec.extra_artifacts);
    let skill_dir = project.path().join("skills").join(SKILL);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("skill.md"), SKILL_BODY).unwrap();

    let shell = SHELL_ALLOW
        .iter()
        .chain(spec.extra_shell)
        .map(|binary| format!("      - {binary}\n"))
        .collect::<String>();
    let plan = if spec.plan {
        "  plan:\n    submit: true\n"
    } else {
        ""
    };
    let env = if spec.env_allow.is_empty() {
        String::new()
    } else {
        let names = spec
            .env_allow
            .iter()
            .map(|name| format!("      - {name}\n"))
            .collect::<String>();
        format!("  env:\n    allow:\n{names}")
    };
    let manifest = format!(
        "name: rd-capsule\nversion: 0.1.0\nartifacts:\n{artifacts}capabilities:\n  network:\n    \
         allow:\n      - {endpoint}\n  shell:\n    allow:\n{shell}{plan}{env}{}inference:\n  \
         transport: http\n  model: test-model\n  max_turns: 40\n  driver:\n    artifact: \
         {DRIVER_NAME}\n",
        spec.extra_capabilities
    );
    let manifest_path = project.path().join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    Some(Project {
        home,
        _staging: staging,
        project,
        manifest: manifest_path,
    })
}

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

// ── commands and native tools cannot change the runtime files ────────────────

/// One `RD|<label>|<status>|<stderr>` line per attempt, each made against `target`.
fn attack_lines(label: &str, target: &str) -> String {
    let attempts = [
        ("overwrite", format!("printf pwned > \"{target}\"")),
        ("append", format!("printf pwned >> \"{target}\"")),
        ("truncate", format!("truncate -s 0 \"{target}\"")),
        ("remove", format!("rm -f \"{target}\"")),
        (
            "rename-over",
            format!("printf pwned > ./rd-src && mv -f ./rd-src \"{target}\""),
        ),
        ("link-out", format!("ln \"{target}\" ./rd-linked-{label}")),
    ];
    attempts
        .iter()
        .map(|(attempt, command)| {
            format!("out=$( {{ {command}; }} 2>&1 ); echo \"RD|{label}:{attempt}|$?|$out\"\n")
        })
        .collect()
}

/// The shell script the scoped and sealed cases run: every attempt on every runtime file, a
/// create in the runtime directory, then the read-backs.
fn attack_script() -> String {
    let mut script = String::from("cd \"$PWD\"\n");
    for (label, rel) in runtime_files() {
        script.push_str(&attack_lines(label, &format!("$MURMUR_RUNTIME_DIR/{rel}")));
    }
    script.push_str(
        "out=$( { printf x > \"$MURMUR_RUNTIME_DIR/created\"; } 2>&1 ); echo \"RD|create|$?|$out\"\n",
    );
    script.push_str("echo \"RD-TASK|$(cat \"$MURMUR_RUNTIME_DIR/task.md\")\"\n");
    script.push_str(&format!(
        "echo \"RD-SKILL|$(cat \"$MURMUR_RUNTIME_DIR/tools/{SKILL}/skill.md\" | head -c 40)\"\n"
    ));
    script.push_str("echo \"RD-MURMUR|$(head -c 40 \"$MURMUR_RUNTIME_DIR/MURMUR.md\")\"\n");
    script.push_str(&format!(
        "echo \"RD-ENTRY|$(wc -c < \"$MURMUR_RUNTIME_DIR/tools/{FIXTURE_TOOL}/{FIXTURE_TOOL}\")\"\n"
    ));
    script.push_str("echo \"RD-DIR|$MURMUR_RUNTIME_DIR\"\n");
    script
}

/// Every runtime file the attacks aim at, relative to the runtime directory.
fn runtime_files() -> Vec<(&'static str, String)> {
    vec![
        ("skill", format!("tools/{SKILL}/skill.md")),
        ("entrypoint", format!("tools/{FIXTURE_TOOL}/{FIXTURE_TOOL}")),
        ("task", "task.md".to_string()),
        ("murmur", "MURMUR.md".to_string()),
    ]
}

/// The plan the scoped and sealed cases submit: the first step tries to overwrite its own plan
/// file, the second reads it back.
fn self_rewriting_plan() -> Value {
    json!({
        "id": "rd-plan",
        "steps": [
            {
                "id": "rewrite",
                "shell": "bash -c 'printf pwned > \"$MURMUR_RUNTIME_DIR/plans/plan-1.json\"; echo rewrite-status=$?'"
            },
            {
                "id": "readback",
                "shell": "bash -c 'echo readback-ran; cat \"$MURMUR_RUNTIME_DIR/plans/plan-1.json\"'",
                "depends_on": ["rewrite"]
            }
        ]
    })
}

/// Runs a session at `floor` with the native tool's probes, and asserts every attempt to
/// change a runtime file failed and every read-back returned the original.
fn commands_cannot_change_the_runtime_files(test: &str, floor: ContainmentClass) {
    if skip_host(test, floor) {
        return;
    }
    let mut calls: Vec<(String, String, Value)> = vec![(
        "toolu_attack".to_string(),
        "bash".to_string(),
        bash(&attack_script()),
    )];
    let server_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let server = {
        let replies = std::sync::Arc::clone(&server_calls);
        common::ScriptedServer::start_answering(64, move |_| {
            let mut replies = replies.lock().unwrap();
            if replies.is_empty() {
                end_turn("done")
            } else {
                replies.remove(0)
            }
        })
    };
    let Some(p) = project(
        &server.endpoint,
        &Spec {
            native_fixture: true,
            plan: true,
            ..Spec::default()
        },
    ) else {
        panic!("the native fixture tool builds on a host that passes the host check");
    };
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        floor,
        Some(workdir.path()),
    );
    let runtime_dir = staged.runtime_dir.clone();
    let records_dir = staged.records_dir.clone();
    fs::write(workdir.path().join("task.md"), TASK).unwrap();

    // The native tool's own write probe on every runtime file, its own entrypoint included,
    // then its read probe.
    for (label, rel) in runtime_files() {
        calls.push((
            format!("toolu_write_{label}"),
            FIXTURE_TOOL.to_string(),
            probe("write", &runtime_dir.join(&rel)),
        ));
    }
    for (label, rel) in runtime_files() {
        if label == "entrypoint" {
            continue;
        }
        calls.push((
            format!("toolu_read_{label}"),
            FIXTURE_TOOL.to_string(),
            probe("read", &runtime_dir.join(&rel)),
        ));
    }
    calls.push(("toolu_skill".to_string(), SKILL.to_string(), json!({})));
    calls.push((
        "toolu_plan".to_string(),
        "submit-plan".to_string(),
        json!({"plan": self_rewriting_plan()}),
    ));
    let plan_file = runtime_dir.join("plans/plan-1.json");
    calls.push((
        "toolu_write_plan".to_string(),
        FIXTURE_TOOL.to_string(),
        probe("write", &plan_file),
    ));
    calls.push((
        "toolu_read_plan".to_string(),
        FIXTURE_TOOL.to_string(),
        probe("read", &plan_file),
    ));
    calls.push((
        "toolu_root".to_string(),
        "bash".to_string(),
        bash("echo \"ROOT-DIR|$MURMUR_RUNTIME_DIR\"; cat /etc/passwd | tail -n 1; ls -a"),
    ));
    *server_calls.lock().unwrap() = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, input))| tool_call(index + 1, id, name, input.clone()))
        .collect();

    assert!(runtime_dir.is_dir());
    launch_session(staged, |_| {}).expect("the session launches");

    // Every shell and plan attempt failed.
    let attack = result_text(&server, "toolu_attack");
    let truncate_mediated = floor == ContainmentClass::Sealed || landlock_abi() >= 3;
    let mut attempts = 0;
    for line in attack.lines().filter(|line| line.starts_with("RD|")) {
        let fields: Vec<&str> = line.splitn(4, '|').collect();
        let (label, status, error) = (fields[1], fields[2], fields[3]);
        attempts += 1;
        if label.ends_with(":truncate") && !truncate_mediated {
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
            write_refusals(floor, &runtime_dir)
        };
        assert!(
            expected.iter().any(|text| error.contains(text))
                || (label.ends_with(":rename-over")
                    && (error.contains("Permission denied")
                        || error.contains("Read-only file system"))),
            "{label} must fail with {expected:?}: {line}"
        );
    }
    assert_eq!(attempts, runtime_files().len() * 6 + 1, "{attack}");
    if truncate_mediated {
        let line = |prefix: &str| {
            attack
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("no {prefix} in {attack}"))
                .to_string()
        };
        assert_eq!(line("RD-TASK|"), TASK);
        assert_eq!(line("RD-SKILL|"), SKILL_BODY.lines().next().unwrap());
        assert!(line("RD-MURMUR|").starts_with("# MURMUR.md"));
        let entry_len = fs::metadata(common::fixture_native_tool_binary().unwrap())
            .unwrap()
            .len();
        assert_eq!(line("RD-ENTRY|").trim(), entry_len.to_string());
        assert_eq!(line("RD-DIR|"), runtime_dir.display().to_string());
    }

    // The native tool's writes fail, its own entrypoint included; its reads return the
    // originals.
    let native_refusals = write_refusals(floor, &runtime_dir);
    for label in runtime_files()
        .into_iter()
        .map(|(label, _)| label)
        .chain(["plan"])
    {
        let data = probe_data(&server, &format!("toolu_write_{label}"));
        assert_eq!(data["ok"], false, "{label}: {data}");
        let error = data["error"].as_str().unwrap_or_default();
        // A running binary opened for writing is refused with ETXTBSY before Landlock is asked,
        // so on `scoped` the fixture's write to its own entrypoint proves nothing about the rule;
        // the shell's attempts on the same file above are what show Landlock refusing it.
        let own_image = label == "entrypoint" && error.contains("Text file busy");
        assert!(
            native_refusals.iter().any(|text| error.contains(text)) || own_image,
            "{label} must fail with {native_refusals:?}: {data}"
        );
    }
    if truncate_mediated {
        let read = |label: &str| probe_data(&server, &format!("toolu_read_{label}"));
        assert_eq!(read("task")["contents"], TASK);
        assert_eq!(read("skill")["contents"], SKILL_BODY);
        assert!(read("murmur")["contents"]
            .as_str()
            .unwrap()
            .starts_with("# MURMUR.md"));
    }

    // The skill still answers with its original text, and the native tool answered as itself:
    // every probe above is its own JSON.
    assert!(result_text(&server, "toolu_skill").contains("The original guidance"));

    // The plan ran as submitted: the rewrite failed, the second step still ran and read the
    // original plan back.
    let plan = result_text(&server, "toolu_plan");
    assert!(plan.contains("rewrite-status=1"), "{plan}");
    assert!(plan.contains("readback-ran"), "{plan}");
    let plan_read = probe_data(&server, "toolu_read_plan");
    let stored: Value = serde_json::from_str(plan_read["contents"].as_str().unwrap_or_default())
        .unwrap_or_else(|_| panic!("the plan file is still the submitted plan: {plan_read}"));
    assert_eq!(stored["id"], "rd-plan", "{plan_read}");

    // Inside the root the runtime directory has the host's path, and the account database is the
    // synthetic one, written beside the runtime directory.
    let root = result_text(&server, "toolu_root");
    assert!(
        root.contains(&format!("ROOT-DIR|{}", runtime_dir.display())),
        "{root}"
    );
    if floor == ContainmentClass::Sealed {
        let home = records_dir.join("home");
        assert!(
            root.contains(&format!(":{}:", home.display())),
            "the synthetic passwd names the session's home folder: {root}"
        );
    }
    for name in [
        "tools",
        "MURMUR.md",
        "plans",
        "murmur.yaml",
        ".mur-etc",
        "created",
        ".murmur",
        ".capsule-home",
    ] {
        assert!(
            !workdir.path().join(name).exists(),
            "the workdir gained {name}"
        );
    }
    assert_eq!(
        fs::read_to_string(workdir.path().join("task.md")).unwrap(),
        TASK
    );
    let traces: Vec<PathBuf> =
        common::newest_session_records(&common::process_home(), workdir.path())
            .into_iter()
            .map(|records| records.join("trace.jsonl"))
            .filter(|trace| trace.is_file())
            .collect();
    assert!(!traces.is_empty(), "the session left its trace");
    for trace in traces {
        let trace = fs::read_to_string(trace).unwrap();
        assert!(
            !trace.contains("\"workdir_path_refused\""),
            "no runtime read or write was refused: {trace}"
        );
    }
    assert!(
        !runtime_dir.exists(),
        "the runtime directory outlived its session"
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_scoped_commands_cannot_change_the_runtime_files() {
    commands_cannot_change_the_runtime_files(
        "runtime_dir_scoped_commands_cannot_change_the_runtime_files",
        ContainmentClass::Scoped,
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_sealed_commands_cannot_change_the_runtime_files() {
    commands_cannot_change_the_runtime_files(
        "runtime_dir_sealed_commands_cannot_change_the_runtime_files",
        ContainmentClass::Sealed,
    );
}

// ── a pulled native tool cannot be rewritten and then run ────────────────────

fn pulled_tool_cannot_be_rewritten(test: &str, floor: ContainmentClass) {
    use murmur_artifact::{
        current_platform, ArtifactMeta, ArtifactName, LocalRegistry, Registry, RuntimeType,
    };

    if skip_host(test, floor) {
        return;
    }
    let binary = common::fixture_native_tool_binary()
        .expect("the native fixture tool builds on a host that passes the host check");
    let rewrite = format!(
        "E=\"$MURMUR_RUNTIME_DIR/tools/{PULLED_TOOL}/{PULLED_TOOL}\"\n\
         printf '#!/bin/sh\\necho pwned\\n' > ./evil\n\
         out=$( {{ cp /bin/sh \"$E\"; }} 2>&1 ); echo \"RD|$?|$out\"\n\
         out=$( {{ cp ./evil \"$E\"; }} 2>&1 ); echo \"RD|$?|$out\"\n\
         out=$( {{ printf '#!/bin/sh\\necho pwned\\n' > \"$E\"; }} 2>&1 ); echo \"RD|$?|$out\"\n\
         out=$( {{ mv -f ./evil \"$E\"; }} 2>&1 ); echo \"RD|$?|$out\"\n\
         [ -x \"$E\" ] && echo \"RD-PULLED|$E\"\n"
    );
    let server_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let server = {
        let replies = std::sync::Arc::clone(&server_calls);
        common::ScriptedServer::start_answering(16, move |_| {
            let mut replies = replies.lock().unwrap();
            if replies.is_empty() {
                end_turn("done")
            } else {
                replies.remove(0)
            }
        })
    };
    // The declared fixture is the native tool that attacks the pulled one.
    let p = project(
        &server.endpoint,
        &Spec {
            native_fixture: true,
            extra_capabilities: "  install:\n    tool: [\"local/tool-*\"]\n",
            ..Spec::default()
        },
    )
    .unwrap();
    let manifest_bytes = fs::read_to_string(common::fixture_native_tool_manifest())
        .unwrap()
        .replacen(
            &format!("name: {FIXTURE_TOOL}\n"),
            &format!("name: {PULLED_TOOL}\n"),
            1,
        );
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        zip.start_file("murmur.yaml", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(manifest_bytes.as_bytes()).unwrap();
        zip.start_file(
            format!("bin/{PULLED_TOOL}"),
            zip::write::SimpleFileOptions::default().unix_permissions(0o755),
        )
        .unwrap();
        zip.write_all(&fs::read(&binary).unwrap()).unwrap();
        zip.finish().unwrap();
    }
    let (os, arch) = current_platform().split_once('-').unwrap();
    LocalRegistry::new(p.home.path().join(".murmur").join("artifacts"))
        .publish(
            ArtifactMeta {
                name: ArtifactName::parse(PULLED_ARTIFACT).unwrap(),
                version: FIXTURE_VERSION.to_string(),
                runtime: RuntimeType::Native,
                artifact_runtime: "tool".to_string(),
                platforms: vec![(os.to_string(), arch.to_string())],
                description: None,
                tags: Vec::new(),
                wit_contracts: None,
                owner_id: None,
                built_at_level: None,
            },
            &cursor.into_inner(),
        )
        .unwrap();
    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("inside.txt"), "inside text").unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        floor,
        Some(workdir.path()),
    );
    let records_dir = staged.records_dir.clone();
    let staged_runtime_dir = staged.runtime_dir.clone();
    fs::write(workdir.path().join("task.md"), "Pull the fixture.").unwrap();
    let pulled_entrypoint = staged_runtime_dir.join(format!("tools/{PULLED_TOOL}/{PULLED_TOOL}"));
    *server_calls.lock().unwrap() = vec![
        tool_call(
            1,
            "toolu_pull",
            "pull-artifact",
            json!({"artifact": PULLED_ARTIFACT}),
        ),
        tool_call(2, "toolu_rewrite", "bash", bash(&rewrite)),
        tool_call(
            3,
            "toolu_native_write",
            FIXTURE_TOOL,
            probe("write", &pulled_entrypoint),
        ),
        tool_call(
            4,
            "toolu_after",
            PULLED_TOOL,
            json!({"operation": "read", "path": "inside.txt"}),
        ),
    ];

    launch_session(staged, |_| {}).expect("the session runs");

    let pulled = common::find_tool_result(&server.requests(), "toolu_pull")
        .map(|block| common::extract_result_text(&block))
        .expect("a pull result");
    println!("── toolu_pull\n{pulled}");
    assert!(
        pulled.contains(&format!("\"tool_name\":\"{PULLED_TOOL}\"")),
        "{pulled}"
    );
    let rewrite = result_text(&server, "toolu_rewrite");
    let lines: Vec<&str> = rewrite.lines().filter(|l| l.starts_with("RD|")).collect();
    assert_eq!(lines.len(), 4, "{rewrite}");
    let entrypoint = format!("tools/{PULLED_TOOL}/{PULLED_TOOL}");
    assert!(
        rewrite.contains(&format!(
            "RD-PULLED|{}",
            staged_runtime_dir.join(&entrypoint).display()
        )),
        "the pull lands in the runtime directory: {rewrite}"
    );
    for (index, line) in lines.iter().enumerate() {
        assert!(!line.starts_with("RD|0|"), "the rewrite must fail: {line}");
        // The first attempt may already fail reading `/bin/sh`; the others can only fail on the
        // entrypoint itself.
        if index > 0 {
            assert!(
                line.contains(&entrypoint)
                    && (line.contains("Permission denied")
                        || line.contains("Read-only file system")),
                "the rewrite must be refused on the entrypoint: {line}"
            );
        }
    }
    // A native tool cannot change another tool's pulled entrypoint either.
    let native = probe_data(&server, "toolu_native_write");
    let native_refusals = write_refusals(floor, &staged_runtime_dir);
    assert_eq!(native["ok"], false, "{native}");
    assert!(
        native["error"]
            .as_str()
            .is_some_and(|error| native_refusals.iter().any(|text| error.contains(text))),
        "the native write must fail with {native_refusals:?}: {native}"
    );
    let after = result_text(&server, "toolu_after");
    assert!(!after.contains("pwned"), "{after}");
    let probe: Value = serde_json::from_str(&after).unwrap_or_else(|_| panic!("{after}"));
    assert_eq!(probe["ok"], true, "the tool answers as itself: {probe}");
    assert!(probe.to_string().contains("inside text"));
    assert!(
        !records_dir.join("tools").exists(),
        "the records directory has no tools/"
    );
    assert!(!workdir.path().join("tools").exists());
    assert!(!workdir.path().join(".murmur").exists());
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_scoped_a_pulled_tool_cannot_be_rewritten_and_run() {
    pulled_tool_cannot_be_rewritten(
        "runtime_dir_scoped_a_pulled_tool_cannot_be_rewritten_and_run",
        ContainmentClass::Scoped,
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_sealed_a_pulled_tool_cannot_be_rewritten_and_run() {
    pulled_tool_cannot_be_rewritten(
        "runtime_dir_sealed_a_pulled_tool_cannot_be_rewritten_and_run",
        ContainmentClass::Sealed,
    );
}

// ── a WASM file tool reads the runtime directory at murmur-runtime/ ──────────

fn wasm_probe_run(
    scope: Option<&str>,
) -> (common::ScriptedServer, Vec<capsule_runtime::PreopenReport>) {
    let calls = [
        (
            "toolu_read",
            json!({"op": "read", "path": "murmur-runtime/MURMUR.md"}),
        ),
        (
            "toolu_write",
            json!({"op": "write", "path": "murmur-runtime/task.md"}),
        ),
        (
            "toolu_create",
            json!({"op": "write", "path": "murmur-runtime/created"}),
        ),
        (
            "toolu_reread",
            json!({"op": "read", "path": "murmur-runtime/task.md"}),
        ),
        (
            "toolu_list",
            json!({"op": "list", "path": "murmur-runtime/tools"}),
        ),
    ];
    let mut replies: Vec<String> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, input))| tool_call(index + 1, id, PROBE_TOOL, input.clone()))
        .collect();
    replies.push(end_turn("done"));
    let server = common::ScriptedServer::start(replies);
    let staging = TempDir::new().unwrap();
    let capabilities = match scope {
        Some(scope) => format!("    capabilities:\n      filesystem:\n        scope: {scope}\n"),
        None => String::new(),
    };
    let entry = format!(
        "  - name: {PROBE_ARTIFACT}\n    version: 0.1.0\n    runtime: tool\n{capabilities}"
    );
    let p = project(
        &server.endpoint,
        &Spec {
            extra_artifacts: &entry,
            ..Spec::default()
        },
    )
    .unwrap();
    let probe = common::create_tool_artifact(
        staging.path(),
        PROBE_ARTIFACT,
        "0.1.0",
        &common::fixture_path("runtime-dir-probe/tool/runtime-dir-probe.wasm"),
    );
    common::publish_local(&p.home, &probe).success();
    let workdir = TempDir::new().unwrap();
    fs::create_dir_all(workdir.path().join("work")).unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let preopens = staged.scope_report().preopens.clone();
    fs::write(workdir.path().join("task.md"), TASK).unwrap();
    launch_session(staged, |_| {}).expect("the session runs");
    (server, preopens)
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_wasm_tool_reads_murmur_runtime_and_cannot_write_it() {
    if common::skip_without_host_support(
        "runtime_dir_wasm_tool_reads_murmur_runtime_and_cannot_write_it",
    ) {
        return;
    }
    let (server, preopens) = wasm_probe_run(None);
    let report = preopens
        .iter()
        .find(|report| report.artifact == PROBE_TOOL || report.artifact == PROBE_ARTIFACT)
        .unwrap_or_else(|| panic!("{preopens:?}"));
    assert_eq!(
        report.runtime_dir_reach,
        Some(capsule_runtime::RuntimeDirReach::MurmurRuntime),
        "{report:?}"
    );
    assert!(report
        .render(true)
        .contains("runtime directory read-only at murmur-runtime/"));

    let read = result_text(&server, "toolu_read");
    assert!(read.contains("# MURMUR.md"), "{read}");
    for id in ["toolu_write", "toolu_create"] {
        let text = result_text(&server, id);
        assert!(text.contains("err:"), "{id}: {text}");
    }
    let reread = result_text(&server, "toolu_reread");
    assert!(
        reread.contains(TASK),
        "the refused write left the task as it was: {reread}"
    );
    assert!(!reread.contains("written by the probe"), "{reread}");
    let list = result_text(&server, "toolu_list");
    assert!(list.contains(SKILL), "{list}");
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_scoped_wasm_tool_has_no_murmur_runtime() {
    if common::skip_without_host_support("runtime_dir_scoped_wasm_tool_has_no_murmur_runtime") {
        return;
    }
    let (server, preopens) = wasm_probe_run(Some("work"));
    let report = preopens
        .iter()
        .find(|report| report.artifact == PROBE_TOOL || report.artifact == PROBE_ARTIFACT)
        .unwrap_or_else(|| panic!("{preopens:?}"));
    assert_eq!(report.runtime_dir_reach, None, "{report:?}");
    assert!(!report.render(true).contains("murmur-runtime"));
    for id in ["toolu_read", "toolu_write", "toolu_list"] {
        let text = result_text(&server, id);
        assert!(text.contains("err:"), "{id}: {text}");
        assert!(!text.contains("# MURMUR.md"), "{id}: {text}");
    }
}

// ── what the agent is told ───────────────────────────────────────────────────

/// The `[Capsule]` block of `request`: everything before the first blank line of its system
/// prompt.
fn block_of(request: &Value) -> String {
    let system = common::system_text(&request["system"]).expect("a system prompt");
    system.split("\n\n").next().expect("a block").to_string()
}

/// One manifest's block is byte-identical whichever way the task arrives — `mur run --task`,
/// a `task.md` in the workdir, an A2A message — and in each the task is the first user message.
/// The block names no `task.md`, no checkpoint and, with the plan tool granted, no plan tool.
#[test]
#[allow(clippy::print_stdout)]
fn capsule_block_is_the_same_whichever_way_the_task_arrives() {
    if common::skip_without_host_support("capsule_block_is_the_same_whichever_way_the_task_arrives")
    {
        return;
    }
    // One server and one project for all three, so the manifest — and the endpoint its network
    // allowlist names — is the same.
    let server =
        common::ScriptedServer::start(vec![end_turn("one"), end_turn("two"), end_turn("three")]);
    let p = project(
        &server.endpoint,
        &Spec {
            plan: true,
            ..Spec::default()
        },
    )
    .unwrap();

    let cli_workdir = TempDir::new().unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            cli_workdir.path().to_str().unwrap(),
            "--task",
            "CLI TASK",
        ])
        .assert()
        .success();

    let file_workdir = TempDir::new().unwrap();
    fs::write(file_workdir.path().join("task.md"), "FILE TASK").unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(file_workdir.path()),
    );
    launch_session(staged, |_| {}).expect("the session runs");

    let a2a_workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(a2a_workdir.path()),
    );
    let _ = send_one_message(staged, "A2A TASK").join();

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one request per launch");
    let blocks: Vec<String> = requests.iter().map(block_of).collect();
    println!("── [Capsule] block\n{}", blocks[0]);
    for (request, task) in requests.iter().zip(["CLI TASK", "FILE TASK", "A2A TASK"]) {
        assert_eq!(
            request["messages"].as_array().map(Vec::len),
            Some(1),
            "{request}"
        );
        assert!(
            common::last_user_text(request).contains(task),
            "the task is the first user message: {request}"
        );
    }
    assert_eq!(blocks[0], blocks[1], "mur run --task and task.md");
    assert_eq!(blocks[0], blocks[2], "mur run --task and A2A");
    let block = &blocks[0];
    assert!(block.starts_with("[Capsule]\nName: rd-capsule\nVersion: 0.1.0\nWork folder: ./"));
    assert!(block.contains(NOTICE), "{block}");
    for absent in ["task.md", "Checkpoint", "submit-plan"] {
        assert!(!block.contains(absent), "{absent:?} in:\n{block}");
    }
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_murmur_md_and_the_capsule_block_name_the_runtime_dir() {
    if common::skip_without_host_support(
        "runtime_dir_murmur_md_and_the_capsule_block_name_the_runtime_dir",
    ) {
        return;
    }
    std::env::set_var(capsule_runtime::RUNTIME_DIR_ENV, "/host/says/otherwise");
    // One server and one project for both rounds: the block names the network allowlist, which
    // names the server's endpoint.
    let env_call = |round: usize| {
        tool_call(
            round,
            &format!("toolu_env{round}"),
            "bash",
            bash("echo \"ENV|$MURMUR_RUNTIME_DIR\""),
        )
    };
    let server = common::ScriptedServer::start(vec![
        env_call(0),
        end_turn("done"),
        env_call(1),
        end_turn("done"),
    ]);
    let p = project(
        &server.endpoint,
        &Spec {
            env_allow: &[capsule_runtime::RUNTIME_DIR_ENV],
            ..Spec::default()
        },
    )
    .unwrap();
    let mut blocks = Vec::new();
    for round in 0..2 {
        let workdir = TempDir::new().unwrap();
        let staged = common::stage_agent_session_at_floor(
            &p.home,
            p.project.path(),
            &p.manifest,
            ContainmentClass::Advisory,
            Some(workdir.path()),
        );
        let runtime_dir = staged.runtime_dir.clone();
        let murmur_md = fs::read_to_string(runtime_dir.join("MURMUR.md")).unwrap();
        assert!(!staged.records_dir.join("MURMUR.md").exists());
        assert!(!workdir.path().join("MURMUR.md").exists());
        assert!(
            murmur_md.contains(&runtime_dir.display().to_string()),
            "{murmur_md}"
        );
        assert!(
            murmur_md.contains(&workdir.path().display().to_string()),
            "{murmur_md}"
        );
        assert!(murmur_md.contains(
            "\n  - a command line a shell interpreter runs with `-c`: `$MURMUR_RUNTIME_DIR`\n"
        ));
        // No tool of this capsule is preopened there.
        assert!(!murmur_md.contains("murmur-runtime/"), "{murmur_md}");
        fs::write(workdir.path().join("task.md"), TASK).unwrap();

        launch_session(staged, |_| {}).expect("the session runs");

        let env = result_text(&server, &format!("toolu_env{round}"));
        assert!(
            env.contains(&format!("ENV|{}", runtime_dir.display())),
            "round {round}: {env}"
        );
        let block = block_of(&server.requests()[round * 2]);
        assert!(block.contains(NOTICE), "{block}");
        assert!(
            !block.contains("Manifest: murmur.yaml (in your workdir)"),
            "{block}"
        );
        assert!(
            !block.contains(&runtime_dir.display().to_string()),
            "{block}"
        );
        assert!(!block.contains(".murmur"), "{block}");
        blocks.push(block);
    }
    assert_eq!(
        blocks[0], blocks[1],
        "the block is byte-identical across launches"
    );
}

/// The text of MURMUR.md's `## {heading}` section, up to the next heading.
fn md_section<'a>(doc: &'a str, heading: &str) -> &'a str {
    let start = doc
        .find(&format!("\n## {heading}\n"))
        .unwrap_or_else(|| panic!("no ## {heading}: {doc}"));
    let rest = &doc[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |end| end + 3);
    &rest[..end]
}

/// The `- **<name>**` entries of a MURMUR.md section, each with its line.
fn md_entries(section: &str) -> Vec<(String, &str)> {
    section
        .lines()
        .filter_map(|line| {
            let name = line.strip_prefix("- **")?.split("**").next()?;
            Some((name.to_string(), line))
        })
        .collect()
}

/// Every tool MURMUR.md names is in the session's tool list, the skill bound as system prompt
/// is named as bound and is not, and every path the document names exists.
#[test]
#[allow(clippy::print_stdout)]
fn murmur_md_names_resolve() {
    if common::skip_without_host_support("murmur_md_names_resolve") {
        return;
    }
    const BOUND: &str = "rd-bound";
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let entries = format!(
        "  - name: {PROBE_ARTIFACT}\n    version: 0.1.0\n    runtime: tool\n  - name: {BOUND}\n    \
         source: ./skills/{BOUND}/skill.md\n    runtime: skill\n"
    );
    let p = project(
        &server.endpoint,
        &Spec {
            plan: true,
            extra_artifacts: &entries,
            extra_capabilities: "  install:\n    tool: [\"murmur/tool-*\"]\n",
            ..Spec::default()
        },
    )
    .unwrap();
    let bound_dir = p.project.path().join("skills").join(BOUND);
    fs::create_dir_all(&bound_dir).unwrap();
    fs::write(bound_dir.join("skill.md"), "You are the bound skill.\n").unwrap();
    let manifest = fs::read_to_string(&p.manifest).unwrap().replace(
        "inference:\n  transport: http\n",
        &format!("inference:\n  transport: http\n  system_prompt_artifact: {BOUND}\n"),
    );
    fs::write(&p.manifest, manifest).unwrap();
    let staging = TempDir::new().unwrap();
    let probe = common::create_tool_artifact(
        staging.path(),
        PROBE_ARTIFACT,
        "0.1.0",
        &common::fixture_path("runtime-dir-probe/tool/runtime-dir-probe.wasm"),
    );
    common::publish_local(&p.home, &probe).success();
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let runtime_dir = staged.runtime_dir.clone();
    let doc = fs::read_to_string(runtime_dir.join("MURMUR.md")).unwrap();
    println!("── MURMUR.md\n{doc}");

    let tools = md_entries(md_section(&doc, "Installed Tools"));
    let skills = md_entries(md_section(&doc, "Installed Skills"));
    let tree: Vec<String> = md_section(&doc, "Runtime Directory")
        .lines()
        .filter_map(|line| {
            line.strip_prefix("├── ")
                .or_else(|| line.strip_prefix("└── "))
        })
        .map(|entry| entry.trim_end_matches('/').to_string())
        .collect();
    // Every absolute path, wherever it stands: a token opening with `/`, without the Markdown
    // and punctuation that close it.
    let paths: Vec<PathBuf> = doc
        .split_whitespace()
        .filter(|token| token.starts_with('/'))
        .map(|token| PathBuf::from(token.trim_end_matches([')', '*', ';', ','])))
        .collect();
    // Checked before the launch, which removes the runtime directory when it ends.
    assert!(paths.len() >= 4, "{paths:?}");
    for path in &paths {
        assert!(path.exists(), "{} does not exist", path.display());
    }
    assert_eq!(tree, ["MURMUR.md", "murmur.yaml", "tools"], "{doc}");
    for entry in &tree {
        assert!(runtime_dir.join(entry).exists(), "{entry}");
    }

    fs::write(workdir.path().join("task.md"), TASK).unwrap();
    launch_session(staged, |_| {}).expect("the session runs");
    let offered: Vec<String> = server.requests()[0]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    println!("── tools\n{offered:?}");

    let names: Vec<&str> = tools.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "bash",
        PROBE_TOOL,
        SKILL,
        "submit-plan",
        "pull-artifact",
        "remove-artifact",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} not listed: {names:?}"
        );
    }
    for name in &names {
        assert!(
            offered.iter().any(|tool| tool == name),
            "{name} not offered: {offered:?}"
        );
    }
    for (name, line) in &skills {
        if name == BOUND {
            assert!(line.contains("bound as system prompt"), "{line}");
            assert!(!offered.iter().any(|tool| tool == name), "{offered:?}");
            assert!(!names.contains(&BOUND), "{names:?}");
        } else {
            assert!(line.contains("call by name"), "{line}");
            assert!(
                offered.iter().any(|tool| tool == name),
                "{name}: {offered:?}"
            );
        }
    }
    assert!(skills.iter().any(|(name, _)| name == BOUND), "{skills:?}");
    assert!(skills.iter().any(|(name, _)| name == SKILL), "{skills:?}");
}

/// The tool `tool-pulled-probe`, the WASM probe under a second name, which an install grant
/// lets a session pull.
const PULLED_PROBE_ARTIFACT: &str = "local/tool-pulled-probe";
const PULLED_PROBE_TOOL: &str = "tool-pulled-probe";

/// The `- Runtime directory:` entry of MURMUR.md, up to the next bullet of `## Capsule`.
fn runtime_dir_entry(doc: &str) -> &str {
    let start = doc
        .find("- Runtime directory: ")
        .unwrap_or_else(|| panic!("no runtime-directory entry: {doc}"));
    let end = doc[start..]
        .find("\n- Writable:")
        .unwrap_or_else(|| panic!("no Writable entry: {doc}"));
    &doc[start..start + end]
}

/// The tools the `murmur-runtime/` sub-bullet of `entry` names, or none.
fn murmur_runtime_tools(entry: &str) -> Vec<String> {
    entry
        .lines()
        .filter_map(|line| {
            line.strip_prefix("  - ")?
                .strip_suffix(": `murmur-runtime/`")
        })
        .flat_map(|names| names.split(", "))
        .map(str::to_string)
        .collect()
}

/// MURMUR.md gives the runtime directory's absolute path and the name each kind of tool opens it
/// by: `$MURMUR_RUNTIME_DIR` for a command line a shell interpreter runs with `-c`,
/// `murmur-runtime/` for exactly the tools preopened there, the absolute path for a program run
/// directly and any other tool. A tool pulled mid-session joins the `murmur-runtime/` list once
/// the pull commits.
#[test]
#[allow(clippy::print_stdout)]
fn murmur_md_runtime_directory_names() {
    if common::skip_without_host_support("murmur_md_runtime_directory_names")
        || capsule_runtime::skip_without_egress_namespace("murmur_md_runtime_directory_names")
    {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_env",
            "bash",
            bash("echo \"ENV|$MURMUR_RUNTIME_DIR\""),
        ),
        tool_call(
            2,
            "toolu_pull",
            "pull-artifact",
            json!({"artifact": PULLED_PROBE_ARTIFACT}),
        ),
        tool_call(
            3,
            "toolu_cat",
            "bash",
            bash("cat \"$MURMUR_RUNTIME_DIR/MURMUR.md\""),
        ),
        end_turn("done"),
    ]);
    let entries = format!("  - name: {PROBE_ARTIFACT}\n    version: 0.1.0\n    runtime: tool\n");
    let p = project(
        &server.endpoint,
        &Spec {
            extra_artifacts: &entries,
            extra_capabilities: "  install:\n    tool: [\"local/tool-*\"]\n",
            ..Spec::default()
        },
    )
    .unwrap();
    let staging = TempDir::new().unwrap();
    for name in [PROBE_ARTIFACT, PULLED_PROBE_ARTIFACT] {
        let probe = common::create_tool_artifact(
            staging.path(),
            name,
            "0.1.0",
            &common::fixture_path("runtime-dir-probe/tool/runtime-dir-probe.wasm"),
        );
        common::publish_local(&p.home, &probe).success();
    }
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let runtime_dir = staged.runtime_dir.clone();
    let preopens = staged.scope_report().preopens.clone();
    let doc = fs::read_to_string(runtime_dir.join("MURMUR.md")).unwrap();
    let entry = runtime_dir_entry(&doc).to_string();
    println!("── staged entry\n{entry}");

    assert!(!doc.contains("task.md"), "{doc}");
    let absolute = runtime_dir.display().to_string();
    assert!(
        entry.starts_with(&format!(
            "- Runtime directory: {absolute} (read-only). The name to use depends on what opens \
             it:\n"
        )),
        "{entry}"
    );
    // Checked before the launch, which removes the runtime directory when it ends.
    assert!(runtime_dir.is_dir());
    assert!(entry.contains(
        "\n  - a command line a shell interpreter runs with `-c`: `$MURMUR_RUNTIME_DIR`\n"
    ));
    assert!(entry.ends_with(&format!(
        "\n  - a program run directly, or any other tool given a path: `{absolute}`"
    )));
    assert_eq!(murmur_runtime_tools(&entry), [PROBE_TOOL], "{entry}");
    // Every tool named there is preopened there.
    for tool in murmur_runtime_tools(&entry) {
        let report = preopens
            .iter()
            .find(|report| {
                report.artifact == tool || report.artifact.ends_with(&format!("/{tool}"))
            })
            .unwrap_or_else(|| panic!("{tool}: {preopens:?}"));
        assert_eq!(
            report.runtime_dir_reach,
            Some(capsule_runtime::RuntimeDirReach::MurmurRuntime),
            "{report:?}"
        );
    }

    fs::write(workdir.path().join("task.md"), TASK).unwrap();
    launch_session(staged, |_| {}).expect("the session runs");

    let env = result_text(&server, "toolu_env");
    assert!(env.contains(&format!("ENV|{absolute}\n")), "{env}");
    let offered: Vec<String> = server.requests()[0]["tools"]
        .as_array()
        .expect("a tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    assert!(offered.iter().any(|tool| tool == PROBE_TOOL), "{offered:?}");

    let pulled = common::unfence(&result_text(&server, "toolu_cat"));
    let pulled_entry = runtime_dir_entry(&pulled);
    println!("── entry after the pull\n{pulled_entry}");
    assert_eq!(
        murmur_runtime_tools(pulled_entry),
        [PROBE_TOOL, PULLED_PROBE_TOOL],
        "{pulled_entry}"
    );
    assert!(!pulled.contains("task.md"), "{pulled}");
}

// ── the work-folder ceiling ──────────────────────────────────────────────────

/// A 1 MiB `workdir_max_bytes`, with `sleep` allowed and a grace period that keeps a 12 s command
/// in the foreground.
fn ceiling_project(endpoint: &str) -> Project {
    let p = project(
        endpoint,
        &Spec {
            extra_capabilities: "  resources:\n    workdir_max_bytes: 1048576\n",
            extra_shell: &["sleep"],
            ..Spec::default()
        },
    )
    .unwrap();
    let mut manifest = fs::read_to_string(&p.manifest).unwrap();
    manifest.push_str("lifecycle:\n  shell_grace_secs: 30\n");
    fs::write(&p.manifest, manifest).unwrap();
    p
}

fn cat_murmur_md(index: usize) -> String {
    tool_call(
        index,
        "toolu_md",
        "bash",
        bash("cat \"$MURMUR_RUNTIME_DIR/MURMUR.md\""),
    )
}

/// MURMUR.md's line for a 1 MiB `workdir_max_bytes`.
const CEILING_LINE: &str = "\n- 1 MiB across the work folder, $HOME and $TMPDIR\n";

/// Under `--workdir` the guard measures the operator's work folder with the session's `home/` and
/// `tmp/`, so MURMUR.md states the ceiling there too, and the `[Capsule]` block states none.
#[test]
#[allow(clippy::print_stdout)]
fn work_folder_ceiling_is_stated_under_workdir() {
    if common::skip_without_host_support("work_folder_ceiling_is_stated_under_workdir") {
        return;
    }
    let server = common::ScriptedServer::start(vec![cat_murmur_md(1), end_turn("done")]);
    let p = ceiling_project(&server.endpoint);
    let workdir = TempDir::new().unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--task",
            TASK,
        ])
        .assert()
        .success();
    let block = block_of(&server.requests()[0]);
    println!("── [Capsule] block\n{block}");
    let murmur_md = result_text(&server, "toolu_md");
    assert!(
        md_section(&murmur_md, "Limits").contains(CEILING_LINE),
        "{murmur_md}"
    );
    assert!(!block.contains("across the work folder"), "{block}");
    assert_eq!(
        fs::read_dir(workdir.path()).unwrap().count(),
        0,
        "the work folder gains nothing"
    );
}

/// Without `--workdir` the guard measures the work folder, MURMUR.md states its ceiling, and a
/// write past it ends the session. The `[Capsule]` block, the same for every launch, states none.
#[test]
#[allow(clippy::print_stdout)]
fn work_folder_ceiling_stated_without_workdir_is_enforced() {
    if common::skip_without_host_support("work_folder_ceiling_stated_without_workdir_is_enforced") {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        cat_murmur_md(1),
        tool_call(
            2,
            "toolu_write",
            "bash",
            bash("head -c 5000000 /dev/zero > big.bin"),
        ),
        // Longer than the guard's 10 s check interval.
        tool_call(3, "toolu_sleep", "bash", bash("sleep 12")),
        end_turn("never sent: the breach ends the session first"),
    ]);
    let p = ceiling_project(&server.endpoint);
    let output = Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--task",
            TASK,
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("── stderr\n{stderr}");
    let block = block_of(&server.requests()[0]);
    assert!(!block.contains("across the work folder"), "{block}");
    let murmur_md = result_text(&server, "toolu_md");
    assert!(
        md_section(&murmur_md, "Limits").contains(CEILING_LINE),
        "{murmur_md}"
    );
    assert!(!output.status.success(), "the breach fails the run");
    assert!(
        stderr.contains("past the 1048576 byte ceiling (capabilities.resources.workdir_max_bytes)"),
        "{stderr}"
    );
    assert_eq!(
        server.requests().len(),
        3,
        "no turn follows the command the breach was seen during"
    );
}

/// MURMUR.md names the programs a command can start, in the manifest's order, and what happens
/// to any other: `date`, which `SHELL_ALLOW` leaves out, is refused and its shell exits 126.
#[test]
#[allow(clippy::print_stdout)]
fn murmur_md_states_the_allowlist_and_an_unlisted_program_exits_126() {
    const TEST: &str = "murmur_md_states_the_allowlist_and_an_unlisted_program_exits_126";
    if common::skip_without_host_support(TEST) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        cat_murmur_md(1),
        tool_call(2, "toolu_date", "bash", bash("date")),
        end_turn("done"),
    ]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--task",
            TASK,
        ])
        .assert()
        .success();
    let murmur_md = result_text(&server, "toolu_md");
    let section = md_section(&murmur_md, "Running Shell Commands");
    let programs = format!("Programs a command can start: {}.", SHELL_ALLOW.join(", "));
    assert!(section.contains(&programs), "{section}");
    assert!(section.contains("exit status 126"), "{section}");
    let date = result_text(&server, "toolu_date");
    assert!(date.contains("Exit code: 126"), "{date}");
    assert!(date.contains("Permission denied"), "{date}");
    println!("{TEST}: ran; MURMUR.md states the allowlist and `date` exited 126");
}

// ── the task's way in ────────────────────────────────────────────────────────

/// One session whose single call cats the runtime directory's task, with `prepare` run on the
/// workdir before launch.
fn task_run(prepare: impl FnOnce(&Path)) -> (common::ScriptedServer, TempDir) {
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_task",
            "bash",
            bash("echo \"TASK|$(cat \"$MURMUR_RUNTIME_DIR/task.md\")\""),
        ),
        end_turn("done"),
    ]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let workdir = TempDir::new().unwrap();
    prepare(workdir.path());
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    launch_session(staged, |_| {}).expect("the session runs");
    (server, workdir)
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_task_md_in_the_workdir_is_imported_and_left_unchanged() {
    if common::skip_without_host_support(
        "runtime_dir_task_md_in_the_workdir_is_imported_and_left_unchanged",
    ) {
        return;
    }
    let (server, workdir) = task_run(|workdir| {
        fs::write(workdir.join("task.md"), "OPERATOR TASK").unwrap();
        fs::write(workdir.join("input.txt"), "NOT THIS ONE").unwrap();
    });
    assert!(common::last_user_text(&server.requests()[0]).contains("OPERATOR TASK"));
    assert!(result_text(&server, "toolu_task").contains("TASK|OPERATOR TASK"));
    assert_eq!(
        fs::read_to_string(workdir.path().join("task.md")).unwrap(),
        "OPERATOR TASK"
    );
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_an_input_txt_only_workdir_runs_its_task() {
    if common::skip_without_host_support("runtime_dir_an_input_txt_only_workdir_runs_its_task") {
        return;
    }
    let (server, workdir) = task_run(|workdir| {
        fs::write(workdir.join("input.txt"), "INPUT TASK").unwrap();
    });
    assert!(common::last_user_text(&server.requests()[0]).contains("INPUT TASK"));
    assert!(result_text(&server, "toolu_task").contains("TASK|INPUT TASK"));
    assert!(!workdir.path().join("task.md").exists());
    assert_eq!(
        fs::read_to_string(workdir.path().join("input.txt")).unwrap(),
        "INPUT TASK"
    );
}

/// `mur run --task` writes the task into the runtime directory, and `--workdir` gets neither a
/// `task.md` nor a manifest copy.
#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_mur_run_task_writes_nothing_into_the_workdir() {
    if common::skip_without_host_support("runtime_dir_mur_run_task_writes_nothing_into_the_workdir")
    {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_task",
            "bash",
            bash(
                "echo \"TASK|$(cat \"$MURMUR_RUNTIME_DIR/task.md\")\"; ls \"$MURMUR_RUNTIME_DIR\"",
            ),
        ),
        end_turn("done"),
    ]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let workdir = TempDir::new().unwrap();
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            workdir.path().to_str().unwrap(),
            "--task",
            "CLI TASK",
        ])
        .assert()
        .success();
    let text = result_text(&server, "toolu_task");
    assert!(text.contains("TASK|CLI TASK"), "{text}");
    assert!(
        text.contains("murmur.yaml"),
        "the manifest is in the runtime directory: {text}"
    );
    assert!(!workdir.path().join("task.md").exists());
    assert!(!workdir.path().join("murmur.yaml").exists());
    let sessions = p.home.path().join(".murmur").join("sessions");
    assert_eq!(
        fs::read_dir(&sessions)
            .map(|entries| entries.count())
            .unwrap_or(0),
        0,
        "the runtime directory is removed when mur run ends"
    );
}

/// A script capsule reads the `--task` text from `murmur-runtime/task.md`, a directory it cannot
/// write, and finds no `task.md` in its workdir.
#[test]
fn runtime_dir_a_script_capsule_reads_its_task_from_murmur_runtime() {
    let home = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: script-capsule\nversion: 0.0.1\n",
    )
    .unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/run/components/capsule-sleep-input.wasm"),
        project.path().join("capsule.wasm"),
    )
    .unwrap();
    let workdir = TempDir::new().unwrap();
    mur(home.path())
        .args(["run", "--manifest"])
        .arg(project.path().join("murmur.yaml"))
        .arg("--workdir")
        .arg(workdir.path())
        .args(["--task", "SCRIPT TASK"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(workdir.path().join("out/result.txt")).unwrap(),
        "SCRIPT TASK"
    );
    assert!(!workdir.path().join("task.md").exists());
}

/// A reopened task reads the runtime directory's `task.md`, rewritten as the original plus the
/// hook's feedback; the first attempt's rewrite of the workdir's `task.md` is its own file, and
/// its rewrite of the runtime directory's is refused.
#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_scoped_a_reopen_rewrites_only_the_runtime_task() {
    let test = "runtime_dir_scoped_a_reopen_rewrites_only_the_runtime_task";
    if skip_host(test, ContainmentClass::Scoped) {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_first",
            "bash",
            bash(
                "printf CHANGED > ./task.md; echo workdir=$?; \
                 out=$(printf CHANGED 2>&1 > \"$MURMUR_RUNTIME_DIR/task.md\"); echo runtime=$?",
            ),
        ),
        end_turn("first"),
        tool_call(
            3,
            "toolu_second",
            "bash",
            bash("echo \"TASK|$(cat \"$MURMUR_RUNTIME_DIR/task.md\")|END\""),
        ),
        end_turn("second"),
    ]);
    let staging = TempDir::new().unwrap();
    let hook_entry = "  - name: local/reopen-once\n    version: 0.1.0\n    runtime: hook\n";
    let p = project(
        &server.endpoint,
        &Spec {
            extra_artifacts: hook_entry,
            ..Spec::default()
        },
    )
    .unwrap();
    let hook = common::hook_wat::create_hook_zip(
        staging.path(),
        "reopen-once",
        "on-task-end",
        "reopen-task",
        &common::hook_wat::reopen_task_once_hook_wasm("check again"),
    );
    common::publish_local(&p.home, &hook).success();
    let workdir = TempDir::new().unwrap();
    fs::write(workdir.path().join("task.md"), "ORIGINAL").unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Scoped,
        Some(workdir.path()),
    );
    launch_session(staged, |_| {}).expect("the session runs");

    let first = result_text(&server, "toolu_first");
    assert!(first.contains("workdir=0"), "{first}");
    assert!(first.contains("runtime=1"), "{first}");
    let second = result_text(&server, "toolu_second");
    let task = second
        .split("\nTASK|")
        .nth(1)
        .and_then(|rest| rest.split("|END").next())
        .unwrap_or_else(|| panic!("{second}"));
    assert!(task.starts_with("ORIGINAL"), "{task}");
    assert!(task.contains("check again"), "{task}");
    assert!(!task.contains("CHANGED"), "{task}");
    assert_eq!(
        fs::read_to_string(workdir.path().join("task.md")).unwrap(),
        "CHANGED",
        "the workdir's task.md is the capsule's own file"
    );
}

/// Starts `staged` on a thread and sends it one A2A message once its door is up.
fn send_one_message(
    staged: capsule_runtime::StagedSession,
    text: &str,
) -> std::thread::JoinHandle<Result<capsule_runtime::LaunchResult, capsule_runtime::RuntimeError>> {
    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
    });
    let url = url_rx
        .recv_timeout(common::CAPSULE_URL_WAIT)
        .expect("the capsule's door comes up");
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "SendMessage",
        "params": {
            "configuration": {"returnImmediately": true},
            "message": {
                "messageId": "msg-runtime-dir",
                "role": "ROLE_USER",
                "parts": [{"text": text, "mediaType": "text/plain"}]
            }
        }
    })
    .to_string();
    let _ = common::idle_capsule::http_post_json(&url, &body);
    handle
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_an_a2a_task_is_written_to_the_runtime_dir() {
    if common::skip_without_host_support("runtime_dir_an_a2a_task_is_written_to_the_runtime_dir") {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        tool_call(
            1,
            "toolu_task",
            "bash",
            bash("echo \"TASK|$(cat \"$MURMUR_RUNTIME_DIR/task.md\")\""),
        ),
        end_turn("done"),
    ]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let workdir = TempDir::new().unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let _ = send_one_message(staged, "A2A TASK").join();
    assert!(result_text(&server, "toolu_task").contains("TASK|A2A TASK"));
    assert!(!workdir.path().join("task.md").exists());
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_a_task_md_link_in_the_workdir_fails_the_first_task() {
    if common::skip_without_host_support(
        "runtime_dir_a_task_md_link_in_the_workdir_fails_the_first_task",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let workdir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let sentinel = outside.path().join("sentinel.txt");
    fs::write(&sentinel, "operator data").unwrap();
    std::os::unix::fs::symlink(&sentinel, workdir.path().join("task.md")).unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        Some(workdir.path()),
    );
    let session = staged.records_dir.clone();
    let _ = send_one_message(staged, "anything").join();

    assert!(server.requests().is_empty(), "the provider was asked");
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "operator data");
    let events = common::read_whole_trace(&session.join("trace.jsonl"));
    let failed = events
        .iter()
        .find(|event| event["event_type"] == "task_failed")
        .unwrap_or_else(|| panic!("no task_failed: {events:#?}"));
    assert_eq!(failed["cause"], "workdir_path_refused", "{failed}");
    let refused = events
        .iter()
        .find(|event| event["event_type"] == "workdir_path_refused")
        .unwrap_or_else(|| panic!("no refusal: {events:#?}"));
    assert_eq!(refused["path"], "task.md", "{refused}");
    assert_eq!(refused["operation"], "read", "{refused}");
}

// ── lifecycle ────────────────────────────────────────────────────────────────

fn lock_is_free(lock: &Path) -> bool {
    fs::File::open(lock).unwrap().try_lock().is_ok()
}

#[test]
fn runtime_dir_is_private_locked_while_held_and_removed_when_the_session_ends() {
    if common::skip_without_host_support(
        "runtime_dir_is_private_locked_while_held_and_removed_when_the_session_ends",
    ) {
        return;
    }
    // Success.
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        None,
    );
    let runtime_dir = staged.runtime_dir.clone();
    let session_dir = runtime_dir.parent().unwrap().to_path_buf();
    for dir in [
        &runtime_dir,
        &session_dir,
        &session_dir.parent().unwrap().to_path_buf(),
    ] {
        let mode = fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    for sub in ["sealed-etc", "harness"] {
        assert!(session_dir.join(sub).is_dir());
    }
    assert!(
        !lock_is_free(&session_dir.join(".lock")),
        "the live session holds its lock"
    );
    fs::write(staged.accessible_workdir.join("task.md"), TASK).unwrap();
    launch_session(staged, |_| {}).expect("the session runs");
    assert!(!session_dir.exists(), "removed after a successful run");

    // A failed task: the provider answers nothing usable.
    let server = common::ScriptedServer::start(vec!["not json".to_string()]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        None,
    );
    let session_dir = staged.runtime_dir.parent().unwrap().to_path_buf();
    fs::write(staged.accessible_workdir.join("task.md"), TASK).unwrap();
    let _ = launch_session(staged, |_| {});
    assert!(!session_dir.exists(), "removed after a failed task");

    // Staged and never launched.
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let staged = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        None,
    );
    let session_dir = staged.runtime_dir.parent().unwrap().to_path_buf();
    assert!(session_dir.is_dir());
    drop(staged);
    assert!(
        !session_dir.exists(),
        "removed with a session that was never launched"
    );
}

#[test]
fn runtime_dir_staging_sweeps_unlocked_stale_directories_only() {
    if common::skip_without_host_support(
        "runtime_dir_staging_sweeps_unlocked_stale_directories_only",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    // Staged once to learn the home this process stages under.
    let first = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        None,
    );
    let sessions = first
        .runtime_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    drop(first);

    let make = |name: &str, age_secs: u64| {
        let dir = sessions.join(name);
        fs::create_dir_all(dir.join("runtime")).unwrap();
        let lock = dir.join(".lock");
        fs::write(&lock, b"").unwrap();
        fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs))
            .unwrap();
        dir
    };
    let stale = make("ses_rd_stale", 61);
    let locked = make("ses_rd_locked", 61);
    let recent = make("ses_rd_recent", 10);
    // Held until `held` drops at the end of the test.
    let held = fs::File::open(locked.join(".lock")).unwrap();
    held.try_lock().unwrap();

    let second = common::stage_agent_session_at_floor(
        &p.home,
        p.project.path(),
        &p.manifest,
        ContainmentClass::Advisory,
        None,
    );

    assert!(
        !stale.exists(),
        "an unlocked 61-second-old directory is swept"
    );
    assert!(locked.exists(), "a locked one is kept");
    assert!(recent.exists(), "a 10-second-old one is kept");
    drop(second);
    drop(held);
    let _ = fs::remove_dir_all(&locked);
    let _ = fs::remove_dir_all(&recent);
}

// ── a runtime directory inside the workdir ───────────────────────────────────

/// A project published under `home`, whose `HOME` is also the `--workdir`.
fn home_is_workdir(endpoint: &str) -> Project {
    project(endpoint, &Spec::default()).unwrap()
}

fn mur(home: &Path) -> Command {
    let mut command = Command::cargo_bin("mur").unwrap();
    command.env("HOME", home);
    command
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_inside_the_workdir_refuses_a_kernel_floor() {
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let p = home_is_workdir(&server.endpoint);
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
        assert!(stderr.contains("E-CAP-003"), "{stderr}");
        assert!(
            stderr.contains(&home.join(".murmur").join("sessions").display().to_string()),
            "{stderr}"
        );
        assert!(
            stderr.contains("run with a --workdir that does not contain ~/.murmur"),
            "{stderr}"
        );
        assert!(!home.join(".murmur").join("sessions").exists());
        assert!(common::sessions_under(&home.join(".murmur")).is_empty());
    }
    assert!(server.requests().is_empty());
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_inside_the_workdir_below_the_floor_warns_and_reports_advisory() {
    if common::skip_without_host_support(
        "runtime_dir_inside_the_workdir_below_the_floor_warns_and_reports_advisory",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("done")]);
    let p = home_is_workdir(&server.endpoint);
    let home = p.home.path();
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
    let session = common::sessions_under(&home.join(".murmur"))
        .into_iter()
        .find(|dir| dir.join("trace.jsonl").is_file())
        .expect("a session directory");
    let events = common::read_whole_trace(&session.join("trace.jsonl"));
    let start = events
        .iter()
        .find(|event| event["event_type"] == "session_start")
        .unwrap();
    assert_eq!(
        start["effective_grants"]["achieved_containment"],
        "advisory"
    );
    assert_eq!(
        start["effective_grants"]["runtime_dir"]["read_only_to_commands"],
        false
    );

    let explained = mur(home)
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            home.to_str().unwrap(),
            "--explain-scope",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let explained = String::from_utf8_lossy(&explained);
    println!("{explained}");
    let commands = explained
        .lines()
        .find_map(|line| line.strip_prefix("  commands:  can change it — "))
        .unwrap_or_else(|| panic!("{explained}"));
    assert!(
        commands.contains(&format!("inside the workdir {}", home.display())),
        "{commands}"
    );
    let not_protected = explained
        .split("\nNot protected here\n")
        .nth(1)
        .unwrap_or_else(|| panic!("{explained}"));
    assert!(
        not_protected.contains(&format!("  - {commands}")),
        "{explained}"
    );

    let json = mur(home)
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            home.to_str().unwrap(),
            "--explain-scope",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        json["runtime_dir"]["read_only_to_commands"], false,
        "{json}"
    );
}

// ── what --explain-scope and doctor say here ─────────────────────────────────

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_explain_scope_and_doctor_name_the_runtime_directory() {
    for (floor, mechanism) in [
        (ContainmentClass::Scoped, "landlock"),
        (ContainmentClass::Sealed, "mountns+landlock"),
    ] {
        let test = "runtime_dir_explain_scope_and_doctor_name_the_runtime_directory";
        if skip_host(test, floor) {
            return;
        }
        let server = common::ScriptedServer::start(vec![]);
        let p = project(&server.endpoint, &Spec::default()).unwrap();
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
        let expected_path = p
            .home
            .path()
            .join(".murmur/sessions/<session-id>/runtime")
            .display()
            .to_string();
        assert!(
            rendered.contains(&format!(
                "Runtime directory\n  path:      {expected_path}\n  commands:  read only ({mechanism})\n"
            )),
            "{rendered}"
        );
        let json = mur(p.home.path())
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
            .stdout
            .clone();
        let json: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            json["runtime_dir"],
            json!({
                "path": expected_path,
                "read_only_to_commands": true,
                "enforced_by": mechanism,
                "not_protected_because": null,
            })
        );
    }

    let server = common::ScriptedServer::start(vec![]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let doctor = mur(p.home.path())
        .current_dir(p.project.path())
        .arg("doctor")
        .assert()
        .get_output()
        .stdout
        .clone();
    let doctor = String::from_utf8_lossy(&doctor);
    println!("{doctor}");
    assert!(
        doctor.contains("Runtime directory\n  path:      "),
        "{doctor}"
    );
    assert!(doctor.contains("  commands:  "), "{doctor}");
}

// ── a delegated child's --workdir is the directory its parent created ────────

fn identity_of(path: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path).unwrap();
    format!("{}:{}", meta.dev(), meta.ino())
}

fn run_with_identity(p: &Project, workdir: &Path, identity: &str) -> String {
    let output = mur(p.home.path())
        .args([
            "run",
            "--manifest",
            p.manifest.to_str().unwrap(),
            "--workdir",
            workdir.to_str().unwrap(),
            "--workdir-identity",
            identity,
            "--task",
            "x",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_workdir_identity_mismatch_refuses_before_writing() {
    if common::skip_without_host_support(
        "runtime_dir_workdir_identity_mismatch_refuses_before_writing",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let workdir = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();

    let stderr = run_with_identity(&p, workdir.path(), &identity_of(other.path()));
    println!("{stderr}");
    assert!(stderr.contains("E-RUN-051"), "{stderr}");
    assert!(
        stderr.contains(&workdir.path().display().to_string()),
        "{stderr}"
    );
    assert!(
        stderr.contains("the delegating capsule's child work folder in children/ changed"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_dir(workdir.path()).unwrap().count(),
        0,
        "nothing was written"
    );
    assert!(server.requests().is_empty());
}

#[test]
#[allow(clippy::print_stdout)]
fn runtime_dir_workdir_identity_refuses_a_link_swapped_onto_the_created_directory() {
    if common::skip_without_host_support(
        "runtime_dir_workdir_identity_refuses_a_link_swapped_onto_the_created_directory",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn("never sent")]);
    let p = project(&server.endpoint, &Spec::default()).unwrap();
    let parent = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let created = parent.path().join("child");
    fs::create_dir(&created).unwrap();
    let identity = identity_of(&created);
    // The capsule moves the created directory away and puts a link to an outside directory in
    // its place.
    fs::rename(&created, parent.path().join("moved")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &created).unwrap();

    let stderr = run_with_identity(&p, &created, &identity);
    println!("{stderr}");
    assert!(stderr.contains("E-RUN-051"), "{stderr}");
    assert_eq!(
        fs::read_dir(outside.path()).unwrap().count(),
        0,
        "the outside directory gains nothing"
    );
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
