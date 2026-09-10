//! What a tool artifact declares about its own input: which values are filesystem destinations,
//! which subtrees are payload it merely stores, and which destinations it derives from an input
//! rather than reading whole.
//!
//! Two JSON Schema `format` values, readable on any property of a tool's `input_schema`, and one
//! keyword at the schema root beside `type` and `properties`:
//!
//! | Annotation | Effect on the write-intent analyser |
//! |---|---|
//! | [`FORMAT_DESTINATION`] on a string property | The value at that location is checked against the capsule's `read_only` rules, wherever in the input it sits |
//! | [`FORMAT_OPAQUE`] on an object or array property | [`crate::protected_paths`]'s key-name heuristic does not descend into that subtree |
//! | [`KEYWORD_DESTINATIONS`] at the schema root | Every `{location}` entry, with its optional `/suffix` joined onto each string that location resolves to, is checked the same way a declared property is |
//!
//! `format` is JSON Schema's own annotation keyword, whose value set is explicitly extensible: an
//! unknown value is ignored by a validator rather than rejected, and it survives the trip through
//! `input-schema: option<string>` and `runtime::yaml_to_json_string` unchanged. An unrecognized
//! *keyword* is ignored on the same terms, which is what lets [`KEYWORD_DESTINATIONS`] sit beside
//! `type` and `properties` without breaking a validator that has never heard of it.
//!
//! # Why the lowered form is only a set of locations
//!
//! An artifact must never get a say in its own containment. What is lowered here is therefore a
//! set of [`InputLocation`]s and nothing else — no boolean, no path, no allow list, no exemption
//! field — so there is no representable annotation whose meaning is "permit". A destination adds
//! a location to check and removes none; an opaque container removes a *guess about a container's
//! interior* and never a check on a value, which is why it is ignored on a string property. A
//! [`DerivedDestination`] holds a location, an optional relative suffix and the text as declared,
//! which is enough to name a path and not enough to excuse one. The refusal itself stays with
//! `ProtectedPaths::covering_rule`, which reads only the operator's declared `read_only` entries.
//!
//! Inside a subtree a tool declared opaque the key-name heuristic does not run, so a destination
//! that tool did not also declare is not seen there. The declaration is taken at its word: the
//! tool author is not this layer's adversary — see the "what it cannot see" table in
//! [`crate::protected_paths`].
//!
//! # Why the third word sits at the schema root
//!
//! A derived destination is not a property's value. A tool that reads Rust sources under
//! `repo_path` and writes its graph database under `<repo_path>/.murmur` has no input naming that
//! directory, and [`FORMAT_DESTINATION`] on `repo_path` would claim something else and untrue —
//! that the tool writes the repository. The statement is about the tool, so it is written where
//! the tool's schema is one object: at the root.
//!
//! A per-property "this is a source, not a destination" `format` value was rejected for the same
//! reason twice over. It cannot express a derived destination at all, and a value-level
//! declaration that a path is *not* a destination is one keystroke from an exemption, which is the
//! one meaning no annotation may carry. The root list says what such a word would have said
//! without that risk: an empty [`KEYWORD_DESTINATIONS`] array is the truthful declaration of a
//! tool that writes nothing named by or derived from its input, so "this tool only reads" arrives
//! as the degenerate case of the derived-destination mechanism rather than as a fourth vocabulary
//! word.
//!
//! # What stays inexpressible
//!
//! A destination whose write-ness depends on another input's value. A `git` tool's `repo` is
//! written by `checkout`, `reset --hard`, `stash pop`, `pull`, `merge` and `cherry_pick`, and read
//! by `log`, `diff`, `show` and `status` — one property, decided by the `operation` value beside
//! it. No annotation here takes a condition, so such a property stays unannotated and
//! [`unannotated_path_properties`] keeps naming it in `W-SEC-018`, permanently. That is the
//! decision rather than an omission: a conditional destination language would be a second
//! evaluator over model-chosen values inside the containment check, and a loudly unjudged property
//! is the better failure.

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

