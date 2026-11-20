//! The structural check a rewritten conversation passes before the runtime saves it.
//!
//! A rewrite is a message list that replaces another one: a compaction hook's `replace-context`,
//! or the tail a truncation keeps. [`check_rewrite`] reads the rewrite against the record it
//! replaces and reports every place it breaks one of three rules over the runtime's own message
//! record:
//!
//! 1. every `tool` message's `tool_call_id` names a `tool_call` block of an earlier `assistant`
//!    message ([`RecordCheck::OrphanedToolResult`]);
//! 2. every `tool_call` block's `input`, when present, is a JSON object
//!    ([`RecordCheck::UnparseableToolArguments`]);
//! 3. every `tool_call` block has a `tool` message answering it later in the rewrite, unless the
//!    call was in flight in the replaced record ([`RecordCheck::UnansweredToolCall`]).
//!
//! The check reads `role`, `tool_call_id`, `content`, and a `tool_call` block's `id` and `input`.
//! It does no I/O and repairs nothing: a caller refuses a rewrite with any fault whole and keeps
//! the record it had.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// The most faults a `record_check_failed` trace line lists. The line's `faults_total` counts
/// every one.
pub(crate) const MAX_REPORTED_FAULTS: usize = 20;

/// Which structural rule a rewrite broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordCheck {
    /// A `tool` message names no `tool_call` made by an earlier `assistant` message, or names
    /// none at all.
    OrphanedToolResult,
    /// A `tool_call` block's `input` is present and is not a JSON object.
    UnparseableToolArguments,
    /// A `tool_call` block has no `tool` message after it that answers it, and was not in flight
    /// in the record the rewrite replaces.
    UnansweredToolCall,
}

impl RecordCheck {
    /// The name a trace line and a CLI message use for the rule.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OrphanedToolResult => "orphaned_tool_result",
            Self::UnparseableToolArguments => "unparseable_tool_arguments",
            Self::UnansweredToolCall => "unanswered_tool_call",
        }
    }
}

/// One place a rewrite broke a rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordFault {
    pub check: RecordCheck,
    /// Position of the offending message in the rewrite that was checked, counting from 0.
    pub index: usize,
    /// The message's `id`, when it has one.
    pub message_id: Option<String>,
    /// The tool call the fault concerns, when the message names one as a non-empty string.
    pub tool_call_id: Option<String>,
    /// One sentence saying what is wrong. It never quotes argument or result text.
    pub detail: String,
}

/// The `tool_call` ids of `previous` that no `tool` message in `previous` answers.
///
/// This is what "in flight" means: the record stores no flag for it, and the only way the runtime
/// leaves a call without a result is a dispatch that had not returned when the record was read —
/// the open turn, or a session killed mid-call whose record a later launch loaded. A rewrite may
/// keep such a call without its result.
pub(crate) fn in_flight_calls(previous: &[Value]) -> HashSet<String> {
    let answered: HashSet<&str> = previous
        .iter()
        .filter(|message| role(message) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
        .collect();
    previous
        .iter()
        .filter(|message| role(message) == Some("assistant"))
        .flat_map(tool_call_blocks)
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty() && !answered.contains(id))
        .map(str::to_string)
        .collect()
}

