//! Each driver choice's per-turn output cap, context window, input ceiling and input budget,
//! resolved once at launch.
//!
//! A `transport: http` capsule names a model and the driver that serves it. The driver's bundled
//! `murmur.yaml` declares, in its `models:` table, each model's `max_output_tokens`,
//! `context_window` and optionally `max_input_tokens` (read by
//! [`murmur_artifact::parse_model_limits`]). For every driver choice, the primary and each
//! `inference.alternates` entry:
//!
//! | Limit | `"manifest"` | else `"driver"` | else |
//! |---|---|---|---|
//! | output cap | `inference.max_tokens` | the table's `max_output_tokens` | refused, `E-RUN-052` |
//! | context window | `context.max_tokens` | the table's `context_window` | none, `"none"` |
//! | input ceiling | — | the table's `max_input_tokens` | none, `"none"` |
//!
//! The manifest's two keys are session-wide: each applies to every choice as written, never
//! clamped to a declared ceiling. A model id matches a table entry by exact string equality. A
//! choice whose model the table does not list draws `W-RUN-009` unless the manifest sets both
//! keys.
//!
//! Every inference call has one input budget, `min(input ceiling, context window − output cap)`
//! with the cap the call sends ([`ResolvedModelLimits::input_budget_for`]). Compaction's
//! threshold and the seed budget are fractions of it. With no window there is no budget: for
//! that choice compaction does not run and no seed budget applies. A choice whose budget is `0`
//! is refused, `E-RUN-055`.
//!
//! `inference.compaction.max_tokens` is checked here too: above the primary driver's ceiling for
//! the compaction model it is refused, `E-RUN-054`; at or above the compaction model's window it
//! leaves compaction calls no input budget, `E-RUN-055`.
//!
//! Staging and `mur doctor` both resolve through [`resolve_model_limits`], so the two never
//! disagree on a number or on the warning's text. `transport: process` resolves nothing: the
//! harness owns its model and its output.

use std::collections::BTreeMap;

use murmur_artifact::{
    ContextConfig, InferenceConfig, ModelLimits, PRIMARY_DRIVER_CHOICE, W_RUN_009,
};
use serde::Serialize;

use crate::inference_import::HookInferencePurpose;

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

impl std::fmt::Display for LimitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_name())
    }
}

/// Which term of `min(input ceiling, context window − output cap)` an input budget is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetBasis {
    /// The driver's `max_input_tokens`, at most the window less the cap (a tie goes here).
    MaxInputTokens,
    /// The context window less the output cap, below any ceiling.
    ContextWindowLessOutputCap,
    /// The context window itself, where no output cap is known (`transport: process`).
    ContextWindow,
    /// No window, so no budget.
    None,
}

impl BudgetBasis {
    /// The value as written in `trace.jsonl`, `session-status` and printed by `mur doctor`.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::MaxInputTokens => "max_input_tokens",
            Self::ContextWindowLessOutputCap => "context_window_less_output_cap",
            Self::ContextWindow => "context_window",
            Self::None => "none",
        }
    }
}

/// The most input tokens one inference call may carry, and which limit set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputBudget {
    pub tokens: u32,
    pub basis: BudgetBasis,
}

impl InputBudget {
    /// `min(ceiling, window − cap)`, saturating at `0`; the ceiling wins a tie.
    #[must_use]
    pub fn of(window: u32, cap: u32, ceiling: Option<u32>) -> Self {
        let rest = window.saturating_sub(cap);
        match ceiling {
            Some(ceiling) if ceiling <= rest => Self {
                tokens: ceiling,
                basis: BudgetBasis::MaxInputTokens,
            },
            _ => Self {
                tokens: rest,
                basis: BudgetBasis::ContextWindowLessOutputCap,
            },
        }
    }
}

/// `n`, or `none` for an absent limit, as the refusals, `mur doctor` and `MURMUR.md` print it.
#[must_use]
pub fn limit_or_none(limit: Option<u32>) -> String {
    limit.map_or_else(|| "none".to_string(), |limit| limit.to_string())
}