/// Schema-root keyword holding the tool's derived destination declarations.
///
/// Read beside `type` and `properties` on the schema's top level and nowhere else: it describes
/// the tool, not one of its values. See [`DerivedDestination`] for the entry grammar.
pub(crate) const KEYWORD_DESTINATIONS: &str = "murmur-destinations";

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

    /// The inverse of [`Self::render`]: the location a [`KEYWORD_DESTINATIONS`] entry names,
    /// written in exactly the spelling `render` produces.
    ///
    /// `None` for a spelling that is not one of those — an empty key, a stray bracket, a trailing
    /// `[`. Whether the location exists in the schema is a separate question, answered against the
    /// locations the walk visited.
    fn parse(text: &str) -> Option<Self> {
        if text == "<input>" {
            return Some(Self::default());
        }
        if text.is_empty() {
            return None;
        }
        let mut steps = Vec::new();
        for segment in text.split('.') {
            let split = segment.find('[').unwrap_or(segment.len());
            let (key, mut rest) = segment.split_at(split);
            if key.is_empty() || key.contains(']') {
                return None;
            }
            steps.push(LocationStep::Key(key.to_string()));
            while let Some(tail) = rest.strip_prefix("[]") {
                steps.push(LocationStep::Element);
                rest = tail;
            }
            if !rest.is_empty() {
                return None;
            }
        }
        Some(Self { steps })
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

/// One entry of the schema-root [`KEYWORD_DESTINATIONS`] list: a location the input names, an
/// optional relative suffix joined onto it, and the entry as the tool declared it.
///
/// The entry grammar is `{<location>}` optionally followed by `/<relative-suffix>`, with
/// `<location>` written the way [`InputLocation::render`] writes it — `{repo_path}/.murmur`,
/// `{paths[]}`, `{edits[].path}`. The braces are mandatory, so `repo_path/.murmur` is malformed.
/// The suffix is relative and stays inside what the location named: a leading `/`, an empty
/// component, a `.` or `..` component, and a `\` anywhere are each rejected.
///
/// Three fields and no fourth. There is nowhere here to write a verdict, so a derived destination
/// adds a path to check and can excuse none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DerivedDestination {
    base: InputLocation,
    suffix: Option<String>,
    declared: String,
}

impl DerivedDestination {
    /// Lower one entry, against the locations the schema walk actually visited.
    ///
    /// `None` for a malformed entry and for one naming a location no walk step reached, which is
    /// what makes a typo — `{repo-path}` against a schema declaring `repo_path` — loud rather than
    /// silently inert.
    fn parse(entry: &str, visited: &[InputLocation]) -> Option<Self> {
        if entry.contains('\\') {
            return None;
        }
        let rest = entry.strip_prefix('{')?;
        let (location, tail) = rest.split_once('}')?;
        let base = InputLocation::parse(location)?;
        if !visited.contains(&base) {
            return None;
        }
        let suffix = match tail {
            "" => None,
            _ => {
                let relative = tail.strip_prefix('/')?;
                if relative
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..")
                {
                    return None;
                }
                Some(relative.to_string())
            }
        };
        Some(Self {
            base,
            suffix,
            declared: entry.to_string(),
        })
    }

    /// The entry as declared, which is what a refusal names — the braces distinguish it from a
    /// property that carried [`FORMAT_DESTINATION`].
    pub(crate) fn declared(&self) -> &str {
        &self.declared
    }

    /// Every path this entry names in one concrete input: each string the location resolves to,
    /// with the suffix joined onto it.
    pub(crate) fn candidates(&self, input: &Value) -> Vec<String> {
        self.base
            .resolve(input)
            .into_iter()
            .map(|base| match &self.suffix {
                Some(suffix) => format!("{base}/{suffix}"),
                None => base.to_string(),
            })
            .collect()
    }
}

/// One tool's lowered annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolAnnotations {
    destinations: Vec<InputLocation>,
    opaque: Vec<InputLocation>,
    derived: Vec<DerivedDestination>,
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
        self.destinations.is_empty() && self.opaque.is_empty() && self.derived.is_empty()
    }

    /// The locations the tool declared as filesystem destinations.
    pub(crate) fn destinations(&self) -> &[InputLocation] {
        &self.destinations
    }

    /// The destinations the tool derives from an input location, in declaration order.
    ///
    /// Empty both for a tool that declared nothing and for one that declared the empty list, which
    /// is the same thing to check: neither adds a path.
    pub(crate) fn derived(&self) -> &[DerivedDestination] {
        &self.derived
    }

    /// Whether the tool declared the container at `steps` to be stored payload.
    pub(crate) fn is_opaque(&self, steps: &[LocationStep]) -> bool {
        self.opaque.iter().any(|location| location.steps == steps)
    }
}

