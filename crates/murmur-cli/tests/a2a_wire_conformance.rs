//! A2A v1.0 conformance with nothing excused: the wire recorder's own behaviour, one
//! authenticated door driven through every method it serves, and the docs held to the v1.0 wire.
//!
//! Every test that goes through the common door helpers fails on any exchange the recorder finds
//! non-conformant. [`every_served_method_conforms`] makes sure each served method is judged at
//! least once, including methods no other test calls. The docs tests read every page under
//! `docs/content`: a retired 0.3 or murmur name must sit beside its replacement, and every
//! JSON-RPC request example must pass the same request check the recorder applies.

#[path = "common/mod.rs"]
mod common;

use std::{
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use common::{
    a2a_conformance::check_request,
    door_capsule::{
        agent_project, driver_home, end_turn, launch_door, message, push_config_requests, rpc,
        stage_door, wait_completed, AUTHENTICATION_YAML, QUEUE_SLEEP_YAML,
    },
    wire_recorder::{record_post, UNTIMED_STATUS, VERSION_HEADER},
};
use serde_json::{json, Value};

#[test]
fn every_served_method_conforms() {
    // Every provider answer takes a moment, so a task queued behind a running one is still
    // `submitted` when it is cancelled. Short enough that a stream never goes the second quiet
    // `request` reads it until.
    let server = common::ScriptedServer::start_with_delay(
        vec![
            end_turn(1, "sent"),
            end_turn(2, "running"),
            end_turn(3, "data"),
            end_turn(4, "streamed"),
            end_turn(5, "streamed with the extension"),
            end_turn(6, "ahead of a subscription"),
            end_turn(7, "subscribed"),
            end_turn(8, "ahead of an activated subscription"),
            end_turn(9, "subscribed with the extension"),
            end_turn(10, "spare"),
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
    let task_id = sent.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {sent:?}"))
        .to_string();
    wait_completed(&addr, token, &task_id);

    // `GetTask` names its task: without an `id` it is refused, and nothing is measured.
    let unnamed = rpc(&addr, token, "GetTask", json!({}));
    assert_eq!(unnamed.status, 200, "{unnamed:?}");
    assert_eq!(unnamed.json()["error"]["code"], -32602, "{unnamed:?}");

    // A task that has ended is not cancelable, and is left as it ended.
    let ended = rpc(&addr, token, "CancelTask", json!({"id": task_id}));
    assert_eq!(ended.status, 200, "{ended:?}");
    assert_eq!(ended.json()["error"]["code"], -32002, "{ended:?}");
    assert_eq!(
        ended.json()["error"]["data"][0]["metadata"]["state"],
        "TASK_STATE_COMPLETED",
        "{ended:?}"
    );

    // A task queued behind a running one is live, and its cancel is accepted.
    let running = rpc(
        &addr,
        token,
        "SendMessage",
        message("m-census-running", "running"),
    );
    let running_id = running.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {running:?}"))
        .to_string();
    let queued = rpc(
        &addr,
        token,
        "SendMessage",
        message("m-census-queued", "queued"),
    );
    let queued_id = queued.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {queued:?}"))
        .to_string();
    let canceled = rpc(&addr, token, "CancelTask", json!({"id": queued_id}));
    assert_eq!(canceled.status, 200, "{canceled:?}");
    assert_eq!(
        canceled.json()["result"]["status"]["state"],
        "TASK_STATE_CANCELED",
        "{canceled:?}"
    );
    wait_completed(&addr, token, &running_id);

    // A message with a text part and a data part is read in full and measured.
    let data = rpc(
        &addr,
        token,
        "SendMessage",
        json!({"message": {"messageId": "m-census-data", "role": "ROLE_USER", "parts": [
            {"text": "summarise", "mediaType": "text/plain"},
            {"data": {"rows": [1, 2]}, "mediaType": "application/json"},
        ]}}),
    );
    let data_id = data.json()["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("SendMessage answered no task: {data:?}"))
        .to_string();
    wait_completed(&addr, token, &data_id);

    // A file part is refused, and starts nothing.
    let file = rpc(
        &addr,
        token,
        "SendMessage",
        json!({"message": {"messageId": "m-census-file", "role": "ROLE_USER",
            "parts": [{"url": "https://example.com/x.pdf", "mediaType": "application/pdf"}]}}),
    );
    assert_eq!(file.status, 200, "{file:?}");
    assert_eq!(file.json()["error"]["code"], -32005, "{file:?}");

    // A stream is measured plain and with the stream extension activated, whose response alone
    // lists it in `A2A-Extensions`.
    for (index, activated) in [false, true].into_iter().enumerate() {
        let (streamed, events) = common::stream::stream_message(
            &addr,
            token,
            message(&format!("m-census-stream-{index}"), "stream"),
            activated,
        );
        assert_eq!(streamed.status, 200, "{streamed:?}");
        assert!(
            events.len() > 1,
            "SendStreamingMessage wrote no task events: {streamed:?}"
        );
        assert_eq!(
            streamed.header("a2a-extensions"),
            activated.then_some(common::stream::STREAM_EXTENSION_URI),
            "{streamed:?}"
        );
        assert_eq!(events.iter().any(|event| event.is_frame()), activated);
    }

    // `ListTasks` is measured on two pages.
    let first = rpc(&addr, token, "ListTasks", json!({"pageSize": 2}));
    assert_eq!(first.status, 200, "{first:?}");
    let page_token = first.json()["result"]["nextPageToken"]
        .as_str()
        .unwrap_or_else(|| panic!("ListTasks answered no page: {first:?}"))
        .to_string();
    assert!(!page_token.is_empty(), "{first:?}");
    let second = rpc(
        &addr,
        token,
        "ListTasks",
        json!({"pageSize": 2, "pageToken": page_token, "includeArtifacts": true}),
    );
    assert_eq!(second.status, 200, "{second:?}");
    assert!(second.json().get("result").is_some(), "{second:?}");
    // And filtered by status time, from the timestamp of the newest task the first page held.
    let newest = first.json()["result"]["tasks"][0]["status"]["timestamp"].clone();
    assert!(newest.is_string(), "{first:?}");
    let since = rpc(
        &addr,
        token,
        "ListTasks",
        json!({"statusTimestampAfter": newest}),
    );
    assert_eq!(since.status, 200, "{since:?}");
    assert!(
        since.json()["result"]["totalSize"].as_u64() >= Some(1),
        "{since:?}"
    );

    // A subscription is measured plain and activated, each on a task queued behind another, so it
    // is still live when subscribed to.
    for (index, activated) in [false, true].into_iter().enumerate() {
        rpc(
            &addr,
            token,
            "SendMessage",
            message(&format!("m-census-ahead-{index}"), "ahead"),
        );
        let subscribed = rpc(
            &addr,
            token,
            "SendMessage",
            message(&format!("m-census-subscribed-{index}"), "subscribed"),
        );
        let subscribed_id = subscribed.json()["result"]["task"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("SendMessage answered no task: {subscribed:?}"))
            .to_string();
        let (streamed, events) =
            common::stream::subscribe(&addr, token, &subscribed_id, activated, None);
        assert_eq!(streamed.status, 200, "{streamed:?}");
        assert!(
            events.len() > 1,
            "SubscribeToTask wrote no task events: {streamed:?}"
        );
        assert_eq!(
            streamed.header("a2a-extensions"),
            activated.then_some(common::stream::STREAM_EXTENSION_URI),
            "{streamed:?}"
        );
    }

    // Push is answered, never served: each config method, and a message asking for it.
    for (method, params) in push_config_requests(&task_id) {
        let refused = rpc(&addr, token, method, params);
        assert_eq!(refused.status, 200, "{refused:?}");
        assert_eq!(refused.json()["error"]["code"], -32003, "{refused:?}");
    }
    let mut asking = message("m-census-push", "notify me");
    asking["configuration"] =
        json!({"taskPushNotificationConfig": {"url": "https://example.com/hook"}});
    let pushed = rpc(&addr, token, "SendMessage", asking);
    assert_eq!(pushed.json()["error"]["code"], -32003, "{pushed:?}");

    // A request naming a tenant is refused.
    let tenant = rpc(
        &addr,
        token,
        "GetTask",
        json!({"id": task_id, "tenant": "acme"}),
    );
    assert_eq!(tenant.json()["error"]["code"], -32602, "{tenant:?}");

    let watched = rpc(&addr, token, "murmur.WatchSession", json!({}));
    assert_eq!(watched.status, 200, "{watched:?}");
    assert!(
        watched.body.contains("data:"),
        "murmur.WatchSession wrote no events: {watched:?}"
    );

    let card = rpc(&addr, token, "GetExtendedAgentCard", json!({}));
    assert_eq!(card.status, 200, "{card:?}");
    assert!(card.json().get("result").is_some(), "{card:?}");

    let unknown = rpc(&addr, token, "GetTask", json!({"id": "tsk_no_such_task"}));
    assert_eq!(unknown.status, 200, "{unknown:?}");
    assert_eq!(unknown.json()["error"]["code"], -32001, "{unknown:?}");

    let stopped = rpc(&addr, token, "murmur.StopSession", json!({}));
    assert_eq!(stopped.status, 200, "{stopped:?}");
    assert!(stopped.json().get("result").is_some(), "{stopped:?}");
    eprintln!(
        "census: every served method driven in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    drop(server);
}

// ── The recorder fails what is not v1.0 ───────────────────────────────────────

const GET_TASK: &str = r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"tsk_1"}}"#;

const STREAM_MESSAGE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"SendStreamingMessage","params":{"message":{"messageId":"m-1","role":"ROLE_USER","parts":[{"text":"hello"}]}}}"#;

const V1_TASK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"id":"tsk_1","contextId":"ctx_1","status":{"state":"TASK_STATE_WORKING","timestamp":"2026-10-13T09:00:00.123Z"}}}"#;