/// Every fault `rewrite` has, in record order, read against `previous`, the record it replaces.
/// Faults of one message come in its block order, a block's argument fault before its
/// unanswered one. Empty means the rewrite may be saved.
pub(crate) fn check_rewrite(rewrite: &[Value], previous: &[Value]) -> Vec<RecordFault> {
    let in_flight = in_flight_calls(previous);
    // How many `tool` messages not yet passed answer each id. At an `assistant` message every
    // `tool` message before it has been passed, so a positive count is an answer after it.
    let mut answers_ahead: HashMap<&str, usize> = HashMap::new();
    for message in rewrite.iter().filter(|m| role(m) == Some("tool")) {
        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
            *answers_ahead.entry(id).or_default() += 1;
        }
    }
    // Where each call id is first made, for a result that comes before its call.
    let mut made_at: HashMap<&str, usize> = HashMap::new();
    for (index, message) in rewrite.iter().enumerate().rev() {
        if role(message) == Some("assistant") {
            for id in tool_call_blocks(message).filter_map(|b| b.get("id").and_then(Value::as_str))
            {
                made_at.insert(id, index);
            }
        }
    }

    let mut faults = Vec::new();
    let mut made_before: HashSet<&str> = HashSet::new();
    for (index, message) in rewrite.iter().enumerate() {
        let message_id = crate::agent::message_id(message).map(str::to_string);
        let fault = |check, tool_call_id: Option<&str>, detail: String| RecordFault {
            check,
            index,
            message_id: message_id.clone(),
            tool_call_id: tool_call_id.map(str::to_string),
            detail,
        };
        match role(message) {
            Some("tool") => {
                let raw = message.get("tool_call_id");
                let named = raw.and_then(Value::as_str).filter(|id| !id.is_empty());
                if let Some(id) = named {
                    *answers_ahead.entry(id).or_default() -= 1;
                }
                let detail = match (raw, named) {
                    (_, Some(id)) if made_before.contains(id) => continue,
                    (_, Some(id)) => match made_at.get(id) {
                        Some(later) => format!(
                            "the tool result comes before the tool call it answers, which is made \
                             at index {later}"
                        ),
                        None => "no earlier assistant message makes the tool call this result \
                                 answers"
                            .to_string(),
                    },
                    (None | Some(Value::Null), None) => {
                        "the tool result carries no tool_call_id".to_string()
                    }
                    (Some(Value::String(_)), None) => {
                        "the tool result's tool_call_id is empty".to_string()
                    }
                    (Some(_), None) => "the tool result's tool_call_id is not a string".to_string(),
                };
                faults.push(fault(RecordCheck::OrphanedToolResult, named, detail));
            }
            Some("assistant") => {
                for block in tool_call_blocks(message) {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty());
                    if let Some(detail) = block.get("input").and_then(arguments_fault) {
                        faults.push(fault(RecordCheck::UnparseableToolArguments, id, detail));
                    }
                    let unanswered = match id {
                        None if block.get("id").and_then(Value::as_str) == Some("") => Some(
                            "the tool call's id is empty, so no tool result can answer it"
                                .to_string(),
                        ),
                        None => {
                            Some("the tool call has no id, so no tool result can answer it".into())
                        }
                        Some(id)
                            if answers_ahead.get(id).copied().unwrap_or(0) == 0
                                && !in_flight.contains(id) =>
                        {
                            Some(
                                "no tool result after this message answers the tool call, and the \
                                 call was not in flight in the record this rewrite replaces"
                                    .to_string(),
                            )
                        }
                        Some(_) => None,
                    };
                    if let Some(detail) = unanswered {
                        faults.push(fault(RecordCheck::UnansweredToolCall, id, detail));
                    }
                    if let Some(id) = id {
                        made_before.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    faults
}

/// The detail for a tool result the caller could not read as one, which it represents in the
/// rewrite as a `tool` message naming no call. `reason` says why, without quoting the content.
pub(crate) fn unreadable_tool_result_detail(reason: &str) -> String {
    format!("its content could not be read as a tool result: {reason}")
}

/// Why `input` is not a JSON object, or `None` when it is.
fn arguments_fault(input: &Value) -> Option<String> {
    let kind = match input {
        Value::Object(_) => return None,
        Value::String(text) => {
            return Some(match serde_json::from_str::<Value>(text) {
                Ok(parsed) => format!(
                    "the arguments are stored as text rather than as an object; the text parses \
                     as JSON ({}) but was never parsed into the record",
                    json_kind(&parsed)
                ),
                Err(error) => format!(
                    "the arguments are stored as text rather than as an object, and the text does \
                     not parse as JSON (it fails at line {}, column {})",
                    error.line(),
                    error.column()
                ),
            })
        }
        other => json_kind(other),
    };
    Some(format!("the arguments are {kind}, not an object"))
}

/// The JSON kind of `value`, with its article.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Object(_) => "an object",
        Value::Array(_) => "an array",
        Value::String(_) => "a string",
        Value::Number(_) => "a number",
        Value::Bool(_) => "a boolean",
        Value::Null => "null",
    }
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

/// The `tool_call` blocks of a message's block-array `content`.
fn tool_call_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_call"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": [{"type": "text", "text": text}]})
    }

    fn call(id: &str) -> Value {
        call_with_input(id, json!({"command": "ls"}))
    }

    fn call_with_input(id: &str, input: Value) -> Value {
        json!({
            "role": "assistant",
            "content": [{"type": "tool_call", "id": id, "name": "bash", "input": input}],
        })
    }

    fn result(id: &str) -> Value {
        json!({"role": "tool", "tool_call_id": id, "is_error": false, "content": "ok"})
    }

    fn checks(faults: &[RecordFault]) -> Vec<(RecordCheck, usize, Option<&str>)> {
        faults
            .iter()
            .map(|f| (f.check, f.index, f.tool_call_id.as_deref()))
            .collect()
    }

    #[test]
    fn a_clean_record_has_no_fault() {
        let record = [
            user("go"),
            call("c1"),
            result("c1"),
            json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}),
        ];
        assert!(check_rewrite(&record, &record).is_empty());
        assert!(check_rewrite(&[], &[]).is_empty());
    }

    #[test]
    fn a_result_naming_an_unknown_call_is_orphaned() {
        let faults = check_rewrite(&[user("go"), result("c9")], &[]);
        assert_eq!(
            checks(&faults),
            [(RecordCheck::OrphanedToolResult, 1, Some("c9"))]
        );
        assert_eq!(
            faults[0].detail,
            "no earlier assistant message makes the tool call this result answers"
        );
    }

    /// A set comparison of issued against answered ids passes this record; the order rule does
    /// not. The call is unanswered too, since its only result comes before it.
    #[test]
    fn a_result_before_its_call_is_orphaned() {
        let faults = check_rewrite(&[result("c1"), call("c1")], &[]);
        assert_eq!(
            checks(&faults),
            [
                (RecordCheck::OrphanedToolResult, 0, Some("c1")),
                (RecordCheck::UnansweredToolCall, 1, Some("c1")),
            ]
        );
        assert!(faults[0].detail.contains("made at index 1"), "{faults:?}");
    }

    #[test]
    fn two_results_for_one_call_pass_when_both_follow_it() {
        assert!(check_rewrite(&[call("c1"), result("c1"), result("c1")], &[]).is_empty());
    }

    #[test]
    fn a_result_with_no_or_an_empty_tool_call_id_is_orphaned() {
        let none = json!({"role": "tool", "content": "ok"});
        let empty = json!({"role": "tool", "tool_call_id": "", "content": "ok"});
        let number = json!({"role": "tool", "tool_call_id": 7, "content": "ok"});
        let faults = check_rewrite(&[call("c1"), result("c1"), none, empty, number], &[]);
        assert_eq!(
            checks(&faults),
            [
                (RecordCheck::OrphanedToolResult, 2, None),
                (RecordCheck::OrphanedToolResult, 3, None),
                (RecordCheck::OrphanedToolResult, 4, None),
            ]
        );
        assert_eq!(faults[0].detail, "the tool result carries no tool_call_id");
        assert_eq!(faults[1].detail, "the tool result's tool_call_id is empty");
        assert_eq!(
            faults[2].detail,
            "the tool result's tool_call_id is not a string"
        );
    }

    #[test]
    fn an_unparseable_string_input_is_a_fault_whose_detail_does_not_echo_it() {
        let text = "{\"command\": \"echo hel";
        let faults = check_rewrite(&[call_with_input("c1", json!(text)), result("c1")], &[]);
        assert_eq!(
            checks(&faults),
            [(RecordCheck::UnparseableToolArguments, 0, Some("c1"))]
        );
        let detail = &faults[0].detail;
        assert_eq!(
            detail,
            "the arguments are stored as text rather than as an object, and the text does not \
             parse as JSON (it fails at line 1, column 21)"
        );
        assert!(!detail.contains("echo"), "{detail}");
    }

    #[test]
    fn a_string_input_that_would_parse_is_still_a_fault() {
        let faults = check_rewrite(
            &[
                call_with_input("c1", json!("{\"command\": \"ls\"}")),
                result("c1"),
            ],
            &[],
        );
        assert_eq!(
            checks(&faults),
            [(RecordCheck::UnparseableToolArguments, 0, Some("c1"))]
        );
        assert_eq!(
            faults[0].detail,
            "the arguments are stored as text rather than as an object; the text parses as JSON \
             (an object) but was never parsed into the record"
        );
    }

    #[test]
    fn an_array_number_boolean_or_null_input_names_its_kind() {
        for (input, kind) in [
            (json!(["ls"]), "an array"),
            (json!(3), "a number"),
            (json!(true), "a boolean"),
            (Value::Null, "null"),
        ] {
            let faults = check_rewrite(&[call_with_input("c1", input), result("c1")], &[]);
            assert_eq!(
                checks(&faults),
                [(RecordCheck::UnparseableToolArguments, 0, Some("c1"))]
            );
            assert_eq!(
                faults[0].detail,
                format!("the arguments are {kind}, not an object")
            );
        }
    }

    #[test]
    fn an_absent_input_is_no_fault() {
        let absent = json!({
            "role": "assistant",
            "content": [{"type": "tool_call", "id": "c1", "name": "bash"}],
        });
        assert!(check_rewrite(&[absent, result("c1")], &[]).is_empty());
    }

    #[test]
    fn a_call_with_no_later_result_is_unanswered() {
        let faults = check_rewrite(&[user("go"), call("c1")], &[]);
        assert_eq!(
            checks(&faults),
            [(RecordCheck::UnansweredToolCall, 1, Some("c1"))]
        );
        assert_eq!(
            faults[0].detail,
            "no tool result after this message answers the tool call, and the call was not in \
             flight in the record this rewrite replaces"
        );
    }

    /// The call `previous` left unanswered may stay unanswered; one `previous` answered may not.
    #[test]
    fn only_a_call_in_flight_in_the_previous_record_may_stay_unanswered() {
        let in_flight = [user("go"), call("c1")];
        assert!(check_rewrite(&[call("c1")], &in_flight).is_empty());

        let answered = [user("go"), call("c1"), result("c1")];
        assert_eq!(
            checks(&check_rewrite(&[call("c1")], &answered)),
            [(RecordCheck::UnansweredToolCall, 0, Some("c1"))]
        );
    }

    #[test]
    fn a_call_with_no_or_an_empty_id_is_unanswered_even_if_in_flight() {
        let no_id = json!({
            "role": "assistant",
            "content": [{"type": "tool_call", "name": "bash", "input": {}}],
        });
        let empty_id = call("");
        let previous = [no_id.clone(), empty_id.clone()];
        let faults = check_rewrite(&[no_id, empty_id], &previous);
        assert_eq!(
            checks(&faults),
            [
                (RecordCheck::UnansweredToolCall, 0, None),
                (RecordCheck::UnansweredToolCall, 1, None),
            ]
        );
        assert_eq!(
            faults[0].detail,
            "the tool call has no id, so no tool result can answer it"
        );
        assert_eq!(
            faults[1].detail,
            "the tool call's id is empty, so no tool result can answer it"
        );
        assert!(in_flight_calls(&previous).is_empty());
    }

    #[test]
    fn several_faults_come_in_record_order_with_their_message_ids() {
        let mut orphan = result("c9");
        orphan["id"] = json!("msg_2");
        let mut bad = call_with_input("c2", json!("nope"));
        bad["id"] = json!("msg_3");
        let rewrite = [user("go"), call("c1"), orphan, bad];
        let faults = check_rewrite(&rewrite, &[]);
        assert_eq!(
            checks(&faults),
            [
                (RecordCheck::UnansweredToolCall, 1, Some("c1")),
                (RecordCheck::OrphanedToolResult, 2, Some("c9")),
                (RecordCheck::UnparseableToolArguments, 3, Some("c2")),
                (RecordCheck::UnansweredToolCall, 3, Some("c2")),
            ]
        );
        assert_eq!(faults[0].message_id, None);
        assert_eq!(faults[1].message_id.as_deref(), Some("msg_2"));
        assert_eq!(faults[2].message_id.as_deref(), Some("msg_3"));
    }

    /// The cases the set comparison this check replaced tested: a call with no answer and an
    /// answer with no call both fail; an empty list, a plain message, a paired call and result,
    /// and a pair among other messages pass.
    #[test]
    fn unpaired_calls_and_results_fail_in_both_directions() {
        assert!(!check_rewrite(&[call("c1")], &[]).is_empty());
        assert!(!check_rewrite(&[result("c1")], &[]).is_empty());
        assert!(!check_rewrite(&[call("c1"), result("c2")], &[]).is_empty());
        assert!(check_rewrite(&[user("hi")], &[]).is_empty());
        assert!(check_rewrite(&[call("c1"), result("c1")], &[]).is_empty());
        assert!(check_rewrite(
            &[
                user("go"),
                call("c1"),
                result("c1"),
                call("c2"),
                result("c2"),
            ],
            &[],
        )
        .is_empty());
    }

    #[test]
    fn in_flight_calls_are_the_previous_record_s_unanswered_ids() {
        let previous = [call("c1"), result("c1"), call("c2")];
        assert_eq!(
            in_flight_calls(&previous),
            HashSet::from(["c2".to_string()])
        );
    }
}
