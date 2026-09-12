//! The inference gateway: a `transport: http` driver reaches its provider through the runtime,
//! which attaches the credential the driver's own `inference_auth:` block declares. The driver
//! never holds the key.
//!
//! Driven through the real `mur` binary against a recording upstream on loopback, so every
//! assertion is about bytes that crossed a socket: the request head and body the upstream saw,
//! and the streamed body the driver read back.

#[path = "common/mod.rs"]
mod common;

use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use assert_cmd::Command;
use tempfile::TempDir;

const ANTHROPIC_DRIVER: &str = "murmur-driver-anthropic";
const DRIVER_VERSION: &str = "0.1.0";
const ANTHROPIC_AUTH: &str = "inference_auth:\n  header: x-api-key\n  value: \"{key}\"\n";

/// One request as the upstream read it off the socket.
#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    target: String,
    /// Names lowercased, in arrival order.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Method, target, headers sorted by name, blank line, body — with the upstream's ephemeral
    /// port replaced so the rendering is stable across runs.
    fn render(&self, port: u16) -> String {
        let mut headers = self.headers.clone();
        headers.sort();
        let mut out = format!("{} {}\n", self.method, self.target);
        for (name, value) in headers {
            out.push_str(&format!("{name}: {value}\n"));
        }
        out.push('\n');
        out.push_str(&String::from_utf8_lossy(&self.body));
        out.replace(&format!("127.0.0.1:{port}"), "127.0.0.1:<upstream-port>")
    }
}

/// What the upstream writes back for every request.
#[derive(Clone)]
enum Reply {
    /// `200` with a `content-length` JSON body.
    Json(String),
    /// `Json` with the n-th body for the n-th request, the last repeated.
    Sequence(Vec<String>),
    /// `200 text/event-stream`, chunked: `first` is written and flushed, then `pause`, then
    /// `rest`.
    Streamed {
        first: Vec<u8>,
        rest: Vec<u8>,
        pause: Duration,
    },
}

struct RecordingUpstream {
    endpoint: String,
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl RecordingUpstream {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let Some(request) = read_request(&mut stream) else {
                    continue;
                };
                recorded.lock().unwrap().push(request);
                let reply = match &reply {
                    Reply::Sequence(bodies) => {
                        Reply::Json(bodies[index.min(bodies.len() - 1)].clone())
                    }
                    other => other.clone(),
                };
                let _ = write_reply(&mut stream, &reply);
            }
        });
        Self {
            endpoint: format!("http://127.0.0.1:{port}"),
            port,
            requests,
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut body = buffer[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();

    let content_length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    if let Some(length) = content_length {
        while body.len() < length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
    } else if chunked {
        while !body.windows(5).any(|w| w == b"0\r\n\r\n") {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body = dechunk(&body);
    }
    Some(RecordedRequest {
        method,
        target,
        headers,
        body,
    })
}

fn dechunk(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&rest[..pos]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = pos + 2;
        out.extend_from_slice(&rest[start..start + size]);
        rest = &rest[start + size + 2..];
    }
    out
}

fn write_reply(stream: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    match reply {
        Reply::Sequence(_) => unreachable!("resolved to one body per request"),
        Reply::Json(body) => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            )?;
            stream.flush()
        }
        Reply::Streamed { first, rest, pause } => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 transfer-encoding: chunked\r\nconnection: close\r\n\r\n"
            )?;
            write_chunk(stream, first)?;
            stream.flush()?;
            thread::sleep(*pause);
            write_chunk(stream, rest)?;
            stream.write_all(b"0\r\n\r\n")?;
            stream.flush()
        }
    }
}

fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")
}

struct Capsule {
    home: TempDir,
    project: TempDir,
    manifest: PathBuf,
}

impl Capsule {
    /// A capsule wired to `driver`, with `extra` spliced in verbatim at the top level.
    fn new(driver: &str, wasm: &Path, auth_block: &str, endpoint: &str, extra: &str) -> Self {
        let home = TempDir::new().unwrap();
        let artifacts = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        let artifact = common::create_driver_artifact_with_auth(
            artifacts.path(),
            driver,
            DRIVER_VERSION,
            wasm,
            auth_block,
        );
        common::publish_local(&home, &artifact).success();
        let manifest = project.path().join("murmur.yaml");
        fs::write(
            &manifest,
            format!(
                "name: gateway-capsule\nversion: 0.1.0\nartifacts:\n  - name: {driver}\n    \
                 version: {DRIVER_VERSION}\n    runtime: driver\n{extra}inference:\n  \
                 transport: http\n  endpoint: {endpoint}\n  model: test-model\n  \
                 api_key: ${{GATEWAY_TEST_KEY}}\n  driver:\n    artifact: {driver}\n"
            ),
        )
        .unwrap();
        Self {
            home,
            project,
            manifest,
        }
    }

