//! `inference.instruction_files`: listed work-folder files reach the model as labelled blocks after
//! the manifest's own prompt, read once at staging, and `session_start` records what became of
//! each entry.
//!
//! The in-process tests stage with an explicit work folder, the shape `mur run --workdir` takes,
//! so the files exist before staging reads them. The CLI tests spawn `mur run` for what lives in
//! `run_run`: manifest refusal and `--system-prompt`.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

use capsule_runtime::launch_session;
use serde_json::{json, Value};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER_ANTHROPIC_NAME: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const AGENTS_TEXT: &str = "Use tabs.\nRun `make check` before you finish.\n";

/// A home with the anthropic fixture driver published, a project, a work folder and a scripted
/// provider.
struct Fixture {
    home: TempDir,
    _artifacts: TempDir,
    project: TempDir,
    workdir: TempDir,
    server: ScriptedServer,
}

impl Fixture {
    fn new(responses: Vec<String>) -> Self {
        let fixture = Self {
            home: tempfile::tempdir().unwrap(),
            _artifacts: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
            workdir: tempfile::tempdir().unwrap(),
            server: ScriptedServer::start(responses),
        };
        let driver = common::create_driver_artifact(
            fixture._artifacts.path(),
            DRIVER_ANTHROPIC_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&fixture.home, &driver).success();
        fixture
    }

    fn work(&self) -> &Path {
        self.workdir.path()
    }

    fn write_manifest(&self, capabilities_extra: &str, inference_extra: &str) -> PathBuf {
        write_manifest(
            self.project.path(),
            &self.server.endpoint,
            capabilities_extra,
            inference_extra,
        )
    }

    /// Stages against the work folder, writes the task, launches, and returns the records
    /// directory and the session's `MURMUR.md` as staging wrote it.
    fn launch(&self, manifest_path: &Path) -> (PathBuf, String) {
        let staged = common::stage_agent_session_with_workdir(
            &self.home,
            self.project.path(),
            manifest_path,
            self.work(),
        );
        let murmur_md = fs::read_to_string(staged.runtime_dir.join("MURMUR.md")).unwrap();
        let records = staged.records_dir.clone();
        fs::write(staged.accessible_workdir.join("task.md"), "Say hello.").unwrap();
        launch_session(staged, |_| {}).expect("agent launch should succeed");
        (records, murmur_md)
    }

    fn system_texts(&self) -> Vec<String> {
        self.server
            .requests()
            .iter()
            .map(|request| common::system_text(&request["system"]).expect("a system field"))
            .collect()
    }
}

fn write_manifest(
    project: &Path,
    endpoint: &str,
    capabilities_extra: &str,
    inference_extra: &str,
) -> PathBuf {
    let manifest = format!(
        "name: instruction-files-test\nversion: 0.1.0\nartifacts:\n  - name: \
         {DRIVER_ANTHROPIC_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    \
         gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  \
         network:\n    allow:\n      - {endpoint}\n{capabilities_extra}inference:\n  transport: \
         http\n  model: test-model\n  driver:\n    artifact: {DRIVER_ANTHROPIC_NAME}\n\
         {inference_extra}"
    );
    let path = project.join("murmur.yaml");
    fs::write(&path, manifest).unwrap();
    path
}

