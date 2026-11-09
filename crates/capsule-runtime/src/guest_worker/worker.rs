//! The worker side: holds the engine, the compiled guests and every store, and answers the
//! supervisor's requests. Every host import that reaches past WASI, clocks, random and the
//! execution limits becomes a host call to the supervisor through a [`RemoteLink`].

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, oneshot};
use wasmtime::component::Component;

use super::wire::{
    read_frame, write_frame, FromWorker, HostReply, HostRequest, HttpError, HttpRequestHead,
    HttpResponseHead, Reply, StreamEvent, ToWorker,
};
use super::{WorkerKind, PROTOCOL_VERSION};
use crate::limits::CallDeadlines;
use crate::runtime::{execute_tool_call, ToolHostSide};
use crate::session_process::{ProcessCall, ProcessReply};

type BodySender = mpsc::UnboundedSender<Result<Bytes, HttpError>>;

/// The worker's connection: the writer every task shares, and what is waiting on the supervisor.
pub(crate) struct WorkerShared {
    writer: Mutex<UnixStream>,
    next_host: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<HostReply>>>,
    bodies: Mutex<HashMap<u64, BodySender>>,
}

impl WorkerShared {
    fn send(&self, message: &FromWorker) {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A failed write means the supervisor is gone; the reader sees the same and ends the
        // worker.
        let _ = write_frame(&mut *writer, message);
    }
}

/// One guest call's way to the supervisor: every host call it makes names the call id the
/// supervisor issued, and is answered only while that call is in flight.
pub struct RemoteLink {
    call: u64,
    shared: Arc<WorkerShared>,
}

impl RemoteLink {
    async fn host(&self, request: HostRequest) -> HostReply {
        let host = self.shared.next_host.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, tx);
        self.shared.send(&FromWorker::HostCall {
            host,
            call: self.call,
            request,
        });
        rx.await.unwrap_or_else(|_| {
            HostReply::Refused("the supervisor's connection closed".to_string())
        })
    }

    pub(crate) fn stream_event(&self, event: StreamEvent) {
        self.shared.send(&FromWorker::Event {
            call: self.call,
            event,
        });
    }

    pub(crate) fn run_started(&self, deadline: Duration) {
        self.shared.send(&FromWorker::RunStarted {
            call: self.call,
            deadline,
        });
    }

    pub(crate) async fn request_input(&self, prompt: String) -> wasmtime::Result<String> {
        match self.host(HostRequest::RequestInput { prompt }).await {
            HostReply::Input(Ok(answer)) => Ok(answer),
            HostReply::Input(Err(error)) | HostReply::Refused(error) => {
                Err(wasmtime::Error::msg(error))
            }
            other => Err(wasmtime::Error::msg(format!("unexpected reply {other:?}"))),
        }
    }

    pub(crate) async fn process(&self, call: ProcessCall) -> ProcessReply {
        match self.host(HostRequest::Process(call)).await {
            HostReply::Process(reply) => reply,
            HostReply::Refused(reason) => ProcessReply::Unanswered(reason),
            other => ProcessReply::Unanswered(format!("unexpected reply {other:?}")),
        }
    }

    /// Sends `head` and returns the response head and a receiver for its body.
    pub(crate) async fn http(
        &self,
        head: HttpRequestHead,
    ) -> Result<
        (
            HttpResponseHead,
            mpsc::UnboundedReceiver<Result<Bytes, HttpError>>,
        ),
        HttpError,
    > {
        let host = self.shared.next_host.fetch_add(1, Ordering::Relaxed);
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        // Registered before the request is sent, so no body frame can arrive unrouted.
        self.shared
            .bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, body_tx);
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, tx);
        self.shared.send(&FromWorker::HostCall {
            host,
            call: self.call,
            request: HostRequest::Http(head),
        });
        let reply = rx.await.unwrap_or_else(|_| {
            HostReply::Refused("the supervisor's connection closed".to_string())
        });
        let forget = || {
            self.shared
                .bodies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&host);
        };
        match reply {
            HostReply::Http(Ok(head)) => Ok((head, body_rx)),
            HostReply::Http(Err(error)) => {
                forget();
                Err(error)
            }
            HostReply::Refused(reason) => {
                forget();
                Err(HttpError::InternalError(Some(reason)))
            }
            other => {
                forget();
                Err(HttpError::InternalError(Some(format!(
                    "unexpected reply {other:?}"
                ))))
            }
        }
    }
}

