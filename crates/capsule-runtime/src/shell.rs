use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::types::CapabilityPolicy;

const MAX_SHELL_OUTPUT_BYTES: usize = 16 * 1024;

const SHELL_INTERPRETERS: &[&str] = &["bash", "sh", "zsh", "fish", "dash", "ksh"];
const DEFAULT_ENV_BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "TERM",
];

/// Directory name for the per-session synthetic home, created under the capsule's
/// session workdir. Planned /proc and subprocess key isolation work
/// depend on this exact name/location convention.
const SYNTHETIC_HOME_DIR_NAME: &str = ".capsule-home";

/// Credential-shaped env var patterns stripped from every subprocess, regardless of
/// whether they came from the host process env, `env_overrides`, or
/// `policy.shell_baseline_env`. Supports exact match, trailing `*` (prefix) and
/// leading `*` (suffix) via `env_name_matches_pattern`.
const CREDENTIAL_ENV_PATTERNS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GOOGLE_API_KEY",
    "HUGGING_FACE_HUB_TOKEN",
    "NEXUS_API_KEY",
    "AWS_*",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "DOCKER_*",
    "KUBECONFIG",
    "NPM_TOKEN",
    "PYPI_TOKEN",
    "CARGO_REGISTRY_TOKEN",
    "*_API_KEY",
];

#[derive(Debug)]
pub(crate) struct ShellResult {
    pub(crate) exit_code: i32,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) duration_ms: u64,
    pub(crate) truncated: bool,
    pub(crate) full_output_path: Option<String>,
}

pub(crate) fn is_shell_interpreter(binary: &str) -> bool {
    SHELL_INTERPRETERS.contains(&binary)
}

pub(crate) fn split_shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match (ch, quote) {
            ('"' | '\'', None) => quote = Some(ch),
            (q, Some(open)) if q == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        words.push(current);
    }
    words
}

pub(crate) fn shell_tool_manifest_yaml(binary: &str) -> String {
    let description = if is_shell_interpreter(binary) {
        format!("Run a shell command via {binary} in the capsule workdir. The `command` field is passed to {binary} via -c.")
    } else {
        format!("Run {binary} in the capsule workdir. The `command` field is the argument list — omit the binary name itself (pass -s http://example.com, not {binary} -s http://example.com).")
    };

    format!(
        "name: {binary}\nversion: 0.0.0\nruntime: tool\nimplementation: native\ndescription: \"{description}\"\ninput_schema: '{{\"type\":\"object\",\"properties\":{{\"command\":{{\"type\":\"string\"}}}},\"required\":[\"command\"]}}'\n"
    )
}

pub(crate) fn execute_shell(
    binary: &str,
    args: &[&str],
    env_overrides: &[(String, String)],
    workdir: &Path,
    policy: &CapabilityPolicy,
    enforcement: &crate::sandbox::ShellEnforcement,
) -> Result<ShellResult, String> {
    if !policy.shell_allow.iter().any(|allowed| allowed == binary) {
        return Err(format!(
            "binary '{binary}' is not in capabilities.shell.allow"
        ));
    }

    let env = build_shell_env(policy, env_overrides, workdir)?;

    let started = Instant::now();
    let mut command = Command::new(binary);
    command
        .args(args)
        .current_dir(workdir)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Fail-closed: if kernel enforcement setup fails unexpectedly, propagate the error and
    // never call `.spawn()` at all — no code path here lets a Linux host silently run this
    // subprocess with zero enforcement because setup failed.
    let supervisor = crate::sandbox::prepare_enforcement(&mut command, enforcement, workdir)?;

    let child = command.spawn().map_err(|error| error.to_string())?;
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    supervisor.join_best_effort();
    let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut truncated = false;
    let mut full_output_path = None;

    if output.stdout.len() > MAX_SHELL_OUTPUT_BYTES || output.stderr.len() > MAX_SHELL_OUTPUT_BYTES
    {
        truncated = true;
        let stdout_cut = output.stdout.len().min(MAX_SHELL_OUTPUT_BYTES);
        let stderr_cut = output.stderr.len().min(MAX_SHELL_OUTPUT_BYTES);
        stdout = String::from_utf8_lossy(&output.stdout[..stdout_cut]).to_string();
        stderr = String::from_utf8_lossy(&output.stderr[..stderr_cut]).to_string();
        full_output_path = Some(write_full_shell_output_log(
            workdir,
            binary,
            args,
            output.status.code().unwrap_or(-1),
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )?);
    }

    Ok(ShellResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout,
        stderr,
        duration_ms,
        truncated,
        full_output_path,
    })
}

