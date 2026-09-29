//! The A2A door of a capsule that declares `network.authentication`, end to end: the gate's order
//! and refusals, the public and extended cards, the tokens `mur run` prints, and what never sees a
//! token. And the door of a capsule that declares nothing, which is unchanged.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest, StagedSession,
};
use common::door_capsule::{
    agent_project, driver_home, end_turn, message, request, rpc, wait_completed, MurRun,
    AUTHENTICATION_YAML, QUEUE_SLEEP_YAML,
};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

const CAPSULE_NAME: &str = "door-agent";

const EXPORTS_YAML: &str = "exports:\n  files:\n    root: out\n    mode: read-only\n";

// ── In-process launch ─────────────────────────────────────────────────────────

/// Stages the capsule `manifest` describes from `home`'s store, declaring what its
/// `network.authentication` block declares.
fn stage(home: &TempDir, manifest: &Path) -> StagedSession {
    let runtime_manifest = load_runtime_manifest(manifest).unwrap();
    let artifacts = runtime_manifest
        .artifacts
        .iter()
        .map(|artifact| ArtifactRequest {
            name: artifact.name.clone(),
            version: artifact.version.clone(),
            runtime: artifact.runtime.clone(),
            source: artifact.source.clone(),
            on_overflow: artifact.on_overflow,
            config: artifact.config.clone(),
            gateway: artifact.gateway.clone(),
            capabilities: artifact.capabilities.clone(),
        })
        .collect();
    stage_session(
        std::sync::Arc::new(LocalRegistry::new(
            home.path().join(".murmur").join("artifacts"),
        )),
        StageRequest {
            credentials_file: None,
            manifest_dir: manifest.parent().unwrap().to_path_buf(),
            capsule_name: runtime_manifest.name.clone(),
            capsule_version: runtime_manifest.version.clone(),
            capsule_component_bytes: Vec::new(),
            artifacts,
            allowlisted_tools: Default::default(),
            lock_expectations: None,
            capability_policy: capability_policy_from_runtime_manifest(&runtime_manifest),
            inference: runtime_manifest.inference.clone(),
            system_prompt_overridden: false,
            context: None,
            context_id: None,
            resume: None,
            forget_session: false,
            otel_endpoint: None,
            eval_config_json: None,
            case_id: None,
            dataset_id: None,
            lifecycle: runtime_manifest.lifecycle.clone(),
            lifecycle_override: None,
            trace: None,
            workdir: None,
            bind_addr: "127.0.0.1".to_string(),
            internal_port: None,
            declared_containment_floor: ContainmentClass::Advisory,
            exports: runtime_manifest.exports.clone(),
            control: None,
            door_authentication: runtime_manifest
                .network
                .as_ref()
                .and_then(|network| network.authentication.clone()),
            spawn_grant: None,
            machine_tokens_per_day: None,
        },
    )
    .unwrap()
}

/// Launches `staged` on a thread of its own and returns its `host:port`. A queue+sleep capsule
/// never exits, so the thread is left behind.
fn launch(staged: StagedSession) -> String {
    let (url_tx, url_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        launch_session(staged, move |url| {
            let _ = url_tx.send(url.to_string());
        })
        .expect("launch should succeed")
    });
    url_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("timed out waiting for the door")
}

