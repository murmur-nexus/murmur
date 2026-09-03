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

/// Under `content` the event's hashes are the driver's own digests of what
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

/// An absent `trace:` block and an explicit `capture: meta` behave identically —
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

/// `none` omits all four hash keys and leaves every other field on the record intact.
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

/// No message identity key reaches a blob, and a `message_shas` entry hashes the
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

// ── The request prefix a provider matches its cache against ──────────────────
//
// A provider matches a cached prefix from the first token, in the order the request carries its
// pieces: the system prompt, then the tool schemas, then the messages in send order. The trace's
// per-turn content hashes are taken from that same payload after message identity is stripped, so
// comparing two launches position by position answers whether a second launch could have hit the
// cache the first one filled. Nothing else in the repo answers it: a volatile value in the prefix
// changes no behaviour and fails no other test — the only symptom is the bill.

/// The pieces a provider matches, as `(label, sha)` in wire order: the system prompt at index 0,
/// the tool schemas at index 1, then one entry per message from index 2. Leaving the tool schemas
/// out would leave the cheapest cache break — a reordered tool array — unguarded.
///
/// `response_sha` is deliberately absent: it hashes what came back, not what was sent.
///
/// Requires `trace.capture` to be `meta` or `content`. Under `none` the event carries no hash at
/// all and there is no prefix to compare, so this panics naming the missing key.
fn request_prefix(event: &Value) -> Vec<(String, String)> {
    let hash = |key: &str| {
        event[key]
            .as_str()
            .unwrap_or_else(|| {
                panic!("{key} missing from {event}; trace.capture must record hashes")
            })
            .to_string()
    };
    let mut prefix = vec![
        ("system prompt".to_string(), hash("system_sha")),
        ("tool schemas".to_string(), hash("tools_sha")),
    ];
    prefix.extend(
        shas(event, "message_shas")
            .into_iter()
            .enumerate()
            .map(|(i, sha)| (format!("message {i}"), sha)),
    );
    prefix
}

/// The index of the first position whose shas differ, over the common prefix only.
///
/// `None` when one prefix is a prefix of the other: a longer run that agrees on everything the
/// shorter one sent has broken no cache, because the provider still matches up to the first
/// genuinely new content. Labels are ignored — a sha is what a cache compares.
fn first_prefix_divergence(a: &[(String, String)], b: &[(String, String)]) -> Option<usize> {
    a.iter().zip(b.iter()).position(|((_, x), (_, y))| x != y)
}

/// Two launches of one task must hand the driver a byte-identical request prefix, so the second
/// hits the prompt cache the first filled. The runs use separate temp homes and separate project
/// directories, so any absolute path, session id or timestamp that leaked into a hashed piece
/// shows up here as a differing sha.
#[test]
fn request_prefix_is_identical_across_two_launches() {
    let first = run_session("trace:\n  capture: meta\n");
    let second = run_session("trace:\n  capture: meta\n");
    let first_event = first.sole_inference();
    let second_event = second.sole_inference();

    let a = request_prefix(&first_event);
    let b = request_prefix(&second_event);

    if let Some(i) = first_prefix_divergence(&a, &b) {
        panic!(
            "request prefix diverges at index {i} ({label}): A {x}… B {y}…\n\
             A per-launch value reached the cached prefix, so every request after the first \
             misses the provider's prompt cache — no error, no changed behaviour, only the bill.\n\
             `mur trace diff <run A>/trace.jsonl <run B>/trace.jsonl` renders the same index \
             against the same data, and under `trace.capture: content` names the blobs holding \
             both bodies.",
            label = a[i].0,
            x = &a[i].1[..12],
            y = &b[i].1[..12],
        );
    }
    assert_eq!(
        a.len(),
        b.len(),
        "two runs of one task sent a different number of prefix pieces ({} vs {}) — \
         that is a red flag in the fixture, not a caching regression: one run saw an extra \
         installed tool or an extra message",
        a.len(),
        b.len(),
    );

    // ── The envelope-vs-prefix boundary ──────────────────────────────────────
    //
    // Identity distinguishes the runs on the trace envelope; nothing that identifies a launch
    // may reach a hashed prefix piece.
    //
    //   Envelope identity — must differ: `session_id`, `event_id`, `message_ids[*]`,
    //     `timestamp`. Freshly minted per launch. These say *which run*, never *what was sent*.
    //   Hashed prefix — must agree: `system_sha`, `tools_sha`, `message_shas[*]`. Derived only
    //     from capsule identity, the manifest, the installed tool set and the task input.
    //
    // A field added to the request later belongs on the envelope side unless it is a pure
    // function of those four inputs: a path, a timestamp, a UUID, a counter, or anything read
    // out of an unordered map belongs on the envelope side, or nowhere.
    //
    // Asserting the envelope side is what stops the agreement above from being vacuous — two
    // launches that somehow shared one session directory would agree on every hash for the
    // wrong reason. `timestamp` is on the must-differ side of the boundary but is not asserted
    // here: two sessions can legitimately land in the same clock tick.
    for key in ["session_id", "event_id"] {
        assert_ne!(
            first_event[key], second_event[key],
            "{key} is minted per launch and must distinguish the two runs"
        );
    }
    let ids_a = shas(&first_event, "message_ids");
    let ids_b = shas(&second_event, "message_ids");
    assert_eq!(ids_a.len(), ids_b.len());
    assert!(
        ids_a.iter().zip(&ids_b).all(|(x, y)| x != y),
        "every message id differs between runs, which is why an id array cannot locate a \
         divergence and a sha array can: {ids_a:?} vs {ids_b:?}"
    );
}

