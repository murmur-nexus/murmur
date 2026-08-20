use clap::Subcommand;

use crate::{
    beta::compiled_beta_features,
    config::{load_effective_mur_config, load_mur_config, save_mur_config},
    error::CliError,
};

#[derive(Debug, Subcommand)]
pub enum BetaCommand {
    /// List beta features compiled into this build and their enabled status
    List,
    /// Enable a beta feature
    Enable {
        /// Feature name
        feature: String,
    },
    /// Disable a beta feature
    Disable {
        /// Feature name
        feature: String,
    },
}

pub fn run_beta(command: &BetaCommand) -> Result<(), CliError> {
    match command {
        BetaCommand::List => run_beta_list(),
        BetaCommand::Enable { feature } => run_beta_enable(feature),
        BetaCommand::Disable { feature } => run_beta_disable(feature),
    }
}

fn run_beta_list() -> Result<(), CliError> {
    let config = load_effective_mur_config()?;
    let features = compiled_beta_features();

    if features.is_empty() {
        println!("This build has no beta features.");
        return Ok(());
    }

    println!("Beta features compiled into this build:\n");
    for f in &features {
        let status = if config.beta.is_enabled(f.name) {
            "enabled "
        } else {
            "disabled"
        };
        println!("  {:<20} {}  {}", f.name, status, f.description);
    }
    println!("\nUse `mur beta enable <name>` or `mur beta disable <name>` to opt in or out.");
    Ok(())
}

fn run_beta_enable(feature: &str) -> Result<(), CliError> {
    let mut config = load_mur_config()?;
    let known = compiled_beta_features();
    let is_known = known.iter().any(|f| f.name == feature);

    if !is_known {
        eprintln!(
            "Warning: '{}' is not compiled into this build. \
             The flag will be saved but has no effect until a build that includes it is installed.",
            feature
        );
    }

    if config.beta.enable(feature) {
        save_mur_config(&config)?;
        println!("Beta feature '{}' enabled.", feature);
    } else {
        println!("Beta feature '{}' is already enabled.", feature);
    }
    Ok(())
}

fn run_beta_disable(feature: &str) -> Result<(), CliError> {
    let mut config = load_mur_config()?;

    if config.beta.disable(feature) {
        save_mur_config(&config)?;
        println!("Beta feature '{}' disabled.", feature);
    } else {
        println!("Beta feature '{}' is already disabled.", feature);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BetaConfig;

    #[test]
    fn beta_config_enable_is_idempotent() {
        let mut cfg = BetaConfig::default();
        assert!(cfg.enable("x"));
        assert!(!cfg.enable("x"));
        assert_eq!(cfg.enabled.len(), 1);
    }

    #[test]
    fn beta_config_disable_is_idempotent() {
        let mut cfg = BetaConfig::default();
        assert!(!cfg.disable("x"));
    }

    #[test]
    fn beta_config_enable_then_disable() {
        let mut cfg = BetaConfig::default();
        cfg.enable("x");
        assert!(cfg.is_enabled("x"));
        cfg.disable("x");
        assert!(!cfg.is_enabled("x"));
    }

    #[test]
    fn beta_config_roundtrips_via_yaml() {
        use crate::config::MurConfig;
        let mut cfg = MurConfig::default();
        cfg.beta.enabled = vec!["foo".to_string(), "bar".to_string()];
        let s = serde_yaml::to_string(&cfg).unwrap();
        assert!(s.contains("beta:"), "expected beta: key in YAML output");
        assert!(
            s.contains("foo") && s.contains("bar"),
            "expected enabled list"
        );
        let back: MurConfig = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back.beta.enabled, vec!["foo", "bar"]);
    }

    #[test]
    fn mur_config_beta_field_defaults_when_absent() {
        use crate::config::MurConfig;
        let yaml_str = r#"
registry:
  default: official
  sources:
    - name: official
      type: github
      repo: example/artifacts
inference:
  provider: anthropic
  model: claude-3-5-sonnet-20241022
  api_key: sk-test
  endpoint: ""
"#;
        let cfg: MurConfig = serde_yaml::from_str(yaml_str).unwrap();
        assert!(cfg.beta.enabled.is_empty());
        assert_eq!(
            cfg.inference.as_ref().map(|i| i.provider.as_str()),
            Some("anthropic")
        );
    }

    #[cfg(not(any(
        feature = "beta-mur-new",
        feature = "beta-mur-deploy",
        feature = "beta-mur-topology"
    )))]
    #[test]
    fn compiled_beta_features_returns_empty_in_stable_build() {
        let features = compiled_beta_features();
        assert!(features.is_empty());
    }

    #[cfg(not(feature = "beta-mur-new"))]
    #[test]
    fn beta_mur_new_not_registered_without_feature() {
        let features = compiled_beta_features();
        assert!(!features.iter().any(|f| f.name == "mur-new"));
    }

    #[cfg(feature = "beta-mur-new")]
    #[test]
    fn beta_mur_new_registered_with_feature() {
        let features = compiled_beta_features();
        let entries: Vec<_> = features.iter().filter(|f| f.name == "mur-new").collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "mur-new");
    }

    #[cfg(not(feature = "beta-mur-deploy"))]
    #[test]
    fn beta_mur_deploy_not_registered_without_feature() {
        let features = compiled_beta_features();
        assert!(!features.iter().any(|f| f.name == "mur-deploy"));
    }

    #[cfg(feature = "beta-mur-deploy")]
    #[test]
    fn beta_mur_deploy_registered_with_feature() {
        let features = compiled_beta_features();
        let entries: Vec<_> = features.iter().filter(|f| f.name == "mur-deploy").collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "mur-deploy");
    }

    #[cfg(not(feature = "beta-mur-topology"))]
    #[test]
    fn beta_mur_topology_not_registered_without_feature() {
        let features = compiled_beta_features();
        assert!(!features.iter().any(|f| f.name == "mur-topology"));
    }

    #[cfg(feature = "beta-mur-topology")]
    #[test]
    fn beta_mur_topology_registered_with_feature() {
        let features = compiled_beta_features();
        let entries: Vec<_> = features
            .iter()
            .filter(|f| f.name == "mur-topology")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "mur-topology");
    }
}
