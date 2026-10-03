//! The door of a formation member, end to end: what a formation token reaches at the door it was
//! issued for, what it is refused at every other door, and what a token that does not verify gets.
//!
//! Each member is an in-process authenticated session staged with the `FormationMember` one
//! `FormationAuthority` built for it over one roster — `planner` may call `coder`, and `coder`
//! may call `reviewer` — exactly as `mur run` builds it from the bundle on its channel.

#[path = "common/mod.rs"]
mod common;

use std::{sync::Arc, time::Duration};

use base64::Engine as _;
use capsule_runtime::mac_token::B64;
use capsule_runtime::{FormationAuthority, FormationId, FormationMember};
use common::door_capsule::{
    agent_project, bearer, driver_home, end_turn, launch_door, message, request, rpc, stage_door,
    trace_events, Response, QUEUE_SLEEP_YAML,
};
use serde_json::{json, Value};
use tempfile::TempDir;

const AUTHENTICATED: &str = "network:\n  authentication:\n    scheme: bearer\n";
const CONSENTS: &str =
    "exports:\n  peer_tasks:\n    accept: true\n  files:\n    root: out\n    mode: read-only\n";

/// One running member: its door, its session directory and its operator token.
struct Door {
    addr: String,
    workdir: std::path::PathBuf,
    operator: String,
    _project: TempDir,
}

/// A queue+sleep authenticated member named `name` — consenting to peer tasks when `consents` —
/// staged as `member`, and launched.
fn door(
    home: &TempDir,
    endpoint: &str,
    name: &str,
    consents: bool,
    member: Option<Arc<FormationMember>>,
) -> Door {
    let exports = if consents { CONSENTS } else { "" };
    let project = agent_project(
        endpoint,
        name,
        "",
        &format!("{QUEUE_SLEEP_YAML}{exports}{AUTHENTICATED}"),
    );
    let staged = stage_door(home, &project.path().join("murmur.yaml"), member);
    let workdir = staged.workdir.clone();
    let operator = staged.door_tokens()[0].1.expose().to_string();
    Door {
        addr: launch_door(staged),
        workdir,
        operator,
        _project: project,
    }
}

fn member(authority: &FormationAuthority, name: &str, callees: &[&str]) -> Arc<FormationMember> {
    Arc::new(FormationMember::from_bundle(
        authority.member_bundle(name, callees),
    ))
}

fn send_with(addr: &str, authorization: &str, headers: &[(&str, &str)]) -> Response {
    let mut all = vec![("Authorization", authorization)];
    all.extend_from_slice(headers);
    request(
        addr,
        "POST",
        "/",
        &all,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "message/send", "params": message("m", "hello")})
            .to_string(),
    )
}

