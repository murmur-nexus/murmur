//! End-to-end coverage for `trace.capture` and the content-addressed blob store.
//!
//! These tests drive a real capsule session through the host `run_agent_loop` against a
//! purpose-built WASM driver fixture (`wire-digest-driver`) that hashes the request *as the guest
//! received it* and reports the digests in its final text. A person can read `out/result.txt`
//! beside `trace.jsonl` and confirm that the hashes the trace recorded name the bytes the driver
//! was handed, without either side trusting the other's serializer.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{launch_session, StagedSession};
use serde_json::Value;

const DRIVER_NAME: &str = "wire-digest-driver";
const DRIVER_VERSION: &str = "0.1.0";

fn driver_wasm() -> PathBuf {
    common::fixture_path("wire-digest-driver/tool/wire-digest-driver.wasm")
}

/// Build a project manifest wiring in the wire-digest-driver, with `trace_block` spliced in
/// verbatim so a test can write any `trace:` shape — including one that must be refused.
///
/// The capsule declares no `capabilities.shell.allow`: the fixture ends the task on turn 0 and
/// never calls a tool, so the session spawns no subprocess and needs no host gate.
fn create_manifest(project_dir: &Path, trace_block: &str) -> PathBuf {
    // Dummy endpoint: the driver answers locally and never makes an HTTP request.
    let endpoint = "http://127.0.0.1:9";
    let manifest = format!(
        concat!(
            "name: agent-capsule\n",
            "version: 0.1.0\n",
            "artifacts:\n",
            "  - name: {driver_name}\n",
            "    version: {driver_version}\n",
            "    runtime: driver\n",
            "capabilities:\n",
            "  network:\n",
            "    allow:\n",
            "      - {endpoint}\n",
            "inference:\n",
            "  transport: http\n",
            "  endpoint: {endpoint}\n",
            "  model: test-model\n",
            "  api_key: test-key\n",
            "  driver:\n",
            "    artifact: {driver_name}\n",
            "{trace_block}",
        ),
        driver_name = DRIVER_NAME,
        driver_version = DRIVER_VERSION,
        endpoint = endpoint,
        trace_block = trace_block,
    );
    fs::write(project_dir.join("murmur.yaml"), manifest).unwrap();
    project_dir.join("murmur.yaml")
}

struct Run {
    /// The driver's own digests of what it received.
    result: String,
    trace: String,
    workdir: PathBuf,
    /// Kept alive so the session directory — and the `blobs/` beside it — outlives the run.
    /// `workdir` is minted under the project directory, so dropping it would delete the very
    /// files these tests read.
    _project: tempfile::TempDir,
    _home: tempfile::TempDir,
}

impl Run {
    /// The session's single `inference` event — the fixture ends the task on turn 0.
    fn sole_inference(&self) -> Value {
        let events: Vec<Value> = self
            .trace
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|ev| ev.get("event_type").and_then(Value::as_str) == Some("inference"))
            .collect();
        assert_eq!(
            events.len(),
            1,
            "expected one inference turn, got {events:?}"
        );
        events.into_iter().next().unwrap()
    }

    fn blob_dir(&self) -> PathBuf {
        self.workdir.join("blobs")
    }

    /// Every filename under `blobs/`, or an empty set when the directory was never created.
    fn blob_names(&self) -> BTreeSet<String> {
        fs::read_dir(self.blob_dir())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `system=<sha> tools=<sha> messages=<sha>,<sha>` as the driver reported it.
    fn reported(&self, key: &str) -> String {
        self.result
            .split_whitespace()
            .find_map(|field| field.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("'{key}=' missing from driver report: {:?}", self.result))
            .to_string()
    }
}

fn run_session(trace_block: &str) -> Run {
    let home = tempfile::tempdir().unwrap();
    let artifact_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &driver_wasm(),
    );
    common::publish_local(&home, &driver_artifact).success();

    let manifest_path = create_manifest(project.path(), trace_block);
    let staged: StagedSession = common::stage_agent_session(&home, project.path(), &manifest_path);
    fs::write(staged.workdir.join("task.md"), "Digest the wire.").unwrap();

    let launched = launch_session(staged, |_| {}).expect("agent launch should succeed");

    Run {
        result: fs::read_to_string(launched.workdir.join("out/result.txt")).unwrap_or_default(),
        trace: fs::read_to_string(launched.workdir.join("trace.jsonl")).unwrap_or_default(),
        workdir: launched.workdir.clone(),
        _project: project,
        _home: home,
    }
}

fn shas(event: &Value, key: &str) -> Vec<String> {
    event[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} missing from {event}"))
        .iter()
        .map(|sha| sha.as_str().unwrap().to_string())
        .collect()
}

