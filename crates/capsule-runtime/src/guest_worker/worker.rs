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
    HttpResponseHead, Reply, StreamEvent, TaskIoCall, TaskIoReply, ToWorker,
};
use super::{WorkerKind, PROTOCOL_VERSION};
use crate::bindings::hook::murmur::conversation::read::MessagePage;
use crate::bindings::hook::murmur::runtime::inference::{InferenceRequest, InferenceResponse};
use crate::bindings::hook::murmur::task_io::read::IoError;
use crate::inference_import::HookInferencePurpose;
use crate::limits::CallDeadlines;
use crate::process_driver::{DriverAnswer, DriverError, ProcessDriver};
use crate::runtime::{execute_tool_call, CapsuleCall, CapsuleReply, ToolHostSide};
use crate::session_process::{ProcessCall, ProcessReply};

type BodySender = mpsc::UnboundedSender<Result<Bytes, HttpError>>;

/// The worker's connection: the writer every task shares, and what is waiting on the supervisor.
/// Every pending host call and response body is keyed by its host id and remembers the call it
/// belongs to, so a call that ends or is cancelled leaves nothing behind.
pub(crate) struct WorkerShared {
    writer: Mutex<UnixStream>,
    next_host: AtomicU64,
    pending: Mutex<HashMap<u64, (u64, oneshot::Sender<HostReply>)>>,
    bodies: Mutex<HashMap<u64, (u64, BodySender)>>,
}

impl WorkerShared {
    /// Forgets every host call and response body of call `call`.
    fn forget_call(&self, call: u64) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, (owner, _)| *owner != call);
        self.bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, (owner, _)| *owner != call);
    }

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

/// One guest store's way to the supervisor: every host call it makes names the call id the
/// supervisor issued for the call running on it, and is answered only while that call is in
/// flight. A session-long hook instance's link is pointed at each new call before it runs.
pub struct RemoteLink {
    call: AtomicU64,
    shared: Arc<WorkerShared>,
}

impl RemoteLink {
    fn new(call: u64, shared: &Arc<WorkerShared>) -> Arc<Self> {
        Arc::new(Self {
            call: AtomicU64::new(call),
            shared: Arc::clone(shared),
        })
    }

    fn call(&self) -> u64 {
        self.call.load(Ordering::SeqCst)
    }

    fn set_call(&self, call: u64) {
        self.call.store(call, Ordering::SeqCst);
    }

