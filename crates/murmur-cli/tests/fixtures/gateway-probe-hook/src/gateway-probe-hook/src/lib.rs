// Test-only `on-stage` hook that reaches its credential gateway and then tries one host no grant
// allows. A hook holds no preopened directory by default, so it reports back over the only channel
// it has: a second request through its gateway, whose body the test's upstream records.
//
// At `on-stage` it:
//
// 1. POSTs a fixed JSON body with a forged `Authorization: Bearer forged` header to
//    `<MURMUR_GATEWAY_ENDPOINT>/v1/hook`;
// 2. sends a GET to `http://<unlisted>/`, where `unlisted` is read from its operator `config:`
//    block (`MURMUR_ARTIFACT_CONFIG`, `{"unlisted": "127.0.0.1:<port>"}`);
// 3. POSTs one line to `<MURMUR_GATEWAY_ENDPOINT>/v1/hook-report`:
//
//      gateway_env=<set|absent> first=<status|error> unlisted=<status|error> env_sha256=<hex,...>
//
//    where `env_sha256` lists the sha256 of every environment value, so a test can show the key is
//    not among them without the hook ever being told the key.
//
// Every other lifecycle event is a no-op, and every outcome is `none`: the hook never fails the
// stage, so what the test observes is what the network gate decided.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/hook",
    world: "hook",
    generate_all,
});

use exports::murmur::hook::lifecycle::{
    CompactionEvent, Guest, HookOutput, InferenceEvent, SessionContext, SessionEndEvent,
    ShellEvent, StageEvent, TaskEndEvent, TaskStartEvent, ToolEvent,
};
use sha2::{Digest, Sha256};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const GATEWAY_ENDPOINT: &str = "MURMUR_GATEWAY_ENDPOINT";
const ARTIFACT_CONFIG: &str = "MURMUR_ARTIFACT_CONFIG";
const BARE_GATEWAY: &str = "http://127.0.0.1:9";
const FIRST_BODY: &[u8] = br#"{"query":"gateway-probe-hook"}"#;

struct GatewayProbeHook;

/// `http://authority/path` into its authority and path.
fn split(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url '{url}'"))?;
    Ok(match rest.find('/') {
        Some(index) => (rest[..index].to_string(), rest[index..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    })
}

/// Sends one request and returns its status, having read the body to the end.
fn send(
    method: Method,
    url: &str,
    headers: &[(&str, &[u8])],
    body: &[u8],
) -> Result<u16, String> {
    let (authority, path) = split(url)?;
    let fields = Fields::new();
    for (name, value) in headers {
        fields
            .append(name, value)
            .map_err(|e| format!("header: {e:?}"))?;
    }
    if !body.is_empty() {
        fields
            .append("content-length", body.len().to_string().as_bytes())
            .map_err(|e| format!("header: {e:?}"))?;
    }
    let request = OutgoingRequest::new(fields);
    request.set_method(&method).map_err(|()| "method")?;
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
    if !body.is_empty() {
        let stream = outgoing.write().map_err(|()| "body stream")?;
        stream
            .blocking_write_and_flush(body)
            .map_err(|e| format!("write: {e:?}"))?;
    }
    OutgoingBody::finish(outgoing, None).map_err(|e| format!("finish: {e:?}"))?;

    let future = wasip2::http::outgoing_handler::handle(request, None)
        .map_err(|e| format!("handle:{e:?}"))?;
    let response = loop {
        match future.get() {
            Some(Ok(Ok(response))) => break response,
            Some(Ok(Err(e))) => return Err(format!("transport:{e:?}")),
            Some(Err(())) => return Err("response-future-consumed".to_string()),
            None => future.subscribe().block(),
        }
    };
    let status = response.status();
    let incoming = response.consume().map_err(|()| "consume")?;
    let stream = incoming.stream().map_err(|()| "incoming stream")?;
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(_) => {}
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("read:{e:?}")),
        }
    }
    Ok(status)
}

fn outcome(result: Result<u16, String>) -> String {
    match result {
        Ok(status) => status.to_string(),
        Err(error) => error.replace(' ', "_"),
    }
}

fn probe() {
    let (gateway_env, endpoint) = match std::env::var(GATEWAY_ENDPOINT) {
        Ok(endpoint) => ("set", endpoint),
        Err(_) => ("absent", BARE_GATEWAY.to_string()),
    };
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let json: &[u8] = b"application/json";
    let forged: &[u8] = b"Bearer forged";

    let first = outcome(send(
        Method::Post,
        &format!("{endpoint}/v1/hook"),
        &[("content-type", json), ("authorization", forged)],
        FIRST_BODY,
    ));

    let unlisted = std::env::var(ARTIFACT_CONFIG)
        .ok()
        .and_then(|config| serde_json::from_str::<serde_json::Value>(&config).ok())
        .and_then(|config| config["unlisted"].as_str().map(str::to_string))
        .map(|authority| outcome(send(Method::Get, &format!("http://{authority}/"), &[], &[])))
        .unwrap_or_else(|| "not-configured".to_string());

    let env_sha256 = std::env::vars()
        .map(|(_, value)| format!("{:x}", Sha256::digest(value.as_bytes())))
        .collect::<Vec<_>>()
        .join(",");
    let report = format!(
        "gateway_env={gateway_env} first={first} unlisted={unlisted} env_sha256={env_sha256}"
    );
    let text: &[u8] = b"text/plain";
    let _ = send(
        Method::Post,
        &format!("{endpoint}/v1/hook-report"),
        &[("content-type", text)],
        report.as_bytes(),
    );
}

impl Guest for GatewayProbeHook {
    fn on_stage(_event: StageEvent) -> Result<HookOutput, String> {
        probe();
        Ok(HookOutput::None)
    }
    fn on_session_start(_ctx: SessionContext) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_task_start(_event: TaskStartEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_inference(_event: InferenceEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_tool_call(_event: ToolEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_shell(_event: ShellEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_compaction(_event: CompactionEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_task_end(_event: TaskEndEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
    fn on_session_end(_event: SessionEndEvent) -> Result<HookOutput, String> {
        Ok(HookOutput::None)
    }
}

export!(GatewayProbeHook);