fn trace_events(workdir: &Path) -> Vec<Value> {
    fs::read_to_string(workdir.join("trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn event_count(workdir: &Path, event: &str) -> usize {
    trace_events(workdir)
        .iter()
        .filter(|e| e["event_type"] == event)
        .count()
}

fn assert_refused(
    response: &common::door_capsule::Response,
    status: u16,
    code: &str,
    challenge: &str,
) {
    assert_eq!(response.status, status, "{response:?}");
    assert_eq!(
        response.header("www-authenticate"),
        Some(challenge),
        "{response:?}"
    );
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{response:?}"
    );
    let body = response.json();
    assert_eq!(body["error"], code, "{response:?}");
    assert!(body["message"].is_string(), "{response:?}");
    assert!(
        !response.body.contains("mdt1."),
        "a refusal never carries a token: {response:?}"
    );
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[test]
fn authenticated_door_gates_every_request_but_the_public_card() {
    if common::skip_without_host_support(
        "authenticated_door_gates_every_request_but_the_public_card",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn(1, "authenticated reply")]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        CAPSULE_NAME,
        "",
        &format!("{QUEUE_SLEEP_YAML}{EXPORTS_YAML}{AUTHENTICATION_YAML}"),
    );
    let manifest = project.path().join("murmur.yaml");
    let staged = stage(&home, &manifest);
    let session_id = staged.session_id.clone();
    let workdir = staged.workdir.clone();
    fs::create_dir_all(workdir.join("out")).unwrap();
    fs::write(workdir.join("out").join("report.txt"), "exported").unwrap();

    let tokens: Vec<(String, String)> = staged
        .door_tokens()
        .into_iter()
        .map(|(name, token)| (name, token.expose().to_string()))
        .collect();
    let names: Vec<&str> = tokens.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["operator", "reader", "watcher"]);
    let token = |name: &str| {
        tokens
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
            .unwrap()
    };
    let (operator, watcher, reader) = (token("operator"), token("watcher"), token("reader"));

    // A second session's operator token: minted under another key.
    let other_project = agent_project(&server.endpoint, CAPSULE_NAME, "", AUTHENTICATION_YAML);
    let other = stage(&home, &other_project.path().join("murmur.yaml"));
    let foreign = other.door_tokens()[0].1.expose().to_string();
    drop(other);

    let addr = launch(staged);
    let realm = format!("Bearer realm=\"{CAPSULE_NAME}\"");
    let invalid = format!("Bearer realm=\"{CAPSULE_NAME}\", error=\"invalid_token\"");

    // The public card: ungated, and without the capsule extension.
    let card = request(&addr, "GET", "/.well-known/agent-card.json", &[], "");
    assert_eq!(card.status, 200, "{card:?}");
    let card = card.json();
    assert_eq!(
        common::a2a_card_conformance::check_agent_card(&card),
        Ok(()),
        "{card:#}"
    );
    assert_eq!(
        card["capabilities"]["extensions"].as_array().unwrap().len(),
        1,
        "{card:#}"
    );
    assert!(common::card_door_methods(&card).contains(&"agent/getAuthenticatedExtendedCard"));
    assert_eq!(card["capabilities"]["extendedAgentCard"], true);
    assert_eq!(
        card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
        "Bearer"
    );
    for key in ["sessionId", "tools", "shell", "network", "planes"] {
        assert!(!card.to_string().contains(key), "{key}: {card:#}");
    }

    // No header: 401 before anything is looked at, and nothing reaches the task loop.
    let refused = rpc(&addr, None, "message/send", message("m-none", "hello"));
    assert_refused(&refused, 401, "unauthenticated", &realm);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(event_count(&workdir, "task_start"), 0);

    // A token this session did not mint, garbage, and two headers: 401 invalid_token.
    for presented in [
        "Bearer garbage".to_string(),
        bearer(&foreign),
        format!("Basic {operator}"),
    ] {
        let refused = request(
            &addr,
            "POST",
            "/",
            &[("Authorization", presented.as_str())],
            &json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": message("m-bad", "hello")}).to_string(),
        );
        assert_refused(&refused, 401, "invalid_token", &invalid);
    }
    let twice = request(
        &addr,
        "POST",
        "/",
        &[
            ("Authorization", &bearer(&operator)),
            ("Authorization", &bearer(&operator)),
        ],
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": "x"}})
            .to_string(),
    );
    assert_refused(&twice, 401, "invalid_token", &invalid);
    // The scheme name is matched without regard to case.
    let lower = request(
        &addr,
        "POST",
        "/",
        &[("authorization", &format!("bearer {watcher}"))],
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": {"id": "x"}})
            .to_string(),
    );
    assert_eq!(lower.status, 200, "{lower:?}");

    // The watcher may not start a task, and is told which credential and which scope.
    let forbidden = rpc(
        &addr,
        Some(&watcher),
        "message/send",
        message("m-w", "hello"),
    );
    assert_refused(
        &forbidden,
        403,
        "insufficient_scope",
        &format!(
            "Bearer realm=\"{CAPSULE_NAME}\", error=\"insufficient_scope\", scope=\"message/send\""
        ),
    );
    let text = forbidden.json()["message"].as_str().unwrap().to_string();
    assert!(
        text.contains("watcher") && text.contains("message/send"),
        "{text}"
    );
    assert_eq!(event_count(&workdir, "task_start"), 0);

    // In scope, every real error is unchanged.
    let missing = rpc(
        &addr,
        Some(&watcher),
        "tasks/get",
        json!({"id": "tsk_nope"}),
    );
    assert_eq!(missing.status, 200, "{missing:?}");
    assert_eq!(missing.json()["error"]["code"], -32001, "{missing:?}");
    let unknown = rpc(&addr, Some(&watcher), "tasks/list", json!({}));
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(unknown.json()["error"]["code"], -32601, "{unknown:?}");

    // The operator runs a task exactly as a public door would, and the watcher can read it.
    let sent = rpc(
        &addr,
        Some(&operator),
        "message/send",
        message("m-op", "hello"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
    wait_completed(&addr, Some(&operator), &task_id);
    let seen = rpc(&addr, Some(&watcher), "tasks/get", json!({"id": task_id}));
    assert_eq!(
        seen.json()["result"]["status"]["state"],
        "completed",
        "{seen:?}"
    );

    // The extended card: any valid token, in A2A 0.3 shape.
    let refused = rpc(&addr, None, "agent/getAuthenticatedExtendedCard", json!({}));
    assert_refused(&refused, 401, "unauthenticated", &realm);
    let extended = rpc(
        &addr,
        Some(&watcher),
        "agent/getAuthenticatedExtendedCard",
        json!({}),
    );
    assert_eq!(extended.status, 200, "{extended:?}");
    let extended = extended.json()["result"].clone();
    assert_eq!(extended["protocolVersion"], "0.3.0");
    assert_eq!(extended["url"], format!("http://{addr}"));
    assert_eq!(extended["supportsAuthenticatedExtendedCard"], true);
    assert_eq!(
        common::card_capsule_params(&extended)["sessionId"],
        session_id.as_str()
    );
    assert_eq!(
        common::card_capsule_params(&extended)["planes"],
        json!(["files"])
    );

    // The streaming methods are refused before any stream opens.
    for method in ["message/stream", "stream/watch"] {
        let refused = rpc(&addr, None, method, message("m-s", "hello"));
        assert_refused(&refused, 401, "unauthenticated", &realm);
        assert_ne!(refused.header("content-type"), Some("text/event-stream"));
    }

    // The operator resource plane needs `resources/files`.
    let refused = request(&addr, "GET", "/resources/files", &[], "");
    assert_refused(&refused, 401, "unauthenticated", &realm);
    let refused = request(
        &addr,
        "GET",
        "/resources/files",
        &[("Authorization", &bearer(&watcher))],
        "",
    );
    assert_refused(
        &refused,
        403,
        "insufficient_scope",
        &format!(
            "Bearer realm=\"{CAPSULE_NAME}\", error=\"insufficient_scope\", scope=\"resources/files\""
        ),
    );
    let listed = request(
        &addr,
        "GET",
        "/resources/files",
        &[("Authorization", &bearer(&reader))],
        "",
    );
    assert_eq!(listed.status, 200, "{listed:?}");
    assert!(listed.body.contains("report.txt"), "{listed:?}");

    // An unknown path says nothing to a stranger and 404 to a caller who is let in.
    let refused = request(&addr, "GET", "/no-such-path", &[], "");
    assert_refused(&refused, 401, "unauthenticated", &realm);
    let absent = request(
        &addr,
        "GET",
        "/no-such-path",
        &[("Authorization", &bearer(&operator))],
        "",
    );
    assert_eq!(absent.status, 404, "{absent:?}");

    // The peer plane keeps its own authoriser: a handle is a peer's credential, not a door token.
    let peer = request(&addr, "GET", "/resources/peer/x", &[], "");
    assert_ne!(peer.status, 401, "{peer:?}");
    assert!(peer.header("www-authenticate").is_none(), "{peer:?}");
    assert_ne!(peer.json()["error"], "unauthenticated", "{peer:?}");

    // Refusals at the gate write nothing: the one task_start is the operator's task.
    assert_eq!(event_count(&workdir, "task_start"), 1);
}

#[test]
fn public_door_ignores_authorization_and_has_no_extended_card() {
    if common::skip_without_host_support(
        "public_door_ignores_authorization_and_has_no_extended_card",
    ) {
        return;
    }
    let server = common::ScriptedServer::start(vec![end_turn(1, "one"), end_turn(2, "two")]);
    let home = driver_home();
    let project = agent_project(&server.endpoint, CAPSULE_NAME, "", QUEUE_SLEEP_YAML);
    let staged = stage(&home, &project.path().join("murmur.yaml"));
    assert!(staged.door_tokens().is_empty());
    let addr = launch(staged);

    let card = request(&addr, "GET", "/.well-known/agent-card.json", &[], "").json();
    assert_eq!(card["securitySchemes"], json!({}));
    assert_eq!(card["securityRequirements"], json!([]));
    assert_eq!(card["capabilities"]["extendedAgentCard"], false);
    assert!(!common::card_capsule_params(&card)["sessionId"]
        .as_str()
        .unwrap()
        .is_empty());
    assert!(!common::card_door_methods(&card).contains(&"agent/getAuthenticatedExtendedCard"));

    for token in [None, Some("garbage")] {
        let sent = rpc(&addr, token, "message/send", message("m-public", "hello"));
        assert_eq!(sent.status, 200, "{sent:?}");
        let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
        wait_completed(&addr, None, &task_id);
    }

    for token in [None, Some("garbage")] {
        let answer = rpc(
            &addr,
            token,
            "agent/getAuthenticatedExtendedCard",
            json!({}),
        );
        assert_eq!(answer.status, 200, "{answer:?}");
        let error = &answer.json()["error"];
        assert_eq!(error["code"], -32007, "{answer:?}");
        assert_eq!(
            error["message"],
            "Authenticated Extended Card is not configured"
        );
    }
    let absent = request(&addr, "GET", "/no-such-path", &[], "");
    assert_eq!(absent.status, 404);
}

/// The `W-SEC-032` lines in `text`.
fn public_door_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| line.contains("warning[W-SEC-032]"))
        .map(str::to_string)
        .collect()
}

