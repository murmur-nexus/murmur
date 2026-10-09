//! The A2A v1.0 wire census: one authenticated door driven through every method it serves, with
//! the set of failing checks compared against `tests/fixtures/a2a/wire-exceptions.txt` in both
//! directions.
//!
//! Every other test that goes through the common door helpers enforces the list one way only: a
//! failing check that is not listed fails it. Only here does a listed exception that no longer
//! occurs fail, because nextest runs each test in its own process and no other process sees every
//! method served.

#[path = "common/mod.rs"]
mod common;

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use common::{
    a2a_conformance::WirePart,
    door_capsule::{
        agent_project, driver_home, end_turn, launch_door, message, rpc, stage_door,
        wait_completed, AUTHENTICATION_YAML, QUEUE_SLEEP_YAML,
    },
    wire_recorder::{
        begin_census, exceptions, exceptions_file, observed, parse_exceptions, EXCEPTIONS_PATH,
        PART5_CARDS, VERSION_HEADER,
    },
};
use serde_json::json;

#[test]
fn wire_exceptions_list_is_well_formed() {
    let mut problems = Vec::new();
    let mut keys = BTreeSet::new();
    for (line_no, line) in parse_exceptions(exceptions_file()) {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                problems.push(format!("line {line_no}: {error}"));
                continue;
            }
        };
        let Some(part) = WirePart::parse(&line.part) else {
            problems.push(format!(
                "line {line_no}: part `{}` is not one of method, header, envelope, params, result, event",
                line.part
            ));
            continue;
        };
        let subject_fits = match part {
            WirePart::Method | WirePart::Envelope => line.subject == "-",
            WirePart::Header => line.subject == VERSION_HEADER,
            WirePart::Params | WirePart::Result | WirePart::Event => {
                line.subject.starts_with("lf.a2a.v1.") && line.subject.len() > "lf.a2a.v1.".len()
            }
        };
        if !subject_fits {
            problems.push(format!(
                "line {line_no}: subject `{}` does not fit part `{}`: `-` for method and envelope, \
                 the header name for header, an lf.a2a.v1 message for params, result and event",
                line.subject, line.part
            ));
        }
        if !PART5_CARDS.contains(&line.card.as_str()) {
            problems.push(format!(
                "line {line_no}: card `{}` is not one of {PART5_CARDS:?}",
                line.card
            ));
        }
        if line.why.is_empty() {
            problems.push(format!("line {line_no}: no reason given"));
        }
        if !keys.insert(line.key()) {
            problems.push(format!("line {line_no}: `{}` is listed twice", line.key()));
        }
    }
    assert!(
        problems.is_empty(),
        "{EXCEPTIONS_PATH} is malformed:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_door_fails_exactly_the_listed_exceptions() {
    begin_census();
    // Every provider answer takes a moment, so a task queued behind a running one is still
    // `submitted` when it is cancelled. Short enough that a stream never goes the second quiet
    // `request` reads it until.
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn(1, "sent"),
            end_turn(2, "running"),
            end_turn(3, "streamed"),
            end_turn(4, "spare"),
        ],
        Duration::from_millis(400),
    );
    let home = driver_home();
    let project = agent_project(
        &server.endpoint,
        "census-agent",
        "",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
    );
    let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
    let operator = staged
        .door_tokens()
        .into_iter()
        .find(|(name, _)| name == "operator")
        .map(|(_, token)| token.expose().to_string())
        .expect("an authenticated door mints an operator token");
    let addr = launch_door(staged);
    let token = Some(operator.as_str());
    let started = Instant::now();

    let sent = rpc(
        &addr,
        token,
        "SendMessage",
        message("m-census-send", "send"),
    );
    assert_eq!(sent.status, 200, "{sent:?}");
    let task_id = sent.json()["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {sent:?}"))
        .to_string();
    wait_completed(&addr, token, &task_id);

    // Without an `id`, the door answers its active task.
    let active = rpc(&addr, token, "GetTask", json!({}));
    assert_eq!(active.status, 200, "{active:?}");
    assert_eq!(
        active.json()["result"]["id"],
        task_id.as_str(),
        "{active:?}"
    );

    // A task that has ended is not cancelable, and is left as it ended.
    let ended = rpc(&addr, token, "CancelTask", json!({"id": task_id}));
    assert_eq!(ended.status, 200, "{ended:?}");
    assert_eq!(ended.json()["error"]["code"], -32002, "{ended:?}");
    assert_eq!(
        ended.json()["error"]["data"][0]["metadata"]["state"],
        "completed",
        "{ended:?}"
    );

    // A task queued behind a running one is live, and its cancel is accepted.
    let running = rpc(
        &addr,
        token,
        "SendMessage",
        message("m-census-running", "running"),
    );
    let running_id = running.json()["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {running:?}"))
        .to_string();
    let queued = rpc(
        &addr,
        token,
        "SendMessage",
        message("m-census-queued", "queued"),
    );
    let queued_id = queued.json()["result"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {queued:?}"))
        .to_string();
    let canceled = rpc(&addr, token, "CancelTask", json!({"id": queued_id}));
    assert_eq!(canceled.status, 200, "{canceled:?}");
    assert_eq!(
        canceled.json()["result"]["status"]["state"],
        "canceled",
        "{canceled:?}"
    );
    wait_completed(&addr, token, &running_id);

    let streamed = rpc(
        &addr,
        token,
        "SendStreamingMessage",
        message("m-census-stream", "stream"),
    );
    assert_eq!(streamed.status, 200, "{streamed:?}");
    assert!(
        streamed.body.contains("data:"),
        "SendStreamingMessage wrote no events: {streamed:?}"
    );

    let watched = rpc(&addr, token, "stream/watch", json!({}));
    assert_eq!(watched.status, 200, "{watched:?}");
    assert!(
        watched.body.contains("data:"),
        "stream/watch wrote no events: {watched:?}"
    );

    let card = rpc(&addr, token, "GetExtendedAgentCard", json!({}));
    assert_eq!(card.status, 200, "{card:?}");
    assert!(card.json().get("result").is_some(), "{card:?}");

    let unknown = rpc(&addr, token, "GetTask", json!({"id": "tsk_no_such_task"}));
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(unknown.json()["error"]["code"], -32001, "{unknown:?}");

    let stopped = rpc(&addr, token, "session/stop", json!({}));
    assert_eq!(stopped.status, 200, "{stopped:?}");
    assert!(stopped.json().get("result").is_some(), "{stopped:?}");
    eprintln!(
        "census: every served method driven in {:.1}s",
        started.elapsed().as_secs_f64()
    );

    let observed = observed();
    let listed: BTreeSet<String> = exceptions().iter().map(|line| line.key()).collect();
    let mut differences = Vec::new();
    for (key, violation) in &observed {
        if !listed.contains(key) {
            let errors: Vec<String> = violation
                .errors
                .iter()
                .map(|error| format!("    - {error}"))
                .collect();
            differences.push(format!(
                "observed but not listed: {key}\n{}",
                errors.join("\n")
            ));
        }
    }
    for line in exceptions() {
        if !observed.contains_key(&line.key()) {
            differences.push(format!(
                "listed but not observed: {} ({}): the door no longer fails it, so remove this line",
                line.key(),
                line.card
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "the door's A2A v1.0 violations differ from {EXCEPTIONS_PATH}:\n{}",
        differences.join("\n")
    );
    drop(server);
}
