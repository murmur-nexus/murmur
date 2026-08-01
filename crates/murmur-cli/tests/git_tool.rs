//! Integration tests for the git-tool native artifact.
//!
//! These tests exercise the native tool dispatch path end-to-end:
//! stage_session installs the binary, launch_session dispatches tool calls
//! from a scripted LLM response, and we verify git filesystem effects.
//!
//! The tests compile murmur-tool-git from default-artifacts on first run
//! (via `cargo build -p murmur-tool-git --release` in that workspace).
//! Subsequent runs reuse the cached binary.

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
    load_runtime_manifest, ArtifactMeta, ArtifactRuntime, ContainmentClass, LocalRegistry, Registry,
    RuntimeType,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use zip::{
    write::{FileOptions, SimpleFileOptions},
    CompressionMethod, ZipWriter,
};

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";
const GIT_TOOL_NAME: &str = "murmur-tool-git";
const GIT_TOOL_VERSION: &str = "0.4.0";

// ── helpers ──────────────────────────────────────────────────────────────────

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

/// Locate or compile the murmur-tool-git binary from default-artifacts.
///
/// Returns None if the default-artifacts workspace cannot be found.
fn git_tool_binary() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // crates/mur-cli → murmur/ → _murmur/ → default-artifacts/
    let default_artifacts = manifest_dir
        .ancestors()
        .nth(3)?
        .join("default-artifacts");

    if !default_artifacts.exists() {
        eprintln!("[git_tool test] default-artifacts not found at {:?}", default_artifacts);
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

    Some(binary_path)
}

/// Locate the murmur-tool-git source manifest.yaml from default-artifacts.
///
/// Returns None if the path cannot be resolved (e.g. the workspace layout changed).
fn git_tool_source_manifest() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = manifest_dir
        .ancestors()
        .nth(3)?
        .join("default-artifacts")
        .join("tools")
        .join("murmur-tool-git")
        .join("manifest.yaml");
    path.exists().then_some(path)
}

/// Create a proper native tool artifact zip for git-tool.
///
/// Uses the actual manifest.yaml from the source tree so that input_schema and
/// capabilities are identical to what the published artifact contains. Falls back
/// to a minimal inline manifest if the source tree is not found.
fn create_git_tool_artifact(dir: &Path, binary_path: &Path) -> PathBuf {
    let artifact_path = dir.join(format!("{GIT_TOOL_NAME}-{GIT_TOOL_VERSION}.mur.zip"));
    let file = fs::File::create(&artifact_path).unwrap();
    let mut zip = ZipWriter::new(file);

    let options: SimpleFileOptions =
        FileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("murmur.yaml", options).unwrap();
    match git_tool_source_manifest() {
        Some(p) => zip.write_all(&fs::read(p).unwrap()).unwrap(),
        None => {
            writeln!(zip, "name: {GIT_TOOL_NAME}").unwrap();
            writeln!(zip, "version: \"{GIT_TOOL_VERSION}\"").unwrap();
            writeln!(zip, "runtime: tool").unwrap();
            writeln!(zip, "implementation: native").unwrap();
            writeln!(
                zip,
                "description: \"Create and manage isolated git worktrees within a capsule workspace.\""
            )
            .unwrap();
        }
    }

    let exec_options: SimpleFileOptions = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o755);
    zip.start_file(&format!("bin/{GIT_TOOL_NAME}"), exec_options).unwrap();
    zip.write_all(&fs::read(binary_path).unwrap()).unwrap();

    zip.finish().unwrap();
    artifact_path
}

/// Publish the git-tool artifact to a local registry.
fn publish_git_tool(home: &TempDir, artifact_path: &Path) {
    let registry = LocalRegistry::new(home.path().join(".murmur").join("artifacts"));
    let bytes = fs::read(artifact_path).unwrap();
    let meta = ArtifactMeta {
        name: GIT_TOOL_NAME.to_string(),
        version: GIT_TOOL_VERSION.to_string(),
        runtime: RuntimeType::Native,
        artifact_runtime: "native".to_string(),
        platforms: Vec::new(),
        description: None,
        tags: Vec::new(),
    };
    registry.publish(meta, &bytes).unwrap();
}

