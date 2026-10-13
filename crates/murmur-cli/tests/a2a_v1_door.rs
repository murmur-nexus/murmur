//! The door speaks A2A v1.0 and nothing else, end to end on an in-process door: it negotiates
//! `A2A-Version` on every JSON-RPC method it serves, answers the v1.0 method names only, serves
//! the v1.0 extended card, and answers its errors from the A2A table with a `google.rpc.ErrorInfo`.

#[path = "common/mod.rs"]
mod common;

use std::time::Duration;

use common::door_capsule::{
    agent_project, driver_home, end_turn, launch_door, message, request, rpc, stage_door,
    wait_completed, Response, A2A_VERSION, AUTHENTICATION_YAML, QUEUE_SLEEP_YAML,
};
use common::{DOOR_EXTENSION_URI, STREAM_EXTENSION_URI};
use serde_json::{json, Value};

const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// Every JSON-RPC method an authenticated door serves, with params it takes.
fn served_methods(task_id: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("SendMessage", message("m-send", "send")),
        ("SendStreamingMessage", message("m-stream", "stream")),
        ("GetTask", json!({"id": task_id})),
        ("CancelTask", json!({"id": task_id})),
        ("GetExtendedAgentCard", json!({})),
        ("murmur.WatchSession", json!({})),
        ("murmur.StopSession", json!({})),
    ]
}

/// The A2A 0.3 method names, each of which a v1.0 door answers `-32601`.
const A2A_0_3_NAMES: [&str; 5] = [
    "message/send",
    "message/stream",
    "tasks/get",
    "tasks/cancel",
    "agent/getAuthenticatedExtendedCard",
];

/// An in-process door, the tokens it minted and what must outlive it.
struct Door {
    addr: String,
    tokens: Vec<(String, String)>,
    provider: common::ScriptedServer,
    _home: tempfile::TempDir,
    _project: tempfile::TempDir,
}

impl Door {
    /// A door named `name` with `extra` appended to its manifest, against `provider`.
    fn launch(name: &str, extra: &str, provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let project = agent_project(&provider.endpoint, name, "", extra);
        let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
        let tokens = staged
            .door_tokens()
            .into_iter()
            .map(|(name, token)| (name, token.expose().to_string()))
            .collect();
        Door {
            addr: launch_door(staged),
            tokens,
            provider,
            _home: home,
            _project: project,
        }
    }

    /// The token of credential `name`.
    fn token(&self, name: &str) -> String {
        self.tokens
            .iter()
            .find(|(credential, _)| credential == name)
            .map(|(_, token)| token.clone())
            .unwrap_or_else(|| panic!("no {name} token"))
    }
}

/// One JSON-RPC request with `id`, presenting `token` and exactly `versions` as `A2A-Version`
/// lines under the header name `name`.
fn versioned(
    addr: &str,
    token: Option<&str>,
    name: &str,
    versions: &[&str],
    id: u64,
    method: &str,
    params: Value,
) -> Response {
    let authorization = token.map(|token| format!("Bearer {token}"));
    let mut headers: Vec<(&str, &str)> = authorization
        .as_deref()
        .map(|value| ("Authorization", value))
        .into_iter()
        .collect();
    headers.extend(versions.iter().map(|version| (name, *version)));
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    request(addr, "POST", "/", &headers, &body)
}

/// The response's JSON-RPC error and its one `ErrorInfo`, asserting it is framed as one JSON
/// body, not an event stream.
fn error_of(response: &Response) -> (Value, Value) {
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{response:?}"
    );
    let body = response.json();
    let error = body["error"].clone();
    assert!(error.is_object(), "{response:?}");
    let data = error["data"].as_array().cloned().unwrap_or_default();
    assert_eq!(data.len(), 1, "one ErrorInfo: {response:?}");
    assert_eq!(data[0]["@type"], ERROR_INFO, "{response:?}");
    (error, data[0].clone())
}

/// Every task id an event stream's frames name.
fn stream_task_ids(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .flat_map(|event| {
            event["result"]
                .as_object()
                .into_iter()
                .flat_map(|members| members.values())
                .flat_map(|body| [body["id"].clone(), body["taskId"].clone()])
                .collect::<Vec<_>>()
        })
        .filter_map(|id| id.as_str().map(str::to_string))
        .filter(|id| id.starts_with("tsk_"))
        .collect()
}

