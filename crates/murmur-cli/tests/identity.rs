#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    path::{Path, PathBuf},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest,
};
use murmur_artifact::{load_runtime_manifest, ArtifactRuntime, ContainmentClass, LocalRegistry};
use serde_json::Value;
use tempfile::TempDir;

const DRIVER_NAME: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.4";

fn fixture_path(relative: &str) -> PathBuf {
    common::fixture_path(relative)
}

fn end_turn_server(text: &str) -> common::ScriptedServer {
    common::ScriptedServer::start(vec![serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()])
}

fn setup_agent_project(endpoint: &str) -> (TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let driver_artifact = common::create_driver_artifact(
        artifacts.path(),
        DRIVER_NAME,
        DRIVER_VERSION,
        &fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm"),
    );
    common::publish_local(&home, &driver_artifact).success();

    fs::write(
        project.path().join("murmur.yaml"),
        format!(
            "name: identity-agent\nversion: 0.2.0\nartifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    runtime: driver\ncapabilities:\n  network:\n    allow:\n      - {endpoint}\n  shell:\n    allow:\n      - bash\ninference:\n  transport: http\n  endpoint: {endpoint}\n  model: test-model\n  api_key: test-key\n  driver:\n    artifact: {DRIVER_NAME}\n"
        ),
    )
    .unwrap();

    (home, project.keep().join("murmur.yaml"))
}

fn stage_agent(home: &TempDir, manifest_path: &Path) -> capsule_runtime::StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest_path).unwrap();
    let mut allowlisted_tools = std::collections::HashSet::new();
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
            manifest_dir: manifest_path.parent().unwrap().to_path_buf(),
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

#[test]
fn murmur_md_has_identity_section_with_correct_values() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);
    let session_id = staged.session_id.clone();
    fs::write(staged.workdir.join("task.md"), "Identity test").unwrap();

    let launched = launch_session(staged, |_| {}).expect("launch should succeed");

    let content = fs::read_to_string(launched.workdir.join("MURMUR.md")).unwrap();

    assert!(
        content.contains("## Identity"),
        "MURMUR.md should have an ## Identity section; got:\n{content}"
    );
    assert!(
        content.contains("identity-agent v0.2.0"),
        "MURMUR.md should include capsule name and version; got:\n{content}"
    );
    assert!(
        content.contains(&session_id),
        "MURMUR.md should include the session ID ({session_id}); got:\n{content}"
    );
    assert!(
        content.contains("localhost:"),
        "MURMUR.md should include a capsule_url with localhost port; got:\n{content}"
    );

    let identity_pos = content.find("## Identity").unwrap();
    let capsule_pos = content.find("## Capsule").unwrap();
    assert!(
        identity_pos < capsule_pos,
        "## Identity should appear before ## Capsule in MURMUR.md"
    );
}

#[test]
fn identity_env_vars_are_visible_to_shell_tool() {
    let server = common::ScriptedServer::start(vec![
        serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_env",
                "name": "bash",
                "input": {
                    "command": "printf \"%s\\n%s\\n%s\\n%s\\n\" \"$MURMUR_CAPSULE_NAME\" \"$MURMUR_CAPSULE_VERSION\" \"$MURMUR_SESSION_ID\" \"$MURMUR_CAPSULE_URL\""
                }
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        serde_json::json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{"type": "text", "text": "env captured"}],
            "stop_reason": "end_turn",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    ]);
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);
    let session_id = staged.session_id.clone();
    fs::write(staged.workdir.join("task.md"), "Echo identity env.").unwrap();

    launch_session(staged, |_| {}).expect("launch should succeed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let tool_result = find_tool_result_block(&requests[1], "toolu_env").expect("tool_result");
    let text = tool_result_text(tool_result);
    assert!(text.contains("identity-agent"), "tool result was:\n{text}");
    assert!(text.contains("0.2.0"), "tool result was:\n{text}");
    assert!(text.contains(&session_id), "tool result was:\n{text}");
    assert!(text.contains("localhost:"), "tool result was:\n{text}");
}

#[test]
fn agent_card_served_at_well_known_path() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(&home, &manifest_path);
    let staged_session_id = staged.session_id.clone();

    fs::write(staged.workdir.join("task.md"), "Agent card test").unwrap();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("timed out waiting for capsule_url");

    let card_json = http_get(&capsule_url, "/.well-known/agent-card.json");
    let card: Value = serde_json::from_str(&card_json).expect("agent card should be valid JSON");

    assert_eq!(card["name"], "identity-agent");
    assert_eq!(card["version"], "0.2.0");
    assert!(
        card["url"].as_str().unwrap_or("").starts_with("localhost:"),
        "agent card url should be localhost:port"
    );
    assert!(
        card["capabilities"]["tools"].is_array(),
        "capabilities.tools should be an array"
    );
    assert_eq!(
        card["capabilities"]["network"], true,
        "network capability should be true (endpoint is allowlisted)"
    );

    let _ = staged_session_id; // ensure borrow survives
    let launched = handle.join().expect("launch thread should not panic");
    assert!(
        launched.workdir.join("out/result.txt").exists(),
        "result.txt should exist after agent completes"
    );
}

