//! Operator-authored per-artifact configuration: the `config:` block on an artifact's entry in
//! the capsule operator's own manifest, lowered to compact JSON and delivered to that artifact
//! alone as [`ARTIFACT_CONFIG_ENV`].
//!
//! Two properties decide the shape, and both are load-bearing:
//!
//! * **One channel, one variable.** The value arrives inline in the guest environment, on the
//!   artifact's own grant, and nowhere else. Writing it to a file under the artifact's `state/`
//!   grant would make delivery depend on `capabilities.state` also being granted, so the same
//!   manifest key would reach an artifact two different ways and every artifact would need two
//!   readers.
//! * **Shape, not meaning.** The runtime checks that the block is a string-keyed mapping which
//!   serialises to JSON within [`MAX_ARTIFACT_CONFIG_BYTES`], and nothing else. It cannot know
//!   which keys a given artifact requires; that stays an artifact-side error. What the channel
//!   buys is that the artifact gets a value it can trust to be well-formed JSON, and that a
//!   malformed one refuses the launch instead of surfacing one tool call at a time.
//!
//! Lowering is pure, like [`crate::state_store::validate_store_name`] beside it: nothing here
//! touches the filesystem or the host environment, so `mur run --explain-scope` refuses exactly
//! what a launch would refuse while remaining a read-only diagnostic. The staging path is what
//! puts the lowered JSON onto a grant.

use crate::errors::RuntimeError;

/// The guest environment variable a configured artifact reads its block out of.
///
/// Runtime-owned: it is never copied out of the host environment, even when
/// `capabilities.env.allow` names it (see [`crate::shell::build_wasi_env_allowlist`]), and both
/// `build_wasi_ctx` functions inject it ahead of anything a manifest can reach.
pub const ARTIFACT_CONFIG_ENV: &str = "MURMUR_ARTIFACT_CONFIG";

/// Largest serialised config, in bytes of compact JSON, that may be handed to a guest.
///
/// A cap rather than a truncation: half a config is not a smaller config, it is a syntactically
/// broken one, and an artifact that received it would fail somewhere far from the manifest line
/// that caused it. Sized so an ordinary declaration never approaches it while a whole document
/// pasted into a manifest is refused at launch.
pub const MAX_ARTIFACT_CONFIG_BYTES: usize = 65_536;

/// Lower one artifact's `config:` block to the compact JSON string delivered as
/// [`ARTIFACT_CONFIG_ENV`].
///
/// `artifact` names the declaring entry and appears in every refusal, because the operator's next
/// action is to find that entry in their own manifest.
///
/// The four rules, in the order they are checked:
///
/// | Rule | Why |
/// | --- | --- |
/// | the block is a mapping | a scalar or a sequence has no keys for an artifact to read |
/// | every key is a string | a JSON object has string keys; a YAML integer key has no faithful form |
/// | it serialises to JSON | the channel carries JSON and nothing else |
/// | the JSON is within [`MAX_ARTIFACT_CONFIG_BYTES`] | an environment variable is not a file |
///
/// A `config:` written with nothing under it arrives here as [`serde_yaml::Value::Null`] and is
/// refused by the first rule: a written declaration that carries nothing is a mistake worth
/// naming, on the same terms `capabilities.state.store: ""` is.
///
/// The serialisation is `serde_json`'s compact form over a `serde_yaml::Mapping`, which preserves
/// declaration order, so the same manifest always produces the same bytes.
pub fn lower_artifact_config(
    artifact: &str,
    config: &serde_yaml::Value,
) -> Result<String, RuntimeError> {
    let refuse = |message: String| RuntimeError::InvalidArtifactConfig {
        artifact: artifact.to_string(),
        message,
    };

    let mapping = config.as_mapping().ok_or_else(|| {
        refuse(format!(
            "'config:' must be a mapping of keys to values, but this entry declares {}",
            describe_yaml_shape(config)
        ))
    })?;

    for key in mapping.keys() {
        if !key.is_string() {
            return Err(refuse(format!(
                "every key in 'config:' must be a string, but this entry declares {} key {}",
                describe_yaml_shape(key),
                render_yaml_scalar(key)
            )));
        }
    }

    let json = serde_json::to_string(mapping)
        .map_err(|err| refuse(format!("'config:' does not serialize to JSON: {err}")))?;

    if json.len() > MAX_ARTIFACT_CONFIG_BYTES {
        return Err(refuse(format!(
            "'config:' serializes to {} bytes of JSON, over the {MAX_ARTIFACT_CONFIG_BYTES}-byte \
             limit for {ARTIFACT_CONFIG_ENV}",
            json.len()
        )));
    }

    Ok(json)
}

