#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, RuntimeError, StagedSession};
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

#[test]
fn inline_system_prompt_appears_in_every_api_call() {
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let system_prompt = "Always begin every response with CONFIRMED:";
    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        &format!("  system_prompt: \"{system_prompt}\"\n"),
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected two inference requests");
    for request in requests {
        let system_field = request["system"].as_str().expect("system field should be present");
        assert!(
            system_field.starts_with("[Capsule]\nName:"),
            "system field should start with [Capsule] block; got:\n{system_field}"
        );
        assert!(
            system_field.contains(system_prompt),
            "system field should contain the manifest system prompt; got:\n{system_field}"
        );
    }
}

#[test]
fn system_prompt_file_contents_injected_as_system_param() {
    let server = ScriptedServer::start(vec![one_turn_response()]);

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let conventions =
        "Follow architecture rules strictly.\nAlways prefix final answers with CONFIRMED:.";
    fs::write(project.path().join("conventions.md"), conventions).unwrap();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        "  system_prompt_file: conventions.md\n",
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Say hello.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 1, "expected one inference request");
    let system_field = requests[0]["system"].as_str().expect("system field should be present");
    assert!(
        system_field.starts_with("[Capsule]\nName:"),
        "system field should start with [Capsule] block; got:\n{system_field}"
    );
    assert!(
        system_field.contains(conventions),
        "system field should contain the system_prompt_file contents; got:\n{system_field}"
    );
}

#[test]
fn missing_system_prompt_file_fails_at_launch() {
    let server = ScriptedServer::start(Vec::new());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(
        project.path(),
        &server.endpoint,
        "  system_prompt_file: missing-conventions.md\n",
    );

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Say hello.").unwrap();

    let err =
        launch_session(staged, |_| {}).expect_err("launch should fail for missing prompt file");

    match err {
        RuntimeError::SystemPromptFileRead { path, .. } => {
            assert!(path.ends_with("missing-conventions.md"));
        }
        other => panic!("expected SystemPromptFileRead, got: {other:?}"),
    }

    assert_eq!(
        server.requests().len(),
        0,
        "launch failure should occur before any inference call"
    );
}

#[test]
fn no_system_prompt_field_sends_no_system_param() {
    let server = ScriptedServer::start(two_turn_responses());

    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = create_driver_artifact(
        artifact_dir.path(),
        DRIVER_ANTHROPIC_NAME,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_agent_manifest(project.path(), &server.endpoint, "");

    let staged = stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Run echo and report.").unwrap();

    launch_session(staged, |_| {}).expect("agent launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected two inference requests");
    for request in requests {
        let system_field = request["system"].as_str().expect("system field should be present");
        assert!(
            system_field.starts_with("[Capsule]\nName:"),
            "system field should contain [Capsule] identity block even with no manifest system prompt; got:\n{system_field}"
        );
    }
}

// ── `mur run --system-prompt` ─────────────────────────────────────────────────
//
// These spawn the compiled `mur` binary rather than staging in-process: the override lives in
// `run_run`, and `murmur-cli` has no lib target for a test to call it through. The same
// `ScriptedServer` still captures the driver payload — it listens on a real port, which a child
// process reaches exactly as this one does.

/// A capsule whose manifest declares an inline prompt, run with an override: the model receives
/// the override and never the manifest's own prompt, and the trace names the CLI as the source.
#[test]
fn cli_system_prompt_overrides_inline_manifest_prompt() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("  system_prompt: \"Manifest prompt\"\n");

    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);

    let system_field = fixture.only_system_field();
    assert!(
        system_field.starts_with("[Capsule]\nName:"),
        "system field should still start with the [Capsule] block; got:\n{system_field}"
    );
    assert!(
        system_field.contains("CLI prompt"),
        "system field should carry the override; got:\n{system_field}"
    );
    assert!(
        !system_field.contains("Manifest prompt"),
        "system field must not carry the overridden manifest prompt; got:\n{system_field}"
    );

    let start = session_start(&workdir);
    assert_eq!(start["system_prompt_source"], "cli");
    assert_eq!(start["system_prompt_sha256"], sha256_hex_of("CLI prompt"));
}

/// Without the flag, the trace attributes the prompt to the manifest — and hashes the same text
/// the model received.
#[test]
fn without_the_flag_the_trace_attributes_the_prompt_to_the_manifest() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("  system_prompt: \"Manifest prompt\"\n");

    let workdir = fixture.run_ok(&manifest_path, &[]);

    assert!(fixture.only_system_field().contains("Manifest prompt"));
    let start = session_start(&workdir);
    assert_eq!(start["system_prompt_source"], "manifest");
    assert_eq!(start["system_prompt_sha256"], sha256_hex_of("Manifest prompt"));
}