    fn run(&self, key: &str, extra_args: &[&str]) -> std::process::Output {
        Command::cargo_bin("mur")
            .unwrap()
            .env("HOME", self.home.path())
            .env_remove("NEXUS_API_KEY")
            .env("GATEWAY_TEST_KEY", key)
            .current_dir(self.project.path())
            .args([
                "run",
                "--manifest",
                self.manifest.to_str().unwrap(),
                "--task",
                "Say hello.",
                "--verbose",
            ])
            .args(extra_args)
            .output()
            .unwrap()
    }
}

fn anthropic_wasm() -> PathBuf {
    common::fixture_path("drivers/anthropic/driver/murmur-driver-anthropic.wasm")
}

fn golden_path() -> PathBuf {
    common::fixture_path("inference-gateway/anthropic-request.txt")
}

const GOLDEN_KEY: &str = "sk-ant-golden-marker";

/// The real anthropic driver's request, as the provider receives it, is byte-for-byte the
/// recording — same method, target, headers (with `host` naming the upstream) and body, and
/// exactly one `x-api-key`.
///
/// `MURMUR_RECORD_GOLDEN=1` rewrites the recording instead of comparing against it.
#[test]
fn upstream_request_matches_recording() {
    let upstream = RecordingUpstream::start(Reply::Json(
        r#"{"id":"msg_1","type":"message","role":"assistant","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":1,"output_tokens":1}}"#
            .to_string(),
    ));
    let recording = std::env::var_os("MURMUR_RECORD_GOLDEN").is_some();
    // A runtime without the gateway reaches the provider only through `network.allow`. The entry
    // changes nothing about the request itself, so it is present only while recording.
    let allow = if recording {
        format!(
            "capabilities:\n  network:\n    allow:\n      - {}\n",
            upstream.endpoint
        )
    } else {
        String::new()
    };
    let capsule = Capsule::new(
        ANTHROPIC_DRIVER,
        &anthropic_wasm(),
        ANTHROPIC_AUTH,
        &upstream.endpoint,
        &allow,
    );
    let output = capsule.run(GOLDEN_KEY, &[]);
    let requests = upstream.requests();
    assert_eq!(
        requests.len(),
        1,
        "expected one upstream request; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = &requests[0];
    assert_eq!(request.header_values("x-api-key"), vec![GOLDEN_KEY]);
    assert_eq!(
        request.header_values("host"),
        vec![format!("127.0.0.1:{}", upstream.port).as_str()]
    );
    let rendered = request.render(upstream.port);

    if std::env::var_os("MURMUR_RECORD_GOLDEN").is_some() {
        fs::create_dir_all(golden_path().parent().unwrap()).unwrap();
        fs::write(golden_path(), &rendered).unwrap();
        return;
    }
    let golden = fs::read_to_string(golden_path()).expect("golden recording is committed");
    assert_eq!(rendered, golden);
}

const ENV_REPORT_DRIVER: &str = "env-report-driver";
const MARKER: &str = "sk-gateway-test-marker";
const W_SEC_025_LINK: &str =
    "https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-025";

fn env_report_wasm() -> PathBuf {
    common::fixture_path("env-report-driver/tool/env-report-driver.wasm")
}

const FIRST_EVENT: &[u8] = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
const REST_EVENTS: &[u8] = b"event: content_block_delta\n\
    data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn streamed_upstream() -> RecordingUpstream {
    RecordingUpstream::start(Reply::Streamed {
        first: FIRST_EVENT.to_vec(),
        rest: REST_EVENTS.to_vec(),
        pause: Duration::from_secs(2),
    })
}

/// The driver's `key=value` report for `field`.
fn reported<'a>(report: &'a str, field: &str) -> &'a str {
    report
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix(&format!("{field}=")))
        .unwrap_or_else(|| panic!("'{field}=' missing from driver report: {report:?}"))
}

struct Run {
    output: std::process::Output,
    stdout: String,
    stderr: String,
}