/// The artifacts that declare a `config:` block, in manifest order, with every block validated.
///
/// Takes `(artifact name, that artifact's declared config)` pairs rather than a concrete artifact
/// type so the two callers that must agree can both satisfy it: `stage_session` holds
/// [`crate::types::ArtifactRequest`]s and `mur run --explain-scope` holds
/// [`murmur_artifact::RuntimeArtifact`]s. One resolution shared by both is what makes the report a
/// description of the launch rather than a second opinion about it — including its refusals.
///
/// Returns names only. What an artifact is configured *with* is operator-authored plaintext that
/// belongs in the manifest and in no diagnostic this produces.
pub fn configured_artifact_names<'a, I>(artifacts: I) -> Result<Vec<String>, RuntimeError>
where
    I: IntoIterator<Item = (&'a str, Option<&'a serde_yaml::Value>)>,
{
    let mut names = Vec::new();
    for (name, config) in artifacts {
        let Some(config) = config else {
            continue;
        };
        lower_artifact_config(name, config)?;
        names.push(name.to_string());
    }
    Ok(names)
}

/// What kind of YAML node this is, in the vocabulary a manifest author writes in, for a refusal
/// that tells them what they wrote rather than what a deserializer expected.
fn describe_yaml_shape(value: &serde_yaml::Value) -> &'static str {
    match value {
        serde_yaml::Value::Null => "an empty block",
        serde_yaml::Value::Bool(_) => "a boolean",
        serde_yaml::Value::Number(_) => "a number",
        serde_yaml::Value::String(_) => "a string",
        serde_yaml::Value::Sequence(_) => "a sequence",
        serde_yaml::Value::Mapping(_) => "a mapping",
        serde_yaml::Value::Tagged(_) => "a tagged value",
    }
}

