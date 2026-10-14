//! The door's whole A2A v1.0 method table, end to end on in-process doors: `ListTasks` pages the
//! tasks a caller may see, `SubscribeToTask` streams the rest of one of them, every method that
//! names a task reads the same ownership rule, the four push config methods and a message asking
//! for push are answered `PushNotificationNotSupportedError`, and a request naming a tenant is
//! refused.
//!
//! Every exchange goes through the common door helpers, so each is also checked against the A2A
//! v1.0 proto by the wire recorder.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use common::door_capsule::{
    agent_project, driver_home, end_turn, launch_door, message, push_config_requests, request,
    returning_immediately, rpc, stage_door, stage_door_with_tools, Response, AUTHENTICATION_YAML,
    CREDENTIALS_YAML, DRIVER_NAME, DRIVER_VERSION, QUEUE_SLEEP_YAML,
};
use common::stream::{stream_method, subscribe, Event, STREAM_EXTENSION_URI};
use serde_json::{json, Value};
use tempfile::TempDir;

/// An in-process door, the tokens it minted and what must outlive it.
struct Door {
    addr: String,
    tokens: Vec<(String, String)>,
    provider: common::ScriptedServer,
    _home: TempDir,
    _project: TempDir,
}

impl Door {
    /// A door named `name` with `extra` appended to its manifest, against `provider`.
    fn launch(name: &str, extra: &str, provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let project = agent_project(&provider.endpoint, name, "", extra);
        let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
        Self::launched(staged, provider, home, project)
    }