#[test]
fn agent_card_returns_404_for_unknown_path() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);

    let staged = stage_agent(&home, &manifest_path);
    fs::write(staged.workdir.join("task.md"), "404 test").unwrap();

    let (url_tx, url_rx) = std::sync::mpsc::channel::<String>();
    let handle = std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    let capsule_url = url_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("timed out waiting for capsule_url");

    let status = http_get_status(&capsule_url, "/unknown/path");
    assert_eq!(status, 404, "unknown paths should return 404");

    handle.join().expect("launch thread should not panic");
}

#[test]
fn two_concurrent_sessions_have_distinct_capsule_urls_and_session_ids() {
    let server1 = end_turn_server("session 1");
    let server2 = end_turn_server("session 2");

    let (home1, manifest_path1) = setup_agent_project(&server1.endpoint);
    let (home2, manifest_path2) = setup_agent_project(&server2.endpoint);

    let staged1 = stage_agent(&home1, &manifest_path1);
    let staged2 = stage_agent(&home2, &manifest_path2);

    assert_ne!(
        staged1.session_id, staged2.session_id,
        "two sessions must have distinct session IDs"
    );

    let workdir1 = staged1.workdir.clone();
    let workdir2 = staged2.workdir.clone();

    fs::write(staged1.workdir.join("task.md"), "Concurrent 1").unwrap();
    fs::write(staged2.workdir.join("task.md"), "Concurrent 2").unwrap();

    let h1 = std::thread::spawn(move || {
        launch_session(staged1, |_| {}).expect("session 1 should launch")
    });
    let h2 = std::thread::spawn(move || {
        launch_session(staged2, |_| {}).expect("session 2 should launch")
    });

    let r1 = h1.join().expect("session 1 thread should not panic");
    let r2 = h2.join().expect("session 2 thread should not panic");

    // Extract capsule URLs from the final MURMUR.md (overwritten by launch_session with full identity)
    let content1 = fs::read_to_string(r1.workdir.join("MURMUR.md")).unwrap();
    let content2 = fs::read_to_string(r2.workdir.join("MURMUR.md")).unwrap();

    let url1 = extract_capsule_url(&content1).expect("MURMUR.md 1 should contain a capsule_url");
    let url2 = extract_capsule_url(&content2).expect("MURMUR.md 2 should contain a capsule_url");

    assert_ne!(
        url1, url2,
        "two concurrent sessions must have distinct capsule URLs"
    );

    let _ = (workdir1, workdir2);
}

#[test]
fn http_server_not_reachable_after_session_ends() {
    let server = end_turn_server("done");
    let (home, manifest_path) = setup_agent_project(&server.endpoint);
    let staged = stage_agent(&home, &manifest_path);
    let workdir_for_thread = staged.workdir.clone();
    fs::write(workdir_for_thread.join("task.md"), "shutdown test").unwrap();

    // Capture the capsule URL before launching (via polling)
    // We need to see MURMUR.md written from stage_session first (partial), then the final one.
    // Simpler: run synchronously, then try to connect after launch completes.
    // The port should be bound during launch and released after.

    // Run synchronously: launch (which starts and stops the server), then try to connect.
    let staged_workdir = staged.workdir.clone();
    let launched = launch_session(staged, |_| {}).expect("launch should succeed");

    let content = fs::read_to_string(launched.workdir.join("MURMUR.md")).unwrap();
    let capsule_url =
        extract_capsule_url(&content).expect("MURMUR.md should contain a capsule_url after launch");

    // The HTTP server should have shut down. Connecting should fail.
    let connect_result = TcpStream::connect(&capsule_url);
    assert!(
        connect_result.is_err(),
        "TCP connection to {capsule_url} should fail after session ends, but it succeeded"
    );

    let _ = staged_workdir;
}

/// Extract `capsule_url` from a MURMUR.md identity section line like:
/// `- Capsule URL: localhost:12345`
fn extract_capsule_url(content: &str) -> Option<String> {
    for line in content.lines() {
        if let Some(url) = line.strip_prefix("- Capsule URL: ") {
            let url = url.trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// Make a blocking HTTP GET request and return the response body.
fn http_get(addr: &str, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("should connect to agent-card server");
    let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);
    let mut response = String::new();

    // Skip headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }

    // Read body
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap() > 0 {
        response.push_str(&line);
        line.clear();
    }

    response
}

/// Make a blocking HTTP GET request and return the HTTP status code.
fn http_get_status(addr: &str, path: &str) -> u16 {
    let mut stream = TcpStream::connect(addr).expect("should connect to agent-card server");
    let request = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut reader = BufReader::new(&stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();

    // "HTTP/1.1 404 Not Found"
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn find_tool_result_block<'a>(request: &'a Value, tool_use_id: &str) -> Option<&'a Value> {
    let messages = request.get("messages")?.as_array()?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }

        let Some(content) = message.get("content").and_then(Value::as_array) else {
            continue;
        };

        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
            {
                return Some(block);
            }
        }
    }

    None
}

fn tool_result_text(block: &Value) -> String {
    if let Some(text) = block.get("content").and_then(Value::as_str) {
        return text.to_string();
    }

    block
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts.iter().find_map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
        })
        .unwrap_or_default()
}
