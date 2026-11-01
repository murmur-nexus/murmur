//! Tools that are a provider's own tools, and the safety checks a driver attaches to a call.
//!
//! A `runtime: tool` artifact whose bundled `murmur.yaml` declares `provider_tool:`
//! ([`murmur_artifact::ProviderTool`]) is one the model's provider defines itself, of a kind in
//! [`ProviderToolKind::ALL`]. The runtime reads the block, checks it, and passes it to the driver
//! on the tool's tool-array entry. It names no provider: which tool a driver sends for the block is
//! the driver's to decide.
//!
//! Declaring one constrains the launch:
//!
//! | Term | Refusal |
//! |---|---|
//! | the effective declared containment floor is `sealed` | `E-CAP-003`, [`RuntimeError::ProviderToolRequiresSealed`] |
//! | no `transport: http` driver choice runs a model its driver marks `vision: false` | `E-RUN-057`, [`RuntimeError::ProviderToolNeedsVision`] |
//! | the block parses | `E-RUN-058`, [`RuntimeError::ProviderToolMalformed`] |
//!
//! A runtime pull of such a tool is refused on the first two terms ([`ProviderToolTerms`]). When
//! a `sealed` capsule is refused because the host cannot back it (`E-CAP-003`) or cannot give its
//! subprocess tree a network namespace (`E-CAP-005`), the refusal names each declared provider
//! tool with a `required by:` line ([`required_by_lines`]).
//!
//! A driver may attach `safety_checks` to a `tool_call` block ([`read_safety_checks`]). The agent
//! loop then asks the person before the call runs ([`ask_safety_checks`]); only a reply that is
//! one of [`ACKNOWLEDGING_REPLIES`] runs it.

use murmur_artifact::{
    current_platform, ArtifactName, ArtifactRuntime, ContainmentClass, ProviderTool,
    ProviderToolKind,
};
use serde_json::Value;

use crate::{errors::RuntimeError, types::ArtifactRequest};

/// One declared tool that is a provider tool: its tool name and its kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderToolUse {
    /// The tool name, as the model's tool array names it.
    pub tool: String,
    pub kind: ProviderToolKind,
}

impl std::fmt::Display for ProviderToolUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool '{}' (provider_tool: {})", self.tool, self.kind)
    }
}

/// One `required by:` line per tool, each on its own line under a refusal's message, or `""` for
/// none: the suffix `E-CAP-003` and `E-CAP-005` append to their message.
#[must_use]
pub fn required_by_lines(tools: &[ProviderToolUse]) -> String {
    tools
        .iter()
        .map(|tool| format!("\n  required by: {tool}"))
        .collect()
}

/// A driver choice whose driver marks its model `vision: false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoVisionChoice {
    /// [`murmur_artifact::PRIMARY_DRIVER_CHOICE`] or the alternate's `name`.
    pub choice: String,
    pub model: String,
    /// The driver artifact as `name@version`.
    pub driver: String,
}

/// What a runtime pull of a provider tool is checked against: the session's effective declared
/// floor and the driver choices whose model has no vision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderToolTerms {
    /// The effective declared containment floor. [`ContainmentClass::Advisory`] by default, which
    /// refuses every provider tool pull.
    pub(crate) declared_floor: ContainmentClass,
    /// What the host can back, named by the sealed refusal's hint.
    pub(crate) host_can_back: ContainmentClass,
    /// Every `transport: http` driver choice whose model its driver marks `vision: false`,
    /// primary first.
    pub(crate) no_vision: Vec<NoVisionChoice>,
    /// The provider tools the session staged with, for the `required by:` line of a refusal made
    /// after staging.
    pub(crate) declared: Vec<ProviderToolUse>,
}

impl ProviderToolTerms {
    /// Whether a tool declaring `provider_tool` may join this session. `Err` is the refusal, the
    /// same one staging makes: the floor first, then vision.
    pub(crate) fn admit(&self, tool: &ProviderToolUse) -> Result<(), RuntimeError> {
        check_sealed(
            std::slice::from_ref(tool),
            self.declared_floor,
            self.host_can_back,
        )?;
        check_vision(std::slice::from_ref(tool), &self.no_vision)
    }
}

