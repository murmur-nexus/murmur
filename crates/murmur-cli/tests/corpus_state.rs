//! The corpus tool and the durable-state grant, proved against each other.
//!
//! Four things have to agree for a capsule to reach the corpus, and only a real launch exercises
//! all four at once. `tests/state.rs` covers `capabilities.state` against a fixture capsule that
//! already knows the runtime's conventions, and the corpus's own repository covers the component
//! against a Wasmtime context that test builds itself rather than one `mur run` built:
//!
//! 1. the guest preopen name — [`capsule_runtime::STATE_PREOPEN_NAME`] against the corpus's own
//!    `STATE_DIR`, both of which are the bare string `state`;
//! 2. the store name `capabilities.state: {}` defaults to, which is the capsule's name and not the
//!    artifact's;
//! 3. where `corpus.config.json` resolves — inside the `0700` directory the runtime created, which
//!    is what puts it out of the agent's reach;
//! 4. what a missing grant produces — `state_unavailable`, rather than a corpus quietly written
//!    into the session workdir.
//!
//! Every test here drives the real `mur` binary as a subprocess with `HOME` pointed at a temporary
//! directory. An in-process `stage_session`/`launch_session` pair cannot do this job: the store's
//! host path is resolved from the process's own `HOME`, so an in-process test would either write
//! into the developer's real `~/.murmur/state/` or have to mutate `HOME` and race every other test
//! in the binary.
//!
//! The corpus component is built from the `default-artifacts` checkout named by
//! `MURMUR_DEFAULT_ARTIFACTS_DIR` on every run, and no copy of it is committed here: a checked-in
//! copy of a third-party artifact goes stale silently, and a proof against a stale copy says
//! nothing about the corpus that ships. Unset variable means skip.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const TOOL_NAME: &str = "murmur-tool-corpus";
const TOOL_VERSION: &str = "0.1.0";
const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

/// The corpus's own file names, relative to the state directory. Repeated here rather than
/// imported because this crate does not depend on the corpus: these two literals are half of what
/// the test is checking, so a copy that drifts is the failure it is meant to catch.
const CORPUS_FILE: &str = "corpus.jsonl";
const CONFIG_FILE: &str = "corpus.config.json";

/// The operator configuration every scenario that expects a working corpus writes into the store
/// before the first launch, exactly as an operator would.
///
/// One type, `note`, whose derived three-letter id prefix (`not`) collides with none of the
/// reserved runtime prefixes, so no `prefix_map` override is needed. `read_recent` and `search`
/// blocks are absent — the corpus supplies its own caps for both, and omitting them
/// keeps this the minimal config that parses.
const OPERATOR_CONFIG: &str = r#"{"config_version":1,"types":{"note":{"schema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}}}}"#;

// ── staging ──────────────────────────────────────────────────────────────────

/// Build the corpus component out of the `default-artifacts` checkout and return its path.
///
/// The build runs on every call rather than only when the file is missing, because the point of
/// building from a checkout is to test the corpus that checkout currently describes; cargo no-ops
/// when it is already fresh. `current_dir` rather than `--manifest-path` so rustup honours that
/// repository's `rust-toolchain.toml`, and `RUSTUP_TOOLCHAIN` is cleared for the same reason —
/// inherited from a `cargo test` run here it would silently override the pin.
///
/// `None` — for the caller to turn into a skip — when the checkout is absent, when the build
/// fails, or when it reports success and leaves nothing behind.
fn corpus_component(checkout: &Path) -> Option<PathBuf> {
    if !checkout.join("Cargo.toml").exists() {
        eprintln!(
            "[corpus fixture] MURMUR_DEFAULT_ARTIFACTS_DIR names {}, which holds no Cargo.toml",
            checkout.display()
        );
        return None;
    }

    let status = std::process::Command::new("cargo")
        .current_dir(checkout)
        .env_remove("RUSTUP_TOOLCHAIN")
        .args([
            "build",
            "-p",
            TOOL_NAME,
            "--target",
            "wasm32-wasip2",
            "--release",
        ])
        .status()
        .ok()?;
    if !status.success() {
        eprintln!("[corpus fixture] cargo build of {TOOL_NAME} failed");
        return None;
    }

    // A build can exit 0 without producing the component, so the caller gets a clean skip rather
    // than a path that fails when the zip is packed.
    let wasm = checkout
        .join("target/wasm32-wasip2/release")
        .join("murmur_tool_corpus.wasm");
    if !wasm.exists() {
        eprintln!(
            "[corpus fixture] {} not found after a successful build",
            wasm.display()
        );
        return None;
    }
    Some(wasm)
}

