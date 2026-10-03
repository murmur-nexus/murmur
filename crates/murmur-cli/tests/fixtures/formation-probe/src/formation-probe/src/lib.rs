// Test-only tool that reports, from inside a formation's entry member, which peers it was handed
// and whether each one's door answers. It holds no token: every request it makes is
// unauthenticated, so an authenticated peer's door answers its public agent card with 200 and a
// JSON-RPC `message/send` with 401.
//
// It reads `MURMUR_FORMATION_PEERS` — `name=url` pairs separated by single spaces — and for each
// pair sends `GET <url>/.well-known/agent-card.json` and one `POST <url>/` carrying a JSON-RPC
// `message/send`. Its summary is one line per peer, in the order the variable names them:
//
//   <name> card=<status> card_name=<card's "name"> send=<status>
//
// with `error=<what>` in place of a status when a request did not complete, and the single line
// `peers=absent` when the variable is not set.

wit_bindgen::generate!({
    path: "../../../../../../capsule-runtime/wit/guest",
    world: "tool",
    generate_all,
});

use exports::murmur::tool::run::{Guest, Status, ToolInput, ToolResult};
use wasip2::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasip2::io::streams::StreamError;

const FORMATION_PEERS: &str = "MURMUR_FORMATION_PEERS";
const SEND_BODY: &str = r#"{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"role":"user","messageId":"formation-probe","parts":[{"kind":"text","text":"hello from the entry member"}]}}}"#;

struct FormationProbe;

/// Sends one request and returns the response status and body.
fn request(method: Method, url: &str, path: &str, body: Option<&str>) -> Result<(u16, Vec<u8>), String> {
    let authority = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported url '{url}'"))?
        .trim_end_matches('/');
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
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("read: {e:?}")),
        }
    }
    Ok((status, bytes))
}

fn probe(name: &str, url: &str) -> String {
    let card = match request(Method::Get, url, "/.well-known/agent-card.json", None) {
        Ok((status, body)) => {
            let card_name = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|card| card["name"].as_str().map(str::to_string))
                .unwrap_or_default();
            format!("card={status} card_name={card_name}")
        }
        Err(error) => format!("card_error={error}"),
    };
    let send = match request(Method::Post, url, "/", Some(SEND_BODY)) {
        Ok((status, _)) => format!("send={status}"),
        Err(error) => format!("send_error={error}"),
    };
    format!("{name} {card} {send}")
}

fn report() -> String {
    let Ok(peers) = std::env::var(FORMATION_PEERS) else {
        return "peers=absent".to_string();
    };
    peers
        .split(' ')
        .map(|pair| match pair.split_once('=') {
            Some((name, url)) => probe(name, url),
            None => format!("error=unreadable pair '{pair}'"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Guest for FormationProbe {
    fn run(_input: ToolInput) -> ToolResult {
        ToolResult {
            status: Status::Passed,
            summary: Some(report()),
            data: None,
            data_path: None,
            truncated: false,
            metadata: Vec::new(),
        }
    }
}

export!(FormationProbe);
