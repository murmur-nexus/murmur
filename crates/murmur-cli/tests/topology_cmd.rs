// Integration tests for `mur topology`.
//
// `topology` is gated twice: by the `beta-mur-topology` Cargo feature at compile time, and by
// `beta.enabled` in the user's config at run time. Both have to be satisfied or the binary
// answers with a clap-shaped `unrecognized subcommand 'topology'` — so this file is compiled
// only under the feature, and every invocation runs against a `HOME` whose config opts in.
#![cfg(feature = "beta-mur-topology")]

use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    thread,
    time::Duration,
};

use assert_cmd::Command;
use tempfile::TempDir;

/// A `mur` bound to a throwaway `HOME` that has the `mur-topology` beta switched on.
///
/// The `TempDir` is returned alongside the command because it has to outlive the run — dropping
/// it deletes the config the binary is about to read.
fn mur() -> (Command, TempDir) {
    let home = TempDir::new().unwrap();
    let config_dir = home.path().join(".murmur");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.yaml"),
        "beta:\n  enabled:\n    - mur-topology\n",
    )
    .unwrap();

    let mut cmd = Command::cargo_bin("mur").unwrap();
    cmd.env("HOME", home.path());
    (cmd, home)
}

// ── Mock Tempo server ─────────────────────────────────────────────────────────

/// Starts a minimal HTTP server that serves a fixed sequence of JSON responses
/// to sequential GET requests. Each response is served to one connection and
/// the connection is closed. The server thread exits after serving all
/// responses or after a 5-second idle timeout.
fn start_tempo_mock(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let endpoint = format!("http://{addr}");

    thread::spawn(move || {
        for body in responses {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                    drain_request(&mut stream);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(_) => break, // timeout or error → exit
            }
        }
    });

    endpoint
}

fn drain_request(stream: &mut std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() > 16384 {
                    break;
                }
            }
        }
    }
}

// ── Fixture builders ──────────────────────────────────────────────────────────

fn ready_response() -> String {
    r#"{"status":"ready"}"#.to_string()
}

/// Response for the `/status/buildinfo` probe that `run_topology` issues via
/// `detect_version()` between the readiness check and the search query. Its
/// content is irrelevant (version detection ignores parse errors), but a
/// response must be supplied or the sequential mock runs dry and the next
/// request (the search) hits a closed listener.
fn buildinfo_response() -> String {
    r#"{"version":"2.3.1"}"#.to_string()
}

fn search_response(trace_ids: &[&str]) -> String {
    let traces: Vec<serde_json::Value> = trace_ids
        .iter()
        .map(|id| {
            serde_json::json!({
                "traceID": id,
                "rootServiceName": "test-capsule",
                "rootTraceName": "capsule.session",
                "startTimeUnixNano": "1700000000000000000",
                "durationMs": 1000
            })
        })
        .collect();
    serde_json::json!({ "traces": traces }).to_string()
}

fn otlp_trace(
    trace_id: &str,
    span_id: &str,
    parent_span_id: &str,
    exit_status: &str,
    capsule_name: &str,
) -> String {
    serde_json::json!({
        "batches": [{
            "resource": {
                "attributes": [
                    {"key": "service.name", "value": {"stringValue": capsule_name}},
                    {"key": "service.version", "value": {"stringValue": "0.1.0"}}
                ]
            },
            "scopeSpans": [{
                "spans": [{
                    "traceId": trace_id,
                    "spanId": span_id,
                    "parentSpanId": parent_span_id,
                    "name": "capsule.session",
                    "startTimeUnixNano": "1700000000000000000",
                    "endTimeUnixNano": "1700000001000000000",
                    "attributes": [
                        {"key": "exit_status", "value": {"stringValue": exit_status}}
                    ]
                }]
            }]
        }]
    })
    .to_string()
}

// ── Test 1: single node, zero edges ──────────────────────────────────────────

