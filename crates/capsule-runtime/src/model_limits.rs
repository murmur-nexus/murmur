//! Each driver choice's per-turn output cap and context window, resolved once at launch.
//!
//! A `transport: http` capsule names a model and the driver that serves it. The driver's bundled
//! `murmur.yaml` declares, in its `models:` table, each model's `max_output_tokens` and
//! `context_window` (read by [`murmur_artifact::parse_model_limits`]). For every driver choice,
//! the primary and each `inference.alternates` entry:
//!
//! | Limit | `"manifest"` | else `"driver"` | else |
//! |---|---|---|---|
//! | output cap | `inference.max_tokens` | the table's `max_output_tokens` | refused, `E-RUN-052` |
//! | context window | `context.max_tokens` | the table's `context_window` | none, `"none"` |
//!
//! The manifest's two keys are session-wide: each applies to every choice as written, never
//! clamped to a declared ceiling. A model id matches a table entry by exact string equality. A
//! choice whose model the table does not list draws `W-RUN-009` unless the manifest sets both
//! keys. With no window, compaction does not run and no seed budget applies for that choice.
//!
//! Staging and `mur doctor` both resolve through [`resolve_model_limits`], so the two never
//! disagree on a number or on the warning's text. `transport: process` resolves nothing: the
//! harness owns its model and its output.

use std::collections::BTreeMap;

use murmur_artifact::{
    ContextConfig, InferenceConfig, ModelLimits, PRIMARY_DRIVER_CHOICE, W_RUN_009,
};
use serde::Serialize;

/// Where a limit in force came from, as `session_start` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitSource {
    /// The manifest's `inference.max_tokens` or `context.max_tokens`.
    Manifest,
    /// The driver's `models:` table entry for the choice's model.
    Driver,
    /// Neither: only ever a context window, which the choice then does not have.
    None,
}

impl LimitSource {
    /// The value as written in `trace.jsonl` and printed by `mur doctor`.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Driver => "driver",
            Self::None => "none",
        }
    }
}

/// One driver choice's output cap and context window in force, and where each came from. The
/// `model_limits` object of `session_start` and of each `inference_choices` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedModelLimits {
    /// The `max_tokens` every agent-loop call under this choice sends, unless a controller has
    /// changed `inference.max_tokens`.
    pub max_output_tokens: u32,
    /// [`LimitSource::Manifest`] or [`LimitSource::Driver`].
    pub max_output_tokens_source: LimitSource,
    /// The window compaction and the seed budget measure against; `None` when neither the
    /// manifest nor the driver gives one.
    pub context_window: Option<u32>,
    pub context_window_source: LimitSource,
}

/// One driver choice's resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceModelLimits {
    /// [`PRIMARY_DRIVER_CHOICE`] or the alternate's `name`.
    pub choice: String,
    pub model: String,
    /// The driver artifact as `name@version`.
    pub driver: String,
    pub limits: ResolvedModelLimits,
    /// `W-RUN-009`'s text, code and link not included, when the driver's table does not list the
    /// model and the manifest does not set both keys.
    pub warning: Option<String>,
}

/// A driver choice's driver, as resolution reads it.
#[derive(Debug, Clone, Copy)]
pub struct DriverManifest<'a> {
    /// The driver artifact as `name@version`, for the warning and the refusal.
    pub label: &'a str,
    /// The driver's bundled `murmur.yaml`.
    pub manifest_yaml: &'a str,
}

/// Why a launch is refused: `E-RUN-052`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelLimitsError {
    /// A choice has no output cap: its driver's table does not list its model and the manifest
    /// sets no `inference.max_tokens`.
    #[error(
        "driver choice '{choice}' has no output cap: driver {driver} declares no limits for model \
         '{model}' and the manifest sets no inference.max_tokens"
    )]
    NoOutputCap {
        choice: String,
        model: String,
        driver: String,
    },
    /// A choice's driver ships a `models:` table that does not parse.
    #[error("driver {driver} ships a malformed models: table: {reason}")]
    MalformedTable { driver: String, reason: String },
}

