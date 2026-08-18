//! Integration tests for the native tool dispatch path.
//!
//! These tests exercise it end-to-end against a fixture tool built from
//! `tests/fixtures/native-tool/`: stage_session installs the binary,
//! launch_session dispatches tool calls from a scripted LLM response, and we
//! verify the tool's filesystem effects and the schema the driver sends up.
//!
//! The fixture tool stands in for the real `murmur-tool-git`: these cases are
//! about murmur's side of the contract — dispatch, the inventory →
//! `input_schema` mapping, and a native tool self-enforcing a path allow list —
//! none of which is about git, and none of which needs an artifact from another
//! checkout.
//!
//! The `slice2_*` block is the exception: those are artifact tests for
//! `murmur-tool-git`'s own operations, and they build that binary out of the
//! `default-artifacts` checkout named by `MURMUR_DEFAULT_ARTIFACTS_DIR`. They
//! skip when it is unset.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

/// Locate or compile the murmur-tool-git binary in the `default-artifacts`
/// checkout named by `MURMUR_DEFAULT_ARTIFACTS_DIR`.
///
/// Only the `slice2_*` block below uses this. Every other test in this file runs
/// against the local fixture tool instead — see `common::fixture_native_tool_binary`.
///
/// `None` — for the caller to turn into a skip — when the variable is unset, when
/// it names a directory that does not exist, or when the build produces no binary.
fn git_tool_binary() -> Option<PathBuf> {
    let default_artifacts = common::default_artifacts_dir()?;

    if !default_artifacts.exists() {
        eprintln!(
            "[git_tool test] MURMUR_DEFAULT_ARTIFACTS_DIR names {default_artifacts:?}, which does not exist"
        );
        return None;
    }

    let binary_path = default_artifacts
        .join("target")
        .join("release")
        .join("murmur-tool-git");

    if !binary_path.exists() {
        eprintln!("[git_tool test] binary not found, building…");
        let status = Command::new("cargo")
            .args(["build", "-p", "murmur-tool-git", "--release"])
            .current_dir(&default_artifacts)
            .status()
            .ok()?;
        if !status.success() {
            eprintln!("[git_tool test] cargo build failed");
            return None;
        }
    }

    // A build can exit 0 without producing the binary, so the caller gets a clean skip
    // rather than a path that fails to spawn inside a test body.
    if !binary_path.exists() {
        eprintln!("[git_tool test] binary missing at {binary_path:?} after a successful build");
        return None;
    }
    Some(binary_path)
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

/// Initialize a git repo with an initial commit and return the path.
///
/// Only the `slice2_*` block below needs this; the fixture tool is not a git tool.
fn init_git_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();

    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git should run");
        assert!(status.success(), "git {:?} failed", args);
    };

    // `-b main` pins the initial branch: several tests below check out `main` by name, and a
    // bare `git init` follows the host's `init.defaultBranch`, which is still `master` on a
    // stock git. Without this the checkouts fail, and because they are not status-checked the
    // test carries on from the wrong branch and fails somewhere unrelated.
    run(&["init", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "initial"]);

    repo
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
                "  - name: {tool_name}\n",
                "    version: {tool_version}\n",
                "    runtime: tool\n",
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
            capabilities: artifact.capabilities.clone(),
        });
    }

    let local_registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    stage_session(
        std::sync::Arc::new(local_registry),
        StageRequest {
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

fn find_tool_result(requests: &[Value], tool_id: &str) -> Option<Value> {
    for req in requests {
        let messages = req.get("messages")?.as_array()?;
        for msg in messages {
            if msg.get("role")?.as_str()? != "user" {
                continue;
            }
            let content = msg.get("content")?.as_array()?;
            for block in content {
                if block.get("type")?.as_str()? == "tool_result"
                    && block.get("tool_use_id")?.as_str()? == tool_id
                {
                    return Some(block.clone());
                }
            }
        }
    }
    None
}

/// Extract the tool result text from a tool_result block.
///
/// The runtime sends `data || summary` as the content text (not the full JSON blob).
/// The Anthropic driver formats this as either a plain string or an array of text blocks.
fn extract_result_text(tool_result: &Value) -> String {
    // content may be a plain string or an array of {type: text, text: ...} blocks
    if let Some(text) = tool_result.get("content").and_then(|c| c.as_str()) {
        return text.to_string();
    }
    tool_result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

/// Invoke a native tool binary directly (bypassing the capsule runtime) with a JSON
/// operation as input and optional environment variable overrides.
///
/// Used for allow-list tests to avoid `std::env::set_var` cross-test contamination.
fn invoke_native_tool_directly(binary: &Path, data: Value, extra_env: &[(&str, &str)]) -> Value {
    let envelope = json!({
        "data": data,
        "log_path": null,
    });
    let stdin_bytes = serde_json::to_vec(&envelope).unwrap();

    let mut cmd = Command::new(binary);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("MURMUR_FILESYSTEM_ALLOW"); // start clean regardless of ambient env

    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(&stdin_bytes).unwrap();
    let out = child.wait_with_output().unwrap();

    serde_json::from_slice(&out.stdout).unwrap_or_else(
        |_| json!({"status": "error", "summary": "binary produced invalid JSON output"}),
    )
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

    let tool_result = find_tool_result(&requests, "toolu_create")
        .expect("tool_result block should exist in second request");
    let result_text = extract_result_text(&tool_result);

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

    let tool_result =
        find_tool_result(&requests, "toolu_conflict").expect("tool_result block should exist");
    let result_text = extract_result_text(&tool_result);

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

    let tool_result = find_tool_result(&requests, "toolu_list").expect("tool_result");
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("alpha.txt") && result_text.contains("beta.txt"),
        "result should list both entries; got:\n{result_text}"
    );
}

/// Test 4: the same read operation addressed by `repo` instead of `path`.
///
/// `repo` is the field the model has to learn about from the schema (test 8), so this
/// keeps a dispatch case that actually travels through it rather than through `path`.
#[test]
fn native_tool_dispatch_lists_entries_with_repo_base() {
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

    let tool_result = find_tool_result(&requests, "toolu_repo_list").expect("tool_result");
    let result_text = extract_result_text(&tool_result);

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

    let tool_result =
        find_tool_result(&requests, "toolu_explicit").expect("tool_result block should exist");
    let result_text = extract_result_text(&tool_result);

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

/// Test 6: when MURMUR_FILESYSTEM_ALLOW is set and the `repo` path is outside every
/// allowed prefix, the tool refuses and says which mechanism refused it.
///
/// Uses direct binary invocation to set the env var without cross-test contamination.
/// The runtime does not enforce `filesystem.allow` — a native tool that wants that
/// boundary self-enforces it, and this is the coverage that the pattern works.
#[test]
fn native_tool_repo_not_in_allow_list() {
    let Some(binary) = common::fixture_native_tool_binary() else {
        eprintln!("[SKIP] native_tool_repo_not_in_allow_list: fixture tool not available");
        return;
    };

    let target = TempDir::new().unwrap();
    // Allow list points at a completely different temp dir — the repo is not under it.
    let other = TempDir::new().unwrap();
    let allow_val = other.path().to_str().unwrap().to_string();

    let result = invoke_native_tool_directly(
        &binary,
        json!({
            "operation": "create_dir",
            "repo": target.path().to_str().unwrap(),
            "path": "blocked",
        }),
        &[("MURMUR_FILESYSTEM_ALLOW", &allow_val)],
    );

    assert_eq!(
        result["status"], "failed",
        "should fail when repo is outside the allow list; got: {result:?}"
    );
    let summary = result["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("filesystem.allow"),
        "message should reference filesystem.allow; got: {summary}"
    );
    assert!(
        !target.path().join("blocked").exists(),
        "a refused operation must not touch the filesystem"
    );
}

/// Test 7: when MURMUR_FILESYSTEM_ALLOW is set and the `repo` path IS within an allowed
/// prefix, the operation succeeds and its on-disk effect is really there.
#[test]
fn native_tool_repo_in_allow_list() {
    let Some(binary) = common::fixture_native_tool_binary() else {
        eprintln!("[SKIP] native_tool_repo_in_allow_list: fixture tool not available");
        return;
    };

    let parent = TempDir::new().unwrap();
    let target = parent.path().join("repo");
    fs::create_dir_all(&target).unwrap();

    // Allow list contains the parent of the target — the repo IS under this prefix.
    let allow_val = parent.path().to_str().unwrap().to_string();

    let result = invoke_native_tool_directly(
        &binary,
        json!({
            "operation": "create_dir",
            "repo": target.to_str().unwrap(),
            "path": "allowed",
            "label": "in-allow-list",
        }),
        &[("MURMUR_FILESYSTEM_ALLOW", &allow_val)],
    );

    assert_eq!(
        result["status"], "passed",
        "should succeed when repo is within the allow list; got: {result:?}"
    );
    let created = target.join("allowed");
    assert!(created.is_dir(), "directory should exist at {created:?}");
    assert_eq!(
        fs::read_to_string(created.join("label.txt")).unwrap(),
        "in-allow-list"
    );
}

/// Test 8: the tool manifest's `input_schema` reaches the model as `input_schema`.
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

// ── Slice 2 helpers ───────────────────────────────────────────────────────────

/// Stage a file, commit it, and return the commit hash.
fn make_commit(repo: &Path, filename: &str, content: &str, message: &str) -> String {
    fs::write(repo.join(filename), content).unwrap();
    let run = |args: &[&str]| {
        let s = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(s.success(), "git {:?} failed", args);
    };
    run(&["add", filename]);
    run(&["commit", "-m", message]);
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Convenience wrapper: invoke binary with no extra env vars.
fn invoke_tool(binary: &Path, data: Value) -> Value {
    invoke_native_tool_directly(binary, data, &[])
}

// ── Slice 2 tests: COMMITS ────────────────────────────────────────────────────

#[test]
fn slice2_commit_success() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_commit_success");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    fs::write(repo.join("new.txt"), "content\n").unwrap();
    Command::new("git")
        .args(["-C", repo_s, "add", "new.txt"])
        .status()
        .unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "commit",
            "repo": repo_s,
            "message": "add new.txt",
        }),
    );

    assert_eq!(result["ok"], true, "commit should succeed; got: {result:?}");
    assert!(
        !result["hash"].as_str().unwrap_or("").is_empty(),
        "hash must be populated"
    );
    assert!(
        !result["short_hash"].as_str().unwrap_or("").is_empty(),
        "short_hash must be populated"
    );
    assert_eq!(
        result["subject"], "add new.txt",
        "subject must match commit message"
    );
}