/// Whether `manifest_yaml` can declare a provider tool at all: a manifest whose text never names
/// the key is not parsed.
fn may_declare(manifest_yaml: &str) -> bool {
    manifest_yaml.contains(murmur_artifact::PROVIDER_TOOL_KEY)
}

/// The provider tools among `manifests`, `(tool name, manifest YAML)` in staging order, each
/// labelled for a refusal by `label` (`name@version`).
///
/// A manifest whose text does not contain `provider_tool` is skipped unparsed. A block that does
/// not parse, or sits on an artifact that is not a tool, is refused with
/// [`RuntimeError::ProviderToolMalformed`].
pub(crate) fn declared_provider_tools<'a>(
    manifests: impl IntoIterator<Item = (&'a str, &'a str)>,
    label: impl Fn(&str) -> String,
) -> Result<Vec<ProviderToolUse>, RuntimeError> {
    let mut tools = Vec::new();
    for (tool, manifest_yaml) in manifests {
        if !may_declare(manifest_yaml) {
            continue;
        }
        match murmur_artifact::parse_provider_tool(manifest_yaml) {
            Ok(Some(declared)) => tools.push(ProviderToolUse {
                tool: tool.to_string(),
                kind: declared.kind,
            }),
            Ok(None) => {}
            Err(error) => {
                return Err(RuntimeError::ProviderToolMalformed {
                    artifact: label(tool),
                    reason: error.to_string(),
                })
            }
        }
    }
    Ok(tools)
}

/// The provider tools among `artifacts` as the local store holds them, for the `required by:`
/// line of a refusal made before staging pulls anything.
///
/// Best-effort and read-only: every declared tool that `registry` resolves, whose manifest reads
/// and whose block parses, is named. A tool that fails any step is left out, and nothing is
/// fetched (staging's registry reads the project and global stores only).
pub(crate) fn provider_tools_in_store(
    registry: &dyn murmur_artifact::Registry,
    artifacts: &[ArtifactRequest],
) -> Vec<ProviderToolUse> {
    artifacts
        .iter()
        .filter(|artifact| {
            matches!(artifact.runtime, ArtifactRuntime::Tool) && artifact.source.is_none()
        })
        .filter_map(|artifact| {
            let name = ArtifactName::parse(&artifact.name).ok()?;
            let resolved = registry
                .resolve_with_platform(&name, &artifact.version, Some(current_platform()))
                .ok()?;
            let manifest_yaml = crate::artifact::extract_manifest_yaml(
                &artifact.name,
                &artifact.version,
                &resolved.bytes,
            )
            .ok()?;
            if !may_declare(&manifest_yaml) {
                return None;
            }
            let declared = murmur_artifact::parse_provider_tool(&manifest_yaml).ok()??;
            Some(ProviderToolUse {
                tool: murmur_artifact::tool_name_of(&artifact.name).to_string(),
                kind: declared.kind,
            })
        })
        .collect()
}

/// Refuses `tools` under an effective declared floor below `sealed`. Decided against the declared
/// floor alone: the floor is never raised for them.
pub(crate) fn check_sealed(
    tools: &[ProviderToolUse],
    declared: ContainmentClass,
    host_can_back: ContainmentClass,
) -> Result<(), RuntimeError> {
    if tools.is_empty() || declared >= ContainmentClass::Sealed {
        return Ok(());
    }
    Err(RuntimeError::ProviderToolRequiresSealed {
        tools: tools.to_vec(),
        declared,
        host_can_back,
    })
}

/// Refuses a computer tool when a driver choice runs a model its driver marks `vision: false`,
/// naming the first such tool and the first such choice.
pub(crate) fn check_vision(
    tools: &[ProviderToolUse],
    no_vision: &[NoVisionChoice],
) -> Result<(), RuntimeError> {
    let computer = tools
        .iter()
        .find(|tool| tool.kind == ProviderToolKind::Computer);
    match (computer, no_vision.first()) {
        (Some(tool), Some(choice)) => Err(RuntimeError::ProviderToolNeedsVision {
            tool: tool.tool.clone(),
            kind: tool.kind,
            model: choice.model.clone(),
            choice: choice.choice.clone(),
            driver: choice.driver.clone(),
        }),
        _ => Ok(()),
    }
}

