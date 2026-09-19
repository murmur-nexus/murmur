// Test-only tool that reports what its credential gateway looks like from inside the guest, and
// what came back through it. It never holds a key: it is told the key marker only so it can say
// whether any of its environment values contains it.
//
// Its input `data` is the marker, hex-encoded — bare, as a script capsule passes it, or as the
// `marker_hex` field of a JSON object, as a model's tool call does — so the marker itself is written
// to no file of the session. It reads `MURMUR_GATEWAY_ENDPOINT` — falling back to the bare gateway authority when the
// variable is absent, so a store without a gateway still tries it — and POSTs a fixed JSON body with
// a forged `Authorization: Bearer forged` header to `<endpoint>/v1/probe`. Its summary is one line:
//
//   gateway_env=<set|absent> endpoint=<url> marker_in_env=<true|false> status=<code> sha256=<hex>
//
// or `... error=<what>` in place of `status` and `sha256` when the request did not complete.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use sha2::{Digest, Sha256};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const GATEWAY_ENDPOINT: &str = "MURMUR_GATEWAY_ENDPOINT";
/// What a store without a gateway is left to try.
const BARE_GATEWAY: &str = "http://127.0.0.1:9";
const REQUEST_BODY: &[u8] = br#"{"query":"gateway-probe"}"#;

struct GatewayProbe;

fn decode_hex(hex: &str) -> String {
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes).unwrap_or_default()
}

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
        format!("{}/v1/probe", path.trim_end_matches('/')),
    ))
}

/// POSTs the probe body and returns the response status and the sha256 of its body.
fn post(endpoint: &str) -> Result<(u16, String), String> {
    let (authority, path) = split_endpoint(endpoint)?;
    let fields = Fields::new();
    for (name, value) in [
        ("content-type", b"application/json".to_vec()),
        ("authorization", b"Bearer forged".to_vec()),
        (
            "content-length",
            REQUEST_BODY.len().to_string().into_bytes(),
        ),
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
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => hasher.update(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("read: {e:?}")),
        }
    }
    Ok((status, format!("{:x}", hasher.finalize())))
}

/// The hex-encoded marker `data` carries, bare or as a JSON object's `marker_hex`.
fn marker_hex(data: &str) -> String {
    serde_json::from_str::<serde_json::Value>(data)
        .ok()
        .and_then(|value| value["marker_hex"].as_str().map(str::to_string))
        .unwrap_or_else(|| data.trim().to_string())
}

fn report(input: &ToolInput) -> String {
    let marker = decode_hex(&marker_hex(input.data.as_deref().unwrap_or_default()));
    let marker_in_env =
        !marker.is_empty() && std::env::vars().any(|(_, value)| value.contains(&marker));
    let (gateway_env, endpoint) = match std::env::var(GATEWAY_ENDPOINT) {
        Ok(endpoint) => ("set", endpoint),
        Err(_) => ("absent", BARE_GATEWAY.to_string()),
    };
    let outcome = match post(&endpoint) {
        Ok((status, sha256)) => format!("status={status} sha256={sha256}"),
        Err(error) => format!("error={error}"),
    };
    format!(
        "gateway_env={gateway_env} endpoint={endpoint} marker_in_env={marker_in_env} {outcome}"
    )
}

impl Guest for GatewayProbe {
    fn run(input: ToolInput) -> ToolResult {
        ToolResult {
            status: Status::Passed,
            summary: Some(report(&input)),
            data: None,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(GatewayProbe);