/// One tool's `input_schema` read for everything this module decides from it.
struct LoweredSchema {
    annotations: ToolAnnotations,
    /// Whether the schema carried a valid [`KEYWORD_DESTINATIONS`] list. True for the empty list
    /// as well, which declares that the tool writes nothing named by or derived from its input.
    declares_destinations: bool,
    /// Every location the walk reached, which is what a [`KEYWORD_DESTINATIONS`] entry's location
    /// is checked against.
    visited: Vec<InputLocation>,
    /// Every path-shaped or destination-shaped property the walk saw outside an opaque subtree,
    /// with the location it sits at — the input to [`unannotated_path_properties`].
    path_shaped: Vec<(String, InputLocation)>,
}

impl LoweredSchema {
    fn from_schema_json(schema: &str) -> Self {
        let mut lowered = Self {
            annotations: ToolAnnotations::default(),
            declares_destinations: false,
            visited: Vec::new(),
            path_shaped: Vec::new(),
        };
        if let Ok(value) = serde_json::from_str::<Value>(schema) {
            lowered.walk(&value, &InputLocation::default(), 0, false);
            lowered.lower_root_destinations(&value);
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
    ///
    /// `in_opaque` says the node sits inside a subtree declared [`FORMAT_OPAQUE`]. The key-name
    /// heuristic does not descend there, so nothing under it is judged by key name and nothing
    /// under it is collected for `W-SEC-018`.
    fn walk(&mut self, node: &Value, at: &InputLocation, depth: usize, in_opaque: bool) {
        if depth > MAX_SCHEMA_DEPTH {
            return;
        }
        let Some(map) = node.as_object() else { return };
        push_unique(&mut self.visited, at);

        let format = murmur_format(node);
        match format {
            Some(FORMAT_DESTINATION) => push_unique(&mut self.annotations.destinations, at),
            Some(FORMAT_OPAQUE) => push_unique(&mut self.annotations.opaque, at),
            _ => {}
        }
        let in_opaque = in_opaque || format == Some(FORMAT_OPAQUE);

        if let Some(properties) = map.get("properties").and_then(Value::as_object) {
            for (key, child) in properties {
                let location = at.child(LocationStep::Key(key.clone()));
                if !in_opaque
                    && (matches_key(TOOL_PATH_KEYS, key) || matches_key(TOOL_DESTINATION_KEYS, key))
                {
                    self.path_shaped.push((key.clone(), location.clone()));
                }
                self.walk(child, &location, depth + 1, in_opaque);
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
                        self.walk(entry, &element, depth + 1, in_opaque);
                    }
                }
                other => self.walk(other, &element, depth + 1, in_opaque),
            }
        }
        if let Some(Value::Array(entries)) = map.get("prefixItems") {
            let element = at.child(LocationStep::Element);
            for entry in entries {
                self.walk(entry, &element, depth + 1, in_opaque);
            }
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(Value::Array(branches)) = map.get(keyword) {
                for branch in branches {
                    self.walk(branch, at, depth + 1, in_opaque);
                }
            }
        }
    }

    /// Lower the schema-root [`KEYWORD_DESTINATIONS`] list, read at the root and nowhere else.
    ///
    /// Validity is all-or-nothing: a value that is not an array of strings, or one malformed or
    /// dangling entry, leaves the tool exactly as it would be with no list at all — no derived
    /// destination is checked and `W-SEC-018` still fires. A partially honoured list would let a
    /// tool believe it had declared something it had not.
    fn lower_root_destinations(&mut self, root: &Value) {
        let Some(entries) = root.get(KEYWORD_DESTINATIONS) else {
            return;
        };
        let Some(entries) = entries.as_array() else {
            return;
        };
        let mut lowered = Vec::new();
        for entry in entries {
            let Some(derived) = entry
                .as_str()
                .and_then(|text| DerivedDestination::parse(text, &self.visited))
            else {
                return;
            };
            lowered.push(derived);
        }
        self.declares_destinations = true;
        self.annotations.derived = lowered;
    }
}

