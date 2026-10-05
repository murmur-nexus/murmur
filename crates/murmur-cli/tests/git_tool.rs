//! Integration tests for the native tool dispatch path.
//!
//! These tests exercise it end-to-end against a fixture tool built from
//! `tests/fixtures/native-tool/`: stage_session installs the binary,
//! launch_session dispatches tool calls from a scripted LLM response, and we
//! verify the tool's filesystem effects and the schema the driver sends up.
//!
//! The fixture tool stands in for the real `murmur-tool-git`: these cases are
//! about murmur's side of the contract — dispatch and the inventory →
//! `input_schema` mapping — none of which is about git, and none of which needs
//! an artifact from another checkout. `murmur-tool-git`'s own operations are
//! tested in default-artifacts.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{
    load_runtime_manifest, ArtifactMeta, ArtifactRuntime, ContainmentClass, LocalRegistry,
    Registry, RuntimeType,
};
use serde_json::{json, Value};
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const TOOL_NAME: &str = common::FIXTURE_NATIVE_TOOL_NAME;
const TOOL_VERSION: &str = "0.1.0";

// ── helpers ──────────────────────────────────────────────────────────────────

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

/// Pack the fixture native tool into a `.mur.zip` with the canonical layout.
///
/// The manifest is the fixture crate's own `murmur.yaml`, so `input_schema` and
/// `capabilities` are exactly what a published artifact would carry. There is no
/// inline fallback manifest: a stub with no `input_schema` reads back as an empty
/// object, which makes the schema test pass vacuously.
fn create_fixture_tool_artifact(dir: &Path, binary_path: &Path) -> PathBuf {
    let manifest = common::fixture_native_tool_manifest();
    let manifest_bytes = fs::read(&manifest).unwrap_or_else(|err| {
        panic!(
            "fixture manifest {} must be readable: {err}",
            manifest.display()
        )
    });
    common::create_native_tool_zip(dir, TOOL_NAME, TOOL_VERSION, &manifest_bytes, binary_path)
}

/// Publish the fixture tool artifact to a local registry.
fn publish_fixture_tool(home: &TempDir, artifact_path: &Path) {
    let registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let bytes = fs::read(artifact_path).unwrap();
    let meta = ArtifactMeta {
        name: TOOL_NAME.to_string(),
        version: TOOL_VERSION.to_string(),
        runtime: RuntimeType::Native,
        artifact_runtime: "native".to_string(),
        platforms: Vec::new(),
        description: None,
        tags: Vec::new(),
        wit_contracts: None,
    };
    registry.publish(meta, &bytes).unwrap();
}

/// Publish the inference driver plus the fixture tool into a fresh local registry.
///
/// Returns the fixture binary path, or `None` when it could not be built — the
/// caller turns that into a skip.
fn publish_driver_and_fixture_tool(home: &TempDir, artifact_dir: &Path) -> Option<PathBuf> {
    let binary = common::fixture_native_tool_binary()?;

    let driver = common::create_driver_artifact(
        artifact_dir,
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(home, &driver).success();

    let tool_artifact = create_fixture_tool_artifact(artifact_dir, &binary);
    publish_fixture_tool(home, &tool_artifact);

    Some(binary)
}

/// Stage a capsule session with the fixture tool and an inference driver.
fn stage_fixture_tool_session(
    home: &TempDir,
    project_dir: &Path,
    endpoint: &str,
) -> capsule_runtime::StagedSession {
    // Write capsule manifest
    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            concat!(
                "name: native-tool-capsule\n",
                "version: 0.1.0\n",
                "artifacts:\n",
                "  - name: {driver_name}\n",
                "    version: {driver_version}\n",
                "    runtime: driver\n",
                "    gateway:\n",
                "      endpoint: {endpoint}\n",
                "      api_key: test-key\n",
                "  - name: {tool_name}\n",
                "    version: {tool_version}\n",
                "    runtime: tool\n",
                "capabilities:\n",
                "  network:\n",
                "    allow:\n",
                "      - {endpoint}\n",
                "inference:\n",
                "  transport: http\n",
                "  model: test-model\n",
                "  driver:\n",
                "    artifact: {driver_name}\n",
            ),
            driver_name = DRIVER_NAME,
            driver_version = DRIVER_VERSION,
            tool_name = TOOL_NAME,
            tool_version = TOOL_VERSION,
            endpoint = endpoint,
        ),
    )
    .unwrap();

    let manifest_path = project_dir.join("murmur.yaml");
    let runtime_manifest = load_runtime_manifest(&manifest_path).unwrap();

    let mut allowlisted_tools = HashSet::new();
    let mut requested_artifacts = Vec::new();
    for artifact in &runtime_manifest.artifacts {
        if matches!(artifact.runtime, ArtifactRuntime::Tool) {
            allowlisted_tools.insert(artifact.name.clone());
        }
        requested_artifacts.push(ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
            credentials_file: None,
            manifest_dir: project_dir.to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts: requested_artifacts,
            allowlisted_tools,
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: runtime_manifest.context.clone(),
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: None,
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: None,
            control: None,
            door_authentication: None,
            spawn_grant: None,
            machine_tokens_per_day: None,
            formation_id: None,
            formation_member: None,
        },
    )
    .unwrap()
}