#[test]
fn topology_graph_reconstruction_single_node() {
    let trace_id = "aaaa000000000000aaaa000000000001";
    let span_id = "aa01000000000001";

    let endpoint = start_tempo_mock(vec![
        ready_response(),
        buildinfo_response(),
        search_response(&[trace_id]),
        otlp_trace(trace_id, span_id, "", "ok", "capsule-a"),
    ]);

    let tmp = TempDir::new().unwrap();
    let output: PathBuf = tmp.path().join("topology.html");

    let (mut cmd, _home) = mur();
    cmd.args([
        "topology",
        "--otel-endpoint",
        &endpoint,
        "--output",
        output.to_str().unwrap(),
    ])
    .assert()
    .success();

    let html = std::fs::read_to_string(&output).unwrap();
    assert!(html.contains("capsule-a"), "HTML must contain capsule name");
    assert!(
        html.contains(r#""nodes":[{"id":"aaaa000000000000aaaa000000000001""#),
        "HTML must contain the node in window.TOPOLOGY_DATA"
    );
    // Edges array must be empty
    assert!(
        html.contains(r#""edges":[]"#),
        "HTML must have empty edges array; got: {html}"
    );
}

// ── Test 2: parent-child edge ─────────────────────────────────────────────────

#[test]
fn topology_graph_parent_child_edge() {
    let trace_a = "aaaa000000000000aaaa000000000001";
    let span_a = "aa01000000000001";

    let trace_b = "bbbb000000000000bbbb000000000002";
    let span_b = "bb01000000000001";

    // Trace B's session span has parentSpanId = span_a (cross-trace)
    let endpoint = start_tempo_mock(vec![
        ready_response(),
        buildinfo_response(),
        search_response(&[trace_a, trace_b]),
        otlp_trace(trace_a, span_a, "", "ok", "capsule-a"),
        otlp_trace(trace_b, span_b, span_a, "ok", "capsule-b"),
    ]);

    let tmp = TempDir::new().unwrap();
    let output: PathBuf = tmp.path().join("topology.html");

    let (mut cmd, _home) = mur();
    cmd.args([
        "topology",
        "--otel-endpoint",
        &endpoint,
        "--output",
        output.to_str().unwrap(),
    ])
    .assert()
    .success();

    let html = std::fs::read_to_string(&output).unwrap();
    assert!(html.contains("capsule-a"), "must contain capsule-a");
    assert!(html.contains("capsule-b"), "must contain capsule-b");

    // Edge from trace_a to trace_b must appear in the embedded JSON
    assert!(
        html.contains(&format!(r#""from":"{trace_a}""#)),
        "edge from must be trace_a; html: {html}"
    );
    assert!(
        html.contains(&format!(r#""to":"{trace_b}""#)),
        "edge to must be trace_b; html: {html}"
    );
}

// ── Test 3: node color by exit_status ────────────────────────────────────────

#[test]
fn topology_node_color_by_exit_status() {
    let trace_ok = "aaaa000000000000aaaa000000000001";
    let trace_failed = "bbbb000000000000bbbb000000000002";

    let endpoint = start_tempo_mock(vec![
        ready_response(),
        buildinfo_response(),
        search_response(&[trace_ok, trace_failed]),
        otlp_trace(trace_ok, "aa01000000000001", "", "ok", "capsule-ok"),
        otlp_trace(
            trace_failed,
            "bb01000000000001",
            "",
            "failed",
            "capsule-fail",
        ),
    ]);

    let tmp = TempDir::new().unwrap();
    let output: PathBuf = tmp.path().join("topology.html");

    let (mut cmd, _home) = mur();
    cmd.args([
        "topology",
        "--otel-endpoint",
        &endpoint,
        "--output",
        output.to_str().unwrap(),
    ])
    .assert()
    .success();

    let html = std::fs::read_to_string(&output).unwrap();

    // ok → teal
    assert!(
        html.contains("#26a69a"),
        "ok node must use teal background #26a69a"
    );
    // failed → coral
    assert!(
        html.contains("#ef5350"),
        "failed node must use coral background #ef5350"
    );
    // Colors must differ (border shades)
    assert!(html.contains("#1d7a74"), "ok node border #1d7a74");
    assert!(html.contains("#c62828"), "failed node border #c62828");
}

// ── Test 4: unreachable endpoint → non-zero exit, error names endpoint ────────

#[test]
fn topology_cli_error_on_unreachable_endpoint() {
    // Bind to get a free port, then drop so nothing is listening on it
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let endpoint = format!("http://127.0.0.1:{port}");

    let (mut cmd, _home) = mur();
    cmd.args(["topology", "--otel-endpoint", &endpoint])
        .assert()
        .failure()
        .stderr(predicates::str::contains(&endpoint));
}

// ── Test 5: empty search result → exit 0, page contains empty-graph message ──

#[test]
fn topology_empty_search_result_renders_empty_graph_message() {
    let endpoint = start_tempo_mock(vec![
        ready_response(),
        buildinfo_response(),
        r#"{"traces":[]}"#.to_string(),
    ]);

    let tmp = TempDir::new().unwrap();
    let output: PathBuf = tmp.path().join("topology.html");

    let (mut cmd, _home) = mur();
    cmd.args([
        "topology",
        "--otel-endpoint",
        &endpoint,
        "--output",
        output.to_str().unwrap(),
    ])
    .assert()
    .success();

    let html = std::fs::read_to_string(&output).unwrap();
    assert!(
        html.contains("No capsule sessions found"),
        "empty result must show no-sessions message"
    );
    assert!(
        html.contains("vis-network"),
        "HTML must load vis.js from CDN"
    );
    assert!(
        html.contains("window.TOPOLOGY_DATA"),
        "HTML must embed TOPOLOGY_DATA"
    );
}