const V1_STATUS_EVENT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"statusUpdate":{"taskId":"tsk_1","contextId":"ctx_1","status":{"state":"TASK_STATE_WORKING","timestamp":"2026-10-13T09:00:00.123Z"}}}}"#;

/// [`V1_TASK`] and [`V1_STATUS_EVENT`] with no `timestamp`: v1.0, but not what murmur writes.
const UNTIMED_TASK: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"id":"tsk_1","contextId":"ctx_1","status":{"state":"TASK_STATE_WORKING"}}}"#;

const UNTIMED_STATUS_EVENT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"statusUpdate":{"taskId":"tsk_1","contextId":"ctx_1","status":{"state":"TASK_STATE_WORKING"}}}}"#;

const VERSIONED: [(&str, &str); 2] = [
    ("Content-Type", "application/json"),
    (VERSION_HEADER, "1.0"),
];

fn json_headers() -> Vec<(String, String)> {
    vec![("content-type".to_string(), "application/json".to_string())]
}

fn event_stream_headers() -> Vec<(String, String)> {
    vec![("content-type".to_string(), "text/event-stream".to_string())]
}

/// The message [`record_post`] panics with for this 200 exchange, or `None` when it passes.
fn recorder_failure(
    request_headers: &[(&str, &str)],
    request_body: &str,
    response_headers: &[(String, String)],
    response_body: &str,
) -> Option<String> {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        record_post(
            request_headers,
            request_body,
            200,
            response_headers,
            response_body,
        )
    }));
    outcome.err().map(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
            .unwrap_or_else(|| "a panic with no message".to_string())
    })
}

