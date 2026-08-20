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
