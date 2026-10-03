//! `exports.peer_tasks`, end to end: a capsule's A2A door serves a request its sending runtime
//! stamped `peer` only when the capsule's own manifest says `accept: true`, refuses it with one
//! fixed `403` otherwise, and says which on the public card and in the scope report.
//!
//! Every door here is a real launched capsule; nothing about the door is mocked.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

use capsule_runtime::delegation::{COMPLETION_SESSION_HEADER, DELEGATION_ID_HEADER};
use capsule_runtime::{
    capability_policy_from_runtime_manifest, launch_session, stage_session, ArtifactRequest,
    StageRequest, StagedSession, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER,
};
use common::door_capsule::{
    agent_project, driver_home, end_turn, message, request, rpc, Response, AUTHENTICATION_YAML,
    QUEUE_SLEEP_YAML,
};
use murmur_artifact::{load_runtime_manifest, ContainmentClass, LocalRegistry};
use serde_json::{json, Value};
use tempfile::TempDir;

/// The exact body of every consent refusal.
const REFUSAL_BODY: &str =
    r#"{"error":"peer_not_accepted","message":"this capsule does not accept tasks from peers"}"#;

const ACCEPT_YAML: &str = "exports:\n  peer_tasks:\n    accept: true\n";
const REFUSE_YAML: &str = "exports:\n  peer_tasks:\n    accept: false\n";
const FILES_ONLY_YAML: &str = "exports:\n  files:\n    root: out\n    mode: read-only\n";

/// The two headers a sending murmur runtime stamps on a peer message.
const PEER: [(&str, &str); 2] = [(PEER_ORIGIN_HEADER, "peer"), (PEER_TRUST_HEADER, "trusted")];

// ── Launching ─────────────────────────────────────────────────────────────────

/// Stages the capsule `manifest` describes from `home`'s store, exactly as `mur run` would: every
/// field that matters here is read from the manifest and from nothing else.
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
            formation_id: None,
            formation_member: None,
            ignore_task_file: false,
        },
    )
    .unwrap()
}

/// A launched queue+sleep capsule: its door, its session, its workdir, its tokens, and the
/// scripted provider and store it runs against.
struct Capsule {
    addr: String,
    session_id: String,
    workdir: PathBuf,
    operator: Option<String>,
    _server: common::ScriptedServer,
    _home: TempDir,
    _project: TempDir,
}