pub(crate) fn build_shell_env(
    policy: &CapabilityPolicy,
    env_overrides: &[(String, String)],
    workdir: &Path,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();

    for key in DEFAULT_ENV_BASELINE {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), value);
        }
    }

    for (key, value) in env_overrides {
        env.insert(key.clone(), value.clone());
    }

    for key in &policy.shell_baseline_env {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    // Inserted last so neither a guest-supplied `env_overrides` entry nor a
    // manifest-declared `shell_baseline_env` entry can resurrect the real host
    // HOME/USERPROFILE, and so no strip_env pattern can remove the synthetic one.
    let synthetic_home = synthetic_home_dir(workdir)?;
    let synthetic_home = synthetic_home.to_string_lossy().into_owned();
    env.insert("HOME".to_string(), synthetic_home.clone());
    env.insert("USERPROFILE".to_string(), synthetic_home);

    Ok(env)
}

/// Drop every entry whose name matches [`CREDENTIAL_ENV_PATTERNS`] or one of
/// `extra_patterns` (a policy's `shell_strip_env`).
///
/// This is the single credential backstop for both environments the runtime builds: the
/// native subprocess env ([`build_shell_env`]) and the WASI guest env
/// ([`build_wasi_env_allowlist`]). It runs *after* whatever allowlist populated `env`, so a
/// declared name never bypasses the filter.
pub(crate) fn strip_credential_shaped_vars(
    env: &mut BTreeMap<String, String>,
    extra_patterns: &[String],
) {
    env.retain(|key, _| {
        !CREDENTIAL_ENV_PATTERNS
            .iter()
            .copied()
            .chain(extra_patterns.iter().map(String::as_str))
            .any(|pattern| env_name_matches_pattern(pattern, key))
    });
}

/// Resolve the host variables a WASM guest may observe: only names the manifest declared in
/// `capabilities.env.allow` and that the host actually has set, minus anything
/// credential-shaped. A declared name absent from the host is simply omitted, not an error.
pub(crate) fn build_wasi_env_allowlist(policy: &CapabilityPolicy) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();

    for key in &policy.env_allow {
        if let Ok(value) = std::env::var(key) {
            env.insert(key.clone(), value);
        }
    }

    strip_credential_shaped_vars(&mut env, &policy.shell_strip_env);

    env
}

fn synthetic_home_dir(workdir: &Path) -> Result<std::path::PathBuf, String> {
    let home = workdir.join(SYNTHETIC_HOME_DIR_NAME);
    fs::create_dir_all(&home).map_err(|error| {
        format!(
            "failed to create synthetic home directory {}: {error}",
            home.display()
        )
    })?;
    Ok(home)
}

fn env_name_matches_pattern(pattern: &str, key: &str) -> bool {
    if let Some(rest) = pattern.strip_prefix('*') {
        if let Some(middle) = rest.strip_suffix('*') {
            return key.contains(middle);
        }
        return key.ends_with(rest);
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        return key.starts_with(prefix);
    }

    pattern == key
}