fn tool_use_response(tool_id: &str, name: &str, input: Value) -> String {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{
            "type": "tool_use",
            "id": tool_id,
            "name": name,
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

// ── tests ────────────────────────────────────────────────────────────────────

/// Test 1: dispatch happy path — the tool runs as a subprocess, its filesystem effect
/// lands in the capsule workdir, and its `data` object comes back as the tool result.
///
/// The runtime sends `data || summary` as the tool result text (not the full JSON), so
/// for a successful `create_dir` the text is the data object the fixture tool emitted.
///
/// The binary's CWD is the session workdir, so the relative `path` resolves there.
#[test]
fn native_tool_dispatch_creates_directory() {
    if common::skip_without_host_support("native_tool_dispatch_creates_directory") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!("[SKIP] native_tool_dispatch_creates_directory: fixture tool not available");
        return;
    }

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_create",
            TOOL_NAME,
            json!({
                "operation": "create_dir",
                "path": "./made/here",
                "label": "fixture-label",
            }),
        ),
        end_turn_response("Directory created successfully."),
    ]);

    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Create ./made/here.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected 2 LLM requests, got {}",
        requests.len()
    );

    let tool_result = common::find_tool_result(&requests, "toolu_create")
        .expect("tool_result block should exist in second request");
    let result_text = common::extract_result_text(&tool_result);

    assert!(
        result_text.contains("made/here"),
        "tool result should contain the created path; got:\n{result_text}"
    );
    assert!(
        result_text.contains("fixture-label"),
        "tool result should contain the label; got:\n{result_text}"
    );

    // The real side effect, on disk where the tool's CWD put it.
    let created = workdir.join("made").join("here");
    assert!(created.is_dir(), "directory should exist at {created:?}");
    assert_eq!(
        fs::read_to_string(created.join("label.txt")).unwrap(),
        "fixture-label"
    );
}

/// Test 2: a tool that reports `status: failed` surfaces its summary as the tool result,
/// and its refusal leaves the conflicting path untouched.
///
/// For a failure the fixture tool emits a null `data`, so the runtime falls back to
/// `summary` as the tool result text.
#[test]
fn native_tool_dispatch_reports_failure_for_existing_path() {
    if common::skip_without_host_support("native_tool_dispatch_reports_failure_for_existing_path") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!(
            "[SKIP] native_tool_dispatch_reports_failure_for_existing_path: fixture tool \
             not available"
        );
        return;
    }

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_conflict",
            TOOL_NAME,
            json!({
                "operation": "create_dir",
                "path": "./occupied",
                "label": "should-not-be-written",
            }),
        ),
        end_turn_response("Got an error, the path already exists."),
    ]);

    // Stage first to learn the workdir, then occupy the target path inside it.
    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    let occupied = workdir.join("occupied");
    fs::create_dir_all(&occupied).unwrap();
    fs::write(occupied.join("pre-existing.txt"), "untouched\n").unwrap();
    fs::write(workdir.join("task.md"), "Try to create ./occupied.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = common::find_tool_result(&requests, "toolu_conflict")
        .expect("tool_result block should exist");
    let result_text = common::extract_result_text(&tool_result);

    assert!(
        result_text.contains("already exists"),
        "error should mention 'already exists'; got:\n{result_text}"
    );

    // The refusal must not have written anything into the occupied directory.
    assert!(
        !occupied.join("label.txt").exists(),
        "a refused create_dir must not write its label"
    );
    assert_eq!(
        fs::read_to_string(occupied.join("pre-existing.txt")).unwrap(),
        "untouched\n"
    );
}

/// Test 3: an operation that reads the filesystem returns structured entries through
/// dispatch, addressed by a `path` relative to the tool's CWD (the session workdir).
#[test]
fn native_tool_dispatch_lists_entries() {
    if common::skip_without_host_support("native_tool_dispatch_lists_entries") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!("[SKIP] native_tool_dispatch_lists_entries: fixture tool not available");
        return;
    }

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_list",
            TOOL_NAME,
            json!({
                "operation": "list_entries",
                "path": "./listing",
            }),
        ),
        end_turn_response("Listed the directory."),
    ]);

    // Stage first to learn the workdir path, then populate the directory inside it.
    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    let listing = workdir.join("listing");
    fs::create_dir_all(&listing).unwrap();
    fs::write(listing.join("alpha.txt"), "a\n").unwrap();
    fs::write(listing.join("beta.txt"), "b\n").unwrap();
    fs::write(workdir.join("task.md"), "List ./listing.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = common::find_tool_result(&requests, "toolu_list").expect("tool_result");
    let result_text = common::extract_result_text(&tool_result);

    assert!(
        result_text.contains("alpha.txt") && result_text.contains("beta.txt"),
        "result should list both entries; got:\n{result_text}"
    );
}