/// The murmur `format` value one schema node carries, and `None` for a node carrying another
/// vocabulary's value or none. The match is exact and case-sensitive.
fn murmur_format(node: &Value) -> Option<&str> {
    match node.get("format").and_then(Value::as_str) {
        Some(value @ (FORMAT_DESTINATION | FORMAT_OPAQUE)) => Some(value),
        _ => None,
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
            derived: Vec::new(),
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

/// The properties that leave a tool's calls judged by key name — the decision behind `W-SEC-018`.
///
/// Empty when the schema carries a valid [`KEYWORD_DESTINATIONS`] list, the empty list included:
/// the tool has answered the question the warning asks, for the whole tool. Otherwise every
/// property whose name is path-shaped or destination-shaped, whose own location carries no murmur
/// annotation, and which does not sit inside a subtree declared [`FORMAT_OPAQUE`] — in walk order,
/// deduplicated by name.
///
/// The decision is per property rather than per tool: annotating one property answers for that
/// property and for no other, so a tool that has declared some of its destinations keeps being
/// asked about the rest.
pub(crate) fn unannotated_path_properties(schema: &str) -> Vec<String> {
    let lowered = LoweredSchema::from_schema_json(schema);
    if lowered.declares_destinations {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for (name, location) in lowered.path_shaped {
        let annotated = lowered.annotations.destinations.contains(&location)
            || lowered.annotations.opaque.contains(&location);
        if !annotated && !names.contains(&name) {
            names.push(name);
        }
    }
    names
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

    /// A location parses back from exactly the spelling it renders to, and from nothing else.
    #[test]
    fn a_location_parses_back_from_its_rendered_spelling() {
        for steps in [
            vec![],
            vec![key("sink")],
            vec![key("edits"), LocationStep::Element, key("path")],
            vec![key("a"), LocationStep::Element, LocationStep::Element],
        ] {
            let at = location(&steps);
            assert_eq!(
                InputLocation::parse(&at.render()),
                Some(at.clone()),
                "{at:?}"
            );
        }
        for malformed in ["", ".", "a.", "a..b", "a[", "a]", "a[0]", "[]", "a[]b"] {
            assert_eq!(InputLocation::parse(malformed), None, "{malformed}");
        }
    }

    /// The entry grammar: a braced location, an optional relative suffix, and a location the walk
    /// actually reached.
    #[test]
    fn a_derived_destination_entry_parses_only_in_its_grammar() {
        let visited = vec![
            location(&[]),
            location(&[key("repo_path")]),
            location(&[key("paths"), LocationStep::Element]),
        ];
        let parsed = |entry: &str| DerivedDestination::parse(entry, &visited);

        let derived = parsed("{repo_path}/.murmur").expect("a suffixed entry parses");
        assert_eq!(derived.base, location(&[key("repo_path")]));
        assert_eq!(derived.suffix.as_deref(), Some(".murmur"));
        assert_eq!(derived.declared(), "{repo_path}/.murmur");

        let bare = parsed("{repo_path}").expect("an entry with no suffix parses");
        assert_eq!(bare.suffix, None);
        assert!(
            parsed("{paths[]}/out").is_some(),
            "an array step is a location"
        );
        assert!(
            parsed("{repo_path}/a/b/c").is_some(),
            "a multi-component suffix is one relative path"
        );

        for malformed in [
            "repo_path/.murmur",
            "{repo_path",
            "{repo_path}//etc",
            "{repo_path}/",
            "{repo_path}/../out",
            "{repo_path}/./out",
            "{repo_path}/a/../b",
            "{repo_path}\\out",
            "{repo_path}.murmur",
            "{repo-path}/.murmur",
            "{absent}",
            "{}",
        ] {
            assert!(parsed(malformed).is_none(), "{malformed}");
        }
    }

    /// A valid list lowers to one derived destination per entry, and each resolves every string
    /// its location names with the suffix joined on.
    #[test]
    fn a_declared_list_lowers_to_its_derived_destinations() {
        let annotations = ToolAnnotations::from_schema_json(
            r#"{"type":"object",
                "properties":{"repo_path":{"type":"string"},"paths":{"type":"array","items":{"type":"string"}}},
                "murmur-destinations":["{repo_path}/.murmur","{paths[]}"]}"#,
        );
        let input = serde_json::json!({"repo_path": "repo", "paths": ["a", "b"]});
        let candidates: Vec<String> = annotations
            .derived()
            .iter()
            .flat_map(|derived| derived.candidates(&input))
            .collect();
        assert_eq!(candidates, vec!["repo/.murmur", "a", "b"]);
        assert_eq!(
            annotations.derived()[0].declared(),
            "{repo_path}/.murmur",
            "a refusal names the entry as the tool wrote it"
        );
    }

    /// Validity is all-or-nothing: one bad entry, or a value that is not an array of strings,
    /// lowers the whole list to nothing and leaves the tool where it was.
    #[test]
    fn one_malformed_entry_invalidates_the_whole_list() {
        let schema = |list: &str| {
            format!(
                r#"{{"type":"object","properties":{{"repo_path":{{"type":"string"}},"file":{{"type":"string"}}}},"murmur-destinations":{list}}}"#
            )
        };
        for list in [
            r#""{repo_path}""#,
            r#"[7]"#,
            r#"["{repo_path}/.murmur","{repo_path}/../out"]"#,
            r#"["repo_path/.murmur"]"#,
            r#"["{repo-path}/.murmur"]"#,
        ] {
            let text = schema(list);
            assert!(
                ToolAnnotations::from_schema_json(&text)
                    .derived()
                    .is_empty(),
                "{list}"
            );
            assert_eq!(
                unannotated_path_properties(&text),
                vec!["file".to_string()],
                "an unreadable declaration leaves the warning firing: {list}"
            );
        }
    }

    /// The keyword is read at the schema root and nowhere else.
    #[test]
    fn a_nested_destinations_keyword_is_not_read() {
        let annotations = ToolAnnotations::from_schema_json(
            r#"{"type":"object","properties":{"inner":{"type":"object",
                "properties":{"repo_path":{"type":"string"}},
                "murmur-destinations":["{inner.repo_path}"]}}}"#,
        );
        assert!(annotations.derived().is_empty());
    }

    /// The `W-SEC-018` decision is per property: an annotated property answers for itself and for
    /// no sibling.
    #[test]
    fn the_warning_names_every_unannotated_path_shaped_property() {
        assert_eq!(
            unannotated_path_properties(
                r#"{"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}}}"#
            ),
            vec!["file_path".to_string()]
        );
        assert_eq!(
            unannotated_path_properties(
                r#"{"type":"object","properties":{"dest":{"type":"string","format":"murmur-destination"},"repo":{"type":"string"},"path":{"type":"string"}}}"#
            ),
            vec!["path".to_string()],
            "an annotated sibling silences itself and nothing else"
        );
        assert_eq!(
            unannotated_path_properties(
                r#"{"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"filename":{"type":"string"}}}},"file":{"type":"string"}}}"#
            ),
            vec!["filename".to_string(), "file".to_string()],
            "a nested property is what the heuristic reads, so it is what the warning reads, in \
             the order the walk reaches it"
        );
        assert_eq!(
            unannotated_path_properties(
                r#"{"type":"object","properties":{"query":{"type":"string"}}}"#
            ),
            Vec::<String>::new()
        );
        assert!(unannotated_path_properties("").is_empty());
    }

    /// A property inside a subtree the schema declared opaque is not judged by key name, so the
    /// warning has nothing to say about it.
    #[test]
    fn a_property_inside_an_opaque_subtree_is_not_named() {
        assert!(unannotated_path_properties(
            r#"{"type":"object","properties":{"note":{"type":"object","format":"murmur-opaque","properties":{"file":{"type":"string"}}}}}"#
        )
        .is_empty());
    }

    /// A valid list answers for the whole tool, the empty list included.
    #[test]
    fn a_valid_destination_list_silences_the_warning() {
        assert!(unannotated_path_properties(
            r#"{"type":"object","properties":{"repo_path":{"type":"string"},"file":{"type":"string"},"path":{"type":"string"}},"murmur-destinations":["{repo_path}/.murmur"]}"#
        )
        .is_empty());
        assert!(
            unannotated_path_properties(
                r#"{"type":"object","properties":{"file":{"type":"string"}},"murmur-destinations":[]}"#
            )
            .is_empty(),
            "the empty list declares that the tool writes nothing named by or derived from its input"
        );
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