/// Pack the corpus as a `runtime: tool` artifact: its own `murmur.yaml` verbatim at the archive
/// root, and the component beside it under the name `requires_files` declares.
///
/// The manifest is copied byte for byte from the checkout rather than synthesised, so the test
/// exercises the real `input_schema`, the real `description` the model is shown, and the real
/// bundled `capabilities:` block — the last of which is what Scenarios B and C show grants
/// nothing on its own. `select_root_wasm_in_archive` picks the single root `.wasm`, so the
/// underscored file name is the entry name.
fn create_corpus_artifact(dir: &Path, manifest_bytes: &[u8], wasm: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{TOOL_NAME}-{TOOL_VERSION}.mur.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&artifact_path).unwrap());
    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    zip.write_all(manifest_bytes).unwrap();

    zip.start_file("murmur_tool_corpus.wasm", options).unwrap();
    zip.write_all(&fs::read(wasm).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// A scratch `HOME`, a project directory, and both artifacts reachable from the two places a
/// `mur run` launch consults.
struct Staging {
    home: TempDir,
    project: TempDir,
    /// Where the packed `.mur.zip`s live. Held only to keep the directory alive.
    _artifacts: TempDir,
}

impl Staging {
    fn manifest(&self) -> PathBuf {
        self.project.path().join("murmur.yaml")
    }

    /// The host directory `capabilities.state` opens for a given store name.
    fn store_dir(&self, store: &str) -> PathBuf {
        self.home.path().join(".murmur/state").join(store)
    }

    fn state_root(&self) -> PathBuf {
        self.home.path().join(".murmur/state")
    }

    /// Write the operator's configuration into a store, creating the store as an operator would.
    ///
    /// `ensure_state_store` is idempotent and re-asserts `0700` over whatever it finds, so a store
    /// prepared here is the same store a launch would have made.
    fn write_operator_config(&self, store: &str) -> PathBuf {
        let dir = self.store_dir(store);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);
        fs::write(&path, OPERATOR_CONFIG).unwrap();
        path
    }

    /// Non-empty lines in a store's corpus file.
    fn corpus_lines(&self, store: &str) -> Vec<String> {
        let path = self.store_dir(store).join(CORPUS_FILE);
        fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Stage a capsule that installs the corpus alongside the inference driver.
///
/// `state_yaml` is spliced in after the tool entry's `state:` key, so `None` produces an entry
/// with no `capabilities:` block at all — an absent block, which is the default-deny baseline,
/// rather than an empty one.
///
/// Both artifacts are published into the scratch `HOME`'s registry *and* installed into the
/// project store: the first is what a staged session resolves against, the second is what a `mur
/// run` CLI invocation resolves against, and installing into only one produces `E-RUN-008` at
/// launch.
fn stage(checkout: &Path, capsule_name: &str, state_yaml: Option<&str>) -> Option<Staging> {
    let wasm = corpus_component(checkout)?;

    let staging = Staging {
        home: tempfile::tempdir().unwrap(),
        project: tempfile::tempdir().unwrap(),
        _artifacts: tempfile::tempdir().unwrap(),
    };

    // A placeholder endpoint, so `mur install` can find the project root before any server is
    // bound; every launch rewrites the manifest with the endpoint it will actually talk to.
    write_manifest(
        staging.project.path(),
        capsule_name,
        "http://127.0.0.1:1",
        state_yaml,
    );

    let corpus_artifact = create_corpus_artifact(
        staging._artifacts.path(),
        &fs::read(checkout.join("tools/murmur-tool-corpus/murmur.yaml")).unwrap(),
        &wasm,
    );
    let driver_artifact = common::create_driver_artifact(
        staging._artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );

    for artifact in [&driver_artifact, &corpus_artifact] {
        common::publish_local(&staging.home, artifact).success();
        common::install_artifact_to_project(staging.project.path(), artifact).success();
    }

    Some(staging)
}

/// Write the capsule manifest, pointing inference at `endpoint` and allowing it on the network.
fn write_manifest(
    project: &Path,
    capsule_name: &str,
    endpoint: &str,
    state_yaml: Option<&str>,
) -> PathBuf {
    let capabilities = state_yaml
        .map(|yaml| format!("    capabilities:\n      state:{yaml}\n"))
        .unwrap_or_default();

    let manifest = project.join("murmur.yaml");
    fs::write(
        &manifest,
        format!(
            "name: {capsule_name}\nversion: 0.1.0\nartifacts:\n  - name: {DRIVER_NAME}\n    \
             version: {DRIVER_VERSION}\n    runtime: driver\n  - name: {TOOL_NAME}\n    version: \
             {TOOL_VERSION}\n    runtime: tool\n{capabilities}capabilities:\n  network:\n    \
             allow:\n      - {endpoint}\ninference:\n  transport: http\n  endpoint: \
             {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: \
             {DRIVER_NAME}\n"
        ),
    )
    .unwrap();
    manifest
}

// ── driving one session ──────────────────────────────────────────────────────

/// Run one task to completion and return the session workdir it reported.
///
/// No `--workdir`, so every invocation gets a fresh `<manifest_dir>/workdir/<session_id>` — which
/// is what makes "a second session, a different directory" free rather than something the test has
/// to arrange. `LifecycleConfig::default()` is single-task-then-exit, so no lifecycle flag is
/// needed for the process to terminate.
fn run_session(home: &TempDir, manifest: &Path, task: &str) -> PathBuf {
    let stdout = Command::cargo_bin("mur")
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
        .success()
        .get_output()
        .stdout
        .clone();

    common::parse_workdir_from_stdout(&String::from_utf8(stdout).unwrap())
}

fn tool_use_response(tool_id: &str, input: Value) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": tool_id,
            "name": TOOL_NAME,
            "input": input,
        }],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