/// The index contract of `first_prefix_divergence`, over synthetic values and no session: it
/// reports the *first* disagreement, not the last and not a count, and treats one side being a
/// strict prefix of the other as no divergence. This is what keeps the failure message above
/// honest while nothing in the repo is broken.
#[test]
fn first_prefix_divergence_names_the_first_differing_index() {
    let piece = |label: &str, sha: &str| (label.to_string(), sha.to_string());
    let base = vec![
        piece("system prompt", "aa"),
        piece("tool schemas", "bb"),
        piece("message 0", "cc"),
        piece("message 1", "dd"),
    ];

    assert_eq!(first_prefix_divergence(&base, &base), None);

    // Two positions disagree: the answer is the first of them, not the last and not a count.
    let mut two_differ = base.clone();
    two_differ[1].1 = "zz".to_string();
    two_differ[3].1 = "zz".to_string();
    assert_eq!(first_prefix_divergence(&base, &two_differ), Some(1));
    assert_eq!(first_prefix_divergence(&two_differ, &base), Some(1));

    // Index 0 — the system prompt, where the original regression lived.
    let mut head_differs = base.clone();
    head_differs[0].1 = "zz".to_string();
    assert_eq!(first_prefix_divergence(&base, &head_differs), Some(0));

    // A strict prefix diverges nowhere: everything the shorter run sent still agrees.
    let short = base[..2].to_vec();
    assert_eq!(first_prefix_divergence(&base, &short), None);
    assert_eq!(first_prefix_divergence(&short, &base), None);
    assert_eq!(first_prefix_divergence(&[], &base), None);

    // A shorter side that disagrees inside the common prefix still reports that index.
    let mut short_differs = short.clone();
    short_differs[1].1 = "zz".to_string();
    assert_eq!(first_prefix_divergence(&short_differs, &base), Some(1));
}

/// The retired boolean maps as documented — `true` stores bodies, `false` does not — and both
/// spellings warn.
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

/// Using the retired boolean warns once, naming `trace.capture` as the replacement.
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

/// Setting both keys is refused — even when they agree — and nothing is staged.
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

/// A `capture` value that is not one of the three modes is refused, naming the field
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

// ── Reading a body back out with `mur trace show --body` ─────────────────────
//
// The blob store's contract is that a hash on an `inference` line names a file whose bytes
// the CLI can hand back verbatim. These drive the real `mur` binary against the session
// directory the run above left behind, so the reader and the writer are checked against each
// other rather than against a fixture either of them could have shaped.

