//! `mur eval run --variant/--repeat` with a case's `start` and `check`, and `mur eval diff --run`,
//! driven through the real `mur` against scripted providers.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
    fs,
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

use common::ScriptedServer;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const SHELL_ALLOW: &[&str] = &["bash", "grep", "cat", "ls", "env", "sleep", "ln", "true"];

// ── Fixture ───────────────────────────────────────────────────────────────────

/// One test's world: its own `HOME` with the fixture driver published, a workspace the dataset's
/// relative paths resolve against, and a baseline and a variant capsule directory beneath it.
struct World {
    home: TempDir,
    workspace: TempDir,
}

impl World {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let driver = common::create_driver_artifact(
            artifacts.path(),
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        common::publish_local(&home, &driver).success();
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("task.md"), "Fix the bug in main.txt.").unwrap();
        Self { home, workspace }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.workspace.path().join(rel)
    }

    /// Writes `<rel>/murmur.yaml` for an agent capsule on `endpoint`.
    fn capsule(&self, rel: &str, endpoint: &str, model: &str, shell_allow: &[&str]) -> PathBuf {
        self.capsule_with_driver(rel, endpoint, model, shell_allow, DRIVER)
    }

    fn capsule_with_driver(
        &self,
        rel: &str,
        endpoint: &str,
        model: &str,
        shell_allow: &[&str],
        driver: &str,
    ) -> PathBuf {
        let dir = self.path(rel);
        fs::create_dir_all(&dir).unwrap();
        let allow: String = shell_allow
            .iter()
            .map(|binary| format!("      - {binary}\n"))
            .collect();
        fs::write(
            dir.join("murmur.yaml"),
            format!(
                "name: {rel}\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      api_key: test-key\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n{allow}inference:\n  transport: http\n  model: {model}\n  driver:\n    artifact: {driver}\n"
            ),
        )
        .unwrap();
        dir
    }

    fn dataset(&self, lines: &[Value]) -> PathBuf {
        let path = self.path("eval.jsonl");
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        fs::write(&path, body).unwrap();
        path
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command
            .env("HOME", self.home.path())
            .current_dir(self.workspace.path());
        command
    }

    fn group(&self, capsule: &str) -> PathBuf {
        common::records_group(self.home.path(), &self.path(capsule).join("workdir"))
    }

    fn run_records(&self, capsule: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.group(capsule)) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("evr_") && name.ends_with(".json"))
            })
            .collect();
        found.sort();
        found
    }

    fn sessions(&self, capsule: &str) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.group(capsule)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ses_"))
            })
            .collect()
    }
}

fn run_ok(command: &mut Command) -> String {
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "mur failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    stdout
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

/// The documented start digest, computed independently of `mur`.
fn start_digest(root: &Path) -> String {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<(Vec<u8>, bool, u32)>) {
        for entry in fs::read_dir(root.join(rel)).unwrap() {
            let entry = entry.unwrap();
            let child = rel.join(entry.file_name());
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            let is_dir = meta.is_dir();
            out.push((
                child.as_os_str().as_bytes().to_vec(),
                is_dir,
                meta.permissions().mode() & 0o7777,
            ));
            if is_dir {
                walk(root, &child, out);
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, Path::new(""), &mut entries);
    entries.sort();
    let mut hasher = Sha256::new();
    for (rel, is_dir, mode) in entries {
        hasher.update(format!("{} {mode:o} ", if is_dir { "d" } else { "f" }).as_bytes());
        hasher.update(&rel);
        hasher.update(b"\0");
        if !is_dir {
            let bytes = fs::read(root.join(std::ffi::OsStr::from_bytes(&rel))).unwrap();
            hasher.update(format!("{}\0", bytes.len()).as_bytes());
            hasher.update(&bytes);
        }
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A buggy start: `main.txt` reads `bug`.
fn buggy_start(world: &World) -> PathBuf {
    let start = world.path("start");
    fs::create_dir_all(start.join("src")).unwrap();
    fs::write(start.join("main.txt"), "bug\n").unwrap();
    fs::write(start.join("src/notes.txt"), "keep me\n").unwrap();
    start
}

fn usage(input: u64, cache_read: u64) -> Value {
    json!({
        "input_tokens": input,
        "output_tokens": 5,
        "cache_read_input_tokens": cache_read,
        "cache_creation_input_tokens": 0
    })
}

fn tool_use(tool_id: &str, binary: &str, command: &str, usage: Value) -> String {
    json!({
        "id": format!("msg_{tool_id}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": tool_id, "name": binary, "input": {"command": command}}],
        "stop_reason": "tool_use",
        "stop_sequence": Value::Null,
        "usage": usage
    })
    .to_string()
}

fn end_turn(text: &str, usage: Value) -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": Value::Null,
        "usage": usage
    })
    .to_string()
}

/// The two responses of a model that fixes the bug: a `bash` call writing `fixed`, then done.
fn fixing(tool_id: &str) -> Vec<String> {
    vec![
        tool_use(tool_id, "bash", "echo fixed > main.txt", usage(10, 0)),
        end_turn("Fixed.", usage(10, 0)),
    ]
}

fn giving_up() -> Vec<String> {
    vec![end_turn("I could not find the bug.", usage(10, 0))]
}

fn runs(record: &Value) -> Vec<&Value> {
    record["cases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|case| case["runs"].as_array().unwrap())
        .collect()
}

fn arm_runs<'a>(record: &'a Value, case: usize, arm: &str) -> Vec<&'a Value> {
    record["cases"][case]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|run| run["arm"] == arm)
        .collect()
}

/// The row labelled `label` in `stdout`'s block for `case`.
fn row<'a>(stdout: &'a str, case: &str, label: &str) -> &'a str {
    let heading = format!("── case: {case} ");
    let start = stdout
        .find(&heading)
        .unwrap_or_else(|| panic!("no block for {case} in\n{stdout}"));
    let block = &stdout[start + heading.len()..];
    let block = &block[..block.find("\n── ").unwrap_or(block.len())];
    block
        .lines()
        .find(|line| is_row(line, label))
        .unwrap_or_else(|| panic!("no {label} row for {case} in\n{stdout}"))
}

/// Whether `line` is the row labelled `label`: the label, then the column padding, so `spend`
/// never matches `spend per pass`.
fn is_row(line: &str, label: &str) -> bool {
    line.trim_start()
        .strip_prefix(label)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with("  "))
}

fn total_block(stdout: &str) -> &str {
    let block = stdout.rsplit("── total").next().unwrap();
    &block[..block.find("\n── ").unwrap_or(block.len())]
}

fn total_row<'a>(stdout: &'a str, label: &str) -> &'a str {
    total_block(stdout)
        .lines()
        .find(|line| is_row(line, label))
        .unwrap_or_else(|| panic!("no total {label} row in\n{stdout}"))
}

// ── S1: the happy path ────────────────────────────────────────────────────────