/// A manifest that declares no prompt at all records `"none"` — the negative case is written,
/// not omitted, so it is distinguishable from a trace predating the field.
#[test]
fn without_a_prompt_the_trace_records_none() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("");

    let workdir = fixture.run_ok(&manifest_path, &[]);

    let start = session_start(&workdir);
    assert_eq!(start["system_prompt_source"], "none");
    assert!(start["system_prompt_sha256"].is_null());
}

/// The override wins over `system_prompt_file` even when the file is not on disk: the field is
/// cleared before resolution, so the read that `missing_system_prompt_file_fails_at_launch`
/// proves fatal never happens.
#[test]
fn cli_system_prompt_overrides_a_prompt_file_that_does_not_exist() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("  system_prompt_file: missing-conventions.md\n");

    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);

    assert!(fixture.only_system_field().contains("CLI prompt"));
    assert_eq!(session_start(&workdir)["system_prompt_source"], "cli");
}

/// Overriding a `system_prompt_artifact` has to reach the two readers that never went through
/// prompt resolution: the callable tool inventory, which excluded the skill, and MURMUR.md,
/// which labelled it as bound. Both follow from clearing the field CLI-side.
#[test]
fn cli_system_prompt_restores_an_overridden_prompt_artifact_to_the_inventory() {
    // Baseline: the manifest declaration alone binds the skill and hides it from the inventory.
    let baseline = CliFixture::new(vec![one_turn_response()]);
    let manifest_path =
        baseline.write_manifest_with_skill(&format!("  system_prompt_artifact: {SKILL_NAME}\n"));
    let baseline_workdir = baseline.run_ok(&manifest_path, &[]);

    let baseline_system = baseline.only_system_field();
    assert!(
        baseline_system.contains("Always answer in haiku."),
        "the skill's own text should be the system prompt; got:\n{baseline_system}"
    );
    assert!(
        !tool_names(&baseline.requests()[0]).contains(&SKILL_NAME.to_string()),
        "a bound skill is not separately callable"
    );
    assert!(murmur_md(&baseline_workdir).contains("bound as system prompt"));

    // Overridden: the skill is an ordinary callable skill again.
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path =
        fixture.write_manifest_with_skill(&format!("  system_prompt_artifact: {SKILL_NAME}\n"));
    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);

    let system_field = fixture.only_system_field();
    assert!(system_field.contains("CLI prompt"));
    assert!(
        !system_field.contains("Always answer in haiku."),
        "the overridden skill's text must not reach the model as a prompt; got:\n{system_field}"
    );
    assert!(
        tool_names(&fixture.requests()[0]).contains(&SKILL_NAME.to_string()),
        "the released skill should be back in the callable inventory"
    );
    let murmur_md = murmur_md(&workdir);
    assert!(
        !murmur_md.contains("bound as system prompt"),
        "MURMUR.md must not still call the skill bound; got:\n{murmur_md}"
    );
    assert_eq!(session_start(&workdir)["system_prompt_source"], "cli");
}

/// The flag applies to a manifest that declared no prompt of its own — there is nothing to
/// override, but the operator still set the prompt, and the trace says so.
#[test]
fn cli_system_prompt_applies_when_the_manifest_declares_no_prompt() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("");

    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);

    assert!(fixture.only_system_field().contains("CLI prompt"));
    let start = session_start(&workdir);
    assert_eq!(start["system_prompt_source"], "cli");
    assert_eq!(start["system_prompt_sha256"], sha256_hex_of("CLI prompt"));
}

/// An empty (here whitespace-only) value is not an error: it clears the manifest's prompt down
/// to the same payload `no_system_prompt_field_sends_no_system_param` asserts. The source stays
/// `"cli"` — the operator asked for this — while the hash goes null.
#[test]
fn cli_empty_system_prompt_clears_the_manifest_prompt() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("  system_prompt: \"Manifest prompt\"\n");

    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "   "]);

    let system_field = fixture.only_system_field();
    assert!(system_field.starts_with("[Capsule]\nName:"));
    assert!(
        !system_field.contains("Manifest prompt"),
        "an empty override still clears the manifest prompt; got:\n{system_field}"
    );

    let start = session_start(&workdir);
    assert_eq!(start["system_prompt_source"], "cli");
    assert!(start["system_prompt_sha256"].is_null());
}

/// The value is trimmed before use, matching what the manifest's own inline prompt gets at
/// parse time — so the hash of a padded value equals the hash of the bare text.
#[test]
fn cli_system_prompt_is_trimmed_before_use() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("");

    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "  \n CLI prompt \n\n "]);

    assert_eq!(
        session_start(&workdir)["system_prompt_sha256"],
        sha256_hex_of("CLI prompt")
    );
}

