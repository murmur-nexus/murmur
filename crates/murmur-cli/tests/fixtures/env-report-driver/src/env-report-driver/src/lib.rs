// Test-only inference driver that reports what the runtime handed it and what came back over the
// wire. It never reveals a credential: `MURMUR_INFERENCE_API_KEY` is reported as present or absent
// only.
//
// It POSTs a fixed JSON body, with no auth header of its own, to `<MURMUR_INFERENCE_ENDPOINT>/v1/messages`
// through `wasi:http/outgoing-handler`, reads the response with `incoming-body.stream()` until the
// end, and ends the task on turn 0 with a final text of
//
//   key=<present|absent> endpoint=<url> authority=<host:port> status=<code> sha256=<hex>
//   bytes=<n> chunks=<n> gap_ms=<n>
//
// on one line, where `sha256` covers every body byte, `chunks` counts the non-empty reads, and
// `gap_ms` is the monotonic time between the first and the last of them. A test compares these
// against what its upstream served to tell a buffered body from a streamed one.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use sha2::{Digest, Sha256};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const REQUEST_BODY: &[u8] = br#"{"model":"test-model","max_tokens":16,"messages":[{"role":"user","content":"hello"}]}"#;

struct EnvReportDriver;

fn split_endpoint(endpoint: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = if let Some(rest) = endpoint.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else if let Some(rest) = endpoint.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else {
        return Err(format!("unsupported endpoint '{endpoint}'"));
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    Ok((
        scheme,
        authority.to_string(),
        format!("{}/v1/messages", path.trim_end_matches('/')),
    ))
}

fn report() -> Result<String, String> {
    let key = if std::env::var_os("MURMUR_INFERENCE_API_KEY").is_some() {
        "present"
    } else {
        "absent"
    };
    let endpoint = std::env::var("MURMUR_INFERENCE_ENDPOINT").unwrap_or_default();
    let (scheme, authority, path) = split_endpoint(&endpoint)?;

    let fields = Fields::new();
    fields
        .append("content-type", b"application/json")
        .map_err(|e| format!("header: {e:?}"))?;
    fields
        .append("content-length", REQUEST_BODY.len().to_string().as_bytes())
        .map_err(|e| format!("header: {e:?}"))?;
    let request = OutgoingRequest::new(fields);
    request.set_method(&Method::Post).map_err(|()| "method")?;
    request.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    request
        .set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|()| "path")?;

    let body = request.body().map_err(|()| "body")?;
    {
        let stream = body.write().map_err(|()| "body stream")?;
        stream
            .blocking_write_and_flush(REQUEST_BODY)
            .map_err(|e| format!("write: {e:?}"))?;
    }
    OutgoingBody::finish(body, None).map_err(|e| format!("finish: {e:?}"))?;

    let future = wasip2::http::outgoing_handler::handle(request, None)
        .map_err(|e| format!("handle: {e:?}"))?;
    let response = loop {
        match future.get() {
            Some(Ok(Ok(response))) => break response,
            Some(Ok(Err(e))) => return Err(format!("transport: {e:?}")),
            Some(Err(())) => return Err("response future consumed".to_string()),
            None => future.subscribe().block(),
        }
    };
    let status = response.status();

    let incoming = response.consume().map_err(|()| "consume")?;
    let stream = incoming.stream().map_err(|()| "incoming stream")?;
    let mut hasher = Sha256::new();
    let mut bytes = 0usize;
    let mut chunks = 0usize;
    let mut first_at = None;
    let mut last_at = 0u64;
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) if chunk.is_empty() => continue,
            Ok(chunk) => {
                let now = wasip2::clocks::monotonic_clock::now();
                first_at.get_or_insert(now);
                last_at = now;
                chunks += 1;
                bytes += chunk.len();
                hasher.update(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("read: {e:?}")),
        }
    }
    let gap_ms = first_at.map_or(0, |first| (last_at - first) / 1_000_000);

    Ok(format!(
        "key={key} endpoint={endpoint} authority={authority} status={status} sha256={:x} \
         bytes={bytes} chunks={chunks} gap_ms={gap_ms}",
        hasher.finalize()
    ))
}

impl Guest for EnvReportDriver {
    fn run(_input: ToolInput) -> ToolResult {
        let text = report().unwrap_or_else(|error| format!("error={error}"));
        let response = serde_json::json!({
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}]
        })
        .to_string();
        ToolResult {
            status: Status::Passed,
            summary: Some("reported".to_string()),
            data: Some(response),
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(EnvReportDriver);