#[test]
fn slice2_commit_nothing() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_commit_nothing");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());

    // Nothing staged — should fail
    let result = invoke_tool(
        &binary,
        json!({
            "operation": "commit",
            "repo": repo.to_str().unwrap(),
            "message": "should fail",
        }),
    );

    assert_eq!(
        result["ok"], false,
        "commit with nothing staged should fail"
    );
    assert_eq!(result["error_kind"], "nothing_to_commit");
}

#[test]
fn slice2_commit_allow_empty() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_commit_allow_empty");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "commit",
            "repo": repo.to_str().unwrap(),
            "message": "empty commit",
            "allow_empty": true,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "allow_empty commit on clean tree should succeed; got: {result:?}"
    );
    assert!(!result["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(result["subject"], "empty commit");
}

#[test]
fn slice2_cherry_pick_success() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_cherry_pick_success");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create feature branch and make a unique commit on it
    Command::new("git")
        .args(["-C", repo_s, "checkout", "-b", "feature"])
        .status()
        .unwrap();
    let pick_hash = make_commit(&repo, "cherry.txt", "cherry content\n", "add cherry.txt");

    // Switch back to main
    Command::new("git")
        .args(["-C", repo_s, "checkout", "main"])
        .status()
        .unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "cherry_pick",
            "repo": repo_s,
            "ref": pick_hash,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "cherry-pick should succeed; got: {result:?}"
    );
    assert!(!result["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(result["subject"], "add cherry.txt");
    assert!(
        repo.join("cherry.txt").exists(),
        "cherry.txt should exist in main after pick"
    );
}

#[test]
fn slice2_cherry_pick_conflict() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_cherry_pick_conflict");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // feature: modify README.md one way and commit
    Command::new("git")
        .args(["-C", repo_s, "checkout", "-b", "feature"])
        .status()
        .unwrap();
    let pick_hash = make_commit(&repo, "README.md", "feature version\n", "feature change");

    // main: modify README.md a different way and commit (creates divergence)
    Command::new("git")
        .args(["-C", repo_s, "checkout", "main"])
        .status()
        .unwrap();
    make_commit(&repo, "README.md", "main version\n", "main change");

    // cherry-pick feature commit onto main → conflict
    let result = invoke_tool(
        &binary,
        json!({
            "operation": "cherry_pick",
            "repo": repo_s,
            "ref": pick_hash,
        }),
    );

    assert_eq!(
        result["ok"], false,
        "conflicting cherry-pick should fail; got: {result:?}"
    );
    assert_eq!(result["error_kind"], "conflict");

    // Repo must still be in conflict state (not auto-aborted)
    let status_out = Command::new("git")
        .args(["-C", repo_s, "status", "--porcelain=v1"])
        .output()
        .unwrap();
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("UU") || status_text.contains("AA"),
        "repo should be in conflict state after failed cherry-pick; status:\n{status_text}"
    );
}