/// Resolves the output cap and context window of every driver choice `inference` declares, the
/// primary first and the alternates in manifest order. Empty under `transport: process`.
///
/// `driver_manifest` takes a choice's driver as `inference` names it (its tool name) to that
/// driver's label and bundled manifest. Each driver's table is parsed once.
pub fn resolve_model_limits<'a>(
    inference: &InferenceConfig,
    context: Option<&ContextConfig>,
    mut driver_manifest: impl FnMut(&str) -> DriverManifest<'a>,
) -> Result<Vec<ChoiceModelLimits>, ModelLimitsError> {
    if inference.transport != "http" {
        return Ok(Vec::new());
    }
    let primary_driver = inference
        .driver
        .as_ref()
        .map(|driver| driver.artifact.as_str())
        .unwrap_or_default();
    let choices = std::iter::once((PRIMARY_DRIVER_CHOICE, primary_driver, &inference.model)).chain(
        inference.alternates.iter().map(|alternate| {
            (
                alternate.name.as_str(),
                alternate.driver.as_str(),
                &alternate.model,
            )
        }),
    );
    let cap_override = inference.max_tokens;
    let window_override = context.and_then(|context| context.max_tokens);
    let mut tables: BTreeMap<&str, BTreeMap<String, ModelLimits>> = BTreeMap::new();
    let mut resolved = Vec::new();
    for (choice, driver, model) in choices {
        let manifest = driver_manifest(driver);
        let table =
            match tables.get(driver) {
                Some(table) => table,
                None => {
                    let table = murmur_artifact::parse_model_limits(manifest.manifest_yaml)
                        .map_err(|err| ModelLimitsError::MalformedTable {
                            driver: manifest.label.to_string(),
                            reason: err.to_string(),
                        })?;
                    tables.entry(driver).or_insert(table)
                }
            };
        let declared = table.get(model.as_str());
        let (max_output_tokens, max_output_tokens_source) = match (cap_override, declared) {
            (Some(cap), _) => (cap, LimitSource::Manifest),
            (None, Some(declared)) => (declared.max_output_tokens, LimitSource::Driver),
            (None, None) => {
                return Err(ModelLimitsError::NoOutputCap {
                    choice: choice.to_string(),
                    model: model.clone(),
                    driver: manifest.label.to_string(),
                })
            }
        };
        let (context_window, context_window_source) = match (window_override, declared) {
            (Some(window), _) => (Some(window), LimitSource::Manifest),
            (None, Some(declared)) => (Some(declared.context_window), LimitSource::Driver),
            (None, None) => (None, LimitSource::None),
        };
        // Undeclared with both keys set needs nothing from the table, so there is nothing to say.
        let warning = (declared.is_none() && context_window.is_none()).then(|| {
            format!(
                "driver {} declares no limits for model '{model}' (driver choice '{choice}'), so \
                 the session uses inference.max_tokens ({max_output_tokens}) as its output cap \
                 and knows no context window: compaction does not run and no seed budget \
                 applies; set context.max_tokens to give it one",
                manifest.label
            )
        });
        resolved.push(ChoiceModelLimits {
            choice: choice.to_string(),
            model: model.clone(),
            driver: manifest.label.to_string(),
            limits: ResolvedModelLimits {
                max_output_tokens,
                max_output_tokens_source,
                context_window,
                context_window_source,
            },
            warning,
        });
    }
    Ok(resolved)
}

/// `W-RUN-009` as staging and `mur doctor` print it, `message` being
/// [`ChoiceModelLimits::warning`].
#[must_use]
pub fn model_limits_warning_line(message: &str) -> String {
    format!(
        "warning[{W_RUN_009}]: {message} ({})",
        murmur_artifact::runtime_warning_link(W_RUN_009)
    )
}

/// The output cap a hook's `run-inference` sends: the primary driver's table entry for the model
/// it calls, else the primary choice's resolved cap. `inference.max_tokens` is an agent-turn cap,
/// so it reaches a hook completion only through that fallback: a small one never truncates a
/// compaction summary for a model the table knows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HookOutputCaps {
    /// The primary driver's `models:` table.
    table: BTreeMap<String, ModelLimits>,
    /// The primary choice's resolved output cap.
    fallback: u32,
}

