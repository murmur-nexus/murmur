//! Driver choices: the named pairs of one driver artifact and one model that a running capsule's
//! agent loop may be switched between.
//!
//! **What a switch selects.** A choice is a name, a driver artifact and a model. Index 0 is
//! [`PRIMARY_DRIVER_CHOICE`], which is `inference.driver.artifact` with `inference.model`. Every
//! other choice is an `inference.alternates` entry, an `artifacts:` entry the operator declared and
//! vetted: nothing is pulled and nothing is installed. Two alternates may share a driver artifact,
//! for two models of one provider. The setting's wire name is `inference.driver` and its value is a
//! choice name.
//!
//! **The conversation across a switch.** One rule, whatever the boundary:
//!
//! - The history is carried unchanged. The message list is in murmur's neutral block format, and
//!   every driver translates that format into its own provider's shape on every request, so the
//!   next driver receives the list the previous one did. A switch between tasks and a switch in the
//!   middle of a task differ only in how much history there is.
//! - Provider-bound state does not cross, and only the runtime knows a switch happened. The held
//!   `continuation_id` is scoped to the choice it was returned under, so the first call under a
//!   different choice is a full resend with no `continuation_id`. A `thinking` block carries its
//!   provider's signature for the model that wrote it, so every assistant message the agent loop
//!   pushes records its producer under the envelope key `produced_by`, and a `thinking` block is
//!   sent only to the driver and model that produced it. It is omitted from the wire view for any
//!   other choice and sent again after a switch back. The stored history and the conversation
//!   record are never edited.
//! - Everything else is unchanged across a switch: tool call ids, tool results, fences, images,
//!   the system prompt, the tool inventory and `prompt_cache_key`.
//! - A switch that lands between a tool call and its result hands the new model a call it did not
//!   make. The neutral format carries that faithfully; a provider that demands the model's own
//!   reasoning on that turn is the driver's to accommodate.
//!
//! **The credential.** Each choice's driver reaches its provider through its own `gateway:`, and a
//! switch changes which credential is attached. Every choice's gateway is metered against the one
//! spend meter. An alternate's credential is resolved at launch as the primary's is, but a failure
//! does not refuse the launch: the choice is staged unavailable and `W-RUN-003` names it. A switch
//! is refused with `409 credential_unresolvable` unless the choice's credential can produce a value
//! now. After launch only an injected `control.secrets` credential with nothing injected cannot: a
//! config source keeps its last good value, and environment and literal sources are fixed at
//! launch. A secret that keys the choice in use cannot be forgotten, and the switch check and the
//! forget check are made under one lock in [`crate::control_plane::ControlState`].
//!
//! **What follows the switch.** Only agent-loop inference calls. Compaction, a hook's
//! `run-inference` and seed summarization stay on the primary driver and its gateway, and
//! `MURMUR_INFERENCE_*` as tools and hooks see it names the primary for the whole session. A
//! driver-choice dispatch sets `MURMUR_INFERENCE_ENDPOINT`, `MURMUR_INFERENCE_MODEL` and
//! `MURMUR_INFERENCE_DRIVER` in that driver's store to the choice's own values.
//! `inference.driver.config` applies to every declared driver. `inference.max_tokens` and
//! `context.max_tokens` are one number each for the session and must suit every declared model.
//!
//! **Token accounting is an estimate.** Occupancy, the compaction trigger and spend admission count
//! cl100k over the serialized payload for every provider, and after a switch they count it against
//! a model whose tokenizer and window may differ more. No per-model window exists. The provider's
//! own counts arrive in `input_tokens_actual` and `output_tokens_actual`, and each agent-loop
//! `inference` event names its choice, so the divergence is visible per model.
//!
//! **The agent's grant.** `control.agent_settings: [inference.driver]` writes the runtime-provided
//! `switch-driver` tool, and the grant is the tool's existence. The runtime answers the call
//! in-process, after the policy decision point, so it needs no credential and never touches
//! `/control` or the controller token. It is not callable from a plan step, and the trace records
//! the change with `principal: "agent"`.
//!
//! **Only `transport: http`.** Under `transport: process` the harness owns the conversation and its
//! model, and `inference.alternates` is refused at load.
//!
//! **Nothing survives a restart.** A relaunch or `--resume` starts on `primary`. A resumed history
//! keeps its `produced_by` marks, so reasoning an alternate wrote is not replayed to the primary.

