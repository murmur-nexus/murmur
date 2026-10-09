//! Tests of the conformance checker itself. They sit outside `a2a_conformance.rs` because
//! the `murmur-cli` integration tests compile that file into every test binary that uses
//! `common`, and a `#[cfg(test)]` module inside it would run once per binary there.

use serde_json::{json, Value};

use crate::a2a_conformance::{
    check_agent_card, check_message, check_request, check_response, check_stream_event,
    resolve_method, v1_methods, MethodKind, Violation, WirePart, EXTENSION_METHODS,
};

/// A conformant card: the one a capsule `my-agent` 0.1.0 on port 41873 serves.
fn conformant_card() -> Value {
    json!({
        "name": "my-agent",
        "description": "Murmur capsule my-agent 0.1.0",
        "version": "0.1.0",
        "supportedInterfaces": [
            { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" }
        ],
        "capabilities": {
            "streaming": true,
            "pushNotifications": false,
            "extendedAgentCard": false,
            "extensions": [
                {
                    "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1",
                    "description": "Every JSON-RPC method this door answers.",
                    "required": false,
                    "params": {
                        "methods": ["SendMessage", "SendStreamingMessage", "stream/watch", "GetTask", "CancelTask", "session/stop"]
                    }
                },
                {
                    "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1",
                    "description": "The session answering this address and what the capsule may do.",
                    "required": false,
                    "params": {
                        "sessionId": "ses_019f01a940ce7761854e768ecbe3d399",
                        "tools": ["bash"],
                        "shell": true,
                        "network": true,
                        "planes": ["files"]
                    }
                }
            ]
        },
        "securitySchemes": {},
        "securityRequirements": [],
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [
            {
                "id": "task",
                "name": "Run a task",
                "description": "Runs one task given as a text message and reports its outcome.",
                "tags": ["task"]
            }
        ]
    })
}

fn errors_for(card: &Value) -> Vec<String> {
    check_agent_card(card).expect_err(&format!("the checker accepted {card:#}"))
}

fn assert_reports(errors: &[String], expected: &str) {
    assert!(
        errors.iter().any(|error| error == expected),
        "expected {expected:?} among {errors:#?}"
    );
}

#[test]
fn a2a_card_conformance_accepts_a_conformant_card() {
    assert_eq!(check_agent_card(&conformant_card()), Ok(()));
}

#[test]
fn a2a_card_conformance_rejects_the_previous_card_shape() {
    let previous = json!({
        "name": "my-agent",
        "version": "0.1.0",
        "url": "localhost:41873",
        "session_id": "ses_019f01a940ce7761854e768ecbe3d399",
        "capabilities": {
            "tools": ["bash"],
            "shell": true,
            "network": true,
            "streaming": true,
            "cancellation": true
        },
        "serves": {
            "methods": ["SendMessage", "SendStreamingMessage", "stream/watch", "GetTask", "CancelTask", "session/stop"],
            "planes": ["files"]
        }
    });
    let errors = errors_for(&previous);
    for expected in [
        "unknown field `url`",
        "unknown field `session_id`",
        "unknown field `serves`",
        "unknown field `capabilities.tools`",
        "unknown field `capabilities.shell`",
        "unknown field `capabilities.network`",
        "unknown field `capabilities.cancellation`",
        "missing required field `description`",
        "missing required field `supportedInterfaces`",
        "missing required field `defaultInputModes`",
        "missing required field `defaultOutputModes`",
        "missing required field `skills`",
    ] {
        assert_reports(&errors, expected);
    }
}

#[test]
fn a2a_card_conformance_rejects_the_0_3_security_key() {
    let mut card = conformant_card();
    card["security"] = json!([]);
    assert_eq!(errors_for(&card), ["unknown field `security`"]);
}

#[test]
fn a2a_card_conformance_rejects_a_card_without_skills() {
    let mut card = conformant_card();
    card.as_object_mut().unwrap().remove("skills");
    assert_eq!(errors_for(&card), ["missing required field `skills`"]);
}

#[test]
fn a2a_card_conformance_rejects_an_interface_without_a_protocol_version() {
    let mut card = conformant_card();
    card["supportedInterfaces"][0]
        .as_object_mut()
        .unwrap()
        .remove("protocolVersion");
    assert_eq!(
        errors_for(&card),
        ["missing required field `supportedInterfaces[0].protocolVersion`"]
    );
}

#[test]
fn a2a_card_conformance_rejects_an_unknown_key_in_an_extension() {
    let mut card = conformant_card();
    card["capabilities"]["extensions"][1]["sessionId"] = json!("ses_1");
    assert_eq!(
        errors_for(&card),
        ["unknown field `capabilities.extensions[1].sessionId`"]
    );
}

#[test]
fn a2a_card_conformance_accepts_any_keys_inside_extension_params() {
    let mut card = conformant_card();
    card["capabilities"]["extensions"][0]["params"] = json!({
        "anything": {"nested": [1, "two", null]},
        "snake_case_is_fine_here": true
    });
    assert_eq!(check_agent_card(&card), Ok(()));
}

#[test]
fn a2a_card_conformance_reads_required_fields_from_the_proto() {
    let mut errors = errors_for(&json!({}));
    errors.sort();
    assert_eq!(
        errors,
        [
            "missing required field `capabilities`",
            "missing required field `defaultInputModes`",
            "missing required field `defaultOutputModes`",
            "missing required field `description`",
            "missing required field `name`",
            "missing required field `skills`",
            "missing required field `supportedInterfaces`",
            "missing required field `version`",
        ]
    );
}

#[test]
fn a2a_card_conformance_checks_types_and_oneofs() {
    let mut card = conformant_card();
    card["capabilities"]["streaming"] = json!("yes");
    card["skills"][0]["tags"] = json!("task");
    card["securitySchemes"] = json!({
        "both": {
            "apiKeySecurityScheme": {"location": "header", "name": "x-key"},
            "mtlsSecurityScheme": {}
        }
    });
    let errors = errors_for(&card);
    assert_reports(
        &errors,
        "`capabilities.streaming` must be a bool, found a string",
    );
    assert_reports(
        &errors,
        "`skills[0].tags` must be a JSON array (repeated string), found a string",
    );
    assert!(
        errors.iter().any(|error| error.starts_with(
            "`securitySchemes[\"both\"]` sets more than one member of oneof `scheme`"
        )),
        "{errors:#?}"
    );
}

#[test]
fn a2a_card_conformance_names_the_json_spelling_of_a_proto_name() {
    let mut card = conformant_card();
    card["default_input_modes"] = json!(["text/plain"]);
    assert_reports(
        &errors_for(&card),
        "unknown field `default_input_modes`: the JSON name of proto field `default_input_modes` is `defaultInputModes`",
    );
}

#[test]
fn a2a_card_conformance_does_not_panic_on_malformed_input() {
    for card in [
        json!(null),
        json!("a card"),
        json!([conformant_card()]),
        json!({"capabilities": [], "skills": {}, "supportedInterfaces": [7], "name": 1}),
    ] {
        assert!(check_agent_card(&card).is_err(), "{card}");
    }
}

// ── The wire: any message, the method table, envelopes and stream events ─────────────────────

fn message_errors(value: &Value, message: &str) -> Vec<String> {
    let mut errors = check_message(value, message)
        .expect_err(&format!("the checker accepted {value:#} as {message}"));
    errors.sort();
    errors
}

fn keys(violations: &[Violation]) -> Vec<String> {
    violations.iter().map(Violation::key).collect()
}

fn errors_under(violations: &[Violation], key: &str) -> Vec<String> {
    violations
        .iter()
        .find(|violation| violation.key() == key)
        .unwrap_or_else(|| panic!("no violation {key} among {violations:#?}"))
        .errors
        .clone()
}

/// A completed task in A2A 0.3's shape: a kebab-case state, role `agent`, and an artifact with no
/// `artifactId`.
fn door_task() -> Value {
    json!({
        "id": "tsk_1",
        "contextId": "ctx_1",
        "status": {
            "state": "completed",
            "message": {"messageId": "msg_1_status", "role": "agent", "parts": [{"text": "done"}]}
        },
        "artifacts": [{"name": "response", "parts": [{"text": "done"}]}],
        "metadata": {"murmur": {"sessionId": "ses_1"}}
    })
}

fn v1_task() -> Value {
    json!({
        "id": "tsk_1",
        "contextId": "ctx_1",
        "status": {
            "state": "TASK_STATE_COMPLETED",
            "message": {"messageId": "msg_1_status", "role": "ROLE_AGENT", "parts": [{"text": "done"}]},
            "timestamp": "2026-10-08T12:00:00Z"
        },
        "artifacts": [{"artifactId": "art_1", "name": "response", "parts": [{"text": "done"}]}],
        "metadata": {"murmur": {"sessionId": "ses_1"}}
    })
}

fn request(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
}

#[test]
fn a2a_wire_conformance_send_message_request() {
    let v1 =
        json!({"message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hi"}]}});
    assert_eq!(check_message(&v1, "lf.a2a.v1.SendMessageRequest"), Ok(()));
    let door = json!({"message": {"messageId": "m1", "role": "user", "parts": [{"text": "hi"}]}});
    assert_eq!(
        message_errors(&door, "lf.a2a.v1.SendMessageRequest"),
        ["`message.role` is \"user\", not a value of Role"]
    );
}

