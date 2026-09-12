use std::{fs, path::Path};

use serde_yaml::Value;
use thiserror::Error;

use crate::manifest_path::MANIFEST_FILENAME;
use crate::RuntimeType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub runtime: String,
    pub implementation: Option<String>,
    /// Declared packaging type, from `execution:` (`wasm` | `native` | `static`). When present it
    /// is authoritative for [`Manifest::registry_runtime`]; when absent that method falls back to
    /// deriving the type from `runtime:` + `implementation:`.
    pub execution: Option<RuntimeType>,
    /// Companion files that must sit beside this manifest for `build_artifact` to succeed.
    /// Declared via `requires_files:`; when absent it defaults to `["skill.md"]` for
    /// `runtime: skill` and to an empty list for every other role. An explicit value — including
    /// an empty list — always wins over that default.
    pub requires_files: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("{} not found at {}", MANIFEST_FILENAME, .0)]
    NotFound(String),
    #[error("{0}")]
    YamlSyntax(String),
    #[error("{}: missing required field '{field}'", MANIFEST_FILENAME)]
    MissingField { field: String },
    #[error(
        "{}: field '{field}' has invalid type (expected {expected}, got {got})",
        MANIFEST_FILENAME
    )]
    InvalidType {
        field: String,
        expected: String,
        got: String,
    },
    #[error("{}: field '{field}' {message}", MANIFEST_FILENAME)]
    InvalidValue { field: String, message: String },
    #[error("failed to read {} at {path}: {source}", MANIFEST_FILENAME)]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[must_use = "validated manifest is required to proceed with build"]
pub fn load_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let content = fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            return ManifestError::NotFound(path.display().to_string());
        }
        ManifestError::Io {
            path: path.display().to_string(),
            source,
        }
    })?;

    Manifest::from_yaml_str(&content)
}

impl Manifest {
    /// The registry's packaging type for this artifact.
    ///
    /// A declared `execution:` field is authoritative. Only when it is absent does the type get
    /// derived from the role: `runtime: skill` → `Static`; `runtime: tool` +
    /// `implementation: native` → `Native`; everything else → `Wasm`.
    #[must_use]
    pub fn registry_runtime(&self) -> RuntimeType {
        if let Some(execution) = self.execution {
            return execution;
        }
        if self.runtime == "skill" {
            RuntimeType::Static
        } else if self.runtime == "tool" && self.implementation.as_deref() == Some("native") {
            RuntimeType::Native
        } else {
            RuntimeType::Wasm
        }
    }

    #[must_use = "parsed manifest is required to proceed with build"]
    pub fn from_yaml_str(input: &str) -> Result<Self, ManifestError> {
        let value: Value = serde_yaml::from_str(input).map_err(|err| {
            if let Some(location) = err.location() {
                ManifestError::YamlSyntax(format!(
                    "{}: YAML syntax error at line {}, column {}: {}",
                    MANIFEST_FILENAME,
                    location.line(),
                    location.column(),
                    err
                ))
            } else {
                ManifestError::YamlSyntax(format!("{MANIFEST_FILENAME}: YAML syntax error: {err}"))
            }
        })?;

        let root = value
            .as_mapping()
            .ok_or_else(|| ManifestError::InvalidType {
                field: "<root>".to_string(),
                expected: "mapping".to_string(),
                got: yaml_type_name(&value),
            })?;

        let name = required_string(root, "name")?;
        let version = required_string(root, "version")?;
        let runtime = required_string(root, "runtime")?;
        let implementation = optional_string(root, "implementation");
        let execution = optional_runtime_type(root, "execution")?;
        let requires_files = optional_string_seq(root, "requires_files")?
            .unwrap_or_else(|| default_requires_files(&runtime));

        Ok(Self {
            name,
            version,
            runtime,
            implementation,
            execution,
            requires_files,
        })
    }
}

/// The companion files a role requires when `requires_files:` is not declared. Reproduces the
/// hardcoded `skill.md` check `build_artifact` used to apply to `runtime: skill`, so a manifest
/// that says nothing about `requires_files:` builds exactly as it did before.
fn default_requires_files(runtime: &str) -> Vec<String> {
    if runtime == "skill" {
        vec!["skill.md".to_string()]
    } else {
        Vec::new()
    }
}

