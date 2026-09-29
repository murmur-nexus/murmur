//! Tests of the conformance checker itself. They sit outside `a2a_card_conformance.rs` because
//! the `murmur-cli` integration tests compile that file into every test binary that uses
//! `common`, and a `#[cfg(test)]` module inside it would run once per binary there.

use serde_json::{json, Value};

use crate::a2a_card_conformance::check_agent_card;

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
                        "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop"]
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
            "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop"],
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