#[test]
fn a2a_wire_conformance_send_message_response() {
    let v1 = json!({"task": {"id": "tsk_1", "contextId": "ctx_1", "status": {"state": "TASK_STATE_SUBMITTED"}}});
    assert_eq!(check_message(&v1, "lf.a2a.v1.SendMessageResponse"), Ok(()));
    let bare_task = json!({"id": "tsk_1", "contextId": "ctx_1", "status": {"state": "submitted"}});
    assert_eq!(
        message_errors(&bare_task, "lf.a2a.v1.SendMessageResponse"),
        [
            "`<SendMessageResponse>` sets no member of oneof `payload`",
            "unknown field `contextId`",
            "unknown field `id`",
            "unknown field `status`",
        ]
    );
}

#[test]
fn a2a_wire_conformance_task() {
    assert_eq!(check_message(&v1_task(), "lf.a2a.v1.Task"), Ok(()));
    assert_eq!(
        message_errors(&door_task(), "lf.a2a.v1.Task"),
        [
            "`status.message.role` is \"agent\", not a value of Role",
            "`status.state` is \"completed\", not a value of TaskState",
            "missing required field `artifacts[0].artifactId`",
        ]
    );
}

#[test]
fn a2a_wire_conformance_stream_response() {
    let v1 = json!({"statusUpdate": {"taskId": "tsk_1", "contextId": "ctx_1", "status": {"state": "TASK_STATE_WORKING"}}});
    assert_eq!(check_message(&v1, "lf.a2a.v1.StreamResponse"), Ok(()));
    let murmur_frame = json!({"id": "tsk_1", "context_id": "ctx_1", "status": {"state": "working"}, "final": false});
    assert_eq!(
        message_errors(&murmur_frame, "lf.a2a.v1.StreamResponse"),
        [
            "`<StreamResponse>` sets no member of oneof `payload`",
            "unknown field `context_id`",
            "unknown field `final`",
            "unknown field `id`",
            "unknown field `status`",
        ]
    );
}