impl Run {
    fn of(output: std::process::Output) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            output,
        }
    }

    fn workdir(&self) -> PathBuf {
        common::parse_workdir_from_stdout(&self.stdout)
    }

    fn result(&self) -> String {
        fs::read_to_string(self.workdir().join("out/result.txt")).unwrap_or_else(|err| {
            panic!(
                "no out/result.txt ({err}); stdout:\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }

    fn warning_lines(&self, code: &str) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| line.contains(code))
            .collect()
    }
}

impl Capsule {
    fn explain_scope(&self) -> Run {
        Run::of(
            Command::cargo_bin("mur")
                .unwrap()
                .env("HOME", self.home.path())
                .env_remove("NEXUS_API_KEY")
                .env("GATEWAY_TEST_KEY", MARKER)
                .current_dir(self.project.path())
                .args([
                    "run",
                    "--manifest",
                    self.manifest.to_str().unwrap(),
                    "--explain-scope",
                ])
                .output()
                .unwrap(),
        )
    }
}

fn env_report_capsule(upstream: &RecordingUpstream, extra: &str) -> Capsule {
    Capsule::new(
        ENV_REPORT_DRIVER,
        &env_report_wasm(),
        ANTHROPIC_AUTH,
        &upstream.endpoint,
        extra,
    )
}

/// A driver that sends no auth header and holds no key reaches its provider through the
/// gateway. The provider sees exactly one `x-api-key`, the operator's, and the streamed body
/// arrives at the driver byte for byte and still in pieces, the pause between them intact.
#[test]
fn gateway_happy_path() {
    let upstream = streamed_upstream();
    let capsule = env_report_capsule(&upstream, "");
    let run = Run::of(capsule.run(MARKER, &[]));
    assert!(
        run.stdout.contains("status:  ok"),
        "stdout:\n{}\nstderr:\n{}",
        run.stdout,
        run.stderr
    );

    let report = run.result();
    println!("driver report: {report}");
    assert_eq!(
        reported(&report, "key"),
        "absent",
        "MURMUR_INFERENCE_API_KEY"
    );
    println!("MURMUR_INFERENCE_API_KEY={}", reported(&report, "key"));
    assert_eq!(reported(&report, "endpoint"), "http://127.0.0.1:9");
    assert_eq!(reported(&report, "authority"), "127.0.0.1:9");
    assert_eq!(reported(&report, "status"), "200");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    let keys = requests[0].header_values("x-api-key");
    println!("upstream x-api-key headers: {keys:?}");
    assert_eq!(keys, vec![MARKER]);
    assert_eq!(requests[0].target, "/v1/messages");

    let served: Vec<u8> = [FIRST_EVENT, REST_EVENTS].concat();
    let served_sha = {
        use sha2::{Digest, Sha256};
        format!("{:x}", Sha256::digest(&served))
    };
    println!(
        "served sha256={served_sha} driver sha256={}",
        reported(&report, "sha256")
    );
    assert_eq!(reported(&report, "sha256"), served_sha);
    assert_eq!(reported(&report, "bytes"), served.len().to_string());

    let chunks: usize = reported(&report, "chunks").parse().unwrap();
    let gap_ms: u64 = reported(&report, "gap_ms").parse().unwrap();
    println!("chunks={chunks} first-to-last gap_ms={gap_ms}");
    assert!(chunks >= 2, "body arrived in {chunks} chunk(s)");
    assert!(gap_ms >= 1500, "first-to-last chunk gap was {gap_ms} ms");

    assert!(run.warning_lines("W-SEC-025").is_empty(), "{}", run.stderr);
}

/// A driver that declares no usable `inference_auth:` refuses to start, by name and version,
/// before anything reaches the provider.
#[test]
fn driver_without_inference_auth_refuses() {
    for (auth_block, reason) in [
        ("", None),
        (
            "inference_auth:\n  header: Authorization\n  value: \"Bearer\"\n",
            Some("exactly once"),
        ),
    ] {
        let upstream = streamed_upstream();
        let capsule = Capsule::new(
            ENV_REPORT_DRIVER,
            &env_report_wasm(),
            auth_block,
            &upstream.endpoint,
            "",
        );
        let run = Run::of(capsule.run(MARKER, &[]));
        let combined = format!("{}{}", run.stdout, run.stderr);
        println!("{}", combined.trim());
        assert!(!run.output.status.success(), "{combined}");
        assert!(combined.contains("E-RUN-025"), "{combined}");
        assert!(
            combined.contains(&format!("{ENV_REPORT_DRIVER}@{DRIVER_VERSION}")),
            "{combined}"
        );
        if let Some(reason) = reason {
            assert!(combined.contains(reason), "{combined}");
        }
        assert!(!combined.contains(MARKER), "{combined}");
        assert!(upstream.requests().is_empty());
    }
}

/// Naming the provider in `network.allow` is accepted and warned about once, on a real run and
/// under `--explain-scope`.
#[test]
fn provider_in_network_allow_warns() {
    let upstream = streamed_upstream();
    let allow = format!(
        "capabilities:\n  network:\n    allow:\n      - {}\n",
        upstream.endpoint
    );
    let capsule = env_report_capsule(&upstream, &allow);

    let run = Run::of(capsule.run(MARKER, &[]));
    assert!(run.stdout.contains("status:  ok"), "{}", run.stderr);
    let lines = run.warning_lines("W-SEC-025");
    println!("{}", lines.join("\n"));
    assert_eq!(lines.len(), 1, "{}", run.stderr);
    assert!(lines[0].contains(W_SEC_025_LINK), "{}", lines[0]);
    assert!(lines[0].contains(&format!("'{}'", upstream.endpoint)));
    assert_eq!(reported(&run.result(), "key"), "absent");

    let explained = capsule.explain_scope();
    assert!(explained.output.status.success(), "{}", explained.stderr);
    let lines = explained.warning_lines("W-SEC-025");
    assert_eq!(lines.len(), 1, "{}", explained.stderr);
    assert!(lines[0].contains(W_SEC_025_LINK));
}

fn files_containing(root: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if fs::read(&path)
                .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

/// At every `trace.capture` setting the key is nowhere the session writes — its workdir with
/// `trace.jsonl`, blobs, logs and `out/`, the conversation records under `HOME` — nor on stdout or
/// stderr.
#[test]
fn key_never_recorded() {
    for capture in ["none", "meta", "content"] {
        let upstream = streamed_upstream();
        let capsule = env_report_capsule(&upstream, &format!("trace:\n  capture: {capture}\n"));
        let run = Run::of(capsule.run(MARKER, &[]));
        assert!(
            run.stdout.contains("status:  ok"),
            "{capture}: {}",
            run.stderr
        );
        assert!(run.workdir().join("trace.jsonl").is_file(), "{capture}");
        assert_eq!(
            upstream.requests()[0].header_values("x-api-key"),
            vec![MARKER]
        );

        let mut leaks = files_containing(capsule.project.path(), MARKER.as_bytes());
        leaks.extend(files_containing(capsule.home.path(), MARKER.as_bytes()));
        println!("trace.capture={capture}: files containing the key: {leaks:?}");
        assert!(leaks.is_empty(), "{capture}: {leaks:?}");
        assert!(!run.stdout.contains(MARKER), "{capture}: stdout");
        assert!(!run.stderr.contains(MARKER), "{capture}: stderr");
    }
}

/// A shell subprocess's environment carries no key either: the session's inference environment,
/// the only place it could come from, holds none.
#[test]
fn no_guest_observes_the_key() {
    if common::skip_without_host_support("no_guest_observes_the_key") {
        return;
    }
    let upstream = RecordingUpstream::start(Reply::Sequence(vec![
        r#"{"id":"msg_1","type":"message","role":"assistant","model":"test-model","content":[{"type":"tool_use","id":"toolu_env","name":"bash","input":{"command":"env"}}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#.to_string(),
        r#"{"id":"msg_2","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#.to_string(),
    ]));
    let capsule = Capsule::new(
        ANTHROPIC_DRIVER,
        &anthropic_wasm(),
        ANTHROPIC_AUTH,
        &upstream.endpoint,
        "capabilities:\n  shell:\n    allow:\n      - bash\n      - env\n",
    );
    let run = Run::of(capsule.run(MARKER, &[]));
    assert!(run.stdout.contains("status:  ok"), "{}", run.stderr);

    let requests = upstream.requests();
    assert_eq!(requests.len(), 2, "{}", run.stderr);
    let tool_turn = String::from_utf8_lossy(&requests[1].body);
    // The `env` output made it back into the conversation, so its absence of the key means
    // something.
    assert!(
        tool_turn.contains("MURMUR_INFERENCE_ENDPOINT=http://127.0.0.1:9"),
        "{tool_turn}"
    );
    assert!(
        !tool_turn.contains("MURMUR_INFERENCE_API_KEY"),
        "{tool_turn}"
    );
    assert!(!tool_turn.contains(MARKER), "{tool_turn}");
    for request in &requests {
        assert_eq!(request.header_values("x-api-key"), vec![MARKER]);
    }
}