/// One driver choice's output cap, context window, input ceiling and input budget in force, and
/// where each came from. The `model_limits` object of `session_start` and of each
/// `inference_choices` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ResolvedModelLimits {
    /// The `max_tokens` every agent-loop call under this choice sends, unless a controller has
    /// changed `inference.max_tokens`.
    pub max_output_tokens: u32,
    /// [`LimitSource::Manifest`] or [`LimitSource::Driver`].
    pub max_output_tokens_source: LimitSource,
    /// The window the input budget is cut from; `None` when neither the manifest nor the driver
    /// gives one.
    pub context_window: Option<u32>,
    pub context_window_source: LimitSource,
    /// The most input tokens the driver says the provider accepts for the model.
    pub max_input_tokens: Option<u32>,
    /// [`LimitSource::Driver`] when the table entry declares one, else [`LimitSource::None`].
    pub max_input_tokens_source: LimitSource,
    /// [`Self::input_budget_for`] `max_output_tokens`: what compaction and the seed budget
    /// measure against until a controller changes the cap. `None` with no window.
    pub input_budget: Option<u32>,
    pub input_budget_basis: BudgetBasis,
}

impl ResolvedModelLimits {
    /// The input budget of a call under this choice that sends `cap` as its output cap; `None`
    /// with no window.
    #[must_use]
    pub fn input_budget_for(&self, cap: u32) -> Option<InputBudget> {
        self.context_window
            .map(|window| InputBudget::of(window, cap, self.max_input_tokens))
    }
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
    /// The table entry's `vision`; `None` when the entry does not say or the table does not list
    /// the model.
    pub vision: Option<bool>,
}

/// A driver choice's driver, as resolution reads it.
#[derive(Debug, Clone, Copy)]
pub struct DriverManifest<'a> {
    /// The driver artifact as `name@version`, for the warning and the refusal.
    pub label: &'a str,
    /// The driver's bundled `murmur.yaml`.
    pub manifest_yaml: &'a str,
}

/// Why a launch is refused: `E-RUN-052`, `E-RUN-054` for
/// [`ModelLimitsError::CompactionCapAboveCeiling`], or `E-RUN-055` for
/// [`ModelLimitsError::NoInputBudget`] and [`ModelLimitsError::CompactionNoInputBudget`].
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
    /// `inference.compaction.max_tokens` exceeds the primary driver's `max_output_tokens` for the
    /// compaction model.
    #[error(
        "inference.compaction.max_tokens ({max_tokens}) is above the output ceiling of \
         {ceiling} that driver {driver} declares for compaction model '{model}'"
    )]
    CompactionCapAboveCeiling {
        max_tokens: u32,
        ceiling: u32,
        model: String,
        driver: String,
    },
    /// A choice's output cap is at or above its context window, so its calls have no input
    /// budget.
    #[error(
        "driver choice '{choice}' has no input budget: its context window of {window} \
         ({window_source}) less its output cap of {cap} ({cap_source}) leaves no input tokens; \
         input ceiling {ceiling_text} (model '{model}', driver {driver})",
        ceiling_text = limit_or_none(*.ceiling)
    )]
    NoInputBudget {
        choice: String,
        model: String,
        driver: String,
        window: u32,
        window_source: LimitSource,
        cap: u32,
        cap_source: LimitSource,
        ceiling: Option<u32>,
    },
    /// `inference.compaction.max_tokens` is at or above the compaction model's context window,
    /// so compaction calls have no input budget.
    #[error(
        "compaction calls have no input budget: the context window of {window} \
         ({window_source}) for compaction model '{model}' less inference.compaction.max_tokens \
         of {cap} leaves no input tokens; input ceiling {ceiling_text} (driver {driver})",
        ceiling_text = limit_or_none(*.ceiling)
    )]
    CompactionNoInputBudget {
        model: String,
        driver: String,
        window: u32,
        window_source: LimitSource,
        cap: u32,
        ceiling: Option<u32>,
    },
}