#[test]
fn a2a_wire_conformance_get_task_request() {
    assert_eq!(
        check_message(
            &json!({"id": "tsk_1", "historyLength": 2}),
            "lf.a2a.v1.GetTaskRequest"
        ),
        Ok(())
    );
    assert_eq!(
        message_errors(&json!({"historyLength": 2}), "lf.a2a.v1.GetTaskRequest"),
        ["missing required field `id`"]
    );
}

#[test]
fn a2a_wire_conformance_cancel_task_request() {
    assert_eq!(
        check_message(&json!({"id": "tsk_1"}), "lf.a2a.v1.CancelTaskRequest"),
        Ok(())
    );
    assert_eq!(
        message_errors(&json!({"id": 7}), "lf.a2a.v1.CancelTaskRequest"),
        ["`id` must be a string, found a number"]
    );
}

#[test]
fn a2a_wire_conformance_names_only_messages_of_the_package() {
    for name in [
        "Task",
        "lf.a2a.v2.Task",
        "google.protobuf.Empty",
        "lf.a2a.v1.Nope",
        "lf.a2a.v1.TaskState",
    ] {
        assert_eq!(
            check_message(&json!({}), name),
            Err(vec![format!("a2a.proto defines no message `{name}`")])
        );
    }
}

#[test]
fn a2a_wire_conformance_bytes_must_be_base64() {
    for good in [
        "",
        "aGk=",
        "aGk",
        "aGVsbG8gd29ybGQ=",
        "+/8=",
        "+/8",
        "-_8=",
        "-_8",
        "aGVs",
    ] {
        assert_eq!(
            check_message(&json!({"raw": good}), "lf.a2a.v1.Part"),
            Ok(()),
            "{good:?}"
        );
    }
    for bad in [
        "a",
        "aGk==",
        "a===",
        "aG=k",
        "+_8=",
        "not base64!",
        "aGVsb",
        "=",
    ] {
        assert_eq!(
            message_errors(&json!({"raw": bad}), "lf.a2a.v1.Part"),
            ["`raw` is not base64 (bytes)"],
            "{bad:?}"
        );
    }
    assert_eq!(
        message_errors(&json!({"raw": 7}), "lf.a2a.v1.Part"),
        ["`raw` must be a bytes, found a number"]
    );
}

#[test]
fn a2a_wire_conformance_struct_must_be_an_object() {
    let any = json!({"text": "a", "metadata": {"n": [1, null, {"x": true}], "s": "t"}});
    assert_eq!(check_message(&any, "lf.a2a.v1.Part"), Ok(()));
    assert_eq!(
        message_errors(&json!({"text": "a", "metadata": [1]}), "lf.a2a.v1.Part"),
        ["`metadata` must be a google.protobuf.Struct, found an array"]
    );
}

