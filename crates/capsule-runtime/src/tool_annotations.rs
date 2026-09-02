//! What a tool artifact declares about its own input: which values are filesystem destinations,
//! and which subtrees are payload it merely stores.
//!
//! Two JSON Schema `format` values, readable on any property of a tool's `input_schema`:
//!
//! | Annotation | Effect on the write-intent analyser |
//! |---|---|
//! | [`FORMAT_DESTINATION`] on a string property | The value at that location is checked against the capsule's `read_only` rules, wherever in the input it sits |
//! | [`FORMAT_OPAQUE`] on an object or array property | [`crate::protected_paths`]'s key-name heuristic does not descend into that subtree |
//!
//! `format` is JSON Schema's own annotation keyword, whose value set is explicitly extensible: an
//! unknown value is ignored by a validator rather than rejected, and it survives the trip through
//! `input-schema: option<string>` and `runtime::yaml_to_json_string` unchanged.
//!
//! # Why the lowered form is only a set of locations
//!
//! An artifact must never get a say in its own containment. What is lowered here is therefore a
//! set of [`InputLocation`]s and nothing else — no boolean, no path, no allow list, no exemption
//! field — so there is no representable annotation whose meaning is "permit". A destination adds
//! a location to check and removes none; an opaque container removes a *guess about a container's
//! interior* and never a check on a value, which is why it is ignored on a string property. The
//! refusal itself stays with `ProtectedPaths::covering_rule`, which reads only the operator's
//! declared `read_only` entries.
//!
//! Inside a subtree a tool declared opaque the key-name heuristic does not run, so a destination
//! that tool did not also declare is not seen there. The declaration is taken at its word: the
//! tool author is not this layer's adversary — see the "what it cannot see" table in
//! [`crate::protected_paths`].

use std::collections::BTreeMap;
use std::path::Path;

use murmur_artifact::PACKED_MANIFEST_ENTRY;
use serde_json::Value;

use crate::protected_paths::{matches_key, TOOL_DESTINATION_KEYS, TOOL_PATH_KEYS};

/// `format` value marking a string property whose value is a filesystem destination.
pub(crate) const FORMAT_DESTINATION: &str = "murmur-destination";

/// `format` value marking an object or array property whose interior is stored payload rather
/// than filesystem intent.
pub(crate) const FORMAT_OPAQUE: &str = "murmur-opaque";

/// How deep the schema walk descends before it stops.
///
/// A schema arrives from an artifact, so it is untrusted input: the bound is what keeps a
/// pathological nesting — or a cycle a future `$ref` resolution could build — from exhausting the
/// stack. A schema deeper than this keeps its shallower annotations and loses the deeper ones,
/// which leaves the tool on the key-name heuristic there.
const MAX_SCHEMA_DEPTH: usize = 32;

/// One step from a tool input's root towards a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum LocationStep {
    /// Into the named member of an object.
    Key(String),
    /// Into every element of an array. An annotation names the array's element *schema*, so it
    /// covers every element rather than an index.
    Element,
}

/// A location inside a tool's input JSON: a sequence of object-key and array-element steps, and
/// nothing else.
///
/// This is the whole lowered form of an annotation. It cannot express a verdict, so no annotation
/// can carry one.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct InputLocation {
    steps: Vec<LocationStep>,
}

impl InputLocation {
    fn child(&self, step: LocationStep) -> Self {
        let mut steps = self.steps.clone();
        steps.push(step);
        Self { steps }
    }

    /// How the location is named in a refusal and in the trace: object keys joined with `.`, an
    /// array step written `[]` after the key it belongs to — `edits[].path`.
    ///
    /// The root itself renders `<input>`, which is what an annotation on the schema's top level
    /// names.
    pub(crate) fn render(&self) -> String {
        if self.steps.is_empty() {
            return "<input>".to_string();
        }
        let mut out = String::new();
        for step in &self.steps {
            match step {
                LocationStep::Key(key) => {
                    if !out.is_empty() {
                        out.push('.');
                    }
                    out.push_str(key);
                }
                LocationStep::Element => out.push_str("[]"),
            }
        }
        out
    }

