//! The required-field check: whether a tool call's input carries every name its tool's
//! `input_schema` lists in the top-level `required` keyword.
//!
//! Pure and I/O-free. Reading the schema off disk, deciding which tools are checked and
//! recording a refusal belong to `CapsuleStoreState::check_required_fields` and
//! `agent::CallGate::check_required_fields`; this module holds the rule itself.
//!
//! Presence is the whole rule. A name is present when it is a key of the input object, whatever
//! its value — `null` and `""` included — and nothing about types, `enum` values, nested objects
//! or `$ref` is judged. The check can only refuse: no shape of `required` makes a call run that
//! would not otherwise have run.

use serde_json::Value;

/// Why a schema's `required` cannot be read, so the check is skipped for that tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MalformedSchema {
    /// The schema's root is not a JSON object, so it has no top-level keywords at all.
    RootNotObject,
    /// `required` is present but is not an array whose every element is a string.
    RequiredNotStringArray,
}

impl MalformedSchema {
    /// The clause `W-RUN-004` completes "declares an input_schema whose …" with.
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            Self::RootNotObject => "root is not a JSON object",
            Self::RequiredNotStringArray => "`required` is not an array of strings",
        }
    }
}

/// What a schema's top-level `required` asks of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequiredDeclaration {
    /// No `required` keyword, or an empty one: every call passes.
    Nothing,
    /// The names every call must carry, in declared order with duplicates removed.
    Fields(Vec<String>),
    /// A `required` the check cannot read. Never partially enforced.
    Malformed(MalformedSchema),
}

/// Read the top-level `required` keyword of one tool's `input_schema`.
///
/// A list with any non-string element is [`RequiredDeclaration::Malformed`] as a whole rather
/// than enforced for its string elements: a partly read list would refuse calls on a reading of
/// the schema its author never wrote.
pub(crate) fn required_declaration(schema: &Value) -> RequiredDeclaration {
    let Some(root) = schema.as_object() else {
        return RequiredDeclaration::Malformed(MalformedSchema::RootNotObject);
    };
    let Some(required) = root.get("required") else {
        return RequiredDeclaration::Nothing;
    };
    let Some(entries) = required.as_array() else {
        return RequiredDeclaration::Malformed(MalformedSchema::RequiredNotStringArray);
    };
    let mut fields: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = entry.as_str() else {
            return RequiredDeclaration::Malformed(MalformedSchema::RequiredNotStringArray);
        };
        if !fields.iter().any(|seen| seen == name) {
            fields.push(name.to_string());
        }
    }
    if fields.is_empty() {
        RequiredDeclaration::Nothing
    } else {
        RequiredDeclaration::Fields(fields)
    }
}

/// The names in `required` that are not keys of `input`, in `required`'s order.
///
/// An `input` that is not an object has no keys, so it is missing every required name.
pub(crate) fn missing_fields(required: &[String], input: &Value) -> Vec<String> {
    let object = input.as_object();
    required
        .iter()
        .filter(|name| !object.is_some_and(|object| object.contains_key(name.as_str())))
        .cloned()
        .collect()
}

/// What the model is handed in place of a call missing a required field.
///
/// The first line names every missing field, so one retry can fix them all; the rest says that
/// nothing ran, lists the whole `required` set, and states that presence is all that was checked
/// — a call that passes may still fail inside the tool.
pub(crate) fn refusal_text(tool: &str, missing: &[String], required: &[String]) -> String {
    let noun = if missing.len() == 1 {
        "field"
    } else {
        "fields"
    };
    format!(
        "{tool}: missing required {noun} {missing}\n\n\
         This call did not run. {tool}'s input_schema requires: {required}. Call it again with \
         every one of them present. Only the presence of required fields is checked before a \
         call runs — not their types, allowed values or nested contents.",
        missing = quoted_list(missing),
        required = quoted_list(required),
    )
}