#[test]
fn a2a_wire_conformance_value_takes_any_json_and_a_null_counts_as_set() {
    for data in [
        json!(null),
        json!(1),
        json!("s"),
        json!([1, {}]),
        json!({"k": null}),
    ] {
        assert_eq!(
            check_message(&json!({"data": data}), "lf.a2a.v1.Part"),
            Ok(()),
            "{data}"
        );
    }
    assert_eq!(
        message_errors(&json!({"text": "a", "data": null}), "lf.a2a.v1.Part"),
        ["`<Part>` sets more than one member of oneof `content`: text, data"]
    );
    // A `null` in any other field is the field unset.
    assert_eq!(
        message_errors(&json!({"text": null}), "lf.a2a.v1.Part"),
        ["`<Part>` sets no member of oneof `content`"]
    );
}

#[test]
fn a2a_wire_conformance_empty_must_be_an_empty_object() {
    let delete = request(
        "DeleteTaskPushNotificationConfig",
        json!({"taskId": "tsk_1", "id": "cfg_1"}),
    );
    let answer = |result: Value| json!({"jsonrpc": "2.0", "id": 1, "result": result});
    assert_eq!(check_response(&delete, &answer(json!({}))), []);
    let violations = check_response(&delete, &answer(json!({"done": true})));
    assert_eq!(
        keys(&violations),
        ["DeleteTaskPushNotificationConfig result google.protobuf.Empty"]
    );
    assert_eq!(
        violations[0].errors,
        ["`result` must be a google.protobuf.Empty, found an object with fields"]
    );
    assert_eq!(
        check_response(&delete, &answer(json!([])))[0].errors,
        ["`result` must be a google.protobuf.Empty, found an array"]
    );
}

#[test]
fn a2a_wire_conformance_timestamp_must_be_rfc_3339() {
    let status = |timestamp: Value| json!({"state": "TASK_STATE_WORKING", "timestamp": timestamp});
    for good in [
        "2026-10-08T12:00:00Z",
        "2026-10-08T12:00:00.123456789+02:00",
        "2024-02-29T23:59:60-11:30",
        "2026-10-08t12:00:00.5z",
    ] {
        assert_eq!(
            check_message(&status(json!(good)), "lf.a2a.v1.TaskStatus"),
            Ok(()),
            "{good}"
        );
    }
    for bad in [
        "2026-13-01T00:00:00Z",
        "2026-00-01T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-04-31T00:00:00Z",
        "2026-10-00T00:00:00Z",
        "2026-10-08 12:00:00Z",
        "2026-10-08T24:00:00Z",
        "2026-10-08T12:60:00Z",
        "2026-10-08T12:00:61Z",
        "2026-10-08T12:00:00",
        "2026-10-08T12:00:00.Z",
        "2026-10-08T12:00:00+2:00",
        "2026-10-08T12:00:00+24:00",
        "2026-10-08T12:00:00Zjunk",
        "1696766400",
        "",
    ] {
        assert_eq!(
            message_errors(&status(json!(bad)), "lf.a2a.v1.TaskStatus"),
            [format!("`timestamp` is {bad:?}, not an RFC 3339 timestamp")]
        );
    }
    assert_eq!(
        message_errors(&status(json!(1696766400)), "lf.a2a.v1.TaskStatus"),
        ["`timestamp` must be a google.protobuf.Timestamp, found a number"]
    );
}

#[test]
fn a2a_wire_conformance_enums_take_names_only() {
    assert_eq!(
        message_errors(&json!({"state": 3}), "lf.a2a.v1.TaskStatus"),
        ["`state` is 3: enum TaskState takes a value name, not an integer"]
    );
    assert_eq!(
        message_errors(
            &json!({"state": "TASK_STATE_UNSPECIFIED"}),
            "lf.a2a.v1.TaskStatus"
        ),
        ["required field `state` holds its zero value `TASK_STATE_UNSPECIFIED`"]
    );
    let message = json!({"messageId": "m", "role": "ROLE_UNSPECIFIED", "parts": [{"text": "a"}]});
    assert_eq!(
        message_errors(&message, "lf.a2a.v1.Message"),
        ["required field `role` holds its zero value `ROLE_UNSPECIFIED`"]
    );
    // An enum field that is not required may hold its zero value.
    assert_eq!(
        check_message(
            &json!({"status": "TASK_STATE_UNSPECIFIED"}),
            "lf.a2a.v1.ListTasksRequest"
        ),
        Ok(())
    );
}