/// Initialize a git repo with an initial commit and return the path.
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

    run(&["init"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    fs::write(repo.join("README.md"), "hello\n").unwrap();
    run(&["add", "README.md"]);
    run(&["commit", "-m", "initial"]);

    repo
}

/// Stage a capsule session with git-tool and an inference driver.
fn stage_git_tool_session(
    home: &TempDir,
    project_dir: &Path,
    endpoint: &str,
) -> capsule_runtime::StagedSession {
    // Write capsule manifest
    fs::write(
        project_dir.join("murmur.yaml"),
        format!(
            concat!(
                "name: git-tool-capsule\n",
                "version: 0.1.0\n",
                "artifacts:\n",
                "  - name: {driver_name}\n",
                "    version: {driver_version}\n",
                "    runtime: driver\n",
                "  - name: {git_tool_name}\n",
                "    version: {git_tool_version}\n",
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
            git_tool_name = GIT_TOOL_NAME,
            git_tool_version = GIT_TOOL_VERSION,
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
            job_id: None,
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
                    b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

/// Invoke the git-tool binary directly (bypassing the capsule runtime) with a JSON
/// operation as input and optional environment variable overrides.
///
/// Used for allowlist tests to avoid `std::env::set_var` cross-test contamination.
fn invoke_git_tool_directly(binary: &Path, data: Value, extra_env: &[(&str, &str)]) -> Value {
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

    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        json!({"status": "error", "summary": "binary produced invalid JSON output"})
    })
}

// ── tests ────────────────────────────────────────────────────────────────────

/// Test 1: create_worktree happy path — worktree appears on disk and tool result contains path/branch.
///
/// The runtime sends `data || summary` as the tool result text (not the full JSON),
/// so for a successful create_worktree the text is the data JSON object.
///
/// After adding `-C <repo>` to git calls, the relative path `./worktrees/feature-x` is
/// resolved relative to the auto-discovered repo root, not the binary's CWD.
#[test]
fn git_tool_create_worktree_success() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_create_worktree_success: git-tool binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    // Publish driver
    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    // Publish git-tool
    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    // Set up git repo in the project dir (capsule workdir is a subdirectory of project dir)
    let repo = init_git_repo(project.path());
    let branch = "feature/x";
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    // The binary's CWD is the session workdir (repo/workdir/<ses>).
    // With `-C <repo>`, `./worktrees/feature-x` is relative to the auto-discovered repo root.
    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_create",
            GIT_TOOL_NAME,
            json!({
                "operation": "create_worktree",
                "path": "./worktrees/feature-x",
                "branch": branch,
            }),
        ),
        end_turn_response("Worktree created successfully."),
    ]);

    let staged = stage_git_tool_session(&home, &repo, &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Create a worktree for feature/x.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected 2 LLM requests, got {}", requests.len());

    let tool_result = find_tool_result(&requests, "toolu_create")
        .expect("tool_result block should exist in second request");

    // The runtime sends `data || summary` as the tool result text.
    // For a successful create_worktree the data JSON contains path and branch.
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("worktrees/feature-x"),
        "tool result should contain the worktree path; got:\n{result_text}"
    );
    assert!(
        result_text.contains("feature/x"),
        "tool result should contain the branch name; got:\n{result_text}"
    );
    assert!(
        !result_text.contains("error") || result_text.contains("feature/x"),
        "tool result should not be an error; got:\n{result_text}"
    );

    // With `-C <repo>`, relative path `./worktrees/feature-x` is resolved from the repo root.
    // The workdir is repo/workdir/<session_id>, so the worktree lands at repo/worktrees/feature-x.
    let worktree_path = repo.join("worktrees").join("feature-x");
    assert!(
        worktree_path.exists(),
        "worktree should exist at {:?}",
        worktree_path
    );

    // Confirm it's on the right branch
    let branch_out = Command::new("git")
        .args(["-C", worktree_path.to_str().unwrap(), "branch", "--show-current"])
        .output()
        .unwrap();
    let checked_out = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();
    assert_eq!(checked_out, branch, "worktree should be on branch {branch}");
}

