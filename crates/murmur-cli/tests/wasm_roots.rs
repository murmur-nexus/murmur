//! The host folders a WASM tool is handed beyond the work folder — its extra roots under
//! `murmur-host/` — driven through the real `mur` binary on a scripted model.
//!
//! The tool is the committed `runtime-dir-probe` fixture, which answers `roots` with the guest
//! paths of its preopens and `read`/`write` with what `std::fs` could do with one path, so each
//! assertion reads what the guest could reach. Every scratch directory is created under
//! `CARGO_TARGET_TMPDIR`, never under `/tmp`, which a host path may not lie under.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Output,
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER: &str = "local/murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const PROBE: &str = "runtime-dir-probe";
const PROBE_ARTIFACT: &str = "local/runtime-dir-probe";
const PROBE_VERSION: &str = "0.1.0";
/// What the probe's `write` writes, whatever the call says.
const PROBE_BYTES: &str = "written by the probe";

/// A scratch directory under `CARGO_TARGET_TMPDIR`: the guard, the path as created and its
/// canonical spelling (they differ where the target directory is reached through a symlink).
fn scratch() -> (TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spelled = dir.path().to_path_buf();
    let canonical = spelled.canonicalize().unwrap();
    assert!(
        !canonical.starts_with("/tmp"),
        "CARGO_TARGET_TMPDIR resolves under /tmp ({}); these tests need a directory outside it",
        canonical.display()
    );
    (dir, spelled, canonical)
}

/// `murmur-host` followed by `path`: where a tool holding a root covering `path` opens it.
fn through(path: &Path) -> String {
    format!("murmur-host{}", path.display())
}

/// A scratch `HOME` and a project, with the driver and the probe installed into the project.
struct Fixture {
    _root: TempDir,
    /// The scratch root as created, possibly through a symlinked parent.
    spelled: PathBuf,
    /// The scratch root, canonical.
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let (root, spelled, base) = scratch();
        let home = base.join("home");
        let project = base.join("project");
        let artifacts = base.join("artifacts");
        for dir in [&home, &project, &artifacts] {
            fs::create_dir_all(dir).unwrap();
        }
        let fixture = Self {
            _root: root,
            spelled,
            base,
            home,
            project,
        };
        fixture.write_manifest("http://127.0.0.1:9", "");
        let driver = common::create_driver_artifact(
            &artifacts,
            DRIVER,
            DRIVER_VERSION,
            &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
        );
        fixture.install(&driver);
        let probe = common::create_tool_artifact(
            &artifacts,
            PROBE_ARTIFACT,
            PROBE_VERSION,
            &common::fixture_path("runtime-dir-probe/tool/runtime-dir-probe.wasm"),
        );
        fixture.install(&probe);
        fixture
    }

    fn install(&self, artifact: &Path) {
        self.mur()
            .args(["install", artifact.to_str().unwrap(), "--no-precompile"])
            .assert()
            .success();
    }

    fn manifest(&self) -> PathBuf {
        self.project.join("murmur.yaml")
    }

    /// The capsule: the driver at `endpoint`, the unscoped probe, and `capabilities` (each line
    /// indented under `capabilities:`).
    fn write_manifest(&self, endpoint: &str, capabilities: &str) {
        fs::write(
            self.manifest(),
            format!(
                "name: wasm-roots-capsule\nversion: 0.0.1\nartifacts:\n  - name: {DRIVER}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n    gateway:\n      endpoint: \
                 {endpoint}\n      api_key: test-key\n  - name: {PROBE_ARTIFACT}\n    version: \
                 {PROBE_VERSION}\n    runtime: tool\ncapabilities:\n  network:\n    allow:\n      \
                 - {endpoint}\n{capabilities}inference:\n  transport: http\n  model: test-model\n  \
                 driver:\n    artifact: {DRIVER}\n"
            ),
        )
        .unwrap();
    }

    /// `~/.murmur/config.yaml` in the scratch `HOME`.
    fn global_config(&self, yaml: &str) {
        fs::create_dir_all(self.home.join(".murmur")).unwrap();
        fs::write(self.home.join(".murmur/config.yaml"), yaml).unwrap();
    }

    fn mur(&self) -> Command {
        let mut command = Command::cargo_bin("mur").unwrap();
        command.env("HOME", &self.home).current_dir(&self.project);
        command
    }

    /// One `mur run` with `args`, the model calling the probe with each `(id, input)` in order.
    fn run(
        &self,
        calls: &[(&str, Value)],
        capabilities: &str,
        args: &[&str],
    ) -> (Output, Vec<Value>) {
        let mut responses: Vec<String> = calls
            .iter()
            .map(|(id, input)| common::tool_use_response(id, PROBE, input.clone()))
            .collect();
        responses.push(end_turn());
        let server = common::ScriptedServer::start(responses);
        self.write_manifest(&server.endpoint, capabilities);
        let output = self
            .mur()
            .args(["run", "--manifest", self.manifest().to_str().unwrap()])
            .args(["--task", "Run each call once.", "--verbose"])
            .args(args)
            .write_stdin("")
            .timeout(std::time::Duration::from_secs(300))
            .output()
            .unwrap();
        assert!(
            output.status.success() && stdout(&output).contains("status:  ok"),
            "the run must finish ok\nstdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        (output, server.requests())
    }

    /// `mur run --explain-scope` with `args`, as text or JSON.
    fn explain(&self, capabilities: &str, json: bool, args: &[&str]) -> Output {
        self.write_manifest("http://127.0.0.1:9", capabilities);
        let mut command = self.mur();
        command.args([
            "run",
            "--manifest",
            self.manifest().to_str().unwrap(),
            "--explain-scope",
        ]);
        if json {
            command.arg("--json");
        }
        let output = command.args(args).output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        output
    }

    fn explain_json(&self, capabilities: &str, args: &[&str]) -> Value {
        serde_json::from_slice(&self.explain(capabilities, true, args).stdout)
            .expect("one JSON object")
    }

    fn doctor(&self, capabilities: &str) -> Output {
        self.write_manifest("http://127.0.0.1:9", capabilities);
        self.mur().arg("doctor").output().unwrap()
    }
}