/// Scenario 1 and 2(a): under `content` the event's hashes are the driver's own digests of what
/// it received, and every one of them is the name of a real file that re-hashes to that name.
#[test]
fn content_capture_hashes_match_the_driver_and_name_blobs_that_exist() {
    let run = run_session("trace:\n  capture: content\n");
    let event = run.sole_inference();

    assert_eq!(
        event["system_sha"].as_str().unwrap(),
        run.reported("system"),
        "system_sha must equal the driver's digest of the system string it received"
    );
    assert_eq!(
        event["tools_sha"].as_str().unwrap(),
        run.reported("tools"),
        "tools_sha must equal the driver's digest of the tools array it received"
    );
    let message_shas = shas(&event, "message_shas");
    assert!(!message_shas.is_empty());
    assert_eq!(
        message_shas.join(","),
        run.reported("messages"),
        "message_shas must equal the driver's per-message digests, in send order"
    );

    let mut named = message_shas;
    for key in ["system_sha", "tools_sha", "response_sha"] {
        named.push(event[key].as_str().unwrap().to_string());
    }
    for sha in &named {
        let path = run.blob_dir().join(sha);
        let meta = fs::metadata(&path).unwrap_or_else(|e| panic!("blob {sha} must exist: {e}"));
        assert!(meta.is_file(), "{sha} must be a regular file");
        assert_eq!(
            murmur_artifact::sha256_hex(&fs::read(&path).unwrap()),
            *sha,
            "every blob must re-hash to its own filename"
        );
    }

    // A bare sha256 names content; no prefix, no extension.
    for name in run.blob_names() {
        assert_eq!(name.len(), 64, "{name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{name}"
        );
    }
}

/// Scenario 4: an absent `trace:` block and an explicit `capture: meta` behave identically —
/// hashes present, no bodies, no `tool_call.output`.
#[test]
fn meta_is_the_default_and_writes_no_bodies() {
    let implicit = run_session("");
    let explicit = run_session("trace:\n  capture: meta\n");

    for run in [&implicit, &explicit] {
        let event = run.sole_inference();
        for key in ["system_sha", "tools_sha", "response_sha"] {
            assert!(event[key].is_string(), "{key} missing from {event}");
        }
        assert!(!shas(&event, "message_shas").is_empty());
        assert!(
            run.blob_names().is_empty(),
            "meta must store no bodies, found {:?}",
            run.blob_names()
        );
    }

    let implicit_event = implicit.sole_inference();
    let explicit_event = explicit.sole_inference();
    for key in ["system_sha", "tools_sha", "message_shas"] {
        assert_eq!(implicit_event[key], explicit_event[key], "{key}");
    }
}

/// Scenario 5: `none` omits all four keys and leaves the rest of the record as it was.
#[test]
fn none_omits_every_hash_and_creates_no_blob_directory() {
    let run = run_session("trace:\n  capture: none\n");
    let event = run.sole_inference();

    for key in ["system_sha", "tools_sha", "response_sha", "message_shas"] {
        assert!(
            event.get(key).is_none(),
            "{key} must be absent, got {event}"
        );
    }
    assert!(!run.blob_dir().exists());
    for key in [
        "event_type",
        "event_id",
        "session_id",
        "timestamp",
        "turn",
        "input_tokens",
        "output_tokens",
        "decision",
        "message_ids",
    ] {
        assert!(event.get(key).is_some(), "{key} must survive, got {event}");
    }
}

/// Scenario 6: no message identity key reaches a blob, and a `message_shas` entry hashes the
/// post-strip bytes — which is exactly what the driver, which only ever sees post-strip
/// messages, independently computed.
#[test]
fn no_message_identity_reaches_a_blob() {
    let run = run_session("trace:\n  capture: content\n");
    let event = run.sole_inference();

    for sha in shas(&event, "message_shas") {
        let body: Value =
            serde_json::from_slice(&fs::read(run.blob_dir().join(&sha)).unwrap()).unwrap();
        assert!(body.get("id").is_none(), "{body}");
        assert!(body.get("source_id").is_none(), "{body}");
    }
    assert_eq!(
        shas(&event, "message_shas").join(","),
        run.reported("messages")
    );
}