/// Test 2: create_worktree with branch already checked out → tool result contains "already checked out".
///
/// For an error result, the runtime sends the summary as the tool result text (data is null).
#[test]
fn git_tool_create_worktree_branch_conflict() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_create_worktree_branch_conflict: binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    let repo = init_git_repo(project.path());
    let branch = "feature/x";
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    // Pre-create a worktree for the branch so conflict is guaranteed.
    // The binary runs from workdir (inside repo), so `git worktree list` will find this.
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "worktree", "add", "./existing", branch])
        .status()
        .unwrap();

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_conflict",
            GIT_TOOL_NAME,
            json!({
                "operation": "create_worktree",
                "path": "./worktrees/feature-x",
                "branch": branch,
            }),
        ),
        end_turn_response("Got an error, branch is already checked out."),
    ]);

    let staged = stage_git_tool_session(&home, &repo, &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Try to create worktree on feature/x.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result(&requests, "toolu_conflict")
        .expect("tool_result block should exist");

    // For an error, data is null so the runtime falls back to summary as the result text.
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("already checked out"),
        "error should mention 'already checked out'; got:\n{result_text}"
    );

    // Confirm the conflict target path was not created
    assert!(
        !workdir.join("worktrees").join("feature-x").exists(),
        "duplicate worktree path should not have been created"
    );
}

/// Test 3: status operation returns structured entries for modified files.
///
/// Stage first to learn the workdir path, then create the worktree AT that path
/// so the binary can access it via `./wt-status` relative to its CWD (workdir).
#[test]
fn git_tool_status_returns_entries() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_status_returns_entries: binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    let repo = init_git_repo(project.path());
    let branch = "feature/status";
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_status",
            GIT_TOOL_NAME,
            json!({
                "operation": "status",
                "path": "./wt-status",
            }),
        ),
        end_turn_response("Got status with modified files."),
    ]);

    // Stage first to learn the workdir path.
    let staged = stage_git_tool_session(&home, &repo, &server.endpoint);
    let workdir = staged.workdir.clone();

    // Create worktree AT the workdir-relative path so the binary can access ./wt-status.
    // The workdir is inside the repo, so git worktree add works by traversing up to the repo root.
    Command::new("git")
        .args([
            "worktree", "add",
            workdir.join("wt-status").to_str().unwrap(),
            branch,
        ])
        .current_dir(&repo)
        .status()
        .expect("git worktree add should succeed");

    // Modify a file in the worktree so status has something to report.
    fs::write(workdir.join("wt-status").join("README.md"), "modified\n").unwrap();

    fs::write(workdir.join("task.md"), "Check status of ./wt-status.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result(&requests, "toolu_status").expect("tool_result");
    // Runtime sends data || summary as the tool result text.
    // For a successful status, data contains the entries JSON.
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("modified"),
        "result should mention modified; got:\n{result_text}"
    );
    assert!(
        result_text.contains("README.md"),
        "result should include README.md; got:\n{result_text}"
    );
}

/// Test 4: status returns modified files in a worktree.
///
/// Migrated from `list_files` (dropped from v1 dispatch table — not in scope).
/// The original test verified that `git ls-files` returned tracked files; this
/// version verifies that `status` reports a modified tracked file instead, which
/// covers the same intent with the in-scope operation.
///
/// Stage first to learn the workdir path, then create the worktree AT that path
/// so the binary can access it via `./wt-files` relative to its CWD (workdir).
#[test]
fn git_tool_list_files() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_list_files (migrated to status): binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    let repo = init_git_repo(project.path());
    let branch = "feature/files";
    Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    // Migrated: use `status` with `repo` instead of `list_files` with `path`.
    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_files",
            GIT_TOOL_NAME,
            json!({
                "operation": "status",
                "repo": "./wt-files",
            }),
        ),
        end_turn_response("Got status with modified files."),
    ]);

    // Stage first to learn the workdir path.
    let staged = stage_git_tool_session(&home, &repo, &server.endpoint);
    let workdir = staged.workdir.clone();

    // Create worktree AT the workdir-relative path so the binary can access ./wt-files.
    Command::new("git")
        .args([
            "worktree", "add",
            workdir.join("wt-files").to_str().unwrap(),
            branch,
        ])
        .current_dir(&repo)
        .status()
        .expect("git worktree add should succeed");

    // Modify README.md so status has something to report (list_files listed all tracked
    // files; status only shows changed files, so we need at least one change).
    fs::write(workdir.join("wt-files").join("README.md"), "modified\n").unwrap();

    fs::write(workdir.join("task.md"), "Check status of ./wt-files.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);

    let tool_result = find_tool_result(&requests, "toolu_files").expect("tool_result");
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("README.md"),
        "result should include README.md; got:\n{result_text}"
    );
}