#[test]
fn a_variant_that_fixes_the_bug_passes_its_checks_and_the_baseline_does_not() {
    if common::skip_without_host_support("a_variant_that_fixes_the_bug_passes_its_checks") {
        return;
    }
    let world = World::new();
    let baseline_server = ScriptedServer::start([giving_up(), giving_up()].concat());
    let variant_server = ScriptedServer::start([fixing("toolu_a"), fixing("toolu_b")].concat());
    world.capsule("base", &baseline_server.endpoint, "test-model", SHELL_ALLOW);
    world.capsule(
        "variant",
        &variant_server.endpoint,
        "test-model",
        SHELL_ALLOW,
    );
    buggy_start(&world);
    world.dataset(&[
        json!({"case_id": "case_a", "task_path": "task.md", "start": "start", "check": "grep -q fixed main.txt"}),
        json!({"case_id": "case_b", "task_path": "task.md", "start": "start",
               "check": {"command": "grep -q fixed main.txt", "timeout_secs": 30}}),
    ]);

    let stdout = run_ok(world.mur().args([
        "eval",
        "run",
        "base",
        "--dataset",
        "eval.jsonl",
        "--variant",
        "./variant",
    ]));

    let records = world.run_records("base");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(mode(&records[0]), 0o600);
    let record = read_json(&records[0]);
    assert_eq!(record["complete"], true);
    assert_eq!(
        record["dataset"],
        world.path("eval.jsonl").display().to_string()
    );
    assert_eq!(record["record_version"], 1);
    assert_eq!(record["arms"][0]["name"], "baseline");
    assert_eq!(record["arms"][1]["name"], "variant");
    for case in 0..2 {
        let baseline = arm_runs(&record, case, "baseline");
        let variant = arm_runs(&record, case, "variant");
        assert_eq!(baseline[0]["check_result"], "fail", "{record:#}");
        assert_eq!(variant[0]["check_result"], "pass", "{record:#}");
        assert_eq!(baseline[0]["outcome"], "completed");
        assert_eq!(variant[0]["check"]["exit_code"], 0);
        assert!(variant[0]["turns"].as_u64().unwrap() >= 2);
        assert!(variant[0]["input_tokens"].as_u64().is_some());
        let log = PathBuf::from(baseline[0]["check"]["output_log"].as_str().unwrap());
        assert_eq!(mode(&log), 0o600);
    }

    let run_id = record["run_id"].as_str().unwrap();
    assert!(
        stdout.trim_end().ends_with(&format!(
            "mur eval diff --run {run_id} --workdir {}",
            world.path("base/workdir").display()
        )),
        "{stdout}"
    );
    assert!(stdout.contains(&format!("run: {run_id}")));
    assert!(stdout.contains("── Summary"));

    let diff = run_ok(world.mur().args([
        "eval",
        "diff",
        "--run",
        "@1",
        "--workdir",
        world.path("base/workdir").to_str().unwrap(),
    ]));
    for case in ["case_a", "case_b"] {
        let check = row(&diff, case, "check");
        assert!(check.contains("0/1 pass"), "{check}");
        assert!(check.contains("1/1 pass"), "{check}");
        assert!(check.trim_end().ends_with("+1"), "{check}");
        for label in [
            "turns",
            "input tokens",
            "cached tokens",
            "cache write tokens",
            "output tokens",
            "time",
        ] {
            row(&diff, case, label);
        }
    }
    assert_eq!(diff.matches("mur trace diff ").count(), 2, "{diff}");
    let total = total_row(&diff, "check");
    assert!(
        total.contains("0/2 pass") && total.contains("2/2 pass"),
        "{total}"
    );
    // `mur eval run` ends with the same table.
    assert!(stdout.contains(diff.trim_end()), "{stdout}\n---\n{diff}");

    let json_out = run_ok(world.mur().args([
        "eval",
        "diff",
        "--run",
        "--json",
        "--workdir",
        world.path("base/workdir").to_str().unwrap(),
    ]));
    let comparison: Value = serde_json::from_str(&json_out).unwrap();
    assert_eq!(comparison["run_id"], run_id);
    for case in comparison["cases"].as_array().unwrap() {
        assert_eq!(case["arms"]["baseline"]["check_pass"], 0);
        assert_eq!(case["arms"]["variant"]["check_pass"], 1);
        assert_eq!(case["trace_diffs"].as_array().unwrap().len(), 1);
    }
    assert_eq!(comparison["total"]["variant"]["check_pass"], 2);
}

// ── S2: byte-identical starts ─────────────────────────────────────────────────

