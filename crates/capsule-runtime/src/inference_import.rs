//! Host implementation of `murmur:runtime/inference@0.4.0#run-inference`.
//!
//! A hook component that imports this interface can run exactly one LLM
//! completion through the capsule's already-configured inference driver. The
//! call goes through [`crate::runtime::invoke_tool_component`] — the same
//! instantiate/type-check/call body an ordinary agent-loop turn uses — so there
//! is no second HTTP client, no second credential path, and no duplicated
//! driver logic. See `wit/hook/inference.wit` for the contract.

use std::{path::PathBuf, sync::Arc, time::Instant};

use serde_json::Value;
use wasmtime::{
    component::{Component, Linker},
    Engine,
};

use crate::{
    agent::{build_driver_payload, count_tokens, parse_driver_usage, provider_counts, DriverUsage},
    bindings::{
        hook::murmur::runtime::inference::{InferenceRequest, InferenceResponse},
        host::murmur::tool::run::{Status, ToolInput},
    },
    network_policy::{NetworkAllowRule, ToolCapabilityGrant},
    runtime::{invoke_tool_component, ToolA2aWiring, ToolInvokeEnv},
    spend::{SpendMeter, SpendRefusal},
    trace::{
        recorded_driver_config, InferenceCallSettings, InferenceOrigin,
        INFERENCE_ERROR_CREDENTIAL_REJECTED, INFERENCE_ERROR_DRIVER_ERROR,
        INFERENCE_ERROR_DRIVER_FAILED, INFERENCE_ERROR_MALFORMED_RESPONSE,
    },
    types::CapabilityPolicy,
};

/// The versioned instance name the host provides `run-inference` under. Hook
/// components that do not import it simply ignore the registration.
pub(crate) const INFERENCE_IFACE_VERSIONED: &str = "murmur:runtime/inference@0.4.0";

/// One completed `run-inference` call, buffered for the agent loop to write
/// through the session's `TraceWriter`/`OtelEmitter`.
///
/// Written for **every** call, success or failure, so a hook that retries after
/// a failure produces two separate, truthful records rather than one relabelled
/// span.
#[derive(Debug, Clone)]
pub(crate) struct HookInferenceRecord {
    /// Carried verbatim into `trace.jsonl` and the OTel `capsule.inference`
    /// span, distinguishing this from an agent-loop turn.
    pub(crate) origin: InferenceOrigin,
    pub(crate) input_tokens: u64,
    /// The runtime's tiktoken count of the completion. `None` for a failed call, which
    /// returned none.
    pub(crate) output_tokens: Option<u64>,
    /// `"end_turn"` when the driver returned a usable completion, `"error"`
    /// otherwise.
    pub(crate) decision: String,
    pub(crate) duration_ms: u64,
    /// The provider's own counts, when the driver reported them. Recorded beside
    /// `input_tokens`/`output_tokens` — which stay this runtime's tiktoken counts —
    /// and never substituted for them. For a failed call, only a `usage` block the
    /// driver's error payload carried.
    pub(crate) usage: Option<DriverUsage>,
    /// Why the call failed. `Some` exactly when `decision` is `"error"`.
    pub(crate) failure: Option<HookInferenceFailure>,
    /// The cap the payload carried and the driver config the call ran under, failed or not.
    pub(crate) settings: InferenceCallSettings,
}

/// Why one `run-inference` call failed: what the guest is told, and what the trace records.
#[derive(Debug, Clone)]
pub(crate) struct HookInferenceFailure {
    /// One of the `INFERENCE_ERROR_*` constants.
    pub(crate) code: &'static str,
    /// The `err` string handed back to the guest.
    pub(crate) message: String,
    /// The `inference.error` text: the rejection message for a credential rejection, otherwise
    /// `message` with the context gateway's credential redacted.
    pub(crate) error: String,
    /// The HTTP status the context's gateway received for the call, when it received one.
    pub(crate) provider_status: Option<u16>,
    /// A `usage` block the driver's error payload carried.
    pub(crate) usage: Option<DriverUsage>,
}

/// One `run-inference` call a spend ceiling refused before dispatch, buffered for the agent loop
/// to write as a `spend_ceiling_reached` trace line. No [`HookInferenceRecord`] accompanies it:
/// no call was made.
#[derive(Debug, Clone)]
pub(crate) struct SpendRefusalRecord {
    /// `hook:<name>` of the hook whose call was refused.
    pub(crate) origin: String,
    pub(crate) refusal: SpendRefusal,
}

/// What a hook's `run-inference` call is made for, read from the calling hook's store: the
/// lifecycle event that store is serving when the call arrives.
///
/// Kept per store rather than per session because an async hook on another event can call
/// `run-inference` while a compaction is being dispatched, and that call is not a compaction
/// call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HookInferencePurpose {
    /// Any event but `on-compaction`, and `on-stage`.
    #[default]
    Lifecycle,
    /// `on-compaction`: the call sends `inference.compaction.max_tokens` and runs the driver
    /// under `inference.compaction.driver_config`.
    Compaction,
}

/// Everything the host needs to run one inference-driver call from inside a
/// hook's wasm call.
///
/// The fields are owned clones of `CapsuleStoreState`'s, not borrows: the agent
/// loop holds the capsule store `&mut` for the whole turn that dispatches hooks,
/// and a `Linker` closure registered once at hook-instantiation time has to
/// outlive any single call anyway.
pub(crate) struct HookInferenceCtx {
    pub(crate) driver_name: String,
    pub(crate) driver_component: Component,
    /// The manifest's primary `inference.model` — what `model: none` resolves to.
    pub(crate) model: String,
    /// The output cap each completion sends and admits spend against, by the model it calls and
    /// what it is called for.
    pub(crate) output_caps: crate::model_limits::HookOutputCaps,
    pub(crate) engine: Engine,
    pub(crate) accessible_workdir: PathBuf,
    /// The session's records directory, where a credential failure writes `out/result.txt`.
    pub(crate) records_root: crate::workdir_fs::WorkdirRoot,
    pub(crate) inference_env: Vec<(String, String)>,
    /// The driver's environment for a [`HookInferencePurpose::Compaction`] call:
    /// [`Self::inference_env`] with `inference.compaction.driver_config` laid over its
    /// `MURMUR_INFERENCE_DRIVER_CONFIG`. Equal to it when the manifest sets no overlay.
    pub(crate) compaction_inference_env: Vec<(String, String)>,
    pub(crate) capability_policy: CapabilityPolicy,
    pub(crate) network_allow_rules: Vec<NetworkAllowRule>,
    /// The driver artifact's own per-artifact narrowing, cloned from the capsule store's
    /// grant map. `None` = the driver declared no `capabilities:` block, so the ceiling
    /// applies. Carried here so a hook's `run-inference` runs the driver under exactly the
    /// grant the agent loop's own turns do, rather than an unnarrowed copy of the ceiling.
    pub(crate) driver_grant: Option<ToolCapabilityGrant>,
    /// The session's inference gateway, so a hook's `run-inference` reaches the provider exactly
    /// as the agent loop's own turns do. Never another artifact's gateway.
    pub(crate) gateway: Option<Arc<crate::credential_gateway::CredentialGateway>>,
    /// Every gateway the session holds, read only by [`Self::redact`], so a record's error text
    /// and `driver_config` are redacted of every credential value the session holds, as the agent
    /// loop's own records are, not only the inference gateway's.
    pub(crate) gateways: crate::credential_gateway::GatewayTable,
    /// The session's formation membership, so the driver's guest reaches the callees its
    /// environment names exactly as the agent loop's own driver does.
    pub(crate) formation: Option<Arc<crate::formation_credentials::FormationMember>>,
    /// The session's spend account — the same one the agent loop admits its turns against — so a
    /// hook's completion counts toward, and is refused by, the same ceilings.
    pub(crate) spend: Arc<SpendMeter>,
    /// Buffered trace records, drained by the agent loop after hook dispatch.
    pub(crate) records: std::sync::Mutex<Vec<HookInferenceRecord>>,
    /// Calls refused by a spend ceiling, drained beside [`Self::records`].
    pub(crate) spend_refusals: std::sync::Mutex<Vec<SpendRefusalRecord>>,
}