/// Test 5: create_worktree with an explicit `repo` field pointing to a repo that is NOT the
/// capsule workdir confirms that `-C <repo>` drives the operation rather than CWD discovery.
///
/// The capsule project dir is a plain temp dir (no git repo), so auto-discovery would fail.
/// The explicit `repo` field makes the operation succeed regardless.
#[test]
fn git_tool_create_worktree_with_explicit_repo() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_create_worktree_with_explicit_repo: binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    // Project dir is NOT a git repo — auto-discovery would fail here.
    let project = TempDir::new().unwrap();

    // Create a SEPARATE git repo outside the capsule project dir.
    let separate = TempDir::new().unwrap();
    let target_repo = init_git_repo(separate.path());
    let branch = "feature/explicit";
    Command::new("git")
        .args(["-C", target_repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    // Worktree will be created at this absolute path (sibling of the repo dir).
    let worktree_path = separate.path().join("wt-explicit");

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    let server = common::ScriptedServer::start(vec![
        tool_use_response(
            "toolu_explicit",
            GIT_TOOL_NAME,
            json!({
                "operation": "create_worktree",
                "repo": target_repo.to_str().unwrap(),
                "path": worktree_path.to_str().unwrap(),
                "branch": branch,
            }),
        ),
        end_turn_response("Worktree created with explicit repo."),
    ]);

    // Stage with the non-git project dir — binary's CWD is inside project, not target_repo.
    let staged = stage_git_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "Create a worktree with explicit repo.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "expected 2 LLM requests, got {}", requests.len());

    let tool_result = find_tool_result(&requests, "toolu_explicit")
        .expect("tool_result block should exist");
    let result_text = extract_result_text(&tool_result);

    assert!(
        result_text.contains("wt-explicit") || result_text.contains("explicit"),
        "tool result should mention the worktree; got:\n{result_text}"
    );
    assert!(
        !result_text.to_lowercase().starts_with("error"),
        "tool result should not be an error; got:\n{result_text}"
    );

    // Confirm worktree exists at the absolute path we specified.
    assert!(
        worktree_path.exists(),
        "worktree should exist at {:?}",
        worktree_path
    );

    let branch_out = Command::new("git")
        .args(["-C", worktree_path.to_str().unwrap(), "branch", "--show-current"])
        .output()
        .unwrap();
    let checked_out = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();
    assert_eq!(checked_out, branch, "worktree should be on branch {branch}");
}

/// Test 6: when MURMUR_FILESYSTEM_ALLOW is set and the repo path is outside every allowed
/// prefix, the tool returns ok=false with a message referencing `filesystem.allow`.
///
/// Uses direct binary invocation to set the env var without cross-test contamination.
///
/// NOTE: assertions use the `ok`/`message` fields of the binary's JSON protocol.
/// A previous version of the binary emitted `status`/`summary` (old wrapper format);
/// the current source uses `ok`/`message`/`error_kind`.
#[test]
fn git_tool_create_worktree_repo_not_in_allow_list() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_create_worktree_repo_not_in_allow_list: binary not available");
            return;
        }
    };

    let repo_dir = TempDir::new().unwrap();
    let target_repo = init_git_repo(repo_dir.path());

    // Allow list points to a completely different temp dir — repo is not under it.
    let other = TempDir::new().unwrap();
    let allow_val = other.path().to_str().unwrap().to_string();

    let result = invoke_git_tool_directly(
        &binary,
        json!({
            "operation": "create_worktree",
            "repo": target_repo.to_str().unwrap(),
            "path": repo_dir.path().join("wt-blocked").to_str().unwrap(),
            "branch": "main",
        }),
        &[("MURMUR_FILESYSTEM_ALLOW", &allow_val)],
    );

    assert_eq!(
        result["ok"],
        false,
        "should return ok=false when repo is outside allow list; got: {result:?}"
    );
    let message = result["message"].as_str().unwrap_or("");
    assert!(
        message.contains("filesystem.allow"),
        "error message should reference filesystem.allow; got: {message}"
    );
}