fn session_start(records: &Path) -> Value {
    fs::read_to_string(records.join("trace.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|event| event["event_type"] == "session_start")
        .expect("a session_start record")
}

fn sha256_hex_of(text: &str) -> String {
    murmur_artifact::sha256_hex(text.as_bytes())
}

fn agents_block() -> String {
    format!("[Instruction file: AGENTS.md]\n{}", AGENTS_TEXT.trim_end())
}

/// Every request's system text ends with the manifest prompt and then the block, the same bytes
/// on every request of the task, and `session_start` records the read.
#[test]
fn instruction_files_follow_the_manifest_prompt_on_every_request() {
    if common::skip_without_host_support(
        "instruction_files_follow_the_manifest_prompt_on_every_request",
    ) {
        return;
    }
    let fixture = Fixture::new(two_turn_responses());
    fs::write(fixture.work().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    let manifest = fixture.write_manifest(
        "  shell:\n    allow:\n      - bash\n",
        "  system_prompt: \"MANIFEST PROMPT\"\n  instruction_files: [AGENTS.md]\n",
    );

    let (records, _) = fixture.launch(&manifest);

    let systems = fixture.system_texts();
    assert_eq!(systems.len(), 2);
    assert!(systems[0].starts_with("[Capsule]\nName:"), "{}", systems[0]);
    assert!(
        systems[0].ends_with(&format!("MANIFEST PROMPT\n\n{}", agents_block())),
        "{}",
        systems[0]
    );
    assert_eq!(
        systems[0], systems[1],
        "the system text is launch-invariant"
    );

    let start = session_start(&records);
    assert_eq!(
        start["instruction_files"],
        json!([{
            "path": "AGENTS.md",
            "status": "read",
            "resolved": "AGENTS.md",
            "bytes": AGENTS_TEXT.len(),
            "included_bytes": AGENTS_TEXT.len(),
            "truncated": false,
            "sha256": sha256_hex_of(AGENTS_TEXT),
        }])
    );
    // The operator's own prompt keeps its record: the blocks are not part of it.
    assert_eq!(start["system_prompt_source"], "manifest");
    assert_eq!(
        start["system_prompt_sha256"],
        sha256_hex_of("MANIFEST PROMPT")
    );
}

/// A listed file that is not there is skipped and recorded; the launch goes ahead.
#[test]
fn instruction_files_missing_entry_is_recorded_and_skipped() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    fs::write(fixture.work().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    let manifest = fixture.write_manifest("", "  instruction_files: [AGENTS.md, CLAUDE.md]\n");

    let (records, murmur_md) = fixture.launch(&manifest);

    let system = &fixture.system_texts()[0];
    assert!(
        system.ends_with(&format!("\n\n{}", agents_block())),
        "{system}"
    );
    assert_eq!(system.matches("[Instruction file:").count(), 1, "{system}");
    let start = session_start(&records);
    assert_eq!(
        start["instruction_files"][1],
        json!({"path": "CLAUDE.md", "status": "missing"})
    );
    assert!(
        murmur_md.contains(
            "## Instruction Files\n\n- In the system prompt: AGENTS.md\n- Not in the system \
             prompt: CLAUDE.md (missing)\n\n## Runtime Directory"
        ),
        "{murmur_md}"
    );
}

/// `AGENTS.md -> CLAUDE.md` with both listed is one block under the first name, and the link
/// latches no refusal: the task ends ok.
#[test]
fn instruction_files_one_file_under_two_names_is_one_block() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    fs::write(fixture.work().join("CLAUDE.md"), AGENTS_TEXT).unwrap();
    symlink("CLAUDE.md", fixture.work().join("AGENTS.md")).unwrap();
    let manifest = fixture.write_manifest("", "  instruction_files: [AGENTS.md, CLAUDE.md]\n");

    let (records, _) = fixture.launch(&manifest);

    let system = &fixture.system_texts()[0];
    assert!(
        system.ends_with(&format!(
            "[Instruction file: AGENTS.md (also listed as CLAUDE.md)]\n{}",
            AGENTS_TEXT.trim_end()
        )),
        "{system}"
    );
    assert_eq!(system.matches("Use tabs.").count(), 1, "{system}");
    let start = session_start(&records);
    assert_eq!(start["instruction_files"][0]["status"], "read");
    assert_eq!(start["instruction_files"][0]["resolved"], "CLAUDE.md");
    assert_eq!(
        start["instruction_files"][1],
        json!({"path": "CLAUDE.md", "status": "same_file", "same_as": "AGENTS.md"})
    );
}

/// A link out of the work folder, on the file or on a directory above it, is never read.
#[test]
fn instruction_files_links_out_of_the_work_folder_are_not_read() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    let outside = tempfile::tempdir().unwrap();
    let marker = "OUTSIDE-MARKER-6f1c2a";
    fs::write(outside.path().join("secret.md"), marker).unwrap();
    fs::write(outside.path().join("AGENTS.md"), marker).unwrap();
    symlink(
        outside.path().join("secret.md"),
        fixture.work().join("AGENTS.md"),
    )
    .unwrap();
    symlink(outside.path(), fixture.work().join("docs")).unwrap();
    let manifest = fixture.write_manifest("", "  instruction_files: [AGENTS.md, docs/AGENTS.md]\n");

    let (records, _) = fixture.launch(&manifest);

    for request in fixture.server.requests() {
        assert!(!request.to_string().contains(marker), "{request}");
    }
    let start = session_start(&records);
    assert_eq!(
        start["instruction_files"],
        json!([
            {"path": "AGENTS.md", "status": "outside_work_folder"},
            {"path": "docs/AGENTS.md", "status": "outside_work_folder"},
        ])
    );
}

/// A 40 000-byte file reaches the model as its first 32 768 bytes, and says so.
#[test]
fn instruction_files_over_the_limit_are_cut_and_say_so() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    let text = "abcdefghij".repeat(4_000);
    fs::write(fixture.work().join("AGENTS.md"), &text).unwrap();
    let manifest = fixture.write_manifest("", "  instruction_files: [AGENTS.md]\n");

    let (records, murmur_md) = fixture.launch(&manifest);

    let system = &fixture.system_texts()[0];
    assert!(
        system.ends_with(&format!(
            "\n\n[Instruction file: AGENTS.md, first 32768 of 40000 bytes]\n{}",
            &text[..32_768]
        )),
        "the block is the first 32768 bytes under a header that says so"
    );
    let start = session_start(&records);
    let entry = &start["instruction_files"][0];
    assert_eq!(entry["truncated"], true);
    assert_eq!(entry["bytes"], 40_000);
    assert_eq!(entry["included_bytes"], 32_768);
    assert_eq!(entry["sha256"], sha256_hex_of(&text[..32_768]));
    assert!(
        murmur_md.contains("- In the system prompt: AGENTS.md (first 32768 of 40000 bytes)\n"),
        "{murmur_md}"
    );
}