/// Reads a pulled payload's manifest as [`ProviderToolTerms::admit`] needs it: `None` when it
/// declares no provider tool.
pub(crate) fn pulled_provider_tool(
    tool: &str,
    label: &str,
    manifest_yaml: &str,
) -> Result<Option<ProviderToolUse>, RuntimeError> {
    if !may_declare(manifest_yaml) {
        return Ok(None);
    }
    match murmur_artifact::parse_provider_tool(manifest_yaml) {
        Ok(declared) => Ok(declared.map(|declared: ProviderTool| ProviderToolUse {
            tool: tool.to_string(),
            kind: declared.kind,
        })),
        Err(error) => Err(RuntimeError::ProviderToolMalformed {
            artifact: label.to_string(),
            reason: error.to_string(),
        }),
    }
}

/// The key of a `tool_call` block that carries its safety checks.
pub(crate) const SAFETY_CHECKS_KEY: &str = "safety_checks";

/// The key of a tool message that carries the checks the person acknowledged, as the driver sent
/// them.
pub(crate) const ACKNOWLEDGED_SAFETY_CHECKS_KEY: &str = "acknowledged_safety_checks";

/// The replies, trimmed and ASCII-lowercased, that acknowledge a safety check. Any other reply
/// declines it.
pub(crate) const ACKNOWLEDGING_REPLIES: [&str; 6] = [
    "yes",
    "y",
    "approve",
    "approved",
    "acknowledge",
    "acknowledged",
];

/// One check as the prompt and the not-run texts name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SafetyCheck {
    pub(crate) code: Option<String>,
    pub(crate) message: String,
}

impl SafetyCheck {
    /// `<code or ->: <message>`.
    fn line(&self) -> String {
        format!("{}: {}", self.code.as_deref().unwrap_or("-"), self.message)
    }
}

/// What a `tool_call` block's `safety_checks` holds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SafetyChecks {
    /// Absent, or `[]`.
    None,
    /// At least one well-formed check. `verbatim` is the array as the driver sent it.
    Checks {
        verbatim: Value,
        checks: Vec<SafetyCheck>,
    },
    /// Anything else. `verbatim` is the value as the driver sent it.
    Malformed { verbatim: Value },
}

/// Reads `block`'s `safety_checks`: an array of `{"id"?: string, "code"?: string, "message":
/// non-empty string}` objects. An absent key and `[]` are no checks; any other shape, `null`
/// included, is malformed.
pub(crate) fn read_safety_checks(block: &Value) -> SafetyChecks {
    let Some(verbatim) = block.get(SAFETY_CHECKS_KEY) else {
        return SafetyChecks::None;
    };
    let malformed = || SafetyChecks::Malformed {
        verbatim: verbatim.clone(),
    };
    let Some(entries) = verbatim.as_array() else {
        return malformed();
    };
    if entries.is_empty() {
        return SafetyChecks::None;
    }
    let mut checks = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(object) = entry.as_object() else {
            return malformed();
        };
        let optional = |key: &str| match object.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(()),
        };
        let (Ok(_id), Ok(code)) = (optional("id"), optional("code")) else {
            return malformed();
        };
        let Some(message) = object
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
        else {
            return malformed();
        };
        checks.push(SafetyCheck {
            code,
            message: message.to_string(),
        });
    }
    SafetyChecks::Checks {
        verbatim: verbatim.clone(),
        checks,
    }
}

/// The `input-required` prompt for one call's checks.
pub(crate) fn safety_prompt(tool: &str, call_id: &str, checks: &[SafetyCheck]) -> String {
    let mut prompt = format!("safety check on tool call '{tool}' ({call_id}):\n");
    for check in checks {
        prompt.push_str(&check.line());
        prompt.push('\n');
    }
    prompt.push_str("reply yes to acknowledge and run the action; any other reply declines it");
    prompt
}

/// Whether `reply` acknowledges the checks.
pub(crate) fn acknowledges(reply: &str) -> bool {
    ACKNOWLEDGING_REPLIES.contains(&reply.trim().to_ascii_lowercase().as_str())
}

/// How one call's safety checks ended, as `safety_check.outcome` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SafetyOutcome {
    Acknowledged,
    /// Any reply that does not acknowledge. The reply goes into the model's text, never the
    /// trace.
    Declined {
        reply: String,
    },
    TimedOut,
    Canceled,
    /// No task in scope, so no person to ask.
    NoInputChannel,
    Malformed,
}