// ── Slice 2 tests: BRANCHES ───────────────────────────────────────────────────

#[test]
fn slice2_branch_list() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_branch_list");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "list",
        }),
    );

    assert_eq!(
        result["ok"], true,
        "branch list should succeed; got: {result:?}"
    );
    let branches = result["branches"]
        .as_array()
        .expect("branches must be an array");
    let current = branches.iter().find(|b| b["current"] == true);
    assert!(current.is_some(), "one branch should be flagged as current");
    let current_name = current.unwrap()["name"].as_str().unwrap_or("");
    assert!(
        !current_name.is_empty(),
        "current branch name must not be empty"
    );
}

#[test]
fn slice2_branch_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_branch_create");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "create",
            "name": "new-feature",
        }),
    );
    assert_eq!(
        result["ok"], true,
        "branch create should succeed; got: {result:?}"
    );
    assert_eq!(result["name"], "new-feature");

    // Verify branch appears in list
    let list = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "list",
        }),
    );
    let branches = list["branches"].as_array().unwrap();
    assert!(
        branches.iter().any(|b| b["name"] == "new-feature"),
        "new-feature should appear in branch list"
    );
}

#[test]
fn slice2_branch_delete() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_branch_delete");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create a branch (same content as main → will be "merged")
    invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "create",
            "name": "to-delete",
        }),
    );

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "delete",
            "name": "to-delete",
        }),
    );
    assert_eq!(
        result["ok"], true,
        "branch delete should succeed; got: {result:?}"
    );
    assert_eq!(result["name"], "to-delete");

    // Verify branch is gone
    let list = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "list",
        }),
    );
    let branches = list["branches"].as_array().unwrap();
    assert!(
        !branches.iter().any(|b| b["name"] == "to-delete"),
        "to-delete should be gone from branch list"
    );
}