    /// Every string value this location names in one concrete input, in document order.
    ///
    /// A location that names a non-string — because the model sent an object where the schema
    /// declared a string, or because the annotation sat on a container — yields nothing, so a
    /// mismatched value is simply not a candidate rather than an error.
    pub(crate) fn resolve<'a>(&self, input: &'a Value) -> Vec<&'a str> {
        let mut out = Vec::new();
        collect_at(input, &self.steps, &mut out);
        out
    }
}

fn collect_at<'a>(value: &'a Value, steps: &[LocationStep], out: &mut Vec<&'a str>) {
    let Some((step, rest)) = steps.split_first() else {
        if let Some(text) = value.as_str() {
            out.push(text);
        }
        return;
    };
    match (step, value) {
        (LocationStep::Key(key), Value::Object(map)) => {
            if let Some(child) = map.get(key) {
                collect_at(child, rest, out);
            }
        }
        (LocationStep::Element, Value::Array(items)) => {
            for item in items {
                collect_at(item, rest, out);
            }
        }
        _ => {}
    }
}

/// One tool's lowered annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolAnnotations {
    destinations: Vec<InputLocation>,
    opaque: Vec<InputLocation>,
}

impl ToolAnnotations {
    /// Lower the annotations in one `input_schema`, given as the JSON text the manifest carries.
    ///
    /// A schema that is not parsable JSON, or that is not a JSON object, lowers to nothing — the
    /// tool keeps the key-name heuristic, which is the conservative direction.
    pub(crate) fn from_schema_json(schema: &str) -> Self {
        LoweredSchema::from_schema_json(schema).annotations
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.destinations.is_empty() && self.opaque.is_empty()
    }

    /// The locations the tool declared as filesystem destinations.
    pub(crate) fn destinations(&self) -> &[InputLocation] {
        &self.destinations
    }

    /// Whether the tool declared the container at `steps` to be stored payload.
    pub(crate) fn is_opaque(&self, steps: &[LocationStep]) -> bool {
        self.opaque.iter().any(|location| location.steps == steps)
    }
}

/// One tool's `input_schema` read for everything this module decides from it.
struct LoweredSchema {
    annotations: ToolAnnotations,
    /// Every property name the walk saw, in walk order — the input to [`unannotated_path_property`].
    property_names: Vec<String>,
}

impl LoweredSchema {
    fn from_schema_json(schema: &str) -> Self {
        let mut lowered = Self {
            annotations: ToolAnnotations::default(),
            property_names: Vec::new(),
        };
        if let Ok(value) = serde_json::from_str::<Value>(schema) {
            lowered.walk(&value, &InputLocation::default(), 0);
        }
        lowered
    }

    /// Walk one schema node.
    ///
    /// Reads `properties` (an object-key step), `items` and `prefixItems` (an array-element step)
    /// and the `allOf`/`anyOf`/`oneOf` branches (which describe the *same* location, so they do
    /// not step). `$ref` is not resolved: an annotation behind one is not seen, and the tool keeps
    /// the key-name heuristic there. `additionalProperties` and `patternProperties` are not walked
    /// because neither names a location that can be rendered or resolved.
    fn walk(&mut self, node: &Value, at: &InputLocation, depth: usize) {
        if depth > MAX_SCHEMA_DEPTH {
            return;
        }
        let Some(map) = node.as_object() else { return };

        match map.get("format").and_then(Value::as_str) {
            Some(FORMAT_DESTINATION) => push_unique(&mut self.annotations.destinations, at),
            Some(FORMAT_OPAQUE) => push_unique(&mut self.annotations.opaque, at),
            _ => {}
        }

        if let Some(properties) = map.get("properties").and_then(Value::as_object) {
            for (key, child) in properties {
                self.property_names.push(key.clone());
                self.walk(child, &at.child(LocationStep::Key(key.clone())), depth + 1);
            }
        }
        if let Some(items) = map.get("items") {
            let element = at.child(LocationStep::Element);
            match items {
                // The pre-2020-12 tuple form: an array of schemas, one per position. Every
                // position is the same location here, because a location names a shape rather
                // than an index.
                Value::Array(entries) => {
                    for entry in entries {
                        self.walk(entry, &element, depth + 1);
                    }
                }
                other => self.walk(other, &element, depth + 1),
            }
        }
        if let Some(Value::Array(entries)) = map.get("prefixItems") {
            let element = at.child(LocationStep::Element);
            for entry in entries {
                self.walk(entry, &element, depth + 1);
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = map.get(keyword) {
                for branch in branches {
                    self.walk(branch, at, depth + 1);
                }
            }
        }
    }
}

fn push_unique(locations: &mut Vec<InputLocation>, location: &InputLocation) {
    if !locations.contains(location) {
        locations.push(location.clone());
    }
}

/// The lowered annotations of every tool staged into one session's workdir, keyed by tool name.
///
/// Holds an entry only for a tool that annotated something. A tool absent from it — one with no
/// schema, an unparsable schema, an unreadable manifest, or one pulled at runtime by
/// `manage.pull()` after staging — is judged by the key-name heuristic alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolAnnotationMap {
    tools: BTreeMap<String, ToolAnnotations>,
}