#[test]
fn a2a_wire_conformance_int32_must_be_in_range() {
    let get = |history: Value| json!({"id": "tsk_1", "historyLength": history});
    for good in [
        json!(0),
        json!(-2147483648i64),
        json!(2147483647),
        json!("42"),
        json!("-7"),
    ] {
        assert_eq!(
            check_message(&get(good.clone()), "lf.a2a.v1.GetTaskRequest"),
            Ok(()),
            "{good}"
        );
    }
    assert_eq!(
        message_errors(&get(json!(2147483648i64)), "lf.a2a.v1.GetTaskRequest"),
        ["`historyLength` is 2147483648, out of range for int32"]
    );
    assert_eq!(
        message_errors(&get(json!("-2147483649")), "lf.a2a.v1.GetTaskRequest"),
        ["`historyLength` is -2147483649, out of range for int32"]
    );
    assert_eq!(
        message_errors(&get(json!(1.5)), "lf.a2a.v1.GetTaskRequest"),
        ["`historyLength` must be a int32, found a number"]
    );
}

#[test]
fn a2a_wire_conformance_every_oneof_needs_one_member() {
    assert_eq!(
        message_errors(&json!({}), "lf.a2a.v1.Part"),
        ["`<Part>` sets no member of oneof `content`"]
    );
    assert_eq!(
        message_errors(
            &json!({"task": v1_task(), "message": {}}),
            "lf.a2a.v1.StreamResponse"
        )
        .iter()
        .filter(|error| error.contains("oneof"))
        .collect::<Vec<_>>(),
        ["`<StreamResponse>` sets more than one member of oneof `payload`: task, message"]
    );
    let mut card_errors = check_agent_card(&json!({"securitySchemes": {"none": {}}})).unwrap_err();
    card_errors.retain(|error| error.contains("oneof"));
    assert_eq!(
        card_errors,
        ["`securitySchemes[\"none\"]` sets no member of oneof `scheme`"]
    );
}

#[test]
fn a2a_wire_conformance_method_table_is_the_service_block() {
    let table: Vec<(&str, &str, &str, bool)> = v1_methods()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m.name.as_str(),
                m.params.as_str(),
                m.result.as_str(),
                m.streaming,
            )
        })
        .collect();
    assert_eq!(
        table,
        [
            (
                "SendMessage",
                "lf.a2a.v1.SendMessageRequest",
                "lf.a2a.v1.SendMessageResponse",
                false
            ),
            (
                "SendStreamingMessage",
                "lf.a2a.v1.SendMessageRequest",
                "lf.a2a.v1.StreamResponse",
                true
            ),
            (
                "GetTask",
                "lf.a2a.v1.GetTaskRequest",
                "lf.a2a.v1.Task",
                false
            ),
            (
                "ListTasks",
                "lf.a2a.v1.ListTasksRequest",
                "lf.a2a.v1.ListTasksResponse",
                false
            ),
            (
                "CancelTask",
                "lf.a2a.v1.CancelTaskRequest",
                "lf.a2a.v1.Task",
                false
            ),
            (
                "SubscribeToTask",
                "lf.a2a.v1.SubscribeToTaskRequest",
                "lf.a2a.v1.StreamResponse",
                true
            ),
            (
                "CreateTaskPushNotificationConfig",
                "lf.a2a.v1.TaskPushNotificationConfig",
                "lf.a2a.v1.TaskPushNotificationConfig",
                false
            ),
            (
                "GetTaskPushNotificationConfig",
                "lf.a2a.v1.GetTaskPushNotificationConfigRequest",
                "lf.a2a.v1.TaskPushNotificationConfig",
                false
            ),
            (
                "ListTaskPushNotificationConfigs",
                "lf.a2a.v1.ListTaskPushNotificationConfigsRequest",
                "lf.a2a.v1.ListTaskPushNotificationConfigsResponse",
                false
            ),
            (
                "GetExtendedAgentCard",
                "lf.a2a.v1.GetExtendedAgentCardRequest",
                "lf.a2a.v1.AgentCard",
                false
            ),
            (
                "DeleteTaskPushNotificationConfig",
                "lf.a2a.v1.DeleteTaskPushNotificationConfigRequest",
                "google.protobuf.Empty",
                false
            ),
        ]
    );
}

#[test]
fn a2a_wire_conformance_every_message_the_table_names_is_defined() {
    for method in v1_methods().unwrap() {
        for name in [&method.params, &method.result] {
            if !name.starts_with("google.protobuf.") {
                assert!(
                    check_message(&json!({}), name)
                        .err()
                        .unwrap_or_default()
                        .iter()
                        .all(|error| !error.contains("defines no message")),
                    "{name}"
                );
            }
        }
    }
}