#[test]
fn slice2_branch_delete_not_merged() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_branch_delete_not_merged");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create branch and add a commit not in main → not merged
    Command::new("git")
        .args(["-C", repo_s, "checkout", "-b", "unmerged"])
        .status()
        .unwrap();
    make_commit(&repo, "extra.txt", "data\n", "unmerged commit");
    Command::new("git")
        .args(["-C", repo_s, "checkout", "main"])
        .status()
        .unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "branch",
            "repo": repo_s,
            "subcommand": "delete",
            "name": "unmerged",
        }),
    );

    assert_eq!(result["ok"], false, "deleting unmerged branch should fail");
    assert_eq!(result["error_kind"], "not_merged");
}

// ── Slice 2 tests: CHECKOUT / SWITCH ─────────────────────────────────────────

#[test]
fn slice2_checkout_switch_branch() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_checkout_switch_branch");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    Command::new("git")
        .args(["-C", repo_s, "branch", "other"])
        .status()
        .unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "checkout",
            "repo": repo_s,
            "ref": "other",
        }),
    );

    assert_eq!(
        result["ok"], true,
        "checkout should succeed; got: {result:?}"
    );
    assert_eq!(result["branch"], "other");
    assert_eq!(result["detached"], false);
}

#[test]
fn slice2_checkout_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_checkout_create");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "checkout",
            "repo": repo_s,
            "ref": "fresh-branch",
            "create": true,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "checkout -b should succeed; got: {result:?}"
    );
    assert_eq!(result["branch"], "fresh-branch");
    assert_eq!(result["detached"], false);
}