/// A JSON-RPC result, or an event stream that wrote at least one event.
fn assert_served(method: &str, response: &Response) {
    assert_eq!(response.status, 200, "{method}: {response:?}");
    if response.header("content-type") == Some("text/event-stream") {
        assert!(response.body.contains("data:"), "{method}: {response:?}");
    } else {
        assert!(
            response.json().get("result").is_some(),
            "{method} was not served: {response:?}"
        );
    }
}

#[test]
fn a2a_v1_door_negotiation_serves_1_0_alone_on_every_method() {
    if common::skip_without_host_support("a2a_v1_door_negotiation_serves_1_0_alone_on_every_method")
    {
        return;
    }
    // Each served round starts two tasks. Every answer takes a moment, so the task just sent is
    // still live when it is cancelled, and short enough that a stream never goes the second quiet
    // `request` reads it until.
    let provider = common::ScriptedServer::start_with_delay(
        (1..=10).map(|n| end_turn(n, "done")).collect(),
        Duration::from_millis(300),
    );
    let door = Door::launch(
        "negotiating-agent",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        provider,
    );
    let addr = door.addr.clone();
    let (operator, watcher) = (door.token("operator"), door.token("watcher"));
    let token = Some(operator.as_str());

    // The refused round: nothing it sends starts or stops anything.
    let refused_versions: [(&[&str], &str); 8] = [
        (&[], ""),
        (&[""], ""),
        (&["0.3"], "0.3"),
        (&["2.0"], "2.0"),
        (&["1"], "1"),
        (&["1.1"], "1.1"),
        (&["1.0x"], "1.0x"),
        (&["1.0", "1.0"], "1.0, 1.0"),
    ];
    let mut id = 0;
    for (method, params) in served_methods("tsk_none") {
        for (versions, requested) in refused_versions {
            id += 1;
            let response = versioned(
                &addr,
                token,
                "A2A-Version",
                versions,
                id,
                method,
                params.clone(),
            );
            let (error, info) = error_of(&response);
            let context = format!("{method} with {versions:?}: {response:?}");
            assert_eq!(response.json()["id"], id, "{context}");
            assert_eq!(error["code"], -32009, "{context}");
            assert_eq!(info["reason"], "VERSION_NOT_SUPPORTED", "{context}");
            assert_eq!(info["domain"], "a2a-protocol.org", "{context}");
            assert_eq!(
                info["metadata"],
                json!({"requestedVersion": requested, "supportedVersions": "1.0"}),
                "{context}"
            );
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        door.provider.requests().is_empty(),
        "a refused request started a task: {:?}",
        door.provider.requests()
    );
    let still = rpc(&addr, token, "GetTask", json!({"id": "tsk_none"}));
    assert_eq!(still.json()["error"]["code"], -32001, "{still:?}");

    // The refusal order: the token first, then the version, ahead of the scope and the method.
    let unauthenticated = versioned(&addr, None, "A2A-Version", &[], 1, "GetTask", json!({}));
    assert_eq!(unauthenticated.status, 401, "{unauthenticated:?}");
    let unscoped = versioned(
        &addr,
        Some(&watcher),
        "A2A-Version",
        &[],
        1,
        "SendMessage",
        message("m-watcher", "hello"),
    );
    assert_eq!(error_of(&unscoped).0["code"], -32009, "{unscoped:?}");
    let retired = versioned(
        &addr,
        token,
        "A2A-Version",
        &[],
        1,
        "tasks/get",
        json!({"id": "tsk_none"}),
    );
    assert_eq!(error_of(&retired).0["code"], -32009, "{retired:?}");

    // The served round: every method under every accepted spelling, `murmur.StopSession` last. Every
    // task it starts is recorded, so the stop can be held to cancelling only those.
    let mut started: Vec<String> = Vec::new();
    let accepted: [(&str, &str); 4] = [
        ("A2A-Version", "1.0"),
        ("A2A-Version", " 1.0 "),
        ("A2A-Version", "1.0.1"),
        ("a2a-version", "1.0"),
    ];
    for (name, version) in accepted {
        let sent = versioned(
            &addr,
            token,
            name,
            &[version],
            1,
            "SendMessage",
            message("m-served", "served"),
        );
        assert_served("SendMessage", &sent);
        let task_id = sent.json()["result"]["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        started.push(task_id.clone());
        // The task just sent is still live, so its cancel is accepted.
        for method in [
            "GetTask",
            "CancelTask",
            "SendStreamingMessage",
            "GetExtendedAgentCard",
            "murmur.WatchSession",
        ] {
            let (_, params) = served_methods(&task_id)
                .into_iter()
                .find(|(served, _)| *served == method)
                .unwrap();
            let response = versioned(&addr, token, name, &[version], 1, method, params);
            assert_served(&format!("{method} with {name}: {version:?}"), &response);
            if method == "SendStreamingMessage" {
                started.extend(stream_task_ids(&response.body));
            }
        }
    }
    for (name, version) in accepted {
        let stopped = versioned(
            &addr,
            token,
            name,
            &[version],
            1,
            "murmur.StopSession",
            json!({}),
        );
        assert_served("murmur.StopSession", &stopped);
        for canceled in stopped.json()["result"]["canceled"].as_array().unwrap() {
            assert!(
                started.iter().any(|task| canceled == task.as_str()),
                "murmur.StopSession canceled {canceled}, which the served round did not start: \
                 {started:?}"
            );
        }
    }
}

#[test]
fn a2a_v1_door_names_are_the_v1_names_alone() {
    if common::skip_without_host_support("a2a_v1_door_names_are_the_v1_names_alone") {
        return;
    }
    let authenticated_door = Door::launch(
        "named-agent",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let public_door = Door::launch(
        "public-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let (authenticated, public) = (&authenticated_door.addr, &public_door.addr);
    let operator = authenticated_door.token("operator");

    for (addr, token) in [(authenticated, Some(operator.as_str())), (public, None)] {
        for name in A2A_0_3_NAMES {
            let response = rpc(addr, token, name, json!({"id": "tsk_x"}));
            assert_eq!(response.status, 200, "{name}: {response:?}");
            let body = response.json();
            assert_eq!(body["error"]["code"], -32601, "{name}: {body}");
            assert_eq!(
                body["error"]["message"], "Method not found",
                "{name}: {body}"
            );
        }
    }

    let card_of = |addr: &str| {
        let card = request(
            addr,
            "GET",
            "/.well-known/agent-card.json",
            &[A2A_VERSION],
            "",
        );
        assert_eq!(card.status, 200, "{card:?}");
        card.json()
    };
    let authenticated_card = card_of(authenticated);
    let public_card = card_of(public);
    for card in [&authenticated_card, &public_card] {
        assert_eq!(
            card["supportedInterfaces"][0]["protocolVersion"], "1.0",
            "{card:#}"
        );
    }
    let all = [
        "SendMessage",
        "SendStreamingMessage",
        "murmur.WatchSession",
        "GetTask",
        "CancelTask",
        "murmur.StopSession",
        "GetExtendedAgentCard",
    ];
    assert_eq!(common::card_door_methods(&authenticated_card), all);
    assert_eq!(common::card_door_methods(&public_card), all[..6]);

    // The spec's own request carries no `params`.
    let body = json!({"jsonrpc": "2.0", "id": "card", "method": "GetExtendedAgentCard"});
    let bearer = format!("Bearer {operator}");
    let extended = request(
        authenticated,
        "POST",
        "/",
        &[("Authorization", bearer.as_str()), A2A_VERSION],
        &body.to_string(),
    );
    assert_eq!(extended.status, 200, "{extended:?}");
    let extended = extended.json();
    assert_eq!(extended["id"], "card");
    let card = &extended["result"];
    assert_eq!(
        common::a2a_conformance::check_agent_card(card),
        Ok(()),
        "{card:#}"
    );
    assert_eq!(card["supportedInterfaces"][0]["protocolVersion"], "1.0");
    assert_eq!(card["capabilities"]["extendedAgentCard"], true);
    assert!(common::card_capsule_params(card)["sessionId"]
        .as_str()
        .is_some_and(|session| session.starts_with("ses_")));
    for key in [
        "url",
        "preferredTransport",
        "security",
        "supportsAuthenticatedExtendedCard",
        "protocolVersion",
    ] {
        assert!(card.get(key).is_none(), "{key}: {card:#}");
    }

    let unsupported = rpc(public, None, "GetExtendedAgentCard", json!({}));
    let (error, info) = error_of(&unsupported);
    assert_eq!(error["code"], -32004, "{unsupported:?}");
    assert_eq!(info["reason"], "UNSUPPORTED_OPERATION", "{unsupported:?}");
    assert_eq!(info["domain"], "a2a-protocol.org", "{unsupported:?}");
}

#[test]
fn a2a_v1_door_errors_are_the_a2a_table_with_error_info() {
    if common::skip_without_host_support("a2a_v1_door_errors_are_the_a2a_table_with_error_info") {
        return;
    }
    let door = Door::launch(
        "erring-agent",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        common::ScriptedServer::start(vec![end_turn(1, "done"), end_turn(2, "spare")]),
    );
    let addr = door.addr.clone();
    let operator = door.token("operator");
    let token = Some(operator.as_str());

    let sent = rpc(&addr, token, "SendMessage", message("m-done", "done"));
    let task_id = sent.json()["result"]["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let completed = wait_completed(&addr, token, &task_id);

    // Cancelling a task that has ended: not cancelable, and left as it ended.
    let ended = rpc(&addr, token, "CancelTask", json!({"id": task_id}));
    let (error, info) = error_of(&ended);
    assert_eq!(error["code"], -32002, "{ended:?}");
    assert_eq!(info["reason"], "TASK_NOT_CANCELABLE");
    assert_eq!(info["domain"], "a2a-protocol.org");
    assert_eq!(
        info["metadata"],
        json!({"taskId": task_id, "state": completed["result"]["status"]["state"]})
    );
    assert_eq!(info["metadata"]["state"], "TASK_STATE_COMPLETED");
    let after = rpc(&addr, token, "GetTask", json!({"id": task_id}));
    assert_eq!(after.json()["result"], completed["result"], "{after:?}");

    let unknown = rpc(&addr, token, "GetTask", json!({"id": "tsk_no_such_task"}));
    let (error, info) = error_of(&unknown);
    assert_eq!(error["code"], -32001, "{unknown:?}");
    assert_eq!(info["reason"], "TASK_NOT_FOUND");
    assert_eq!(info["metadata"], json!({"taskId": "tsk_no_such_task"}));

    let bearer = format!("Bearer {operator}");
    let headers = [("Authorization", bearer.as_str()), A2A_VERSION];
    let raw = |body: &str| request(&addr, "POST", "/", &headers, body).json();
    for (body, id) in [
        (
            r#"{"jsonrpc":"1.0","id":1,"method":"GetTask","params":{}}"#,
            json!(1),
        ),
        (r#"{"id":2,"method":"GetTask","params":{}}"#, json!(2)),
        (
            r#"[{"jsonrpc":"2.0","id":3,"method":"GetTask"}]"#,
            Value::Null,
        ),
        (r#"{"jsonrpc":"2.0","id":4,"method":7}"#, json!(4)),
        (
            r#"{"jsonrpc":"2.0","id":{"a":1},"method":"GetTask"}"#,
            Value::Null,
        ),
        (r#"{"jsonrpc":"2.0","method":"GetTask"}"#, Value::Null),
    ] {
        let answer = raw(body);
        assert_eq!(answer["error"]["code"], -32600, "{body}: {answer}");
        assert_eq!(
            answer["error"]["message"], "Request payload validation error",
            "{body}: {answer}"
        );
        assert_eq!(answer["id"], id, "{body}: {answer}");
    }

    let unparsed = raw("{not json");
    assert_eq!(unparsed["error"]["code"], -32700, "{unparsed}");
    assert_eq!(unparsed["error"]["message"], "Invalid JSON payload");
    assert_eq!(unparsed["id"], Value::Null);

    let listed = raw(r#"{"jsonrpc":"2.0","id":5,"method":"GetTask","params":[1]}"#);
    assert_eq!(listed["error"]["code"], -32602, "{listed}");
    assert_eq!(listed["id"], 5);
}

/// What the door extension says about itself on every card.
const DOOR_EXTENSION_DESCRIPTION: &str =
    "Every JSON-RPC method this door answers, A2A methods and \
     murmur's own alike, the murmur ones named murmur.<Method>; whether it accepts tasks from \
     peer capsules; and the forgetSession field a message's metadata may carry under this URI.";

/// The card at `addr`, read without a token.
fn public_card(addr: &str) -> Value {
    let card = request(addr, "GET", "/.well-known/agent-card.json", &[], "");
    assert_eq!(card.status, 200, "{card:?}");
    card.json()
}

/// The door extension's entry on `card`.
fn door_extension(card: &Value) -> Value {
    card["capabilities"]["extensions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|extension| extension["uri"] == DOOR_EXTENSION_URI)
        .cloned()
        .unwrap_or_else(|| panic!("no door extension: {card:#}"))
}

/// murmur's own methods are `murmur.WatchSession` and `murmur.StopSession`: listed on every card
/// in the old names' places, served under `A2A-Version: 1.0` alone, gated by their own scopes,
/// and answered with the extensions their use activates. The old names, and every other
/// spelling, are methods the door does not have.
#[test]
fn murmur_methods_are_listed_served_and_negotiated() {
    if common::skip_without_host_support("murmur_methods_are_listed_served_and_negotiated") {
        return;
    }
    let door = Door::launch(
        "murmur-methods-agent",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let public_door = Door::launch(
        "murmur-methods-public",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let addr = door.addr.clone();
    let (operator, watcher) = (door.token("operator"), door.token("watcher"));
    let token = Some(operator.as_str());

    // The cards. `door_discovery` holds the card of a door that starts no task.
    let public_methods = [
        "SendMessage",
        "SendStreamingMessage",
        "murmur.WatchSession",
        "GetTask",
        "CancelTask",
        "murmur.StopSession",
    ];
    let mut authenticated_methods = public_methods.to_vec();
    authenticated_methods.push("GetExtendedAgentCard");
    let extended = rpc(&addr, token, "GetExtendedAgentCard", json!({})).json()["result"].clone();
    for (card, methods) in [
        (public_card(&public_door.addr), &public_methods[..]),
        (public_card(&addr), &authenticated_methods[..]),
        (extended, &authenticated_methods[..]),
    ] {
        assert_eq!(common::card_door_methods(&card), methods, "{card:#}");
        let extension = door_extension(&card);
        assert_eq!(extension["description"], DOOR_EXTENSION_DESCRIPTION);
        assert_eq!(extension["required"], false);
    }

    // `murmur.StopSession`: murmur's own result, echoing the door extension.
    let stopped = rpc(&addr, token, "murmur.StopSession", json!({}));
    assert_eq!(stopped.status, 200, "{stopped:?}");
    assert_eq!(stopped.header("content-type"), Some("application/json"));
    assert_eq!(stopped.header("a2a-extensions"), Some(DOOR_EXTENSION_URI));
    let result = stopped.json()["result"].clone();
    let mut keys: Vec<&str> = result
        .as_object()
        .unwrap_or_else(|| panic!("{stopped:?}"))
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["canceled", "residue", "session_id"], "{result}");
    assert!(result["session_id"]
        .as_str()
        .is_some_and(|session| session.starts_with("ses_")));

    // `murmur.WatchSession`: the session's stream, echoing both extensions, door first. The
    // watcher's token holds exactly `GetTask` and `murmur.WatchSession`.
    for watching in [operator.as_str(), watcher.as_str()] {
        let watched = rpc(&addr, Some(watching), "murmur.WatchSession", json!({}));
        assert_eq!(watched.status, 200, "{watched:?}");
        assert_eq!(watched.header("content-type"), Some("text/event-stream"));
        assert_eq!(
            watched.header("a2a-extensions"),
            Some(format!("{DOOR_EXTENSION_URI}, {STREAM_EXTENSION_URI}").as_str()),
            "{watched:?}"
        );
        let events = common::stream::parse(&watched.body);
        assert_eq!(
            events.first().map(|event| event.kind()).as_deref(),
            Some("connection-ack"),
            "{events:#?}"
        );
    }
    let unscoped = rpc(&addr, Some(&watcher), "murmur.StopSession", json!({}));
    assert_eq!(unscoped.status, 403, "{unscoped:?}");
    assert!(
        unscoped
            .header("www-authenticate")
            .is_some_and(|challenge| challenge.contains("insufficient_scope")),
        "{unscoped:?}"
    );
    assert_eq!(unscoped.header("a2a-extensions"), None, "{unscoped:?}");

    // Both need `A2A-Version: 1.0`, and a refusal echoes nothing.
    for method in ["murmur.WatchSession", "murmur.StopSession"] {
        for versions in [&[][..], &["0.3"][..]] {
            let refused = versioned(&addr, token, "A2A-Version", versions, 7, method, json!({}));
            let (error, info) = error_of(&refused);
            assert_eq!(error["code"], -32009, "{method} {versions:?}: {refused:?}");
            assert_eq!(info["reason"], "VERSION_NOT_SUPPORTED");
            assert_eq!(refused.header("a2a-extensions"), None, "{refused:?}");
        }
    }

    // No other name reaches either method.
    for (door_addr, door_token) in [(addr.as_str(), token), (public_door.addr.as_str(), None)] {
        for name in [
            "stream/watch",
            "session/stop",
            "murmur.watchsession",
            " murmur.StopSession",
            "WatchSession",
            "StopSession",
        ] {
            let response = rpc(door_addr, door_token, name, json!({}));
            assert_eq!(response.status, 200, "{name}: {response:?}");
            let body = response.json();
            assert_eq!(body["error"]["code"], -32601, "{name}: {body}");
            assert_eq!(
                body["error"]["message"], "Method not found",
                "{name}: {body}"
            );
            assert!(body["error"].get("data").is_none(), "{name}: {body}");
            assert_eq!(response.header("a2a-extensions"), None, "{name}");
        }
    }
}

/// A message's `forgetSession` asks a capsule to forget a harness session, which only a
/// `transport: process` capsule keeps: this http capsule refuses it as invalid params, on
/// `SendStreamingMessage` as one JSON body before any event stream, and starts nothing. A
/// message that uses the door extension without asking for a forget is served, and the stream's
/// head lists the door extension ahead of the stream extension.
#[test]
fn forget_session_is_refused_by_an_http_capsule_before_any_stream() {
    if common::skip_without_host_support(
        "forget_session_is_refused_by_an_http_capsule_before_any_stream",
    ) {
        return;
    }
    let door = Door::launch(
        "forget-refusing-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start(vec![end_turn(1, "kept"), end_turn(2, "kept")]),
    );
    let addr = door.addr.clone();
    let with_forget = |message_id: &str, forget: Value| {
        let mut params = message(message_id, "hello");
        params["message"]["extensions"] = json!([DOOR_EXTENSION_URI]);
        params["message"]["metadata"] = json!({ DOOR_EXTENSION_URI: { "forgetSession": forget } });
        params
    };

    for activated in [false, true] {
        let extra: &[(&str, &str)] = if activated {
            &[common::stream::extensions_header()]
        } else {
            &[]
        };
        let (response, events) = common::stream::stream_message_with(
            &addr,
            None,
            json!("forget"),
            with_forget("m-forget", json!(true)),
            extra,
        );
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.header("content-type"),
            Some("application/json"),
            "{response:?}"
        );
        assert!(events.is_empty(), "{response:?}");
        assert_eq!(response.header("a2a-extensions"), None, "{response:?}");
        let body = response.json();
        assert_eq!(body["id"], "forget");
        assert_eq!(body["error"]["code"], -32602, "{body}");
        assert_eq!(
            body["error"]["message"],
            "forgetSession asks this capsule to forget the harness session this context names, \
             and only a capsule on inference.transport: process has one"
        );
    }
    let sent = rpc(
        &addr,
        None,
        "SendMessage",
        with_forget("m-forget-send", json!(true)),
    );
    assert_eq!(sent.json()["error"]["code"], -32602, "{sent:?}");
    assert_eq!(sent.header("a2a-extensions"), None, "{sent:?}");
    // A malformed field is refused the same way, before anything else is looked at.
    let malformed = rpc(
        &addr,
        None,
        "SendMessage",
        with_forget("m-bad", json!("yes")),
    );
    assert_eq!(
        malformed.json()["error"]["message"],
        format!(
            "Invalid params: message.metadata[\"{DOOR_EXTENSION_URI}\"].forgetSession must be a \
             boolean"
        ),
        "{malformed:?}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        door.provider.requests().is_empty(),
        "a refused forget started a task: {:?}",
        door.provider.requests()
    );

    // `forgetSession: false` asks for nothing, so the message is served, and both extensions it
    // used are listed, the door's first.
    let (response, events) = common::stream::stream_message_with(
        &addr,
        None,
        json!("kept"),
        with_forget("m-kept", json!(false)),
        &[common::stream::extensions_header()],
    );
    assert_eq!(response.header("content-type"), Some("text/event-stream"));
    assert_eq!(
        response.header("a2a-extensions"),
        Some(format!("{DOOR_EXTENSION_URI}, {STREAM_EXTENSION_URI}").as_str()),
        "{response:?}"
    );
    assert!(
        events
            .last()
            .is_some_and(|event| event.is_terminal_status()),
        "{events:#?}"
    );
    let (response, _) = common::stream::stream_message_with(
        &addr,
        None,
        json!("plain"),
        with_forget("m-door-only", json!(false)),
        &[],
    );
    assert_eq!(
        response.header("a2a-extensions"),
        Some(DOOR_EXTENSION_URI),
        "{response:?}"
    );
}