impl HookInferenceCtx {
    /// Run one completion. `Err` is the string handed straight back to the guest
    /// as the `result`'s error case.
    async fn run(
        &self,
        origin: &str,
        purpose: HookInferencePurpose,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, String> {
        // `none` resolves to the manifest's primary model; `some(m)` is sent
        // verbatim and, if the driver rejects it, surfaces as `Err` with no
        // retry — a caller wanting fallback calls again with `none` itself.
        let messages = wire_messages(&request);
        let model = request.model.unwrap_or_else(|| self.model.clone());
        // `inference.compaction.max_tokens` for a compaction call when set; otherwise the primary
        // driver's declared ceiling for the model called, ahead of the manifest's
        // `inference.max_tokens`: that is the *agent turn* output cap, and a small value chosen
        // for the agent loop would silently truncate a compaction summary.
        let max_output_tokens = self.output_caps.for_call(&model, purpose);
        let payload = build_driver_payload(
            &model,
            max_output_tokens,
            &messages,
            &[],
            request.system_prompt.as_deref().unwrap_or(""),
            None,
            // A hook's `run-inference` shares no prompt prefix with the agent loop — its own
            // system prompt, no tools — so there is no prefix for a routing hint to point at,
            // and `HookInferenceCtx` carries no capsule name or version to build one from.
            None,
        );
        let payload_json = serde_json::to_string(&payload)
            .map_err(|e| format!("failed to encode driver payload: {e}"))?;
        let input_tokens = u64::from(count_tokens(&payload_json));

        // Refused before dispatch: the guest gets the refusal text as `err`, and the trace gets a
        // refusal line rather than an `inference` record for a call that never happened.
        let admission = match self.spend.admit(input_tokens, u64::from(max_output_tokens)) {
            Ok(admission) => admission,
            Err(refusal) => {
                let text = refusal.to_string();
                if let Ok(mut refusals) = self.spend_refusals.lock() {
                    refusals.push(SpendRefusalRecord {
                        origin: origin.to_string(),
                        refusal,
                    });
                }
                return Err(text);
            }
        };

        let inference_env = self.env_for(purpose);
        let settings = InferenceCallSettings {
            max_output_tokens,
            driver_config: recorded_driver_config(inference_env, |text| self.redact(text)),
        };
        let started = Instant::now();
        let outcome = self.dispatch(payload_json, inference_env).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let (output_tokens, usage, failure, result) = match outcome {
            Ok((raw, text, usage)) => {
                // `input-tokens`/`output-tokens` on the WIT response are documented as the
                // runtime's own tiktoken counts, so a driver-reported `usage` never lands
                // there. It goes to the trace record below, and its counts are what the spend
                // meter is charged when they make a report.
                let output_tokens = u64::from(count_tokens(&raw));
                match usage.as_ref().and_then(provider_counts) {
                    Some(counts) => admission.settle(counts.request, counts.completion),
                    None => admission.settle(input_tokens, output_tokens),
                }
                (
                    Some(output_tokens),
                    usage,
                    None,
                    Ok(InferenceResponse {
                        text,
                        // Echoed, not driver-confirmed — the response never
                        // names the model back.
                        model_used: model.clone(),
                        input_tokens,
                        output_tokens,
                    }),
                )
            }
            Err(failure) => {
                // A failed call is charged what the provider reported for it, and its input
                // estimate alone when it reported nothing.
                match failure.usage.as_ref().and_then(provider_counts) {
                    Some(counts) => admission.settle(counts.request, counts.completion),
                    None => drop(admission),
                }
                let message = failure.message.clone();
                (None, failure.usage.clone(), Some(*failure), Err(message))
            }
        };

        self.record(HookInferenceRecord {
            origin: InferenceOrigin {
                source: origin.to_string(),
                model,
            },
            input_tokens,
            output_tokens,
            decision: if result.is_ok() { "end_turn" } else { "error" }.to_string(),
            duration_ms,
            usage,
            failure,
            settings,
        });

        result
    }

    /// The driver environment a call made for `purpose` runs under.
    fn env_for(&self, purpose: HookInferencePurpose) -> &[(String, String)] {
        match purpose {
            HookInferencePurpose::Lifecycle => &self.inference_env,
            HookInferencePurpose::Compaction => &self.compaction_inference_env,
        }
    }

    /// Dispatch the payload through the shared tool-invocation path and pull the
    /// completion text and the optional `usage` block out of the driver's response.
    /// Returns `(raw, text, usage)`. The failure is boxed because it is far larger than the
    /// success value and is built only on the cold path.
    async fn dispatch(
        &self,
        payload_json: String,
        inference_env: &[(String, String)],
    ) -> Result<(String, String, Option<DriverUsage>), Box<HookInferenceFailure>> {
        // The status the gateway receives during this dispatch belongs to this call.
        if let Some(gateway) = &self.gateway {
            gateway.take_upstream_status();
        }
        let outcome = self.dispatch_driver(payload_json, inference_env).await;
        let provider_status = self
            .gateway
            .as_ref()
            .and_then(|gateway| gateway.take_upstream_status());
        outcome.map_err(|(code, message, usage)| {
            Box::new(HookInferenceFailure {
                code,
                error: match code {
                    INFERENCE_ERROR_CREDENTIAL_REJECTED => message.clone(),
                    _ => self.redact(&message),
                },
                message,
                provider_status,
                usage,
            })
        })
    }

    /// [`Self::dispatch`] without the status bookkeeping. `Err` is `(error_code, guest message,
    /// usage the failed payload carried)`.
    async fn dispatch_driver(
        &self,
        payload_json: String,
        inference_env: &[(String, String)],
    ) -> Result<(String, String, Option<DriverUsage>), (&'static str, String, Option<DriverUsage>)>
    {
        let result = invoke_tool_component(
            ToolInvokeEnv {
                engine: &self.engine,
                accessible_workdir: &self.accessible_workdir,
                // A driver is handed no runtime directory.
                runtime_dir: None,
                inference_env,
                capability_policy: &self.capability_policy,
                network_allow_rules: &self.network_allow_rules,
                artifact_grant: self.driver_grant.as_ref(),
                gateway: self.gateway.as_ref(),
                formation: self.formation.as_ref(),
                // A hook's completion runs for no task of its own, so a formation call from it
                // stamps `untrusted`.
                task_provenance: None,
            },
            // A hook's completion is not part of the user-facing turn: it must
            // not stream chunks into the SSE stream or ask the user for input.
            ToolA2aWiring::silent(),
            &self.driver_name,
            &self.driver_component,
            ToolInput {
                data: Some(payload_json),
                log_path: None,
            },
        )
        .await
        .map_err(|e| {
            (
                INFERENCE_ERROR_DRIVER_FAILED,
                format!("inference driver '{}' failed: {e}", self.driver_name),
                None,
            )
        })?;

        let passed = matches!(result.status, Status::Passed);
        let raw = result.data.or(result.summary).ok_or_else(|| {
            (
                if passed {
                    INFERENCE_ERROR_MALFORMED_RESPONSE
                } else {
                    INFERENCE_ERROR_DRIVER_ERROR
                },
                "inference driver returned no data".to_string(),
                None,
            )
        })?;
        if !passed {
            if let Some(message) = self.credential_failure() {
                return Err((INFERENCE_ERROR_CREDENTIAL_REJECTED, message, None));
            }
            let usage = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|response| parse_driver_usage(&response));
            return Err((
                INFERENCE_ERROR_DRIVER_ERROR,
                format!("inference driver returned an error: {raw}"),
                usage,
            ));
        }

        let response: Value = serde_json::from_str(&raw).map_err(|e| {
            (
                INFERENCE_ERROR_MALFORMED_RESPONSE,
                format!("failed to parse inference driver response: {e}"),
                None,
            )
        })?;
        if response.get("stop_reason").and_then(Value::as_str) == Some("error") {
            let usage = parse_driver_usage(&response);
            if let Some(message) = self.credential_failure() {
                return Err((INFERENCE_ERROR_CREDENTIAL_REJECTED, message, usage));
            }
            let err = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("driver returned error");
            return Err((
                INFERENCE_ERROR_DRIVER_ERROR,
                format!("inference driver returned an error: {err}"),
                usage,
            ));
        }

        let text = response_text(&response);
        let usage = parse_driver_usage(&response);
        Ok((raw, text, usage))
    }

    /// A credential rejection the gateway recorded for this call, reported as `E-RUN-027` and
    /// written to `out/result.txt` in place of the driver's error, or `None` when there is none.
    fn credential_failure(&self) -> Option<String> {
        let message = self
            .gateway
            .as_ref()
            .and_then(|gateway| gateway.credential())
            .and_then(|credential| credential.report_rejection())?;
        if let Err(err) =
            crate::agent::write_result(&self.records_root, &format!("error: {message}"))
        {
            crate::runtime_err!("{err}");
        }
        Some(message)
    }

    /// `text` with every credential value the session holds redacted.
    fn redact(&self, text: &str) -> String {
        self.gateways.redact(text)
    }

    fn record(&self, record: HookInferenceRecord) {
        if let Ok(mut guard) = self.records.lock() {
            guard.push(record);
        }
    }

    /// Take every record buffered since the last drain.
    pub(crate) fn drain_records(&self) -> Vec<HookInferenceRecord> {
        self.records
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    /// Take every spend refusal buffered since the last drain.
    pub(crate) fn drain_spend_refusals(&self) -> Vec<SpendRefusalRecord> {
        self.spend_refusals
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }
}

/// Convert a request's WIT `message` records into driver wire messages.
///
/// The WIT `message.content` is a plain `string`, but the driver deserializes
/// each message's `content` into a `Vec<MurmurContentBlock>` — a bare string
/// fails serde with `invalid type: string, expected a sequence`. Wrap each
/// message's content in a single text block, matching `build_driver_payload`
/// and what the agent loop already sends every driver. `role` is passed through
/// unchanged: `murmur-hook-compact` has already normalized roles to
/// `user`/`assistant` before calling `run-inference`.
fn wire_messages(request: &InferenceRequest) -> Vec<Value> {
    request
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": [{"type": "text", "text": m.content}],
            })
        })
        .collect()
}

