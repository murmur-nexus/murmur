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

                if is_secret_shaped_name(&key_name) {
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

/// Whether a name is credential-shaped: it contains one of [`SENSITIVE_KEYS`], case-insensitively.
///
/// Judges a name and never a value, which is what lets it serve both callers: the manifest scan
/// here, which asks it about a YAML key before looking at what that key holds, and
/// `capsule_runtime::secret_shaped_env_grants`, which asks it about a `capabilities.env.allow`
/// entry and must reach the same verdict on a host that has the variable set and one that does not.
/// Substring matching is deliberate — `DATABASE_PASSWORD` and `db_api_key_id` both match — because
/// a name this misses is a grant nothing warns about.
pub fn is_secret_shaped_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|s| normalized.contains(s))
}

/// Whole name segments that mark an environment variable as carrying a credential, compared
/// case-insensitively against the pieces of a name split on non-alphanumerics.
///
/// Matched as segments rather than substrings because env names are short and dense with
/// unrelated words: `KEY` as a substring would catch `KEYBOARD_LAYOUT` and `MONKEY_PATCH`, and
/// `PASS` would catch `PASSENGER_COUNT`. `PWD` is absent because it is the shell's working
/// directory, `PRIVATE` because of names like `PRIVATE_REGISTRY_HOST`, and `URL`, `CERT` and
/// `SESSION` because most names holding them carry no secret.
const ENV_CREDENTIAL_SEGMENTS: [&str; 12] = [
    "KEY",
    "KEYS",
    "PASS",
    "PASSWD",
    "PASSPHRASE",
    "CREDENTIAL",
    "CREDENTIALS",
    "CREDS",
    "DSN",
    "AUTH",
    "PAT",
    "COOKIE",
];

/// Whether an environment variable name looks like it carries a credential: it matches
/// [`is_secret_shaped_name`], or one of its segments is in [`ENV_CREDENTIAL_SEGMENTS`].
///
/// Judges a name and never a value. Wider than [`is_secret_shaped_name`] and kept apart from it
/// because the manifest literal-secret scan in [`scan_yaml_secrets`] reads YAML keys, where a
/// segment rule would flag keys such as `auth:` or `key:` that hold no literal secret. The
/// measured recall over a fixed corpus is pinned by
/// `env_name_recall_is_measured_over_a_fixed_corpus`; `DATABASE_URL`-style connection strings
/// are the known class it misses.
pub fn is_credential_shaped_env_name(name: &str) -> bool {
    is_secret_shaped_name(name)
        || name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|segment| {
                ENV_CREDENTIAL_SEGMENTS
                    .iter()
                    .any(|marker| segment.eq_ignore_ascii_case(marker))
            })
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

    /// Recall of the two name checks over one fixed corpus of env names. Each row is
    /// `(name, is_secret_shaped_name, is_credential_shaped_env_name)`; the totals are what
    /// `docs/content/reference/diagnostics.md#w-sec-024` publishes, so a change to either check
    /// fails here until the corpus and that page agree again.
    #[test]
    fn env_name_recall_is_measured_over_a_fixed_corpus() {
        const CARRIERS: [(&str, bool, bool); 26] = [
            ("DATABASE_PASSWORD", true, true),
            ("SLACK_BOT_TOKEN", true, true),
            ("JWT_SECRET", true, true),
            ("CLIENT_SECRET", true, true),
            ("STRIPE_API_KEY", true, true),
            ("AWS_SECRET_ACCESS_KEY", true, true),
            ("PRIVATE_KEY", false, true),
            ("SSH_KEY", false, true),
            ("SIGNING_KEY", false, true),
            ("ENCRYPTION_KEY", false, true),
            ("MASTER_KEY", false, true),
            ("CREDENTIALS", false, true),
            ("GOOGLE_APPLICATION_CREDENTIALS", false, true),
            ("DB_CREDS", false, true),
            ("SENTRY_DSN", false, true),
            ("DSN", false, true),
            ("SMTP_PASS", false, true),
            ("MYSQL_PASSWD", false, true),
            ("GPG_PASSPHRASE", false, true),
            ("BASIC_AUTH", false, true),
            ("GH_PAT", false, true),
            ("SESSION_COOKIE", false, true),
            // Known misses under both checks: a connection string or a shortened password name.
            ("DATABASE_URL", false, false),
            ("REDIS_URL", false, false),
            ("DB_PWD", false, false),
            ("CONNECTION_STRING", false, false),
        ];
        const ORDINARY: [&str; 16] = [
            "PATH",
            "HOME",
            "TZ",
            "LANG",
            "LOG_LEVEL",
            "NODE_ENV",
            "PORT",
            "DATABASE_HOST",
            "AUTHOR_NAME",
            "KEYBOARD_LAYOUT",
            "MONKEY_PATCH",
            "PWD",
            "OLDPWD",
            "SSL_CERT_FILE",
            "PASSENGER_COUNT",
            "COMPASS_URL",
        ];
        const FALSE_POSITIVES: [(&str, bool, bool); 2] = [
            ("TOKENIZERS_PARALLELISM", true, true),
            ("CACHE_KEY_PREFIX", false, true),
        ];

        for (name, old, new) in CARRIERS.iter().chain(FALSE_POSITIVES.iter()) {
            assert_eq!(is_secret_shaped_name(name), *old, "old check on {name}");
            assert_eq!(
                is_credential_shaped_env_name(name),
                *new,
                "new check on {name}"
            );
        }
        for name in ORDINARY {
            assert!(!is_credential_shaped_env_name(name), "new check on {name}");
        }

        let old_hits = CARRIERS
            .iter()
            .filter(|(name, _, _)| is_secret_shaped_name(name))
            .count();
        let new_hits = CARRIERS
            .iter()
            .filter(|(name, _, _)| is_credential_shaped_env_name(name))
            .count();
        assert_eq!((old_hits, new_hits), (6, 22));
    }

    /// The build-time literal scan keeps its own four-substring rule: a YAML key the env-name
    /// check would flag by segment, holding a long literal, is not a warning.
    #[test]
    fn scan_yaml_secrets_is_not_widened_by_the_env_name_check() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("murmur.yaml");
        fs::write(
            &path,
            "author: \"a-sufficiently-long-name\"\nprivate_key: \"a-long-literal-value\"\n",
        )
        .unwrap();

        assert!(scan_yaml_secrets(&path).unwrap().is_empty());
    }
}
