//! Host implementation of `murmur:runtime/inference@0.2.0#run-inference`.
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
    agent::{build_driver_payload, count_tokens},
    bindings::{
        hook::murmur::runtime::inference::{InferenceRequest, InferenceResponse},
        host::murmur::tool::run::{Status, ToolInput},
    },
    network_policy::NetworkAllowRule,
    runtime::{invoke_tool_component, ToolA2aWiring, ToolInvokeEnv},
    trace::InferenceOrigin,
    types::CapabilityPolicy,
};

/// The versioned instance name the host provides `run-inference` under. Hook
/// components that do not import it simply ignore the registration.
pub(crate) const INFERENCE_IFACE_VERSIONED: &str = "murmur:runtime/inference@0.2.0";

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
    pub(crate) output_tokens: u64,
    /// `"end_turn"` when the driver returned a usable completion, `"error"`
    /// otherwise.
    pub(crate) decision: String,
    pub(crate) duration_ms: u64,
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
    pub(crate) engine: Engine,
    pub(crate) accessible_workdir: PathBuf,
    pub(crate) inference_env: Vec<(String, String)>,
    pub(crate) capability_policy: CapabilityPolicy,
    pub(crate) network_allow_rules: Vec<NetworkAllowRule>,
    /// Buffered trace records, drained by the agent loop after hook dispatch.
    pub(crate) records: std::sync::Mutex<Vec<HookInferenceRecord>>,
}

impl HookInferenceCtx {
    /// Run one completion. `Err` is the string handed straight back to the guest
    /// as the `result`'s error case.
    async fn run(
        &self,
        origin: &str,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, String> {
        // `none` resolves to the manifest's primary model; `some(m)` is sent
        // verbatim and, if the driver rejects it, surfaces as `Err` with no
        // retry — a caller wanting fallback calls again with `none` itself.
        let model = request.model.unwrap_or_else(|| self.model.clone());

        let messages: Vec<Value> = request
            .messages
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();
        let payload = build_driver_payload(
            &model,
            &messages,
            &[],
            request.system_prompt.as_deref().unwrap_or(""),
            None,
        );
        let payload_json = serde_json::to_string(&payload)
            .map_err(|e| format!("failed to encode driver payload: {e}"))?;
        let input_tokens = u64::from(count_tokens(&payload_json));

        let started = Instant::now();
        let outcome = self.dispatch(payload_json).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let (output_tokens, result) = match outcome {
            Ok((raw, text)) => {
                // Same host-side tiktoken convention the agent loop uses: the
                // driver wire format carries no usage block to read instead.
                let output_tokens = u64::from(count_tokens(&raw));
                (
                    output_tokens,
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
            Err(err) => (0, Err(err)),
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
        });

        result
    }

    /// Dispatch the payload through the shared tool-invocation path and pull the
    /// completion text out of the driver's response. Returns `(raw, text)`.
    async fn dispatch(&self, payload_json: String) -> Result<(String, String), String> {
        let result = invoke_tool_component(
            ToolInvokeEnv {
                engine: &self.engine,
                accessible_workdir: &self.accessible_workdir,
                inference_env: &self.inference_env,
                capability_policy: &self.capability_policy,
                network_allow_rules: &self.network_allow_rules,
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
        .map_err(|e| format!("inference driver '{}' failed: {e}", self.driver_name))?;

        let raw = result
            .data
            .or(result.summary)
            .ok_or_else(|| "inference driver returned no data".to_string())?;
        if !matches!(result.status, Status::Passed) {
            return Err(format!("inference driver returned an error: {raw}"));
        }

        let response: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("failed to parse inference driver response: {e}"))?;
        if response.get("stop_reason").and_then(Value::as_str) == Some("error") {
            let err = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("driver returned error");
            return Err(format!("inference driver returned an error: {err}"));
        }

        let text = response_text(&response);
        Ok((raw, text))
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

/// Register `murmur:runtime/inference@0.2.0#run-inference` on a hook linker.
///
/// `origin` is the `hook:<name>` tag attached to every trace record this hook's
/// calls produce. `ctx` is `None` when the capsule has no usable inference
/// driver: the import is still *defined* (so a hook that imports it links and
/// runs), it just always returns the same `err`.
pub(crate) fn add_inference_to_linker<T: 'static>(
    linker: &mut Linker<T>,
    origin: String,
    ctx: Option<Arc<HookInferenceCtx>>,
) -> Result<(), String> {
    let origin = Arc::new(origin);
    linker
        .instance(INFERENCE_IFACE_VERSIONED)
        .map_err(|e| format!("failed to define {INFERENCE_IFACE_VERSIONED}: {e}"))?
        .func_wrap_async(
            "run-inference",
            move |_store: wasmtime::StoreContextMut<'_, T>, (request,): (InferenceRequest,)| {
                let ctx = ctx.clone();
                let origin = Arc::clone(&origin);
                let fut: Box<
                    dyn std::future::Future<
                            Output = wasmtime::Result<(Result<InferenceResponse, String>,)>,
                        > + Send,
                > = Box::new(async move {
                    let result = match ctx {
                        Some(ctx) => ctx.run(&origin, request).await,
                        // Wording tracks RuntimeError::DriverNotConfigured.
                        None => Err("inference driver is not configured; add \
                                     inference.driver.artifact to murmur.yaml"
                            .to_string()),
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
        let len = response.len();
        let escaped: String = response.bytes().map(|b| format!("\\{b:02x}")).collect();
        let wat = format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    (global $bump (mut i32) (i32.const 1024))
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
      (i32.store  (i32.const 172) (i32.const 0))
      (i32.store  (i32.const 176) (i32.const 0))
      (i32.const 128))
    (data (i32.const 4096) "{escaped}")
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
    use crate::bindings::hook::murmur::hook::lifecycle::Message;
    use tempfile::TempDir;

    const CANNED: &str =
        r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"compacted summary"}]}"#;

    fn engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.epoch_interruption(true);
        Engine::new(&config).expect("engine builds")
    }

    fn ctx(engine: &Engine, workdir: &std::path::Path, driver: Component) -> HookInferenceCtx {
        HookInferenceCtx {
            driver_name: "mock-driver".to_string(),
            driver_component: driver,
            model: "manifest-model".to_string(),
            engine: engine.clone(),
            accessible_workdir: workdir.to_path_buf(),
            inference_env: Vec::new(),
            capability_policy: CapabilityPolicy::default(),
            network_allow_rules: Vec::new(),
            records: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn request(model: Option<&str>) -> InferenceRequest {
        InferenceRequest {
            messages: vec![Message {
                role: "user".to_string(),
                content: "summarize this".to_string(),
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
        let messages = vec![serde_json::json!({"role":"user","content":"summarize this"})];
        let payload = build_driver_payload(model, &messages, &[], "", None);
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

        let resp = block_on(ctx.run("hook:test", request(None)))
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

        let err = block_on(ctx.run("hook:test", request(Some("nope"))))
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

    /// A non-`passed` tool status from the driver is also an `Err`.
    #[test]
    fn run_inference_failed_driver_status_is_err() {
        let dir = TempDir::new().unwrap();
        let engine = engine();
        let ctx = ctx(&engine, dir.path(), driver_double(&engine, 2, "boom"));

        let err =
            block_on(ctx.run("hook:test", request(None))).expect_err("status: error must surface");
        assert!(err.contains("boom"), "got: {err}");
        assert_eq!(ctx.drain_records()[0].decision, "error");
    }
}