/// Files with a listed name deeper in the work folder are named in `MURMUR.md` and not loaded;
/// one under `.git` is not even named.
#[test]
fn instruction_files_nested_copies_are_listed_not_loaded() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    fs::write(fixture.work().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    fs::create_dir_all(fixture.work().join("src/api")).unwrap();
    fs::write(fixture.work().join("src/api/AGENTS.md"), "NESTED-MARKER").unwrap();
    fs::create_dir_all(fixture.work().join(".git")).unwrap();
    fs::write(fixture.work().join(".git/AGENTS.md"), "GIT-MARKER").unwrap();
    let manifest = fixture.write_manifest("", "  instruction_files: [AGENTS.md]\n");

    let (_, murmur_md) = fixture.launch(&manifest);

    let section_start = murmur_md
        .find("## Instruction Files\n")
        .expect("the section is present");
    let section_end = murmur_md.find("## Runtime Directory\n").unwrap();
    let section = &murmur_md[section_start..section_end];
    assert_eq!(
        section,
        "## Instruction Files\n\n- In the system prompt: AGENTS.md\n- Other files with these \
         names in the work folder (not in the system prompt): src/api/AGENTS.md\n\n"
    );
    let system = &fixture.system_texts()[0];
    assert!(system.ends_with(&agents_block()), "{system}");
    assert!(!system.contains("NESTED-MARKER"), "{system}");
    assert!(!system.contains("GIT-MARKER"), "{system}");
}

/// Without the key nothing is read, the trace says `null`, and `MURMUR.md` has no section, even
/// with an `AGENTS.md` in the work folder.
#[test]
fn instruction_files_without_the_key_change_nothing() {
    let fixture = Fixture::new(vec![one_turn_response()]);
    fs::write(fixture.work().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    let manifest = fixture.write_manifest("", "  system_prompt: \"MANIFEST PROMPT\"\n");

    let (records, murmur_md) = fixture.launch(&manifest);

    let system = &fixture.system_texts()[0];
    assert!(system.ends_with("\n\nMANIFEST PROMPT"), "{system}");
    assert!(!system.contains("Instruction file"), "{system}");
    let start = session_start(&records);
    assert!(start.get("instruction_files").is_some(), "{start}");
    assert!(start["instruction_files"].is_null(), "{start}");
    assert!(!murmur_md.contains("Instruction Files"), "{murmur_md}");
}

// ── `mur run` ───────────────────────────────────────────────────────────────────

/// A project with the driver installed into its store, a work folder, and a scripted provider,
/// for a spawned `mur run`.
struct CliFixture {
    home: TempDir,
    artifacts: TempDir,
    project: TempDir,
    workdir: TempDir,
    server: ScriptedServer,
}

impl CliFixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            artifacts: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
            workdir: tempfile::tempdir().unwrap(),
            server: ScriptedServer::start(vec![one_turn_response()]),
        }
    }

    /// Writes the manifest first, because `mur install` finds the project by it.
    fn write_manifest(&self, inference_extra: &str) -> PathBuf {
        let path = write_manifest(
            self.project.path(),
            &self.server.endpoint,
            "",
            inference_extra,
        );
        let driver = common::create_driver_artifact(
            self.artifacts.path(),
            DRIVER_ANTHROPIC_NAME,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::install_artifact_to_project(self.project.path(), &driver).success();
        path
    }

    fn mur_run(&self, manifest: &Path, extra: &[&str]) -> assert_cmd::assert::Assert {
        let mut command = assert_cmd::Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .args(["run", "--manifest", manifest.to_str().unwrap()])
            .args(["--workdir", self.workdir.path().to_str().unwrap()])
            .args(["--task", "Say hello.", "--json"])
            .args(extra)
            .assert()
    }

    fn records_of(assert: &assert_cmd::assert::Assert) -> PathBuf {
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let line = stdout
            .lines()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
            .unwrap_or_else(|| panic!("no JSON launch line in stdout:\n{stdout}"));
        PathBuf::from(line["records_dir"].as_str().expect("records_dir"))
    }
}