impl Run {
    /// `mur trace show <this run's trace> <args…>`.
    fn mur_trace_show(&self, args: &[&str]) -> assert_cmd::assert::Assert {
        assert_cmd::Command::cargo_bin("mur")
            .unwrap()
            .args(["trace", "show"])
            .arg(self.workdir.join("trace.jsonl"))
            .args(args)
            .assert()
    }

    /// The turn the sole `inference` line recorded, as a `--turn` argument.
    fn sole_turn(&self) -> String {
        self.sole_inference()["turn"].as_u64().unwrap().to_string()
    }
}

/// Under `content`, every named selector prints the stored body byte for byte, and the
/// SHA-256 of what was printed is the blob's own filename.
#[test]
fn body_selectors_print_bytes_that_hash_to_the_blob_name() {
    let run = run_session("trace:\n  capture: content\n");
    let event = run.sole_inference();
    let turn = run.sole_turn();

    let mut cases: Vec<(String, String)> = ["system_sha", "tools_sha", "response_sha"]
        .into_iter()
        .map(|key| {
            (
                key.trim_end_matches("_sha").to_string(),
                event[key].as_str().unwrap().to_string(),
            )
        })
        .collect();
    cases.push((
        "message:0".to_string(),
        shas(&event, "message_shas")[0].clone(),
    ));

    for (selector, sha) in cases {
        let assert = run
            .mur_trace_show(&["--body", &selector, "--turn", &turn])
            .success();
        let stdout = assert.get_output().stdout.clone();
        assert_eq!(
            stdout,
            fs::read(run.blob_dir().join(&sha)).unwrap(),
            "--body {selector} must print the blob the turn names"
        );
        assert_eq!(
            murmur_artifact::sha256_hex(&stdout),
            sha,
            "--body {selector} output must hash to the blob's own name"
        );
        assert!(
            !String::from_utf8_lossy(&stdout).contains("── Session"),
            "--body prints the body and nothing else"
        );
    }
}

/// A default `show` names the hashes and prints no body — not even part of one.
#[test]
fn default_show_names_hashes_and_prints_no_body() {
    let run = run_session("trace:\n  capture: content\n");
    let event = run.sole_inference();
    let system_sha = event["system_sha"].as_str().unwrap();
    let system_body = fs::read_to_string(run.blob_dir().join(system_sha)).unwrap();

    let assert = run.mur_trace_show(&[]).success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(stdout.contains("── Wire"), "{stdout}");
    assert!(stdout.contains(&system_sha[..12]), "{stdout}");
    assert!(
        stdout.contains("mur trace show --body system --turn"),
        "{stdout}"
    );
    let distinctive: String = system_body.chars().take(40).collect();
    assert!(
        !stdout.contains(distinctive.trim()),
        "no part of a body may reach default output:\n{stdout}"
    );
}

/// Under `meta` the hash is recorded and no body ever was: the request explains that rather
/// than reporting a file that went missing.
#[test]
fn body_under_meta_says_no_body_was_stored() {
    let run = run_session("trace:\n  capture: meta\n");
    let system_sha = run.sole_inference()["system_sha"]
        .as_str()
        .unwrap()
        .to_string();

    let assert = run
        .mur_trace_show(&["--body", "system", "--turn", &run.sole_turn()])
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(stderr.contains("E-TRC-001"), "{stderr}");
    assert!(stderr.contains(&system_sha), "{stderr}");
    assert!(
        stderr.contains("recorded under capture: meta; no body was stored"),
        "{stderr}"
    );
    assert!(!stderr.contains("No such file"), "{stderr}");
}

/// Under `none` the reason is the absent hash, not an absent blob.
#[test]
fn body_under_none_says_no_hash_was_recorded() {
    let run = run_session("trace:\n  capture: none\n");

    let assert = run
        .mur_trace_show(&["--body", "system", "--turn", &run.sole_turn()])
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

    assert!(stderr.contains("E-TRC-001"), "{stderr}");
    assert!(stderr.contains("recorded no content hashes"), "{stderr}");
    assert!(stderr.contains("trace.capture: none"), "{stderr}");
}