fn end_turn() -> String {
    json!({
        "id": "msg_end",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": "done"}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
    .to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read(id: &'static str, path: &str) -> (&'static str, Value) {
    (id, json!({"op": "read", "path": path}))
}

fn write(id: &'static str, path: &str) -> (&'static str, Value) {
    (id, json!({"op": "write", "path": path}))
}

/// A write the key-name rules read as one: the probe ignores `content`.
fn write_content(id: &'static str, path: &str) -> (&'static str, Value) {
    (id, json!({"op": "write", "path": path, "content": "x"}))
}

fn roots(id: &'static str) -> (&'static str, Value) {
    (id, json!({"op": "roots"}))
}

/// The text of call `id`'s result, printed so a run's record shows what the guest saw.
#[allow(clippy::print_stdout)]
fn result(requests: &[Value], id: &str, output: &Output) -> String {
    let text = common::tool_result_text(requests, id)
        .unwrap_or_else(|| panic!("no tool result for {id}; stderr: {}", stderr(output)));
    println!("── {id}\n{text}");
    text
}

/// The guest paths a `roots` result lists: every line naming a preopen.
fn listed_roots(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| *line == "." || *line == "state" || line.starts_with("murmur-"))
        .map(str::to_string)
        .collect()
}

/// A call the guest completed: a write answers `ok`, a read answers the bytes it read.
fn assert_ok(text: &str, id: &str) {
    assert!(!text.contains("err:"), "{id}: {text}");
}

fn assert_err(text: &str, id: &str) {
    assert!(text.contains("err:"), "{id}: {text}");
}

fn trace(output: &Output) -> Vec<Value> {
    let records = common::parse_records_from_stdout(&stdout(output));
    common::read_whole_trace(&records.join("trace.jsonl"))
}

fn events<'a>(trace: &'a [Value], event_type: &str) -> Vec<&'a Value> {
    trace
        .iter()
        .filter(|event| event["event_type"] == event_type)
        .collect()
}