fn end_turn_response(text: &str) -> String {
    json!({
        "id": "msg_2",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// The corpus's own response envelope for one call, read back out of the `tool_result` block the
/// runtime posted on the following request.
///
/// The runtime sends the tool's `data` (falling back to its `summary`), and the corpus puts its
/// whole `{ok, operation, …}` envelope in `data`, so this parses back to exactly what the tool
/// returned.
fn corpus_response(requests: &[Value], tool_id: &str) -> Value {
    let block = common::find_tool_result(requests, tool_id)
        .unwrap_or_else(|| panic!("no tool_result posted for {tool_id}"));
    let text = common::extract_result_text(&block);
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{tool_id} returned text that is not JSON ({err}): {text}"))
}

// ── assertions shared by the scenarios ───────────────────────────────────────

/// Nothing the corpus writes may appear anywhere under a session workdir, at any depth.
///
/// The corpus's core safety property is that it refuses rather than falling back to the workdir,
/// and a fallback would be invisible to every other assertion here: the store would work, and it
/// would be one the agent can rewrite at will. A recursive walk is the only check that sees it.
fn assert_workdir_holds_no_corpus(workdir: &Path) {
    assert!(
        workdir.is_dir(),
        "{} must be a session workdir",
        workdir.display()
    );
    let mut pending = vec![workdir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let file_type = entry.file_type().unwrap();

            assert!(
                !(file_type.is_dir() && name == "state"),
                "a 'state' directory must never appear in a session workdir: {}",
                entry.path().display()
            );
            assert!(
                !(name == CORPUS_FILE || name == CONFIG_FILE),
                "the corpus must never write into a session workdir: {}",
                entry.path().display()
            );

            if file_type.is_dir() {
                pending.push(entry.path());
            }
        }
    }
}

/// Both directories a store needs are owner-only: the store itself, and the root above it, since
/// a readable root leaks the names of every capsule's store.
fn assert_store_is_private(staging: &Staging, store: &str) {
    for dir in [staging.state_root(), staging.store_dir(store)] {
        let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "{} must be 0700, got {mode:04o}",
            dir.display()
        );
    }
}

// ── A — durability across sessions ───────────────────────────────────────────

/// Two `mur run` invocations, two session workdirs, one store: the second session searches back
/// and resolves records the first one appended and never saw again.
///
/// This is what shows the two halves agree on the preopen name. The corpus writes to the relative
/// guest path `state/corpus.jsonl` and the runtime mounts the store at the preopen `state`; if
/// those two literals disagreed, the first session's append would either fail or land in the
/// workdir, and the second session — a different directory entirely — would find nothing.
#[test]
#[ignore = "requires a default-artifacts checkout; set MURMUR_DEFAULT_ARTIFACTS_DIR"]
fn corpus_records_survive_into_a_second_session() {
    let Some(checkout) = common::default_artifacts_dir() else {
        eprintln!(
            "[SKIP] corpus_records_survive_into_a_second_session: set \
             MURMUR_DEFAULT_ARTIFACTS_DIR to a default-artifacts checkout"
        );
        return;
    };
    let Some(staging) = stage(
        &checkout,
        "corpus-proof-capsule",
        Some("\n        store: corpus-proof"),
    ) else {
        eprintln!(
            "[SKIP] corpus_records_survive_into_a_second_session: the corpus component could not \
             be built from {}",
            checkout.display()
        );
        return;
    };

    let config = staging.write_operator_config("corpus-proof");

    // Session one: two appends the session never reads back.
    let first_server = common::ScriptedServer::start(vec![
        tool_use_response(
            "call-append-1",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "a kestrel hunts over the estuary at dawn"},
            }),
        ),
        tool_use_response(
            "call-append-2",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "a heron waits in the estuary shallows"},
            }),
        ),
        end_turn_response("recorded both notes"),
    ]);
    write_manifest(
        staging.project.path(),
        "corpus-proof-capsule",
        &first_server.endpoint,
        Some("\n        store: corpus-proof"),
    );
    let first_workdir = run_session(&staging.home, &staging.manifest(), "Record two notes.");

    let first_requests = first_server.requests();
    let mut ids = Vec::new();
    for tool_id in ["call-append-1", "call-append-2"] {
        let response = corpus_response(&first_requests, tool_id);
        assert_eq!(response["ok"], json!(true), "{tool_id}: {response}");
        let id = response["id"].as_str().unwrap_or_default().to_string();
        assert!(!id.is_empty(), "{tool_id} must mint an id: {response}");
        ids.push(id);
    }
    assert_eq!(staging.corpus_lines("corpus-proof").len(), 2);

    // Session two: a second process, a second workdir, the same store.
    let second_server = common::ScriptedServer::start(vec![
        tool_use_response(
            "call-search",
            json!({"operation": "search", "query": "estuary"}),
        ),
        tool_use_response("call-get", json!({"operation": "get", "id": ids[0]})),
        end_turn_response("found them"),
    ]);
    write_manifest(
        staging.project.path(),
        "corpus-proof-capsule",
        &second_server.endpoint,
        Some("\n        store: corpus-proof"),
    );
    let second_workdir = run_session(&staging.home, &staging.manifest(), "Find the notes.");

    assert_ne!(
        first_workdir, second_workdir,
        "each launch must get its own session workdir, or durability is not what was proved"
    );

    let second_requests = second_server.requests();
    let search = corpus_response(&second_requests, "call-search");
    assert_eq!(search["ok"], json!(true), "{search}");
    let hit_ids: Vec<&str> = search["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("search must return hits: {search}"))
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    for id in &ids {
        assert!(
            hit_ids.contains(&id.as_str()),
            "search must name {id}, got {hit_ids:?}"
        );
    }

    let got = corpus_response(&second_requests, "call-get");
    assert_eq!(got["ok"], json!(true), "{got}");
    assert_eq!(got["record"]["id"], json!(ids[0]));
    assert_eq!(
        got["record"]["body"]["text"],
        json!("a kestrel hunts over the estuary at dawn"),
        "the body must be the one session one appended: {got}"
    );

    // Reading is not writing: two sessions in, the corpus is still the two lines session one left.
    assert_eq!(staging.corpus_lines("corpus-proof").len(), 2);
    assert_store_is_private(&staging, "corpus-proof");
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        OPERATOR_CONFIG,
        "the operator's configuration is not the agent's to rewrite"
    );
    for workdir in [&first_workdir, &second_workdir] {
        assert_workdir_holds_no_corpus(workdir);
    }
}