/// `mur run`'s stderr once its door is up and anything staging printed has arrived.
fn stderr_after_launch(home: &Path, manifest: &Path, args: &[&str]) -> String {
    let run = MurRun::start(home, manifest, true, args, &[]);
    // Staging prints before the door is announced; its stderr is read on another thread.
    std::thread::sleep(Duration::from_millis(500));
    run.stderr()
}

#[test]
fn public_door_warning_names_what_a_stranger_reaches() {
    let server = common::ScriptedServer::start(vec![]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        CAPSULE_NAME,
        "",
        &format!("{QUEUE_SLEEP_YAML}{EXPORTS_YAML}"),
    );
    let manifest = project.path().join("murmur.yaml");

    let exposed = stderr_after_launch(home.path(), &manifest, &["--bind", "0.0.0.0"]);
    let lines = public_door_lines(&exposed);
    assert_eq!(lines.len(), 1, "{exposed}");
    let line = &lines[0];
    assert!(
        line.starts_with(
            "warning[W-SEC-032]: the door is bound to 0.0.0.0 and network.authentication is not declared"
        ),
        "{line}"
    );
    for named in [
        "message/send, message/stream, stream/watch, tasks/get, tasks/cancel, session/stop",
        "the session id",
        "tools []",
        "shell: false",
        "network: true",
        "planes [files]",
        "#w-sec-032",
    ] {
        assert!(line.contains(named), "{named} missing from {line}");
    }

    let loopback = stderr_after_launch(home.path(), &manifest, &[]);
    assert!(public_door_lines(&loopback).is_empty(), "{loopback}");
}

