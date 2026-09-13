use clap::Subcommand;

use crate::{
    config::{
        is_literal_inference_api_key, is_valid_credential_name, load_mur_config,
        load_project_mur_config_if_exists, project_mur_config_path, save_mur_config,
        save_project_mur_config, MurConfig,
    },
    error::{CliError, E_CFG_002},
};

const SUPPORTED_KEYS: &[&str] = &[
    "registry.default",
    "registry.index_url",
    "inference.provider",
    "inference.model",
    "inference.api_key",
    "inference.endpoint",
    "credentials.<NAME>",
];

const CREDENTIALS_PREFIX: &str = "credentials.";

/// Printed whenever `inference.api_key` is set: `mur new` reads that scalar, and `mur run` does
/// not.
const INFERENCE_API_KEY_NOTE: &str =
    "note: mur run does not read inference.api_key; a manifest's `inference.api_key: ${NAME}` is \
     read from credentials.<NAME> — set it with `mur config set -g credentials.<NAME> <key>`";

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Set a config key. Writes the project-level file (<cwd>/.murmur/config.yaml) by
    /// default; use -g to write ~/.murmur/config.yaml instead.
    Set {
        /// Dotted config key (registry.default, registry.index_url, inference.provider,
        /// inference.model, inference.api_key, inference.endpoint, or credentials.<NAME> with -g)
        key: String,

        /// Value to write
        value: String,

        /// Write to the global config (~/.murmur/config.yaml) instead of the
        /// project-level file (<cwd>/.murmur/config.yaml)
        #[arg(short = 'g', long)]
        global: bool,
    },
}

pub fn run_config(command: &ConfigCommand) -> Result<(), CliError> {
    match command {
        ConfigCommand::Set { key, value, global } => run_config_set(key, value, *global),
    }
}

/// A settable key: one of [`SUPPORTED_KEYS`]' scalars, or one entry of the `credentials:` map.
enum ConfigKey<'a> {
    Scalar(&'a str),
    Credential(&'a str),
}

fn parse_key(key: &str, global: bool) -> Result<ConfigKey<'_>, CliError> {
    if let Some(name) = key.strip_prefix(CREDENTIALS_PREFIX) {
        if !is_valid_credential_name(name) {
            return Err(CliError::with_hint(
                E_CFG_002,
                format!("unsupported config key '{key}'"),
                "a credential name is uppercase letters, digits and underscores, starting with a \
                 letter or underscore — the NAME a manifest writes as `inference.api_key: ${NAME}`",
            ));
        }
        if !global {
            return Err(CliError::with_hint(
                E_CFG_002,
                format!("'{key}' can only be set in the global config"),
                format!(
                    "credentials are read from the global config (~/.murmur/config.yaml) only; \
                     run `mur config set -g {key} <key>`"
                ),
            ));
        }
        return Ok(ConfigKey::Credential(name));
    }
    if !SUPPORTED_KEYS.contains(&key) {
        return Err(CliError::with_hint(
            E_CFG_002,
            format!("unsupported config key '{key}'"),
            format!("supported keys: {}", SUPPORTED_KEYS.join(", ")),
        ));
    }
    Ok(ConfigKey::Scalar(key))
}

fn run_config_set(key: &str, value: &str, global: bool) -> Result<(), CliError> {
    let target = parse_key(key, global)?;

    if key == "inference.api_key" {
        eprintln!("{INFERENCE_API_KEY_NOTE}");
        if !global && is_literal_inference_api_key(value) {
            let path = project_mur_config_path()?;
            eprintln!(
                "warning: writing a literal inference.api_key to {} has no effect; \
                 inference.api_key is always read from the global config \
                 (~/.murmur/config.yaml) — this project-level value will be ignored when \
                 resolving effective config",
                path.display()
            );
        }
    }

    let mut config = if global {
        load_mur_config()?
    } else {
        load_project_mur_config_if_exists()?.unwrap_or_default()
    };

    apply_key(&mut config, &target, value);

    if global {
        save_mur_config(&config)?;
    } else {
        save_project_mur_config(&config)?;
    }
    println!("{}", set_message(key, global)?);

    Ok(())
}

/// The confirmation `mur config set` prints. Names the key and the file, never the value.
fn set_message(key: &str, global: bool) -> Result<String, CliError> {
    Ok(if global {
        format!("Set {key} in ~/.murmur/config.yaml")
    } else {
        format!("Set {key} in {}", project_mur_config_path()?.display())
    })
}

