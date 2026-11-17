// Test-only components for the code mode bus tests. One crate builds two components:
//
//   bus-repl   (default features) the `code-mode` world: a program that calls the capsule's
//              operations through murmur:bus
//   bus-echo   (feature `plain`) the `tool` world with no bus import: an operation for bus-repl
//              to call
//
// bus-repl's input is a JSON object; `op` picks what it does, and `data` of its result is JSON:
//
//   list             `list()` → `[{"name", "description", "input_schema"}]`, `input_schema`
//                    being the parsed JSON of the operation's schema text
//   call             `call(name, <input as JSON text>)` → one call answer
//   call_text        `call(name, text)` with `text` sent verbatim → one call answer
//   batch            `call-batch(calls)` → `{"elapsed_ms", "results": [<call answer>]}`, the
//                    elapsed time measured around the `call-batch` call
//   sleep_then_call  sleep `sleep_ms` ms, then `call`
//   call_then_sleep  `call`, then sleep `sleep_ms` ms, then answer the call
//
// A call answer is `{"ok": {"status", "summary", "data", "data_path", "truncated"}}`,
// `{"refused": text}` or `{"failed": text}`; a refused `list()` is `{"refused": text}`. Every
// answer has status `passed`; a missing or unknown `op` has status `error`.
//
// bus-echo's input is `{"tag": s, "sleep_ms": n}`: it sleeps `sleep_ms` ms (default 0) and answers
// status `passed` with data `tag=<s>`.

#[cfg(not(feature = "plain"))]
mod repl {
    wit_bindgen::generate!({
        path: "../../../../../../capsule-runtime/wit/guest",
        world: "code-mode",
        generate_all,
    });

    use std::time::{Duration, Instant};

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use murmur::bus::bus::{self, CallError, CallRequest};
    use murmur::tool::run as imported;
    use serde_json::{json, Value};

    fn status_name(status: imported::Status) -> &'static str {
        match status {
            imported::Status::Passed => "passed",
            imported::Status::Failed => "failed",
            imported::Status::Error => "error",
        }
    }

    fn call_answer(result: Result<imported::ToolResult, CallError>) -> Value {
        match result {
            Ok(result) => json!({
                "ok": {
                    "status": status_name(result.status),
                    "summary": result.summary,
                    "data": result.data,
                    "data_path": result.data_path,
                    "truncated": result.truncated,
                }
            }),
            Err(CallError::Refused(text)) => json!({ "refused": text }),
            Err(CallError::Failed(text)) => json!({ "failed": text }),
        }
    }

    fn input_text(input: &Value) -> String {
        serde_json::to_string(input.get("input").unwrap_or(&Value::Null))
            .unwrap_or_else(|_| "null".to_owned())
    }

    fn name(input: &Value) -> String {
        input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn sleep(input: &Value) {
        let ms = input.get("sleep_ms").and_then(Value::as_u64).unwrap_or(0);
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn call(input: &Value) -> Value {
        call_answer(bus::call(&name(input), &input_text(input)))
    }

    fn list() -> Value {
        match bus::list() {
            Ok(operations) => Value::Array(
                operations
                    .into_iter()
                    .map(|operation| {
                        let schema = serde_json::from_str(&operation.input_schema)
                            .unwrap_or(Value::String(operation.input_schema));
                        json!({
                            "name": operation.name,
                            "description": operation.description,
                            "input_schema": schema,
                        })
                    })
                    .collect(),
            ),
            Err(text) => json!({ "refused": text }),
        }
    }

    fn batch(input: &Value) -> Value {
        let calls: Vec<CallRequest> = input
            .get("calls")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .map(|call| CallRequest {
                name: name(call),
                input: input_text(call),
            })
            .collect();
        let started = Instant::now();
        let results = bus::call_batch(&calls);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        json!({
            "elapsed_ms": elapsed_ms,
            "results": results.into_iter().map(call_answer).collect::<Vec<_>>(),
        })
    }

    fn result(status: Status, summary: Option<String>, data: Option<String>) -> ToolResult {
        ToolResult {
            status,
            summary,
            data,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
            attachments: Vec::new(),
        }
    }

    struct BusRepl;

    impl Guest for BusRepl {
        fn run(input: ToolInput) -> ToolResult {
            let input: Value =
                serde_json::from_str(input.data.as_deref().unwrap_or("{}")).unwrap_or(Value::Null);
            let answer = match input.get("op").and_then(Value::as_str) {
                Some("list") => list(),
                Some("call") => call(&input),
                Some("call_text") => {
                    let text = input
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    call_answer(bus::call(&name(&input), text))
                }
                Some("batch") => batch(&input),
                Some("sleep_then_call") => {
                    sleep(&input);
                    call(&input)
                }
                Some("call_then_sleep") => {
                    let answer = call(&input);
                    sleep(&input);
                    answer
                }
                other => {
                    return result(
                        Status::Error,
                        Some(format!("bus-repl has no op {other:?}")),
                        None,
                    )
                }
            };
            result(Status::Passed, None, Some(answer.to_string()))
        }
    }

    export!(BusRepl);
}

#[cfg(feature = "plain")]
mod echo {
    wit_bindgen::generate!({
        path: "../../../../../../capsule-runtime/wit/guest",
        world: "tool",
        generate_all,
    });

    use std::time::Duration;

    use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
    use serde_json::Value;

    struct BusEcho;

    impl Guest for BusEcho {
        fn run(input: ToolInput) -> ToolResult {
            let input: Value =
                serde_json::from_str(input.data.as_deref().unwrap_or("{}")).unwrap_or(Value::Null);
            let ms = input.get("sleep_ms").and_then(Value::as_u64).unwrap_or(0);
            std::thread::sleep(Duration::from_millis(ms));
            let tag = input.get("tag").and_then(Value::as_str).unwrap_or_default();
            ToolResult {
                status: Status::Passed,
                summary: None,
                data: Some(format!("tag={tag}")),
                data_path: None,
                truncated: false,
                metadata: Vec::new(),
                attachments: Vec::new(),
            }
        }
    }

    export!(BusEcho);
}