    async fn host(&self, request: HostRequest) -> HostReply {
        let host = self.shared.next_host.fetch_add(1, Ordering::Relaxed);
        let call = self.call();
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, (call, tx));
        self.shared.send(&FromWorker::HostCall {
            host,
            call,
            request,
        });
        rx.await.unwrap_or_else(|_| {
            HostReply::Refused("the supervisor's connection closed".to_string())
        })
    }

    pub(crate) fn stream_event(&self, event: StreamEvent) {
        self.shared.send(&FromWorker::Event {
            call: self.call(),
            event,
        });
    }

    pub(crate) fn run_started(&self, deadline: Duration) {
        self.shared.send(&FromWorker::RunStarted {
            call: self.call(),
            deadline,
        });
    }

    pub(crate) async fn inference(
        &self,
        purpose: HookInferencePurpose,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, String> {
        match self.host(HostRequest::Inference { purpose, request }).await {
            HostReply::Inference(result) => result,
            HostReply::Refused(reason) => Err(reason),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }

    pub(crate) async fn task_io(&self, call: TaskIoCall) -> TaskIoReply {
        let reads_len = matches!(call, TaskIoCall::InputLen(_) | TaskIoCall::OutputLen);
        match self.host(HostRequest::TaskIo(call)).await {
            HostReply::TaskIo(reply) => reply,
            // A refused read is the same `not-granted` value an ungranted hook branches on.
            _ if reads_len => TaskIoReply::Len(Err(IoError::NotGranted)),
            _ => TaskIoReply::Text(Err(IoError::NotGranted)),
        }
    }

    pub(crate) async fn conversation(
        &self,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<MessagePage, String> {
        match self.host(HostRequest::Conversation { cursor, limit }).await {
            HostReply::Conversation(result) => result,
            HostReply::Refused(reason) => Err(reason),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }

    pub(crate) async fn capsule(&self, call: CapsuleCall) -> CapsuleReply {
        match self.host(HostRequest::Capsule(Box::new(call))).await {
            HostReply::Capsule(reply) => *reply,
            HostReply::Refused(reason) => CapsuleReply::Unanswered(reason),
            other => CapsuleReply::Unanswered(format!("unexpected reply {other:?}")),
        }
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
        let call = self.call();
        let (body_tx, body_rx) = mpsc::unbounded_channel();
        // Registered before the request is sent, so no body frame can arrive unrouted.
        self.shared
            .bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, (call, body_tx));
        let (tx, rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(host, (call, tx));
        self.shared.send(&FromWorker::HostCall {
            host,
            call,
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
    let state = Arc::new(ServerState {
        engine,
        shared: Arc::clone(&shared),
        guests: Mutex::new(HashMap::new()),
        hooks: Mutex::new(HashMap::new()),
        drivers: Mutex::new(HashMap::new()),
    });
    let mut running: HashMap<u64, tokio::task::AbortHandle> = HashMap::new();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<u64>();

    while let Ok(message) = read_frame::<ToWorker>(&mut reader) {
        while let Ok(done) = done_rx.try_recv() {
            running.remove(&done);
        }
        let spawn_call =
            |req: u64, body: std::pin::Pin<Box<dyn std::future::Future<Output = Reply> + Send>>| {
                let shared = Arc::clone(&shared);
                let done = done_tx.clone();
                runtime
                    .spawn(async move {
                        let reply = body.await;
                        shared.forget_call(req);
                        shared.send(&FromWorker::Reply { req, reply });
                        let _ = done.send(req);
                    })
                    .abort_handle()
            };
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
                let loaded =
                    unsafe { Component::deserialize(&state.engine, &form) }.map_err(|error| {
                        format!("failed to load '{name}' in the guest worker: {error}")
                    });
                let reply = match loaded {
                    Ok(component) => {
                        state
                            .guests
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
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let deadlines = CallDeadlines::default();
                        let result = match state.guest(guest) {
                            None => Err(format!(
                                "tool '{}' is not loaded in the guest worker",
                                spec.name
                            )),
                            Some(component) => {
                                let link = RemoteLink::new(req, &state.shared);
                                Box::pin(execute_tool_call(
                                    &state.engine,
                                    &spec,
                                    ToolHostSide::Remote(link),
                                    &deadlines,
                                    &component,
                                    input,
                                ))
                                .await
                            }
                        };
                        #[cfg(all(feature = "escape-probe", target_os = "linux"))]
                        let result = result.map(|mut result| {
                            super::escape_probe::answer(&spec, &mut result);
                            result
                        });
                        Reply::Tool {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::HookOpen {
                req,
                guest,
                instance,
                spec,
            } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let deadlines = CallDeadlines::default();
                        let result = match state.guest(guest) {
                            None => Err(format!(
                                "hook '{}' is not loaded in the guest worker",
                                spec.name
                            )),
                            Some(component) => {
                                let link = RemoteLink::new(req, &state.shared);
                                crate::hooks::open_remote_instance(
                                    &state.engine,
                                    &component,
                                    &spec,
                                    Arc::clone(&link),
                                    deadlines.clone(),
                                )
                                .await
                                .map(|hook| {
                                    let fns = hook.exported_fns();
                                    state
                                        .hooks
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .insert(
                                            instance,
                                            Arc::new(tokio::sync::Mutex::new(WorkerHook {
                                                hook,
                                                link,
                                                deadlines: deadlines.clone(),
                                                in_call: None,
                                            })),
                                        );
                                    fns
                                })
                            }
                        };
                        Reply::HookOpened {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::HookCall {
                req,
                instance,
                fn_name,
                arg,
                purpose,
            } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let hook = state
                            .hooks
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&instance)
                            .cloned();
                        let Some(hook) = hook else {
                            return Reply::Hook {
                                result: Err(format!(
                                    "hook instance {instance} is not held by the guest worker"
                                )),
                                deadlines: Vec::new(),
                            };
                        };
                        let mut hook = hook.lock().await;
                        hook.link.set_call(req);
                        let WorkerHook {
                            hook: instance,
                            deadlines,
                            in_call,
                            ..
                        } = &mut *hook;
                        // A call dropped mid-guest by `Cancel` left the instance where its future
                        // stopped: it is cut, as an instance the wall-clock bound stopped is.
                        if let Some(stopped) = in_call.take() {
                            crate::hooks::mark_cut(instance, &stopped);
                        }
                        *in_call = Some(fn_name.clone());
                        let result =
                            crate::hooks::call_instance(instance, &fn_name, *arg, purpose).await;
                        *in_call = None;
                        Reply::Hook {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::HookStage {
                req,
                guest,
                spec,
                event,
            } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let deadlines = CallDeadlines::default();
                        let result = match state.guest(guest) {
                            None => Err(format!(
                                "hook '{}' is not loaded in the guest worker",
                                spec.name
                            )),
                            Some(component) => {
                                let link = RemoteLink::new(req, &state.shared);
                                crate::hooks::stage_remote_once(
                                    &state.engine,
                                    &component,
                                    &spec,
                                    link,
                                    deadlines.clone(),
                                    event,
                                )
                                .await
                                .map(Some)
                            }
                        };
                        Reply::Hook {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::HookClose { instance } => {
                state
                    .hooks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&instance);
            }
            ToWorker::DriverOpen {
                req,
                guest,
                instance,
                name,
                version,
            } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let deadlines = CallDeadlines::default();
                        let result = match state.guest(guest) {
                            None => Err(DriverError::Other(format!(
                                "process driver '{name}' is not loaded in the guest worker"
                            ))),
                            Some(component) => ProcessDriver::instantiate(
                                &state.engine,
                                &component,
                                &name,
                                &version,
                                deadlines.clone(),
                            )
                            .await
                            .map(|driver| {
                                state
                                    .drivers
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(
                                        instance,
                                        Arc::new(tokio::sync::Mutex::new((
                                            driver,
                                            deadlines.clone(),
                                        ))),
                                    );
                                DriverAnswer::Opened
                            })
                            .map_err(DriverError::from_runtime),
                        };
                        Reply::Driver {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::DriverCall {
                req,
                instance,
                call,
            } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let driver = state
                            .drivers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get(&instance)
                            .cloned();
                        let Some(driver) = driver else {
                            return Reply::Driver {
                                result: Err(DriverError::Other(format!(
                                    "process driver instance {instance} is not held by the guest worker"
                                ))),
                                deadlines: Vec::new(),
                            };
                        };
                        let mut driver = driver.lock().await;
                        let (driver, deadlines) = &mut *driver;
                        let result = driver.call(*call).await.map_err(DriverError::from_runtime);
                        Reply::Driver {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::DriverClose { instance } => {
                state
                    .drivers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&instance);
            }
            ToWorker::RunCapsule { req, guest, spec } => {
                let state = Arc::clone(&state);
                let task = spawn_call(
                    req,
                    Box::pin(async move {
                        let deadlines = CallDeadlines::default();
                        let result = match state.guest(guest) {
                            None => Err(crate::runtime::CapsuleRunError::Runtime(
                                "the capsule component is not loaded in the guest worker"
                                    .to_string(),
                            )),
                            Some(component) => {
                                let link = RemoteLink::new(req, &state.shared);
                                crate::runtime::run_capsule_guest(
                                    &state.engine,
                                    &component,
                                    &spec,
                                    link,
                                    &deadlines,
                                )
                                .await
                            }
                        };
                        Reply::Capsule {
                            result,
                            deadlines: deadlines.take(),
                        }
                    }),
                );
                running.insert(req, task);
            }
            ToWorker::Cancel { req } => {
                if let Some(task) = running.remove(&req) {
                    task.abort();
                }
                shared.forget_call(req);
            }
            ToWorker::HostReply { host, reply } => {
                let waiter = shared
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&host);
                if let Some((_, waiter)) = waiter {
                    let _ = waiter.send(reply);
                }
            }
            ToWorker::BodyChunk { stream, bytes } => {
                let bodies = shared
                    .bodies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some((_, body)) = bodies.get(&stream) {
                    let _ = body.send(Ok(Bytes::from(bytes)));
                }
            }
            ToWorker::BodyEnd { stream, error } => {
                let body = shared
                    .bodies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&stream);
                if let (Some((_, body)), Some(error)) = (body, error) {
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

/// A link on a connection no supervisor reads, for tests that need a store's host side but never
/// reach it. The other end is returned so the connection stays open.
#[cfg(test)]
pub(crate) fn detached_link() -> (Arc<RemoteLink>, UnixStream) {
    let (ours, theirs) = UnixStream::pair().expect("a socket pair");
    let shared = Arc::new(WorkerShared {
        writer: Mutex::new(ours),
        next_host: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        bodies: Mutex::new(HashMap::new()),
    });
    (RemoteLink::new(0, &shared), theirs)
}

/// A session-long hook instance in the hooks worker, with the link its host imports use and the
/// latch its wall-clock bounds record into, drained after each call.
struct WorkerHook {
    hook: crate::hooks::HookInstance,
    link: Arc<RemoteLink>,
    deadlines: CallDeadlines,
    /// The lifecycle function a call is running, while one is.
    in_call: Option<String>,
}

/// What the worker holds: its engine, the guests handed to it, and the session-long instances.
struct ServerState {
    engine: wasmtime::Engine,
    shared: Arc<WorkerShared>,
    guests: Mutex<HashMap<u64, Component>>,
    hooks: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<WorkerHook>>>>,
    drivers: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<WorkerDriver>>>>,
}

/// A process driver instance in the driver worker, with the latch its wall-clock bounds record
/// into, drained after each call.
type WorkerDriver = (ProcessDriver, CallDeadlines);

impl ServerState {
    fn guest(&self, guest: u64) -> Option<Component> {
        self.guests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&guest)
            .cloned()
    }
}