// ── B — the default store name ───────────────────────────────────────────────

/// `capabilities.state: {}` with no `store:` lands in the *capsule's* directory, not the
/// artifact's.
///
/// The distinction is the whole reason the store name is read from the operator's manifest entry:
/// were it the artifact's name, every capsule that installed the corpus from a registry would land
/// in one shared `murmur-tool-corpus` directory and read each other's records with no grant on
/// either side.
#[test]
#[ignore = "requires a default-artifacts checkout; set MURMUR_DEFAULT_ARTIFACTS_DIR"]
fn an_undeclared_store_name_defaults_to_the_capsule_name() {
    let Some(checkout) = common::default_artifacts_dir() else {
        eprintln!(
            "[SKIP] an_undeclared_store_name_defaults_to_the_capsule_name: set \
             MURMUR_DEFAULT_ARTIFACTS_DIR to a default-artifacts checkout"
        );
        return;
    };
    let capsule = "corpus-default-capsule";
    let Some(staging) = stage(&checkout, capsule, Some(" {}")) else {
        eprintln!(
            "[SKIP] an_undeclared_store_name_defaults_to_the_capsule_name: the corpus component \
             could not be built from {}",
            checkout.display()
        );
        return;
    };

    staging.write_operator_config(capsule);

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "call-append",
            json!({
                "operation": "append",
                "type": "note",
                "body": {"text": "the default store is the capsule's own name"},
            }),
        ),
        end_turn_response("recorded"),
    ]);
    write_manifest(
        staging.project.path(),
        capsule,
        &server.endpoint,
        Some(" {}"),
    );
    run_session(&staging.home, &staging.manifest(), "Record one note.");

    let response = corpus_response(&server.requests(), "call-append");
    assert_eq!(response["ok"], json!(true), "{response}");
    assert_eq!(staging.corpus_lines(capsule).len(), 1);
    assert!(
        !staging.store_dir(TOOL_NAME).exists(),
        "the default store name is the capsule's, never the artifact's"
    );

    // The same resolution reaches the diagnostic, so an operator can see where their records will
    // land without launching anything.
    assert_eq!(
        common::explain_scope_json(&staging.home, &staging.manifest())["state_stores"],
        json!([{
            "artifact": TOOL_NAME,
            "store": capsule,
            "host_path": staging.store_dir(capsule).display().to_string(),
        }])
    );
}

