//! The plan a model submits, run end to end by the session that was handed it.
//!
//! Every case here runs the real `mur run` binary against a stub inference endpoint, so what is
//! asserted is what a model would actually be offered and told: the tool array the driver was
//! sent, the `tool_result` text that came back, the session's own `trace.jsonl`, and the process's
//! exit status. Nothing about the scheduler is stubbed — the steps really run through this
//! session's own tool dispatch.
//!
//! The capsule is deliberately the smallest one that can hold a plan: one driver artifact, one
//! local-source skill, no `capabilities.shell.allow` and no `capabilities.spawn.allow`, so the
//! launch needs no delegated cgroup scope and runs on any host. The one case that does declare a
//! shell binary carries the usual host-support gate.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The one tool a plan step in this file calls. A skill, because a skill needs no component and
/// no host support: the runtime reads `skill.md` and hands the bytes back as the step's output.
const SKILL: &str = "notes";
/// Spelled nowhere else, so finding it in a step's output means it came from the skill file.
const SKILL_BODY: &str = "# Notes\nNOTES-BODY-7F3Q-PLAN\n";

// ── harness ──────────────────────────────────────────────────────────────────

fn publish_driver(home: &TempDir, artifacts: &Path) {
    let driver = common::create_driver_artifact(
        artifacts,
        DRIVER,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver).success();
}

/// Write the project: one driver, one local-source skill, and whichever grants the case needs.
///
/// `plan_grant` is the whole of what puts `submit-plan` in the capsule's inventory, so the cases
/// that assert absence differ from the ones that assert presence in this one boolean.
fn write_project(
    project: &Path,
    endpoint: &str,
    plan_grant: bool,
    shell_allow: &[&str],
) -> PathBuf {
    write_project_with(project, endpoint, plan_grant, shell_allow, &[])
}

/// [`write_project`] for a case that also declares `capabilities.filesystem.read_only`, which is
/// one of the two things the session's decision point refuses a call for.
fn write_project_with(
    project: &Path,
    endpoint: &str,
    plan_grant: bool,
    shell_allow: &[&str],
    read_only: &[&str],
) -> PathBuf {
    let skill_dir = project.join("skills").join(SKILL);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(skill_dir.join("skill.md"), SKILL_BODY).unwrap();

    let plan_block = if plan_grant {
        "  plan:\n    submit: true\n"
    } else {
        ""
    };
    let shell_block = if shell_allow.is_empty() {
        String::new()
    } else {
        let entries = shell_allow
            .iter()
            .map(|binary| format!("      - {binary}\n"))
            .collect::<String>();
        format!("  shell:\n    allow:\n{entries}")
    };

    let filesystem_block = if read_only.is_empty() {
        String::new()
    } else {
        let entries = read_only
            .iter()
            .map(|path| format!("      - {path}\n"))
            .collect::<String>();
        format!("  filesystem:\n    read_only:\n{entries}")
    };

    let manifest = format!(
        "name: plan-capsule\nversion: 0.1.0\nartifacts:\n  \
         - name: {DRIVER}\n    version: {DRIVER_VERSION}\n    runtime: driver\n  \
         - name: {SKILL}\n    source: ./skills/{SKILL}/skill.md\n    runtime: skill\n\
         capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
         {plan_block}{shell_block}{filesystem_block}\
         inference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
         api_key: test-key\n  driver:\n    artifact: {DRIVER}\n"
    );
    let manifest_path = project.join("murmur.yaml");
    fs::write(&manifest_path, manifest).unwrap();
    manifest_path
}

/// `mur run --manifest <path> --task <task> --verbose`, the whole capsule in one process.
fn run(home: &TempDir, manifest: &Path, task: &str) -> assert_cmd::assert::Assert {
    Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args([
            "run",
            "--manifest",
            manifest.to_str().unwrap(),
            "--task",
            task,
            "--verbose",
        ])
        .assert()
}

