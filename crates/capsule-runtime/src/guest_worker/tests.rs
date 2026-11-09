use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::wire::{
    encode, read_frame, write_frame, FrameError, FromWorker, HostReply, HostRequest, HttpError,
    HttpRequestHead, HttpResponseHead, Reply, StreamEvent, ToWorker, MAX_FRAME_BYTES,
};
use super::*;
use crate::bindings::host::murmur::tool::run::{Status, ToolInput, ToolResult};
use crate::limits::{CallDeadline, CallDeadlines, ComponentRole, ExecutionLimits};
use crate::runtime::{ToolCallSpec, WasiPlan};
use crate::session_process::{ProcessCall, ProcessReply};

type Peers = Arc<Mutex<HashMap<WorkerKind, UnixStream>>>;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Starts a `kind` worker whose end the test holds, and speaks its handshake.
fn peer_worker(workers: &Arc<GuestWorkers>, peers: &Peers, kind: WorkerKind) -> UnixStream {
    workers.start(kind);
    let mut stream = loop {
        if let Some(stream) = peers.lock().unwrap().remove(&kind) {
            break stream;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    write_frame(
        &mut stream,
        &FromWorker::Hello {
            protocol: PROTOCOL_VERSION,
            version: "test".to_string(),
            pid: std::process::id(),
        },
    )
    .unwrap();
    match read_frame::<ToWorker>(&mut stream).unwrap() {
        ToWorker::Init { kind: told } => assert_eq!(told, kind),
        other => panic!("expected Init, got {other:?}"),
    }
    stream
}

fn spec(name: &str, env: Vec<(String, String)>) -> ToolCallSpec {
    ToolCallSpec {
        name: name.to_string(),
        role: ComponentRole::Tool,
        limits: ExecutionLimits::default(),
        wasi: WasiPlan {
            env,
            preopens: vec![crate::guest_roots::PlannedPreopen {
                host_path: "/tmp".into(),
                guest_path: ".".to_string(),
                read_only: false,
            }],
        },
    }
}

fn round_trip<T: serde::Serialize + for<'de> serde::Deserialize<'de> + std::fmt::Debug>(
    message: &T,
) -> T {
    let frame = encode(message).unwrap();
    let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
    assert_eq!(len, frame.len() - 4, "the prefix is the body's length");
    read_frame(&mut frame.as_slice()).unwrap()
}

#[test]
fn every_frame_kind_round_trips() {
    let to_worker = vec![
        ToWorker::Init {
            kind: WorkerKind::Tools,
        },
        ToWorker::Load {
            req: 1,
            guest: 2,
            name: "tool-read".to_string(),
            form: vec![0, 1, 2, 255],
        },
        ToWorker::RunTool {
            req: 3,
            guest: 2,
            spec: Box::new(spec("tool-read", vec![("A".into(), "b".into())])),
            input: ToolInput {
                data: Some("{}".to_string()),
                log_path: None,
            },
        },
        ToWorker::Cancel { req: 3 },
        ToWorker::HostReply {
            host: 4,
            reply: HostReply::Http(Ok(HttpResponseHead {
                status: 200,
                headers: vec![("content-type".into(), b"text/plain".to_vec())],
            })),
        },
        ToWorker::HostReply {
            host: 5,
            reply: HostReply::Http(Err(HttpError::HttpRequestDenied)),
        },
        ToWorker::HostReply {
            host: 6,
            reply: HostReply::Input(Ok("yes".into())),
        },
        ToWorker::HostReply {
            host: 7,
            reply: HostReply::Process(ProcessReply::Found(Some("prc_1".into()))),
        },
        ToWorker::HostReply {
            host: 8,
            reply: HostReply::Refused("no".into()),
        },
        ToWorker::BodyChunk {
            stream: 4,
            bytes: b"data: x\n\n".to_vec(),
        },
        ToWorker::BodyEnd {
            stream: 4,
            error: Some(HttpError::DnsError {
                rcode: Some("address not available".into()),
                info_code: None,
            }),
        },
        ToWorker::Shutdown,
    ];
    for message in &to_worker {
        let back = round_trip(message);
        assert_eq!(format!("{back:?}"), format!("{message:?}"));
    }
    let from_worker = vec![
        FromWorker::Hello {
            protocol: PROTOCOL_VERSION,
            version: "0.11.0-dev".into(),
            pid: 42,
        },
        FromWorker::Reply {
            req: 1,
            reply: Reply::Loaded(Ok(())),
        },
        FromWorker::Reply {
            req: 3,
            reply: Reply::Tool {
                result: Ok(ToolResult {
                    status: Status::Passed,
                    summary: None,
                    data: Some("ok".into()),
                    data_path: None,
                    truncated: false,
                    metadata: vec![("k".into(), "v".into())],
                    attachments: Vec::new(),
                }),
                deadlines: vec![CallDeadline {
                    component: "tool-read".into(),
                    role: ComponentRole::Tool,
                    call: "run".into(),
                    deadline_seconds: 1,
                    elapsed_ms: 1000,
                }],
            },
        },
        FromWorker::HostCall {
            host: 1,
            call: 3,
            request: HostRequest::Http(HttpRequestHead {
                method: "POST".into(),
                uri: "http://127.0.0.1:9/v1/messages".into(),
                headers: vec![("x".into(), vec![1, 2])],
                connect_timeout: Some(Duration::from_secs(1)),
                first_byte_timeout: None,
                between_bytes_timeout: None,
                has_options: true,
                body: b"{}".to_vec(),
            }),
        },
        FromWorker::HostCall {
            host: 2,
            call: 3,
            request: HostRequest::RequestInput {
                prompt: "why?".into(),
            },
        },
        FromWorker::HostCall {
            host: 3,
            call: 3,
            request: HostRequest::Process(ProcessCall::Read("prc_1".into(), 64)),
        },
        FromWorker::Event {
            call: 3,
            event: StreamEvent::ToolCallInputBytes {
                id: "c1".into(),
                bytes: 9,
            },
        },
        FromWorker::RunStarted {
            call: 3,
            deadline: Duration::from_secs(30),
        },
    ];
    for message in &from_worker {
        let back = round_trip(message);
        assert_eq!(format!("{back:?}"), format!("{message:?}"));
    }
}

/// An oversize length prefix is refused from the prefix alone: no body follows it, and none is
/// read or allocated.
#[test]
fn an_oversize_frame_is_refused_before_allocation() {
    let prefix = (MAX_FRAME_BYTES + 1).to_le_bytes();
    let result = read_frame::<ToWorker>(&mut prefix.as_slice());
    assert!(
        matches!(result, Err(FrameError::Oversize(len)) if len == MAX_FRAME_BYTES + 1),
        "{result:?}"
    );
    let max = u32::MAX.to_le_bytes();
    assert!(matches!(
        read_frame::<ToWorker>(&mut max.as_slice()),
        Err(FrameError::Oversize(_))
    ));
}

/// Every `wasi:http` error a guest can be shown crosses the connection as itself.
#[test]
fn http_errors_cross_as_the_code_the_guest_sees() {
    use wasmtime_wasi_http::Error as E;
    let cases = vec![
        E::HttpRequestDenied,
        E::ConnectionRefused,
        E::ConnectionTimeout,
        E::HttpResponseTimeout,
        E::DnsError {
            rcode: Some("x".into()),
            info_code: Some(3),
        },
        E::InternalError(Some("boom".into())),
        E::HttpRequestUriInvalid,
    ];
    for error in cases {
        let wire = http::to_wire(&error);
        let back = http::from_wire(round_trip(&wire));
        assert_eq!(back.to_string(), error.to_string());
    }
    let connect = E::Connect(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    assert_eq!(http::to_wire(&connect), HttpError::ConnectionRefused);
    let tls = E::Tls(std::io::Error::other("bad cert"));
    assert_eq!(http::to_wire(&tls), HttpError::TlsProtocolError);
}

/// A host call naming a call id not in flight on its connection — never issued, or issued on
/// another worker's connection — is refused and runs nothing; the call in flight on its own
/// connection is answered.
#[test]
fn a_host_call_outside_its_own_call_is_refused() {
    let peers: Peers = Arc::default();
    let workers = GuestWorkers::new(Launcher::Peer(Arc::clone(&peers)));
    let mut tools = peer_worker(&workers, &peers, WorkerKind::Tools);
    let mut driver = peer_worker(&workers, &peers, WorkerKind::Driver);
    workers.ready().unwrap();
    let (call, _) = workers.register_test_call(WorkerKind::Tools);

    // The tools worker's call, named on the driver worker's connection.
    write_frame(
        &mut driver,
        &FromWorker::HostCall {
            host: 1,
            call,
            request: HostRequest::RequestInput { prompt: "p".into() },
        },
    )
    .unwrap();
    match read_frame::<ToWorker>(&mut driver).unwrap() {
        ToWorker::HostReply {
            host: 1,
            reply: HostReply::Refused(reason),
        } => assert!(reason.contains("not in flight"), "{reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }

    // A call id never issued, on the tools worker's own connection.
    write_frame(
        &mut tools,
        &FromWorker::HostCall {
            host: 2,
            call: u64::MAX,
            request: HostRequest::Process(ProcessCall::Find("x".into())),
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<ToWorker>(&mut tools).unwrap(),
        ToWorker::HostReply {
            host: 2,
            reply: HostReply::Refused(_)
        }
    ));

    // The call in flight, on its own connection: answered by the call's own objects.
    write_frame(
        &mut tools,
        &FromWorker::HostCall {
            host: 3,
            call,
            request: HostRequest::RequestInput { prompt: "p".into() },
        },
    )
    .unwrap();
    match read_frame::<ToWorker>(&mut tools).unwrap() {
        ToWorker::HostReply {
            host: 3,
            reply: HostReply::Input(Err(error)),
        } => assert!(error.contains("outside an A2A task context"), "{error}"),
        other => panic!("expected the call's own answer, got {other:?}"),
    }
}

/// An event sent before a reply is applied before the reply resolves.
#[test]
fn events_before_a_reply_are_applied_before_it_resolves() {
    let peers: Peers = Arc::default();
    let workers = GuestWorkers::new(Launcher::Peer(Arc::clone(&peers)));
    let mut tools = peer_worker(&workers, &peers, WorkerKind::Tools);
    workers.ready().unwrap();
    let (call, events) = workers.register_test_call(WorkerKind::Tools);
    let rt = runtime();
    let req = 9_000_001;
    let pending = {
        let workers = Arc::clone(&workers);
        rt.spawn(async move {
            workers
                .request_for_test(
                    WorkerKind::Tools,
                    req,
                    ToWorker::Load {
                        req,
                        guest: 1,
                        name: "x".into(),
                        form: Vec::new(),
                    },
                )
                .await
        })
    };
    assert!(matches!(
        read_frame::<ToWorker>(&mut tools).unwrap(),
        ToWorker::Load { .. }
    ));
    write_frame(
        &mut tools,
        &FromWorker::Event {
            call,
            event: StreamEvent::Chunk("hello".into()),
        },
    )
    .unwrap();
    write_frame(
        &mut tools,
        &FromWorker::Reply {
            req,
            reply: Reply::Loaded(Ok(())),
        },
    )
    .unwrap();
    let reply = rt.block_on(pending).unwrap().unwrap();
    assert!(matches!(reply, Reply::Loaded(Ok(()))));
    assert!(
        events
            .chunks_emitted
            .load(std::sync::atomic::Ordering::SeqCst),
        "the chunk sent before the reply was applied by the time it resolved"
    );
}

/// Closing the worker side fails every pending request with the lost-worker error, within 1 s.
#[test]
fn a_closed_worker_fails_every_pending_request_within_a_second() {
    let peers: Peers = Arc::default();
    let workers = GuestWorkers::new(Launcher::Peer(Arc::clone(&peers)));
    let mut tools = peer_worker(&workers, &peers, WorkerKind::Tools);
    workers.ready().unwrap();
    let rt = runtime();
    let pending: Vec<_> = (0..3u64)
        .map(|n| {
            let workers = Arc::clone(&workers);
            let req = 9_100_000 + n;
            rt.spawn(async move {
                workers
                    .request_for_test(
                        WorkerKind::Tools,
                        req,
                        ToWorker::Load {
                            req,
                            guest: n,
                            name: "x".into(),
                            form: Vec::new(),
                        },
                    )
                    .await
            })
        })
        .collect();
    for _ in 0..3 {
        read_frame::<ToWorker>(&mut tools).unwrap();
    }
    let closed = std::time::Instant::now();
    drop(tools);
    for handle in pending {
        let result = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(1), handle).await })
            .expect("resolved within 1 s")
            .unwrap();
        let error = result.expect_err("a lost worker fails the request");
        assert!(
            error.contains("the tools guest worker") && error.contains("was lost"),
            "{error}"
        );
    }
    assert!(closed.elapsed() < Duration::from_secs(1));
    assert!(workers.lost().is_some(), "the session's loss latch is set");
}

/// Dropping a guest call's future tells its worker to drop the call.
#[test]
fn dropping_a_call_sends_cancel() {
    let peers: Peers = Arc::default();
    let workers = GuestWorkers::new(Launcher::Peer(Arc::clone(&peers)));
    let mut tools = peer_worker(&workers, &peers, WorkerKind::Tools);
    workers.ready().unwrap();
    let engine = crate::runtime::build_engine().unwrap();
    let component = crate::inference_import::test_support::driver_double(&engine, 0, "{}");
    let rt = runtime();
    let handle = {
        let workers = Arc::clone(&workers);
        rt.spawn(async move {
            let host = test_host();
            let deadlines = CallDeadlines::default();
            workers
                .run_tool(
                    WorkerKind::Tools,
                    &component,
                    spec("probe", Vec::new()),
                    host,
                    ToolInput {
                        data: None,
                        log_path: None,
                    },
                    &deadlines,
                )
                .await
        })
    };
    let load = read_frame::<ToWorker>(&mut tools).unwrap();
    let ToWorker::Load { req, .. } = load else {
        panic!("expected Load, got {load:?}");
    };
    write_frame(
        &mut tools,
        &FromWorker::Reply {
            req,
            reply: Reply::Loaded(Ok(())),
        },
    )
    .unwrap();
    let run = read_frame::<ToWorker>(&mut tools).unwrap();
    let ToWorker::RunTool { req: run_req, .. } = run else {
        panic!("expected RunTool, got {run:?}");
    };
    handle.abort();
    match read_frame::<ToWorker>(&mut tools).unwrap() {
        ToWorker::Cancel { req } => assert_eq!(req, run_req),
        other => panic!("expected Cancel, got {other:?}"),
    }
}

fn test_host() -> crate::runtime::ToolCallHost {
    crate::runtime::ToolCallHost {
        http_hooks: crate::runtime::NetworkPolicyHooks {
            network_allow_rules: Vec::new(),
            gateway: None,
            formation: None,
            task_provenance: None,
            member_requests: None,
        },
        events: crate::stream_events::StreamEventsTarget {
            sse: None,
            task: None,
            chunks_emitted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_calls: None,
        },
        input: crate::runtime::InputWiring::unavailable(),
        process: crate::session_process::ProcessAccess::Unavailable(
            crate::session_process::unavailable::NOT_AGENT_TOOL_CALL,
        ),
    }
}

/// A tool run through a real worker server (on a thread) returns what the guest returned.
#[test]
fn a_tool_runs_in_a_thread_worker() {
    let workers = GuestWorkers::new(Launcher::Thread);
    let engine = crate::runtime::build_engine().unwrap();
    let component = crate::inference_import::test_support::driver_double(
        &engine,
        0,
        r#"{"stop_reason":"end_turn"}"#,
    );
    let workdir = tempfile::TempDir::new().unwrap();
    let mut spec = spec("probe", Vec::new());
    spec.wasi.preopens[0].host_path = workdir.path().to_path_buf();
    let rt = runtime();
    let result = rt.block_on(workers.run_tool(
        WorkerKind::Tools,
        &component,
        spec,
        test_host(),
        ToolInput {
            data: Some("{}".into()),
            log_path: None,
        },
        &CallDeadlines::default(),
    ));
    let result = result.expect("the tool ran");
    assert_eq!(
        result.data.as_deref(),
        Some(r#"{"stop_reason":"end_turn"}"#)
    );
    assert_eq!(workers.started_kinds(), vec![WorkerKind::Tools]);
}

/// The value, its hex, both base64 alphabets and its sha256: every form a key could leak in.
fn leak_forms(secret: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    let hex: String = secret.bytes().map(|b| format!("{b:02x}")).collect();
    vec![
        secret.as_bytes().to_vec(),
        hex.into_bytes(),
        base64::engine::general_purpose::STANDARD
            .encode(secret)
            .into_bytes(),
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(secret)
            .into_bytes(),
        murmur_artifact::sha256_hex(secret.as_bytes()).into_bytes(),
    ]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// No supervisor-to-worker frame carries the gateway key, while the driver's environment does
/// carry the gateway and inference endpoints.
#[test]
fn no_credential() {
    use crate::credential_gateway::{CredentialGateway, GatewayMetering};
    const CANARY: &str = "sk-canary-guest-worker-7f3a9c";
    let gateway = Arc::new(
        CredentialGateway::new(
            "driver-probe",
            "http://127.0.0.1:9",
            murmur_artifact::UpstreamAuth {
                header: "Authorization".to_string(),
                value: "Bearer {key}".to_string(),
            },
            Some(Arc::new(
                crate::gateway_credential::GatewayCredential::resolve(
                    "driver-probe",
                    &murmur_artifact::ApiKeyReference::Literal(CANARY.to_string()),
                    None,
                )
                .unwrap(),
            )),
            GatewayMetering::Unmetered,
        )
        .unwrap(),
    );
    let tap = Arc::new(Mutex::new(Vec::new()));
    let workers = GuestWorkers::new_tapped(Launcher::Thread, Arc::clone(&tap));
    let engine = crate::runtime::build_engine().unwrap();
    let component = crate::inference_import::test_support::driver_double(&engine, 0, "{}");
    let workdir = tempfile::TempDir::new().unwrap();
    let inference_env = vec![(
        "MURMUR_INFERENCE_ENDPOINT".to_string(),
        gateway.guest_endpoint(),
    )];
    let policy = crate::types::CapabilityPolicy::default();
    let member_requests = crate::member_request::MemberRequests::default();
    let deadlines = CallDeadlines::default();
    let (spec, host) = crate::runtime::prepare_tool_call(
        crate::runtime::ToolInvokeEnv {
            engine: &engine,
            accessible_workdir: workdir.path(),
            runtime_dir: None,
            extra_roots: &[],
            inference_env: &inference_env,
            capability_policy: &policy,
            network_allow_rules: &[],
            artifact_grant: None,
            gateway: Some(&gateway),
            formation: None,
            task_provenance: None,
            role: ComponentRole::Driver,
            call_deadlines: &deadlines,
            member_requests: &member_requests,
            task_id: None,
            process: crate::session_process::ProcessAccess::Unavailable(
                crate::session_process::unavailable::DRIVER,
            ),
        },
        crate::runtime::ToolA2aWiring::silent(),
        "driver-probe",
    )
    .unwrap();
    let names: Vec<_> = spec
        .wasi
        .env
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(names.contains(&"MURMUR_INFERENCE_ENDPOINT"), "{names:?}");
    assert!(names.contains(&"MURMUR_GATEWAY_ENDPOINT"), "{names:?}");
    let rt = runtime();
    rt.block_on(workers.run_tool(
        WorkerKind::Driver,
        &component,
        spec,
        host,
        ToolInput {
            data: Some("{}".into()),
            log_path: None,
        },
        &deadlines,
    ))
    .expect("the driver ran in its worker");
    let frames = tap.lock().unwrap();
    assert!(frames.len() >= 2, "Load and RunTool were tapped");
    let run_frame = frames
        .iter()
        .find(|frame| contains(frame, b"MURMUR_GATEWAY_ENDPOINT"))
        .expect("the RunTool frame carries the environment");
    assert!(contains(run_frame, b"MURMUR_INFERENCE_ENDPOINT"));
    for frame in frames.iter() {
        for form in leak_forms(CANARY) {
            assert!(
                !contains(frame, &form),
                "a supervisor-to-worker frame carries a form of the gateway key"
            );
        }
    }
}

/// A launcher naming a missing program, and a worker answering with the wrong protocol version,
/// each refuse with `E-RUN-066` naming the kind and the reason.
#[test]
fn refuses() {
    let workers = GuestWorkers::new(Launcher::Process {
        program: "/nonexistent/murmur-guest-worker".into(),
    });
    workers.start(WorkerKind::Tools);
    let error = workers.ready().expect_err("a missing program cannot start");
    assert!(
        matches!(error, RuntimeError::GuestWorkerStart(_)),
        "{error:?}"
    );
    let text = error.to_string();
    assert!(
        text.starts_with("the tools guest worker could not be started:")
            && text.contains("/nonexistent/murmur-guest-worker"),
        "{text}"
    );

    fn wrong_protocol(_kind: WorkerKind, mut stream: UnixStream) {
        let _ = write_frame(
            &mut stream,
            &FromWorker::Hello {
                protocol: PROTOCOL_VERSION + 98,
                version: "9.9.9".into(),
                pid: 1,
            },
        );
        let _ = read_frame::<ToWorker>(&mut stream);
    }
    let workers = GuestWorkers::new(Launcher::ThreadServing(wrong_protocol));
    workers.start(WorkerKind::Driver);
    let error = workers.ready().expect_err("a wrong protocol is refused");
    let text = error.to_string();
    assert!(
        text.starts_with("the driver guest worker could not be started:")
            && text.contains(&format!("protocol {}", PROTOCOL_VERSION + 98)),
        "{text}"
    );
}

/// Each store creation site refuses under supervisor placement and is allowed in a worker or a
/// process that registered no worker processes.
#[test]
fn guard() {
    let sites = [StoreSite::Tool, StoreSite::Driver];
    for site in sites {
        let refused =
            store_allowed(Placement::Supervisor, site).expect_err("no store in the supervisor");
        assert!(
            refused
                .to_string()
                .contains("guests run only in guest workers"),
            "{refused}"
        );
        assert!(store_allowed(Placement::Worker, site).is_ok());
        assert!(store_allowed(Placement::Unsplit, site).is_ok());
    }
}

#[test]
fn the_entry_is_recognised_only_as_the_first_argument() {
    let args = |list: &[&str]| -> Vec<std::ffi::OsString> {
        list.iter().map(std::ffi::OsString::from).collect()
    };
    assert_eq!(
        entry_kind(args(&["mur", ENTRY, "tools"])),
        Some(WorkerEntry::Kind(WorkerKind::Tools))
    );
    assert_eq!(
        entry_kind(args(&["/usr/bin/mur", ENTRY, "hooks"])),
        Some(WorkerEntry::Kind(WorkerKind::Hooks))
    );
    assert_eq!(entry_kind(args(&["mur", ENTRY])), Some(WorkerEntry::Misuse));
    assert_eq!(
        entry_kind(args(&["mur", ENTRY, "tools", "x"])),
        Some(WorkerEntry::Misuse)
    );
    assert_eq!(
        entry_kind(args(&["mur", ENTRY, "shell"])),
        Some(WorkerEntry::Misuse)
    );
    assert_eq!(entry_kind(args(&["mur", "run", ENTRY])), None);
    assert_eq!(entry_kind(args(&["mur"])), None);
}

#[test]
fn the_lost_worker_message_names_kind_pid_call_and_exit() {
    let message = supervisor::lost_message(
        WorkerKind::Hooks,
        Some(77),
        Some("hook 'sleep-hook'"),
        "was killed by signal 9 (SIGKILL)",
    );
    assert_eq!(
        message,
        "the hooks guest worker (pid 77) was lost while running hook 'sleep-hook': it was killed \
         by signal 9 (SIGKILL); the task failed and the session ended"
    );
}