/// A scalar rendered the way it reads in the manifest, so a refusal can quote the offending key.
/// Non-scalars fall back to their JSON form, which is still something the operator can search for.
fn render_yaml_scalar(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::Bool(value) => value.to_string(),
        serde_yaml::Value::Number(value) => value.to_string(),
        serde_yaml::Value::String(value) => format!("'{value}'"),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<unprintable>".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(source: &str) -> serde_yaml::Value {
        serde_yaml::from_str(source).expect("fixture parses as YAML")
    }

    /// The shape the channel promises: JSON, with scalars carried across unchanged. An integer
    /// stays an integer and a sequence stays a sequence, so an artifact deserializing into its own
    /// types needs no YAML-flavoured coercion.
    #[test]
    fn a_mapping_lowers_to_compact_json_with_types_preserved() {
        let json = lower_artifact_config(
            "murmur-tool-corpus",
            &yaml(
                "types:\n  utterance:\n    schema: { type: object, required: [text] }\n\
                 read_recent: { default: 20, max: 100 }\n",
            ),
        )
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({
                "types": {"utterance": {"schema": {"type": "object", "required": ["text"]}}},
                "read_recent": {"default": 20, "max": 100},
            })
        );
        assert!(!json.contains(' '), "the compact form carries no padding");
    }

    /// Declaration order is preserved, so the same manifest always produces the same bytes and a
    /// guest that hashes or logs the value sees a stable one.
    #[test]
    fn the_same_block_always_lowers_to_the_same_bytes() {
        let block = yaml("b: 1\na: 2\nc: [3, 4]\n");
        let first = lower_artifact_config("config-echo", &block).unwrap();
        assert_eq!(first, r#"{"b":1,"a":2,"c":[3,4]}"#);
        assert_eq!(first, lower_artifact_config("config-echo", &block).unwrap());
    }

    /// An empty mapping is a mapping: it carries nothing, but it is well-formed and an artifact
    /// reading it gets valid JSON. Distinct from `config:` with nothing under it, below.
    #[test]
    fn an_empty_mapping_is_accepted() {
        assert_eq!(
            lower_artifact_config("config-echo", &yaml("{}")).unwrap(),
            "{}"
        );
    }

    /// Every rejection names the artifact, because the operator's next action is to find that
    /// entry in their own manifest.
    #[test]
    fn a_block_that_is_not_a_string_keyed_mapping_is_refused_by_artifact_name() {
        for (source, expected) in [
            ("7", "a number"),
            ("[a, b]", "a sequence"),
            ("~", "an empty block"),
            ("'literal'", "a string"),
            ("true", "a boolean"),
        ] {
            let err = match lower_artifact_config("config-echo", &yaml(source)) {
                Ok(json) => panic!("'{source}' must be refused, lowered to {json}"),
                Err(err) => err,
            };
            assert!(
                matches!(err, RuntimeError::InvalidArtifactConfig { .. }),
                "'{source}' must refuse as InvalidArtifactConfig, got: {err}"
            );
            let message = err.to_string();
            assert!(
                message.contains("config-echo"),
                "the refusal must name the artifact: {message}"
            );
            assert!(
                message.contains(expected),
                "the refusal must say what was declared ({expected}): {message}"
            );
        }
    }

    #[test]
    fn a_non_string_key_is_refused_and_quoted() {
        let err = lower_artifact_config("config-echo", &yaml("1: x\n")).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("must be a string"),
            "the refusal must state the rule: {message}"
        );
        assert!(
            message.contains('1'),
            "the refusal must quote the offending key: {message}"
        );
    }

    /// Refused loudly rather than truncated: half a config is not a smaller config, it is broken
    /// JSON that would fail somewhere far from the manifest line that caused it.
    #[test]
    fn an_oversized_block_names_the_size_and_the_cap() {
        let mut block = serde_yaml::Mapping::new();
        block.insert("blob".into(), "x".repeat(70_000).into());
        let err =
            lower_artifact_config("config-echo", &serde_yaml::Value::Mapping(block)).unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("70011"),
            "the refusal must name the actual byte size: {message}"
        );
        assert!(
            message.contains("65536"),
            "the refusal must name the cap: {message}"
        );
    }

    /// One block at the cap is accepted, so the boundary is `>` and not `>=`.
    #[test]
    fn a_block_exactly_at_the_cap_is_accepted() {
        // `{"blob":"…"}` is 11 bytes of envelope around the padding.
        let mut block = serde_yaml::Mapping::new();
        block.insert(
            "blob".into(),
            "x".repeat(MAX_ARTIFACT_CONFIG_BYTES - 11).into(),
        );
        let json =
            lower_artifact_config("config-echo", &serde_yaml::Value::Mapping(block)).unwrap();
        assert_eq!(json.len(), MAX_ARTIFACT_CONFIG_BYTES);
    }

    /// The resolver both callers share: names in manifest order, undeclared entries skipped
    /// entirely, and no trace of what any of them was configured with.
    #[test]
    fn names_are_reported_in_manifest_order_and_carry_no_values() {
        let first = yaml("who: a\n");
        let second = yaml("read_recent: { max: 100 }\n");

        let names = configured_artifact_names([
            ("tool-a", Some(&first)),
            ("plain-tool", None),
            ("tool-b", Some(&second)),
        ])
        .unwrap();

        assert_eq!(names, vec!["tool-a".to_string(), "tool-b".to_string()]);
        assert!(
            !names.iter().any(|name| name.contains("read_recent")),
            "a name list must carry nothing from inside a block"
        );
    }

    #[test]
    fn an_undeclared_artifact_set_reports_nothing() {
        assert!(
            configured_artifact_names([("plain-tool", None), ("bare-tool", None)])
                .unwrap()
                .is_empty()
        );
    }

    /// The diagnostic refuses whatever the launch refuses: a malformed block on any entry fails
    /// the whole resolution, naming the entry.
    #[test]
    fn one_malformed_block_refuses_the_whole_resolution() {
        let good = yaml("who: a\n");
        let bad = yaml("7");

        let err = configured_artifact_names([("tool-a", Some(&good)), ("tool-b", Some(&bad))])
            .unwrap_err();
        assert!(err.to_string().contains("tool-b"), "{err}");
    }
}