fn tool_use_turn(tool_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn(text: &str) -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// Every tool name the model was offered, across every request the endpoint received.
fn offered_tools(requests: &[Value]) -> HashSet<String> {
    let mut names = HashSet::new();
    for request in requests {
        if let Some(tools) = request.get("tools").and_then(Value::as_array) {
            for tool in tools {
                if let Some(name) = tool.get("name").and_then(Value::as_str) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names
}

/// The `tool_result` text for one `tool_use` id, with the untrusted fence taken back off.
///
/// The fence is applied once, by `dispatch_agent_tool_async`, to the whole plan report — so a
/// report that parses as JSON after exactly one unwrap is also the evidence that a step's own
/// output was not fenced a second time on its way in.
fn tool_result_text(requests: &[Value], tool_id: &str) -> String {
    let block = common::find_tool_result(requests, tool_id)
        .unwrap_or_else(|| panic!("no tool_result for {tool_id} in: {requests:#?}"));
    unfence(&common::extract_result_text(&block))
}

fn unfence(text: &str) -> String {
    let Some((open, rest)) = text.split_once('\n') else {
        return text.to_string();
    };
    if !open.starts_with("<untrusted-content source=tool:") || !open.ends_with('>') {
        return text.to_string();
    }
    rest.strip_suffix("\n</untrusted-content>")
        .unwrap_or_else(|| panic!("a fenced tool result must end at the closing marker: {text}"))
        .to_string()
}

/// The plan report the model was handed, parsed.
fn plan_report(requests: &[Value], tool_id: &str) -> Value {
    let text = tool_result_text(requests, tool_id);
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the plan report is JSON ({error}): {text}"))
}

fn workdir_from(assert: &assert_cmd::assert::Assert) -> PathBuf {
    common::parse_workdir_from_stdout(&String::from_utf8_lossy(&assert.get_output().stdout))
}

fn trace_events(workdir: &Path, event_type: &str) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .expect("a launched session writes trace.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

/// The two-step plan the happy path submits: `b` depends on `a` and reads its output.
fn two_step_plan() -> Value {
    json!({
        "id": "p1",
        "steps": [
            {"id": "a", "tool": SKILL},
            {
                "id": "b",
                "tool": SKILL,
                "depends_on": ["a"],
                "input": {"seen": "$a.output"}
            }
        ]
    })
}

// ── the happy path ───────────────────────────────────────────────────────────

/// One plan, two ordered steps, one reply. The model gets back every step's status and output in
/// a single tool result, and the run exits 0.
///
/// `b`'s success is what proves the ordering: `$a.output` resolves against results that already
/// exist, so a `b` dispatched before `a` had finished would have failed on an unready reference
/// rather than run early.
#[test]
fn a_submitted_plan_runs_its_steps_in_dependency_order_and_reports_both() {
    let server = common::ScriptedServer::start(vec![
        tool_use_turn(
            "toolu_plan",
            "submit-plan",
            json!({"plan": two_step_plan()}),
        ),
        end_turn("Plan run."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &[]);

    let assert = run(&home, &manifest, "run the plan").success();
    let workdir = workdir_from(&assert);
    let requests = server.requests();

    // The tool exists because the grant does, and the model was offered it.
    assert!(
        offered_tools(&requests).contains("submit-plan"),
        "a granted capsule offers the plan tool: {:?}",
        offered_tools(&requests)
    );

    let report = plan_report(&requests, "toolu_plan");
    assert_eq!(report["plan_id"], "p1", "{report}");
    assert_eq!(report["completed"], true, "{report}");
    assert_eq!(report["failed_step"], Value::Null, "{report}");

    let steps = report["steps"].as_array().expect("a steps array");
    assert_eq!(steps.len(), 2, "{report}");
    assert_eq!(steps[0]["step_id"], "a", "{report}");
    assert_eq!(steps[1]["step_id"], "b", "{report}");
    for step in steps {
        assert_eq!(step["status"], "success", "{step}");
        assert_eq!(step["error"], Value::Null, "{step}");
        assert!(
            step["output"]
                .as_str()
                .is_some_and(|output| output.contains("NOTES-BODY-7F3Q-PLAN")),
            "each step's output is the tool's own bytes: {step}"
        );
    }

    // The plan file is named from the session's own counter, never from the model's plan id.
    let plan_file = workdir.join("plans").join("plan-1.json");
    let written: Value = serde_json::from_str(&fs::read_to_string(&plan_file).unwrap()).unwrap();
    assert_eq!(written, two_step_plan(), "{plan_file:?}");

    // The interpolation, as the scheduler recorded it: `b` was dispatched with `a`'s real output
    // in place of the reference.
    let steps = trace_events(&workdir, "plan_step");
    let b = steps
        .iter()
        .find(|event| event["step_id"] == "b")
        .unwrap_or_else(|| panic!("no plan_step for b: {steps:#?}"));
    assert!(
        b["input"]["seen"]
            .as_str()
            .is_some_and(|seen| seen.contains("NOTES-BODY-7F3Q-PLAN")),
        "$a.output must have been substituted before dispatch: {b}"
    );
    assert_eq!(trace_events(&workdir, "plan_start").len(), 1);
    assert_eq!(
        trace_events(&workdir, "plan_end")[0]["outcome"],
        "completed"
    );
}

// ── the mechanical tail ──────────────────────────────────────────────────────

/// The work a plan is actually for: two independent commands and a step that reads both.
///
/// Gated on host support because the capsule declares `capabilities.shell.allow`, which refuses
/// the launch with `E-RUN-012` on a host that cannot delegate a cgroup v2 scope — before anything
/// this case observes. The happy path above covers the same dispatch arm, scheduler and report
/// shape without a subprocess.
#[test]
fn independent_shell_steps_run_together_and_a_later_step_reads_both() {
    if common::skip_without_host_support(
        "independent_shell_steps_run_together_and_a_later_step_reads_both",
    ) {
        return;
    }
    // Each command announces itself and then waits for the other, using nothing but `bash`
    // builtins — the sandbox permits executing only the allowlisted binary, so `sleep` and
    // `touch` are not available. A step that reaches the printf has seen the other step's marker
    // while its own was still running, which serial execution cannot produce; the `SECONDS`
    // bound is what stops a serial run from spinning forever instead of failing.
    let rendezvous = |mine: &str, theirs: &str, output: &str| {
        format!(
            "bash -c ': > {mine}; while [ ! -e {theirs} ] && [ $SECONDS -lt 20 ]; do :; done; \
             [ -e {theirs} ] && printf {output} || printf alone'"
        )
    };
    let plan = json!({
        "id": "tail",
        "steps": [
            {"id": "w1", "shell": rendezvous("w1.started", "w2.started", "one")},
            {"id": "w2", "shell": rendezvous("w2.started", "w1.started", "two")},
            {
                "id": "join",
                "tool": SKILL,
                "depends_on": ["w1", "w2"],
                "input": {"first": "$w1.output", "second": "$w2.output"}
            }
        ]
    });
    let server = common::ScriptedServer::start(vec![
        tool_use_turn("toolu_tail", "submit-plan", json!({"plan": plan})),
        end_turn("Tail run."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &["bash"]);

    let assert = run(&home, &manifest, "run the tail").success();
    let workdir = workdir_from(&assert);
    let report = plan_report(&server.requests(), "toolu_tail");

    assert_eq!(report["completed"], true, "{report}");
    let steps = report["steps"].as_array().expect("a steps array");
    assert_eq!(steps.len(), 3, "{report}");
    for step in steps {
        assert_eq!(step["status"], "success", "{step}");
    }
    assert_eq!(
        steps
            .iter()
            .find(|step| step["step_id"] == "w1")
            .map(|step| step["output"].clone()),
        Some(json!("one")),
        "{report}"
    );
    assert_eq!(
        steps
            .iter()
            .find(|step| step["step_id"] == "w2")
            .map(|step| step["output"].clone()),
        Some(json!("two")),
        "{report}"
    );

    // Both outputs reached the joining step, which is only possible if both had settled — and
    // neither is `alone`, which is only possible if both were running at the same time.
    let join = trace_events(&workdir, "plan_step")
        .into_iter()
        .find(|event| event["step_id"] == "join")
        .expect("a plan_step for join");
    assert_eq!(
        join["input"],
        json!({"first": "one", "second": "two"}),
        "{join}"
    );
}

// ── the decision point ───────────────────────────────────────────────────────

/// Every file called `name` anywhere under `root`.
fn find_all(root: &Path, name: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_all(&path, name));
        } else if entry.file_name() == name {
            found.push(path);
        }
    }
    found
}

/// The plan submitted by both halves of the case below: one command, writing one file.
fn one_write_plan() -> Value {
    json!({
        "id": "write",
        "steps": [{"id": "w", "shell": format!("bash -c ': > {PROTECTED}'")}]
    })
}

/// The file the write step targets, and the single `read_only` entry that covers it.
const PROTECTED: &str = "protected.txt";

/// A plan step is refused by `capabilities.filesystem.read_only`, exactly as the same command is
/// when the model runs it directly.
///
/// Asserted against a control run of the identical plan with the declaration removed, because the
/// measurement that matters is the difference: without it the file appears, with it the file does
/// not and the step carries the manifest's refusal. A step that entered underneath the decision
/// point would write the file in both runs.
///
/// Gated on host support for the same reason the mechanical tail above is: the capsule declares
/// `capabilities.shell.allow`.
#[test]
fn a_plan_step_is_refused_by_the_read_only_declaration() {
    if common::skip_without_host_support("a_plan_step_is_refused_by_the_read_only_declaration") {
        return;
    }

    // ── control: no declaration, so the write lands ──
    let server = common::ScriptedServer::start(vec![
        tool_use_turn(
            "toolu_write",
            "submit-plan",
            json!({"plan": one_write_plan()}),
        ),
        end_turn("Written."),
    ]);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &["bash"]);

    let assert = run(&home, &manifest, "write the file").success();
    let workdir = workdir_from(&assert);
    let report = plan_report(&server.requests(), "toolu_write");
    assert_eq!(report["completed"], true, "{report}");
    assert!(
        !find_all(&workdir, PROTECTED).is_empty(),
        "the control run must actually write {PROTECTED} under {workdir:?}"
    );

    // ── the same plan, with the path declared read-only ──
    let server = common::ScriptedServer::start(vec![
        tool_use_turn(
            "toolu_write",
            "submit-plan",
            json!({"plan": one_write_plan()}),
        ),
        end_turn("Refused."),
    ]);
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project_with(
        project.path(),
        &server.endpoint,
        true,
        &["bash"],
        &[PROTECTED],
    );

    let assert = run(&home, &manifest, "write the file").success();
    let workdir = workdir_from(&assert);
    let report = plan_report(&server.requests(), "toolu_write");

    assert_eq!(report["completed"], false, "{report}");
    assert_eq!(report["failed_step"], "w", "{report}");
    let error = report["steps"][0]["error"]
        .as_str()
        .unwrap_or_else(|| panic!("the refused step carries an error: {report}"));
    assert!(
        error.contains("Refused by the capsule manifest") && error.contains(PROTECTED),
        "the step names the manifest rule that refused it: {error}"
    );
    assert!(
        find_all(&workdir, PROTECTED).is_empty(),
        "a refused step must not have written anything: {workdir:?}"
    );

    // Recorded where a direct call's refusal is recorded, so an audit reads one kind of line.
    let denials = trace_events(&workdir, "protected_path_denied");
    assert_eq!(denials.len(), 1, "{denials:#?}");
    assert_eq!(denials[0]["call"], "shell", "{}", denials[0]);
    assert_eq!(denials[0]["rule"], PROTECTED, "{}", denials[0]);
}

// ── absence, not failure ─────────────────────────────────────────────────────

/// With no `capabilities.plan` block the tool does not exist: no manifest file, nothing in the
/// inventory, nothing in `tools_declared`. A call naming it anyway is refused with the missing
/// declaration named, and the session carries on.
#[test]
fn an_ungranted_capsule_is_offered_no_plan_tool_and_is_refused_if_it_names_one() {
    let server = common::ScriptedServer::start(vec![
        tool_use_turn(
            "toolu_denied",
            "submit-plan",
            json!({"plan": two_step_plan()}),
        ),
        end_turn("Refused."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, false, &[]);

    let assert = run(&home, &manifest, "try to plan").success();
    let workdir = workdir_from(&assert);
    let requests = server.requests();

    assert!(
        !workdir.join("tools").join("submit-plan").exists(),
        "an ungranted capsule is written no plan tool manifest"
    );
    assert!(
        !offered_tools(&requests).contains("submit-plan"),
        "the tool must be absent from the inventory: {:?}",
        offered_tools(&requests)
    );
    let declared = trace_events(&workdir, "session_start")[0]["tools_declared"].clone();
    assert!(
        !declared
            .as_array()
            .expect("tools_declared is an array")
            .contains(&json!("submit-plan")),
        "{declared}"
    );

    // Refused by name, with the declaration to add — and the turn after it still ran.
    let refusal = tool_result_text(&requests, "toolu_denied");
    assert!(refusal.contains("submit-plan"), "{refusal}");
    assert!(refusal.contains("capabilities.plan.submit"), "{refusal}");
    assert!(
        !workdir.join("plans").exists(),
        "a refused call writes no plan file"
    );
}

// ── refusals that never reach the scheduler ──────────────────────────────────

/// A plan cannot submit a plan. Refused by `validate_plan` before any step runs, so the report
/// names the offending step and nothing else happened.
#[test]
fn a_plan_step_naming_the_plan_tool_is_refused_before_anything_runs() {
    let plan = json!({
        "id": "nested",
        "steps": [
            {"id": "outer", "tool": "submit-plan", "input": {"plan": {"id": "inner", "steps": []}}}
        ]
    });
    let server = common::ScriptedServer::start(vec![
        tool_use_turn("toolu_nested", "submit-plan", json!({"plan": plan})),
        end_turn("Refused."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &[]);

    let assert = run(&home, &manifest, "try to nest").success();
    let workdir = workdir_from(&assert);
    let report = plan_report(&server.requests(), "toolu_nested");

    assert_eq!(report["completed"], false, "{report}");
    assert_eq!(report["failed_step"], "outer", "{report}");
    let error = report["steps"][0]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("cannot submit another plan"),
        "the refusal states the rule: {report}"
    );
    // Refused ahead of dispatch, so no step ever started.
    assert!(trace_events(&workdir, "plan_step_start").is_empty());
}

/// A failing first step stops the plan under the default `on_error: fail`: the dependent step is
/// absent from the report, and the result the model is handed is marked as a failure.
///
/// The step that fails names the driver artifact — a real directory under `tools/`, so the plan
/// validates, and a tool the agent's allowlist does not cover, so dispatch refuses it. That is a
/// failure produced by this session's own dispatch rather than by the scheduler.
#[test]
fn a_failed_step_stops_the_plan_and_is_reported_as_a_failure() {
    let plan = json!({
        "id": "broken",
        "steps": [
            {"id": "a", "tool": DRIVER},
            {"id": "b", "tool": SKILL, "depends_on": ["a"]}
        ]
    });
    let server = common::ScriptedServer::start(vec![
        tool_use_turn("toolu_broken", "submit-plan", json!({"plan": plan})),
        end_turn("Reported."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &[]);

    let assert = run(&home, &manifest, "run the broken plan").success();
    let requests = server.requests();
    let report = plan_report(&requests, "toolu_broken");

    assert_eq!(report["completed"], false, "{report}");
    assert_eq!(report["failed_step"], "a", "{report}");
    let steps = report["steps"].as_array().expect("a steps array");
    assert_eq!(steps.len(), 1, "the dependent step is absent: {report}");
    assert!(
        steps[0]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the failing step carries its error text: {report}"
    );

    // The result carries `Status::Failed`, so the call is recorded as an error rather than as a
    // passing call whose body happens to say otherwise. (The fixture driver does not put the
    // runtime's `is_error` flag onto the wire, so the trace is where it is observable.)
    let workdir = workdir_from(&assert);
    let call = trace_events(&workdir, "tool_call")
        .into_iter()
        .find(|event| event["tool_name"] == "submit-plan")
        .expect("a tool_call for the plan tool");
    assert_eq!(call["status"], "error", "{call}");
}

/// Malformed arguments are refused by the tool, naming itself and the argument it needs — no plan
/// file is written and the scheduler never starts.
#[test]
fn malformed_input_is_refused_before_a_plan_file_exists() {
    let server = common::ScriptedServer::start(vec![
        tool_use_turn("toolu_empty", "submit-plan", json!({})),
        tool_use_turn("toolu_string", "submit-plan", json!({"plan": "build it"})),
        end_turn("Refused twice."),
    ]);

    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    publish_driver(&home, artifacts.path());
    let manifest = write_project(project.path(), &server.endpoint, true, &[]);

    let assert = run(&home, &manifest, "submit nonsense").success();
    let workdir = workdir_from(&assert);
    let requests = server.requests();

    for tool_id in ["toolu_empty", "toolu_string"] {
        let refusal = tool_result_text(&requests, tool_id);
        assert!(refusal.contains("submit-plan"), "{tool_id}: {refusal}");
        assert!(refusal.contains("'plan'"), "{tool_id}: {refusal}");
    }
    assert!(
        !workdir.join("plans").exists(),
        "a refused call writes no plan file"
    );
    assert!(trace_events(&workdir, "plan_start").is_empty());
}