#[test]
fn a2a_wire_conformance_resolves_v1_and_extension_names() {
    for v1 in [
        "SendMessage",
        "SendStreamingMessage",
        "GetTask",
        "CancelTask",
        "GetExtendedAgentCard",
    ] {
        let resolved = resolve_method(v1).unwrap();
        assert_eq!((resolved.kind, resolved.name), (MethodKind::V1, v1));
        assert_eq!(resolved.spec.unwrap().name, v1);
    }
    for (name, streaming) in EXTENSION_METHODS {
        let resolved = resolve_method(name).unwrap();
        assert_eq!(
            (
                resolved.kind,
                resolved.name,
                resolved.streaming,
                resolved.spec
            ),
            (MethodKind::Extension, name, streaming, None)
        );
    }
    assert!(resolve_method("tasks/list").is_none());
    assert!(resolve_method("sendmessage").is_none());
}

#[test]
fn a2a_wire_conformance_part_spellings() {
    let spellings: Vec<&str> = WirePart::ALL.iter().map(|part| part.as_str()).collect();
    assert_eq!(
        spellings,
        ["method", "header", "envelope", "params", "result", "event"]
    );
    for part in WirePart::ALL {
        assert_eq!(WirePart::parse(part.as_str()), Some(part));
    }
    assert_eq!(WirePart::parse("Method"), None);
}

#[test]
fn a2a_wire_conformance_a_v1_request_raises_nothing() {
    let send = request(
        "SendMessage",
        json!({"message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hi"}]}}),
    );
    assert_eq!(check_request(&send), []);
    let card = json!({"jsonrpc": "2.0", "id": "a", "method": "GetExtendedAgentCard"});
    assert_eq!(check_request(&card), []);
}

#[test]
fn a2a_wire_conformance_a2a_0_3_names_are_not_v1_methods() {
    for name in [
        "message/send",
        "message/stream",
        "tasks/get",
        "tasks/cancel",
        "agent/getAuthenticatedExtendedCard",
    ] {
        assert!(resolve_method(name).is_none(), "{name}");
        let violations = check_request(&request(name, json!({})));
        assert_eq!(keys(&violations), [format!("{name} method -")]);
        assert_eq!(
            violations[0].errors,
            [format!(
                "`{name}` is not a v1.0 method or a declared murmur extension method"
            )]
        );
    }
}

#[test]
fn a2a_wire_conformance_absent_params_are_checked_as_empty() {
    let get = json!({"jsonrpc": "2.0", "id": 1, "method": "GetTask"});
    let violations = check_request(&get);
    assert_eq!(
        keys(&violations),
        ["GetTask params lf.a2a.v1.GetTaskRequest"]
    );
    assert_eq!(violations[0].errors, ["missing required field `params.id`"]);
}

#[test]
fn a2a_wire_conformance_extension_methods_are_envelope_checked_only() {
    assert_eq!(
        check_request(&request("stream/watch", json!({"anything": 1}))),
        []
    );
    assert_eq!(
        check_request(&request("session/stop", json!({"reason": "x"}))),
        []
    );
    let violations = check_request(&json!({"jsonrpc": "1.0", "id": 1, "method": "session/stop"}));
    assert_eq!(keys(&violations), ["session/stop envelope -"]);
    let stop = request("session/stop", json!({}));
    let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {"session_id": "ses_1", "canceled": [], "residue": null}});
    assert_eq!(check_response(&stop, &answer), []);
}

#[test]
fn a2a_wire_conformance_unknown_methods_are_method_violations() {
    let violations = check_request(&request("tasks/list", json!({})));
    assert_eq!(keys(&violations), ["tasks/list method -"]);
    assert_eq!(
        violations[0].errors,
        ["`tasks/list` is not a v1.0 method or a declared murmur extension method"]
    );
}