impl HookOutputCaps {
    pub(crate) fn new(table: BTreeMap<String, ModelLimits>, fallback: u32) -> Self {
        Self { table, fallback }
    }

    /// The cap a completion on `model` sends and admits spend against.
    pub(crate) fn for_model(&self, model: &str) -> u32 {
        self.table
            .get(model)
            .map_or(self.fallback, |limits| limits.max_output_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::RuntimeManifest;

    const DRIVER: &str = "name: drv\nversion: 0.1.0\nruntime: driver\nmodels:\n  m:\n    \
        max_output_tokens: 65536\n    context_window: 400000\n  alt-m:\n    \
        max_output_tokens: 1000\n    context_window: 2000\n";
    const BARE_DRIVER: &str = "name: bare\nversion: 0.2.0\nruntime: driver\n";

    /// A capsule on `local/drv` with `model`, the `inference` lines `extra`, and `context`, and
    /// `local/bare` declared beside it for an alternate to name.
    fn manifest(model: &str, extra: &str, context: &str) -> RuntimeManifest {
        let entry = |name: &str, version: &str| {
            format!(
                "  - name: {name}\n    version: {version}\n    runtime: driver\n    gateway:\n      \
                 endpoint: http://127.0.0.1:1\n      keyless: true\n"
            )
        };
        let bare = if extra.contains("local/bare") {
            entry("local/bare", "0.2.0")
        } else {
            String::new()
        };
        RuntimeManifest::from_yaml_str(&format!(
            "name: cap\nversion: 0.0.1\nartifacts:\n{}{bare}inference:\n  transport: http\n  \
             model: {model}\n  driver:\n    artifact: local/drv\n{extra}{context}",
            entry("local/drv", "0.1.0")
        ))
        .expect("manifest parses")
    }

    fn resolve(manifest: &RuntimeManifest) -> Result<Vec<ChoiceModelLimits>, ModelLimitsError> {
        resolve_model_limits(
            manifest.inference.as_ref().unwrap(),
            manifest.context.as_ref(),
            |driver| match driver {
                "local/bare" => DriverManifest {
                    label: "local/bare@0.2.0",
                    manifest_yaml: BARE_DRIVER,
                },
                _ => DriverManifest {
                    label: "local/drv@0.1.0",
                    manifest_yaml: DRIVER,
                },
            },
        )
    }

    #[test]
    fn a_listed_model_takes_both_limits_from_the_driver() {
        let resolved = resolve(&manifest("m", "", "")).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].choice, PRIMARY_DRIVER_CHOICE);
        assert_eq!(
            resolved[0].limits,
            ResolvedModelLimits {
                max_output_tokens: 65536,
                max_output_tokens_source: LimitSource::Driver,
                context_window: Some(400_000),
                context_window_source: LimitSource::Driver,
            }
        );
        assert_eq!(resolved[0].warning, None);
    }

    #[test]
    fn each_manifest_key_overrides_its_own_limit_as_written() {
        let both = resolve(&manifest(
            "m",
            "  max_tokens: 100000\n",
            "context:\n  max_tokens: 5000\n",
        ))
        .unwrap();
        assert_eq!(
            both[0].limits,
            ResolvedModelLimits {
                max_output_tokens: 100_000,
                max_output_tokens_source: LimitSource::Manifest,
                context_window: Some(5000),
                context_window_source: LimitSource::Manifest,
            }
        );
        let cap = resolve(&manifest("m", "  max_tokens: 4096\n", "")).unwrap();
        assert_eq!(
            cap[0].limits.max_output_tokens_source,
            LimitSource::Manifest
        );
        assert_eq!(cap[0].limits.context_window_source, LimitSource::Driver);
    }