use murmur_artifact::{InferenceConfig, PRIMARY_DRIVER_CHOICE};
use serde_json::{json, Value};

/// One named pair of a driver artifact and a model, and whether its credential could be staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverChoice {
    /// [`PRIMARY_DRIVER_CHOICE`] or an alternate's `name`.
    pub(crate) name: String,
    /// The driver artifact the call is dispatched to.
    pub(crate) driver: String,
    /// The model string the payload carries.
    pub(crate) model: String,
    /// Where the driver's gateway credential came from, as `session_start` names it: `"config"`,
    /// `"environment"`, `"manifest"`, `"injected"`, `"keyless"`, or `"none"` for a choice whose
    /// credential was found nowhere.
    pub(crate) credential_source: &'static str,
    /// The `control.secrets` name the driver's gateway reads on every request, when it is one.
    /// Only such a credential can be without a value after launch.
    pub(crate) injected_secret: Option<String>,
    /// The credential name that could not be resolved at launch, for a choice staged unavailable.
    pub(crate) unresolved_credential: Option<String>,
}

impl DriverChoice {
    /// Whether the choice's credential resolved at launch.
    pub(crate) fn staged(&self) -> bool {
        self.unresolved_credential.is_none()
    }

    /// The `produced_by` value an assistant message this choice produced carries.
    pub(crate) fn producer(&self) -> Value {
        json!({"driver": self.driver, "model": self.model})
    }
}

/// Every driver choice of one session, [`PRIMARY_DRIVER_CHOICE`] first, then the alternates in
/// manifest order. A setting value names a choice by its index here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverChoices {
    choices: Vec<DriverChoice>,
}

impl DriverChoices {
    /// The choices `inference` declares, each staged with no credential recorded. Staging fills in
    /// the credential of each through [`Self::record_credential`].
    pub(crate) fn declared(inference: &InferenceConfig) -> Self {
        let primary = DriverChoice {
            name: PRIMARY_DRIVER_CHOICE.to_string(),
            driver: inference
                .driver
                .as_ref()
                .map(|driver| driver.artifact.clone())
                .unwrap_or_default(),
            model: inference.model.clone(),
            credential_source: "none",
            injected_secret: None,
            unresolved_credential: None,
        };
        let alternates = inference.alternates.iter().map(|alternate| DriverChoice {
            name: alternate.name.clone(),
            driver: alternate.driver.clone(),
            model: alternate.model.clone(),
            credential_source: "none",
            injected_secret: None,
            unresolved_credential: None,
        });
        Self {
            choices: std::iter::once(primary).chain(alternates).collect(),
        }
    }

    /// Records, for every choice on `driver`, where its credential came from and which injected
    /// secret it reads, or the credential name that could not be resolved.
    pub(crate) fn record_credential(
        &mut self,
        driver: &str,
        credential_source: &'static str,
        injected_secret: Option<&str>,
        unresolved_credential: Option<&str>,
    ) {
        for choice in self
            .choices
            .iter_mut()
            .filter(|choice| choice.driver == driver)
        {
            choice.credential_source = credential_source;
            choice.injected_secret = injected_secret.map(str::to_string);
            choice.unresolved_credential = unresolved_credential.map(str::to_string);
        }
    }

    /// The primary choice.
    pub(crate) fn primary(&self) -> &DriverChoice {
        &self.choices[0]
    }

    /// The choice at `index`.
    pub(crate) fn get(&self, index: usize) -> Option<&DriverChoice> {
        self.choices.get(index)
    }