#[test]
fn a2a_wire_conformance_request_envelope_rules() {
    let cases = [
        (
            json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard", "params": {}}),
            vec![],
        ),
        (
            json!({"id": 1, "method": "GetExtendedAgentCard"}),
            vec!["`jsonrpc` must be \"2.0\", found nothing"],
        ),
        (
            json!({"jsonrpc": 2.0, "id": 1, "method": "GetExtendedAgentCard"}),
            vec!["`jsonrpc` must be \"2.0\", found 2.0"],
        ),
        (
            json!({"jsonrpc": "2.0", "method": "GetExtendedAgentCard"}),
            vec!["`id` must be a string or an integer, found nothing"],
        ),
        (
            json!({"jsonrpc": "2.0", "id": null, "method": "GetExtendedAgentCard"}),
            vec!["`id` must be a string or an integer, found null"],
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "GetExtendedAgentCard"}),
            vec!["`id` must be a string or an integer, found 1.5"],
        ),
        (
            json!({"jsonrpc": "2.0", "id": {}, "method": "GetExtendedAgentCard"}),
            vec!["`id` must be a string or an integer, found {}"],
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard", "params": []}),
            vec!["`params` must be a JSON object, found an array"],
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "method": "GetExtendedAgentCard", "extra": true}),
            vec!["unknown request key `extra`"],
        ),
    ];
    for (request, expected) in cases {
        let violations = check_request(&request);
        if expected.is_empty() {
            assert_eq!(violations, [], "{request}");
        } else {
            assert_eq!(
                errors_under(&violations, "GetExtendedAgentCard envelope -"),
                expected,
                "{request}"
            );
        }
    }
    let nameless = check_request(&json!({"jsonrpc": "2.0", "id": 1, "method": 7}));
    assert_eq!(keys(&nameless), ["- envelope -"]);
    assert_eq!(nameless[0].errors, ["`method` must be a string, found 7"]);
    assert_eq!(
        check_request(&json!("not an object"))[0].errors,
        ["the request must be a JSON object, found a string"]
    );
}

#[test]
fn a2a_wire_conformance_response_envelope_rules() {
    let get = request("GetTask", json!({"id": "tsk_1"}));
    let ok = json!({"jsonrpc": "2.0", "id": 1, "result": v1_task()});
    assert_eq!(check_response(&get, &ok), []);
    let error =
        |code: Value| json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": "m"}});
    for code in [
        -32700, -32600, -32601, -32602, -32603, -32001, -32050, -32099, -31001, -31002, -31999,
        -32769, 500,
    ] {
        assert_eq!(check_response(&get, &error(json!(code))), [], "{code}");
    }
    let cases = [
        (json!({"jsonrpc": "2.0", "id": 2, "result": v1_task()}), "`id` is 2, not the request's id 1"),
        (json!({"jsonrpc": "2.0", "id": "1", "result": v1_task()}), "`id` is \"1\", not the request's id 1"),
        (json!({"jsonrpc": "2.0", "result": v1_task()}), "missing `id`"),
        (json!({"id": 1, "result": v1_task()}), "`jsonrpc` must be \"2.0\", found nothing"),
        (json!({"jsonrpc": "2.0", "id": 1}), "sets neither `result` nor `error`"),
        (
            json!({"jsonrpc": "2.0", "id": 1, "result": v1_task(), "error": {"code": -32603, "message": "m"}}),
            "sets both `result` and `error`",
        ),
        (json!({"jsonrpc": "2.0", "id": 1, "result": v1_task(), "extra": 1}), "unknown response key `extra`"),
        (
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32001, "message": "m"}}),
            "`id` is null, which only an error -32700 or -32600 may answer; the request's id is 1",
        ),
        (error(json!(-32000)), "`error.code` -32000 is reserved by JSON-RPC 2.0 and is neither a standard code nor in the A2A range -32099..=-32001"),
        (error(json!(-32100)), "`error.code` -32100 is reserved by JSON-RPC 2.0 and is neither a standard code nor in the A2A range -32099..=-32001"),
        (error(json!(-32768)), "`error.code` -32768 is reserved by JSON-RPC 2.0 and is neither a standard code nor in the A2A range -32099..=-32001"),
        (error(json!("-32001")), "`error.code` must be an integer, found \"-32001\""),
        (json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001}}), "`error.message` must be a string, found nothing"),
        (json!({"jsonrpc": "2.0", "id": 1, "error": "boom"}), "`error` must be a JSON object, found a string"),
        (
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "m", "data": {"@type": "x"}}}),
            "`error.data` must be an array of `@type` objects, found an object",
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "m", "data": [{"@type": "t"}, {"reason": "r"}]}}),
            "`error.data[1]` must be an object with a string `@type`",
        ),
        (
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "m", "detail": "d"}}),
            "unknown error key `error.detail`",
        ),
    ];
    for (response, expected) in cases {
        assert_eq!(
            errors_under(&check_response(&get, &response), "GetTask envelope -"),
            [expected],
            "{response}"
        );
    }
    let parse_error =
        json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "m"}});
    assert_eq!(check_response(&get, &parse_error), []);
    let with_data = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "m", "data": [{"@type": "t", "x": 1}]}});
    assert_eq!(check_response(&get, &with_data), []);
}