/// An absolute or escaping entry refuses the launch at manifest parse, naming the field and the
/// entry, before any request is made.
#[test]
fn instruction_files_absolute_or_escaping_entry_refuses_the_launch() {
    for entry in ["/etc/passwd", "../AGENTS.md"] {
        let fixture = CliFixture::new();
        let manifest = fixture.write_manifest(&format!("  instruction_files: [\"{entry}\"]\n"));
        let assert = fixture.mur_run(&manifest, &[]).failure();
        let output = assert.get_output();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(text.contains("E-MAN-003"), "{entry}: {text}");
        assert!(
            text.contains("inference.instruction_files[0]"),
            "{entry}: {text}"
        );
        assert!(text.contains(&format!("'{entry}'")), "{entry}: {text}");
        assert!(
            fixture.server.requests().is_empty(),
            "{entry}: no inference request is made"
        );
    }
}

/// `--system-prompt` replaces the manifest's prompt and leaves `instruction_files` alone: the
/// blocks follow the CLI prompt.
#[test]
fn instruction_files_follow_the_cli_system_prompt() {
    let fixture = CliFixture::new();
    fs::write(fixture.workdir.path().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    let manifest = fixture
        .write_manifest("  system_prompt: \"MANIFEST PROMPT\"\n  instruction_files: [AGENTS.md]\n");

    let assert = fixture
        .mur_run(&manifest, &["--system-prompt", "CLI PROMPT"])
        .success();
    let records = CliFixture::records_of(&assert);

    let requests = fixture.server.requests();
    assert_eq!(requests.len(), 1);
    let system = common::system_text(&requests[0]["system"]).unwrap();
    assert!(
        system.ends_with(&format!("\n\nCLI PROMPT\n\n{}", agents_block())),
        "{system}"
    );
    assert!(!system.contains("MANIFEST PROMPT"), "{system}");
    let start = session_start(&records);
    assert_eq!(start["system_prompt_source"], "cli");
    assert_eq!(start["system_prompt_sha256"], sha256_hex_of("CLI PROMPT"));
    assert_eq!(start["instruction_files"][0]["status"], "read");
}

/// The operator's check: `mur run --workdir` with a listed `AGENTS.md` and a listed file that is
/// not there.
#[test]
fn instruction_files_reach_a_mur_run_workdir_session() {
    let fixture = CliFixture::new();
    fs::write(fixture.workdir.path().join("AGENTS.md"), AGENTS_TEXT).unwrap();
    let manifest = fixture.write_manifest("  instruction_files: [AGENTS.md, CLAUDE.md]\n");

    let assert = fixture.mur_run(&manifest, &[]).success();
    let records = CliFixture::records_of(&assert);

    let system = common::system_text(&fixture.server.requests()[0]["system"]).unwrap();
    assert!(
        system.ends_with(&format!("\n\n{}", agents_block())),
        "{system}"
    );
    let start = session_start(&records);
    assert_eq!(start["instruction_files"][0]["status"], "read");
    assert_eq!(
        start["instruction_files"][1],
        json!({"path": "CLAUDE.md", "status": "missing"})
    );
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
        one_turn_response(),
    ]
}
