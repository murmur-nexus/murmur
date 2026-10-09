//! Tasks and messages on the door have the A2A v1.0 shape, end to end on an in-process
//! authenticated door: `TASK_STATE_*` states, `ROLE_*` roles, a `SendMessageResponse`, artifacts
//! with an `artifactId`, data parts the agent reads as fenced JSON, file parts refused with
//! `-32005`, `referenceTaskIds` handed to the agent, and malformed messages refused with
//! `-32602`. Every exchange goes through the recorded `common::door_capsule` helpers, so each one
//! is also checked against the A2A v1.0 proto.

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
    agent_project, driver_home, end_turn, launch_door, message, rpc, stage_door,
    stage_door_with_tools, trace_events, wait_completed, wait_for_requests, Response,
    AUTHENTICATION_YAML, DRIVER_NAME, DRIVER_VERSION, QUEUE_SLEEP_YAML,
};
use serde_json::{json, Value};
use tempfile::TempDir;

/// An in-process authenticated door, its operator token and what must outlive it.
struct Door {
    addr: String,
    operator: String,
    workdir: PathBuf,
    provider: common::ScriptedServer,
    _home: TempDir,
    _project: TempDir,
}

impl Door {
    /// A queue+sleep door named `name` holding at most `queue_depth` queued tasks, against
    /// `provider`.
    fn launch(name: &str, queue_depth: usize, provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let lifecycle =
            QUEUE_SLEEP_YAML.replace("queue_depth: 8", &format!("queue_depth: {queue_depth}"));
        let project = agent_project(
            &provider.endpoint,
            name,
            "",
            &format!("{lifecycle}{AUTHENTICATION_YAML}"),
        );
        let staged = stage_door(&home, &project.path().join("murmur.yaml"), None);
        Self::start(staged, provider, home, project)
    }

    /// A queue+sleep door whose model may call the `request-input-tool` fixture, against
    /// `provider`.
    fn launch_with_request_input(provider: common::ScriptedServer) -> Self {
        let home = driver_home();
        let artifacts = tempfile::tempdir().unwrap();
        common::publish_local(&home, &request_input_tool(artifacts.path())).success();
        let project = tempfile::tempdir().unwrap();
        let endpoint = &provider.endpoint;
        fs::write(
            project.path().join("murmur.yaml"),
            format!(
                "name: waiting-agent\nversion: 0.1.0\n\
                 artifacts:\n  - name: {DRIVER_NAME}\n    version: {DRIVER_VERSION}\n    \
                 runtime: driver\n    gateway:\n      endpoint: {endpoint}\n      \
                 api_key: test-key\n  - name: {TOOL_NAME}\n    version: 0.1.0\n    \
                 runtime: tool\n\
                 capabilities:\n  network:\n    allow:\n      - {endpoint}\n\
                 inference:\n  transport: http\n  model: test-model\n  driver:\n    \
                 artifact: {DRIVER_NAME}\n\
                 {QUEUE_SLEEP_YAML}{AUTHENTICATION_YAML}"
            ),
        )
        .unwrap();
        let staged = stage_door_with_tools(&home, &project.path().join("murmur.yaml"));
        Self::start(staged, provider, home, project)
    }