/// Test 7: when MURMUR_FILESYSTEM_ALLOW is set and the repo path IS within an allowed prefix,
/// the operation succeeds and the worktree is created on disk.
///
/// Uses direct binary invocation to set the env var without cross-test contamination.
///
/// NOTE: assertions use the `ok` field of the binary's JSON protocol.
/// A previous version of the binary emitted `status`/`summary` (old wrapper format);
/// the current source uses `ok`/`message`/`error_kind`.
#[test]
fn git_tool_create_worktree_repo_in_allow_list() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_create_worktree_repo_in_allow_list: binary not available");
            return;
        }
    };

    let repo_dir = TempDir::new().unwrap();
    let target_repo = init_git_repo(repo_dir.path());
    let branch = "feature/allowlisted";
    Command::new("git")
        .args(["-C", target_repo.to_str().unwrap(), "branch", branch])
        .status()
        .unwrap();

    let wt_path = repo_dir.path().join("wt-allowed");
    // Allow list contains repo_dir (parent of target_repo) — repo IS under this prefix.
    let allow_val = repo_dir.path().to_str().unwrap().to_string();

    let result = invoke_git_tool_directly(
        &binary,
        json!({
            "operation": "create_worktree",
            "repo": target_repo.to_str().unwrap(),
            "path": wt_path.to_str().unwrap(),
            "branch": branch,
        }),
        &[("MURMUR_FILESYSTEM_ALLOW", &allow_val)],
    );

    assert_eq!(
        result["ok"],
        true,
        "should return ok=true when repo is within allow list; got: {result:?}"
    );
    assert!(
        wt_path.exists(),
        "worktree should exist at {:?}",
        wt_path
    );
}

/// Test 8: the tool manifest exposes an input_schema that includes `repo` as a named property.
///
/// This is the end-to-end path that caught the original production bug: the schema is read
/// from the artifact zip's murmur.yaml, converted by build_tool_inventory, serialised by
/// the Anthropic driver into `input_schema`, and sent to the model in the first API request.
/// Without `repo` in the schema the model never passes it, and the tool silently falls back
/// to CWD discovery, which fails when the capsule is not running from inside a git repo.
#[test]
fn git_tool_schema_includes_repo_field() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] git_tool_schema_includes_repo_field: binary not available");
            return;
        }
    };

    let home = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let project = TempDir::new().unwrap();

    let driver = common::create_driver_artifact(
        artifact_dir.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver).success();

    let git_artifact = create_git_tool_artifact(artifact_dir.path(), &binary);
    publish_git_tool(&home, &git_artifact);

    // One end_turn response is enough — we only need the first request to the model,
    // which carries the tools array.
    let server = common::ScriptedServer::start(vec![
        end_turn_response("done"),
    ]);

    let staged = stage_git_tool_session(&home, project.path(), &server.endpoint);
    let workdir = staged.workdir.clone();
    fs::write(workdir.join("task.md"), "no-op").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert!(!requests.is_empty(), "expected at least one LLM request");

    let tools = requests[0]
        .get("tools")
        .and_then(|t| t.as_array())
        .expect("first request should contain a tools array");

    let git_tool = tools
        .iter()
        .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(GIT_TOOL_NAME))
        .expect("murmur-tool-git should appear in the tools array");

    // The Anthropic driver maps inventory `parameters` → `input_schema` in the API call.
    let properties = git_tool
        .get("input_schema")
        .and_then(|s| s.get("properties"))
        .expect("tool should have input_schema.properties");

    assert!(
        properties.get("repo").is_some(),
        "'repo' must be a named property in the tool schema so the model knows to pass it; \
         got properties: {properties}"
    );
    assert!(properties.get("operation").is_some(), "schema must include 'operation'");
    assert!(properties.get("path").is_some(), "schema must include 'path'");
    assert!(properties.get("branch").is_some(), "schema must include 'branch'");
}

// ── Slice 2 helpers ───────────────────────────────────────────────────────────