fn write_full_shell_output_log(
    workdir: &Path,
    binary: &str,
    args: &[&str],
    exit_code: i32,
    stdout: &str,
    stderr: &str,
) -> Result<String, String> {
    let logs_dir = workdir.join("logs");
    fs::create_dir_all(&logs_dir)
        .map_err(|error| format!("failed to create shell logs directory: {error}"))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let filename = format!("shell-{timestamp}.log");
    let relative_path = format!("logs/{filename}");
    let path = logs_dir.join(&filename);

    let command = if args.is_empty() {
        binary.to_string()
    } else {
        format!("{binary} {}", args.join(" "))
    };

    let content = format!(
        "Command: {command}\nExit code: {exit_code}\n\nStdout:\n{stdout}\n\nStderr:\n{stderr}\n"
    );

    fs::write(&path, content)
        .map_err(|error| format!("failed to write shell log {}: {error}", path.display()))?;

    Ok(relative_path)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::sandbox::ShellEnforcement;

    #[test]
    fn execute_shell_blocks_binary_not_in_allowlist() {
        let policy = CapabilityPolicy::default();
        let error = execute_shell(
            "bash",
            &["-c", "echo hi"],
            &[],
            Path::new("."),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap_err();

        assert!(error.contains("capabilities.shell.allow"));
    }

    #[test]
    fn execute_shell_returns_nonzero_exit_code_without_error() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "exit 42"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 42);
    }

    #[test]
    fn build_shell_env_sets_synthetic_home_under_workdir() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        let env = build_shell_env(&policy, &[], temp.path()).unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(
            env.get("HOME"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("USERPROFILE"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
        assert!(expected_home.is_dir());
    }

    #[test]
    fn build_shell_env_reports_synthetic_home_creation_failure() {
        let temp = tempdir().unwrap();
        let blocking_file = temp.path().join("blocked");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        let workdir = blocking_file.join("subdir");
        let policy = CapabilityPolicy::default();

        let error = build_shell_env(&policy, &[], &workdir).unwrap_err();

        let expected_home = workdir.join(".capsule-home");
        assert!(error.contains(&expected_home.to_string_lossy().into_owned()));
    }

    #[test]
    fn execute_shell_does_not_spawn_when_synthetic_home_creation_fails() {
        let temp = tempdir().unwrap();
        let blocking_file = temp.path().join("blocked");
        std::fs::write(&blocking_file, b"not a directory").unwrap();
        let workdir = blocking_file.join("subdir");
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let error = execute_shell(
            "bash",
            &["-c", "echo should-not-run"],
            &[],
            &workdir,
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap_err();

        assert!(error.contains(".capsule-home"));
    }

    #[test]
    fn execute_shell_reports_synthetic_home_not_real_host_home() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };

        let result = execute_shell(
            "bash",
            &["-c", "echo -n $HOME"],
            &[],
            temp.path(),
            &policy,
            &ShellEnforcement::environment_only(),
        )
        .unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(result.stdout, expected_home.to_string_lossy());
        if let Ok(real_home) = std::env::var("HOME") {
            assert_ne!(result.stdout, real_home);
        }
    }

    #[test]
    fn build_shell_env_strips_wildcard_credential_patterns_from_overrides() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();
        let overrides = vec![
            ("AWS_ACCESS_KEY_ID".to_string(), "leaked".to_string()),
            ("DOCKER_AUTH_CONFIG".to_string(), "leaked".to_string()),
            ("STRIPE_API_KEY".to_string(), "leaked".to_string()),
            ("GITHUB_TOKEN".to_string(), "leaked".to_string()),
            ("SAFE_VAR".to_string(), "kept".to_string()),
        ];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        assert!(!env.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(!env.contains_key("DOCKER_AUTH_CONFIG"));
        assert!(!env.contains_key("STRIPE_API_KEY"));
        assert!(!env.contains_key("GITHUB_TOKEN"));
        assert_eq!(env.get("SAFE_VAR"), Some(&"kept".to_string()));
    }

    #[test]
    fn build_wasi_env_allowlist_is_empty_without_declarations() {
        std::env::set_var("MURMUR_TEST_WASI_UNDECLARED", "host-value");
        let policy = CapabilityPolicy::default();

        let env = build_wasi_env_allowlist(&policy);

        assert!(env.is_empty());
    }

    #[test]
    fn build_wasi_env_allowlist_passes_through_declared_host_var() {
        std::env::set_var("MURMUR_TEST_WASI_ALLOWED", "host-value");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "MURMUR_TEST_WASI_ALLOWED".to_string(),
                // Declared but unset on the host: omitted, not an error.
                "MURMUR_TEST_WASI_NEVER_SET".to_string(),
            ],
            ..CapabilityPolicy::default()
        };

        let env = build_wasi_env_allowlist(&policy);

        assert_eq!(
            env.get("MURMUR_TEST_WASI_ALLOWED"),
            Some(&"host-value".to_string())
        );
        assert!(!env.contains_key("MURMUR_TEST_WASI_NEVER_SET"));
    }

    #[test]
    fn build_wasi_env_allowlist_strips_credential_shaped_declarations() {
        std::env::set_var("GITHUB_TOKEN", "leaked-token");
        std::env::set_var("STRIPE_API_KEY", "leaked-key");
        std::env::set_var("MURMUR_TEST_WASI_CUSTOM_SECRET", "leaked-secret");
        let policy = CapabilityPolicy {
            env_allow: vec![
                "GITHUB_TOKEN".to_string(),
                "STRIPE_API_KEY".to_string(),
                "MURMUR_TEST_WASI_CUSTOM_SECRET".to_string(),
            ],
            shell_strip_env: vec!["*_CUSTOM_SECRET".to_string()],
            ..CapabilityPolicy::default()
        };

        let env = build_wasi_env_allowlist(&policy);

        // Declaring a credential-shaped name does not bypass the backstop.
        assert!(env.is_empty(), "expected all names stripped, got {env:?}");
    }

    #[test]
    fn build_shell_env_home_override_cannot_survive() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_baseline_env: vec!["HOME".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("HOME".to_string(), "/tmp/guest-controlled".to_string())];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        let expected_home = temp.path().join(".capsule-home");
        assert_eq!(
            env.get("HOME"),
            Some(&expected_home.to_string_lossy().into_owned())
        );
    }

    #[test]
    fn build_shell_env_keeps_safe_baseline_vars() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy::default();

        std::env::set_var("CARGO_HOME", "/fake/cargo/home");
        let env = build_shell_env(&policy, &[], temp.path()).unwrap();
        std::env::remove_var("CARGO_HOME");

        assert_eq!(env.get("CARGO_HOME"), Some(&"/fake/cargo/home".to_string()));
    }

    #[test]
    fn build_shell_env_developer_declared_strip_env_still_composes() {
        let temp = tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_strip_env: vec!["MYCOMPANY_*".to_string()],
            ..CapabilityPolicy::default()
        };
        let overrides = vec![("MYCOMPANY_SECRET".to_string(), "leaked".to_string())];

        let env = build_shell_env(&policy, &overrides, temp.path()).unwrap();

        assert!(!env.contains_key("MYCOMPANY_SECRET"));
    }

    #[test]
    fn env_name_matches_pattern_trailing_wildcard() {
        assert!(env_name_matches_pattern("AWS_*", "AWS_ACCESS_KEY_ID"));
        assert!(!env_name_matches_pattern("AWS_*", "MY_AWS_KEY"));
    }

    #[test]
    fn env_name_matches_pattern_leading_wildcard() {
        assert!(env_name_matches_pattern("*_API_KEY", "STRIPE_API_KEY"));
        assert!(!env_name_matches_pattern("*_API_KEY", "API_KEY_ID"));
    }

    #[test]
    fn env_name_matches_pattern_exact_match() {
        assert!(env_name_matches_pattern("GITHUB_TOKEN", "GITHUB_TOKEN"));
        assert!(!env_name_matches_pattern("GITHUB_TOKEN", "GITHUB_TOKEN_2"));
    }

    #[test]
    fn shell_tool_manifest_yaml_is_valid_yaml_for_interpreter() {
        let yaml = shell_tool_manifest_yaml("bash");
        serde_yaml::from_str::<serde_yaml::Value>(&yaml)
            .expect("bash manifest must be valid YAML");
    }

    #[test]
    fn shell_tool_manifest_yaml_is_valid_yaml_for_non_interpreter() {
        let yaml = shell_tool_manifest_yaml("curl");
        serde_yaml::from_str::<serde_yaml::Value>(&yaml)
            .expect("curl manifest must be valid YAML — embedded quotes break serde_yaml parsing");
    }

    #[test]
    fn shell_tool_manifest_yaml_non_interpreter_has_tool_runtime() {
        let yaml = shell_tool_manifest_yaml("curl");
        let value: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            value.get("runtime").and_then(|v| v.as_str()),
            Some("tool"),
            "non-interpreter manifest must have runtime: tool so inventory picks it up"
        );
    }
}