    /// A door whose model may call the `request-input-tool` fixture, with `extra` appended to its
    /// manifest, against `provider`.
    fn request_input(name: &str, extra: &str, provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let artifacts = tempfile::tempdir().unwrap();
        common::publish_local(&home, &request_input_tool(artifacts.path())).success();
        let endpoint = &provider.endpoint;
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join("murmur.yaml"),
            format!(
                "name: {name}\nversion: 0.1.0\n\
                 artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    \
                 runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n  - name: {REQUEST_INPUT_TOOL}\n    version: 0.1.0\n    \
                 runtime: tool\n\
                 capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    \
                 artifact: {DRIVER_NAME}\n{extra}"
            ),
        )
        .unwrap();
        let staged = stage_door_with_tools(&home, &project.path().join("murmur.yaml"));
        Self::launched(staged, provider, home, project)
    }

    fn launched(
        staged: capsule_runtime::StagedSession,
        provider: common::ScriptedServer,
        home: TempDir,
        project: TempDir,
    ) -> Self {
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

    /// One JSON-RPC call as credential `who`, or with no token for `None`.
    fn call(&self, who: Option<&str>, method: &str, params: Value) -> Response {
        let token = who.map(|who| self.token(who));
        rpc(&self.addr, token.as_deref(), method, params)
    }

    /// The JSON-RPC answer to one call as `who`, which the door answers `200`.
    fn answer(&self, who: Option<&str>, method: &str, params: Value) -> Value {
        let response = self.call(who, method, params);
        assert_eq!(response.status, 200, "{method}: {response:?}");
        response.json()
    }

    /// Starts a task as `who` with `params`, and returns its id.
    fn start(&self, who: Option<&str>, params: Value) -> String {
        let answer = self.answer(who, "SendMessage", returning_immediately(params));
        answer["result"]["task"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("SendMessage answered no task: {answer}"))
            .to_string()
    }

    /// `GetTask` as `who`, polled until the task is in `state`.
    fn wait_state(&self, who: Option<&str>, task_id: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let task = self.answer(who, "GetTask", json!({"id": task_id}));
            if task["result"]["status"]["state"] == state {
                return task["result"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "the task never reached {state}: {task}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// `SubscribeToTask` on `task_id` as `who`, read to its end.
    fn subscribe(
        &self,
        who: Option<&str>,
        task_id: &str,
        extensions: bool,
        last_event_id: Option<u64>,
    ) -> (Response, Vec<Event>) {
        let token = who.map(|who| self.token(who));
        subscribe(
            &self.addr,
            token.as_deref(),
            task_id,
            extensions,
            last_event_id,
        )
    }
}

const REQUEST_INPUT_TOOL: &str = "request-input-tool";

/// The `request-input-tool` fixture packed as an artifact in `dir`.
fn request_input_tool(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{REQUEST_INPUT_TOOL}-0.1.0.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(
        zip,
        "name: {REQUEST_INPUT_TOOL}\nversion: 0.1.0\nruntime: wasm"
    )
    .unwrap();
    zip.start_file("tool.wasm", options).unwrap();
    zip.write_all(
        &fs::read(common::fixture_path(
            "input-required/tool/request-input-tool.wasm",
        ))
        .unwrap(),
    )
    .unwrap();
    zip.finish().unwrap();
    path
}

fn tool_call_response(n: usize, name: &str, input: Value) -> String {
    json!({
        "id": format!("msg_{n}"),
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": format!("call_{n}"), "name": name, "input": input}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// The provider's answer asking the `request-input-tool` for input with `prompt`.
fn ask_for_input(n: usize, prompt: &str) -> String {
    tool_call_response(n, REQUEST_INPUT_TOOL, json!({"data": prompt}))
}

/// A message naming the task `task_id`, replying `text`.
fn reply(task_id: &str, text: &str) -> Value {
    json!({"message": {"messageId": format!("reply-{text}"), "role": "ROLE_USER",
        "taskId": task_id, "parts": [{"text": text, "mediaType": "text/plain"}]}})
}

/// The ids of a `ListTasks` answer's tasks, in order.
fn listed_ids(answer: &Value) -> Vec<String> {
    answer["result"]["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("ListTasks answered no tasks: {answer}"))
        .iter()
        .map(|task| task["id"].as_str().unwrap().to_string())
        .collect()
}

/// The `ListTasks` pages `who` walks with `params` and `pageSize: page_size`, in order.
fn walk(door: &Door, who: Option<&str>, params: Value, page_size: u64) -> Vec<Value> {
    let mut pages = Vec::new();
    let mut token = String::new();
    loop {
        let mut params = params.clone();
        params["pageSize"] = json!(page_size);
        params["pageToken"] = json!(token);
        let page = door.answer(who, "ListTasks", params);
        assert!(page.get("result").is_some(), "{page}");
        token = page["result"]["nextPageToken"]
            .as_str()
            .unwrap()
            .to_string();
        pages.push(page);
        if token.is_empty() {
            return pages;
        }
        assert!(pages.len() < 100, "the walk never ended");
    }
}

/// The JSON-RPC error a refused call answered with, framed as one JSON body.
fn error_of(response: &Response) -> Value {
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.header("content-type"),
        Some("application/json"),
        "{response:?}"
    );
    let error = response.json()["error"].clone();
    assert!(error.is_object(), "not refused: {response:?}");
    error
}

/// The `TaskNotFoundError` body `response` carries, with `task_id` written back as the id the
/// session never held, so two bodies compare byte for byte apart from the id.
fn as_never_held(response: &Response, task_id: &str) -> String {
    assert_eq!(error_of(response)["code"], -32001, "{response:?}");
    response.body.replace(task_id, "tsk_never_held")
}

/// Two credentials each list only the tasks they started, a page at a time, newest first; the
/// operator lists every task. A walk returns each task once.
#[test]
fn list_owned_tasks_are_paged_per_credential() {
    let door = Door::launch(
        "listing-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start((1..=9).map(|n| end_turn(n, "done")).collect()),
    );
    // Interleaved, each run to completion before the next is sent, so completion order is send
    // order.
    let senders = [
        "alice", "bob", "alice", "operator", "alice", "bob", "alice", "bob", "alice",
    ];
    let mut completed: Vec<(&str, String)> = Vec::new();
    for (index, who) in senders.into_iter().enumerate() {
        let task_id = door.start(Some(who), message(&format!("m-{index}"), "list me"));
        door.wait_state(Some(who), &task_id, "TASK_STATE_COMPLETED");
        completed.push((who, task_id));
    }
    let newest_first = |who: Option<&str>| -> Vec<String> {
        completed
            .iter()
            .rev()
            .filter(|(sender, _)| who.is_none_or(|who| *sender == who))
            .map(|(_, task_id)| task_id.clone())
            .collect()
    };

    let pages = walk(&door, Some("alice"), json!({}), 2);
    let sizes: Vec<usize> = pages.iter().map(|page| listed_ids(page).len()).collect();
    assert_eq!(sizes, [2, 2, 1], "{pages:?}");
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(page["result"]["totalSize"], 5, "{page}");
        assert_eq!(page["result"]["pageSize"], 2, "{page}");
        assert_eq!(
            page["result"]["nextPageToken"] == "",
            index == pages.len() - 1,
            "{page}"
        );
        for task in page["result"]["tasks"].as_array().unwrap() {
            assert!(task.get("artifacts").is_none(), "{task}");
        }
    }
    let walked: Vec<String> = pages.iter().flat_map(listed_ids).collect();
    assert_eq!(walked, newest_first(Some("alice")));

    let bob = door.answer(Some("bob"), "ListTasks", json!({}));
    assert_eq!(listed_ids(&bob), newest_first(Some("bob")), "{bob}");
    assert_eq!(bob["result"]["pageSize"], 50, "{bob}");
    assert_eq!(bob["result"]["nextPageToken"], "", "{bob}");
    assert_eq!(bob["result"]["totalSize"], 3, "{bob}");

    let all = door.answer(Some("operator"), "ListTasks", json!({}));
    assert_eq!(listed_ids(&all), newest_first(None), "{all}");
    assert_eq!(all["result"]["totalSize"], 9, "{all}");
    for task in all["result"]["tasks"].as_array().unwrap() {
        let mut read =
            door.answer(Some("operator"), "GetTask", json!({"id": task["id"]}))["result"].clone();
        read.as_object_mut().unwrap().remove("artifacts");
        assert_eq!(*task, read);
    }
}

/// The instant a status timestamp names, which must be RFC 3339 in UTC with a `Z`.
fn status_instant(timestamp: &Value) -> chrono::DateTime<chrono::Utc> {
    let text = timestamp
        .as_str()
        .unwrap_or_else(|| panic!("no status timestamp: {timestamp}"));
    assert!(text.ends_with('Z'), "{text}");
    chrono::DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|error| panic!("{text} is not RFC 3339: {error}"))
        .with_timezone(&chrono::Utc)
}

/// `statusTimestampAfter` keeps the tasks whose status timestamp is at or after it, alone or with
/// the other filters, page by page; the cutoff GetTask's own timestamp gives includes that task.
#[test]
fn status_timestamp_after_lists_only_tasks_changed_at_or_after_it() {
    let door = Door::launch(
        "timestamp-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start((1..=3).map(|n| end_turn(n, "done")).collect()),
    );
    let mut ids = Vec::new();
    for name in ["a", "b", "c"] {
        let task_id = door.start(None, message(&format!("m-{name}"), name));
        door.wait_state(None, &task_id, "TASK_STATE_COMPLETED");
        ids.push(task_id);
        thread::sleep(Duration::from_millis(5));
    }
    let timestamp_of = |task_id: &str| {
        let task = door.answer(None, "GetTask", json!({"id": task_id}));
        task["result"]["status"]["timestamp"].clone()
    };
    let [a, b, c] = [&ids[0], &ids[1], &ids[2]].map(|id| timestamp_of(id));
    let [at_a, at_b, at_c] = [&a, &b, &c].map(status_instant);
    assert!(at_a <= at_b && at_b <= at_c, "{a} {b} {c}");
    let list = |params: Value| door.answer(None, "ListTasks", params);
    let rfc3339 =
        |at: chrono::DateTime<chrono::Utc>| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

    let since_b = list(json!({"statusTimestampAfter": b}));
    assert_eq!(
        listed_ids(&since_b),
        [ids[2].clone(), ids[1].clone()],
        "{since_b}"
    );
    assert_eq!(since_b["result"]["totalSize"], 2, "{since_b}");

    let after_c = rfc3339(at_c + chrono::Duration::milliseconds(1));
    let none = list(json!({"statusTimestampAfter": after_c}));
    assert_eq!(none["result"]["tasks"], json!([]), "{none}");
    assert_eq!(none["result"]["totalSize"], 0, "{none}");

    let before_a = rfc3339(at_a - chrono::Duration::seconds(1));
    let newest_first: Vec<String> = ids.iter().rev().cloned().collect();
    for cutoff in [json!(before_a), Value::Null] {
        let all = list(json!({"statusTimestampAfter": cutoff}));
        assert_eq!(listed_ids(&all), newest_first, "{cutoff}: {all}");
        assert_eq!(all["result"]["totalSize"], 3, "{all}");
    }

    let first = list(json!({"statusTimestampAfter": b, "pageSize": 1}));
    assert_eq!(listed_ids(&first), std::slice::from_ref(&ids[2]), "{first}");
    assert_eq!(first["result"]["totalSize"], 2, "{first}");
    let token = first["result"]["nextPageToken"].clone();
    assert_ne!(token, "", "{first}");
    let second = list(json!({"statusTimestampAfter": b, "pageSize": 1, "pageToken": token}));
    assert_eq!(
        listed_ids(&second),
        std::slice::from_ref(&ids[1]),
        "{second}"
    );
    assert_eq!(second["result"]["nextPageToken"], "", "{second}");

    let completed = list(json!({"statusTimestampAfter": b, "status": "TASK_STATE_COMPLETED"}));
    assert_eq!(
        listed_ids(&completed),
        [ids[2].clone(), ids[1].clone()],
        "{completed}"
    );
    let working = list(json!({"statusTimestampAfter": b, "status": "TASK_STATE_WORKING"}));
    assert_eq!(working["result"]["totalSize"], 0, "{working}");
}

/// `ListTasks` filters by context and state, pages by a size it may lower, and refuses each
/// parameter it cannot read by name. A public door lists every task to every caller.
#[test]
fn list_params_filter_and_refuse_by_name() {
    let door = Door::launch(
        "filtering-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start_with_delay(
            (1..=4).map(|n| end_turn(n, "done")).collect(),
            Duration::from_millis(1000),
        ),
    );
    let alice = Some("alice");
    let first = door.start(alice, message("m-first", "first"));
    door.wait_state(alice, &first, "TASK_STATE_COMPLETED");
    let running = door.start(alice, message("m-running", "running"));
    let queued = door.start(alice, message("m-queued", "queued"));
    let canceled = door.answer(alice, "CancelTask", json!({"id": queued}));
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );
    door.wait_state(alice, &running, "TASK_STATE_COMPLETED");
    let mut elsewhere = message("m-elsewhere", "elsewhere");
    elsewhere["message"]["contextId"] = json!("ctx-second");
    let second = door.start(alice, elsewhere);
    door.wait_state(alice, &second, "TASK_STATE_COMPLETED");
    let list = |params: Value| door.answer(alice, "ListTasks", params);

    assert_eq!(
        listed_ids(&list(json!({"contextId": "ctx-second"}))),
        std::slice::from_ref(&second)
    );
    assert_eq!(
        listed_ids(&list(json!({"status": "TASK_STATE_CANCELED"}))),
        std::slice::from_ref(&queued)
    );
    let unfiltered = list(json!({"status": "TASK_STATE_UNSPECIFIED"}));
    assert_eq!(
        listed_ids(&unfiltered),
        [&second, &queued, &running, &first].map(String::clone)
    );
    let with = list(json!({"includeArtifacts": true}));
    for task in with["result"]["tasks"].as_array().unwrap() {
        if task["id"] == queued.as_str() {
            assert_eq!(task["artifacts"], json!([]), "{task}");
        } else {
            assert_eq!(task["artifacts"][0]["artifactId"], "response", "{task}");
            assert_eq!(task["artifacts"][0]["parts"][0]["text"], "done", "{task}");
        }
    }
    let lowered = list(json!({"pageSize": 500}));
    assert_eq!(lowered["result"]["pageSize"], 100, "{lowered}");
    assert_eq!(listed_ids(&lowered).len(), 4, "{lowered}");
    let accepted = list(json!({"historyLength": 3}));
    assert_eq!(accepted["result"]["totalSize"], 4, "{accepted}");

    for (params, code, named) in [
        (json!({"status": "done"}), -32602, "status"),
        (json!({"pageSize": 0}), -32602, "pageSize"),
        (json!({"pageToken": "not-a-token"}), -32602, "pageToken"),
        (
            json!({"statusTimestampAfter": "yesterday"}),
            -32602,
            "statusTimestampAfter",
        ),
        (
            json!({"statusTimestampAfter": 42}),
            -32602,
            "statusTimestampAfter",
        ),
        (
            json!({"statusTimestampAfter": true}),
            -32602,
            "statusTimestampAfter",
        ),
        (
            json!({"statusTimestampAfter": "2026-13-01T00:00:00Z"}),
            -32602,
            "statusTimestampAfter",
        ),
    ] {
        let error = error_of(&door.call(alice, "ListTasks", params.clone()));
        assert_eq!(error["code"], code, "{params}: {error}");
        assert!(
            error["message"].as_str().unwrap().contains(named),
            "{params}: {error}"
        );
        if named == "statusTimestampAfter" {
            assert_eq!(
                error["message"],
                "Invalid params: statusTimestampAfter must be an RFC 3339 timestamp, such as \
                 2026-10-13T09:00:00Z",
                "{params}"
            );
        }
    }
    let no_cutoff = list(json!({"statusTimestampAfter": null}));
    assert_eq!(no_cutoff["result"]["totalSize"], 4, "{no_cutoff}");

    let public = Door::launch(
        "public-listing-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start((1..=2).map(|n| end_turn(n, "done")).collect()),
    );
    let mut sent = Vec::new();
    for index in 0..2 {
        let task_id = public.start(None, message(&format!("m-public-{index}"), "hi"));
        public.wait_state(None, &task_id, "TASK_STATE_COMPLETED");
        sent.insert(0, task_id);
    }
    let listed = public.answer(None, "ListTasks", json!({}));
    assert_eq!(listed_ids(&listed), sent, "{listed}");
    assert_eq!(listed["result"]["totalSize"], 2, "{listed}");
}

/// `alice` reaches none of `bob`'s tasks by any method that names a task, and is answered for each
/// exactly as for an id the session never held. The operator reaches every one.
#[test]
fn owner_rule_hides_another_credentials_tasks_on_every_method() {
    let door = Door::request_input(
        "owning-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start(vec![
            end_turn(1, "bob is done"),
            ask_for_input(2, "which branch?"),
            end_turn(3, "resumed"),
            end_turn(4, "alice is done"),
        ]),
    );
    let (alice, bob) = (Some("alice"), Some("bob"));
    let done = door.start(bob, message("m-bob-done", "finish"));
    door.wait_state(bob, &done, "TASK_STATE_COMPLETED");
    let waiting = door.start(bob, message("m-bob-wait", "ask me"));
    door.wait_state(bob, &waiting, "TASK_STATE_INPUT_REQUIRED");

    let never = |method: &str, params: Value| door.call(alice, method, params).body;
    let never_get = never("GetTask", json!({"id": "tsk_never_held"}));
    let never_cancel = never("CancelTask", json!({"id": "tsk_never_held"}));
    let never_send = never("SendMessage", reply("tsk_never_held", "hijack"));
    let (never_subscribed, _) = door.subscribe(alice, "tsk_never_held", false, None);
    assert_eq!(error_of(&never_subscribed)["code"], -32001);
    for bobs in [&done, &waiting] {
        let got = door.call(alice, "GetTask", json!({"id": bobs}));
        assert_eq!(as_never_held(&got, bobs), never_get, "GetTask {bobs}");
        let cancel = door.call(alice, "CancelTask", json!({"id": bobs}));
        assert_eq!(
            as_never_held(&cancel, bobs),
            never_cancel,
            "CancelTask {bobs}"
        );
        let (subscribed, events) = door.subscribe(alice, bobs, false, None);
        assert!(events.is_empty(), "{subscribed:?}");
        assert_eq!(
            as_never_held(&subscribed, bobs),
            never_subscribed.body,
            "SubscribeToTask {bobs}"
        );
        let sent = door.call(alice, "SendMessage", reply(bobs, "hijack"));
        assert_eq!(as_never_held(&sent, bobs), never_send, "SendMessage {bobs}");
    }
    let held = door.wait_state(Some("operator"), &waiting, "TASK_STATE_INPUT_REQUIRED");
    assert_eq!(held["artifacts"][0]["artifactId"], "prompt", "{held}");

    let mut referencing = message("m-alice-ref", "compare");
    referencing["message"]["referenceTaskIds"] = json!([done, waiting]);
    let alices = door.start(alice, referencing);

    // The operator reaches both of bob's tasks: it reads them, lists them and answers the one
    // that waits.
    let read = door.answer(Some("operator"), "GetTask", json!({"id": done}));
    assert_eq!(read["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    let listed = listed_ids(&door.answer(Some("operator"), "ListTasks", json!({})));
    assert!(
        listed.contains(&done) && listed.contains(&waiting),
        "{listed:?}"
    );
    let answered = door.answer(
        Some("operator"),
        "SendMessage",
        returning_immediately(reply(&waiting, "left")),
    );
    assert_eq!(
        answered["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
        "{answered}"
    );
    door.wait_state(bob, &waiting, "TASK_STATE_COMPLETED");
    door.wait_state(alice, &alices, "TASK_STATE_COMPLETED");

    let requests: Vec<String> = door
        .provider
        .requests()
        .iter()
        .map(Value::to_string)
        .collect();
    for bobs in [&done, &waiting] {
        let line = format!("- {bobs}: not a task this capsule holds");
        assert!(
            requests.iter().any(|request| request.contains(&line)),
            "alice's task did not read {line:?}: {requests:?}"
        );
    }
}

/// The kind, state and artifact id of each event that carries no murmur frame, with each run of
/// consecutive `working` statuses read as one: when the subscription opens, the status that
/// starts a turn may or may not have been written yet.
fn standard_shape(events: &[Event]) -> Vec<(String, Option<String>, Option<String>)> {
    let mut shape: Vec<(String, Option<String>, Option<String>)> = events
        .iter()
        .filter(|event| !event.is_frame())
        .map(|event| {
            (
                event.kind(),
                event.state().map(str::to_string),
                event
                    .artifact()
                    .and_then(|artifact| artifact["artifactId"].as_str())
                    .map(str::to_string),
            )
        })
        .collect();
    shape.dedup_by(|later, earlier| later.0 == "status-update" && *later == *earlier);
    shape
}

/// A caller subscribed to its own running task receives the task, then the rest of its events,
/// and the stream ends after its terminal status.
#[test]
fn subscribe_own_running_task_streams_it_to_its_end() {
    let door = Door::launch(
        "subscribed-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start_with_delay(
            vec![
                tool_call_response(1, "no-such-tool", json!({})),
                end_turn(2, "four"),
                tool_call_response(3, "no-such-tool", json!({})),
                end_turn(4, "four"),
            ],
            Duration::from_millis(1500),
        ),
    );
    let alice = Some("alice");
    let mut shapes = Vec::new();
    for activated in [false, true] {
        let task_id = door.start(alice, message("m-sub", "what is two and two"));
        door.wait_state(alice, &task_id, "TASK_STATE_WORKING");
        let (response, events) = door.subscribe(alice, &task_id, activated, None);
        assert_eq!(response.status, 200, "{response:?}");
        assert_eq!(
            response.header("content-type"),
            Some("text/event-stream"),
            "{response:?}"
        );
        assert_eq!(
            response.header("a2a-extensions"),
            activated.then_some(STREAM_EXTENSION_URI),
            "{response:?}"
        );
        assert!(
            events.iter().all(|event| event.json["id"] == "sub-1"),
            "{response:?}"
        );
        assert!(
            events.iter().all(|event| event.task_id() == Some(&task_id)),
            "an event of another task: {response:?}"
        );
        assert_eq!(events[0].kind(), "task", "{response:?}");
        assert_eq!(events[0].id, None, "{response:?}");
        assert_eq!(events[0].state(), Some("TASK_STATE_WORKING"));
        let standard: Vec<&Event> = events.iter().filter(|event| !event.is_frame()).collect();
        let (answer, last) = (standard[standard.len() - 2], standard[standard.len() - 1]);
        assert_eq!(answer.artifact_text(), Some("four"), "{response:?}");
        assert_eq!(last.state(), Some("TASK_STATE_COMPLETED"), "{response:?}");
        let read = door.answer(alice, "GetTask", json!({"id": task_id}));
        assert_eq!(*last.status(), read["result"]["status"]);
        assert_eq!(
            events.last().unwrap(),
            last,
            "the terminal status is the last event"
        );
        if activated {
            assert!(events.iter().any(Event::is_frame), "{response:?}");
        } else {
            assert!(
                !response.body.contains(STREAM_EXTENSION_URI),
                "{response:?}"
            );
        }
        shapes.push(standard_shape(&events));
    }
    assert_eq!(shapes[0], shapes[1]);
}

/// Every refusal of `SubscribeToTask` is one JSON body, never a stream.
#[test]
fn subscribe_refused_before_any_stream() {
    let door = Door::launch(
        "refusing-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start((1..=2).map(|n| end_turn(n, "done")).collect()),
    );
    let (alice, bob) = (Some("alice"), Some("bob"));
    let bobs = door.start(bob, message("m-bob", "hi"));
    door.wait_state(bob, &bobs, "TASK_STATE_COMPLETED");
    let alices = door.start(alice, message("m-alice", "hi"));
    door.wait_state(alice, &alices, "TASK_STATE_COMPLETED");

    for task_id in [bobs.as_str(), "tsk_never_held"] {
        let (response, events) = door.subscribe(alice, task_id, false, None);
        assert!(events.is_empty());
        let error = error_of(&response);
        assert_eq!(error["code"], -32001, "{task_id}: {response:?}");
        assert_eq!(error["data"][0]["metadata"], json!({"taskId": task_id}));
    }
    let (ended, events) = door.subscribe(alice, &alices, true, None);
    assert!(events.is_empty());
    let error = error_of(&ended);
    assert_eq!(error["code"], -32004, "{ended:?}");
    assert_eq!(
        error["data"][0]["metadata"],
        json!({"taskId": alices, "state": "TASK_STATE_COMPLETED"})
    );
    let token = door.token("alice");
    let (unnamed, events) = stream_method(
        &door.addr,
        Some(&token),
        "SubscribeToTask",
        json!(1),
        json!({}),
        &[],
    );
    assert!(events.is_empty());
    assert_eq!(error_of(&unnamed)["code"], -32602, "{unnamed:?}");

    // A door that starts no task is up while its `task.md` runs, which the provider holds.
    let provider = common::ScriptedServer::start_with_delay(
        vec![end_turn(1, "done")],
        Duration::from_secs(120),
    );
    let home = driver_home();
    let project = agent_project(
        &provider.endpoint,
        "no-tasks",
        "",
        "lifecycle:\n  task_acceptance: none\n  after_task: exit\n",
    );
    let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
    fs::write(staged.workdir.join("task.md"), "Run the task").unwrap();
    let addr = launch_door(staged);
    let card = request(&addr, "GET", "/.well-known/agent-card.json", &[], "").json();
    assert_eq!(card["capabilities"]["streaming"], false, "{card}");
    let methods = common::card_door_methods(&card);
    assert!(methods.contains(&"ListTasks"), "{methods:?}");
    for streaming in ["SendStreamingMessage", "SubscribeToTask"] {
        assert!(!methods.contains(&streaming), "{methods:?}");
    }
    let (unsupported, events) = subscribe(&addr, None, "tsk_any", false, None);
    assert!(events.is_empty());
    let error = error_of(&unsupported);
    assert_eq!(error["code"], -32004, "{unsupported:?}");
    assert_eq!(error["data"][0]["reason"], "UNSUPPORTED_OPERATION");
    drop(provider);
}

/// Subscribes to `task_id` on a thread of its own, reading to the end of the stream.
fn subscribe_in_background(
    addr: &str,
    task_id: &str,
    last_event_id: Option<u64>,
) -> thread::JoinHandle<(Response, Vec<Event>)> {
    let (addr, task_id) = (addr.to_string(), task_id.to_string());
    let handle = thread::spawn(move || subscribe(&addr, None, &task_id, false, last_event_id));
    // Long enough for the subscription to open before the test changes the task.
    thread::sleep(Duration::from_secs(2));
    handle
}

/// A subscription stays open through `input-required`, sees the task resume when another request
/// answers it, and ends after its terminal status; one resumed from a cursor replays this task's
/// buffered events first.
#[test]
fn subscribe_input_required_survives_the_wait_and_resumes_from_a_cursor() {
    let door = Door::request_input(
        "waiting-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start(vec![
            ask_for_input(1, "which branch?"),
            end_turn(2, "left it is"),
            ask_for_input(3, "which side?"),
            end_turn(4, "right it is"),
        ]),
    );

    let first = door.start(None, message("m-first", "ask me"));
    door.wait_state(None, &first, "TASK_STATE_INPUT_REQUIRED");
    let watcher = subscribe_in_background(&door.addr, &first, None);
    assert!(
        !watcher.is_finished(),
        "the subscription ended on input-required"
    );
    door.answer(
        None,
        "SendMessage",
        returning_immediately(reply(&first, "left")),
    );
    let (response, events) = watcher.join().unwrap();
    assert_eq!(events[0].kind(), "task", "{response:?}");
    assert_eq!(events[0].state(), Some("TASK_STATE_INPUT_REQUIRED"));
    assert!(
        events[1..]
            .iter()
            .any(|event| event.state() == Some("TASK_STATE_WORKING")),
        "the subscription never saw the task resume: {response:?}"
    );
    assert_eq!(
        events.last().unwrap().state(),
        Some("TASK_STATE_COMPLETED"),
        "{response:?}"
    );
    assert!(events.iter().all(|event| event.task_id() == Some(&first)));

    let second = door.start(None, message("m-second", "ask me again"));
    door.wait_state(None, &second, "TASK_STATE_INPUT_REQUIRED");
    let watcher = subscribe_in_background(&door.addr, &second, Some(0));
    door.answer(
        None,
        "SendMessage",
        returning_immediately(reply(&second, "right")),
    );
    let (response, events) = watcher.join().unwrap();
    assert_eq!(events[0].kind(), "task", "{response:?}");
    assert_eq!(events[0].id, None, "{response:?}");
    assert!(events.iter().all(|event| event.task_id() == Some(&second)));
    let ids: Vec<u64> = events[1..]
        .iter()
        .map(|event| event.id.expect("every later event is numbered"))
        .collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ids do not ascend, or repeat: {ids:?}"
    );
    let states: Vec<&str> = events[1..].iter().filter_map(Event::state).collect();
    let waited = states
        .iter()
        .position(|state| *state == "TASK_STATE_INPUT_REQUIRED")
        .expect("the buffered input-required status is replayed");
    assert!(
        states[waited..].contains(&"TASK_STATE_WORKING"),
        "the live resume follows the replay: {states:?}"
    );
    assert_eq!(states.last(), Some(&"TASK_STATE_COMPLETED"), "{states:?}");
}

