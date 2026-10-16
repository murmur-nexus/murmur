/// A beta feature compiled into this build.
pub struct BetaFeature {
    pub name: &'static str,
    pub description: &'static str,
}

/// Returns all beta features compiled into this binary.
/// Each feature must add itself here under its own `#[cfg(feature = "beta-<name>")]` guard.
///
/// Example (add this block when introducing beta-blueprint):
///
/// ```rust,ignore
/// #[cfg(feature = "beta-blueprint")]
/// features.push(BetaFeature {
///     name: "blueprint",
///     description: "Blueprint file support in taskflow stage slots (Fleet v1.1 preview)",
/// });
/// ```
// Each entry below is `#[cfg]`-gated on its own feature, so the list cannot be written as a
// `vec![]` literal; with every feature enabled at once clippy sees only the resulting run of
// unconditional pushes. The push-per-feature shape is also what this module's doc comment tells
// the next author to follow when registering a beta feature.
#[allow(clippy::vec_init_then_push)]
pub fn compiled_beta_features() -> Vec<BetaFeature> {
    #[allow(unused_mut)]
    let mut features: Vec<BetaFeature> = Vec::new();

    // ── beta features register here ──────────────────────────────────────
    #[cfg(feature = "beta-mur-new")]
    features.push(BetaFeature {
        name: "mur-new",
        description: "LLM-powered manifest generation from a task description",
    });

    #[cfg(feature = "beta-mur-deploy")]
    features.push(BetaFeature {
        name: "mur-deploy",
        description: "Cloud deployment to Hetzner, AWS, or DigitalOcean (A2A endpoints are unauthenticated until v2.0.0)",
    });

    #[cfg(feature = "beta-mur-topology")]
    features.push(BetaFeature {
        name: "mur-topology",
        description: "Capsule session graph viewer — requires a running Grafana Tempo OTel backend",
    });
    // ─────────────────────────────────────────────────────────────────────

    features
}

#[cfg(test)]
mod tests {
    /// The `[features]` table of `crates/murmur-cli/Cargo.toml` as `(name, enabled features)`,
    /// one entry per `name = [ … ]` line; comments and other tables are skipped.
    fn cargo_features() -> Vec<(String, Vec<String>)> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let text = std::fs::read_to_string(path).expect("read murmur-cli's Cargo.toml");
        let mut in_features = false;
        let mut features = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_features = line == "[features]";
                continue;
            }
            if !in_features || line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, list) = line
                .split_once('=')
                .unwrap_or_else(|| panic!("a [features] line without '=': {line}"));
            let list = list.trim();
            let inner = list
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .unwrap_or_else(|| {
                    panic!("feature {} is not a one-line list: {list}", name.trim())
                });
            let enabled = inner
                .split(',')
                .map(|item| item.trim().trim_matches('"').to_string())
                .filter(|item| !item.is_empty())
                .collect();
            features.push((name.trim().to_string(), enabled));
        }
        features
    }

    #[test]
    fn beta_features_are_all_in_beta_all() {
        let features = cargo_features();
        let beta_all = features
            .iter()
            .find(|(name, _)| name == "beta-all")
            .map(|(_, enabled)| enabled.clone())
            .expect("crates/murmur-cli/Cargo.toml defines no beta-all feature");
        let beta: Vec<&String> = features
            .iter()
            .map(|(name, _)| name)
            .filter(|name| name.starts_with("beta-") && *name != "beta-all")
            .collect();
        assert!(!beta.is_empty(), "no beta-* feature found in [features]");
        for name in beta {
            assert!(
                beta_all.contains(name),
                "beta feature {name} is not in beta-all's list in crates/murmur-cli/Cargo.toml; \
                 add it there so CI builds and tests it"
            );
        }
    }

    #[cfg(feature = "beta-all")]
    #[test]
    fn beta_features_in_beta_all_register_themselves() {
        let registered: Vec<&str> = super::compiled_beta_features()
            .iter()
            .map(|feature| feature.name)
            .collect();
        for (name, _) in cargo_features() {
            let Some(short) = name.strip_prefix("beta-") else {
                continue;
            };
            if short == "all" {
                continue;
            }
            assert!(
                registered.contains(&short),
                "beta feature {name} is enabled but compiled_beta_features() does not register \
                 '{short}'; push it there under #[cfg(feature = \"{name}\")]"
            );
        }
    }
}