fn probe_preopen(report: &Value) -> Value {
    report["preopens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|preopen| preopen["artifact"] == PROBE_ARTIFACT || preopen["artifact"] == PROBE)
        .unwrap_or_else(|| panic!("no probe preopen in {report}"))
        .clone()
}

// ── S3: unconfined ─────────────────────────────────────────────────────────────────────────

/// Under `--unconfined` the probe holds the host's `/` at `murmur-host`: it reads and writes the
/// real home folder through it, a relative path stays in the work folder, and an unprefixed
/// absolute path resolves into the work folder rather than onto the host.
#[test]
fn an_unconfined_wasm_tool_opens_a_file_in_the_real_home_folder() {
    let fixture = Fixture::new();
    // The probe's own `HOME` is not ours to pick; the operator's real home folder here is any
    // host folder, so the scratch `HOME` stands in for it.
    let notes = fixture.home.join("notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("hello.txt"), "hello from the host\n").unwrap();
    let outside = fixture.base.join("outside.txt");
    fs::write(&outside, "through a link\n").unwrap();
    std::os::unix::fs::symlink(&outside, notes.join("link.txt")).unwrap();
    std::os::unix::fs::symlink("../../outside.txt", notes.join("relative-link.txt")).unwrap();

    let calls = [
        roots("toolu_roots"),
        read("toolu_read", &format!("{}/hello.txt", through(&notes))),
        write("toolu_write", &format!("{}/new.txt", through(&notes))),
        write("toolu_rel", "rel.txt"),
        read("toolu_plain", &format!("{}/hello.txt", notes.display())),
        read("toolu_link", &format!("{}/link.txt", through(&notes))),
        read(
            "toolu_relative_link",
            &format!("{}/relative-link.txt", through(&notes)),
        ),
    ];
    let work = fixture.base.join("work");
    fs::create_dir(&work).unwrap();
    let workdir = work.display().to_string();
    let (output, requests) = fixture.run(&calls, "", &["--unconfined", "--workdir", &workdir]);

    let listed = result(&requests, "toolu_roots", &output);
    assert_eq!(
        listed_roots(&listed),
        vec![".", "murmur-host", "murmur-runtime"],
        "{listed}"
    );
    let text = result(&requests, "toolu_read", &output);
    assert_ok(&text, "toolu_read");
    assert!(text.contains("hello from the host"), "{text}");
    assert_ok(&result(&requests, "toolu_write", &output), "toolu_write");
    assert_eq!(
        fs::read_to_string(notes.join("new.txt")).unwrap(),
        PROBE_BYTES
    );
    assert_ok(&result(&requests, "toolu_rel", &output), "toolu_rel");
    assert_eq!(
        fs::read_to_string(work.join("rel.txt")).unwrap(),
        PROBE_BYTES,
        "a relative path lands in the work folder"
    );
    assert!(!Path::new("/rel.txt").exists(), "never at the host's /");
    assert_err(&result(&requests, "toolu_plain", &output), "toolu_plain");
    // An absolute symlink is never followed inside a WASI root, even one whose target the same
    // root covers; a relative one that stays inside the root is.
    let link = result(&requests, "toolu_link", &output);
    assert!(link.contains("err: Operation not permitted"), "{link}");
    let relative = result(&requests, "toolu_relative_link", &output);
    assert_ok(&relative, "toolu_relative_link");
    assert!(relative.contains("through a link"), "{relative}");

    let report = fixture.explain_json("", &["--unconfined"]);
    assert_eq!(
        probe_preopen(&report)["extra_roots"],
        json!([{
            "guest_path": "murmur-host",
            "path": "/",
            "access": "read-write",
            "source": "unconfined",
        }]),
        "{report}"
    );
    let text = stdout(&fixture.explain("", false, &["--unconfined"]));
    assert!(
        text.contains("; whole host read-write under murmur-host/"),
        "{text}"
    );
}

// ── S4: host paths ─────────────────────────────────────────────────────────────────────────

/// A contained session hands the probe each `host_paths` entry under `murmur-host/` with its
/// access: a `read` folder is readable and refuses a write, a `read-write` folder is writable,
/// and a folder not listed is out of reach.
#[test]
fn host_paths_reach_a_wasm_tool_with_their_access() {
    let fixture = Fixture::new();
    let reference = fixture.home.join("reference");
    let output_dir = fixture.home.join("output");
    let unlisted = fixture.home.join("unlisted");
    for dir in [&reference, &output_dir, &unlisted] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(reference.join("data.txt"), "reference data\n").unwrap();
    fs::write(unlisted.join("secret.txt"), "not lent\n").unwrap();
    fixture.global_config(&format!(
        "host_paths:\n  - path: {}\n    access: read\n  - path: {}\n    access: read-write\n",
        reference.display(),
        output_dir.display()
    ));
    let advisory = "  containment: advisory\n";

    let calls = [
        roots("toolu_roots"),
        read("toolu_read", &format!("{}/data.txt", through(&reference))),
        write(
            "toolu_write_ro",
            &format!("{}/data.txt", through(&reference)),
        ),
        write(
            "toolu_write_rw",
            &format!("{}/made.txt", through(&output_dir)),
        ),
        read(
            "toolu_unlisted",
            &format!("{}/secret.txt", through(&unlisted)),
        ),
    ];
    let (output, requests) = fixture.run(&calls, advisory, &[]);

    let listed = result(&requests, "toolu_roots", &output);
    assert_eq!(
        listed_roots(&listed),
        vec![
            ".".to_string(),
            through(&output_dir),
            through(&reference),
            "murmur-runtime".to_string(),
        ],
        "sorted: {listed}"
    );
    let text = result(&requests, "toolu_read", &output);
    assert_ok(&text, "toolu_read");
    assert!(text.contains("reference data"), "{text}");
    assert_err(
        &result(&requests, "toolu_write_ro", &output),
        "toolu_write_ro",
    );
    assert_eq!(
        fs::read_to_string(reference.join("data.txt")).unwrap(),
        "reference data\n"
    );
    assert_ok(
        &result(&requests, "toolu_write_rw", &output),
        "toolu_write_rw",
    );
    assert_eq!(
        fs::read_to_string(output_dir.join("made.txt")).unwrap(),
        PROBE_BYTES
    );
    assert_err(
        &result(&requests, "toolu_unlisted", &output),
        "toolu_unlisted",
    );
    assert_eq!(
        fs::read_dir(&unlisted).unwrap().count(),
        1,
        "nothing created in the unlisted folder"
    );

    // `session_start` records the report `--explain-scope --json` prints for the same launch.
    let start = events(&trace(&output), "session_start")[0].clone();
    let report = fixture.explain_json(advisory, &[]);
    assert_eq!(
        start["effective_grants"]["preopens"].to_string(),
        report["preopens"].to_string()
    );
    assert_eq!(
        probe_preopen(&report)["extra_roots"],
        json!([
            {
                "guest_path": through(&reference),
                "path": reference.display().to_string(),
                "access": "read",
                "source": "host_paths",
            },
            {
                "guest_path": through(&output_dir),
                "path": output_dir.display().to_string(),
                "access": "read-write",
                "source": "host_paths",
            },
        ]),
        "{report}"
    );
    let text = stdout(&fixture.explain(advisory, false, &[]));
    assert!(
        text.contains(&format!(
            "; host folders under murmur-host/: {} (read), {} (read-write)",
            reference.display(),
            output_dir.display()
        )),
        "{text}"
    );
    let doctor = stdout(&fixture.doctor(advisory));
    assert!(
        doctor.contains(&format!(
            "; host folders under murmur-host/: {} (read), {} (read-write)",
            reference.display(),
            output_dir.display()
        )),
        "{doctor}"
    );
}

// ── S5: a plain contained session ──────────────────────────────────────────────────────────

/// Without `host_paths` a contained session hands the probe exactly the work folder and
/// `murmur-runtime`, and `murmur-host/…` reaches nothing on the host.
#[test]
fn a_contained_session_without_host_paths_has_todays_preopens() {
    let fixture = Fixture::new();
    let notes = fixture.home.join("notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("hello.txt"), "hello from the host\n").unwrap();

    let calls = [
        roots("toolu_roots"),
        read("toolu_read", &format!("{}/hello.txt", through(&notes))),
    ];
    let (output, requests) = fixture.run(&calls, "", &[]);
    let listed = result(&requests, "toolu_roots", &output);
    assert_eq!(
        listed_roots(&listed),
        vec![".", "murmur-runtime"],
        "{listed}"
    );
    assert_err(&result(&requests, "toolu_read", &output), "toolu_read");

    let report = fixture.explain_json("", &[]);
    for preopen in report["preopens"].as_array().unwrap() {
        assert_eq!(preopen["extra_roots"], json!([]), "{preopen}");
    }
    for text in [
        stdout(&fixture.explain("", false, &[])),
        stdout(&fixture.doctor("")),
    ] {
        assert!(!text.contains("murmur-host"), "{text}");
    }
}

// ── S6: read_only through a root ───────────────────────────────────────────────────────────

const READ_ONLY_TESTS: &str = "  filesystem:\n    read_only:\n      - tests\n";
const KEPT: &str = "kept\n";

/// The work folder of an S6 run, as created and canonical, holding `tests/keep.txt`.
fn read_only_workdir(fixture: &Fixture) -> (PathBuf, PathBuf) {
    let spelled = fixture.spelled.join("project/workdir");
    let canonical = fixture.base.join("project/workdir");
    fs::create_dir_all(canonical.join("tests")).unwrap();
    fs::write(canonical.join("tests/keep.txt"), KEPT).unwrap();
    (spelled, canonical)
}

/// Each refused call: refused before dispatch, named in a `protected_path_denied` record, told to
/// the model, with `tests/keep.txt` unchanged; the control write ran.
fn assert_refused(
    output: &Output,
    requests: &[Value],
    refused: &[&str],
    dispatched: usize,
    canonical: &Path,
) {
    let trace = trace(output);
    let denied = events(&trace, "protected_path_denied");
    assert_eq!(denied.len(), refused.len(), "{denied:#?}");
    for record in &denied {
        assert_eq!(record["rule"], "tests", "{record}");
        assert_eq!(record["path"], "tests/keep.txt", "{record}");
        assert_eq!(record["call"], "tool", "{record}");
    }
    let tool_calls = events(&trace, "tool_call")
        .into_iter()
        .filter(|event| event["tool_name"] == PROBE || event["tool_name"] == PROBE_ARTIFACT)
        .count();
    assert_eq!(tool_calls, dispatched, "only the control call ran");
    for id in refused {
        let text = result(requests, id, output);
        assert!(text.contains("'tests'"), "{id}: {text}");
        assert!(text.contains("Nothing ran"), "{id}: {text}");
    }
    assert_eq!(
        fs::read_to_string(canonical.join("tests/keep.txt")).unwrap(),
        KEPT
    );
    assert_eq!(
        fs::read_to_string(canonical.join("free.txt")).unwrap(),
        PROBE_BYTES,
        "the control write ran"
    );
}

/// Under `--unconfined`, a protected work-folder file is refused through `murmur-host/` and
/// through every absolute spelling of the work folder.
#[test]
fn an_unconfined_read_only_subtree_is_refused_through_the_whole_host() {
    let fixture = Fixture::new();
    let (spelled, canonical) = read_only_workdir(&fixture);
    let mut calls = vec![
        write_content(
            "toolu_through",
            &format!("{}/tests/keep.txt", through(&spelled)),
        ),
        write_content(
            "toolu_absolute",
            &format!("{}/tests/keep.txt", spelled.display()),
        ),
    ];
    if spelled != canonical {
        calls.push(write_content(
            "toolu_canonical_through",
            &format!("{}/tests/keep.txt", through(&canonical)),
        ));
        calls.push(write_content(
            "toolu_canonical",
            &format!("{}/tests/keep.txt", canonical.display()),
        ));
    }
    calls.push(write_content(
        "toolu_free",
        &format!("{}/free.txt", through(&canonical)),
    ));
    let refused: Vec<&str> = calls[..calls.len() - 1].iter().map(|(id, _)| *id).collect();
    eprintln!(
        "work folder as created: {}; canonical: {}",
        spelled.display(),
        canonical.display()
    );
    let workdir = spelled.display().to_string();
    let (output, requests) = fixture.run(
        &calls,
        READ_ONLY_TESTS,
        &["--unconfined", "--workdir", &workdir],
    );
    assert_ok(&result(&requests, "toolu_free", &output), "toolu_free");
    assert_refused(&output, &requests, &refused, 1, &canonical);
}

/// A contained session lending the project folder read-write refuses a protected work-folder
/// file reached through that host path.
#[test]
fn a_read_only_subtree_is_refused_through_a_read_write_host_path() {
    let fixture = Fixture::new();
    let (spelled, canonical) = read_only_workdir(&fixture);
    fixture.global_config(&format!(
        "host_paths:\n  - path: {}\n    access: read-write\n",
        fixture.project.display()
    ));
    let calls = [
        write_content(
            "toolu_through",
            &format!("{}/tests/keep.txt", through(&canonical)),
        ),
        write_content("toolu_free", &format!("{}/free.txt", through(&canonical))),
    ];
    let workdir = spelled.display().to_string();
    let capabilities = format!("  containment: advisory\n{READ_ONLY_TESTS}");
    let (output, requests) = fixture.run(&calls, &capabilities, &["--workdir", &workdir]);
    let listed_report = fixture.explain_json(&capabilities, &["--workdir", &workdir]);
    assert_eq!(
        probe_preopen(&listed_report)["extra_roots"][0]["guest_path"],
        json!(through(&fixture.project)),
        "{listed_report}"
    );
    assert_ok(&result(&requests, "toolu_free", &output), "toolu_free");
    assert_refused(&output, &requests, &["toolu_through"], 1, &canonical);
}