#[test]
fn recorder_fails_a_nonconformant_result() {
    let failure = recorder_failure(
        &VERSIONED,
        GET_TASK,
        &json_headers(),
        r#"{"jsonrpc":"2.0","id":1,"result":{"id":"tsk_1","contextId":"ctx_1","status":{"state":"working"}}}"#,
    )
    .expect("a 0.3 task state in a GetTask result fails the recorder");
    assert!(
        failure.starts_with("A2A v1.0 wire check failed: GetTask result lf.a2a.v1.Task"),
        "{failure}"
    );
    assert!(failure.contains("result.status.state"), "{failure}");
}

#[test]
fn recorder_fails_a_nonconformant_stream_event() {
    let body = concat!(
        "id: 1\n",
        r#"data: {"jsonrpc":"2.0","id":1,"result":{"kind":"status-update","taskId":"tsk_1","contextId":"ctx_1","status":{"state":"working"},"final":false}}"#,
        "\n\n",
    );
    let failure = recorder_failure(&VERSIONED, STREAM_MESSAGE, &event_stream_headers(), body)
        .expect("a 0.3 status-update event fails the recorder");
    assert!(
        failure.starts_with("A2A v1.0 wire check failed: SendStreamingMessage event"),
        "{failure}"
    );
}

/// The proto leaves `TaskStatus.timestamp` optional, so these pass the proto checks; the recorder
/// still fails them, since every status murmur writes carries one.
#[test]
fn recorder_fails_a_status_without_a_timestamp() {
    let failure = recorder_failure(&VERSIONED, GET_TASK, &json_headers(), UNTIMED_TASK)
        .expect("a GetTask status without a timestamp fails the recorder");
    assert!(
        failure.starts_with(&format!(
            "A2A v1.0 wire check failed: GetTask result {UNTIMED_STATUS}"
        )),
        "{failure}"
    );
    assert!(failure.contains("result.status:"), "{failure}");

    let body = format!("id: 1\ndata: {UNTIMED_STATUS_EVENT}\n\n");
    let failure = recorder_failure(&VERSIONED, STREAM_MESSAGE, &event_stream_headers(), &body)
        .expect("a statusUpdate without a timestamp fails the recorder");
    assert!(
        failure.starts_with(&format!(
            "A2A v1.0 wire check failed: SendStreamingMessage event {UNTIMED_STATUS}"
        )),
        "{failure}"
    );
    assert!(failure.contains("result.statusUpdate.status:"), "{failure}");
}