/// The 401 / 403 matrix of formation tokens at formation members' doors.
#[test]
fn a_formation_token_reaches_its_audience_and_nothing_else() {
    if common::skip_without_host_support("a_formation_token_reaches_its_audience_and_nothing_else")
    {
        return;
    }
    let server = common::ScriptedServer::start(
        (1..=8)
            .map(|n| end_turn(n, "done"))
            .collect::<Vec<String>>(),
    );
    let home = driver_home();
    let authority = FormationAuthority::generate(&FormationId::mint()).unwrap();
    let coder = door(
        &home,
        &server.endpoint,
        "coder",
        true,
        Some(member(&authority, "coder", &["reviewer"])),
    );
    let reviewer = door(
        &home,
        &server.endpoint,
        "reviewer",
        true,
        Some(member(&authority, "reviewer", &[])),
    );
    let planner_bundle = authority.member_bundle("planner", &["coder"]);
    let token = planner_bundle.calls[0].1.expose().to_string();
    let garbage = send_with(&coder.addr, "Bearer garbage", &[]);
    assert_eq!(garbage.status, 401, "{garbage:?}");
    assert_eq!(garbage.json()["error"], "invalid_token");

    // (a) At its audience: a task, and its state.
    let sent = rpc(
        &coder.addr,
        Some(&token),
        "message/send",
        message("m-a", "hi"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["id"].as_str().unwrap().to_string();
    let got = rpc(
        &coder.addr,
        Some(&token),
        "tasks/get",
        json!({"id": task_id}),
    );
    assert_eq!(got.status, 200, "{got:?}");
    assert!(got.json()["result"]["id"].is_string(), "{got:?}");
    // Every other method is the ordinary scope refusal, naming the member credential.
    for (method, path, scope) in [
        ("tasks/cancel", "/", "tasks/cancel"),
        ("session/stop", "/", "session/stop"),
        ("", "/resources/files/x", "resources/files"),
    ] {
        let refused = if method.is_empty() {
            request(
                &coder.addr,
                "GET",
                path,
                &[("Authorization", &bearer(&token))],
                "",
            )
        } else {
            rpc(&coder.addr, Some(&token), method, json!({"id": task_id}))
        };
        assert_eq!(refused.status, 403, "{scope}: {refused:?}");
        let body = refused.json();
        assert_eq!(body["error"], "insufficient_scope", "{refused:?}");
        let text = body["message"].as_str().unwrap();
        assert!(
            text.contains("member:planner") && text.contains(scope),
            "{text}"
        );
    }

    // (b) The same token at a door it was not issued for: 403 not_permitted, naming only the
    // member that presented it.
    let elsewhere = send_with(&reviewer.addr, &bearer(&token), &[]);
    assert_eq!(elsewhere.status, 403, "{elsewhere:?}");
    assert_eq!(
        elsewhere.header("www-authenticate"),
        Some("Bearer realm=\"reviewer\", error=\"insufficient_scope\"")
    );
    let body = elsewhere.json();
    assert_eq!(body["error"], "not_permitted");
    assert!(elsewhere.body.contains("planner"), "{}", elsewhere.body);
    assert!(
        !elsewhere.body.contains("reviewer") && !elsewhere.body.contains("coder"),
        "{}",
        elsewhere.body
    );

    // (c) Every token that does not verify is the garbage token's 401, byte for byte.
    let other = FormationAuthority::generate(&FormationId::mint()).unwrap();
    let foreign = other.member_bundle("planner", &["coder"]).calls[0]
        .1
        .expose()
        .to_string();
    let segments: Vec<&str> = token.split('.').collect();
    let readdressed = format!(
        "mft1.{}.{}",
        B64.encode(format!(
            r#"{{"formation":"{}","from":"planner","to":"reviewer"}}"#,
            authority.formation_id()
        )),
        segments[2]
    );
    let mut signature = B64.decode(segments[2]).unwrap();
    signature[0] ^= 0x01;
    let flipped = format!("mft1.{}.{}", segments[1], B64.encode(&signature));
    for forged in [&foreign, &readdressed, &flipped] {
        let refused = send_with(&coder.addr, &bearer(forged), &[]);
        assert_eq!(refused.status, 401, "{refused:?}");
        assert_eq!(refused.body, garbage.body);
        assert_eq!(
            refused.header("www-authenticate"),
            garbage.header("www-authenticate")
        );
    }
    let readdressed_at_reviewer = send_with(&reviewer.addr, &bearer(&readdressed), &[]);
    assert_eq!(readdressed_at_reviewer.status, 401);

    // (d) A session that is no member refuses every formation token.
    let outsider = door(&home, &server.endpoint, "coder", true, None);
    let refused = send_with(&outsider.addr, &bearer(&token), &[]);
    assert_eq!(refused.status, 401, "{refused:?}");
    assert_eq!(refused.json()["error"], "invalid_token");

    // (e) The operator's door token is unchanged.
    let operator = rpc(
        &coder.addr,
        Some(&coder.operator),
        "message/send",
        message("m-op", "hi"),
    );
    assert_eq!(operator.status, 200, "{operator:?}");

    // (f) A member that does not consent refuses a peer task on a valid formation token.
    let auditor = door(
        &home,
        &server.endpoint,
        "auditor",
        false,
        Some(member(&authority, "auditor", &[])),
    );
    let to_auditor = authority.member_bundle("planner", &["auditor"]).calls[0]
        .1
        .expose()
        .to_string();
    let refused = send_with(
        &auditor.addr,
        &bearer(&to_auditor),
        &[
            ("x-murmur-task-origin", "peer"),
            ("x-murmur-task-trust", "trusted"),
        ],
    );
    assert_eq!(refused.status, 403, "{refused:?}");
    assert_eq!(refused.json()["error"], "peer_not_accepted");

    // The task the formation token started is recorded as `planner`'s; the operator's is no
    // member's.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let received = loop {
        let received: Vec<Value> = trace_events(&coder.workdir)
            .into_iter()
            .filter(|event| event["event_type"] == "a2a_task_received")
            .collect();
        if received.len() >= 2 || std::time::Instant::now() >= deadline {
            break received;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let callers: Vec<Option<&str>> = received
        .iter()
        .map(|event| event["caller_member"].as_str())
        .collect();
    assert_eq!(callers, [Some("planner"), None], "{received:?}");
    let start = &trace_events(&coder.workdir)[0];
    assert_eq!(start["formation_member"], "coder");
    assert_eq!(start["formation_callees"], json!(["reviewer"]));
    for workdir in [&coder.workdir, &reviewer.workdir, &auditor.workdir] {
        assert!(
            common::door_capsule::files_containing(workdir, "mft1.").is_empty(),
            "{}",
            workdir.display()
        );
    }
}