    #[test]
    fn an_unlisted_model_with_a_cap_warns_and_has_no_window() {
        let resolved = resolve(&manifest("M", "  max_tokens: 4096\n", "")).unwrap();
        assert_eq!(resolved[0].limits.max_output_tokens, 4096);
        assert_eq!(resolved[0].limits.context_window, None);
        assert_eq!(resolved[0].limits.context_window_source, LimitSource::None);
        let warning = resolved[0].warning.as_deref().expect("W-RUN-009");
        assert!(
            warning.starts_with(
                "driver local/drv@0.1.0 declares no limits for model 'M' (driver choice \
                 'primary'), so the session uses inference.max_tokens (4096) as its output cap"
            ),
            "{warning}"
        );
        assert!(warning.contains("set context.max_tokens"), "{warning}");
        let both = resolve(&manifest(
            "M",
            "  max_tokens: 4096\n",
            "context:\n  max_tokens: 9000\n",
        ))
        .unwrap();
        assert_eq!(
            both[0].warning, None,
            "both keys set need nothing from the table"
        );
    }

    #[test]
    fn an_unlisted_model_without_a_cap_is_refused() {
        assert_eq!(
            resolve(&manifest("m-2026", "", "context:\n  max_tokens: 9000\n")),
            Err(ModelLimitsError::NoOutputCap {
                choice: "primary".to_string(),
                model: "m-2026".to_string(),
                driver: "local/drv@0.1.0".to_string(),
            })
        );
    }

    #[test]
    fn every_alternate_resolves_against_its_own_driver() {
        let alternates = "  alternates:\n    - name: cheap\n      driver:\n        artifact: \
                          local/drv\n      model: alt-m\n    - name: other\n      driver:\n        \
                          artifact: local/bare\n      model: x\n";
        let control = "control:\n  agent_settings: [inference.driver]\n";
        let refused = resolve(&manifest("m", alternates, control));
        assert_eq!(
            refused,
            Err(ModelLimitsError::NoOutputCap {
                choice: "other".to_string(),
                model: "x".to_string(),
                driver: "local/bare@0.2.0".to_string(),
            })
        );
        let with_cap = format!("  max_tokens: 777\n{alternates}");
        let resolved = resolve(&manifest("m", &with_cap, control)).unwrap();
        let caps: Vec<_> = resolved
            .iter()
            .map(|choice| (choice.choice.as_str(), choice.limits.max_output_tokens))
            .collect();
        assert_eq!(caps, [("primary", 777), ("cheap", 777), ("other", 777)]);
        assert_eq!(resolved[1].limits.context_window, Some(2000));
        assert!(resolved[1].warning.is_none());
        assert!(resolved[2].warning.is_some());
    }

    #[test]
    fn a_malformed_table_is_refused_naming_the_driver_and_field() {
        let manifest = manifest("m", "", "");
        let refused = resolve_model_limits(manifest.inference.as_ref().unwrap(), None, |_| {
            DriverManifest {
                label: "local/drv@0.1.0",
                manifest_yaml: "name: drv\nversion: 0.1.0\nruntime: driver\nmodels:\n  m:\n    \
                                max_output_tokens: 0\n    context_window: 1\n",
            }
        })
        .unwrap_err();
        let message = refused.to_string();
        assert!(
            message.starts_with("driver local/drv@0.1.0 ships a malformed models: table:"),
            "{message}"
        );
        assert!(message.contains("models.m.max_output_tokens"), "{message}");
    }

    #[test]
    fn a_hook_completion_caps_at_the_table_entry_else_the_primary_cap() {
        let caps = HookOutputCaps::new(murmur_artifact::parse_model_limits(DRIVER).unwrap(), 4096);
        assert_eq!(caps.for_model("m"), 65536);
        assert_eq!(caps.for_model("alt-m"), 1000);
        assert_eq!(caps.for_model("unknown"), 4096);
    }

    #[test]
    fn the_source_wire_names() {
        assert_eq!(
            serde_json::to_value(ResolvedModelLimits {
                max_output_tokens: 1,
                max_output_tokens_source: LimitSource::Manifest,
                context_window: None,
                context_window_source: LimitSource::None,
            })
            .unwrap(),
            serde_json::json!({
                "max_output_tokens": 1,
                "max_output_tokens_source": "manifest",
                "context_window": null,
                "context_window_source": "none",
            })
        );
        assert_eq!(LimitSource::Driver.wire_name(), "driver");
    }
}
