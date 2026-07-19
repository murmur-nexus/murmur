use std::{fs, path::Path};

use serde_yaml::Value;

use crate::manifest::ManifestError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretWarning {
    pub file: String,
    pub field_path: String,
    pub value_hint: String,
}

const SENSITIVE_KEYS: [&str; 4] = ["api_key", "token", "secret", "password"];
const KNOWN_SECRET_PREFIXES: [&str; 2] = ["sk-ant-", "sk-"];
const MIN_LITERAL_SECRET_LEN: usize = 8;

#[must_use = "warnings should be surfaced to users before packaging"]
pub fn scan_yaml_secrets(path: &Path) -> Result<Vec<SecretWarning>, ManifestError> {
    let content = fs::read_to_string(path).map_err(|source| ManifestError::Io {
        path: path.display().to_string(),
        source,
    })?;

    let value: Value = serde_yaml::from_str(&content).map_err(|err| {
        if let Some(location) = err.location() {
            ManifestError::YamlSyntax(format!(
                "{}: YAML syntax error at line {}, column {}: {}",
                path.display(),
                location.line(),
                location.column(),
                err
            ))
        } else {
            ManifestError::YamlSyntax(format!("{}: YAML syntax error: {}", path.display(), err))
        }
    })?;

    let mut warnings = Vec::new();
    visit_value(
        &value,
        &mut Vec::new(),
        &path.display().to_string(),
        &mut warnings,
    );

    Ok(warnings)
}

fn visit_value(
    value: &Value,
    path: &mut Vec<String>,
    file: &str,
    warnings: &mut Vec<SecretWarning>,
) {
    match value {
        Value::Mapping(map) => {
            for (key, child) in map {
                let key_name = yaml_key_to_string(key);
                path.push(key_name.clone());

                if is_sensitive_key(&key_name) {
                    if let Some(str_value) = child.as_str() {
                        if should_warn_secret_value(str_value) {
                            warnings.push(SecretWarning {
                                file: file.to_string(),
                                field_path: path.join("."),
                                value_hint: mask_value(str_value),
                            });
                        }
                    }
                }

                visit_value(child, path, file, warnings);
                path.pop();
            }
        }
        Value::Sequence(seq) => {
            for (idx, item) in seq.iter().enumerate() {
                path.push(idx.to_string());
                visit_value(item, path, file, warnings);
                path.pop();
            }
        }
        _ => {}
    }
}

fn yaml_key_to_string(key: &Value) -> String {
    match key {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        _ => "<complex key>".to_string(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|s| normalized.contains(s))
}

fn should_warn_secret_value(value: &str) -> bool {
    if is_env_reference(value) {
        return false;
    }

    KNOWN_SECRET_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
        || value.len() > MIN_LITERAL_SECRET_LEN
}

fn is_env_reference(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 4 || !value.starts_with("${") || !value.ends_with('}') {
        return false;
    }

    value[2..value.len() - 1]
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn mask_value(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        return "****".to_string();
    }

    let visible: String = chars[..4].iter().collect();
    format!("{}****", visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn literal_sensitive_value_warns() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("murmur.yaml");
        // Key assembled at runtime so the source never contains a
        // credential-shaped literal that secret scanners could flag.
        let key = ["sk-", "ant-", "supersecret"].concat();
        fs::write(
            &path,
            format!("name: demo\nversion: 0.0.1\napi_key: {key}\n"),
        )
        .unwrap();

        let warnings = scan_yaml_secrets(&path).unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].field_path, "api_key");
    }

    #[test]
    fn env_reference_does_not_warn() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("murmur.yaml");
        fs::write(&path, "name: demo\nversion: 0.0.1\napi_key: ${API_KEY}\n").unwrap();

        let warnings = scan_yaml_secrets(&path).unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn nested_paths_are_reported() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("murmur.yaml");
        fs::write(
            &path,
            "name: demo\nversion: 0.0.1\nconfig:\n  creds:\n    password: hunter2hunter2\n",
        )
        .unwrap();

        let warnings = scan_yaml_secrets(&path).unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].field_path, "config.creds.password");
    }
}