/// The four push config methods, and a message asking for push, are answered
/// `PushNotificationNotSupportedError` before anything starts, by every caller the door lets in.
#[test]
fn push_config_methods_and_push_requests_are_not_supported() {
    let door = Door::launch(
        "pushless-agent",
        &format!("{QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"),
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let public = Door::launch(
        "public-pushless-agent",
        QUEUE_SLEEP_YAML,
        common::ScriptedServer::start(vec![end_turn(1, "spare")]),
    );
    let assert_not_supported = |response: &Response, context: &str| {
        let error = error_of(response);
        assert_eq!(error["code"], -32003, "{context}: {response:?}");
        assert_eq!(
            error["message"], "push notifications are not supported by this agent",
            "{context}"
        );
        assert_eq!(
            error["data"][0]["reason"], "PUSH_NOTIFICATION_NOT_SUPPORTED",
            "{context}"
        );
        assert_eq!(error["data"][0]["domain"], "a2a-protocol.org", "{context}");
        assert_eq!(error["data"][0]["metadata"], json!({}), "{context}");
    };
    for (method, params) in push_config_requests("tsk_x") {
        for (target, who) in [
            (&door, Some("operator")),
            (&door, Some("watcher")),
            (&public, None),
        ] {
            let response = target.call(who, method, params.clone());
            assert_not_supported(&response, &format!("{method} as {who:?}"));
        }
        let unauthenticated = door.call(None, method, params);
        assert_eq!(unauthenticated.status, 401, "{unauthenticated:?}");
    }

    let mut asking = message("m-push", "notify me");
    asking["configuration"] =
        json!({"taskPushNotificationConfig": {"url": "https://example.com/hook"}});
    let sent = door.call(Some("operator"), "SendMessage", asking.clone());
    assert_not_supported(&sent, "SendMessage");
    let token = door.token("operator");
    let (streamed, events) = stream_method(
        &door.addr,
        Some(&token),
        "SendStreamingMessage",
        json!(1),
        asking,
        &[],
    );
    assert!(events.is_empty());
    assert_not_supported(&streamed, "SendStreamingMessage");
    let listed = door.answer(Some("operator"), "ListTasks", json!({}));
    assert_eq!(listed["result"]["totalSize"], 0, "{listed}");
    thread::sleep(Duration::from_millis(300));
    assert!(
        door.provider.requests().is_empty(),
        "a push request started a task"
    );

    // A credential holding no `ListTasks` scope is refused it, naming the scope.
    let unscoped = door.call(Some("watcher"), "ListTasks", json!({}));
    assert_eq!(unscoped.status, 403, "{unscoped:?}");
    assert_eq!(unscoped.json()["error"], "insufficient_scope");
    assert_eq!(
        unscoped.header("www-authenticate"),
        Some("Bearer realm=\"pushless-agent\", error=\"insufficient_scope\", scope=\"ListTasks\"")
    );

    for target in [&door, &public] {
        let card = request(&target.addr, "GET", "/.well-known/agent-card.json", &[], "").json();
        assert_eq!(card["capabilities"]["pushNotifications"], false, "{card}");
        let methods = common::card_door_methods(&card);
        for (method, _) in push_config_requests("tsk_x") {
            assert!(!methods.contains(&method), "{methods:?}");
        }
    }
}

/// A request naming a tenant is refused on every method the door serves, before it starts, stops
/// or cancels anything; the unset `""` is served.
#[test]
fn tenant_is_refused_on_every_served_method() {
    let door = Door::launch(
        "tenantless-agent",
        &format!("{QUEUE_SLEEP_YAML}{CREDENTIALS_YAML}"),
        common::ScriptedServer::start_with_delay(
            vec![end_turn(1, "held")],
            Duration::from_secs(20),
        ),
    );
    let operator = Some("operator");
    let running = door.start(operator, message("m-held", "hold"));
    door.wait_state(operator, &running, "TASK_STATE_WORKING");
    let count = || door.answer(operator, "ListTasks", json!({}))["result"]["totalSize"].clone();
    let before = count();

    let with_tenant = |mut params: Value, tenant: Value| {
        params["tenant"] = tenant;
        params
    };
    let served = [
        ("SendMessage", message("m-tenant", "hi")),
        ("SendStreamingMessage", message("m-tenant-stream", "hi")),
        ("GetTask", json!({"id": running})),
        ("ListTasks", json!({})),
        ("CancelTask", json!({"id": running})),
        ("SubscribeToTask", json!({"id": running})),
        ("GetExtendedAgentCard", json!({})),
        ("murmur.WatchSession", json!({})),
        ("murmur.StopSession", json!({})),
    ];
    for (method, params) in served.clone() {
        for tenant in [json!("acme"), json!(7)] {
            let response = door.call(
                operator,
                method,
                with_tenant(params.clone(), tenant.clone()),
            );
            let error = error_of(&response);
            assert_eq!(error["code"], -32602, "{method} {tenant}: {response:?}");
            assert_eq!(
                error["message"],
                format!(
                    "Invalid params: tenant {tenant} names a tenant; this agent serves no \
                     tenants, so omit tenant"
                ),
                "{method}"
            );
        }
    }
    assert_eq!(count(), before);
    door.wait_state(operator, &running, "TASK_STATE_WORKING");

    let unset = door.answer(operator, "GetTask", json!({"id": running, "tenant": ""}));
    assert_eq!(unset["result"]["id"], running.as_str(), "{unset}");
    let listed = door.answer(operator, "ListTasks", json!({"tenant": ""}));
    assert_eq!(listed["result"]["totalSize"], before, "{listed}");

    let (push, params) = push_config_requests("tsk_x")[0].clone();
    let pushed = door.call(operator, push, with_tenant(params, json!("acme")));
    assert_eq!(error_of(&pushed)["code"], -32003, "{pushed:?}");
    let unserved = door.call(operator, "tasks/list", json!({"tenant": "acme"}));
    assert_eq!(error_of(&unserved)["code"], -32601, "{unserved:?}");
}