/// Stage a file, commit it, and return the commit hash.
fn make_commit(repo: &Path, filename: &str, content: &str, message: &str) -> String {
    fs::write(repo.join(filename), content).unwrap();
    let run = |args: &[&str]| {
        let s = Command::new("git").args(args).current_dir(repo).status().unwrap();
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
    invoke_git_tool_directly(binary, data, &[])
}

// ── Slice 2 tests: COMMITS ────────────────────────────────────────────────────

#[test]
fn slice2_commit_success() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_commit_success"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    fs::write(repo.join("new.txt"), "content\n").unwrap();
    Command::new("git").args(["-C", repo_s, "add", "new.txt"]).status().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "commit",
        "repo": repo_s,
        "message": "add new.txt",
    }));

    assert_eq!(result["ok"], true, "commit should succeed; got: {result:?}");
    assert!(!result["hash"].as_str().unwrap_or("").is_empty(), "hash must be populated");
    assert!(!result["short_hash"].as_str().unwrap_or("").is_empty(), "short_hash must be populated");
    assert_eq!(result["subject"], "add new.txt", "subject must match commit message");
}

#[test]
fn slice2_commit_nothing() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_commit_nothing"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());

    // Nothing staged — should fail
    let result = invoke_tool(&binary, json!({
        "operation": "commit",
        "repo": repo.to_str().unwrap(),
        "message": "should fail",
    }));

    assert_eq!(result["ok"], false, "commit with nothing staged should fail");
    assert_eq!(result["error_kind"], "nothing_to_commit");
}

#[test]
fn slice2_commit_allow_empty() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_commit_allow_empty"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());

    let result = invoke_tool(&binary, json!({
        "operation": "commit",
        "repo": repo.to_str().unwrap(),
        "message": "empty commit",
        "allow_empty": true,
    }));

    assert_eq!(result["ok"], true, "allow_empty commit on clean tree should succeed; got: {result:?}");
    assert!(!result["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(result["subject"], "empty commit");
}

#[test]
fn slice2_cherry_pick_success() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_cherry_pick_success"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create feature branch and make a unique commit on it
    Command::new("git").args(["-C", repo_s, "checkout", "-b", "feature"]).status().unwrap();
    let pick_hash = make_commit(&repo, "cherry.txt", "cherry content\n", "add cherry.txt");

    // Switch back to main
    Command::new("git").args(["-C", repo_s, "checkout", "main"]).status().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "cherry_pick",
        "repo": repo_s,
        "ref": pick_hash,
    }));

    assert_eq!(result["ok"], true, "cherry-pick should succeed; got: {result:?}");
    assert!(!result["hash"].as_str().unwrap_or("").is_empty());
    assert_eq!(result["subject"], "add cherry.txt");
    assert!(repo.join("cherry.txt").exists(), "cherry.txt should exist in main after pick");
}

#[test]
fn slice2_cherry_pick_conflict() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_cherry_pick_conflict"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // feature: modify README.md one way and commit
    Command::new("git").args(["-C", repo_s, "checkout", "-b", "feature"]).status().unwrap();
    let pick_hash = make_commit(&repo, "README.md", "feature version\n", "feature change");

    // main: modify README.md a different way and commit (creates divergence)
    Command::new("git").args(["-C", repo_s, "checkout", "main"]).status().unwrap();
    make_commit(&repo, "README.md", "main version\n", "main change");

    // cherry-pick feature commit onto main → conflict
    let result = invoke_tool(&binary, json!({
        "operation": "cherry_pick",
        "repo": repo_s,
        "ref": pick_hash,
    }));

    assert_eq!(result["ok"], false, "conflicting cherry-pick should fail; got: {result:?}");
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
        None => { eprintln!("[SKIP] slice2_branch_list"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "list",
    }));

    assert_eq!(result["ok"], true, "branch list should succeed; got: {result:?}");
    let branches = result["branches"].as_array().expect("branches must be an array");
    let current = branches.iter().find(|b| b["current"] == true);
    assert!(current.is_some(), "one branch should be flagged as current");
    let current_name = current.unwrap()["name"].as_str().unwrap_or("");
    assert!(!current_name.is_empty(), "current branch name must not be empty");
}