/// Parses `execution:` into a [`RuntimeType`]. An unparseable value is a hard error rather than a
/// silent fall-back to the derived default — a typo'd packaging type must not publish as `wasm`.
fn optional_runtime_type(
    root: &serde_yaml::Mapping,
    field: &str,
) -> Result<Option<RuntimeType>, ManifestError> {
    let key = Value::String(field.to_string());
    let Some(value) = root.get(&key) else {
        return Ok(None);
    };

    let raw = value.as_str().ok_or_else(|| ManifestError::InvalidType {
        field: field.to_string(),
        expected: "string".to_string(),
        got: yaml_type_name(value),
    })?;

    raw.parse::<RuntimeType>()
        .map(Some)
        .map_err(|_| ManifestError::InvalidType {
            field: field.to_string(),
            expected: "one of: wasm, native, static".to_string(),
            got: raw.to_string(),
        })
}

fn optional_string_seq(
    root: &serde_yaml::Mapping,
    field: &str,
) -> Result<Option<Vec<String>>, ManifestError> {
    let key = Value::String(field.to_string());
    let Some(value) = root.get(&key) else {
        return Ok(None);
    };

    let seq = value
        .as_sequence()
        .ok_or_else(|| ManifestError::InvalidType {
            field: field.to_string(),
            expected: "sequence of strings".to_string(),
            got: yaml_type_name(value),
        })?;

    seq.iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ManifestError::InvalidType {
                    field: field.to_string(),
                    expected: "sequence of strings".to_string(),
                    got: yaml_type_name(entry),
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn optional_string(root: &serde_yaml::Mapping, field: &str) -> Option<String> {
    let key = serde_yaml::Value::String(field.to_string());
    root.get(&key)?.as_str().map(str::to_string)
}

fn required_string(root: &serde_yaml::Mapping, field: &str) -> Result<String, ManifestError> {
    let key = Value::String(field.to_string());
    let value = root.get(&key).ok_or_else(|| ManifestError::MissingField {
        field: field.into(),
    })?;

    let value_str = value.as_str().ok_or_else(|| ManifestError::InvalidType {
        field: field.to_string(),
        expected: "string".to_string(),
        got: yaml_type_name(value),
    })?;

    Ok(value_str.to_string())
}

fn yaml_type_name(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(_) => "boolean".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
        Value::Sequence(_) => "sequence".into(),
        Value::Mapping(_) => "mapping".into(),
        Value::Tagged(_) => "tagged".into(),
    }
}

/// The substitution point for the inference credential in [`InferenceAuth::value`].
pub const INFERENCE_AUTH_KEY_PLACEHOLDER: &str = "{key}";

/// How a driver's provider expects the inference credential to be presented, from the driver's
/// own bundled `murmur.yaml`:
///
/// ```yaml
/// inference_auth:
///   header: Authorization
///   value: "Bearer {key}"
/// ```
///
/// The runtime attaches `header` to each request the driver sends through the inference gateway,
/// with `{key}` replaced by `inference.api_key`. Holds the template only, never a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceAuth {
    /// A valid HTTP header name, as written. Compared case-insensitively on the wire.
    pub header: String,
    /// The header value template. Contains [`INFERENCE_AUTH_KEY_PLACEHOLDER`] exactly once.
    pub value: String,
}

impl InferenceAuth {
    /// The header value for `key`.
    #[must_use]
    pub fn render(&self, key: &str) -> String {
        self.value.replacen(INFERENCE_AUTH_KEY_PLACEHOLDER, key, 1)
    }
}

