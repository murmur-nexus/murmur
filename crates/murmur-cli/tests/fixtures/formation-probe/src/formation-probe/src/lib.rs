// Test-only tool that reports, from inside a formation member, which callees its runtime handed
// it and whether each one answers through the runtime's formation egress. It holds no token: the
// runtime resolves each virtual address to the callee's real door and presents the callee's token
// itself.
//
// Its input is optional `{"names": ["coder", "reviewer"]}`; without it, the names are the ones
// `MURMUR_FORMATION_PEERS` lists. For each name, in order, it sends
// `GET http://<name>.formation.invalid/.well-known/agent-card.json` and one JSON-RPC `message/send`
// as `POST http://<name>.formation.invalid/`. Its summary is one line per name:
//
//   <name> card=<status|refused:<error>> send=<status|refused:<error>>
//
// where `refused:<error>` is the request's failure as wasi-http reported it, and then the line
// `peers=<MURMUR_FORMATION_PEERS, or absent>`.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const FORMATION_PEERS: &str = "MURMUR_FORMATION_PEERS";
const PEER_DOMAIN: &str = "formation.invalid";
const SEND_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"role":"user","messageId":"formation-probe","parts":[{"kind":"text","text":"hello from a formation member"}]}}}"#;

struct FormationProbe;

/// Sends one request to `authority` and returns the response status.
fn request(method: Method, authority: &str, path: &str, body: Option<&str>) -> Result<u16, String> {
    let fields = Fields::new();
    let mut headers = vec![("accept", b"application/json".to_vec())];
    if let Some(body) = body {
        headers.push(("content-type", b"application/json".to_vec()));
        headers.push(("content-length", body.len().to_string().into_bytes()));
    }
    for (name, value) in headers {
        fields
            .append(name, &value)
            .map_err(|e| format!("header: {e:?}"))?;
    }
    let outgoing = OutgoingRequest::new(fields);
    outgoing.set_method(&method).map_err(|()| "method")?;
    outgoing
        .set_scheme(Some(&Scheme::Http))
        .map_err(|()| "scheme")?;
    outgoing
        .set_authority(Some(authority))
        .map_err(|()| "authority")?;
    outgoing
        .set_path_with_query(Some(path))
        .map_err(|()| "path")?;

    let outgoing_body = outgoing.body().map_err(|()| "body")?;
    if let Some(body) = body {
        let stream = outgoing_body.write().map_err(|()| "body stream")?;
        stream
            .blocking_write_and_flush(body.as_bytes())
            .map_err(|e| format!("write: {e:?}"))?;
    }
    OutgoingBody::finish(outgoing_body, None).map_err(|e| format!("finish: {e:?}"))?;

    let future = wasip2::http::outgoing_handler::handle(outgoing, None)
        .map_err(|e| format!("{e:?}"))?;
    let response = loop {
        match future.get() {
            Some(Ok(Ok(response))) => break response,
            Some(Ok(Err(e))) => return Err(format!("{e:?}")),
            Some(Err(())) => return Err("response future consumed".to_string()),
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
            Err(e) => return Err(format!("read: {e:?}")),
        }
    }
    Ok(status)
}

fn outcome(result: Result<u16, String>) -> String {
    match result {
        Ok(status) => status.to_string(),
        Err(error) => format!("refused:{error}"),
    }
}

fn probe(name: &str) -> String {
    let authority = format!("{name}.{PEER_DOMAIN}");
    let card = outcome(request(
        Method::Get,
        &authority,
        "/.well-known/agent-card.json",
        None,
    ));
    let send = outcome(request(Method::Post, &authority, "/", Some(SEND_BODY)));
    format!("{name} card={card} send={send}")
}

fn report(input: &ToolInput) -> String {
    let peers = std::env::var(FORMATION_PEERS).ok();
    let asked: Option<Vec<String>> = input
        .data
        .as_deref()
        .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        .and_then(|value| {
            value["names"].as_array().map(|names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_string))
                    .collect()
            })
        });
    let names = asked.unwrap_or_else(|| {
        peers
            .as_deref()
            .unwrap_or_default()
            .split(' ')
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name.to_string()))
            .collect()
    });
    let mut lines: Vec<String> = names.iter().map(|name| probe(name)).collect();
    lines.push(format!("peers={}", peers.as_deref().unwrap_or("absent")));
    lines.join("\n")
}

impl Guest for FormationProbe {
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

export!(FormationProbe);