/// The prompt text itself is capsule content: withheld from the trace by default, written
/// verbatim only when the manifest opts in the same way it opts in to tool output.
#[test]
fn trace_records_the_prompt_verbatim_only_when_tool_output_is_included() {
    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest("  system_prompt: \"Manifest prompt\"\n");
    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);
    let start = session_start(&workdir);
    assert!(
        start.get("system_prompt").is_none(),
        "prompt text must be withheld by default; got: {start}"
    );

    let fixture = CliFixture::new(vec![one_turn_response()]);
    let manifest_path = fixture.write_manifest_with_trailer(
        "  system_prompt: \"Manifest prompt\"\n",
        "trace:\n  include_tool_output: true\n",
    );
    let workdir = fixture.run_ok(&manifest_path, &["--system-prompt", "CLI prompt"]);
    assert_eq!(session_start(&workdir)["system_prompt"], "CLI prompt");
}

/// A script capsule has no prompt to override. The flag is refused before anything is staged.
#[test]
fn cli_system_prompt_is_rejected_for_a_capsule_without_inference() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        "name: script-capsule\nversion: 0.1.0\n",
    )
    .unwrap();

    let assert = mur_run(
        &home,
        &project.path().join("murmur.yaml"),
        &["--system-prompt", "CLI prompt"],
    )
    .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(stderr.contains("E-IO-003"), "expected E-IO-003; got: {stderr}");
    assert!(stderr.contains("--system-prompt"), "error should name the flag; got: {stderr}");
    assert!(
        stderr.contains("inference:"),
        "error should name the missing block; got: {stderr}"
    );
    assert!(
        !project.path().join("workdir").exists(),
        "rejection must happen before any workdir is created"
    );
}

/// `--explain-scope` reports the capability grant set; a system prompt is not a grant, so the
/// flag leaves its output untouched — and, like `--task`, stages nothing.
#[test]
fn explain_scope_output_is_unchanged_by_system_prompt() {
    let fixture = CliFixture::new(Vec::new());
    let manifest_path = fixture.write_manifest("  system_prompt: \"Manifest prompt\"\n");

    let plain = mur_run(&fixture.home, &manifest_path, &["--explain-scope"])
        .success()
        .get_output()
        .stdout
        .clone();
    let with_flag = mur_run(
        &fixture.home,
        &manifest_path,
        &["--explain-scope", "--system-prompt", "CLI prompt"],
    )
    .success()
    .get_output()
    .stdout
    .clone();
    assert_eq!(
        String::from_utf8(plain).unwrap(),
        String::from_utf8(with_flag).unwrap()
    );

    let plain_json = mur_run(&fixture.home, &manifest_path, &["--explain-scope", "--json"])
        .success()
        .get_output()
        .stdout
        .clone();
    let with_flag_json = mur_run(
        &fixture.home,
        &manifest_path,
        &["--explain-scope", "--json", "--system-prompt", "CLI prompt"],
    )
    .success()
    .get_output()
    .stdout
    .clone();
    assert_eq!(
        String::from_utf8(plain_json).unwrap(),
        String::from_utf8(with_flag_json).unwrap()
    );
}

// ── CLI test scaffolding ─────────────────────────────────────────────────────

const SKILL_NAME: &str = "house-style";
const SKILL_VERSION: &str = "0.1.0";
const SKILL_CONTENT: &str = "# House style\nAlways answer in haiku.";

/// An isolated `$HOME`, a project directory with the driver installed into its store, and a
/// `ScriptedServer` — everything one `mur run` subprocess needs.
///
/// The manifest declares no `shell` capability: none of these tests calls a tool, and granting
/// shell would make the launch require a delegated cgroup scope for host-process bounding, which
/// is a property of the host rather than of anything this slice changed.
struct CliFixture {
    home: TempDir,
    artifacts: TempDir,
    project: TempDir,
    server: ScriptedServer,
}