impl SafetyOutcome {
    pub(crate) fn wire_name(&self) -> &'static str {
        match self {
            Self::Acknowledged => "acknowledged",
            Self::Declined { .. } => "declined",
            Self::TimedOut => "timed_out",
            Self::Canceled => "canceled",
            Self::NoInputChannel => "no_input_channel",
            Self::Malformed => "malformed",
        }
    }

    /// The tool message text of a call that did not run, or `None` for an acknowledged one.
    pub(crate) fn not_run_text(&self, checks: &[SafetyCheck]) -> Option<String> {
        let named = checks
            .iter()
            .map(SafetyCheck::line)
            .collect::<Vec<_>>()
            .join("; ");
        match self {
            Self::Acknowledged => None,
            Self::Declined { reply } => Some(format!(
                "the person did not acknowledge the safety check on this call, so it was not \
                 run: {named}; their reply: {reply}"
            )),
            Self::TimedOut => Some(format!(
                "the person did not answer the safety check on this call within \
                 lifecycle.input_timeout_secs, so it was not run: {named}"
            )),
            Self::Canceled => Some(format!(
                "the task was canceled while the safety check on this call waited for an answer, \
                 so it was not run: {named}"
            )),
            Self::NoInputChannel => Some(format!(
                "this call carries a safety check and this task has no way to ask a person, so it \
                 was not run: {named}"
            )),
            Self::Malformed => Some(
                "the driver returned a malformed safety check with this call, so it was not run"
                    .to_string(),
            ),
        }
    }
}