#[test]
fn recorder_fails_an_unversioned_request() {
    let failure = recorder_failure(
        &[("Content-Type", "application/json")],
        GET_TASK,
        &json_headers(),
        V1_TASK,
    )
    .expect("an accepted request without A2A-Version fails the recorder");
    assert!(
        failure.starts_with("A2A v1.0 wire check failed: GetTask header A2A-Version"),
        "{failure}"
    );
}

#[test]
fn recorder_passes_a_conformant_exchange() {
    let stream_body = format!("id: 1\ndata: {V1_STATUS_EVENT}\n\n");
    let exchanges = [
        ("result", GET_TASK, json_headers(), V1_TASK.to_string()),
        (
            "stream event",
            STREAM_MESSAGE,
            event_stream_headers(),
            stream_body,
        ),
    ];
    for (what, request, response_headers, response) in exchanges {
        assert_eq!(
            recorder_failure(&VERSIONED, request, &response_headers, &response),
            None,
            "a v1.0 {what} fails the recorder"
        );
    }
}

// ── The docs describe the v1.0 wire ───────────────────────────────────────────

/// Each retired wire name the docs may mention only beside its replacement.
const RETIRED_NAMES: [(&str, &str); 9] = [
    ("message/send", "SendMessage"),
    ("message/stream", "SendStreamingMessage"),
    ("tasks/get", "GetTask"),
    ("tasks/cancel", "CancelTask"),
    ("getAuthenticatedExtendedCard", "GetExtendedAgentCard"),
    ("input-required", "TASK_STATE_INPUT_REQUIRED"),
    ("stream/watch", "murmur.WatchSession"),
    ("session/stop", "murmur.StopSession"),
    ("x-murmur-forget-session", "forgetSession"),
];

/// The fewest JSON-RPC request examples the docs hold; fewer means the extractor lost them.
const MIN_REQUEST_EXAMPLES: usize = 15;

fn docs_content() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/content")
}

/// Every `.md` file under `docs/content`, as (path relative to it, text), sorted by path.
fn docs_pages() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, pages: &mut Vec<(String, String)>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            if path.is_dir() {
                walk(&path, root, pages);
            } else if path.extension().is_some_and(|ext| ext == "md") {
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                let relative = path.strip_prefix(root).expect("under docs/content");
                pages.push((relative.to_string_lossy().replace('\\', "/"), text));
            }
        }
    }
    let root = docs_content();
    let mut pages = Vec::new();
    walk(&root, &root, &mut pages);
    pages.sort();
    assert!(!pages.is_empty(), "no pages under {}", root.display());
    pages
}