#[test]
fn a2a_wire_conformance_response_results_are_checked_as_the_method_result() {
    let get = request("GetTask", json!({"id": "tsk_1"}));
    let answer = json!({"jsonrpc": "2.0", "id": 1, "result": door_task()});
    let violations = check_response(&get, &answer);
    assert_eq!(keys(&violations), ["GetTask result lf.a2a.v1.Task"]);
    assert!(
        violations[0].errors.contains(
            &"`result.status.state` is \"completed\", not a value of TaskState".to_string()
        ),
        "{violations:#?}"
    );
    // An error answer carries no result to check.
    let refused =
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32001, "message": "no such task"}});
    assert_eq!(check_response(&get, &refused), []);
}

#[test]
fn a2a_wire_conformance_a_streaming_method_answers_with_events_not_a_result() {
    for method in ["SendStreamingMessage", "stream/watch"] {
        let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        let violations = check_response(&request(method, json!({})), &answer);
        assert_eq!(keys(&violations), [format!("{method} envelope -")]);
        assert_eq!(
            violations[0].errors,
            [format!(
                "`{method}` streams: its results travel as SSE events, not as one JSON `result`"
            )]
        );
        let refused = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "m"}});
        assert_eq!(check_response(&request(method, json!({})), &refused), []);
    }
}

#[test]
fn a2a_wire_conformance_stream_events() {
    let stream = request(
        "SendStreamingMessage",
        json!({"message": {"messageId": "m1", "role": "user", "parts": [{"text": "hi"}]}}),
    );
    let event = json!({"jsonrpc": "2.0", "id": 1, "result": {"statusUpdate": {
        "taskId": "tsk_1", "contextId": "ctx_1", "status": {"state": "TASK_STATE_WORKING"}
    }}});
    assert_eq!(check_stream_event(&stream, &event.to_string()), []);
    let error_event = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "m"}});
    assert_eq!(check_stream_event(&stream, &error_event.to_string()), []);

    let murmur_frame =
        r#"{"id":"tsk_1","context_id":"ctx_1","status":{"state":"working"},"final":false}"#;
    let violations = check_stream_event(&stream, murmur_frame);
    assert_eq!(
        keys(&violations),
        ["SendStreamingMessage event lf.a2a.v1.StreamResponse"]
    );
    let mut errors = violations[0].errors.clone();
    errors.sort();
    assert_eq!(
        errors,
        [
            "`id` is \"tsk_1\", not the request's id 1",
            "`jsonrpc` must be \"2.0\", found nothing",
            "sets neither `result` nor `error`",
            "unknown response key `context_id`",
            "unknown response key `final`",
            "unknown response key `status`",
        ]
    );

    let wrong_id = json!({"jsonrpc": "2.0", "id": 9, "result": {"task": v1_task()}});
    assert_eq!(
        check_stream_event(&stream, &wrong_id.to_string())[0].errors,
        ["`id` is 9, not the request's id 1"]
    );
    let not_a_stream_response = json!({"jsonrpc": "2.0", "id": 1, "result": v1_task()});
    assert!(
        check_stream_event(&stream, &not_a_stream_response.to_string())[0]
            .errors
            .contains(&"`result` sets no member of oneof `payload`".to_string())
    );

    let watch = request("stream/watch", json!({}));
    let ack = check_stream_event(
        &watch,
        r#"{"role":"observer","conversation_mode":"stateless"}"#,
    );
    assert_eq!(keys(&ack), ["stream/watch event lf.a2a.v1.StreamResponse"]);
    let garbage = check_stream_event(&watch, "not json");
    assert_eq!(
        keys(&garbage),
        ["stream/watch event lf.a2a.v1.StreamResponse"]
    );
    assert!(
        garbage[0].errors[0].starts_with("the event data is not JSON: "),
        "{garbage:#?}"
    );
}

#[test]
fn a2a_wire_conformance_never_panics() {
    let values = [
        json!(null),
        json!(true),
        json!(-1),
        json!(18446744073709551615u64),
        json!(1e308),
        json!("é∂ƒ"),
        json!([null, {}]),
        json!({"jsonrpc": "2.0", "id": [], "method": {}, "params": "x", "result": 1, "error": []}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {"message": {"parts": [{"raw": "é", "data": null}], "role": 99}}}),
        json!({"task": {"status": {"timestamp": "2026-10-08T12:00:00.é"}}}),
        json!({"error": {"code": 9223372036854775807i64, "data": [null, 1]}}),
    ];
    for value in &values {
        for message in [
            "lf.a2a.v1.AgentCard",
            "lf.a2a.v1.StreamResponse",
            "lf.a2a.v1.ListTasksResponse",
        ] {
            let _ = check_message(value, message);
        }
        let _ = check_request(value);
        for other in &values {
            let _ = check_response(value, other);
            let _ = check_stream_event(value, &other.to_string());
        }
        let _ = check_stream_event(value, "{\"jsonrpc\":");
        let _ = check_stream_event(value, "\u{0}");
    }
}