/// Reads the top-level `inference_auth:` block from a driver's bundled manifest text.
///
/// `Ok(None)` means the manifest has no `inference_auth` key. `Err` means the block is present
/// and unusable: not a mapping, `header` or `value` missing or not a string, `header` not a valid
/// HTTP header name, or `value` not containing `{key}` exactly once or carrying characters no
/// header value may hold.
pub fn parse_inference_auth(manifest_yaml: &str) -> Result<Option<InferenceAuth>, ManifestError> {
    const FIELD: &str = "inference_auth";
    let value: Value = serde_yaml::from_str(manifest_yaml).map_err(|err| {
        ManifestError::YamlSyntax(format!("{MANIFEST_FILENAME}: YAML syntax error: {err}"))
    })?;
    let Some(block) = value
        .as_mapping()
        .and_then(|root| root.get(Value::String(FIELD.to_string())))
    else {
        return Ok(None);
    };
    let block = block
        .as_mapping()
        .ok_or_else(|| ManifestError::InvalidType {
            field: FIELD.to_string(),
            expected: "mapping".to_string(),
            got: yaml_type_name(block),
        })?;

    let header = required_string(block, "header").map_err(|err| prefix_field(err, FIELD))?;
    let template = required_string(block, "value").map_err(|err| prefix_field(err, FIELD))?;

    if http::HeaderName::from_bytes(header.as_bytes()).is_err() {
        return Err(ManifestError::InvalidValue {
            field: format!("{FIELD}.header"),
            message: format!("'{header}' is not a valid HTTP header name"),
        });
    }
    let placeholders = template.matches(INFERENCE_AUTH_KEY_PLACEHOLDER).count();
    if placeholders != 1 {
        return Err(ManifestError::InvalidValue {
            field: format!("{FIELD}.value"),
            message: format!(
                "must contain {INFERENCE_AUTH_KEY_PLACEHOLDER} exactly once (found {placeholders})"
            ),
        });
    }
    if http::HeaderValue::from_str(&template.replace(INFERENCE_AUTH_KEY_PLACEHOLDER, "")).is_err() {
        return Err(ManifestError::InvalidValue {
            field: format!("{FIELD}.value"),
            message: "contains characters an HTTP header value cannot hold".to_string(),
        });
    }

    Ok(Some(InferenceAuth {
        header,
        value: template,
    }))
}

/// Names a child of `block` by its full dotted path rather than its bare key.
fn prefix_field(err: ManifestError, block: &str) -> ManifestError {
    match err {
        ManifestError::MissingField { field } => ManifestError::MissingField {
            field: format!("{block}.{field}"),
        },
        ManifestError::InvalidType {
            field,
            expected,
            got,
        } => ManifestError::InvalidType {
            field: format!("{block}.{field}"),
            expected,
            got,
        },
        other => other,
    }
}

#[cfg(test)]
mod inference_auth_tests {
    use super::*;

    /// Each block exactly as the four shipped drivers write it, comment line included.
    const ANTHROPIC: &str = "name: murmur-driver-anthropic\nversion: 0.1.0\nruntime: driver\n\n\
        # How this provider expects the inference credential to be presented; the runtime substitutes {key}.\n\
        inference_auth:\n  header: x-api-key\n  value: \"{key}\"\n";
    const BEARER: &str = "\n# How this provider expects the inference credential to be presented; the runtime substitutes {key}.\n\
        inference_auth:\n  header: Authorization\n  value: \"Bearer {key}\"\n";

    fn bearer(name: &str) -> String {
        format!("name: {name}\nversion: 0.1.0\nruntime: driver\n{BEARER}")
    }

    #[test]
    fn inference_auth_parses_the_anthropic_block() {
        let auth = parse_inference_auth(ANTHROPIC).unwrap().unwrap();
        assert_eq!(auth.header, "x-api-key");
        assert_eq!(auth.render("sk-1"), "sk-1");
    }

    #[test]
    fn inference_auth_parses_the_three_bearer_blocks() {
        for driver in [
            "murmur-driver-openai",
            "murmur-driver-deepseek",
            "murmur-driver-moonshotai",
        ] {
            let auth = parse_inference_auth(&bearer(driver)).unwrap().unwrap();
            assert_eq!(auth.header, "Authorization", "{driver}");
            assert_eq!(auth.render("sk-2"), "Bearer sk-2", "{driver}");
        }
    }