    fn start(
        staged: capsule_runtime::StagedSession,
        provider: common::ScriptedServer,
        home: TempDir,
        project: TempDir,
    ) -> Self {
        let operator = staged
            .door_tokens()
            .into_iter()
            .find(|(name, _)| name == "operator")
            .map(|(_, token)| token.expose().to_string())
            .expect("an authenticated door mints an operator token");
        let workdir = staged.workdir.clone();
        Door {
            addr: launch_door(staged),
            operator,
            workdir,
            provider,
            _home: home,
            _project: project,
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Response {
        let response = rpc(&self.addr, Some(&self.operator), method, params);
        assert_eq!(response.status, 200, "{response:?}");
        response
    }

    /// `SendMessage` with `message`, answered.
    fn send(&self, message: Value) -> Value {
        self.rpc("SendMessage", json!({ "message": message }))
            .json()
    }

    fn get(&self, task_id: &str) -> Value {
        self.rpc("GetTask", json!({ "id": task_id })).json()
    }

    /// Polls `GetTask` until `task_id` is in `state`, and returns the answer.
    fn wait_state(&self, task_id: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let task = self.get(task_id);
            if task["result"]["status"]["state"] == state {
                return task;
            }
            assert!(Instant::now() < deadline, "never {state}: {task}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn completed(&self, task_id: &str) -> Value {
        wait_completed(&self.addr, Some(&self.operator), task_id)
    }

    /// Every record of type `event_type` in the session's trace.
    fn trace(&self, event_type: &str) -> Vec<Value> {
        trace_events(&self.workdir)
            .into_iter()
            .filter(|event| event["event_type"] == event_type)
            .collect()
    }

    /// The `task_start` record of `task_id`.
    fn task_start(&self, task_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(start) = self
                .trace("task_start")
                .into_iter()
                .find(|event| event["task_id"] == task_id)
            {
                return start;
            }
            assert!(Instant::now() < deadline, "no task_start for {task_id}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The text of the provider's `index`th request that contains `needle`.
    fn provider_text(&self, index: usize, needle: &str) -> String {
        wait_for_requests(&self.provider, index + 1);
        let request = &self.provider.requests()[index];
        strings(request)
            .into_iter()
            .find(|text| text.contains(needle))
            .unwrap_or_else(|| panic!("no text containing {needle:?} in {request}"))
    }
}

/// Every string in `value`, depth first.
fn strings(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items.iter().flat_map(strings).collect(),
        Value::Object(fields) => fields.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// A message from the user carrying `parts`.
fn user(message_id: &str, parts: Value) -> Value {
    json!({"messageId": message_id, "role": "ROLE_USER", "parts": parts})
}

const TOOL_NAME: &str = "request-input-tool";

/// The `request-input-tool` fixture packed as an artifact in `dir`.
fn request_input_tool(dir: &Path) -> PathBuf {
    let path = dir.join(format!("{TOOL_NAME}-0.1.0.mur.zip"));
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("murmur.yaml", options).unwrap();
    writeln!(zip, "name: {TOOL_NAME}\nversion: 0.1.0\nruntime: wasm").unwrap();
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

/// The provider's answer asking the `request-input-tool` for input.
fn ask_for_input() -> String {
    json!({
        "id": "msg_ask",
        "type": "message",
        "role": "assistant",
        "model": "test-model",
        "content": [{"type": "tool_use", "id": "call_1", "name": TOOL_NAME,
            "input": {"data": "Which branch?"}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
    .to_string()
}

/// S1: every task, message and artifact the door answers is v1.0 ProtoJSON.
#[test]
fn shapes_on_the_wire_are_a2a_v1() {
    let door = Door::launch(
        "shapes-agent",
        1,
        common::ScriptedServer::start_with_delay(
            vec![end_turn(1, "four"), end_turn(2, "spare")],
            Duration::from_millis(1500),
        ),
    );

    let sent = door.send(user(
        "m-shapes-1",
        json!([{"text": "add 2 and 2", "mediaType": "text/plain"}]),
    ));
    let result = &sent["result"];
    assert!(result.get("id").is_none(), "a bare task: {sent}");
    assert_eq!(result["task"]["status"]["state"], "TASK_STATE_SUBMITTED");
    let task_id = result["task"]["id"].as_str().unwrap().to_string();
    door.wait_state(&task_id, "TASK_STATE_WORKING");

    // One task runs and one waits; a third finds the door busy.
    let queued = door.send(user("m-shapes-2", json!([{"text": "queued"}])));
    assert_eq!(
        queued["result"]["task"]["status"]["state"],
        "TASK_STATE_SUBMITTED"
    );
    let queued_id = queued["result"]["task"]["id"].as_str().unwrap().to_string();
    let busy = door.send(user("m-shapes-3", json!([{"text": "busy"}])));
    let rejected = &busy["result"]["task"];
    assert_eq!(rejected["status"]["state"], "TASK_STATE_REJECTED", "{busy}");
    let status_message = &rejected["status"]["message"];
    assert_eq!(status_message["role"], "ROLE_AGENT", "{busy}");
    assert_eq!(status_message["taskId"], rejected["id"], "{busy}");
    assert_eq!(status_message["contextId"], rejected["contextId"], "{busy}");
    assert_eq!(
        status_message["parts"],
        json!([{"text": "task rejected: capsule is busy", "mediaType": "text/plain"}])
    );

    // A live task's cancel is answered with its canceled state.
    let canceled = door.rpc("CancelTask", json!({"id": queued_id})).json();
    assert_eq!(
        canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
        "{canceled}"
    );

    let first = door.completed(&task_id);
    let second = door.get(&task_id);
    for read in [&first, &second] {
        assert_eq!(
            read["result"]["artifacts"],
            json!([{"artifactId": "response", "name": "response",
                "parts": [{"text": "four", "mediaType": "text/plain"}]}]),
            "{read}"
        );
        assert_eq!(read["result"]["status"]["message"]["role"], "ROLE_AGENT");
    }
}

/// S3: a data part reaches the agent as pretty-printed JSON in a fence one backtick longer than
/// any run in it, and the trace names the part kinds.
#[test]
fn a_data_part_reaches_the_agent_as_fenced_json() {
    let door = Door::launch(
        "data-agent",
        8,
        common::ScriptedServer::start(vec![end_turn(1, "summarised"), end_turn(2, "null")]),
    );
    let data = json!({"rows": [1, 2], "note": "a ``` b"});
    let sent = door.send(user(
        "m-data-1",
        json!([{"text": "summarise"}, {"data": data, "mediaType": "application/json"}]),
    ));
    assert_eq!(
        sent["result"]["task"]["status"]["state"], "TASK_STATE_SUBMITTED",
        "{sent}"
    );
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();
    door.completed(&task_id);

    let rendered = format!(
        "summarise\n````data media-type=application/json\n{}\n````",
        serde_json::to_string_pretty(&data).unwrap()
    );
    let text = door.provider_text(0, "summarise");
    assert!(
        text.contains(&rendered),
        "the agent reads:\n{text}\nnot:\n{rendered}"
    );
    let start = door.task_start(&task_id);
    assert_eq!(start["part_kinds"], json!(["text", "data"]), "{start}");
    assert_eq!(start["message_parts_bytes"], rendered.len(), "{start}");
    assert!(start.get("reference_task_ids").is_none(), "{start}");

    let null = door.send(user("m-data-2", json!([{"data": null}])));
    let null_id = null["result"]["task"]["id"].as_str().unwrap().to_string();
    door.completed(&null_id);
    let text = door.provider_text(1, "```data");
    assert!(text.contains("```data\nnull\n```"), "{text}");
    assert_eq!(door.task_start(&null_id)["part_kinds"], json!(["data"]));
}

/// The four refused messages S4 sends, `naming` a task or none: each part list over each method.
fn file_part_messages(naming: Option<&str>) -> Vec<(&'static str, Value, &'static str)> {
    let mut sent = Vec::new();
    for (parts, index) in [
        (
            json!([{"text": "a"},
                {"url": "https://example.com/x.pdf", "mediaType": "application/pdf"}]),
            "1",
        ),
        (json!([{"raw": "aGk="}]), "0"),
    ] {
        for method in ["SendMessage", "SendStreamingMessage"] {
            let mut message = user("m-file", parts.clone());
            if let Some(task_id) = naming {
                message["taskId"] = Value::from(task_id);
            }
            sent.push((method, json!({ "message": message }), index));
        }
    }
    sent
}

/// Sends each of [`file_part_messages`] and asserts it is refused with `-32005` as one JSON body.
fn assert_file_parts_refused(door: &Door, naming: Option<&str>) {
    for (method, params, index) in file_part_messages(naming) {
        let answer = door.rpc(method, params.clone());
        assert_eq!(
            answer.header("content-type"),
            Some("application/json"),
            "{method}: {answer:?}"
        );
        let answer = answer.json();
        assert_eq!(
            answer["error"]["code"], -32005,
            "{method} {params}: {answer}"
        );
        let info = &answer["error"]["data"][0];
        assert_eq!(info["reason"], "CONTENT_TYPE_NOT_SUPPORTED", "{answer}");
        assert_eq!(info["metadata"]["partIndex"], index, "{answer}");
        if index == "1" {
            assert_eq!(info["metadata"]["mediaType"], "application/pdf", "{answer}");
        } else {
            assert!(info["metadata"].get("mediaType").is_none(), "{answer}");
        }
    }
}

/// S4: a file part is refused before anything happens: no task, no trace, no provider request,
/// and nothing delivered to the task waiting for input.
#[test]
fn a_file_part_is_refused_and_starts_nothing() {
    let door = Door::launch_with_request_input(common::ScriptedServer::start(vec![
        ask_for_input(),
        end_turn(2, "done"),
    ]));

    assert_file_parts_refused(&door, None);
    assert!(
        door.provider.requests().is_empty(),
        "the provider was asked"
    );
    assert!(door.trace("task_start").is_empty());
    assert!(door.trace("a2a_task_received").is_empty());

    let sent = door.send(user("m-wait", json!([{"text": "start"}])));
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();
    door.wait_state(&task_id, "TASK_STATE_INPUT_REQUIRED");
    let starts = door.trace("task_start");
    assert_eq!(starts.len(), 1, "the first new task is the next one sent");
    assert_eq!(starts[0]["task_id"], task_id.as_str());

    assert_file_parts_refused(&door, Some(&task_id));
    assert_eq!(
        door.get(&task_id)["result"]["status"]["state"],
        "TASK_STATE_INPUT_REQUIRED"
    );
    assert_eq!(door.provider.requests().len(), 1, "nothing was delivered");
    assert_eq!(door.trace("task_start").len(), 1);
    assert_eq!(door.trace("a2a_task_received").len(), 1);

    let resumed = door.send(json!({"messageId": "m-reply", "role": "ROLE_USER",
        "taskId": task_id, "parts": [{"text": "main"}]}));
    assert_eq!(
        resumed["result"]["task"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    door.completed(&task_id);
}

/// S5: `referenceTaskIds` is recorded once each on `task_start`, and reaches the agent as a
/// block naming each task's state and outcome.
#[test]
fn reference_task_ids_are_recorded_and_reach_the_agent() {
    let door = Door::launch(
        "reference-agent",
        8,
        common::ScriptedServer::start(vec![end_turn(1, "four"), end_turn(2, "noted")]),
    );
    let a = door.send(user("m-ref-a", json!([{"text": "add 2 and 2"}])));
    let a = a["result"]["task"]["id"].as_str().unwrap().to_string();
    door.completed(&a);

    let mut b = user("m-ref-b", json!([{"text": "use the earlier answer"}]));
    b["referenceTaskIds"] = json!([a, "tsk_no_such", a]);
    let b = door.send(b);
    let b = b["result"]["task"]["id"].as_str().unwrap().to_string();
    door.completed(&b);

    assert_eq!(
        door.task_start(&b)["reference_task_ids"],
        json!([a, "tsk_no_such"])
    );
    let text = door.provider_text(1, "use the earlier answer");
    let block = format!(
        "use the earlier answer\n\nReferenced tasks:\n- {a}: completed\n```response\nfour\n```\n\
         - tsk_no_such: not a task this capsule holds"
    );
    assert!(text.contains(&block), "the agent reads:\n{text}");

    let starts = door.trace("task_start").len();
    let distinct: Vec<String> = (0..17).map(|n| format!("tsk_{n}")).collect();
    for references in [json!(distinct), json!([""]), json!([7])] {
        let mut message = user("m-ref-bad", json!([{"text": "x"}]));
        message["referenceTaskIds"] = references.clone();
        let answer = door.send(message);
        assert_eq!(answer["error"]["code"], -32602, "{references}: {answer}");
    }
    assert_eq!(door.trace("task_start").len(), starts, "a task started");
    assert_eq!(door.provider.requests().len(), 2);
}

/// S6: a message that is not a v1.0 message is refused with `-32602` and starts nothing, and
/// `GetTask` names the task it reads.
#[test]
fn malformed_messages_and_an_unnamed_get_task_are_refused() {
    let door = Door::launch(
        "malformed-agent",
        8,
        common::ScriptedServer::start(vec![end_turn(1, "never")]),
    );
    let text = json!([{"text": "a"}]);
    let with = |field: &str, value: Value| {
        let mut message = user("m-bad", text.clone());
        message[field] = value;
        json!({ "message": message })
    };
    let without = |field: &str| {
        let mut message = user("m-bad", text.clone());
        message.as_object_mut().unwrap().remove(field);
        json!({ "message": message })
    };
    let parts = |parts: Value| json!({ "message": user("m-bad", parts) });
    for params in [
        with("role", json!("user")),
        with("role", json!("ROLE_AGENT")),
        without("role"),
        without("messageId"),
        with("messageId", json!("")),
        parts(json!([])),
        parts(json!([{}])),
        parts(json!([{"text": "a", "data": 1}])),
        parts(json!([{"kind": "file", "file": {"uri": "x"}}])),
        parts(json!([{"text": 7}])),
        json!({}),
        json!({"messageId": "m-bare", "role": "user", "parts": [{"kind": "text", "text": "a"}]}),
        message("m-bare", "a")["message"].clone(),
    ] {
        let answer = door.rpc("SendMessage", params.clone()).json();
        assert_eq!(answer["error"]["code"], -32602, "{params}: {answer}");
    }
    assert!(door.provider.requests().is_empty());
    assert!(door.trace("task_start").is_empty());

    for params in [json!({}), json!({"id": ""}), json!({"id": 7})] {
        let answer = door.rpc("GetTask", params.clone()).json();
        assert_eq!(answer["error"]["code"], -32602, "{params}: {answer}");
        assert_eq!(answer["error"]["message"], "GetTask requires an id");
    }
}
