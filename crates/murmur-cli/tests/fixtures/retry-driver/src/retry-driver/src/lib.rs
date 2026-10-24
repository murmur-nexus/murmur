// Test-only inference driver that lets an upstream script every failure the runtime's retry loop
// classifies. It POSTs the request payload, byte for byte, to
// `$MURMUR_INFERENCE_ENDPOINT/v1/messages` with `content-type: application/json`, and answers:
//
//   * a 2xx body holding `fixture_break: {error, transient?, usage?}` — `status: error`, data
//     `{"error": <error>, "stop_reason": "error"}` plus `"usage": <usage>` when given, and
//     metadata `[("transient", <transient>)]` when given: a stream that broke after the `200`;
//   * any other 2xx body — `status: passed`, with the body as data, so the upstream speaks the
//     host's own driver response shape;
//   * a non-2xx status — `status: error`, data `{"error":"HTTP <status>: <body>"}`, plus metadata
//     `transient` from the body's `fixture_transient` string when present;
//   * a response body that breaks off after the head — `status: error`, data
//     `{"error":"response broke: <error>"}`, metadata `[("transient", "true")]`;
//   * an outgoing-handler error — `status: error`, data `{"error":"request failed: <error>"}`.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use serde_json::{json, Value};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const INFERENCE_ENDPOINT: &str = "MURMUR_INFERENCE_ENDPOINT";
const TRANSIENT: &str = "transient";

struct RetryDriver;

fn split_endpoint(endpoint: &str) -> Result<(String, String), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported endpoint '{endpoint}'"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    Ok((
        authority.to_string(),
        format!("{}/v1/messages", path.trim_end_matches('/')),
    ))
}

/// Why [`post`] got no whole response.
enum PostError {
    /// The request was not made, or no response head came back.
    Request(String),
    /// The response head came back and its body broke off.
    Body(String),
}

impl From<String> for PostError {
    fn from(error: String) -> Self {
        Self::Request(error)
    }
}

impl From<&str> for PostError {
    fn from(error: &str) -> Self {
        Self::Request(error.to_string())
    }
}

/// POSTs `body` and returns the response status and body.
fn post(endpoint: &str, body: &[u8]) -> Result<(u16, String), PostError> {
    let (authority, path) = split_endpoint(endpoint)?;
    let fields = Fields::new();
    for (name, value) in [
        ("content-type", b"application/json".to_vec()),
        ("content-length", body.len().to_string().into_bytes()),
    ] {
        fields
            .append(name, &value)
            .map_err(|e| format!("header: {e:?}"))?;
    }
    let request = OutgoingRequest::new(fields);
    request.set_method(&Method::Post).map_err(|()| "method")?;
    request
        .set_scheme(Some(&Scheme::Http))
        .map_err(|()| "scheme")?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| "path")?;

    let outgoing = request.body().map_err(|()| "body")?;
    {
        let stream = outgoing.write().map_err(|()| "body stream")?;
        for chunk in body.chunks(4096) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|e| format!("write: {e:?}"))?;
        }
    }
    OutgoingBody::finish(outgoing, None).map_err(|e| format!("finish: {e:?}"))?;

    let future = wasip2::http::outgoing_handler::handle(request, None)
        .map_err(|e| format!("handle: {e:?}"))?;
    let response = loop {
        match future.get() {
            Some(Ok(Ok(response))) => break response,
            Some(Ok(Err(e))) => return Err(format!("{e:?}").into()),
            Some(Err(())) => return Err("response future consumed".into()),
            None => future.subscribe().block(),
        }
    };
    let status = response.status();
    let incoming = response.consume().map_err(|()| "consume")?;
    let stream = incoming.stream().map_err(|()| "incoming stream")?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => return Err(PostError::Body(format!("{e:?}"))),
        }
    }
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn result(status: Status, data: String, metadata: Vec<(String, String)>) -> ToolResult {
    ToolResult {
        status,
        summary: None,
        data: Some(data),
        data_path: None,
        truncated: false,
        metadata,
    }
}

/// `[("transient", value)]` when `value` is a string, empty otherwise.
fn transient(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_str)
        .map(|value| vec![(TRANSIENT.to_string(), value.to_string())])
        .unwrap_or_default()
}

impl Guest for RetryDriver {
    fn run(input: ToolInput) -> ToolResult {
        let payload = input.data.unwrap_or_default();
        let endpoint = std::env::var(INFERENCE_ENDPOINT).unwrap_or_default();
        let (status, body) = match post(&endpoint, payload.as_bytes()) {
            Ok(answer) => answer,
            Err(PostError::Request(error)) => {
                return result(
                    Status::Error,
                    json!({"error": format!("request failed: {error}")}).to_string(),
                    Vec::new(),
                )
            }
            Err(PostError::Body(error)) => {
                return result(
                    Status::Error,
                    json!({"error": format!("response broke: {error}")}).to_string(),
                    vec![(TRANSIENT.to_string(), "true".to_string())],
                )
            }
        };
        let parsed = serde_json::from_str::<Value>(&body).ok();
        if !(200..300).contains(&status) {
            return result(
                Status::Error,
                json!({"error": format!("HTTP {status}: {body}")}).to_string(),
                transient(
                    parsed
                        .as_ref()
                        .and_then(|body| body.get("fixture_transient")),
                ),
            );
        }
        match parsed.as_ref().and_then(|body| body.get("fixture_break")) {
            Some(broken) => {
                let mut data = json!({
                    "error": broken.get("error").cloned().unwrap_or(Value::Null),
                    "stop_reason": "error",
                });
                if let Some(usage) = broken.get("usage") {
                    data["usage"] = usage.clone();
                }
                result(
                    Status::Error,
                    data.to_string(),
                    transient(broken.get(TRANSIENT)),
                )
            }
            None => result(Status::Passed, body, Vec::new()),
        }
    }
}

export!(RetryDriver);