/// One fenced code block: the 0-based lines of its opening and closing fences (the closing one
/// is the page's end for an unclosed block), and its info string's first word.
struct Fence {
    open: usize,
    close: usize,
    info: String,
}

/// The fenced blocks of a page. A fence is a run of three or more backticks or tildes, indented
/// or not, so blocks inside admonitions count.
fn fences(lines: &[&str]) -> Vec<Fence> {
    let mut fences = Vec::new();
    let mut open: Option<(usize, char, usize, String)> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~');
        let Some(marker) = marker else { continue };
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if run < 3 {
            continue;
        }
        match &open {
            None => {
                let info = trimmed[run..]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
                open = Some((index, marker, run, info));
            }
            Some((start, open_marker, open_run, info)) => {
                if marker == *open_marker && run >= *open_run && trimmed[run..].trim().is_empty() {
                    fences.push(Fence {
                        open: *start,
                        close: index,
                        info: info.clone(),
                    });
                    open = None;
                }
            }
        }
    }
    if let Some((start, _, _, info)) = open {
        fences.push(Fence {
            open: start,
            close: lines.len(),
            info,
        });
    }
    fences
}

fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

/// The blank-line-delimited paragraph around `line`, stopping at a fence line, as a line range.
fn paragraph(lines: &[&str], line: usize) -> (usize, usize) {
    let stops = |index: usize| lines[index].trim().is_empty() || is_fence_line(lines[index]);
    let mut start = line;
    while start > 0 && !stops(start - 1) {
        start -= 1;
    }
    let mut end = line + 1;
    while end < lines.len() && !stops(end) {
        end += 1;
    }
    (start, end)
}

/// The text a mention on `line` is judged with:
/// - a table row is its own unit;
/// - a line inside a fenced block takes the whole block, plus the nearest non-blank paragraph
///   before the opening fence;
/// - any other line takes its blank-line-delimited paragraph.
fn unit(lines: &[&str], fences: &[Fence], line: usize) -> String {
    if lines[line].trim_start().starts_with('|') {
        return lines[line].to_string();
    }
    if let Some(fence) = fences
        .iter()
        .find(|fence| fence.open <= line && line <= fence.close)
    {
        let mut before = fence.open;
        while before > 0 && lines[before - 1].trim().is_empty() {
            before -= 1;
        }
        let start = if before == 0 {
            fence.open
        } else {
            paragraph(lines, before - 1).0
        };
        let end = (fence.close + 1).min(lines.len());
        return lines[start..end].join("\n");
    }
    let (start, end) = paragraph(lines, line);
    lines[start..end].join("\n")
}

/// Where `term` occurs in `line`, as byte offsets. `message/send` right after `:` or `/` is the
/// WIT interface `murmur:message/send` or its file path, not the 0.3 method.
fn mentions(line: &str, term: &str) -> Vec<usize> {
    line.match_indices(term)
        .map(|(at, _)| at)
        .filter(|&at| {
            term != "message/send" || !matches!(line[..at].chars().last(), Some(':' | '/'))
        })
        .collect()
}

#[test]
fn the_docs_describe_no_a2a_0_3_wire() {
    let mut misses = Vec::new();
    for (path, text) in docs_pages() {
        let lines: Vec<&str> = text.lines().collect();
        let fences = fences(&lines);
        for (index, line) in lines.iter().enumerate() {
            for (term, replacement) in RETIRED_NAMES {
                if mentions(line, term).is_empty() {
                    continue;
                }
                if !unit(&lines, &fences, index).contains(replacement) {
                    misses.push(format!(
                        "{path}:{}: {term} without {replacement}",
                        index + 1
                    ));
                }
            }
        }
    }
    assert!(
        misses.is_empty(),
        "these docs lines name a retired wire name with no replacement in the same table row, \
         paragraph or code block (with the paragraph before it):\n{}",
        misses.join("\n")
    );
}