impl Capsule {
    /// Launches a capsule whose manifest carries `extra` after the queue+sleep lifecycle. Its
    /// provider answers every turn with `end_turn`.
    fn launch(name: &str, extra: &str) -> Self {
        let server = common::ScriptedServer::start(
            (1..=8).map(|n| end_turn(n, "peer task reply")).collect(),
        );
        let home = driver_home();
        let project = agent_project(
            &server.endpoint,
            name,
            "",
            &format!("{QUEUE_SLEEP_YAML}{extra}"),
        );
        let staged = stage(&home, &project.path().join("murmur.yaml"));
        let session_id = staged.session_id.clone();
        let workdir = staged.workdir.clone();
        let operator = staged
            .door_tokens()
            .into_iter()
            .find(|(name, _)| name == "operator")
            .map(|(_, token)| token.expose().to_string());

        let (url_tx, url_rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            launch_session(staged, move |url| {
                let _ = url_tx.send(url.to_string());
            })
            .expect("launch should succeed")
        });
        let addr = url_rx
            .recv_timeout(Duration::from_secs(60))
            .expect("timed out waiting for the door");
        Capsule {
            addr,
            session_id,
            workdir,
            operator,
            _server: server,
            _home: home,
            _project: project,
        }
    }

    fn events(&self, event_type: &str) -> Vec<Value> {
        fs::read_to_string(self.workdir.join("trace.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// Blocks until the trace holds a `task_start` for `task_id`, and returns it.
    fn task_start(&self, task_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(event) = self
                .events("task_start")
                .into_iter()
                .find(|event| event["task_id"] == task_id)
            {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "no task_start for {task_id}; trace:\n{}",
                fs::read_to_string(self.workdir.join("trace.jsonl")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Blocks until the trace holds a `task_end` for `task_id`.
    fn task_end(&self, task_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !self
            .events("task_end")
            .iter()
            .any(|event| event["task_id"] == task_id)
        {
            assert!(
                Instant::now() < deadline,
                "no task_end for {task_id}; trace:\n{}",
                fs::read_to_string(self.workdir.join("trace.jsonl")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn card(&self) -> Value {
        let card = request(&self.addr, "GET", "/.well-known/agent-card.json", &PEER, "");
        assert_eq!(card.status, 200, "{card:?}");
        card.json()
    }
}

// ── Raw requests ──────────────────────────────────────────────────────────────

/// `message/send` on `POST /` carrying `headers`.
fn send(addr: &str, headers: &[(&str, &str)], message_id: &str) -> Response {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "message/send",
        "params": message(message_id, "hello from elsewhere"),
    })
    .to_string();
    request(addr, "POST", "/", headers, &body)
}

/// One request written as given, and every byte of the answer up to the door closing the
/// connection.
fn raw_exchange(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("should connect to the door");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer);
    answer
}

fn submitted_id(response: &Response) -> String {
    assert_eq!(response.status, 200, "{response:?}");
    let body = response.json();
    assert_eq!(body["result"]["status"]["state"], "submitted", "{body}");
    body["result"]["id"].as_str().unwrap().to_string()
}

fn assert_peer_refusal(response: &Response) {
    assert_eq!(response.status, 403, "{response:?}");
    assert_eq!(response.body, REFUSAL_BODY, "{response:?}");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{response:?}"
    );
    assert_eq!(response.header("www-authenticate"), None, "{response:?}");
}

fn peer_tasks_param(card: &Value) -> &Value {
    &common::card_extension_params(card, common::DOOR_EXTENSION_URI)["peerTasks"]
}

// ── S1: consent ───────────────────────────────────────────────────────────────

#[test]
fn a_consenting_capsule_takes_a_peer_task_and_records_it_as_peer() {
    if common::skip_without_host_support(
        "a_consenting_capsule_takes_a_peer_task_and_records_it_as_peer",
    ) {
        return;
    }
    let capsule = Capsule::launch("consenting-peer", ACCEPT_YAML);

    let task_id = submitted_id(&send(&capsule.addr, &PEER, "m-peer"));
    let start = capsule.task_start(&task_id);
    assert_eq!(start["origin"], "peer", "{start}");
    assert_eq!(start["trust"], "trusted", "{start}");
    capsule.task_end(&task_id);
    assert_eq!(peer_tasks_param(&capsule.card()), &json!(true));
}

// ── S2, S3: no consent ────────────────────────────────────────────────────────

/// Absent, `accept: false` and a files-only `exports:` block are one posture: every peer task is
/// refused with the same bytes, no challenge, and nothing reaches the loop.
#[test]
fn a_capsule_that_does_not_consent_refuses_a_peer_task() {
    if common::skip_without_host_support("a_capsule_that_does_not_consent_refuses_a_peer_task") {
        return;
    }
    for (name, extra) in [
        ("undeclared-peer", ""),
        ("refusing-peer", REFUSE_YAML),
        ("files-only-peer", FILES_ONLY_YAML),
    ] {
        let capsule = Capsule::launch(name, extra);
        assert_peer_refusal(&send(&capsule.addr, &PEER, "m-peer"));
        // An untrusted claim and a claim with no trust header are the same origin.
        assert_peer_refusal(&send(
            &capsule.addr,
            &[
                (PEER_ORIGIN_HEADER, "peer"),
                (PEER_TRUST_HEADER, "untrusted"),
            ],
            "m-peer-untrusted",
        ));
        assert_peer_refusal(&send(
            &capsule.addr,
            &[(PEER_ORIGIN_HEADER, "PEER")],
            "m-peer-shouted",
        ));
        std::thread::sleep(Duration::from_millis(300));
        assert!(capsule.events("task_start").is_empty(), "{name}");
        assert!(capsule.events("a2a_task_received").is_empty(), "{name}");
        let trace = fs::read_to_string(capsule.workdir.join("trace.jsonl")).unwrap_or_default();
        assert!(!trace.contains("peer_not_accepted"), "{name}: {trace}");
    }
}

// ── S4: one refusal for everything ────────────────────────────────────────────

#[test]
fn the_refusal_is_the_same_bytes_for_every_method_path_and_body() {
    if common::skip_without_host_support(
        "the_refusal_is_the_same_bytes_for_every_method_path_and_body",
    ) {
        return;
    }
    let capsule = Capsule::launch("uniform-refusal", "");
    // A real task, submitted by a caller that claims no origin.
    let real_id = submitted_id(&send(&capsule.addr, &[], "m-real"));
    let rpc_body = |method: &str, params: Value| {
        json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}).to_string()
    };

    let cases: Vec<(&str, &str, &str, String)> = vec![
        (
            "message/send",
            "POST",
            "/",
            rpc_body("message/send", message("m-1", "hi")),
        ),
        (
            "message/stream",
            "POST",
            "/",
            rpc_body("message/stream", message("m-2", "hi")),
        ),
        (
            "tasks/get real",
            "POST",
            "/",
            rpc_body("tasks/get", json!({"id": real_id})),
        ),
        (
            "tasks/get missing",
            "POST",
            "/",
            rpc_body("tasks/get", json!({"id": "tsk_doesnotexist"})),
        ),
        (
            "tasks/cancel",
            "POST",
            "/",
            rpc_body("tasks/cancel", json!({"id": real_id})),
        ),
        ("foo/bar", "POST", "/", rpc_body("foo/bar", json!({}))),
        ("malformed", "POST", "/", "{not json".to_string()),
        (
            "resource plane",
            "GET",
            "/resources/files/anything",
            String::new(),
        ),
        ("no such path", "GET", "/no/such/path", String::new()),
    ];

    let expected = raw_exchange(&capsule.addr, "POST", "/", &PEER, &cases[0].3);
    let text = String::from_utf8_lossy(&expected).to_string();
    assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{text}");
    assert!(text.ends_with(REFUSAL_BODY), "{text}");
    for (label, method, path, body) in &cases {
        let answer = raw_exchange(&capsule.addr, method, path, &PEER, body);
        assert_eq!(
            String::from_utf8_lossy(&answer),
            text,
            "{label} must be answered with the same bytes"
        );
    }

    // A declared body that never arrives: the refusal comes back without the door waiting for it.
    let mut stream = TcpStream::connect(&capsule.addr).expect("should connect to the door");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        stream,
        "POST / HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\n\
         Content-Length: 10000000\r\n{}: peer\r\n{}: trusted\r\n\r\n",
        capsule.addr, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER
    )
    .unwrap();
    let mut unread = Vec::new();
    let _ = stream.read_to_end(&mut unread);
    assert_eq!(
        String::from_utf8_lossy(&unread),
        text,
        "the refusal must not wait for the body"
    );

    // The real task is untouched by the refused cancel.
    let task = rpc(&capsule.addr, None, "tasks/get", json!({"id": real_id}));
    assert_eq!(task.status, 200, "{task:?}");
    assert_ne!(task.json()["result"]["status"]["state"], "canceled");
}

// ── S5: what the gate does not cover ──────────────────────────────────────────

#[test]
fn a_capsule_that_does_not_consent_still_serves_events_completions_planes_and_its_card() {
    if common::skip_without_host_support(
        "a_capsule_that_does_not_consent_still_serves_events_completions_planes_and_its_card",
    ) {
        return;
    }
    let capsule = Capsule::launch("still-serving", "");

    // No origin claim, and an explicit `event` claim: both are events, untrusted.
    for headers in [vec![], vec![(PEER_ORIGIN_HEADER, "event")]] {
        let task_id = submitted_id(&send(&capsule.addr, &headers, "m-event"));
        let start = capsule.task_start(&task_id);
        assert_eq!(start["origin"], "event", "{headers:?}: {start}");
        assert_eq!(start["trust"], "untrusted", "{headers:?}: {start}");
    }

    // The card answers a caller carrying peer headers.
    assert_eq!(peer_tasks_param(&capsule.card()), &json!(false));

    // A completion addressed to this session is a child reporting back, not a peer task.
    let completion = send(
        &capsule.addr,
        &[
            (PEER_ORIGIN_HEADER, "completion"),
            (PEER_TRUST_HEADER, "trusted"),
            (DELEGATION_ID_HEADER, "dlg_0000000000000001"),
            (COMPLETION_SESSION_HEADER, capsule.session_id.as_str()),
        ],
        "m-completion",
    );
    let task_id = submitted_id(&completion);
    assert_eq!(capsule.task_start(&task_id)["origin"], "completion");

    // The peer file plane answers with its own authoriser, whatever the origin claim.
    let peer_plane = request(&capsule.addr, "GET", "/resources/peer/garbage", &PEER, "");
    assert!(
        !peer_plane.body.contains("peer_not_accepted"),
        "{peer_plane:?}"
    );
    let unclaimed = request(&capsule.addr, "GET", "/resources/peer/garbage", &[], "");
    assert_eq!(
        (peer_plane.status, &peer_plane.body),
        (unclaimed.status, &unclaimed.body),
        "the peer plane does not read the origin claim"
    );
}

// ── S6: an authenticated door ─────────────────────────────────────────────────

/// The token comes first and the consent second, so an unauthenticated caller learns nothing the
/// public card does not say; consenting changes nothing about the token check.
#[test]
fn an_authenticated_door_checks_the_token_before_consent() {
    if common::skip_without_host_support("an_authenticated_door_checks_the_token_before_consent") {
        return;
    }
    for (name, extra, consents) in [
        ("auth-refusing", AUTHENTICATION_YAML.to_string(), false),
        (
            "auth-accepting",
            format!("{AUTHENTICATION_YAML}{ACCEPT_YAML}"),
            true,
        ),
    ] {
        let capsule = Capsule::launch(name, &extra);
        let operator = capsule.operator.clone().expect("an operator token");

        let anonymous = send(&capsule.addr, &PEER, "m-anon");
        assert_eq!(anonymous.status, 401, "{name}: {anonymous:?}");
        assert_eq!(anonymous.json()["error"], "unauthenticated", "{name}");

        let bad = send(
            &capsule.addr,
            &[PEER[0], PEER[1], ("Authorization", "Bearer garbage")],
            "m-bad",
        );
        assert_eq!(bad.status, 401, "{name}: {bad:?}");
        assert_eq!(bad.json()["error"], "invalid_token", "{name}");

        let bearer = format!("Bearer {operator}");
        let authenticated = send(
            &capsule.addr,
            &[PEER[0], PEER[1], ("Authorization", bearer.as_str())],
            "m-operator",
        );
        if consents {
            let task_id = submitted_id(&authenticated);
            assert_eq!(capsule.task_start(&task_id)["origin"], "peer");
        } else {
            assert_peer_refusal(&authenticated);
            // The same token without the peer claim is served.
            submitted_id(&send(
                &capsule.addr,
                &[("Authorization", bearer.as_str())],
                "m-operator-event",
            ));
        }
    }
}

// ── S7: the card ──────────────────────────────────────────────────────────────

#[test]
fn the_card_states_the_peer_task_posture() {
    if common::skip_without_host_support("the_card_states_the_peer_task_posture") {
        return;
    }
    for (name, extra, expected) in [
        ("card-undeclared", String::new(), false),
        ("card-refusing", REFUSE_YAML.to_string(), false),
        ("card-accepting", ACCEPT_YAML.to_string(), true),
        ("card-auth-refusing", AUTHENTICATION_YAML.to_string(), false),
        (
            "card-auth-accepting",
            format!("{AUTHENTICATION_YAML}{ACCEPT_YAML}"),
            true,
        ),
    ] {
        let capsule = Capsule::launch(name, &extra);
        let card = capsule.card();
        assert_eq!(
            common::a2a_card_conformance::check_agent_card(&card),
            Ok(()),
            "{card:#}"
        );
        assert_eq!(
            peer_tasks_param(&card),
            &json!(expected),
            "{name}: {card:#}"
        );

        if let Some(operator) = &capsule.operator {
            let extended = rpc(
                &capsule.addr,
                Some(operator),
                "agent/getAuthenticatedExtendedCard",
                json!({}),
            );
            assert_eq!(extended.status, 200, "{extended:?}");
            let extended = extended.json()["result"].clone();
            assert_eq!(
                peer_tasks_param(&extended),
                &json!(expected),
                "{name}: {extended:#}"
            );
            // Consent never changes whether or how the door authenticates.
            assert_eq!(
                card["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
                "Bearer"
            );
        } else {
            assert_eq!(card["securitySchemes"], json!({}), "{name}");
        }
    }
}

// ── S8: nothing but the manifest sets it ──────────────────────────────────────

fn mur(home: &Path, project: &Path, args: &[&str]) -> std::process::Output {
    assert_cmd::Command::cargo_bin("mur")
        .unwrap()
        .env("HOME", home)
        .env_remove("NEXUS_API_KEY")
        .current_dir(project)
        .args(args)
        .output()
        .unwrap()
}

fn combined(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A script capsule: nothing in it needs a driver, and every case refuses or reports before a
/// capsule would run.
fn manifest_only_project(extra: &str) -> TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("murmur.yaml"),
        format!("name: peer-tasks-fixture\nversion: 0.0.1\n{extra}"),
    )
    .unwrap();
    project
}

/// An agent capsule — the control surface is served only on one — with `extra` appended. Never
/// launched: every case refuses at manifest parse, before an artifact is looked for.
fn unlaunched_agent_project(extra: &str) -> TempDir {
    agent_project("http://127.0.0.1:1", "peer-tasks-agent", "", extra)
}

#[test]
fn peer_task_consent_is_not_a_controllable_setting() {
    let home = tempfile::tempdir().unwrap();
    let project = unlaunched_agent_project("control:\n  settings: [exports.peer_tasks]\n");
    for args in [
        &["run", "--manifest", "murmur.yaml", "--explain-scope"][..],
        &["doctor"][..],
    ] {
        let output = mur(home.path(), project.path(), args);
        assert!(!output.status.success(), "{args:?}");
        let text = combined(&output);
        assert!(text.contains("error[E-MAN-003]"), "{args:?}: {text}");
        assert!(text.contains("control.settings"), "{args:?}: {text}");
        assert!(text.contains("exports.peer_tasks"), "{args:?}: {text}");
    }
}

#[test]
fn no_launcher_flag_names_peer_tasks() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // `mur deploy` exists only in a build with its beta feature compiled in.
    let commands: &[&str] = if cfg!(feature = "beta-mur-deploy") {
        &["run", "deploy"]
    } else {
        &["run"]
    };
    for &command in commands {
        let output = mur(home.path(), project.path(), &[command, "--help"]);
        assert!(output.status.success(), "{command}");
        let help = combined(&output).to_lowercase();
        for needle in ["peer-task", "peer_task", "peer task"] {
            assert!(!help.contains(needle), "mur {command} --help: {help}");
        }
    }
}

// ── S9: a malformed declaration ───────────────────────────────────────────────

#[test]
fn a_peer_tasks_block_without_accept_is_refused_by_run_and_doctor() {
    let home = tempfile::tempdir().unwrap();
    let project = manifest_only_project("exports:\n  peer_tasks: {}\n");
    for args in [
        &["run", "--manifest", "murmur.yaml"][..],
        &["run", "--manifest", "murmur.yaml", "--explain-scope"][..],
        &["doctor"][..],
    ] {
        let output = mur(home.path(), project.path(), args);
        assert!(!output.status.success(), "{args:?}");
        let text = combined(&output);
        assert!(text.contains("error[E-MAN-003]"), "{args:?}: {text}");
        assert!(
            text.contains("exports.peer_tasks.accept"),
            "{args:?}: {text}"
        );
        assert!(text.contains("never inferred"), "{args:?}: {text}");
    }
}

#[test]
fn a_peer_tasks_value_of_the_wrong_type_is_refused_naming_it() {
    let home = tempfile::tempdir().unwrap();
    for (extra, field) in [
        (
            "exports:\n  peer_tasks:\n    accept: \"yes\"\n",
            "exports.peer_tasks.accept",
        ),
        ("exports:\n  peer_tasks: true\n", "exports.peer_tasks"),
    ] {
        let project = manifest_only_project(extra);
        for args in [&["run", "--manifest", "murmur.yaml"][..], &["doctor"][..]] {
            let output = mur(home.path(), project.path(), args);
            assert!(!output.status.success(), "{extra:?} {args:?}");
            let text = combined(&output);
            assert!(text.contains("E-MAN-"), "{extra:?} {args:?}: {text}");
            assert!(text.contains(field), "{extra:?} {args:?}: {text}");
        }
    }
}

#[test]
fn an_unknown_peer_tasks_key_is_warned_by_run_and_doctor() {
    let home = tempfile::tempdir().unwrap();
    let project =
        manifest_only_project("exports:\n  peer_tasks:\n    accept: true\n    peers: [a]\n");
    for args in [
        &["run", "--manifest", "murmur.yaml", "--explain-scope"][..],
        &["doctor"][..],
    ] {
        let text = combined(&mur(home.path(), project.path(), args));
        let warning = text
            .lines()
            .find(|line| line.contains("W-SEC-019"))
            .unwrap_or_else(|| panic!("{args:?}: no W-SEC-019 in {text}"));
        assert!(
            warning.contains("'peers' in exports.peer_tasks"),
            "{args:?}: {warning}"
        );
        if args[0] == "run" {
            assert!(
                text.contains("  exports.peer_tasks: accept\n"),
                "the block still consents: {text}"
            );
        }
    }
}

// ── S11: the scope report ─────────────────────────────────────────────────────

#[test]
fn explain_scope_reports_the_peer_task_posture() {
    let home = tempfile::tempdir().unwrap();
    let mut reports = Vec::new();
    for (extra, accepts) in [("", false), (REFUSE_YAML, false), (ACCEPT_YAML, true)] {
        let project = manifest_only_project(extra);
        let args = ["run", "--manifest", "murmur.yaml", "--explain-scope"];
        let text = mur(home.path(), project.path(), &args);
        assert!(text.status.success(), "{}", combined(&text));
        let line = if accepts {
            "  exports.peer_tasks: accept\n"
        } else {
            "  exports.peer_tasks: refuse\n"
        };
        assert!(
            combined(&text).contains(line),
            "{extra:?}: {}",
            combined(&text)
        );

        let json_out = mur(
            home.path(),
            project.path(),
            &[
                "run",
                "--manifest",
                "murmur.yaml",
                "--explain-scope",
                "--json",
            ],
        );
        assert!(json_out.status.success(), "{}", combined(&json_out));
        let report: Value =
            serde_json::from_str(String::from_utf8_lossy(&json_out.stdout).trim()).unwrap();
        assert_eq!(report["peer_tasks"], json!(accepts), "{extra:?}: {report}");
        reports.push(report);
    }
    for key in ["achieved_containment", "enforcement_tier", "floor_met"] {
        assert_eq!(reports[0][key], reports[2][key], "{key} must not move");
    }
}

#[test]
fn session_start_records_the_peer_task_posture() {
    if common::skip_without_host_support("session_start_records_the_peer_task_posture") {
        return;
    }
    for (name, extra, accepts) in [
        ("grants-undeclared", "", false),
        ("grants-accepting", ACCEPT_YAML, true),
    ] {
        let capsule = Capsule::launch(name, extra);
        let deadline = Instant::now() + Duration::from_secs(30);
        let start = loop {
            if let Some(start) = capsule.events("session_start").into_iter().next() {
                break start;
            }
            assert!(Instant::now() < deadline, "{name}: no session_start");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(
            start["effective_grants"]["peer_tasks"],
            json!(accepts),
            "{name}: {start}"
        );
    }
}