/// Asks the person in scope about one call's `checks` and waits for the answer, on the same wait
/// `request-input` uses ([`crate::runtime::wait_for_input`]).
///
/// [`SafetyOutcome::NoInputChannel`] without asking when no task is in scope. A wait that cannot
/// start (the task is not running) is treated the same way: the call does not run.
pub(crate) async fn ask_safety_checks(
    state: &crate::runtime::CapsuleStoreState,
    tool: &str,
    call_id: &str,
    checks: &[SafetyCheck],
) -> SafetyOutcome {
    let (Some(task), Some(registry)) = (state.a2a_task.clone(), state.a2a_task_registry.clone())
    else {
        return SafetyOutcome::NoInputChannel;
    };
    let prompt = safety_prompt(tool, call_id, checks);
    match crate::runtime::wait_for_input(
        task,
        prompt,
        registry,
        state.a2a_sse.clone(),
        state.input_timeout_secs,
    )
    .await
    {
        Ok(reply) if acknowledges(&reply) => SafetyOutcome::Acknowledged,
        Ok(reply) => SafetyOutcome::Declined { reply },
        Err(crate::runtime::InputWaitEnd::Canceled) => SafetyOutcome::Canceled,
        Err(crate::runtime::InputWaitEnd::TimedOut) => SafetyOutcome::TimedOut,
        Err(crate::runtime::InputWaitEnd::NotWaiting(_)) => SafetyOutcome::NoInputChannel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMPUTER: &str = "name: computer\nversion: 0.1.0\nruntime: tool\nprovider_tool:\n  \
                            kind: computer\n  display_width: 1280\n  display_height: 800\n";
    const PLAIN: &str = "name: plain\nversion: 0.1.0\nruntime: tool\n";

    fn computer(tool: &str) -> ProviderToolUse {
        ProviderToolUse {
            tool: tool.to_string(),
            kind: ProviderToolKind::Computer,
        }
    }

    #[test]
    fn required_by_lines_name_each_computer_tool_from_the_manifests() {
        let tools = declared_provider_tools(
            [
                ("plain", PLAIN),
                ("computer", COMPUTER),
                ("second", COMPUTER),
            ],
            |tool| format!("local/{tool}@0.1.0"),
        )
        .unwrap();
        assert_eq!(tools, vec![computer("computer"), computer("second")]);
        assert_eq!(
            required_by_lines(&tools),
            "\n  required by: tool 'computer' (provider_tool: computer)\n  required by: tool \
             'second' (provider_tool: computer)"
        );
    }

    #[test]
    fn required_by_lines_are_empty_without_a_provider_tool() {
        let tools = declared_provider_tools([("plain", PLAIN)], |tool| tool.to_string()).unwrap();
        assert!(tools.is_empty());
        assert_eq!(required_by_lines(&tools), "");
    }

    #[test]
    fn required_by_a_failed_store_read_yields_no_line() {
        struct EmptyStore;
        impl murmur_artifact::Registry for EmptyStore {
            fn resolve(
                &self,
                name: &ArtifactName,
                version: &str,
            ) -> Result<murmur_artifact::ResolvedArtifact, murmur_artifact::RegistryError>
            {
                Err(murmur_artifact::RegistryError::NotFound {
                    name: name.to_string(),
                    version: version.to_string(),
                })
            }
            fn publish(
                &self,
                _: murmur_artifact::ArtifactMeta,
                _: &[u8],
            ) -> Result<murmur_artifact::PublishResult, murmur_artifact::RegistryError>
            {
                unreachable!("never published")
            }
            fn list_index(
                &self,
            ) -> Result<Vec<murmur_artifact::ArtifactMeta>, murmur_artifact::RegistryError>
            {
                Ok(Vec::new())
            }
        }
        let artifacts = vec![ArtifactRequest {
            name: "local/computer".to_string(),
            version: "0.1.0".to_string(),
            runtime: ArtifactRuntime::Tool,
            source: None,
            on_overflow: Default::default(),
            config: None,
            gateway: None,
            capabilities: None,
        }];
        let tools = provider_tools_in_store(&EmptyStore, &artifacts);
        assert!(tools.is_empty());
        assert_eq!(required_by_lines(&tools), "");
    }

    #[test]
    fn required_by_a_malformed_block_is_refused_naming_the_artifact_and_field() {
        let error = declared_provider_tools(
            [(
                "computer",
                "name: computer\nversion: 0.1.0\nruntime: tool\nprovider_tool:\n  kind: \
                 computer\n  display_width: 0\n  display_height: 800\n",
            )],
            |tool| format!("local/{tool}@0.1.0"),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("local/computer@0.1.0")
                && message.contains("provider_tool.display_width"),
            "{message}"
        );
    }

    #[test]
    fn provider_tool_sealed_is_decided_on_the_declared_floor() {
        let tools = [computer("a"), computer("b")];
        for floor in [ContainmentClass::Advisory, ContainmentClass::Scoped] {
            let error = check_sealed(&tools, floor, ContainmentClass::Sealed).unwrap_err();
            let message = error.to_string();
            assert!(
                message.contains(
                    "tool 'a' (provider_tool: computer), tool 'b' (provider_tool: \
                                  computer)"
                ) && message.contains(&format!("'{floor}'")),
                "{message}"
            );
        }
        assert!(check_sealed(&tools, ContainmentClass::Sealed, ContainmentClass::Scoped).is_ok());
        assert!(check_sealed(&[], ContainmentClass::Advisory, ContainmentClass::Sealed).is_ok());
    }

    #[test]
    fn provider_tool_vision_refuses_only_a_declared_false() {
        let blind = NoVisionChoice {
            choice: "primary".to_string(),
            model: "probe-model".to_string(),
            driver: "local/drv@0.1.0".to_string(),
        };
        let message = check_vision(&[computer("computer")], std::slice::from_ref(&blind))
            .unwrap_err()
            .to_string();
        assert_eq!(
            message,
            "tool 'computer' is a computer (provider_tool: computer) but model 'probe-model' of \
             driver choice 'primary' has no vision: driver local/drv@0.1.0 declares vision: \
             false for it"
        );
        assert!(check_vision(&[computer("computer")], &[]).is_ok());
        assert!(check_vision(&[], &[blind]).is_ok());
    }

    fn checks_of(block: Value) -> Vec<SafetyCheck> {
        match read_safety_checks(&block) {
            SafetyChecks::Checks { checks, .. } => checks,
            other => panic!("{block}: {other:?}"),
        }
    }

    #[test]
    fn safety_checks_malformed_shapes_are_refused() {
        for checks in [
            json!("check this"),
            json!([1]),
            json!([{"code": "c"}]),
            json!([{"code": "c", "message": ""}]),
            json!([{"code": "c", "message": "   "}]),
            json!([{"code": 3, "message": "m"}]),
            json!([{"id": false, "message": "m"}]),
            json!({"message": "m"}),
            json!(null),
        ] {
            let block = json!({"type": "tool_call", "safety_checks": checks});
            assert_eq!(
                read_safety_checks(&block),
                SafetyChecks::Malformed { verbatim: checks },
            );
        }
        assert_eq!(
            SafetyOutcome::Malformed.not_run_text(&[]).as_deref(),
            Some("the driver returned a malformed safety check with this call, so it was not run")
        );
    }

    #[test]
    fn safety_checks_absent_or_empty_are_none() {
        assert_eq!(
            read_safety_checks(&json!({"type": "tool_call"})),
            SafetyChecks::None
        );
        assert_eq!(
            read_safety_checks(&json!({"type": "tool_call", "safety_checks": []})),
            SafetyChecks::None
        );
    }

    #[test]
    fn safety_checks_are_kept_verbatim() {
        let checks = json!([
            {"id": "sc_1", "code": "irrelevant_domain", "message": "The page is not the one the task named.", "extra": 1}
        ]);
        let block = json!({"type": "tool_call", "safety_checks": checks.clone()});
        match read_safety_checks(&block) {
            SafetyChecks::Checks { verbatim, .. } => assert_eq!(verbatim, checks),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn safety_prompt_for_two_checks_matches_line_by_line() {
        let checks = checks_of(json!({"safety_checks": [
            {"id": "sc_1", "code": "irrelevant_domain", "message": "The page is not the one the task named."},
            {"id": "sc_2", "message": "A payment form is open."}
        ]}));
        let prompt = safety_prompt("computer", "call_0_0", &checks);
        assert_eq!(
            prompt.lines().collect::<Vec<_>>(),
            vec![
                "safety check on tool call 'computer' (call_0_0):",
                "irrelevant_domain: The page is not the one the task named.",
                "-: A payment form is open.",
                "reply yes to acknowledge and run the action; any other reply declines it",
            ]
        );
    }

    #[test]
    fn safety_replies_acknowledge_only_the_yes_words() {
        for reply in [
            "yes",
            " YES ",
            "y",
            "Approve",
            "approved",
            "acknowledge",
            "acknowledged",
        ] {
            assert!(acknowledges(reply), "{reply:?}");
        }
        for reply in ["yes please", "no", "", "ok", "sure", "no, wrong site"] {
            assert!(!acknowledges(reply), "{reply:?}");
        }
    }

    #[test]
    fn safety_not_run_texts_are_as_documented() {
        let checks = checks_of(json!({"safety_checks": [
            {"code": "irrelevant_domain", "message": "The page is not the one the task named."}
        ]}));
        assert_eq!(SafetyOutcome::Acknowledged.not_run_text(&checks), None);
        assert_eq!(
            SafetyOutcome::Declined {
                reply: "no, wrong site".to_string()
            }
            .not_run_text(&checks)
            .unwrap(),
            "the person did not acknowledge the safety check on this call, so it was not run: \
             irrelevant_domain: The page is not the one the task named.; their reply: no, wrong \
             site"
        );
        assert_eq!(
            SafetyOutcome::NoInputChannel.not_run_text(&checks).unwrap(),
            "this call carries a safety check and this task has no way to ask a person, so it was \
             not run: irrelevant_domain: The page is not the one the task named."
        );
        for (outcome, wire) in [
            (SafetyOutcome::Acknowledged, "acknowledged"),
            (
                SafetyOutcome::Declined {
                    reply: String::new(),
                },
                "declined",
            ),
            (SafetyOutcome::TimedOut, "timed_out"),
            (SafetyOutcome::Canceled, "canceled"),
            (SafetyOutcome::NoInputChannel, "no_input_channel"),
            (SafetyOutcome::Malformed, "malformed"),
        ] {
            assert_eq!(outcome.wire_name(), wire);
        }
    }

    #[test]
    fn safety_without_a_task_in_scope_is_no_input_channel() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = crate::runtime::build_test_state(
            std::sync::Arc::new(murmur_artifact::LocalRegistry::new(
                dir.path().join("registry"),
            )),
            dir.path().to_path_buf(),
            dir.path().join("murmur.lock"),
        );
        assert!(state.a2a_task.is_none());
        let checks = checks_of(json!({"safety_checks": [{"message": "m"}]}));
        let outcome = rt.block_on(ask_safety_checks(&state, "computer", "call_0_0", &checks));
        assert_eq!(outcome, SafetyOutcome::NoInputChannel);
    }
}