/// Serves the supervisor on `stream` until `Shutdown` or end of file, as a worker of `kind`.
/// Returns the process exit code a worker process ends with.
#[allow(unsafe_code)]
pub(crate) fn serve(kind: WorkerKind, stream: UnixStream) -> i32 {
    let Ok(writer) = stream.try_clone() else {
        return 2;
    };
    let shared = Arc::new(WorkerShared {
        writer: Mutex::new(writer),
        next_host: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        bodies: Mutex::new(HashMap::new()),
    });
    shared.send(&FromWorker::Hello {
        protocol: PROTOCOL_VERSION,
        version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
    });
    let mut reader = stream;
    match read_frame::<ToWorker>(&mut reader) {
        Ok(ToWorker::Init { kind: told }) if told == kind => {}
        _ => return 2,
    }
    let Ok(engine) = crate::runtime::build_engine() else {
        return 2;
    };
    let _ticker = crate::limits::EpochTicker::spawn(&engine);
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(kind.runtime_threads())
        .thread_name(format!("guest-worker-{}", kind.as_str()))
        .enable_all()
        .build()
    else {
        return 2;
    };
    let guests: Arc<Mutex<HashMap<u64, Component>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<u64>();

    while let Ok(message) = read_frame::<ToWorker>(&mut reader) {
        while let Ok(done) = done_rx.try_recv() {
            running.remove(&done);
        }
        match message {
            ToWorker::Init { .. } => {}
            ToWorker::Load {
                req,
                guest,
                name,
                form,
            } => {
                // SAFETY: `form` is a component this same binary serialized with an engine built by
                // the same `build_engine`, handed over a connection only the supervisor holds.
                let loaded = unsafe { Component::deserialize(&engine, &form) }.map_err(|error| {
                    format!("failed to load '{name}' in the guest worker: {error}")
                });
                let reply = match loaded {
                    Ok(component) => {
                        guests
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(guest, component);
                        Ok(())
                    }
                    Err(error) => Err(error),
                };
                shared.send(&FromWorker::Reply {
                    req,
                    reply: Reply::Loaded(reply),
                });
            }
            ToWorker::RunTool {
                req,
                guest,
                spec,
                input,
            } => {
                let component = guests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&guest)
                    .cloned();
                let shared_task = Arc::clone(&shared);
                let engine = engine.clone();
                let done = done_tx.clone();
                let task = runtime.spawn(async move {
                    let deadlines = CallDeadlines::default();
                    let result = match component {
                        None => Err(format!(
                            "tool '{}' is not loaded in the guest worker",
                            spec.name
                        )),
                        Some(component) => {
                            let link = Arc::new(RemoteLink {
                                call: req,
                                shared: Arc::clone(&shared_task),
                            });
                            Box::pin(execute_tool_call(
                                &engine,
                                &spec,
                                ToolHostSide::Remote(link),
                                &deadlines,
                                &component,
                                input,
                            ))
                            .await
                        }
                    };
                    shared_task.send(&FromWorker::Reply {
                        req,
                        reply: Reply::Tool {
                            result,
                            deadlines: deadlines.take(),
                        },
                    });
                    let _ = done.send(req);
                });
                running.insert(req, task.abort_handle());
            }
            ToWorker::Cancel { req } => {
                if let Some(task) = running.remove(&req) {
                    task.abort();
                }
            }
            ToWorker::HostReply { host, reply } => {
                let waiter = shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&host);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(reply);
                }
            }
            ToWorker::BodyChunk { stream, bytes } => {
                let bodies = shared
                    .bodies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(body) = bodies.get(&stream) {
                    let _ = body.send(Ok(Bytes::from(bytes)));
                }
            }
            ToWorker::BodyEnd { stream, error } => {
                let body = shared
                    .bodies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&stream);
                if let (Some(body), Some(error)) = (body, error) {
                    let _ = body.send(Err(error));
                }
            }
            ToWorker::Shutdown => break,
        }
    }
    for (_, task) in running {
        task.abort();
    }
    runtime.shutdown_background();
    0
}