/// Resolves the output cap, context window, input ceiling and input budget of every driver
/// choice `inference` declares, the primary first and the alternates in manifest order. Empty
/// under `transport: process`.
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
        let max_input_tokens = declared.and_then(|declared| declared.max_input_tokens);
        let mut limits = ResolvedModelLimits {
            max_output_tokens,
            max_output_tokens_source,
            context_window,
            context_window_source,
            max_input_tokens,
            max_input_tokens_source: if max_input_tokens.is_some() {
                LimitSource::Driver
            } else {
                LimitSource::None
            },
            input_budget: None,
            input_budget_basis: BudgetBasis::None,
        };
        if let Some(budget) = limits.input_budget_for(max_output_tokens) {
            if budget.tokens == 0 {
                return Err(ModelLimitsError::NoInputBudget {
                    choice: choice.to_string(),
                    model: model.clone(),
                    driver: manifest.label.to_string(),
                    window: context_window.unwrap_or_default(),
                    window_source: context_window_source,
                    cap: max_output_tokens,
                    cap_source: max_output_tokens_source,
                    ceiling: max_input_tokens,
                });
            }
            limits.input_budget = Some(budget.tokens);
            limits.input_budget_basis = budget.basis;
        }
        resolved.push(ChoiceModelLimits {
            choice: choice.to_string(),
            model: model.clone(),
            driver: manifest.label.to_string(),
            limits,
            warning,
            vision: declared.and_then(|declared| declared.vision),
        });
    }
    check_compaction_cap(inference, primary_driver, &tables, &resolved)?;
    check_compaction_budget(
        inference,
        window_override,
        primary_driver,
        &tables,
        &resolved,
    )?;
    Ok(resolved)
}

/// Refuses an `inference.compaction.max_tokens` above the primary driver's ceiling for the
/// compaction model: the primary driver serves every compaction call. A model its table does not
/// list has no ceiling to check against.
fn check_compaction_cap(
    inference: &InferenceConfig,
    primary_driver: &str,
    tables: &BTreeMap<&str, BTreeMap<String, ModelLimits>>,
    resolved: &[ChoiceModelLimits],
) -> Result<(), ModelLimitsError> {
    let Some(compaction) = inference.compaction.as_ref() else {
        return Ok(());
    };
    let Some(max_tokens) = compaction.max_tokens else {
        return Ok(());
    };
    let model = compaction.model.as_deref().unwrap_or(&inference.model);
    let Some(declared) = tables
        .get(primary_driver)
        .and_then(|table| table.get(model))
    else {
        return Ok(());
    };
    if max_tokens <= declared.max_output_tokens {
        return Ok(());
    }
    Err(ModelLimitsError::CompactionCapAboveCeiling {
        max_tokens,
        ceiling: declared.max_output_tokens,
        model: model.to_string(),
        driver: resolved
            .first()
            .map(|primary| primary.driver.clone())
            .unwrap_or_default(),
    })
}

/// Refuses an `inference.compaction.max_tokens` at or above the compaction model's window:
/// `context.max_tokens`, else the primary driver's table entry for the model. A model with
/// neither has no window to check against.
fn check_compaction_budget(
    inference: &InferenceConfig,
    window_override: Option<u32>,
    primary_driver: &str,
    tables: &BTreeMap<&str, BTreeMap<String, ModelLimits>>,
    resolved: &[ChoiceModelLimits],
) -> Result<(), ModelLimitsError> {
    let Some(cap) = inference
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.max_tokens)
    else {
        return Ok(());
    };
    let model = inference
        .compaction
        .as_ref()
        .and_then(|compaction| compaction.model.as_deref())
        .unwrap_or(&inference.model);
    let declared = tables
        .get(primary_driver)
        .and_then(|table| table.get(model));
    let (window, window_source) = match (window_override, declared) {
        (Some(window), _) => (window, LimitSource::Manifest),
        (None, Some(declared)) => (declared.context_window, LimitSource::Driver),
        (None, None) => return Ok(()),
    };
    let ceiling = declared.and_then(|declared| declared.max_input_tokens);
    if InputBudget::of(window, cap, ceiling).tokens > 0 {
        return Ok(());
    }
    Err(ModelLimitsError::CompactionNoInputBudget {
        model: model.to_string(),
        driver: resolved
            .first()
            .map(|primary| primary.driver.clone())
            .unwrap_or_default(),
        window,
        window_source,
        cap,
        ceiling,
    })
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