/// Scenario 7: two runs of the same capsule share a message prefix, so their `message_shas`
/// agree pairwise up to the first message whose content differs — the divergence index.
#[test]
fn two_runs_diverge_at_the_first_unequal_message_sha() {
    let first = run_session("trace:\n  capture: meta\n");
    let second = run_session("trace:\n  capture: meta\n");

    let a = shas(&first.sole_inference(), "message_shas");
    let b = shas(&second.sole_inference(), "message_shas");
    assert_eq!(
        a.iter().zip(&b).position(|(x, y)| x != y),
        None,
        "two runs of the same task send the same messages: {a:?} vs {b:?}"
    );

    // The message ids, by contrast, are freshly minted every run and say nothing about content.
    let ids_a = shas(&first.sole_inference(), "message_ids");
    let ids_b = shas(&second.sole_inference(), "message_ids");
    assert_eq!(ids_a.len(), ids_b.len());
    assert!(
        ids_a.iter().zip(&ids_b).all(|(x, y)| x != y),
        "every id differs between runs, which is why ids cannot locate a divergence"
    );
}

/// Scenario 8: the retired boolean still behaves exactly as it did — `true` stores bodies,
/// `false` does not — and both spellings warn.
#[test]
fn the_include_tool_output_alias_still_behaves_as_before() {
    let opted_in = run_session("trace:\n  include_tool_output: true\n");
    assert!(
        !opted_in.blob_names().is_empty(),
        "include_tool_output: true must behave as capture: content"
    );

    let opted_out = run_session("trace:\n  include_tool_output: false\n");
    assert!(
        opted_out.blob_names().is_empty(),
        "include_tool_output: false must behave as capture: meta"
    );
    assert!(opted_out.sole_inference()["system_sha"].is_string());
}

// ── Manifest resolution at the CLI boundary ──────────────────────────────────
//
// The three cases below never reach a session: `trace:` is resolved while the manifest is being
// parsed, so a refusal happens before anything is staged and the warning is printed by the same
// pass. They drive `mur run` rather than `launch_session` because the exit code and stderr are
// the contract.

/// `mur run` against a project whose manifest carries `trace_block`, with no artifacts published
/// — every case here is decided while parsing, before a driver is ever resolved.
fn mur_run_with_trace(trace_block: &str) -> (assert_cmd::assert::Assert, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let manifest_path = create_manifest(project.path(), trace_block);

    let assert = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["run", "--manifest", manifest_path.to_str().unwrap()])
        .args(["--task", "Digest the wire."])
        .assert();
    (assert, project)
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

/// The session directories a run minted. `mur run` stages into `<project>/workdir/<session_id>`,
/// so a manifest refused while parsing leaves this empty.
fn session_dirs(project: &tempfile::TempDir) -> Vec<String> {
    fs::read_dir(project.path().join("workdir"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("ses_"))
                .collect()
        })
        .unwrap_or_default()
}

/// Scenario 8: using the retired boolean warns once, naming `trace.capture` as the replacement.
#[test]
fn the_retired_boolean_warns_and_names_its_replacement() {
    for flag in ["true", "false"] {
        let (assert, _project) =
            mur_run_with_trace(&format!("trace:\n  include_tool_output: {flag}\n"));
        let stderr = stderr_of(&assert);
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|line| line.contains("trace.include_tool_output"))
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one deprecation warning for '{flag}', got: {stderr}"
        );
        assert!(warnings[0].starts_with("warning:"), "{stderr}");
        assert!(
            warnings[0].contains("trace.capture"),
            "the warning must name the replacement key, got: {stderr}"
        );
    }
}

/// Scenario 9: setting both keys is refused — even when they agree — and nothing is staged.
#[test]
fn setting_both_capture_keys_refuses_the_launch() {
    let (assert, project) =
        mur_run_with_trace("trace:\n  capture: content\n  include_tool_output: true\n");
    let assert = assert.failure();
    let stderr = stderr_of(&assert);

    assert!(stderr.contains("trace.capture"), "{stderr}");
    assert!(stderr.contains("trace.include_tool_output"), "{stderr}");
    assert!(
        stderr.contains("keep 'trace.capture'"),
        "the error must say which of the two keys survives, got: {stderr}"
    );
    assert!(
        session_dirs(&project).is_empty(),
        "a refused manifest must stage no session, found {:?}",
        session_dirs(&project)
    );
}

/// Scenario 10: a `capture` value that is not one of the three modes is refused, naming the field
/// and every value it would have accepted.
#[test]
fn an_unparseable_capture_value_is_refused() {
    let (assert, project) = mur_run_with_trace("trace:\n  capture: verbose\n");
    let assert = assert.failure();
    let stderr = stderr_of(&assert);

    assert!(stderr.contains("trace.capture"), "{stderr}");
    for accepted in ["none", "meta", "content"] {
        assert!(
            stderr.contains(accepted),
            "the error must list '{accepted}', got: {stderr}"
        );
    }
    assert!(stderr.contains("verbose"), "{stderr}");
    assert!(session_dirs(&project).is_empty());
}