// ── C — refusal without the grant ────────────────────────────────────────────

/// With no `capabilities:` block on the tool entry, every corpus operation refuses by name and
/// nothing is written anywhere.
///
/// The corpus's own bundled `murmur.yaml` declares `capabilities: state: {}`, and this is where
/// that declaration is shown to grant nothing: the grant comes from the operator's manifest entry
/// or it does not come at all.
///
/// The workdir half of the assertion is the load-bearing one. Without the grant the guest path
/// `state/` resolves inside the workdir preopen, so a corpus that created its own directory would
/// work perfectly and be worthless — a store the agent can rewrite at will.
#[test]
#[ignore = "requires a default-artifacts checkout; set MURMUR_DEFAULT_ARTIFACTS_DIR"]
fn every_operation_refuses_without_the_state_grant() {
    let Some(checkout) = common::default_artifacts_dir() else {
        eprintln!(
            "[SKIP] every_operation_refuses_without_the_state_grant: set \
             MURMUR_DEFAULT_ARTIFACTS_DIR to a default-artifacts checkout"
        );
        return;
    };
    let capsule = "corpus-ungranted-capsule";
    let Some(staging) = stage(&checkout, capsule, None) else {
        eprintln!(
            "[SKIP] every_operation_refuses_without_the_state_grant: the corpus component could \
             not be built from {}",
            checkout.display()
        );
        return;
    };

    // No operator configuration and no state root: a missing grant must be reported as a missing
    // grant, not as a missing configuration, so neither is put in place to be found.
    let calls = [
        (
            "call-append",
            json!({"operation": "append", "type": "note", "body": {"text": "unreachable"}}),
        ),
        ("call-get", json!({"operation": "get", "id": "not-1"})),
        (
            "call-read-recent",
            json!({"operation": "read_recent", "type": "note"}),
        ),
        (
            "call-search",
            json!({"operation": "search", "query": "estuary"}),
        ),
    ];
    let mut responses: Vec<String> = calls
        .iter()
        .map(|(id, input)| tool_use_response(id, input.clone()))
        .collect();
    responses.push(end_turn_response("every call refused"));

    let server = common::ScriptedServer::start(responses);
    write_manifest(staging.project.path(), capsule, &server.endpoint, None);

    // The tool refuses; it neither traps nor fails the session.
    let workdir = run_session(&staging.home, &staging.manifest(), "Try every operation.");

    let requests = server.requests();
    for (tool_id, _) in &calls {
        let response = corpus_response(&requests, tool_id);
        assert_eq!(response["ok"], json!(false), "{tool_id}: {response}");
        assert_eq!(
            response["error_kind"],
            json!("state_unavailable"),
            "{tool_id} must refuse for the missing grant, not for anything found beyond it: \
             {response}"
        );
        let message = response["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("capabilities.state"),
            "{tool_id} must name the declaration the operator has to add: {message}"
        );
    }

    assert!(
        !staging.state_root().exists(),
        "default-deny must bring no part of the state tree into existence"
    );
    assert_workdir_holds_no_corpus(&workdir);
}