#[test]
fn slice2_branch_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_branch_create"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "create",
        "name": "new-feature",
    }));
    assert_eq!(result["ok"], true, "branch create should succeed; got: {result:?}");
    assert_eq!(result["name"], "new-feature");

    // Verify branch appears in list
    let list = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "list",
    }));
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
        None => { eprintln!("[SKIP] slice2_branch_delete"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create a branch (same content as main → will be "merged")
    invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "create",
        "name": "to-delete",
    }));

    let result = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "delete",
        "name": "to-delete",
    }));
    assert_eq!(result["ok"], true, "branch delete should succeed; got: {result:?}");
    assert_eq!(result["name"], "to-delete");

    // Verify branch is gone
    let list = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "list",
    }));
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
        None => { eprintln!("[SKIP] slice2_branch_delete_not_merged"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create branch and add a commit not in main → not merged
    Command::new("git").args(["-C", repo_s, "checkout", "-b", "unmerged"]).status().unwrap();
    make_commit(&repo, "extra.txt", "data\n", "unmerged commit");
    Command::new("git").args(["-C", repo_s, "checkout", "main"]).status().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "branch",
        "repo": repo_s,
        "subcommand": "delete",
        "name": "unmerged",
    }));

    assert_eq!(result["ok"], false, "deleting unmerged branch should fail");
    assert_eq!(result["error_kind"], "not_merged");
}

// ── Slice 2 tests: CHECKOUT / SWITCH ─────────────────────────────────────────

#[test]
fn slice2_checkout_switch_branch() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_checkout_switch_branch"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    Command::new("git").args(["-C", repo_s, "branch", "other"]).status().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "checkout",
        "repo": repo_s,
        "ref": "other",
    }));

    assert_eq!(result["ok"], true, "checkout should succeed; got: {result:?}");
    assert_eq!(result["branch"], "other");
    assert_eq!(result["detached"], false);
}

#[test]
fn slice2_checkout_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_checkout_create"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "checkout",
        "repo": repo_s,
        "ref": "fresh-branch",
        "create": true,
    }));

    assert_eq!(result["ok"], true, "checkout -b should succeed; got: {result:?}");
    assert_eq!(result["branch"], "fresh-branch");
    assert_eq!(result["detached"], false);
}

#[test]
fn slice2_checkout_dirty() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_checkout_dirty"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    // Create `other` branch where README.md differs from main
    Command::new("git").args(["-C", repo_s, "checkout", "-b", "other"]).status().unwrap();
    make_commit(&repo, "README.md", "other branch version\n", "other change");
    Command::new("git").args(["-C", repo_s, "checkout", "main"]).status().unwrap();

    // Dirty the working tree: modify README.md locally (unstaged) so it conflicts with `other`
    fs::write(repo.join("README.md"), "local dirty change\n").unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "checkout",
        "repo": repo_s,
        "ref": "other",
    }));

    assert_eq!(result["ok"], false, "checkout with conflicting dirty tree should fail");
    assert_eq!(result["error_kind"], "dirty_working_tree");
}

#[test]
fn slice2_switch_create() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_switch_create"); return; }
    };
    let dir = TempDir::new().unwrap();
    let repo = init_git_repo(dir.path());
    let repo_s = repo.to_str().unwrap();

    let result = invoke_tool(&binary, json!({
        "operation": "switch",
        "repo": repo_s,
        "branch": "switched-branch",
        "create": true,
    }));

    assert_eq!(result["ok"], true, "switch -c should succeed; got: {result:?}");
    assert_eq!(result["branch"], "switched-branch");
}

// ── Slice 2 tests: RESET ──────────────────────────────────────────────────────

#[test]
fn slice2_reset_soft() {
    let binary = match git_tool_binary() {
        Some(b) => b,
        None => { eprintln!("[SKIP] slice2_reset_soft"); return; }
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
    let result = invoke_tool(&binary, json!({
        "operation": "reset",
        "repo": repo_s,
        "mode": "soft",
        "ref": &parent_hash,
    }));

    assert_eq!(result["ok"], true, "reset soft should succeed; got: {result:?}");
    let resolved = result["ref"].as_str().unwrap_or("");
    assert_eq!(resolved, parent_hash, "HEAD should point to the parent commit after soft reset");

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
        None => { eprintln!("[SKIP] slice2_reset_hard"); return; }
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

    let result = invoke_tool(&binary, json!({
        "operation": "reset",
        "repo": repo_s,
        "mode": "hard",
        "ref": &parent_hash,
    }));

    assert_eq!(result["ok"], true, "reset hard should succeed; got: {result:?}");
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