    #[test]
    fn inference_auth_absent_is_none() {
        let yaml = "name: d\nversion: 0.1.0\nruntime: driver\n";
        assert_eq!(parse_inference_auth(yaml).unwrap(), None);
    }

    fn refused(block: &str) -> String {
        let yaml = format!("name: d\nversion: 0.1.0\nruntime: driver\n{block}");
        parse_inference_auth(&yaml).unwrap_err().to_string()
    }

    #[test]
    fn inference_auth_refuses_each_malformed_block() {
        let cases = [
            (
                "inference_auth: x-api-key\n",
                "'inference_auth' has invalid type",
            ),
            ("inference_auth:\n", "'inference_auth' has invalid type"),
            (
                "inference_auth:\n  value: \"{key}\"\n",
                "'inference_auth.header'",
            ),
            (
                "inference_auth:\n  header: x-api-key\n",
                "'inference_auth.value'",
            ),
            (
                "inference_auth:\n  header: [x]\n  value: \"{key}\"\n",
                "'inference_auth.header' has invalid type",
            ),
            (
                "inference_auth:\n  header: x-api-key\n  value: 42\n",
                "'inference_auth.value' has invalid type",
            ),
            (
                "inference_auth:\n  header: \"bad header\"\n  value: \"{key}\"\n",
                "not a valid HTTP header name",
            ),
            (
                "inference_auth:\n  header: \"\"\n  value: \"{key}\"\n",
                "not a valid HTTP header name",
            ),
            (
                "inference_auth:\n  header: Authorization\n  value: Bearer\n",
                "exactly once (found 0)",
            ),
            (
                "inference_auth:\n  header: Authorization\n  value: \"{key}{key}\"\n",
                "exactly once (found 2)",
            ),
            (
                "inference_auth:\n  header: Authorization\n  value: \"Bearer\\n{key}\"\n",
                "cannot hold",
            ),
        ];
        for (block, expected) in cases {
            let message = refused(block);
            assert!(
                message.contains(expected),
                "{block:?}: expected {expected:?} in {message:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_name_field_is_actionable() {
        let err = Manifest::from_yaml_str("version: 0.0.1\nruntime: wasm\n").unwrap_err();
        assert!(err.to_string().contains("missing required field 'name'"));
    }

    #[test]
    fn missing_version_field_is_actionable() {
        let err = Manifest::from_yaml_str("name: hello\nruntime: wasm\n").unwrap_err();
        assert!(err.to_string().contains("missing required field 'version'"));
    }

    #[test]
    fn missing_runtime_field_is_actionable() {
        let err = Manifest::from_yaml_str("name: hello\nversion: 0.0.1\n").unwrap_err();
        assert!(err.to_string().contains("missing required field 'runtime'"));
    }

    #[test]
    fn malformed_yaml_reports_line_info() {
        let err = Manifest::from_yaml_str("name: ok\nversion: [\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line"));
        assert!(msg.contains("column"));
    }

    #[test]
    fn type_mismatch_reports_expected_and_got() {
        let err = Manifest::from_yaml_str("name: 42\nversion: 1.0.0\nruntime: wasm\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("field 'name'"));
        assert!(msg.contains("expected string"));
        assert!(msg.contains("got number"));
    }

    #[test]
    fn registry_runtime_skill_returns_static_type() {
        let manifest =
            Manifest::from_yaml_str("name: my-skill\nversion: 0.1.0\nruntime: skill\n").unwrap();
        assert_eq!(manifest.registry_runtime(), RuntimeType::Static);
    }

    #[test]
    fn registry_runtime_tool_native_returns_native() {
        let manifest = Manifest::from_yaml_str(
            "name: my-tool\nversion: 0.1.0\nruntime: tool\nimplementation: native\n",
        )
        .unwrap();
        assert_eq!(manifest.registry_runtime(), RuntimeType::Native);
    }

    #[test]
    fn registry_runtime_wasm_returns_wasm() {
        let manifest =
            Manifest::from_yaml_str("name: my-wasm\nversion: 0.1.0\nruntime: wasm\n").unwrap();
        assert_eq!(manifest.registry_runtime(), RuntimeType::Wasm);
    }

    #[test]
    fn execution_field_overrides_derived_registry_runtime() {
        let manifest = Manifest::from_yaml_str(
            "name: my-tool\nversion: 0.1.0\nruntime: tool\nexecution: static\n",
        )
        .unwrap();
        assert_eq!(manifest.execution, Some(RuntimeType::Static));
        // Role-based derivation would have said Wasm for a plain tool.
        assert_eq!(manifest.registry_runtime(), RuntimeType::Static);
    }

    #[test]
    fn execution_field_overrides_skill_static_default() {
        let manifest = Manifest::from_yaml_str(
            "name: my-skill\nversion: 0.1.0\nruntime: skill\nexecution: wasm\n",
        )
        .unwrap();
        assert_eq!(manifest.registry_runtime(), RuntimeType::Wasm);
    }

    #[test]
    fn execution_is_parsed_case_insensitively() {
        let manifest = Manifest::from_yaml_str(
            "name: my-tool\nversion: 0.1.0\nruntime: tool\nexecution: STATIC\n",
        )
        .unwrap();
        assert_eq!(manifest.registry_runtime(), RuntimeType::Static);
    }

    #[test]
    fn execution_absent_preserves_all_derived_branches() {
        let skill = Manifest::from_yaml_str("name: s\nversion: 0.1.0\nruntime: skill\n").unwrap();
        let native = Manifest::from_yaml_str(
            "name: n\nversion: 0.1.0\nruntime: tool\nimplementation: native\n",
        )
        .unwrap();
        let wasm = Manifest::from_yaml_str("name: w\nversion: 0.1.0\nruntime: tool\n").unwrap();

        assert_eq!(skill.execution, None);
        assert_eq!(skill.registry_runtime(), RuntimeType::Static);
        assert_eq!(native.registry_runtime(), RuntimeType::Native);
        assert_eq!(wasm.registry_runtime(), RuntimeType::Wasm);
    }

    #[test]
    fn invalid_execution_value_is_rejected() {
        let err = Manifest::from_yaml_str(
            "name: my-tool\nversion: 0.1.0\nruntime: tool\nexecution: banana\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("field 'execution'"), "error was: {msg}");
        assert!(msg.contains("wasm, native, static"), "error was: {msg}");
        assert!(msg.contains("got banana"), "error was: {msg}");
    }

    #[test]
    fn non_string_execution_value_is_rejected() {
        let err = Manifest::from_yaml_str(
            "name: my-tool\nversion: 0.1.0\nruntime: tool\nexecution: 42\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("field 'execution'"));
    }

    #[test]
    fn requires_files_defaults_to_skill_md_for_skill_role() {
        let manifest =
            Manifest::from_yaml_str("name: s\nversion: 0.1.0\nruntime: skill\n").unwrap();
        assert_eq!(manifest.requires_files, vec!["skill.md".to_string()]);
    }

    #[test]
    fn requires_files_defaults_to_empty_for_other_roles() {
        let manifest = Manifest::from_yaml_str("name: t\nversion: 0.1.0\nruntime: tool\n").unwrap();
        assert!(manifest.requires_files.is_empty());
    }

    #[test]
    fn explicit_requires_files_wins_over_role_default() {
        let manifest = Manifest::from_yaml_str(
            "name: t\nversion: 0.1.0\nruntime: tool\nrequires_files:\n  - config.json\n",
        )
        .unwrap();
        assert_eq!(manifest.requires_files, vec!["config.json".to_string()]);
    }

    #[test]
    fn explicit_empty_requires_files_overrides_skill_default() {
        let manifest = Manifest::from_yaml_str(
            "name: s\nversion: 0.1.0\nruntime: skill\nrequires_files: []\n",
        )
        .unwrap();
        assert!(manifest.requires_files.is_empty());
    }

    #[test]
    fn non_sequence_requires_files_is_rejected() {
        let err = Manifest::from_yaml_str(
            "name: s\nversion: 0.1.0\nruntime: skill\nrequires_files: nope\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("field 'requires_files'"), "error was: {msg}");
        assert!(msg.contains("sequence of strings"), "error was: {msg}");
    }
}