#[test]
fn public_door_warning_is_silent_for_an_authenticated_door() {
    let server = common::ScriptedServer::start(vec![]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        CAPSULE_NAME,
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let stderr = stderr_after_launch(
        home.path(),
        &project.path().join("murmur.yaml"),
        &["--bind", "0.0.0.0"],
    );
    assert!(public_door_lines(&stderr).is_empty(), "{stderr}");
}

#[test]
fn tokens_stay_out_of_the_session() {
    if common::skip_without_host_support("tokens_stay_out_of_the_session") {
        return;
    }
    let server = common::ScriptedServer::start(vec![
        json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "test-model",
            "content": [{
                "type": "tool_use",
                "id": "toolu_env",
                "name": "bash",
                "input": {"command": "export -p"}
            }],
            "stop_reason": "tool_use",
            "stop_sequence": Value::Null,
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
        end_turn(2, "environment read"),
    ]);
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        CAPSULE_NAME,
        "  shell:\n    allow:\n      - bash\n",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let manifest = project.path().join("murmur.yaml");
    let run = MurRun::start(home.path(), &manifest, true, &[], &[]);
    let addr = run.url();
    let workdir = std::path::PathBuf::from(run.startup["workdir"].as_str().unwrap());
    let tokens: Vec<String> = ["operator", "watcher", "reader"]
        .into_iter()
        .map(|name| run.token(name))
        .collect();

    let sent = rpc(
        &addr,
        Some(&tokens[0]),
        "message/send",
        message("m-env", "read env"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
    wait_completed(&addr, Some(&tokens[1]), &task_id);

    // A refusal on the way: its body carries no token either.
    let refused = rpc(
        &addr,
        Some(&tokens[1]),
        "message/send",
        message("m-no", "no"),
    );
    assert_eq!(refused.status, 403);

    // End the capsule through its own teardown, so every file it writes is final.
    let stopped = assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home.path())
        .env_remove("NEXUS_API_KEY")
        .args(["stop", "@1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stopped = String::from_utf8_lossy(&stopped).to_string();
    assert!(!stopped.contains("mdt1."), "{stopped}");
    let mut run = run;
    let deadline = Instant::now() + Duration::from_secs(30);
    while run.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the capsule did not exit");
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));

    // The guest's environment names the session and carries no token.
    let tool_result = common::find_tool_result(&server.requests(), "toolu_env")
        .expect("the provider saw the tool result");
    let environment = common::extract_result_text(&tool_result);
    assert!(environment.contains("MURMUR_SESSION_ID"), "{environment}");
    assert!(!environment.contains("mdt1."), "{environment}");
    for request in server.requests() {
        assert!(
            !request.to_string().contains("mdt1."),
            "the provider saw a token"
        );
    }

    assert!(workdir.join("trace.jsonl").exists());
    assert!(workdir.join("MURMUR.md").exists());
    let stderr = run.stderr();
    for token in &tokens {
        assert_eq!(
            common::door_capsule::files_containing(&workdir, token),
            Vec::<std::path::PathBuf>::new(),
            "a session file carries a token"
        );
        assert_eq!(
            common::door_capsule::files_containing(project.path(), token),
            Vec::<std::path::PathBuf>::new(),
            "a project file carries a token"
        );
        assert!(!stderr.contains(token.as_str()), "stderr carries a token");
    }
    assert!(!stderr.contains("mdt1."), "stderr carries a token");
}