fn apply_key(config: &mut MurConfig, key: &ConfigKey<'_>, value: &str) {
    let key = match key {
        ConfigKey::Credential(name) => {
            config
                .credentials
                .insert((*name).to_string(), value.to_string());
            return;
        }
        ConfigKey::Scalar(key) => *key,
    };
    match key {
        "registry.default" => config.registry.default = Some(value.to_string()),
        "registry.index_url" => config.registry.index_url = Some(value.to_string()),
        "inference.provider" | "inference.model" | "inference.api_key" | "inference.endpoint" => {
            let inference = config.inference.get_or_insert_with(Default::default);
            match key {
                "inference.provider" => inference.provider = value.to_string(),
                "inference.model" => inference.model = value.to_string(),
                "inference.api_key" => inference.api_key = value.to_string(),
                "inference.endpoint" => inference.endpoint = value.to_string(),
                _ => unreachable!(),
            }
        }
        _ => unreachable!("SUPPORTED_KEYS should have rejected this key"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HOME_ENV_LOCK;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved_home: Option<std::ffi::OsString>,
        saved_cwd: std::path::PathBuf,
    }

    impl EnvGuard {
        fn set_up(home: &std::path::Path, cwd: &std::path::Path) -> Self {
            let lock = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved_home = std::env::var_os("HOME");
            let saved_cwd = std::env::current_dir().expect("cwd");
            // SAFETY: serialized by HOME_ENV_LOCK across this module's tests.
            unsafe {
                std::env::set_var("HOME", home);
            }
            std::env::set_current_dir(cwd).expect("set cwd");
            Self {
                _lock: lock,
                saved_home,
                saved_cwd,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: still holding `_lock`.
            unsafe {
                match &self.saved_home {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
            let _ = std::env::set_current_dir(&self.saved_cwd);
        }
    }

    #[test]
    fn set_rejects_unknown_key() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        let err = run_config_set("nonsense.field", "value", false).expect_err("should reject");
        assert_eq!(err.code, E_CFG_002);
        assert!(!cwd.path().join(".murmur").join("config.yaml").exists());
    }

    #[test]
    fn set_writes_project_file_by_default() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        run_config_set("registry.default", "local", false).expect("set should succeed");

        assert!(cwd.path().join(".murmur").join("config.yaml").exists());
        assert!(!home.path().join(".murmur").join("config.yaml").exists());

        let cfg = load_project_mur_config_if_exists()
            .expect("load")
            .expect("project config should exist");
        assert_eq!(cfg.registry.default.as_deref(), Some("local"));
    }

    #[test]
    fn set_writes_global_file_with_g_flag() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        run_config_set("registry.default", "local", true).expect("set should succeed");

        assert!(home.path().join(".murmur").join("config.yaml").exists());
        assert!(!cwd.path().join(".murmur").join("config.yaml").exists());
    }

    #[test]
    fn set_does_not_clobber_unrelated_keys() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        run_config_set("registry.default", "local", false).expect("first set");
        run_config_set("inference.model", "claude-3-5-sonnet", false).expect("second set");

        let cfg = load_project_mur_config_if_exists()
            .expect("load")
            .expect("project config should exist");
        assert_eq!(cfg.registry.default.as_deref(), Some("local"));
        assert_eq!(
            cfg.inference.as_ref().map(|i| i.model.as_str()),
            Some("claude-3-5-sonnet")
        );
    }

    #[test]
    fn credentials_set_writes_the_global_map_and_keeps_every_other_key() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        run_config_set("registry.default", "local", true).expect("scalar");
        run_config_set("inference.model", "some-model", true).expect("scalar");
        run_config_set("credentials.PROVIDER_KEY", "first-value", true).expect("credential");
        run_config_set("credentials.OTHER_KEY", "other-value", true).expect("credential");
        run_config_set("credentials.PROVIDER_KEY", "second-value", true).expect("replace");

        let written =
            std::fs::read_to_string(home.path().join(".murmur").join("config.yaml")).unwrap();
        assert!(written.contains("credentials:"), "{written}");
        let cfg = load_mur_config().expect("load");
        assert_eq!(cfg.registry.default.as_deref(), Some("local"));
        assert_eq!(
            cfg.inference.as_ref().map(|i| i.model.as_str()),
            Some("some-model")
        );
        assert_eq!(cfg.credentials.len(), 2);
        assert_eq!(cfg.credentials["PROVIDER_KEY"], "second-value");
        assert_eq!(cfg.credentials["OTHER_KEY"], "other-value");
        assert!(!cwd.path().join(".murmur").exists());
    }

    #[test]
    fn credentials_set_message_names_the_key_and_never_the_value() {
        let message = set_message("credentials.PROVIDER_KEY", true).unwrap();
        assert_eq!(
            message,
            "Set credentials.PROVIDER_KEY in ~/.murmur/config.yaml"
        );
    }

    #[test]
    fn credentials_without_global_are_refused_with_a_hint() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        let err = run_config_set("credentials.PROVIDER_KEY", "a-value", false)
            .expect_err("project-level credentials are refused");
        assert_eq!(err.code, E_CFG_002);
        let hint = err.hint.clone().unwrap_or_default();
        assert!(hint.contains("global config"), "{hint}");
        assert!(!err.to_string().contains("a-value"), "{err}");
        assert!(!cwd.path().join(".murmur").exists());
        assert!(!home.path().join(".murmur").exists());
    }

    #[test]
    fn credentials_name_outside_the_grammar_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let _guard = EnvGuard::set_up(home.path(), cwd.path());

        for key in [
            "credentials.lower_key",
            "credentials.1LEADING_DIGIT",
            "credentials.",
            "credentials.HAS-DASH",
            "credentials.A.B",
        ] {
            let err = run_config_set(key, "a-value", true).expect_err(key);
            assert_eq!(err.code, E_CFG_002, "{key}");
        }
        assert!(!home.path().join(".murmur").exists());
    }

    #[test]
    fn credentials_note_is_printed_for_inference_api_key() {
        assert!(INFERENCE_API_KEY_NOTE.contains("credentials.<NAME>"));
        assert!(!INFERENCE_API_KEY_NOTE.contains('\n'));
        assert!(SUPPORTED_KEYS.contains(&"credentials.<NAME>"));
    }
}
