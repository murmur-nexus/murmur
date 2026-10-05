//! Printing what a cancel found in flight.
//!
//! One printer for both commands that ask. `mur cancel` ends a task and `mur stop` ends a session,
//! and the two reach the same registries through different methods — but a detached shell command
//! named by one has to read identically to the same command named by the other, or an operator
//! reading two outputs cannot tell they are looking at the same process.

use serde_json::Value;

/// One line per thing the cancel found in flight, in the order the capsule listed them.
///
/// A detached shell command keeps its own lifecycle, so it is printed `running:`: whoever
/// cancelled or stopped now knows what is still out there under no capsule's supervision. A
/// delegation is printed `ended:`, because the cancelled task that started it ends its child once
/// the residue has named it.
///
/// A part is either the residue item itself — the shape `session/stop` returns — or an A2A
/// artifact part wrapping it as JSON text, which is how `tasks/cancel` carries it. Both spellings
/// describe the same item, so both are read here rather than at each caller.
pub(crate) fn print_residue(parts: &[Value]) {
    for item in parts.iter().filter_map(unwrap_item) {
        println!("{}", residue_line(&item));
    }
}

/// The line [`print_residue`] prints for one item.
fn residue_line(item: &Value) -> String {
    let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or("?");
    match field("kind") {
        "detached_shell" => format!(
            "running: {}  detached shell  {}",
            field("work_id"),
            item.get("command").and_then(Value::as_str).unwrap_or("")
        ),
        "delegation" => format!(
            "ended:   {}  delegation  {}@{}",
            field("delegation_id"),
            field("capsule"),
            field("version")
        ),
        other => format!("running: {other}"),
    }
}

/// The item inside an artifact part, or the part itself when it already is one.
fn unwrap_item(part: &Value) -> Option<Value> {
    match part.get("text").and_then(Value::as_str) {
        Some(text) => serde_json::from_str(text).ok(),
        None => part.is_object().then(|| part.clone()),
    }
}

/// Every residue part in an A2A task's `artifacts`, flattened.
///
/// `tasks/cancel` omits the key entirely when nothing is running, so an empty result here means
/// exactly that.
pub(crate) fn parts_from_artifacts(result: &Value) -> Vec<Value> {
    result
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|artifacts| {
            artifacts
                .iter()
                .filter(|artifact| artifact.get("name").and_then(Value::as_str) == Some("residue"))
                .filter_map(|artifact| artifact.get("parts").and_then(Value::as_array))
                .flatten()
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The two spellings the two methods use carry the same item, and both are unwrapped to it.
    #[test]
    fn both_part_shapes_unwrap_to_the_same_item() {
        let item = json!({"kind": "detached_shell", "work_id": "wrk_1", "command": "sleep 30"});
        let wrapped = json!({"text": item.to_string()});
        assert_eq!(unwrap_item(&wrapped).unwrap(), item);
        assert_eq!(unwrap_item(&item).unwrap(), item);
    }

    /// A detached shell command outlives the cancel and reads `running:`; a delegation is ended
    /// by the task the cancel stopped and reads `ended:`.
    #[test]
    fn a_delegation_reads_ended_and_a_detached_shell_reads_running() {
        let shell = json!({"kind": "detached_shell", "work_id": "wrk_1", "command": "sleep 30"});
        assert_eq!(
            residue_line(&shell),
            "running: wrk_1  detached shell  sleep 30"
        );
        let delegation = json!({"kind": "delegation", "delegation_id": "dlg_1",
                                "capsule": "worker", "version": "0.1.0"});
        assert_eq!(
            residue_line(&delegation),
            "ended:   dlg_1  delegation  worker@0.1.0"
        );
    }

    #[test]
    fn a_missing_artifacts_key_is_no_residue() {
        assert!(parts_from_artifacts(&json!({"id": "tsk_1"})).is_empty());
    }

    #[test]
    fn only_the_residue_artifact_contributes_parts() {
        let result = json!({
            "artifacts": [
                {"name": "other", "parts": [{"text": "{}"}]},
                {"name": "residue", "parts": [{"text": "{\"kind\":\"delegation\"}"}]},
            ]
        });
        let parts = parts_from_artifacts(&result);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "{\"kind\":\"delegation\"}");
    }
}