    /// The index of the choice named `name`.
    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        self.choices.iter().position(|choice| choice.name == name)
    }

    /// Whether any alternate is declared. Without one, nothing a switch introduces is written:
    /// no `produced_by`, no `driver_choice`, no `inference_choices`.
    pub(crate) fn has_alternates(&self) -> bool {
        self.choices.len() > 1
    }

    /// Every choice, primary first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &DriverChoice> {
        self.choices.iter()
    }

    /// Every choice's name, comma-joined, for a refusal that lists what may be selected.
    pub(crate) fn names(&self) -> String {
        self.choices
            .iter()
            .map(|choice| choice.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The `choices` array `GET /control` lists beside `inference.driver`. `available` is whether a
    /// switch to the choice would be accepted now, as `available(choice)` answers it.
    pub(crate) fn listing(&self, available: impl Fn(&DriverChoice) -> bool) -> Vec<Value> {
        self.choices
            .iter()
            .map(|choice| {
                json!({
                    "name": choice.name,
                    "driver": choice.driver,
                    "model": choice.model,
                    "available": available(choice),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use murmur_artifact::{InferenceAlternate, InferenceDriver, ToolRefresh};

    fn inference() -> InferenceConfig {
        InferenceConfig {
            transport: "http".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            driver: Some(InferenceDriver {
                artifact: "murmur-driver-anthropic".to_string(),
                config: None,
            }),
            command: None,
            compaction: None,
            system_prompt: None,
            system_prompt_file: None,
            system_prompt_artifact: None,
            max_turns: 10,
            max_tokens: None,
            max_session_tokens: None,
            tool_refresh: ToolRefresh::Compaction,
            alternates: vec![
                InferenceAlternate {
                    name: "haiku".to_string(),
                    model: "claude-haiku-4-5".to_string(),
                    driver: "murmur-driver-anthropic".to_string(),
                },
                InferenceAlternate {
                    name: "gpt".to_string(),
                    model: "gpt-5".to_string(),
                    driver: "murmur-driver-openai".to_string(),
                },
            ],
        }
    }

    #[test]
    fn driver_choice_table_puts_primary_first_then_alternates_in_order() {
        let choices = DriverChoices::declared(&inference());
        let names: Vec<&str> = choices.iter().map(|choice| choice.name.as_str()).collect();
        assert_eq!(names, vec!["primary", "haiku", "gpt"]);
        assert_eq!(choices.primary().driver, "murmur-driver-anthropic");
        assert_eq!(choices.primary().model, "claude-sonnet-4-5");
        assert_eq!(choices.index_of("gpt"), Some(2));
        assert_eq!(choices.index_of("murmur-driver-openai"), None);
        assert!(choices.has_alternates());
        assert_eq!(choices.names(), "primary, haiku, gpt");
    }

    #[test]
    fn driver_choice_without_alternates_is_the_primary_alone() {
        let mut bare = inference();
        bare.alternates.clear();
        let choices = DriverChoices::declared(&bare);
        assert!(!choices.has_alternates());
        assert_eq!(choices.get(1), None);
    }

    /// A credential is recorded per driver, so two choices on one driver share it.
    #[test]
    fn driver_choice_credential_is_recorded_for_every_choice_on_the_driver() {
        let mut choices = DriverChoices::declared(&inference());
        choices.record_credential("murmur-driver-anthropic", "config", None, None);
        choices.record_credential("murmur-driver-openai", "none", None, Some("OPENAI_API_KEY"));
        assert_eq!(choices.get(1).unwrap().credential_source, "config");
        assert!(choices.get(1).unwrap().staged());
        let gpt = choices.get(2).unwrap();
        assert!(!gpt.staged());
        assert_eq!(gpt.unresolved_credential.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(
            gpt.producer(),
            json!({"driver": "murmur-driver-openai", "model": "gpt-5"})
        );
        assert_eq!(
            choices.listing(DriverChoice::staged),
            vec![
                json!({"name": "primary", "driver": "murmur-driver-anthropic", "model": "claude-sonnet-4-5", "available": true}),
                json!({"name": "haiku", "driver": "murmur-driver-anthropic", "model": "claude-haiku-4-5", "available": true}),
                json!({"name": "gpt", "driver": "murmur-driver-openai", "model": "gpt-5", "available": false}),
            ]
        );
    }
}