/// The output cap a hook's `run-inference` sends: `inference.compaction.max_tokens` while the hook
/// serves `on-compaction` and the key is set, else the primary driver's table entry for the model
/// it calls, else the primary choice's resolved cap. `inference.max_tokens` is an agent-turn cap,
/// so it reaches a hook completion only through that fallback: a small one never truncates a
/// compaction summary for a model the table knows. A compaction call is capped by its own key
/// instead, which the operator sets for that call alone.
///
/// The call's input budget is cut the same way as an agent-loop call's
/// ([`InputBudget::of`]), from `context.max_tokens`, else the table entry's window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct HookOutputCaps {
    /// The primary driver's `models:` table.
    table: BTreeMap<String, ModelLimits>,
    /// The primary choice's resolved output cap.
    fallback: u32,
    /// `inference.compaction.max_tokens`.
    compaction: Option<u32>,
    /// `context.max_tokens`.
    window: Option<u32>,
}

impl HookOutputCaps {
    pub(crate) fn new(
        table: BTreeMap<String, ModelLimits>,
        fallback: u32,
        compaction: Option<u32>,
        window: Option<u32>,
    ) -> Self {
        Self {
            table,
            fallback,
            compaction,
            window,
        }
    }

    /// The input budget of a completion on `model` that sends `cap` ([`Self::for_call`]'s
    /// result): `None` with no window, `0` when the cap fills a window the launch did not check.
    pub(crate) fn input_budget(&self, model: &str, cap: u32) -> Option<u32> {
        let declared = self.table.get(model);
        let window = self
            .window
            .or_else(|| declared.map(|limits| limits.context_window))?;
        Some(
            InputBudget::of(
                window,
                cap,
                declared.and_then(|limits| limits.max_input_tokens),
            )
            .tokens,
        )
    }