/// Test 4: the same read operation addressed by `repo` instead of `path`.
///
/// `repo` is the field the model has to learn about from the schema (test 6), so this
/// keeps a dispatch case that actually travels through it rather than through `path`.
#[test]
fn native_tool_dispatch_lists_entries_with_repo_base() {
    if common::skip_without_host_support("native_tool_dispatch_lists_entries_with_repo_base") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!(
            "[SKIP] native_tool_dispatch_lists_entries_with_repo_base: fixture tool not available"
        );
        return;
    }

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_repo_list",
            TOOL_NAME,
            json!({
                "operation": "list_entries",
                "repo": "./base",
            }),
        ),
        end_turn_response("Listed the base directory."),
    ]);

    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    let base = workdir.join("base");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("gamma.txt"), "g\n").unwrap();
    fs::write(workdir.join("task.md"), "List ./base.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = common::find_tool_result(&requests, "toolu_repo_list").expect("tool_result");
    let result_text = common::extract_result_text(&tool_result);

    assert!(
        result_text.contains("gamma.txt"),
        "result should include gamma.txt; got:\n{result_text}"
    );
}

/// Test 5: an explicit `repo` outside the capsule workdir drives the operation, rather
/// than the tool's CWD.
///
/// The target directory lives in a separate temp dir the capsule knows nothing about, so
/// the effect can only land there if `repo` was honoured.
#[test]
fn native_tool_dispatch_with_explicit_repo() {
    if common::skip_without_host_support("native_tool_dispatch_with_explicit_repo") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!("[SKIP] native_tool_dispatch_with_explicit_repo: fixture tool not available");
        return;
    }

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_explicit",
            TOOL_NAME,
            json!({
                "operation": "create_dir",
                "repo": outside.path().to_str().unwrap(),
                "path": "made-outside",
                "label": "explicit-repo",
            }),
        ),
        end_turn_response("Directory created with an explicit repo."),
    ]);

    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(
        workdir.join("task.md"),
        "Create a directory with an explicit repo.",
    )
    .unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected 2 LLM requests, got {}",
        requests.len()
    );

    let tool_result = common::find_tool_result(&requests, "toolu_explicit")
        .expect("tool_result block should exist");
    let result_text = common::extract_result_text(&tool_result);

    assert!(
        result_text.contains("made-outside"),
        "tool result should mention the created directory; got:\n{result_text}"
    );

    let created = outside.path().join("made-outside");
    assert!(created.is_dir(), "directory should exist at {created:?}");
    assert_eq!(
        fs::read_to_string(created.join("label.txt")).unwrap(),
        "explicit-repo"
    );
    assert!(
        !workdir.join("made-outside").exists(),
        "with an explicit repo nothing should have been created relative to the CWD"
    );
}

/// Test 6: the tool manifest's `input_schema` reaches the model as `input_schema`.
///
/// The schema is read from the artifact zip's murmur.yaml, converted by
/// build_tool_inventory, serialised by the Anthropic driver into `input_schema`, and sent
/// to the model in the first API request. Without `repo` in the schema the model never
/// passes it, and the tool silently falls back to CWD discovery.
///
/// The assertion is on named properties, not on the presence of `input_schema`: a manifest
/// with no schema reads back as an empty object, which satisfies any softer assertion.
#[test]
fn native_tool_schema_includes_repo_field() {
    if common::skip_without_host_support("native_tool_schema_includes_repo_field") {
        return;
    }
    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    if publish_driver_and_fixture_tool(&home, artifact_dir.path()).is_none() {
        eprintln!("[SKIP] native_tool_schema_includes_repo_field: fixture tool not available");
        return;
    }

    // One end_turn response is enough — we only need the first request to the model,
    // which carries the tools array.
    let server = common::ScriptedServer::start(vec![end_turn_response("done")]);

    let staged = stage_fixture_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "no-op").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert!(!requests.is_empty(), "expected at least one LLM request");

    let tools = requests[0]
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("first request should contain a tools array");

    let tool = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(TOOL_NAME))
        .unwrap_or_else(|| panic!("{TOOL_NAME} should appear in the tools array: {tools:?}"));

    // The Anthropic driver maps inventory `parameters` → `input_schema` in the API call.
    let properties = tool
        .get("input_schema")
        .and_then(|s| s.get("properties"))
        .expect("tool should have input_schema.properties");

    assert!(
        properties.get("repo").is_some(),
        "'repo' must be a named property in the tool schema so the model knows to pass it; \
         got properties: {properties}"
    );
    assert!(
        properties.get("operation").is_some(),
        "schema must include 'operation'"
    );
    assert!(
        properties.get("path").is_some(),
        "schema must include 'path'"
    );
    assert!(
        properties.get("label").is_some(),
        "schema must include 'label'"
    );
}