#[test]
fn every_run_starts_from_a_byte_identical_copy_of_the_start() {
    if common::skip_without_host_support("every_run_starts_from_a_byte_identical_copy") {
        return;
    }
    let world = World::new();
    let editing = |id: &str| {
        vec![
            tool_use(
                id,
                "bash",
                "echo changed > main.txt; echo new > added.txt; echo more >> src/notes.txt",
                usage(10, 0),
            ),
            end_turn("Edited.", usage(10, 0)),
        ]
    };
    let listing = |id: &str| {
        vec![
            tool_use(
                id,
                "bash",
                "ls -la . src; cat main.txt src/notes.txt",
                usage(10, 0),
            ),
            end_turn("Listed.", usage(10, 0)),
        ]
    };
    let baseline_server =
        ScriptedServer::start([editing("toolu_e0"), editing("toolu_e1")].concat());
    let variant_server = ScriptedServer::start([listing("toolu_l0"), listing("toolu_l1")].concat());
    world.capsule("base", &baseline_server.endpoint, "test-model", SHELL_ALLOW);
    world.capsule(
        "variant",
        &variant_server.endpoint,
        "test-model",
        SHELL_ALLOW,
    );
    let start = buggy_start(&world);
    fs::set_permissions(
        start.join("src/notes.txt"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let source_digest = start_digest(&start);
    world.dataset(&[json!({"case_id": "c", "task_path": "task.md", "start": "start"})]);

    run_ok(world.mur().args([
        "eval",
        "run",
        "base",
        "--variant",
        "variant",
        "--repeat",
        "2",
    ]));

    let record = read_json(&world.run_records("base")[0]);
    assert_eq!(record["cases"][0]["start_sha256"], source_digest.as_str());
    let all = runs(&record);
    assert_eq!(all.len(), 4);
    for run in &all {
        assert_eq!(run["start_sha256"], source_digest.as_str(), "{run:#}");
        assert_eq!(run["outcome"], "completed");
        assert!(run["check_result"].is_null());
    }
    let order: Vec<(String, u64, u64)> = all
        .iter()
        .map(|run| {
            (
                run["arm"].as_str().unwrap().to_string(),
                run["repeat"].as_u64().unwrap(),
                run["order"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("baseline".to_string(), 0, 0),
            ("variant".to_string(), 0, 1),
            ("variant".to_string(), 1, 0),
            ("baseline".to_string(), 1, 1),
        ]
    );

    // Repeat 0 ran baseline first, which edited its own copy: the variant still saw the source.
    let seen = common::tool_result_text(&variant_server.requests(), "toolu_l0").unwrap();
    assert!(seen.contains("bug"), "{seen}");
    assert!(seen.contains("keep me"), "{seen}");
    assert!(
        !seen.contains("changed") && !seen.contains("added.txt"),
        "{seen}"
    );
    let edited = common::tool_result_text(&baseline_server.requests(), "toolu_e0").unwrap();
    assert!(!edited.to_lowercase().contains("error"), "{edited}");

    assert_eq!(start_digest(&start), source_digest);
    assert_eq!(fs::read_to_string(start.join("main.txt")).unwrap(), "bug\n");
}

// ── S3: the agent never sees the check ────────────────────────────────────────

const MARKER: &str = "CHKMARK7f3a9";
const MARKER_FILE: &str = "verdict-q81z.txt";

#[test]
fn the_agent_never_sees_the_check() {
    if common::skip_without_host_support("the_agent_never_sees_the_check") {
        return;
    }
    let world = World::new();
    let snooping = || {
        vec![
            tool_use(
                "toolu_snoop",
                "bash",
                "ls -laR .; env; ls -laR \"$MURMUR_RECORDS_DIR\" \"$MURMUR_RUNTIME_DIR\"; cat \"$MURMUR_RUNTIME_DIR\"/*.md",
                usage(10, 0),
            ),
            end_turn("Looked around.", usage(10, 0)),
        ]
    };
    let server = ScriptedServer::start([snooping(), snooping()].concat());
    world.capsule("base", &server.endpoint, "test-model", SHELL_ALLOW);
    buggy_start(&world);
    let files = world.path("check-files");
    fs::create_dir_all(&files).unwrap();
    fs::write(files.join(MARKER_FILE), format!("{MARKER}\n")).unwrap();

    world.dataset(&[
        json!({"case_id": "c", "task_path": "task.md", "start": "start",
        "check": {"command": format!("grep -q {MARKER} {MARKER_FILE}"), "files": "check-files"}}),
    ]);
    run_ok(world.mur().args(["eval", "run", "base"]));
    world.dataset(&[json!({"case_id": "c", "task_path": "task.md", "start": "start"})]);
    run_ok(world.mur().args(["eval", "run", "base"]));

    let records = world.run_records("base");
    assert_eq!(records.len(), 2);
    let checked = read_json(&records[0]);
    let unchecked = read_json(&records[1]);
    let checked_run = runs(&checked)[0].clone();
    let unchecked_run = runs(&unchecked)[0].clone();
    assert_eq!(checked_run["check_result"], "pass", "{checked_run:#}");

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests {
        let text = request.to_string();
        assert!(
            !text.contains(MARKER),
            "a request carried the marker: {text}"
        );
        assert!(
            !text.contains(MARKER_FILE),
            "a request named the file: {text}"
        );
    }
    // The snooping call did run and list the folder.
    let listing = common::tool_result_text(&requests[..2], "toolu_snoop").unwrap();
    assert!(listing.contains("main.txt"), "{listing}");

    let trace_of =
        |run: &Value| PathBuf::from(run["records_dir"].as_str().unwrap()).join("trace.jsonl");
    let checked_trace = fs::read_to_string(trace_of(&checked_run)).unwrap();
    assert!(!checked_trace.contains(MARKER));
    assert!(!checked_trace.contains(MARKER_FILE));
    let kinds = |path: PathBuf| -> BTreeSet<String> {
        common::read_whole_trace(&path)
            .iter()
            .map(|event| event["event_type"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        kinds(trace_of(&checked_run)),
        kinds(trace_of(&unchecked_run))
    );

    let workdir = PathBuf::from(checked_run["workdir"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(workdir.join(MARKER_FILE)).unwrap(),
        format!("{MARKER}\n")
    );
    let unchecked_workdir = PathBuf::from(unchecked_run["workdir"].as_str().unwrap());
    assert!(!unchecked_workdir.join(MARKER_FILE).exists());
}

// ── S4: the check is contained ────────────────────────────────────────────────

#[test]
fn the_check_runs_under_the_sessions_containment() {
    if common::skip_without_host_support("the_check_runs_under_the_sessions_containment") {
        return;
    }
    let world = World::new();
    let secret_dir = TempDir::new().unwrap();
    let secret = secret_dir.path().join("secret.txt");
    fs::write(&secret, "s3cr3t-value-91\n").unwrap();
    let host = std::process::Command::new("cat")
        .arg(&secret)
        .output()
        .unwrap();
    assert!(host.status.success());
    assert!(String::from_utf8_lossy(&host.stdout).contains("s3cr3t-value-91"));

    let server = ScriptedServer::start(giving_up());
    world.capsule("base", &server.endpoint, "test-model", SHELL_ALLOW);
    world.dataset(&[json!({"case_id": "c", "task_path": "task.md",
        "check": format!("cat {}", secret.display())})]);
    run_ok(world.mur().args(["eval", "run", "base"]));

    let record = read_json(&world.run_records("base")[0]);
    let run = runs(&record)[0].clone();
    assert_eq!(run["check_result"], "fail", "{run:#}");
    let tail = run["check"]["output_tail"].as_str().unwrap();
    assert!(!tail.contains("s3cr3t-value-91"), "{tail}");
    assert!(
        tail.contains("Permission denied") || tail.contains("No such file"),
        "{tail}"
    );
    let trace = common::read_whole_trace(
        &PathBuf::from(run["records_dir"].as_str().unwrap()).join("trace.jsonl"),
    );
    let start = trace
        .iter()
        .find(|event| event["event_type"] == "session_start")
        .unwrap();
    assert_eq!(run["check"]["containment"], start["containment_achieved"]);
}

// ── S5: refusals ──────────────────────────────────────────────────────────────

/// A world whose preflight refuses: both servers would answer, and must see nothing.
struct Refusal {
    world: World,
    baseline_server: ScriptedServer,
    variant_server: ScriptedServer,
}

impl Refusal {
    fn new() -> Self {
        let world = World::new();
        let baseline_server = ScriptedServer::start(giving_up());
        let variant_server = ScriptedServer::start(giving_up());
        world.capsule("base", &baseline_server.endpoint, "test-model", SHELL_ALLOW);
        world.capsule(
            "variant",
            &variant_server.endpoint,
            "test-model",
            SHELL_ALLOW,
        );
        buggy_start(&world);
        fs::create_dir_all(world.path("check-files")).unwrap();
        Self {
            world,
            baseline_server,
            variant_server,
        }
    }

    /// Runs `args`, expects a refusal whose stderr contains every one of `needles`, and nothing
    /// staged, launched or recorded.
    fn refuses(&self, args: &[&str], needles: &[&str]) {
        let output = self.world.mur().args(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} succeeded: {stderr}");
        for needle in needles {
            assert!(
                stderr.contains(needle),
                "{args:?}: no {needle:?} in {stderr}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
        assert!(self.baseline_server.requests().is_empty());
        assert!(self.variant_server.requests().is_empty());
        assert!(self.world.run_records("base").is_empty());
        assert!(self.world.sessions("base").is_empty());
        assert!(self.world.sessions("variant").is_empty());
        assert!(!self.world.path("base/workdir").exists());
        assert!(!self.world.path("variant/workdir").exists());
    }
}

const VARIANT_RUN: &[&str] = &["eval", "run", "base", "--variant", "variant"];

#[test]
fn a_check_binary_missing_from_the_variants_allowlist_is_refused() {
    let refusal = Refusal::new();
    refusal.world.capsule(
        "variant",
        &refusal.variant_server.endpoint,
        "test-model",
        &["bash"],
    );
    refusal
        .world
        .dataset(&[json!({"case_id": "case_g", "task_path": "task.md",
        "check": "grep -q fixed main.txt"})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-004", "case_g", "variant", "grep"]);
}

#[test]
fn a_missing_start_is_refused() {
    let refusal = Refusal::new();
    refusal
        .world
        .dataset(&[json!({"case_id": "case_s", "task_path": "task.md",
        "start": "no-such-start"})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-003", "case_s", "no-such-start"]);
}

#[test]
fn a_link_in_the_start_is_refused() {
    let refusal = Refusal::new();
    std::os::unix::fs::symlink("/etc/hostname", refusal.world.path("start/src/link")).unwrap();
    refusal
        .world
        .dataset(&[json!({"case_id": "case_l", "task_path": "task.md",
        "start": "start"})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-003", "case_l", "link"]);
}

#[test]
fn missing_check_files_are_refused() {
    let refusal = Refusal::new();
    refusal
        .world
        .dataset(&[json!({"case_id": "case_f", "task_path": "task.md",
        "check": {"command": "true", "files": "no-such-files"}})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-003", "case_f", "no-such-files"]);
}

#[test]
fn an_empty_check_command_is_refused() {
    let refusal = Refusal::new();
    refusal
        .world
        .dataset(&[json!({"case_id": "case_e", "task_path": "task.md",
        "check": {"command": "  "}})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-003", "case_e", "check.command"]);
}

#[test]
fn a_zero_timeout_is_refused() {
    let refusal = Refusal::new();
    refusal
        .world
        .dataset(&[json!({"case_id": "case_z", "task_path": "task.md",
        "check": {"command": "true", "timeout_secs": 0}})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-003", "case_z", "timeout_secs"]);
}

#[test]
fn a_zero_repeat_is_a_usage_error() {
    let refusal = Refusal::new();
    refusal
        .world
        .dataset(&[json!({"case_id": "c", "task_path": "task.md"})]);
    refusal.refuses(
        &[
            "eval",
            "run",
            "base",
            "--variant",
            "variant",
            "--repeat",
            "0",
        ],
        &["--repeat"],
    );
}

#[test]
fn differing_models_are_refused() {
    let refusal = Refusal::new();
    refusal.world.capsule(
        "variant",
        &refusal.variant_server.endpoint,
        "other-model",
        SHELL_ALLOW,
    );
    refusal
        .world
        .dataset(&[json!({"case_id": "c", "task_path": "task.md"})]);
    refusal.refuses(VARIANT_RUN, &["E-EVAL-004", "test-model", "other-model"]);
}

#[test]
fn a_variant_without_a_manifest_is_refused() {
    let refusal = Refusal::new();
    fs::create_dir_all(refusal.world.path("empty")).unwrap();
    refusal
        .world
        .dataset(&[json!({"case_id": "c", "task_path": "task.md"})]);
    refusal.refuses(
        &["eval", "run", "base", "--variant", "empty"],
        &["E-IO-001", "murmur.yaml not found"],
    );
}

// ── S6: a check that outruns its timeout ──────────────────────────────────────

/// Every live `sleep 60` whose current directory is beneath `root`.
fn sleeps_under(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let pid = entry.file_name().to_string_lossy().to_string();
        if !pid.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline != b"sleep\x0060\x00" {
            continue;
        }
        if let Ok(cwd) = fs::read_link(entry.path().join("cwd")) {
            if cwd.starts_with(root) {
                found.push(pid);
            }
        }
    }
    found
}

#[test]
fn a_check_that_outruns_its_timeout_is_killed() {
    if common::skip_without_host_support("a_check_that_outruns_its_timeout_is_killed") {
        return;
    }
    let world = World::new();
    let server = ScriptedServer::start([giving_up(), giving_up()].concat());
    world.capsule("base", &server.endpoint, "test-model", SHELL_ALLOW);
    let check = json!({"command": "sleep 60", "timeout_secs": 2});
    world.dataset(&[
        json!({"case_id": "slow_a", "task_path": "task.md", "check": check}),
        json!({"case_id": "slow_b", "task_path": "task.md", "check": check}),
    ]);

    let started = Instant::now();
    run_ok(world.mur().args(["eval", "run", "base"]));
    assert!(
        started.elapsed() < Duration::from_secs(100),
        "{:?}",
        started.elapsed()
    );

    let record = read_json(&world.run_records("base")[0]);
    for run in runs(&record) {
        assert_eq!(run["check_result"], "timed_out", "{run:#}");
        let duration = run["check"]["duration_ms"].as_u64().unwrap();
        assert!((2000..=10000).contains(&duration), "{duration}");
        assert!(run["check"]["exit_code"].is_null());
    }
    assert!(sleeps_under(world.workspace.path()).is_empty());
}

// ── S7: a link the agent planted ──────────────────────────────────────────────

#[test]
fn check_files_are_not_written_through_a_link_the_agent_planted() {
    if common::skip_without_host_support("check_files_are_not_written_through_a_link") {
        return;
    }
    let world = World::new();
    let outside = TempDir::new().unwrap();
    let server = ScriptedServer::start(vec![
        tool_use(
            "toolu_ln",
            "ln",
            &format!("-s {} hidden", outside.path().display()),
            usage(10, 0),
        ),
        end_turn("Linked.", usage(10, 0)),
    ]);
    world.capsule("base", &server.endpoint, "test-model", SHELL_ALLOW);
    let files = world.path("check-files");
    fs::create_dir_all(files.join("hidden")).unwrap();
    fs::write(files.join("hidden/check.txt"), "x\n").unwrap();
    world.dataset(&[json!({"case_id": "c", "task_path": "task.md",
        "check": {"command": "cat hidden/check.txt", "files": "check-files"}})]);

    run_ok(world.mur().args(["eval", "run", "base"]));

    let record = read_json(&world.run_records("base")[0]);
    let run = runs(&record)[0].clone();
    let workdir = PathBuf::from(run["workdir"].as_str().unwrap());
    assert!(
        fs::symlink_metadata(workdir.join("hidden"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the scripted model's link was not made: {run:#}"
    );
    assert_eq!(run["check_result"], "error", "{run:#}");
    assert!(
        run["check"]["error"].as_str().unwrap().contains("hidden"),
        "{run:#}"
    );
    assert!(run["check"]["exit_code"].is_null());
    assert!(run["check"]["output_log"].is_null());
    assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
}

// ── S8: the spread over repeats ───────────────────────────────────────────────

#[test]
fn repeats_report_the_median_and_the_spread() {
    if common::skip_without_host_support("repeats_report_the_median_and_the_spread") {
        return;
    }
    let world = World::new();
    let baseline = [
        vec![end_turn("Nothing.", usage(100, 0))],
        vec![
            tool_use("toolu_r1", "bash", "echo fixed > main.txt", usage(150, 25)),
            end_turn("Fixed.", usage(150, 25)),
        ],
        vec![end_turn("Nothing.", usage(200, 0))],
    ]
    .concat();
    let variant = [fixing("toolu_v0"), fixing("toolu_v1"), fixing("toolu_v2")].concat();
    let baseline_server = ScriptedServer::start(baseline);
    let variant_server = ScriptedServer::start(variant);
    world.capsule("base", &baseline_server.endpoint, "test-model", SHELL_ALLOW);
    world.capsule(
        "variant",
        &variant_server.endpoint,
        "test-model",
        SHELL_ALLOW,
    );
    buggy_start(&world);
    world.dataset(&[
        json!({"case_id": "spread", "task_path": "task.md", "start": "start",
        "check": "grep -q fixed main.txt"}),
    ]);

    let stdout = run_ok(world.mur().args([
        "eval",
        "run",
        "base",
        "--variant",
        "variant",
        "--repeat",
        "3",
    ]));

    let record = read_json(&world.run_records("base")[0]);
    let inputs: Vec<u64> = arm_runs(&record, 0, "baseline")
        .iter()
        .map(|run| run["input_tokens"].as_u64().unwrap())
        .collect();
    assert_eq!(inputs, [100, 300, 200], "{record:#}");
    let cached: Vec<Value> = arm_runs(&record, 0, "baseline")
        .iter()
        .map(|run| run["cached_tokens"].clone())
        .collect();
    assert_eq!(cached, [json!(0), json!(50), json!(0)], "{record:#}");
    // Each call is charged its request (input + cache read + cache write) and its 5 output
    // tokens: 100 + 5; two calls of 150 + 25 + 5; 200 + 5; and two calls of 10 + 5 per variant run.
    let spend = |arm: &str| -> Vec<u64> {
        arm_runs(&record, 0, arm)
            .iter()
            .map(|run| run["spend_tokens"].as_u64().unwrap())
            .collect()
    };
    assert_eq!(spend("baseline"), [105, 360, 205], "{record:#}");
    assert_eq!(spend("variant"), [30, 30, 30], "{record:#}");

    let input = row(&stdout, "spread", "input tokens");
    assert!(input.contains("200 (100–300)"), "{input}");
    let variant_input = spread_cell_after(input, "200 (100–300)");
    assert!(variant_input.starts_with("20"), "{input}");
    assert!(input.trim_end().ends_with("-180"), "{input}");
    let check = row(&stdout, "spread", "check");
    assert!(
        check.contains("1/3 pass") && check.contains("3/3 pass"),
        "{check}"
    );
    assert!(check.trim_end().ends_with("+2"), "{check}");
    // The total sums every run: 100 + 300 + 200 against three runs of 20.
    let total_input = total_row(&stdout, "input tokens");
    assert!(total_input.contains("600"), "{total_input}");
    assert!(total_input.trim_end().ends_with("-540"), "{total_input}");

    // One baseline pass for 670 tokens against three variant passes for 90.
    let per_pass = row(&stdout, "spread", "spend per pass");
    assert_eq!(
        cells(per_pass, "spend per pass"),
        ["670", "30", "-640"],
        "{stdout}"
    );
    let spend_row = row(&stdout, "spread", "spend");
    assert!(spend_row.contains("205 (105–360)"), "{spend_row}");
    assert!(
        spread_cell_after(spend_row, "205 (105–360)").starts_with("30"),
        "{spend_row}"
    );
    assert!(spend_row.trim_end().ends_with("-175"), "{spend_row}");
    assert_eq!(
        cells(total_row(&stdout, "spend"), "spend"),
        ["670", "90", "-580"]
    );
    assert_eq!(
        cells(total_row(&stdout, "spend per pass"), "spend per pass"),
        ["670", "30", "-640"]
    );
    let lines: Vec<&str> = total_block(&stdout).lines().collect();
    let at = |label: &str| lines.iter().position(|line| is_row(line, label)).unwrap();
    assert!(at("check") + 1 == at("spend per pass") && at("spend per pass") + 1 == at("spend"));
    assert!(at("spend") + 1 == at("turns"), "{stdout}");

    let json_out = run_ok(world.mur().args([
        "eval",
        "diff",
        "--run",
        "--json",
        "--workdir",
        world.path("base/workdir").to_str().unwrap(),
    ]));
    let comparison: Value = serde_json::from_str(&json_out).unwrap();
    let baseline = &comparison["cases"][0]["arms"]["baseline"];
    assert_eq!(
        baseline["input_tokens"],
        json!({"median": 200.0, "min": 100, "max": 300})
    );
    assert_eq!(baseline["cached_tokens"]["max"], 50);
    assert_eq!(
        baseline["spend_tokens"],
        json!({"median": 205.0, "min": 105, "max": 360})
    );
    assert_eq!(baseline["spend_per_pass"], 670);
    assert_eq!(comparison["total"]["variant"]["spend_per_pass"], 30);
    assert_eq!(comparison["total"]["baseline"]["spend_tokens"], 670);
    assert!(comparison.get("requirements").is_none());
    assert_eq!(baseline["check_pass"], 1);
    assert_eq!(baseline["check_runs"], 3);
    assert_eq!(comparison["cases"][0]["arms"]["variant"]["check_pass"], 3);
    assert_eq!(
        comparison["cases"][0]["trace_diffs"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

/// A row's cells after its label: the columns are separated by two or more spaces.
fn cells<'a>(row: &'a str, label: &str) -> Vec<&'a str> {
    row.trim_start()
        .strip_prefix(label)
        .unwrap()
        .split("  ")
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect()
}

/// The text after `cell` in a table row, trimmed.
fn spread_cell_after<'a>(row: &'a str, cell: &str) -> &'a str {
    row.split_once(cell).unwrap().1.trim_start()
}

// ── S9: addressing an eval run ────────────────────────────────────────────────

fn synthetic_record(run_id: &str, dataset: &str) -> Value {
    json!({
        "record_version": 1,
        "run_id": run_id,
        "started_at_ms": 1,
        "ended_at_ms": 2,
        "complete": true,
        "dataset": dataset,
        "repeat": 1,
        "arms": [{"name": "baseline", "manifest": "/m/murmur.yaml", "manifest_sha256": "00", "model": "m"}],
        "cases": [{
            "case_id": "c", "task_path": "t.md", "start": null, "start_sha256": null, "check": null,
            "runs": [{
                "arm": "baseline", "repeat": 0, "order": 0, "session_id": "ses_x",
                "records_dir": "/r/ses_x", "workdir": "/w/ses_x", "start_sha256": null,
                "outcome": "completed", "exit_status": "ok", "check_result": null, "check": null,
                "turns": 1, "input_tokens": 3, "output_tokens": 4, "cached_tokens": null,
                "cache_write_tokens": null, "agent_ms": 10, "scores_overall": null
            }]
        }]
    })
}

#[test]
fn an_eval_run_is_addressed_like_a_session() {
    let world = World::new();
    let workdir = world.path("base/workdir");
    let group = world.group("base");
    let older = "evr_0192aaaa000070008000000000001111";
    let newer = "evr_0192aaaa000070008000000000002222";

    let empty = world
        .mur()
        .args([
            "eval",
            "diff",
            "--run",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("E-EVAL-002"));

    fs::create_dir_all(&group).unwrap();
    for (id, dataset) in [(older, "older.jsonl"), (newer, "newer.jsonl")] {
        fs::write(
            group.join(format!("{id}.json")),
            synthetic_record(id, dataset).to_string(),
        )
        .unwrap();
    }

    let diff = |run: Option<&str>| -> Vec<u8> {
        let mut args = vec!["eval", "diff", "--run"];
        args.extend(run);
        args.extend(["--workdir", workdir.to_str().unwrap()]);
        let output = world.mur().args(&args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    let latest = diff(None);
    assert!(String::from_utf8_lossy(&latest).contains("newer.jsonl"));
    assert_eq!(diff(Some("@1")), latest);
    assert_eq!(diff(Some("2222")), latest);
    assert_eq!(diff(Some(newer)), latest);
    let second = diff(Some("@2"));
    assert!(String::from_utf8_lossy(&second).contains("older.jsonl"));
    let path = group.join(format!("{older}.json"));
    assert_eq!(diff(Some(path.to_str().unwrap())), second);

    let out_of_range = world
        .mur()
        .args([
            "eval",
            "diff",
            "--run",
            "@9",
            "--workdir",
            workdir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out_of_range.status.success());
    assert!(String::from_utf8_lossy(&out_of_range.stderr).contains("E-EVAL-002"));

    let conflicting = world
        .mur()
        .args(["eval", "diff", "--run", "@1", "a", "b"])
        .output()
        .unwrap();
    assert!(!conflicting.status.success());
    assert!(
        String::from_utf8_lossy(&conflicting.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&conflicting.stderr)
    );
}

// ── Spend per pass and --require ──────────────────────────────────────────────

/// One run of a synthetic record: its arm, repeat, outcome, check result and spend. A run never
/// launched carries no session and no measures.
fn synthetic_run(arm: &str, repeat: u32, outcome: &str, check: &str, spend: Option<u64>) -> Value {
    let launched = outcome == "completed" || outcome == "launch_failed";
    json!({
        "arm": arm, "repeat": repeat, "order": 0,
        "session_id": launched.then(|| format!("ses_{arm}{repeat}")),
        "records_dir": launched.then(|| format!("/r/ses_{arm}{repeat}")),
        "workdir": null, "start_sha256": null, "outcome": outcome,
        "exit_status": launched.then_some("ok"), "check_result": check, "check": null,
        "turns": launched.then_some(2), "input_tokens": launched.then_some(10),
        "output_tokens": launched.then_some(5), "cached_tokens": null, "cache_write_tokens": null,
        "agent_ms": launched.then_some(1000), "spend_tokens": spend, "scores_overall": null
    })
}

/// A record of one checked case holding `runs`, with a variant arm when `paired`.
fn checked_record(run_id: &str, paired: bool, complete: bool, runs: Vec<Value>) -> Value {
    let mut arms = vec![
        json!({"name": "baseline", "manifest": "/m/base/murmur.yaml", "manifest_sha256": "00", "model": "m"}),
    ];
    if paired {
        arms.push(json!({"name": "variant", "manifest": "/m/variant/murmur.yaml", "manifest_sha256": "11", "model": "m"}));
    }
    json!({
        "record_version": 1, "run_id": run_id, "started_at_ms": 1, "ended_at_ms": 2,
        "complete": complete, "dataset": "/d/eval.jsonl", "repeat": 3, "arms": arms,
        "cases": [{
            "case_id": "gate", "task_path": "t.md", "start": null, "start_sha256": null,
            "check": {"command": "make test", "files": null, "timeout_secs": 600},
            "runs": runs
        }]
    })
}

/// A two-arm record: baseline pass/fail/fail for 1000/1200/800 tokens, variant pass/pass/pass for
/// 900/1100/1000.
fn gate_record(run_id: &str) -> Value {
    checked_record(
        run_id,
        true,
        true,
        vec![
            synthetic_run("baseline", 0, "completed", "pass", Some(1000)),
            synthetic_run("variant", 0, "completed", "pass", Some(900)),
            synthetic_run("variant", 1, "completed", "pass", Some(1100)),
            synthetic_run("baseline", 1, "completed", "fail", Some(1200)),
            synthetic_run("baseline", 2, "completed", "fail", Some(800)),
            synthetic_run("variant", 2, "completed", "pass", Some(1000)),
        ],
    )
}

struct Diff {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// `mur eval diff --run <record> <args>` on `record`, written to a scratch file.
fn diff_record(world: &World, record: &Value, args: &[&str]) -> Diff {
    let run_id = record["run_id"].as_str().unwrap();
    let path = world.path(&format!("{run_id}.json"));
    fs::write(&path, record.to_string()).unwrap();
    let mut command = world.mur();
    command.args(["eval", "diff", "--run", path.to_str().unwrap()]);
    command.args(args);
    let output = command.output().unwrap();
    Diff {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

/// The requirements block's line for `requirement`.
fn requirement_line<'a>(stdout: &'a str, requirement: &str) -> &'a str {
    let block = stdout
        .split("── requirements")
        .nth(1)
        .unwrap_or_else(|| panic!("no requirements block in\n{stdout}"));
    block
        .lines()
        .find(|line| line.split_whitespace().any(|word| word == requirement))
        .unwrap_or_else(|| panic!("no line for {requirement} in\n{stdout}"))
}

#[test]
fn spend_per_pass_and_requirements_read_from_the_record() {
    let world = World::new();
    let gate = gate_record("evr_0192aaaa000070008000000000003333");

    // The rows, and no threshold applies without --require.
    let plain = diff_record(&world, &gate, &[]);
    assert_eq!(plain.code, Some(0), "{}", plain.stderr);
    assert_eq!(
        cells(
            row(&plain.stdout, "gate", "spend per pass"),
            "spend per pass"
        ),
        ["3000", "1000", "-2000"]
    );
    assert_eq!(
        cells(row(&plain.stdout, "gate", "spend"), "spend"),
        ["1000 (800–1200)", "1000 (900–1100)", "+0"]
    );
    assert_eq!(
        cells(total_row(&plain.stdout, "spend per pass"), "spend per pass"),
        ["3000", "1000", "-2000"]
    );
    assert_eq!(
        cells(total_row(&plain.stdout, "spend"), "spend"),
        ["3000", "3000", "+0"]
    );
    assert!(!plain.stdout.contains("requirements"), "{}", plain.stdout);

    let met = diff_record(
        &world,
        &gate,
        &["--require", "passes>=3,spend-per-pass<=1000"],
    );
    assert_eq!(met.code, Some(0), "{}", met.stderr);
    assert!(met.stdout.starts_with(&plain.stdout), "{}", met.stdout);
    let passes = requirement_line(&met.stdout, "variant.passes>=3");
    assert!(
        passes.starts_with("  met ") && passes.ends_with("passes 3"),
        "{passes}"
    );
    let per_pass = requirement_line(&met.stdout, "variant.spend-per-pass<=1000");
    assert!(per_pass.starts_with("  met ") && per_pass.ends_with("spend per pass 1000"));

    let unmet = diff_record(&world, &gate, &["--require", "spend-per-pass<=999"]);
    assert_eq!(unmet.code, Some(1));
    let line = requirement_line(&unmet.stdout, "variant.spend-per-pass<=999");
    assert!(
        line.starts_with("  not met ") && line.ends_with("spend per pass 1000"),
        "{line}"
    );
    assert!(
        unmet.stderr.contains("error[E-EVAL-005]"),
        "{}",
        unmet.stderr
    );
    assert!(
        unmet
            .stderr
            .contains("1 of 1 requirements not met: variant.spend-per-pass<=999"),
        "{}",
        unmet.stderr
    );

    let lead = diff_record(&world, &gate, &["--require", "pass-lead>=4"]);
    assert_eq!(lead.code, Some(1));
    let line = requirement_line(&lead.stdout, "pass-lead>=4");
    assert!(
        line.starts_with("  not met ") && line.ends_with("pass lead 2"),
        "{line}"
    );
    let strict = diff_record(&world, &gate, &["--require", "pass-lead>2"]);
    assert_eq!(strict.code, Some(1), "{}", strict.stdout);
    let below = diff_record(
        &world,
        &gate,
        &["--require", "baseline.passes<2,pass-lead>1"],
    );
    assert_eq!(below.code, Some(0), "{}", below.stdout);

    // A requirement that does not parse is a usage error, and nothing reaches stdout.
    for malformed in [
        "cost<=5",
        "passes=~3",
        "passes>=-1",
        "variant.pass-lead>=1",
        "spend-per-pass<=abc",
        "other.passes>=1",
    ] {
        let refused = diff_record(&world, &gate, &["--require", malformed]);
        assert_eq!(refused.code, Some(2), "{malformed}: {}", refused.stderr);
        assert!(refused.stdout.is_empty(), "{malformed}: {}", refused.stdout);
        assert!(
            refused.stderr.contains(&format!(
                "invalid value '{malformed}' for '--require <REQ>'"
            )),
            "{malformed}: {}",
            refused.stderr
        );
    }
    let without_run = world
        .mur()
        .args(["eval", "diff", "--require", "passes>=1"])
        .output()
        .unwrap();
    assert_eq!(without_run.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&without_run.stderr).contains("--run"));

    // A one-arm record cannot answer pass-lead or variant., and the bare measure is the
    // baseline's.
    let single = checked_record(
        "evr_0192aaaa000070008000000000004444",
        false,
        true,
        vec![
            synthetic_run("baseline", 0, "completed", "pass", Some(400)),
            synthetic_run("baseline", 1, "completed", "fail", Some(600)),
            synthetic_run("baseline", 2, "completed", "pass", Some(500)),
        ],
    );
    for unanswerable in ["pass-lead>=0", "variant.passes>=1"] {
        let refused = diff_record(&world, &single, &["--require", unanswerable]);
        assert_eq!(refused.code, Some(1), "{unanswerable}");
        assert!(refused.stdout.is_empty(), "{}", refused.stdout);
        assert!(
            refused.stderr.contains("error[E-EVAL-006]"),
            "{}",
            refused.stderr
        );
        assert!(
            refused.stderr.contains(&format!(
                "--require {unanswerable}: run evr_0192aaaa000070008000000000004444 has one arm"
            )),
            "{}",
            refused.stderr
        );
    }
    let baseline = diff_record(&world, &single, &["--require", "passes>=1"]);
    assert_eq!(baseline.code, Some(0), "{}", baseline.stderr);
    assert_eq!(
        cells(
            row(&baseline.stdout, "gate", "spend per pass"),
            "spend per pass"
        ),
        ["750"]
    );
    assert!(requirement_line(&baseline.stdout, "baseline.passes>=1").ends_with("passes 2"));

    // No pass, no recorded spend, and a run never launched.
    let no_pass = checked_record(
        "evr_0192aaaa000070008000000000005555",
        true,
        true,
        vec![
            synthetic_run("baseline", 0, "completed", "pass", Some(100)),
            synthetic_run("variant", 0, "completed", "fail", Some(100)),
            synthetic_run("variant", 1, "launch_failed", "fail", Some(50)),
        ],
    );
    let zero = diff_record(&world, &no_pass, &["--require", "spend-per-pass<=1000000"]);
    assert_eq!(zero.code, Some(1));
    assert_eq!(
        cells(
            row(&zero.stdout, "gate", "spend per pass"),
            "spend per pass"
        ),
        ["100", "no pass", "—"]
    );
    assert!(
        requirement_line(&zero.stdout, "variant.spend-per-pass<=1000000")
            .ends_with("no strict pass")
    );

    let mut unkeyed = gate_record("evr_0192aaaa000070008000000000006666");
    for run in unkeyed["cases"][0]["runs"].as_array_mut().unwrap() {
        run.as_object_mut().unwrap().remove("spend_tokens");
    }
    let old = diff_record(&world, &unkeyed, &["--require", "spend-per-pass<=1000000"]);
    assert_eq!(old.code, Some(1));
    assert_eq!(
        cells(row(&old.stdout, "gate", "spend per pass"), "spend per pass"),
        ["—", "—", "—"]
    );
    assert_eq!(
        cells(row(&old.stdout, "gate", "spend"), "spend"),
        ["—", "—", "—"]
    );
    assert_eq!(
        cells(total_row(&old.stdout, "spend"), "spend"),
        ["—", "—", "—"]
    );
    assert!(
        requirement_line(&old.stdout, "variant.spend-per-pass<=1000000")
            .ends_with("spend not recorded")
    );
    // Every other row reads as the same record with spend did.
    let without_spend = |stdout: &str| -> String {
        stdout
            .lines()
            .filter(|line| !is_row(line, "spend") && !is_row(line, "spend per pass"))
            .take_while(|line| !line.starts_with("── requirements"))
            .map(|line| format!("{line}\n"))
            .collect::<String>()
            .trim_end()
            .replace("6666", "3333")
    };
    assert_eq!(without_spend(&old.stdout), without_spend(&plain.stdout));

    let staged = checked_record(
        "evr_0192aaaa000070008000000000007777",
        true,
        true,
        vec![
            synthetic_run("baseline", 0, "completed", "pass", Some(700)),
            synthetic_run("variant", 0, "stage_failed", "not_run", None),
            synthetic_run("variant", 1, "completed", "pass", Some(300)),
        ],
    );
    let counted = diff_record(&world, &staged, &["--require", "spend-per-pass<=300"]);
    assert_eq!(counted.code, Some(0), "{}", counted.stdout);
    assert_eq!(
        cells(
            row(&counted.stdout, "gate", "spend per pass"),
            "spend per pass"
        ),
        ["700", "300", "-400"]
    );

    // An incomplete record never meets its requirements.
    let mut partial = gate_record("evr_0192aaaa000070008000000000008888");
    partial["complete"] = json!(false);
    partial["cases"][0]["runs"]
        .as_array_mut()
        .unwrap()
        .truncate(4);
    let incomplete = diff_record(&world, &partial, &["--require", "passes>=1"]);
    assert_eq!(incomplete.code, Some(1));
    assert!(
        incomplete
            .stdout
            .trim_end()
            .ends_with("  not met  run is incomplete: 4 of 6 runs recorded"),
        "{}",
        incomplete.stdout
    );
    assert!(incomplete.stderr.contains("error[E-EVAL-005]"));
    assert!(incomplete
        .stderr
        .contains("run evr_0192aaaa000070008000000000008888 is incomplete: 4 of 6 runs recorded"));

    // The JSON gains the spend keys always and the requirements only with --require.
    let json_plain = diff_record(&world, &gate, &["--json"]);
    assert_eq!(json_plain.code, Some(0));
    let comparison: Value = serde_json::from_str(&json_plain.stdout).unwrap();
    let variant = &comparison["cases"][0]["arms"]["variant"];
    for key in [
        "runs",
        "check_pass",
        "check_runs",
        "check_not_run",
        "turns",
        "input_tokens",
        "cached_tokens",
        "cache_write_tokens",
        "output_tokens",
        "agent_ms",
        "scores_pass",
        "scores_runs",
    ] {
        assert!(variant.get(key).is_some(), "{key}: {variant}");
    }
    assert_eq!(
        variant["spend_tokens"],
        json!({"median": 1000.0, "min": 900, "max": 1100})
    );
    assert_eq!(variant["spend_per_pass"], 1000);
    assert_eq!(comparison["total"]["baseline"]["spend_tokens"], 3000);
    assert_eq!(comparison["total"]["baseline"]["spend_per_pass"], 3000);
    assert!(comparison.get("requirements").is_none());
    assert!(comparison.get("requirements_met").is_none());

    let json_unmet = diff_record(
        &world,
        &gate,
        &["--json", "--require", "spend-per-pass<=999"],
    );
    assert_eq!(json_unmet.code, Some(1));
    assert!(json_unmet.stderr.contains("error[E-EVAL-005]"));
    let comparison: Value = serde_json::from_str(&json_unmet.stdout).unwrap();
    assert_eq!(comparison["requirements_met"], false);
    assert_eq!(
        comparison["requirements"],
        json!([{
            "requirement": "variant.spend-per-pass<=999", "arm": "variant",
            "measure": "spend-per-pass", "op": "<=", "threshold": 999, "actual": 1000,
            "met": false, "reason": null
        }])
    );
    let json_met = diff_record(&world, &gate, &["--json", "--require", "pass-lead>=2"]);
    assert_eq!(json_met.code, Some(0));
    let comparison: Value = serde_json::from_str(&json_met.stdout).unwrap();
    assert_eq!(comparison["requirements_met"], true);
    assert_eq!(comparison["requirements"][0]["arm"], Value::Null);

    // A variant that fails every check and spends more still exits 0 without --require.
    let worse = checked_record(
        "evr_0192aaaa000070008000000000009999",
        true,
        true,
        vec![
            synthetic_run("baseline", 0, "completed", "pass", Some(100)),
            synthetic_run("variant", 0, "completed", "fail", Some(5000)),
        ],
    );
    let ungated = diff_record(&world, &worse, &[]);
    assert_eq!(ungated.code, Some(0), "{}", ungated.stderr);
    assert!(!ungated.stdout.contains("requirements"));
    assert!(ungated.stderr.is_empty(), "{}", ungated.stderr);
}

// ── S10: a variant that cannot stage ──────────────────────────────────────────

#[test]
fn a_variant_that_cannot_stage_is_recorded_and_the_run_completes() {
    if common::skip_without_host_support("a_variant_that_cannot_stage_is_recorded") {
        return;
    }
    let world = World::new();
    let baseline_server = ScriptedServer::start([giving_up(), giving_up()].concat());
    let variant_server = ScriptedServer::start(giving_up());
    world.capsule("base", &baseline_server.endpoint, "test-model", SHELL_ALLOW);
    world.capsule_with_driver(
        "variant",
        &variant_server.endpoint,
        "test-model",
        SHELL_ALLOW,
        "local/not-installed-driver",
    );
    buggy_start(&world);
    world.dataset(&[
        json!({"case_id": "c1", "task_path": "task.md", "start": "start", "check": "grep -q fixed main.txt"}),
        json!({"case_id": "c2", "task_path": "task.md", "start": "start", "check": "grep -q fixed main.txt"}),
    ]);

    let stdout = run_ok(
        world
            .mur()
            .args(["eval", "run", "base", "--variant", "variant"]),
    );

    let record = read_json(&world.run_records("base")[0]);
    assert_eq!(record["complete"], true);
    for case in 0..2 {
        let variant = arm_runs(&record, case, "variant");
        assert_eq!(variant[0]["outcome"], "stage_failed", "{record:#}");
        assert_eq!(variant[0]["check_result"], "not_run");
        assert!(variant[0]["check"].is_null());
        assert_eq!(
            arm_runs(&record, case, "baseline")[0]["check_result"],
            "fail"
        );
    }
    assert!(variant_server.requests().is_empty());
    let total = total_row(&stdout, "check");
    assert!(total.contains("0/2 pass (2 not run)"), "{total}");
}

// ── S11: the dataset shape from before ────────────────────────────────────────

#[test]
fn an_old_dataset_runs_as_it_always_has() {
    if common::skip_without_host_support("an_old_dataset_runs_as_it_always_has") {
        return;
    }
    let world = World::new();
    let server = ScriptedServer::start([giving_up(), giving_up()].concat());
    world.capsule("base", &server.endpoint, "test-model", SHELL_ALLOW);
    world.dataset(&[
        json!({"case_id": "old_1", "task_path": "task.md", "expected": {"x": 1}}),
        json!({"case_id": "old_2", "task_path": "task.md"}),
    ]);

    let stdout = run_ok(world.mur().args(["eval", "run", "base"]));

    assert!(
        stdout.starts_with(
            "Running 2 case(s) …\n  case: old_1\n    result: no_scores  session: ses_"
        ),
        "{stdout}"
    );
    assert!(!stdout.contains("arm: "), "{stdout}");
    assert!(stdout.contains("── Summary ──────────────────────────────────────\npass: 0/2\n"));
    assert!(
        stdout.contains("  old_2                    no_scores"),
        "{stdout}"
    );

    let record = read_json(&world.run_records("base")[0]);
    assert_eq!(record["arms"].as_array().unwrap().len(), 1);
    for case in record["cases"].as_array().unwrap() {
        assert!(case["start"].is_null());
        assert!(case["check"].is_null());
        let run = &case["runs"][0];
        assert!(run["check_result"].is_null());
        let workdir = PathBuf::from(run["workdir"].as_str().unwrap());
        assert!(
            fs::read_dir(&workdir).unwrap().next().is_none(),
            "{workdir:?}"
        );
    }

    let diff = run_ok(world.mur().args([
        "eval",
        "diff",
        "--run",
        "--workdir",
        world.path("base/workdir").to_str().unwrap(),
    ]));
    assert!(!diff.contains("variant"), "{diff}");
    assert!(!diff.contains("delta"), "{diff}");
    assert!(!diff.contains("mur trace diff"), "{diff}");
    assert!(row(&diff, "old_1", "check").contains('—'), "{diff}");
}