    /// The cap a completion on `model` made for `purpose` sends and admits spend against.
    pub(crate) fn for_call(&self, model: &str, purpose: HookInferencePurpose) -> u32 {
        match (purpose, self.compaction) {
            (HookInferencePurpose::Compaction, Some(cap)) => cap,
            _ => self
                .table
                .get(model)
                .map_or(self.fallback, |limits| limits.max_output_tokens),
        }
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
                max_input_tokens: None,
                max_input_tokens_source: LimitSource::None,
                input_budget: Some(334_464),
                input_budget_basis: BudgetBasis::ContextWindowLessOutputCap,
            }
        );
        assert_eq!(resolved[0].warning, None);
    }

    #[test]
    fn each_manifest_key_overrides_its_own_limit_as_written() {
        let both = resolve(&manifest(
            "m",
            "  max_tokens: 4000\n",
            "context:\n  max_tokens: 5000\n",
        ))
        .unwrap();
        assert_eq!(
            both[0].limits,
            ResolvedModelLimits {
                max_output_tokens: 4000,
                max_output_tokens_source: LimitSource::Manifest,
                context_window: Some(5000),
                context_window_source: LimitSource::Manifest,
                max_input_tokens: None,
                max_input_tokens_source: LimitSource::None,
                input_budget: Some(1000),
                input_budget_basis: BudgetBasis::ContextWindowLessOutputCap,
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
        let table = murmur_artifact::parse_model_limits(DRIVER).unwrap();
        let lifecycle = HookInferencePurpose::Lifecycle;
        let compaction = HookInferencePurpose::Compaction;
        let caps = HookOutputCaps::new(table.clone(), 4096, None, None);
        for purpose in [lifecycle, compaction] {
            assert_eq!(caps.for_call("m", purpose), 65536);
            assert_eq!(caps.for_call("alt-m", purpose), 1000);
            assert_eq!(caps.for_call("unknown", purpose), 4096);
        }
        let capped = HookOutputCaps::new(table, 4096, Some(2000), None);
        assert_eq!(capped.for_call("m", lifecycle), 65536);
        assert_eq!(capped.for_call("unknown", lifecycle), 4096);
        assert_eq!(capped.for_call("m", compaction), 2000);
        assert_eq!(capped.for_call("unknown", compaction), 2000);
    }

    #[test]
    fn a_hook_completions_budget_is_cut_from_its_own_cap() {
        let table = murmur_artifact::parse_model_limits(WORKED_DRIVER).unwrap();
        let caps = HookOutputCaps::new(table.clone(), 4096, Some(2000), None);
        // min(922000, 1050000 − 2000) for a compaction call; the table's cap otherwise.
        assert_eq!(caps.input_budget("ceiled", 2000), Some(922_000));
        assert_eq!(caps.input_budget("ceiled", 128_000), Some(922_000));
        assert_eq!(caps.input_budget("plain", 128_000), Some(922_000));
        assert_eq!(caps.input_budget("plain", 6000), Some(1_044_000));
        assert_eq!(caps.input_budget("unlisted", 4096), None, "no window");
        let windowed = HookOutputCaps::new(table, 4096, None, Some(200_000));
        assert_eq!(windowed.input_budget("ceiled", 8192), Some(191_808));
        assert_eq!(windowed.input_budget("unlisted", 4096), Some(195_904));
        assert_eq!(
            windowed.input_budget("unlisted", 300_000),
            Some(0),
            "a model the launch could not check saturates"
        );
    }

    /// A driver with the worked-numbers entries: `ceiled` 1050000 / 128000 / 922000, `plain`
    /// 1050000 / 128000 / —, `tight` 1050000 / 128000 / 900000, `small` 1000000 / 8192 / —,
    /// `alt-m` 2000 / 1000 / — and `full` 4000 / 4000 / —.
    const WORKED_DRIVER: &str =
        "name: drv\nversion: 0.1.0\nruntime: driver\nmodels:\n  ceiled:\n    \
        max_output_tokens: 128000\n    context_window: 1050000\n    max_input_tokens: 922000\n  \
        plain:\n    max_output_tokens: 128000\n    context_window: 1050000\n  tight:\n    \
        max_output_tokens: 128000\n    context_window: 1050000\n    max_input_tokens: 900000\n  \
        small:\n    max_output_tokens: 8192\n    context_window: 1000000\n  alt-m:\n    \
        max_output_tokens: 1000\n    context_window: 2000\n  full:\n    \
        max_output_tokens: 4000\n    context_window: 4000\n";

    fn resolve_worked(
        manifest: &RuntimeManifest,
    ) -> Result<Vec<ChoiceModelLimits>, ModelLimitsError> {
        resolve_model_limits(
            manifest.inference.as_ref().unwrap(),
            manifest.context.as_ref(),
            |_| DriverManifest {
                label: "local/drv@0.1.0",
                manifest_yaml: WORKED_DRIVER,
            },
        )
    }

    #[test]
    fn every_worked_numbers_row_resolves_its_budget_and_basis() {
        for (model, extra, context, budget, basis, fires_at) in [
            (
                "ceiled",
                "",
                "",
                922_000,
                BudgetBasis::MaxInputTokens,
                461_000,
            ),
            (
                "plain",
                "  max_tokens: 6000\n",
                "",
                1_044_000,
                BudgetBasis::ContextWindowLessOutputCap,
                522_000,
            ),
            (
                "tight",
                "",
                "",
                900_000,
                BudgetBasis::MaxInputTokens,
                450_000,
            ),
            (
                "ceiled",
                "  max_tokens: 8192\n",
                "context:\n  max_tokens: 200000\n",
                191_808,
                BudgetBasis::ContextWindowLessOutputCap,
                95_904,
            ),
        ] {
            let resolved = resolve_worked(&manifest(model, extra, context)).unwrap();
            let limits = resolved[0].limits;
            assert_eq!(
                (limits.input_budget, limits.input_budget_basis),
                (Some(budget), basis),
                "{model} {extra:?} {context:?}"
            );
            // Threshold 0.5 fires at half the budget, and not one token below it.
            let occupancy = |tokens| crate::agent::SessionOccupancy::estimate(tokens);
            assert!(occupancy(fires_at).crosses(budget, 0.5));
            assert!(!occupancy(fires_at - 1).crosses(budget, 0.5));
        }
        let ceiled = resolve_worked(&manifest("ceiled", "", "")).unwrap()[0].limits;
        assert_eq!(
            ceiled,
            ResolvedModelLimits {
                max_output_tokens: 128_000,
                max_output_tokens_source: LimitSource::Driver,
                context_window: Some(1_050_000),
                context_window_source: LimitSource::Driver,
                max_input_tokens: Some(922_000),
                max_input_tokens_source: LimitSource::Driver,
                input_budget: Some(922_000),
                input_budget_basis: BudgetBasis::MaxInputTokens,
            }
        );
        let plain =
            resolve_worked(&manifest("plain", "  max_tokens: 6000\n", "")).unwrap()[0].limits;
        assert_eq!(
            (plain.max_input_tokens, plain.max_input_tokens_source),
            (None, LimitSource::None)
        );
        assert_eq!(
            resolve_worked(&manifest(
                "small",
                "  max_tokens: 200000\n",
                "context:\n  max_tokens: 200000\n"
            )),
            Err(ModelLimitsError::NoInputBudget {
                choice: "primary".to_string(),
                model: "small".to_string(),
                driver: "local/drv@0.1.0".to_string(),
                window: 200_000,
                window_source: LimitSource::Manifest,
                cap: 200_000,
                cap_source: LimitSource::Manifest,
                ceiling: None,
            })
        );
    }

    #[test]
    fn the_input_ceiling_wins_a_tie_with_the_window_less_the_cap() {
        let limits = resolve_worked(&manifest("ceiled", "", "")).unwrap()[0].limits;
        assert_eq!(
            limits.input_budget_for(128_000),
            Some(InputBudget {
                tokens: 922_000,
                basis: BudgetBasis::MaxInputTokens
            })
        );
        assert_eq!(
            limits.input_budget_for(128_001),
            Some(InputBudget {
                tokens: 921_999,
                basis: BudgetBasis::ContextWindowLessOutputCap
            })
        );
        assert_eq!(
            limits.input_budget_for(2000),
            Some(InputBudget {
                tokens: 922_000,
                basis: BudgetBasis::MaxInputTokens
            })
        );
        let windowless = ResolvedModelLimits {
            context_window: None,
            ..limits
        };
        assert_eq!(windowless.input_budget_for(1), None);
    }

    #[test]
    fn a_cap_that_fills_the_window_is_refused_naming_every_number() {
        for cap in [200_000, 300_000] {
            let refused = resolve_worked(&manifest(
                "plain",
                &format!("  max_tokens: {cap}\n"),
                "context:\n  max_tokens: 200000\n",
            ))
            .unwrap_err();
            assert_eq!(
                refused.to_string(),
                format!(
                    "driver choice 'primary' has no input budget: its context window of 200000 \
                     (manifest) less its output cap of {cap} (manifest) leaves no input tokens; \
                     input ceiling none (model 'plain', driver local/drv@0.1.0)"
                )
            );
        }
        let ceiled = resolve_worked(&manifest(
            "ceiled",
            "  max_tokens: 200000\n",
            "context:\n  max_tokens: 200000\n",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            ceiled.ends_with("input ceiling 922000 (model 'ceiled', driver local/drv@0.1.0)"),
            "{ceiled}"
        );
        let one = resolve_worked(&manifest(
            "plain",
            "  max_tokens: 199999\n",
            "context:\n  max_tokens: 200000\n",
        ))
        .unwrap();
        assert_eq!(one[0].limits.input_budget, Some(1));
    }

    #[test]
    fn an_alternate_is_refused_on_its_own_window() {
        let alternates =
            "  max_tokens: 2000\n  alternates:\n    - name: cheap\n      driver:\n        \
                          artifact: local/drv\n      model: alt-m\n";
        let control = "control:\n  agent_settings: [inference.driver]\n";
        let refused = resolve_worked(&manifest("ceiled", alternates, control)).unwrap_err();
        assert_eq!(
            refused,
            ModelLimitsError::NoInputBudget {
                choice: "cheap".to_string(),
                model: "alt-m".to_string(),
                driver: "local/drv@0.1.0".to_string(),
                window: 2000,
                window_source: LimitSource::Driver,
                cap: 2000,
                cap_source: LimitSource::Manifest,
                ceiling: None,
            }
        );
    }

    #[test]
    fn a_compaction_cap_that_fills_the_compaction_window_is_refused() {
        // Inside the output ceiling (128000) but at the window `context.max_tokens` sets.
        let refused = resolve_worked(&manifest(
            "ceiled",
            &format!("  max_tokens: 8192\n{}", compaction_cap(100_000, "")),
            "context:\n  max_tokens: 100000\n",
        ))
        .unwrap_err();
        assert_eq!(
            refused,
            ModelLimitsError::CompactionNoInputBudget {
                model: "ceiled".to_string(),
                driver: "local/drv@0.1.0".to_string(),
                window: 100_000,
                window_source: LimitSource::Manifest,
                cap: 100_000,
                ceiling: Some(922_000),
            }
        );
        assert_eq!(
            refused.to_string(),
            "compaction calls have no input budget: the context window of 100000 (manifest) for \
             compaction model 'ceiled' less inference.compaction.max_tokens of 100000 leaves no \
             input tokens; input ceiling 922000 (driver local/drv@0.1.0)"
        );
        // The table's window for the compaction model, when the manifest sets none.
        assert_eq!(
            resolve_worked(&manifest(
                "ceiled",
                &compaction_cap(4000, "    model: full\n"),
                ""
            )),
            Err(ModelLimitsError::CompactionNoInputBudget {
                model: "full".to_string(),
                driver: "local/drv@0.1.0".to_string(),
                window: 4000,
                window_source: LimitSource::Driver,
                cap: 4000,
                ceiling: None,
            })
        );
        assert!(resolve_worked(&manifest(
            "ceiled",
            &compaction_cap(1000, "    model: alt-m\n"),
            ""
        ))
        .is_ok());
        assert!(resolve_worked(&manifest(
            "ceiled",
            &format!("  max_tokens: 8192\n{}", compaction_cap(99_999, "")),
            "context:\n  max_tokens: 100000\n",
        ))
        .is_ok());
        // An unlisted compaction model with no context.max_tokens has no window to check.
        assert!(resolve_worked(&manifest(
            "ceiled",
            &compaction_cap(u32::MAX, "    model: unlisted\n"),
            ""
        ))
        .is_ok());
    }

    fn compaction_cap(max_tokens: u32, model_line: &str) -> String {
        format!("  compaction:\n{model_line}    max_tokens: {max_tokens}\n")
    }

    #[test]
    fn a_compaction_cap_above_the_ceiling_is_refused_naming_it() {
        let refused = resolve(&manifest("m", &compaction_cap(65537, ""), ""));
        assert_eq!(
            refused,
            Err(ModelLimitsError::CompactionCapAboveCeiling {
                max_tokens: 65537,
                ceiling: 65536,
                model: "m".to_string(),
                driver: "local/drv@0.1.0".to_string(),
            })
        );
        assert_eq!(
            refused.unwrap_err().to_string(),
            "inference.compaction.max_tokens (65537) is above the output ceiling of 65536 that \
             driver local/drv@0.1.0 declares for compaction model 'm'"
        );
        let at_ceiling = resolve(&manifest("m", &compaction_cap(65536, ""), "")).unwrap();
        assert_eq!(
            at_ceiling[0].limits.max_output_tokens, 65536,
            "the compaction cap leaves the agent-turn cap alone"
        );
    }

    #[test]
    fn the_compaction_model_is_the_one_checked() {
        let model_line = |model: &str| format!("    model: {model}\n");
        // alt-m's ceiling is 1000, so a value fine for the primary model is refused for it.
        assert_eq!(
            resolve(&manifest(
                "m",
                &compaction_cap(2000, &model_line("alt-m")),
                ""
            )),
            Err(ModelLimitsError::CompactionCapAboveCeiling {
                max_tokens: 2000,
                ceiling: 1000,
                model: "alt-m".to_string(),
                driver: "local/drv@0.1.0".to_string(),
            })
        );
        // ...and a value above the primary model's ceiling passes when compaction uses another.
        assert!(resolve(&manifest(
            "alt-m",
            &compaction_cap(60000, &model_line("m")),
            ""
        ))
        .is_ok());
        // An unlisted compaction model has no ceiling to check against.
        assert!(resolve(&manifest(
            "m",
            &compaction_cap(u32::MAX, &model_line("unlisted")),
            ""
        ))
        .is_ok());
        assert!(resolve(&manifest(
            "M",
            &format!("  max_tokens: 4096\n{}", compaction_cap(u32::MAX, "")),
            ""
        ))
        .is_ok());
    }

    #[test]
    fn the_source_wire_names() {
        assert_eq!(
            serde_json::to_value(ResolvedModelLimits {
                max_output_tokens: 1,
                max_output_tokens_source: LimitSource::Manifest,
                context_window: None,
                context_window_source: LimitSource::None,
                max_input_tokens: None,
                max_input_tokens_source: LimitSource::None,
                input_budget: None,
                input_budget_basis: BudgetBasis::None,
            })
            .unwrap(),
            serde_json::json!({
                "max_output_tokens": 1,
                "max_output_tokens_source": "manifest",
                "context_window": null,
                "context_window_source": "none",
                "max_input_tokens": null,
                "max_input_tokens_source": "none",
                "input_budget": null,
                "input_budget_basis": "none",
            })
        );
        assert_eq!(LimitSource::Driver.wire_name(), "driver");
        let text =
            serde_json::to_string(&resolve_worked(&manifest("ceiled", "", "")).unwrap()[0].limits)
                .unwrap();
        assert_eq!(
            text,
            "{\"max_output_tokens\":128000,\"max_output_tokens_source\":\"driver\",\
             \"context_window\":1050000,\"context_window_source\":\"driver\",\
             \"max_input_tokens\":922000,\"max_input_tokens_source\":\"driver\",\
             \"input_budget\":922000,\"input_budget_basis\":\"max_input_tokens\"}"
        );
        for (basis, name) in [
            (BudgetBasis::MaxInputTokens, "max_input_tokens"),
            (
                BudgetBasis::ContextWindowLessOutputCap,
                "context_window_less_output_cap",
            ),
            (BudgetBasis::ContextWindow, "context_window"),
            (BudgetBasis::None, "none"),
        ] {
            assert_eq!(basis.wire_name(), name);
            assert_eq!(
                serde_json::to_value(basis).unwrap(),
                serde_json::json!(name)
            );
        }
    }
}