fn quoted_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn required_fields_root_not_an_object_is_malformed() {
        for schema in [json!([]), json!("object"), json!(null), json!(7)] {
            assert_eq!(
                required_declaration(&schema),
                RequiredDeclaration::Malformed(MalformedSchema::RootNotObject),
                "{schema}"
            );
        }
    }

    #[test]
    fn required_fields_absent_or_empty_required_is_nothing() {
        assert_eq!(
            required_declaration(&json!({"type": "object", "properties": {}})),
            RequiredDeclaration::Nothing
        );
        assert_eq!(
            required_declaration(&json!({"type": "object", "required": []})),
            RequiredDeclaration::Nothing
        );
    }

    #[test]
    fn required_fields_required_not_an_array_is_malformed() {
        for required in [json!("operation"), json!({"operation": true}), json!(null)] {
            assert_eq!(
                required_declaration(&json!({ "required": required })),
                RequiredDeclaration::Malformed(MalformedSchema::RequiredNotStringArray),
                "{required}"
            );
        }
    }

    #[test]
    fn required_fields_a_non_string_element_makes_the_whole_list_malformed() {
        assert_eq!(
            required_declaration(&json!({"required": ["operation", 3, "dest_path"]})),
            RequiredDeclaration::Malformed(MalformedSchema::RequiredNotStringArray)
        );
    }

    #[test]
    fn required_fields_keeps_declared_order_and_drops_duplicates() {
        assert_eq!(
            required_declaration(&json!({
                "required": ["operation", "dest_path", "operation", "content", "dest_path"]
            })),
            RequiredDeclaration::Fields(names(&["operation", "dest_path", "content"]))
        );
    }

    #[test]
    fn required_fields_presence_is_any_key_whatever_its_value() {
        let required = names(&["operation", "dest_path", "content"]);
        let input = json!({"operation": null, "dest_path": "", "content": {"nested": 1}});
        assert!(missing_fields(&required, &input).is_empty());
    }

    #[test]
    fn required_fields_missing_names_keep_required_order() {
        let required = names(&["operation", "dest_path", "content"]);
        let input = json!({"content": "hello", "extra": true});
        assert_eq!(
            missing_fields(&required, &input),
            names(&["operation", "dest_path"])
        );
    }

    #[test]
    fn required_fields_a_non_object_input_misses_every_name() {
        let required = names(&["operation", "dest_path"]);
        for input in [
            json!(null),
            json!("operation"),
            json!(["operation"]),
            json!(1),
        ] {
            assert_eq!(missing_fields(&required, &input), required, "{input}");
        }
    }

    #[test]
    fn required_fields_malformed_descriptions() {
        assert_eq!(
            MalformedSchema::RootNotObject.describe(),
            "root is not a JSON object"
        );
        assert_eq!(
            MalformedSchema::RequiredNotStringArray.describe(),
            "`required` is not an array of strings"
        );
    }

    #[test]
    fn required_fields_refusal_text_for_one_missing_field() {
        assert_eq!(
            refusal_text(
                "murmur-tool-editor",
                &names(&["operation"]),
                &names(&["operation", "dest_path"]),
            ),
            "murmur-tool-editor: missing required field \"operation\"\n\n\
             This call did not run. murmur-tool-editor's input_schema requires: \"operation\", \
             \"dest_path\". Call it again with every one of them present. Only the presence of \
             required fields is checked before a call runs — not their types, allowed values or \
             nested contents."
        );
    }

    #[test]
    fn required_fields_refusal_text_for_several_missing_fields() {
        assert_eq!(
            refusal_text(
                "murmur-tool-editor",
                &names(&["operation", "dest_path"]),
                &names(&["operation", "dest_path"]),
            ),
            "murmur-tool-editor: missing required fields \"operation\", \"dest_path\"\n\n\
             This call did not run. murmur-tool-editor's input_schema requires: \"operation\", \
             \"dest_path\". Call it again with every one of them present. Only the presence of \
             required fields is checked before a call runs — not their types, allowed values or \
             nested contents."
        );
    }
}