#[test]
fn slice2_checkout_dirty() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_checkout_dirty");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create `other` branch where README.md differs from main
    Command::new("git")
        .args(["-C", repo_s, "checkout", "-b", "other"])
        .status()
        .unwrap();
    make_commit(&repo, "README.md", "other branch version\n", "other change");
    Command::new("git")
        .args(["-C", repo_s, "checkout", "main"])
        .status()
        .unwrap();

    // Dirty the working tree: modify README.md locally (unstaged) so it conflicts with `other`
    fs::write(repo.join("README.md"), "local dirty change\n").unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "checkout",
            "repo": repo_s,
            "ref": "other",
        }),
    );

    assert_eq!(
        result["ok"], false,
        "checkout with conflicting dirty tree should fail"
    );
    assert_eq!(result["error_kind"], "dirty_working_tree");
}

#[test]
fn slice2_switch_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_switch_create");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "switch",
            "repo": repo_s,
            "branch": "switched-branch",
            "create": true,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "switch -c should succeed; got: {result:?}"
    );
    assert_eq!(result["branch"], "switched-branch");
}

// ── Slice 2 tests: RESET ──────────────────────────────────────────────────────

#[test]
fn slice2_reset_soft() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_reset_soft");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Get the initial commit hash (parent we'll reset to)
    let parent_hash = {
        let out = Command::new("git")
            .args(["-C", repo_s, "rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    // Add a second commit
    make_commit(&repo, "extra.txt", "data\n", "second commit");

    // Soft reset back to the initial commit
    let result = invoke_tool(
        &binary,
        json!({
            "operation": "reset",
            "repo": repo_s,
            "mode": "soft",
            "ref": &parent_hash,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "reset soft should succeed; got: {result:?}"
    );
    let resolved = result["ref"].as_str().unwrap_or("");
    assert_eq!(
        resolved, parent_hash,
        "HEAD should point to the parent commit after soft reset"
    );

    // With soft reset, changes should be staged (index should still have extra.txt)
    let status_out = Command::new("git")
        .args(["-C", repo_s, "status", "--porcelain=v1"])
        .output()
        .unwrap();
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("extra.txt"),
        "after soft reset, extra.txt should be staged; status:\n{status_text}"
    );
}

#[test]
fn slice2_reset_hard() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] slice2_reset_hard");
            return;
        }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let parent_hash = {
        let out = Command::new("git")
            .args(["-C", repo_s, "rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    make_commit(&repo, "extra.txt", "data\n", "second commit");

    let result = invoke_tool(
        &binary,
        json!({
            "operation": "reset",
            "repo": repo_s,
            "mode": "hard",
            "ref": &parent_hash,
        }),
    );

    assert_eq!(
        result["ok"], true,
        "reset hard should succeed; got: {result:?}"
    );
    assert_eq!(result["ref"].as_str().unwrap_or(""), parent_hash);

    // Hard reset: extra.txt must not exist and working dir must be clean
    assert!(
        !repo.join("extra.txt").exists(),
        "extra.txt should be gone after hard reset"
    );
    let status_out = Command::new("git")
        .args(["-C", repo_s, "status", "--porcelain=v1"])
        .output()
        .unwrap();
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.trim().is_empty(),
        "working directory should be clean after hard reset; status:\n{status_text}"
    );
}