impl CliFixture {
    fn new(responses: Vec<String>) -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            artifacts: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
            server: ScriptedServer::start(responses),
        }
    }

    fn write_manifest(&self, inference_extra: &str) -> PathBuf {
        self.write_manifest_with_trailer(inference_extra, "")
    }

    /// A manifest that also declares the `house-style` skill, for the `system_prompt_artifact`
    /// tests.
    fn write_manifest_with_skill(&self, inference_extra: &str) -> PathBuf {
        let manifest_path = create_agent_manifest_full(
            self.project.path(),
            &self.server.endpoint,
            &format!("  - name: {SKILL_NAME}\n    version: {SKILL_VERSION}\n    runtime: skill\n"),
            "",
            inference_extra,
            "",
        );
        let skill = common::create_skill_artifact(
            self.artifacts.path(),
            SKILL_NAME,
            SKILL_VERSION,
            SKILL_CONTENT,
        );
        self.install(&skill);
        self.install_driver();
        manifest_path
    }

    fn write_manifest_with_trailer(&self, inference_extra: &str, trailer: &str) -> PathBuf {
        let manifest_path = create_agent_manifest_full(
            self.project.path(),
            &self.server.endpoint,
            "",
            "",
            inference_extra,
            trailer,
        );
        self.install_driver();
        manifest_path
    }

    fn install_driver(&self) {
        let driver = create_driver_artifact(
            self.artifacts.path(),
            DRIVER_ANTHROPIC_NAME,
            &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        self.install(&driver);
    }

    /// `mur install <path>` into the *project* store — the store `mur run` stages from. This is
    /// the whole reason these tests write murmur.yaml first: `mur install` locates the project
    /// by it.
    fn install(&self, artifact_path: &Path) {
        common::install_artifact_to_project(self.project.path(), artifact_path).success();
    }

    /// Runs `mur run --json --task ...` to completion and returns the session workdir.
    fn run_ok(&self, manifest_path: &Path, extra_args: &[&str]) -> PathBuf {
        let mut args = vec!["--task", "Say hello.", "--json"];
        args.extend_from_slice(extra_args);
        let assert = mur_run(&self.home, manifest_path, &args).success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        parse_workdir_from_json(&stdout)
    }

    fn requests(&self) -> Vec<Value> {
        self.server.requests()
    }

    fn only_system_field(&self) -> String {
        let requests = self.server.requests();
        assert_eq!(requests.len(), 1, "expected one inference request");
        requests[0]["system"]
            .as_str()
            .expect("system field should be present")
            .to_string()
    }
}

fn mur_run(home: &TempDir, manifest_path: &Path, extra_args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = assert_cmd::Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .args(extra_args)
        .assert()
}

/// The `--json` launch line is the first parseable JSON object on stdout.
fn parse_workdir_from_json(stdout: &str) -> PathBuf {
    let line = stdout
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or_else(|| panic!("no JSON launch line in stdout:\n{stdout}"));
    PathBuf::from(line["workdir"].as_str().expect("workdir in launch JSON"))
}

/// The single `session_start` record of a one-task session.
fn session_start(workdir: &Path) -> Value {
    let content = fs::read_to_string(workdir.join("trace.jsonl")).unwrap();
    let starts: Vec<Value> = content
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event_type"] == "session_start")
        .collect();
    assert_eq!(starts.len(), 1, "expected one session_start record");
    starts.into_iter().next().unwrap()
}

fn murmur_md(workdir: &Path) -> String {
    fs::read_to_string(workdir.join("MURMUR.md")).unwrap()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn sha256_hex_of(text: &str) -> String {
    murmur_artifact::sha256_hex(text.as_bytes())
}

fn stage_agent_session(home: &TempDir, project_dir: &Path, manifest_path: &Path) -> StagedSession {
    common::stage_agent_session(home, project_dir, manifest_path)
}

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn create_driver_artifact(dir: &Path, name: &str, wasm_path: &Path) -> PathBuf {
    common::create_driver_artifact(dir, name, DRIVER_VERSION, wasm_path)
}

fn create_agent_manifest(project_dir: &Path, endpoint: &str, inference_extra: &str) -> PathBuf {
    create_agent_manifest_full(project_dir, endpoint, "", SHELL_CAPABILITY, inference_extra, "")
}

/// `capabilities.shell.allow: [bash]`, for the tests whose scripted turns include a bash tool
/// call. Granting it makes the launch require a delegated cgroup scope for host-process
/// bounding, so the tests that never call a tool leave it out and pass `""` instead.
const SHELL_CAPABILITY: &str = "  shell:\n    allow:\n      - bash\n";

/// `create_agent_manifest` with three extra insertion points: additional `artifacts:` entries,
/// the capability block beyond `network`, and a top-level trailer (used for `trace:`).
fn create_agent_manifest_full(
    project_dir: &Path,
    endpoint: &str,
    extra_artifacts: &str,
    extra_capabilities: &str,
    inference_extra: &str,
    trailer: &str,
) -> PathBuf {
    fs::write(
        project_dir.join("murmur.yaml"),
        "registry:\n  default: local\n",
    )
    .unwrap();

    let manifest = format!(
        concat!(
            "name: system-prompt-test\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "{extra_artifacts}",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "{extra_capabilities}",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "{inference_extra}",
            "{trailer}",
        ),
        driver_name = DRIVER_ANTHROPIC_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        extra_artifacts = extra_artifacts,
        extra_capabilities = extra_capabilities,
        inference_extra = inference_extra,
        trailer = trailer,
    );

    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

fn one_turn_response() -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn two_turn_responses() -> Vec<String> {
    vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_bash",
                "name": "bash",
                "input": {"command": "echo hello"}
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
            "content": [{"type": "text", "text": "done"}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]
}