/// Concatenate the `text` blocks of a driver response's `content` array, in
/// order. Matches the wire format in `docs/murmur-inference-message-format.md`.
fn response_text(response: &Value) -> String {
    response
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Why a session's hooks have no inference driver behind `run-inference`. Every call answers
/// `err` with [`Self::message`]; nothing is sent, admitted against a spend ceiling, or recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InferenceUnavailable {
    /// No callable driver is staged — including under `on-stage`, which runs before one exists.
    NotConfigured,
    /// The capsule runs under `inference.transport: process`: the harness runs the model.
    ProcessTransport,
}

impl InferenceUnavailable {
    /// The `err` string every `run-inference` call receives.
    pub(crate) fn message(self) -> &'static str {
        match self {
            // Wording tracks RuntimeError::DriverNotConfigured.
            Self::NotConfigured => {
                "inference driver is not configured; add inference.driver.artifact to murmur.yaml"
            }
            Self::ProcessTransport => {
                "run-inference is not available under inference.transport: process: the harness \
                 runs the model, and this capsule has no inference driver the runtime can call"
            }
        }
    }
}

/// Register `murmur:runtime/inference@0.4.0#run-inference` on a hook linker.
///
/// `origin` is the `hook:<name>` tag attached to every trace record this hook's
/// calls produce. `purpose` reads, from the calling store, what the call is made for.
/// `ctx` is `Err` when the capsule has no inference driver a hook
/// can call: the import is still *defined* (so a hook that imports it links and
/// runs), it just always returns the reason's `err`.
pub(crate) fn add_inference_to_linker<T: 'static>(
    linker: &mut Linker<T>,
    origin: String,
    purpose: fn(&T) -> HookInferencePurpose,
    ctx: Result<Arc<HookInferenceCtx>, InferenceUnavailable>,
) -> Result<(), String> {
    let origin = Arc::new(origin);
    linker
        .instance(INFERENCE_IFACE_VERSIONED)
        .map_err(|e| format!("failed to define {INFERENCE_IFACE_VERSIONED}: {e}"))?
        .func_wrap_async(
            "run-inference",
            move |store: wasmtime::StoreContextMut<'_, T>, (request,): (InferenceRequest,)| {
                let purpose = purpose(store.data());
                let ctx = ctx.clone();
                let origin = Arc::clone(&origin);
                let fut: Box<
                    dyn std::future::Future<
                            Output = wasmtime::Result<(Result<InferenceResponse, String>,)>,
                        > + Send,
                > = Box::new(async move {
                    let result = match ctx {
                        Ok(ctx) => ctx.run(&origin, purpose, request).await,
                        Err(reason) => Err(reason.message().to_string()),
                    };
                    Ok((result,))
                });
                fut
            },
        )
        .map_err(|e| {
            format!("failed to register {INFERENCE_IFACE_VERSIONED}#run-inference: {e}")
        })?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use wasmtime::component::Component;

    /// A hand-authored `murmur:tool/run@0.1.0` component double standing in for
    /// an inference driver: it ignores its input entirely and returns `response`
    /// as the `tool-result.data` string with the given `status`.
    ///
    /// Written in WAT rather than compiled from Rust because this crate has no
    /// wasm32-wasip2 build step — the same reason `hooks.rs` hand-authors its
    /// lifecycle doubles. `run`'s result exceeds one flat value, so the core
    /// function returns an i32 pointer to a `tool-result` it lays out by hand;
    /// the offsets below are the canonical ABI's (record align 4: `status` 0,
    /// `summary` 4, `data` 16, `data-path` 28, `truncated` 40, `metadata` 44).
    pub(crate) fn driver_double(
        engine: &wasmtime::Engine,
        status: u32,
        response: &str,
    ) -> Component {
        driver_double_with_metadata(engine, status, response, &[])
    }

    /// A `murmur:tool/run@0.1.0` component whose `run` executes `unreachable`: a driver that
    /// traps on every call.
    pub(crate) fn trapping_driver_double(engine: &wasmtime::Engine) -> Component {
        bare_driver_double(engine, "unreachable")
    }

    /// A `murmur:tool/run@0.1.0` component whose `run` answers `status: passed` with neither
    /// `data` nor `summary`. The result is the zeroed memory at offset 128, which the canonical
    /// ABI reads as `passed`, every option `none`, and an empty `metadata` list.
    pub(crate) fn no_data_driver_double(engine: &wasmtime::Engine) -> Component {
        bare_driver_double(engine, "(i32.const 128)")
    }

    /// A driver component whose core `run` is `run_body`, which must leave an `i32` result
    /// pointer on the stack or trap.
    fn bare_driver_double(engine: &wasmtime::Engine, run_body: &str) -> Component {
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (i32.const 1024))
    (func (export "run") (param i32 i32 i32 i32 i32 i32) (result i32)
      {run_body})
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $status (enum "passed" "failed" "error"))
  (type $tool-input (record
    (field "data" (option string))
    (field "log-path" (option string))))
  (type $tool-result (record
    (field "status" $status)
    (field "summary" (option string))
    (field "data" (option string))
    (field "data-path" (option string))
    (field "truncated" bool)
    (field "metadata" (list (tuple string string)))))
  (type $ft (func (param "input" $tool-input) (result $tool-result)))
  (func $run (type $ft)
    (canon lift (core func $i "run") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (instance $ti
    (export "status" (type $status))
    (export "tool-input" (type $tool-input))
    (export "tool-result" (type $tool-result))
    (export "run" (func $run)))
  (export "murmur:tool/run@0.1.0" (instance $ti))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("bare driver double WAT parses");
        Component::new(engine, &bytes).expect("bare driver double compiles")
    }

    /// A driver double answering `before` until a request carries `marker` anywhere in its
    /// input, and `after` on every request that does — so one capsule's turns can answer
    /// differently once, say, a reopen's feedback is in the conversation.
    ///
    /// The marker sits at 2048 and the two responses from 4096, each laid out as
    /// [`driver_double`] lays out its one; the bump allocator starts past both.
    pub(crate) fn driver_double_switching(
        engine: &wasmtime::Engine,
        marker: &str,
        before: &str,
        after: &str,
    ) -> Component {
        let escape =
            |text: &str| -> String { text.bytes().map(|b| format!("\\{b:02x}")).collect() };
        let marker_len = marker.len();
        assert!(
            2048 + marker_len <= 4096,
            "the marker must fit below the responses"
        );
        let (before_at, before_len) = (4096, before.len());
        let after_at = before_at + before_len + 8;
        let after_len = after.len();
        let bump_start = after_at + after_len + 8;
        let (marker_data, before_data, after_data) =
            (escape(marker), escape(before), escape(after));
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 4)
    (global $bump (mut i32) (i32.const {bump_start}))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $p i32)
      (local.set $p (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (global.set $bump (i32.add (local.get $p) (i32.add (local.get 3) (i32.const 8))))
      (local.get $p))
    (func $contains (param $p i32) (param $n i32) (result i32)
      (local $i i32) (local $j i32)
      (loop $outer
        (if (i32.gt_u (i32.add (local.get $i) (i32.const {marker_len})) (local.get $n))
          (then (return (i32.const 0))))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (if (i32.eq (local.get $j) (i32.const {marker_len}))
              (then (return (i32.const 1))))
            (br_if $mismatch
              (i32.ne
                (i32.load8_u (i32.add (local.get $p) (i32.add (local.get $i) (local.get $j))))
                (i32.load8_u (i32.add (i32.const 2048) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $inner)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer))
      (i32.const 0))
    (func (export "run") (param i32 i32 i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 128) (i32.const 0))
      (i32.store  (i32.const 132) (i32.const 0))
      (i32.store  (i32.const 144) (i32.const 1))
      (if (i32.and (local.get 0) (call $contains (local.get 1) (local.get 2)))
        (then
          (i32.store (i32.const 148) (i32.const {after_at}))
          (i32.store (i32.const 152) (i32.const {after_len})))
        (else
          (i32.store (i32.const 148) (i32.const {before_at}))
          (i32.store (i32.const 152) (i32.const {before_len}))))
      (i32.store  (i32.const 156) (i32.const 0))
      (i32.store8 (i32.const 168) (i32.const 0))
      (i32.store  (i32.const 172) (i32.const 0))
      (i32.store  (i32.const 176) (i32.const 0))
      (i32.const 128))
    (data (i32.const 2048) "{marker_data}")
    (data (i32.const {before_at}) "{before_data}")
    (data (i32.const {after_at}) "{after_data}")
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $status (enum "passed" "failed" "error"))
  (type $tool-input (record
    (field "data" (option string))
    (field "log-path" (option string))))
  (type $tool-result (record
    (field "status" $status)
    (field "summary" (option string))
    (field "data" (option string))
    (field "data-path" (option string))
    (field "truncated" bool)
    (field "metadata" (list (tuple string string)))))
  (type $ft (func (param "input" $tool-input) (result $tool-result)))
  (func $run (type $ft)
    (canon lift (core func $i "run") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (instance $ti
    (export "status" (type $status))
    (export "tool-input" (type $tool-input))
    (export "tool-result" (type $tool-result))
    (export "run" (func $run)))
  (export "murmur:tool/run@0.1.0" (instance $ti))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("switching driver double WAT parses");
        Component::new(engine, &bytes).expect("switching driver double compiles")
    }

    /// [`driver_double`], answering with `metadata` on every call — a `continuation_id` entry
    /// makes it a stateful driver holding a continuation.
    ///
    /// Each `(string, string)` tuple is 16 bytes (key pointer, key length, value pointer, value
    /// length) laid out from 192; the strings follow from 256, below the response at 4096. The bump
    /// allocator starts past the response, so lowering the request overwrites none of them.
    pub(crate) fn driver_double_with_metadata(
        engine: &wasmtime::Engine,
        status: u32,
        response: &str,
        metadata: &[(&str, &str)],
    ) -> Component {
        let len = response.len();
        let escape =
            |text: &str| -> String { text.bytes().map(|b| format!("\\{b:02x}")).collect() };
        let escaped = escape(response);
        let mut metadata_stores = String::new();
        let mut metadata_data = String::new();
        let mut next_string = 256usize;
        for (index, (key, value)) in metadata.iter().enumerate() {
            let tuple = 192 + 16 * index;
            for (offset, text) in [(0, key), (8, value)] {
                metadata_stores.push_str(&format!(
                    "      (i32.store (i32.const {}) (i32.const {next_string}))\n      \
                     (i32.store (i32.const {}) (i32.const {}))\n",
                    tuple + offset,
                    tuple + offset + 4,
                    text.len()
                ));
                metadata_data.push_str(&format!(
                    "    (data (i32.const {next_string}) \"{}\")\n",
                    escape(text)
                ));
                next_string += text.len();
            }
        }
        assert!(
            next_string <= 4096,
            "metadata strings must fit below the response"
        );
        let bump_start = 4096 + len + 8;
        let metadata_ptr = if metadata.is_empty() { 0 } else { 192 };
        let metadata_len = metadata.len();
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 4)
    (global $bump (mut i32) (i32.const {bump_start}))
    (func (export "realloc") (param i32 i32 i32 i32) (result i32)
      (local $p i32)
      (local.set $p (i32.and (i32.add (global.get $bump) (i32.const 7)) (i32.const -8)))
      (global.set $bump (i32.add (local.get $p) (i32.add (local.get 3) (i32.const 8))))
      (local.get $p))
    (func (export "run") (param i32 i32 i32 i32 i32 i32) (result i32)
      (i32.store8 (i32.const 128) (i32.const {status}))
      (i32.store  (i32.const 132) (i32.const 0))
      (i32.store  (i32.const 144) (i32.const 1))
      (i32.store  (i32.const 148) (i32.const 4096))
      (i32.store  (i32.const 152) (i32.const {len}))
      (i32.store  (i32.const 156) (i32.const 0))
      (i32.store8 (i32.const 168) (i32.const 0))
      (i32.store  (i32.const 172) (i32.const {metadata_ptr}))
      (i32.store  (i32.const 176) (i32.const {metadata_len}))
{metadata_stores}      (i32.const 128))
    (data (i32.const 4096) "{escaped}")
{metadata_data}
  )
  (core instance $i (instantiate $m))
  (alias core export $i "memory" (core memory $mem))
  (alias core export $i "realloc" (core func $realloc))

  (type $status (enum "passed" "failed" "error"))
  (type $tool-input (record
    (field "data" (option string))
    (field "log-path" (option string))))
  (type $tool-result (record
    (field "status" $status)
    (field "summary" (option string))
    (field "data" (option string))
    (field "data-path" (option string))
    (field "truncated" bool)
    (field "metadata" (list (tuple string string)))))
  (type $ft (func (param "input" $tool-input) (result $tool-result)))
  (func $run (type $ft)
    (canon lift (core func $i "run") (memory $mem) (realloc $realloc) string-encoding=utf8))
  (instance $ti
    (export "status" (type $status))
    (export "tool-input" (type $tool-input))
    (export "tool-result" (type $tool-result))
    (export "run" (func $run)))
  (export "murmur:tool/run@0.1.0" (instance $ti))
)"#
        );
        let bytes = wat::parse_str(&wat).expect("driver double WAT parses");
        Component::new(engine, &bytes).expect("driver double compiles")
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::driver_double, *};
    use crate::bindings::hook::murmur::hook::lifecycle::{ContextInsertion, Message};
    use tempfile::TempDir;

    const CANNED: &str =
        r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"compacted summary"}]}"#;

    fn engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        Engine::new(&config).expect("engine builds")
    }

    /// The one model the primary driver's `models:` table lists in these tests, and its ceiling.
    const DECLARED_MODEL: &str = "declared-model";
    const DECLARED_CAP: u32 = 1_048_576;
    /// The primary choice's resolved cap, what a model the table does not list gets.
    const PRIMARY_CAP: u32 = 4096;
    const LIFECYCLE: HookInferencePurpose = HookInferencePurpose::Lifecycle;
    const COMPACTION: HookInferencePurpose = HookInferencePurpose::Compaction;

    /// [`ctx_with_spend`]'s primary driver table and cap, with `compaction` as
    /// `inference.compaction.max_tokens`.
    fn output_caps(compaction: Option<u32>) -> crate::model_limits::HookOutputCaps {
        crate::model_limits::HookOutputCaps::new(
            std::collections::BTreeMap::from([(
                DECLARED_MODEL.to_string(),
                murmur_artifact::ModelLimits {
                    max_output_tokens: DECLARED_CAP,
                    context_window: 2_000_000,
                },
            )]),
            PRIMARY_CAP,
            compaction,
        )
    }

    fn ctx(engine: &Engine, workdir: &std::path::Path, driver: Component) -> HookInferenceCtx {
        ctx_with_spend(engine, workdir, driver, Arc::new(SpendMeter::unlimited()))
    }

    fn ctx_with_spend(
        engine: &Engine,
        workdir: &std::path::Path,
        driver: Component,
        spend: Arc<SpendMeter>,
    ) -> HookInferenceCtx {
        HookInferenceCtx {
            driver_name: "mock-driver".to_string(),
            driver_component: driver,
            model: "manifest-model".to_string(),
            output_caps: output_caps(None),
            engine: engine.clone(),
            accessible_workdir: workdir.to_path_buf(),
            records_root: crate::workdir_fs::WorkdirRoot::open(workdir).unwrap(),
            inference_env: Vec::new(),
            compaction_inference_env: Vec::new(),
            capability_policy: CapabilityPolicy::default(),
            network_allow_rules: Vec::new(),
            driver_grant: None,
            gateway: None,
            gateways: Default::default(),
            formation: None,
            spend,
            records: std::sync::Mutex::new(Vec::new()),
            spend_refusals: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn request(model: Option<&str>) -> InferenceRequest {
        InferenceRequest {
            messages: vec![Message {
                role: "user".to_string(),
                content: "summarize this".to_string(),
                id: None,
                source_id: None,
                inserted_by: None,
            }],
            system_prompt: None,
            model: model.map(str::to_string),
        }
    }

    /// Token count of the payload `run` must have built for `model` — recomputed
    /// here from the *same* `build_driver_payload` the agent loop uses. Asserting
    /// `input_tokens` against it proves the resolved model really went onto the
    /// wire, not just into `model-used`.
    fn expected_input_tokens(model: &str) -> u64 {
        expected_input_tokens_at(model, PRIMARY_CAP)
    }

    /// [`expected_input_tokens`] for a payload carrying `max_tokens: cap`.
    fn expected_input_tokens_at(model: &str, cap: u32) -> u64 {
        let messages = vec![
            serde_json::json!({"role":"user","content":[{"type":"text","text":"summarize this"}]}),
        ];
        let payload = build_driver_payload(model, cap, &messages, &[], "", None, None);
        u64::from(count_tokens(&serde_json::to_string(&payload).unwrap()))
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(f)
    }

    /// `model: none` resolves to the manifest's primary `inference.model`, the
    /// call goes through the shared driver-invocation path, and the response
    /// carries the driver's text plus host-computed token counts.
    #[test]
    fn run_inference_model_none_resolves_to_manifest_model() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));

        let resp = block_on(ctx.run("hook:test", LIFECYCLE, request(None)))
            .expect("the driver double returns a well-formed completion");

        assert_eq!(resp.text, "compacted summary");
        assert_eq!(resp.model_used, "manifest-model");
        assert_eq!(resp.input_tokens, expected_input_tokens("manifest-model"));
        assert_eq!(resp.output_tokens, u64::from(count_tokens(CANNED)));

        let records = ctx.drain_records();
        assert_eq!(records.len(), 1, "exactly one trace record per call");
        assert_eq!(records[0].origin.source, "hook:test");
        assert_eq!(records[0].origin.model, "manifest-model");
        assert_eq!(records[0].decision, "end_turn");
        assert!(ctx.drain_records().is_empty(), "drain is destructive");
    }

    /// `model: some(m)` sends `m` instead of the manifest model. `input_tokens`
    /// is recomputed from a payload built with the override (whose name is a
    /// different token length from the manifest model's), so this fails if the
    /// override only reached `model-used`.
    #[test]
    fn run_inference_model_some_overrides_the_sent_model() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));

        let resp = block_on(ctx.run(
            "hook:test",
            LIFECYCLE,
            request(Some("override-model-with-longer-name")),
        ))
        .expect("override model still completes against the double");

        assert_eq!(resp.model_used, "override-model-with-longer-name");
        assert_eq!(
            resp.input_tokens,
            expected_input_tokens("override-model-with-longer-name")
        );
        assert_ne!(
            expected_input_tokens("override-model-with-longer-name"),
            expected_input_tokens("manifest-model"),
            "the two model names must tokenize differently for this test to discriminate"
        );
        assert_eq!(
            ctx.drain_records()[0].origin.model,
            "override-model-with-longer-name"
        );
    }

    /// A driver-reported error surfaces as `Err` and is **not** retried with any
    /// other model — exactly one dispatch, one trace record, tagged `error`.
    #[test]
    fn run_inference_driver_error_is_err_with_no_retry() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let driver = driver_double(
            &engine,
            0,
            r#"{"stop_reason":"error","error":"unknown model: nope"}"#,
        );
        let ctx = ctx(&engine, dir.path(), driver);

        let err = block_on(ctx.run("hook:test", LIFECYCLE, request(Some("nope"))))
            .expect_err("a driver-reported error must not be swallowed");
        assert!(err.contains("unknown model: nope"), "got: {err}");

        let records = ctx.drain_records();
        assert_eq!(records.len(), 1, "one attempt, no implicit fallback retry");
        assert_eq!(records[0].decision, "error");
        assert_eq!(
            records[0].origin.model, "nope",
            "the failed record must name the model that was actually attempted"
        );
    }

    /// Regression: the driver deserializes each message's `content` into a
    /// `Vec<MurmurContentBlock>`, so the host must send it as a JSON array of
    /// `{"type":"text","text":…}` blocks, never a bare string. Fails if
    /// `wire_messages` ever emits `content` as a plain string again — the shape
    /// that made LLM compaction fail against every driver.
    #[test]
    fn run_inference_message_content_is_a_sequence_of_text_blocks() {
        let req = InferenceRequest {
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                    id: None,
                    source_id: None,
                    inserted_by: None,
                },
                Message {
                    role: "assistant".to_string(),
                    content: "hi there".to_string(),
                    id: None,
                    source_id: None,
                    inserted_by: None,
                },
            ],
            system_prompt: None,
            model: None,
        };

        let messages = wire_messages(&req);
        assert_eq!(messages.len(), 2);
        for (msg, (role, text)) in messages
            .iter()
            .zip([("user", "hello"), ("assistant", "hi there")])
        {
            assert_eq!(msg["role"], role);
            let content = msg["content"]
                .as_array()
                .expect("content must be a sequence of blocks, not a bare string");
            assert_eq!(content.len(), 1, "one text block per message");
            assert_eq!(content[0]["type"], "text");
            assert_eq!(content[0]["text"], text);
        }
    }

    /// Invariant: `message.id`, `message.source-id` and `message.inserted-by` never reach the
    /// provider.
    ///
    /// `wire_messages` names the two fields it forwards rather than serializing the
    /// record, so a field added to the WIT `message` is inert on the wire until someone
    /// deliberately forwards it. A uuid at the head of a cached prefix is volatile
    /// content and would defeat provider prompt-prefix caching on every request.
    #[test]
    fn message_id_and_source_id_are_stripped_from_a_hooks_inference_request() {
        let req = InferenceRequest {
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
                id: Some("msg_0198f1c2d3e44a5b8c9d0e1f2a3b4c5d".to_string()),
                source_id: Some("corpus:abc".to_string()),
                inserted_by: Some(ContextInsertion::ReplaceContext),
            }],
            system_prompt: None,
            model: None,
        };

        let messages = wire_messages(&req);
        assert_eq!(messages.len(), 1);
        let keys: Vec<&String> = messages[0]
            .as_object()
            .expect("a wire message is a JSON object")
            .keys()
            .collect();
        assert_eq!(keys, vec!["content", "role"]);

        let payload = build_driver_payload("m", 8192, &messages, &[], "sys", None, None);
        let serialized = serde_json::to_string(&payload).unwrap();
        assert!(!serialized.contains("msg_0198f1c2d3e44a5b8c9d0e1f2a3b4c5d"));
        assert!(!serialized.contains("corpus:abc"));
        assert!(!serialized.contains("source_id"));
        assert!(!serialized.contains("source-id"));
        assert!(!serialized.contains("inserted_by"));
        assert!(!serialized.contains("inserted-by"));
        assert!(!serialized.contains("replace-context"));
    }

    /// A completion on a model the primary driver's table lists sends that model's ceiling, and
    /// one on any other model sends the primary's resolved cap; spend is admitted against the
    /// same number, so a session ceiling between the two refuses only the first.
    #[test]
    fn run_inference_caps_at_the_table_ceiling_else_the_primary_cap() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));
        let declared =
            block_on(ctx.run("hook:compact", LIFECYCLE, request(Some(DECLARED_MODEL)))).unwrap();
        assert_eq!(
            declared.input_tokens,
            expected_input_tokens_at(DECLARED_MODEL, DECLARED_CAP)
        );
        assert_ne!(
            expected_input_tokens_at(DECLARED_MODEL, DECLARED_CAP),
            expected_input_tokens_at(DECLARED_MODEL, PRIMARY_CAP),
            "the two caps must tokenize differently for this test to discriminate"
        );
        let unlisted = block_on(ctx.run("hook:compact", LIFECYCLE, request(None))).unwrap();
        assert_eq!(
            unlisted.input_tokens,
            expected_input_tokens_at("manifest-model", PRIMARY_CAP)
        );

        let meter = Arc::new(SpendMeter::new(Some(100_000), None));
        let ctx = ctx_with_spend(
            &engine,
            dir.path(),
            driver_double(&engine, 0, CANNED),
            Arc::clone(&meter),
        );
        block_on(ctx.run("hook:compact", LIFECYCLE, request(None)))
            .expect("4096 fits under the ceiling");
        let err = block_on(ctx.run("hook:compact", LIFECYCLE, request(Some(DECLARED_MODEL))))
            .expect_err("the declared ceiling does not");
        assert!(err.starts_with("spend ceiling reached"), "{err}");
    }

    /// A call made for `on-compaction` sends `inference.compaction.max_tokens` and admits spend
    /// against it, on any model; every other call keeps the table ceiling else the primary cap.
    /// With the key unset, a compaction call caps as any other does.
    #[test]
    fn run_inference_for_compaction_caps_at_the_compaction_cap() {
        const COMPACTION_CAP: u32 = 2000;
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let mut ctx = ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));
        ctx.output_caps = output_caps(Some(COMPACTION_CAP));
        for (purpose, model, cap) in [
            (COMPACTION, DECLARED_MODEL, COMPACTION_CAP),
            (COMPACTION, "manifest-model", COMPACTION_CAP),
            (LIFECYCLE, DECLARED_MODEL, DECLARED_CAP),
            (LIFECYCLE, "manifest-model", PRIMARY_CAP),
        ] {
            let resp = block_on(ctx.run("hook:compact", purpose, request(Some(model)))).unwrap();
            assert_eq!(
                resp.input_tokens,
                expected_input_tokens_at(model, cap),
                "{purpose:?} on {model}"
            );
        }
        assert_ne!(
            expected_input_tokens_at(DECLARED_MODEL, COMPACTION_CAP),
            expected_input_tokens_at(DECLARED_MODEL, DECLARED_CAP),
            "the two caps must tokenize differently for this test to discriminate"
        );

        let unset = self::ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));
        let resp =
            block_on(unset.run("hook:compact", COMPACTION, request(Some(DECLARED_MODEL)))).unwrap();
        assert_eq!(
            resp.input_tokens,
            expected_input_tokens_at(DECLARED_MODEL, DECLARED_CAP)
        );

        // Spend is reserved at the compaction cap: a ceiling between it and the table ceiling
        // admits the compaction call and refuses the same call made for any other event.
        let meter = Arc::new(SpendMeter::new(Some(100_000), None));
        let mut ctx = ctx_with_spend(
            &engine,
            dir.path(),
            driver_double(&engine, 0, CANNED),
            Arc::clone(&meter),
        );
        ctx.output_caps = output_caps(Some(COMPACTION_CAP));
        block_on(ctx.run("hook:compact", COMPACTION, request(Some(DECLARED_MODEL))))
            .expect("the compaction cap fits under the ceiling");
        let err = block_on(ctx.run("hook:compact", LIFECYCLE, request(Some(DECLARED_MODEL))))
            .expect_err("the declared ceiling does not");
        assert!(err.starts_with("spend ceiling reached"), "{err}");
    }

    /// A compaction call runs the driver under the compaction environment, every other call
    /// under the session's.
    #[test]
    fn run_inference_for_compaction_dispatches_with_the_compaction_env() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let mut ctx = ctx(&engine, dir.path(), driver_double(&engine, 0, CANNED));
        let session = vec![(
            "MURMUR_INFERENCE_DRIVER_CONFIG".to_string(),
            r#"{"effort":"max"}"#.to_string(),
        )];
        let compaction = vec![(
            "MURMUR_INFERENCE_DRIVER_CONFIG".to_string(),
            r#"{"effort":"low"}"#.to_string(),
        )];
        ctx.inference_env = session.clone();
        ctx.compaction_inference_env = compaction.clone();
        assert_eq!(ctx.env_for(LIFECYCLE), session.as_slice());
        assert_eq!(ctx.env_for(COMPACTION), compaction.as_slice());
    }

    /// Each buffered record carries the cap its payload sent and the driver config its driver ran
    /// under: the compaction cap and environment for a compaction call, the lifecycle cap and the
    /// session's environment otherwise, and a failed call's own.
    #[test]
    fn run_inference_records_each_calls_cap_and_driver_config() {
        const COMPACTION_CAP: u32 = 2000;
        let config_env = |config: &str| {
            vec![(
                "MURMUR_INFERENCE_DRIVER_CONFIG".to_string(),
                config.to_string(),
            )]
        };
        let settings = |cap: u32, effort: &str| InferenceCallSettings {
            max_output_tokens: cap,
            driver_config: Some(serde_json::json!({ "reasoning_effort": effort })),
        };
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let with_envs = |mut ctx: HookInferenceCtx| {
            ctx.output_caps = output_caps(Some(COMPACTION_CAP));
            ctx.inference_env = config_env(r#"{"reasoning_effort":"high"}"#);
            ctx.compaction_inference_env = config_env(r#"{"reasoning_effort":"low"}"#);
            ctx
        };

        let ctx = with_envs(self::ctx(
            &engine,
            dir.path(),
            driver_double(&engine, 0, CANNED),
        ));
        block_on(ctx.run("hook:compactor", COMPACTION, request(None))).unwrap();
        block_on(ctx.run("hook:summary", LIFECYCLE, request(None))).unwrap();
        let recorded: Vec<(String, InferenceCallSettings)> = ctx
            .drain_records()
            .into_iter()
            .map(|record| (record.origin.source, record.settings))
            .collect();
        assert_eq!(
            recorded,
            [
                (
                    "hook:compactor".to_string(),
                    settings(COMPACTION_CAP, "low")
                ),
                ("hook:summary".to_string(), settings(PRIMARY_CAP, "high")),
            ]
        );

        let failing = with_envs(self::ctx(
            &engine,
            dir.path(),
            driver_double(
                &engine,
                2,
                r#"{"stop_reason":"error","error":"HTTP 500: boom"}"#,
            ),
        ));
        block_on(failing.run("hook:compactor", COMPACTION, request(None))).expect_err("an error");
        let records = failing.drain_records();
        assert_eq!(records.len(), 1);
        assert!(records[0].failure.is_some());
        assert_eq!(records[0].settings, settings(COMPACTION_CAP, "low"));
    }

    /// A spend ceiling refuses a hook's completion before dispatch — both a ceiling too small for
    /// the call and a session already latched by an earlier refusal. The guest gets the refusal
    /// text, no `inference` record is buffered, and one refusal record names the hook.
    #[test]
    fn run_inference_refused_by_spend_ceiling() {
        let dir = TempDir::new().unwrap();
        let engine = engine();

        let latched = Arc::new(SpendMeter::new(Some(1_000_000), None));
        assert!(latched.admit(2_000_000, 0).is_err(), "latches the session");

        for (case, meter) in [
            ("too small", Arc::new(SpendMeter::new(Some(10), None))),
            ("latched", latched),
        ] {
            // A driver double that, had it been invoked, would have buffered an `error` record:
            // `run` records every dispatched call, success or failure.
            let ctx = ctx_with_spend(
                &engine,
                dir.path(),
                driver_double(&engine, 2, "the driver was invoked"),
                Arc::clone(&meter),
            );
            let err = block_on(ctx.run("hook:compact", LIFECYCLE, request(None)))
                .expect_err("a refused call is an err");
            assert!(
                err.starts_with("spend ceiling reached: inference.max_session_tokens is"),
                "{case}: {err}"
            );
            assert!(
                err.contains("retrying will not get past it"),
                "{case}: {err}"
            );
            assert!(!err.contains("the driver was invoked"), "{case}: {err}");
            assert!(
                ctx.drain_records().is_empty(),
                "{case}: no inference record"
            );
            let refusals = ctx.drain_spend_refusals();
            assert_eq!(refusals.len(), 1, "{case}");
            assert_eq!(refusals[0].origin, "hook:compact", "{case}");
            assert_eq!(
                refusals[0].refusal.limit,
                crate::spend::SpendLimit::Session,
                "{case}"
            );
            assert_eq!(meter.used(), 0, "{case}: nothing was charged");
            assert!(!meter.has_open_admission(), "{case}");
        }
    }

    /// A driver `status: error` reporting a provider error buffers a failed record naming its
    /// code and error, with no output count, while the guest's `err` is the text it always was.
    #[test]
    fn run_inference_driver_error_status_records_its_failure() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let response = r#"{"stop_reason":"error","error":"HTTP 500: boom"}"#;
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 2, response));

        let err =
            block_on(ctx.run("hook:compactor", LIFECYCLE, request(None))).expect_err("an error");
        assert_eq!(
            err,
            format!("inference driver returned an error: {response}")
        );

        let records = ctx.drain_records();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.origin.source, "hook:compactor");
        assert_eq!(record.origin.model, "manifest-model");
        assert_eq!(record.decision, "error");
        assert_eq!(record.input_tokens, expected_input_tokens("manifest-model"));
        assert_eq!(record.output_tokens, None);
        let failure = record
            .failure
            .as_ref()
            .expect("a failed call names its failure");
        assert_eq!(failure.code, "driver_error");
        assert_eq!(failure.message, err);
        assert!(
            failure.error.contains("HTTP 500: boom"),
            "{}",
            failure.error
        );
        assert_eq!(
            failure.provider_status, None,
            "the context holds no gateway"
        );
    }

    /// A completion that reports usage is charged the provider's request and completion, cache
    /// members included and thinking not added; one that reports nothing is charged the
    /// runtime's own counts of its payload and raw response.
    #[test]
    fn run_inference_charges_the_reported_counts() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let charged = |response: &str| {
            let meter = Arc::new(SpendMeter::new(Some(1_000_000), None));
            let ctx = ctx_with_spend(
                &engine,
                dir.path(),
                driver_double(&engine, 0, response),
                Arc::clone(&meter),
            );
            let resp = block_on(ctx.run("hook:compact", LIFECYCLE, request(None))).unwrap();
            (meter.used(), resp)
        };

        let reporting = r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"s"}],"usage":{"input_tokens":70,"output_tokens":12,"cached_tokens":30,"cache_write_tokens":3,"thinking_tokens":5}}"#;
        let (used, resp) = charged(reporting);
        assert_eq!(used, 70 + 30 + 3 + 12);
        assert_eq!(
            resp.input_tokens,
            expected_input_tokens("manifest-model"),
            "the WIT counts stay the runtime's own"
        );

        let (used, _) = charged(CANNED);
        assert_eq!(
            used,
            expected_input_tokens("manifest-model") + u64::from(count_tokens(CANNED))
        );

        // Input alone is no report.
        let input_only = r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"s"}],"usage":{"input_tokens":70}}"#;
        let (used, _) = charged(input_only);
        assert_eq!(
            used,
            expected_input_tokens("manifest-model") + u64::from(count_tokens(input_only))
        );
    }

    /// A failed completion that reported usage is charged it; one that reported nothing is
    /// charged its input estimate alone.
    #[test]
    fn run_inference_charges_a_failed_call_its_reported_counts() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let charged = |response: &str| {
            let meter = Arc::new(SpendMeter::new(Some(1_000_000), None));
            let ctx = ctx_with_spend(
                &engine,
                dir.path(),
                driver_double(&engine, 2, response),
                Arc::clone(&meter),
            );
            block_on(ctx.run("hook:compact", LIFECYCLE, request(None))).expect_err("a failed call");
            meter.used()
        };

        assert_eq!(
            charged(
                r#"{"stop_reason":"error","error":"HTTP 500","usage":{"input_tokens":40,"output_tokens":2,"cached_tokens":8}}"#
            ),
            40 + 8 + 2
        );
        assert_eq!(
            charged(r#"{"stop_reason":"error","error":"HTTP 500"}"#),
            expected_input_tokens("manifest-model")
        );
    }

    /// A non-`passed` tool status from the driver is also an `Err`.
    #[test]
    fn run_inference_failed_driver_status_is_err() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 2, "boom"));

        let err = block_on(ctx.run("hook:test", LIFECYCLE, request(None)))
            .expect_err("status: error must surface");
        assert!(err.contains("boom"), "got: {err}");
        assert_eq!(ctx.drain_records()[0].decision, "error");
    }
}