impl ToolAnnotationMap {
    /// Lower the `input_schema` of every `<workdir>/tools/<name>/murmur.yaml`.
    ///
    /// Read from the staged manifests rather than from anything the session produces: the schema
    /// is fixed before the session starts, and the model chooses values at call time but never
    /// annotations. A directory that cannot be read contributes nothing.
    pub(crate) fn from_workdir(workdir: &Path) -> Self {
        let mut tools = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(workdir.join("tools")) else {
            return Self { tools };
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(schema) = read_staged_schema(&path.join(PACKED_MANIFEST_ENTRY)) else {
                continue;
            };
            let annotations = ToolAnnotations::from_schema_json(&schema);
            if !annotations.is_empty() {
                tools.insert(name.to_string(), annotations);
            }
        }
        Self { tools }
    }

    /// The annotations `tool_name` declared, or the empty set when it declared none.
    pub(crate) fn for_tool(&self, tool_name: &str) -> &ToolAnnotations {
        static NONE: ToolAnnotations = ToolAnnotations {
            destinations: Vec::new(),
            opaque: Vec::new(),
        };
        self.tools.get(tool_name).unwrap_or(&NONE)
    }

    /// A map built straight from schema text, for cases that assert on the analyser rather than
    /// on the staging read.
    #[cfg(test)]
    pub(crate) fn from_schemas(entries: &[(&str, &str)]) -> Self {
        Self {
            tools: entries
                .iter()
                .map(|(name, schema)| (name.to_string(), ToolAnnotations::from_schema_json(schema)))
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// The `input_schema` one tool manifest declares, under either spelling and in either shape.
///
/// The single reader for that field: the analyser must judge the same schema
/// [`crate::agent::inventory`] offers the model, so a manifest whose schema one of them reads and
/// the other does not is a hole rather than a difference of opinion.
pub(crate) fn declared_input_schema(manifest: &serde_yaml::Value) -> Option<Value> {
    let schema = manifest
        .get("input_schema")
        .or_else(|| manifest.get("input"))?;
    match schema {
        // The common shape: a YAML string holding JSON.
        serde_yaml::Value::String(text) => serde_json::from_str(text).ok(),
        // A YAML mapping written out directly still describes the same schema.
        other => serde_json::to_value(other).ok(),
    }
}

/// The `input_schema` of one staged tool manifest, as JSON text.
pub(crate) fn schema_from_manifest_yaml(manifest_yaml: &str) -> Option<String> {
    let manifest: serde_yaml::Value = serde_yaml::from_str(manifest_yaml).ok()?;
    Some(declared_input_schema(&manifest)?.to_string())
}

fn read_staged_schema(manifest_path: &Path) -> Option<String> {
    schema_from_manifest_yaml(&std::fs::read_to_string(manifest_path).ok()?)
}

/// The property name that makes an unannotated schema path-shaped, and so makes the tool's calls
/// judged by key name — the decision behind `W-SEC-018`.
///
/// `None` when the schema annotates anything, when no property name is path-shaped or
/// destination-shaped, or when the schema cannot be read: each of those is a tool the warning has
/// nothing to say about.
pub(crate) fn unannotated_path_property(schema: &str) -> Option<String> {
    let lowered = LoweredSchema::from_schema_json(schema);
    if !lowered.annotations.is_empty() {
        return None;
    }
    lowered
        .property_names
        .into_iter()
        .find(|name| matches_key(TOOL_PATH_KEYS, name) || matches_key(TOOL_DESTINATION_KEYS, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(steps: &[LocationStep]) -> InputLocation {
        InputLocation {
            steps: steps.to_vec(),
        }
    }

    fn key(name: &str) -> LocationStep {
        LocationStep::Key(name.to_string())
    }

    /// A location renders the way a refusal has to name it: `.` between object keys, `[]` for an
    /// array step, and `<input>` for the root.
    #[test]
    fn a_location_renders_with_dots_and_array_steps() {
        assert_eq!(location(&[]).render(), "<input>");
        assert_eq!(location(&[key("sink")]).render(), "sink");
        assert_eq!(
            location(&[key("edits"), LocationStep::Element, key("path")]).render(),
            "edits[].path"
        );
        assert_eq!(
            location(&[key("a"), LocationStep::Element, LocationStep::Element]).render(),
            "a[][]"
        );
    }

    /// A location names a shape, so an array step resolves to every element, and a value of the
    /// wrong kind resolves to nothing.
    #[test]
    fn a_location_resolves_every_string_it_names() {
        let input = serde_json::json!({
            "edits": [{"path": "a.py"}, {"path": "b.py"}, {"path": {"nested": "c.py"}}, {}],
            "sink": 7
        });
        let at = location(&[key("edits"), LocationStep::Element, key("path")]);
        assert_eq!(at.resolve(&input), vec!["a.py", "b.py"]);
        assert!(location(&[key("sink")]).resolve(&input).is_empty());
        assert!(location(&[key("absent")]).resolve(&input).is_empty());
    }

    /// The walk reads `properties`, `items`, `prefixItems` and the composition branches, and each
    /// annotation lowers to the location it sits on.
    #[test]
    fn the_walk_lowers_each_annotation_to_its_location() {
        let annotations = ToolAnnotations::from_schema_json(
            r#"{
              "type": "object",
              "properties": {
                "edits": {
                  "type": "array",
                  "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "format": "murmur-destination"}}
                  }
                },
                "note": {"type": "object", "format": "murmur-opaque"},
                "pair": {"type": "array", "prefixItems": [
                  {"type": "string", "format": "murmur-destination"}
                ]},
                "either": {"anyOf": [{"type": "object", "format": "murmur-opaque"}]}
              }
            }"#,
        );
        assert_eq!(
            annotations.destinations(),
            &[
                location(&[key("edits"), LocationStep::Element, key("path")]),
                location(&[key("pair"), LocationStep::Element]),
            ]
        );
        assert!(annotations.is_opaque(&[key("note")]));
        assert!(
            annotations.is_opaque(&[key("either")]),
            "a composition branch describes the same location it sits under"
        );
        assert!(!annotations.is_opaque(&[key("edits")]));
    }

    /// An unrecognized `format` value is left to whatever else reads the schema, and a `$ref` is
    /// not resolved — both leave the tool on the key-name heuristic.
    #[test]
    fn an_unknown_format_and_a_ref_lower_to_nothing() {
        assert!(ToolAnnotations::from_schema_json(
            r#"{"properties":{"path":{"type":"string","format":"uri-reference"}}}"#
        )
        .is_empty());
        assert!(ToolAnnotations::from_schema_json(
            r##"{"properties":{"path":{"$ref":"#/$defs/dest"}},"$defs":{"dest":{"format":"murmur-destination"}}}"##
        )
        .is_empty());
    }

    /// A schema that is not JSON, or not an object, lowers to nothing rather than failing.
    #[test]
    fn an_unparsable_schema_lowers_to_nothing() {
        for schema in ["", "not json at all", "[1,2,3]", "\"a string\""] {
            assert!(
                ToolAnnotations::from_schema_json(schema).is_empty(),
                "{schema}"
            );
        }
    }

    /// The depth bound holds: a schema nested past it keeps its shallow annotations, loses its
    /// deep ones, and does not exhaust the stack.
    #[test]
    fn the_walk_stops_at_the_depth_bound() {
        let mut schema = r#"{"type":"string","format":"murmur-destination"}"#.to_string();
        for _ in 0..(MAX_SCHEMA_DEPTH + 10) {
            schema = format!(r#"{{"type":"object","properties":{{"n":{schema}}}}}"#);
        }
        assert!(ToolAnnotations::from_schema_json(&schema).is_empty());
    }

    /// The `W-SEC-018` decision: a path-shaped or destination-shaped property with no annotation
    /// anywhere in the schema, and nothing else.
    #[test]
    fn the_warning_fires_only_for_an_unannotated_path_shaped_schema() {
        let path_shaped = r#"{"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}}}"#;
        assert_eq!(
            unannotated_path_property(path_shaped).as_deref(),
            Some("file_path")
        );
        assert_eq!(
            unannotated_path_property(
                r#"{"type":"object","properties":{"output_path":{"type":"string"}}}"#
            )
            .as_deref(),
            Some("output_path"),
            "a destination-shaped name is judged by key name too"
        );
        assert_eq!(
            unannotated_path_property(
                r#"{"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"filename":{"type":"string"}}}}}}"#
            )
            .as_deref(),
            Some("filename"),
            "a nested property is what the heuristic reads, so it is what the warning reads"
        );

        assert_eq!(
            unannotated_path_property(
                r#"{"type":"object","properties":{"file_path":{"type":"string","format":"murmur-destination"}}}"#
            ),
            None,
            "an annotated schema is silent"
        );
        assert_eq!(
            unannotated_path_property(
                r#"{"type":"object","properties":{"note":{"type":"object","format":"murmur-opaque"},"file":{"type":"string"}}}"#
            ),
            None,
            "any annotation makes the tool's schema a statement rather than a guess"
        );
        assert_eq!(
            unannotated_path_property(
                r#"{"type":"object","properties":{"query":{"type":"string"}}}"#
            ),
            None
        );
        assert_eq!(unannotated_path_property(""), None);
    }

    /// The `input_schema` a staged manifest carries is read whether it is a YAML string holding
    /// JSON or a YAML mapping, and a manifest without one contributes nothing.
    #[test]
    fn a_staged_manifest_yields_its_schema_in_either_spelling() {
        let from_string = schema_from_manifest_yaml(
            "name: noter\ninput_schema: '{\"properties\":{\"note\":{\"format\":\"murmur-opaque\"}}}'\n",
        )
        .expect("a JSON string schema is read");
        assert!(ToolAnnotations::from_schema_json(&from_string).is_opaque(&[key("note")]));

        let from_mapping = schema_from_manifest_yaml(
            "name: noter\ninput_schema:\n  properties:\n    note:\n      format: murmur-opaque\n",
        )
        .expect("a YAML mapping schema is read");
        assert!(ToolAnnotations::from_schema_json(&from_mapping).is_opaque(&[key("note")]));

        assert!(schema_from_manifest_yaml("name: noter\nversion: 0.1.0\n").is_none());
    }

    /// The staging read: one directory per tool, and only a tool that annotated something takes
    /// an entry.
    #[test]
    fn the_workdir_read_keys_annotations_by_tool_name() {
        let workdir = tempfile::tempdir().unwrap();
        let write = |name: &str, manifest: &str| {
            let dir = workdir.path().join("tools").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(PACKED_MANIFEST_ENTRY), manifest).unwrap();
        };
        write(
            "noter",
            "name: noter\ninput_schema: '{\"properties\":{\"note\":{\"format\":\"murmur-opaque\"}}}'\n",
        );
        write(
            "writer",
            "name: writer\ninput_schema: '{\"properties\":{\"path\":{\"type\":\"string\"}}}'\n",
        );
        write("bare", "name: bare\nversion: 0.1.0\n");

        let map = ToolAnnotationMap::from_workdir(workdir.path());
        assert!(map.for_tool("noter").is_opaque(&[key("note")]));
        assert!(
            map.for_tool("writer").is_empty() && map.for_tool("bare").is_empty(),
            "a tool that annotated nothing keeps the key-name heuristic"
        );
        assert!(ToolAnnotationMap::from_workdir(Path::new("/nowhere/at/all")).is_empty());
    }
}