/// A JSON-RPC request example in the docs: the page and 1-based line it starts on, its text, and
/// the fenced block it sits in.
struct RequestExample {
    path: String,
    line: usize,
    text: String,
    fence: usize,
}

/// Every candidate request in a page's fenced blocks:
/// - the text between `-d '` (or `--data '`) and the next `'`;
/// - the whole of a block whose info string is `json`, when it is a JSON-RPC request.
fn request_candidates(path: &str, lines: &[&str], fences: &[Fence]) -> Vec<RequestExample> {
    let mut candidates = Vec::new();
    for (index, fence) in fences.iter().enumerate() {
        let body_start = fence.open + 1;
        let body_end = fence.close.min(lines.len()).max(body_start);
        let body = lines[body_start..body_end].join("\n");
        if fence.info == "json" && is_json_rpc_request(&body) {
            candidates.push(RequestExample {
                path: path.to_string(),
                line: body_start + 1,
                text: body.clone(),
                fence: index,
            });
        }
        for flag in ["-d '", "--data '"] {
            for (at, _) in body.match_indices(flag) {
                let start = at + flag.len();
                let Some(length) = body[start..].find('\'') else {
                    continue;
                };
                candidates.push(RequestExample {
                    path: path.to_string(),
                    line: body_start + 1 + body[..at].matches('\n').count(),
                    text: body[start..start + length].to_string(),
                    fence: index,
                });
            }
        }
    }
    candidates
}

/// `text` is a JSON object carrying `jsonrpc` and `method`.
fn is_json_rpc_request(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .is_ok_and(|value| value.get("jsonrpc").is_some() && value.get("method").is_some())
}

/// The unit shows a refusal: a JSON-RPC error code `-32xxx`, or an HTTP `401` or `403`, each as a
/// word of its own. A bare substring would skip any example whose ids or ports hold `401`.
fn shows_refusal(unit: &str) -> bool {
    unit.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|word| {
            let is_error_code = word.strip_prefix("-32").is_some_and(|digits| {
                digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit())
            });
            is_error_code || word == "401" || word == "403"
        })
}

#[test]
fn the_docs_request_examples_are_v1_requests() {
    let mut checked = 0;
    let mut failures = Vec::new();
    for (path, text) in docs_pages() {
        let lines: Vec<&str> = text.lines().collect();
        let fences = fences(&lines);
        let mut unversioned_curls = Vec::new();
        for example in request_candidates(&path, &lines, &fences) {
            let at = format!("{}:{}", example.path, example.line);
            let request = match serde_json::from_str::<Value>(&example.text) {
                Ok(request) => request,
                Err(error) => {
                    eprintln!("skipped {at}: not JSON ({error})");
                    continue;
                }
            };
            if request.get("jsonrpc").is_none() || request.get("method").is_none() {
                eprintln!("skipped {at}: not a JSON-RPC request (no jsonrpc and method)");
                continue;
            }
            let fence = &fences[example.fence];
            let block = lines[fence.open..fence.close.min(lines.len())].join("\n");
            if block.contains("curl")
                && !block.contains(&format!("{VERSION_HEADER}: 1.0"))
                && !unversioned_curls.contains(&example.fence)
            {
                unversioned_curls.push(example.fence);
                failures.push(format!(
                    "{at}: a curl request without `-H \"{VERSION_HEADER}: 1.0\"`"
                ));
            }
            if shows_refusal(&unit(&lines, &fences, example.line - 1)) {
                eprintln!("skipped {at}: shown being refused");
                continue;
            }
            checked += 1;
            for violation in check_request(&request) {
                failures.push(format!(
                    "{at}: {}\n{}",
                    violation.key(),
                    violation
                        .errors
                        .iter()
                        .map(|error| format!("  - {error}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
    }
    eprintln!("checked {checked} JSON-RPC request examples");
    assert!(
        failures.is_empty(),
        "these docs request examples are not A2A v1.0 requests:\n{}",
        failures.join("\n")
    );
    assert!(
        checked >= MIN_REQUEST_EXAMPLES,
        "checked only {checked} request examples, fewer than {MIN_REQUEST_EXAMPLES}"
    );
}
