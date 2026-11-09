//! The supervisor side: starts each worker at most once, sends it guests and calls, and answers
//! its host calls with the objects it registered for the call in flight.

use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use tokio::sync::oneshot;
use wasmtime::component::Component;
use wasmtime_wasi_http::WasiHttpHooks;

use super::http::{head_of, request_from_head, to_wire};
use super::wire::{
    read_frame, write_frame, FromWorker, HostReply, HostRequest, Reply, StreamEvent, ToWorker,
};
use super::{Launcher, WorkerKind, PROTOCOL_VERSION};
use crate::bindings::host::murmur::tool::run::{ToolInput, ToolResult};
use crate::errors::RuntimeError;
use crate::limits::CallDeadlines;
use crate::runtime::{NetworkPolicyHooks, ToolCallHost, ToolCallSpec};
use crate::session_process::RemoteProcessHost;
use crate::stream_events::StreamEventsTarget;

/// How long past a call's own bound the supervisor waits for a worker to answer before killing it
/// and treating it as lost.
pub(crate) const UNRESPONSIVE_GRACE: Duration = Duration::from_secs(5);

/// How long a worker has to say hello once started.
const HANDSHAKE_WAIT: Duration = Duration::from_secs(60);

/// Request, call and guest ids: issued from one counter, so an id from one connection is never in
/// flight on another.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Where a worker's start stands.
#[derive(Debug, Clone)]
enum Start {
    Starting,
    /// Forked and exec'd (`child`), or running on a thread (`None`); not yet past the handshake.
    Spawned {
        child: Option<u32>,
    },
    Ready {
        pid: u32,
    },
    Failed(String),
}

/// What the supervisor holds for one guest call in flight: the objects its host imports reach.
struct CallEntry {
    description: String,
    http: Mutex<NetworkPolicyHooks>,
    events: StreamEventsTarget,
    input: crate::runtime::InputWiring,
    process: tokio::sync::Mutex<RemoteProcessHost>,
    /// The response bodies this call is streaming, stopped when the call ends.
    bodies: Mutex<Vec<tokio::task::AbortHandle>>,
}

impl Drop for CallEntry {
    fn drop(&mut self) {
        for body in self
            .bodies
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            body.abort();
        }
    }
}

/// One worker's connection, seen from the supervisor.
pub(crate) struct WorkerConn {
    kind: WorkerKind,
    start: Mutex<Start>,
    started: Condvar,
    writer: Mutex<Option<UnixStream>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Reply, String>>>>,
    calls: Mutex<HashMap<u64, Arc<CallEntry>>>,
    /// The guest being handed over to the worker, named as the loss message names a call.
    loading: Mutex<Option<String>>,
    /// Set once the connection is lost: every later request fails with it.
    lost: Mutex<Option<String>>,
    /// The session-wide loss latch this connection reports into.
    session_lost: Arc<Mutex<Option<String>>>,
    host_runtime: tokio::runtime::Handle,
    /// A frame tap for tests: every supervisor-to-worker frame, encoded.
    #[cfg(test)]
    tap: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
}

impl WorkerConn {
    fn send(&self, message: &ToWorker) -> Result<(), String> {
        #[cfg(test)]
        if let Some(tap) = &self.tap {
            if let Ok(frame) = super::wire::encode(message) {
                tap.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(frame);
            }
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match writer.as_mut() {
            Some(writer) => write_frame(writer, message),
            None => Err("the guest worker is not connected".to_string()),
        }
    }

    /// The worker's pid once started, waiting for the start to finish. A thread worker has the
    /// supervisor's own pid.
    fn wait_started(&self, wait: Duration) -> Result<u32, String> {
        let deadline = Instant::now() + wait;
        let mut start = self
            .start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*start {
                Start::Ready { pid } => return Ok(*pid),
                Start::Failed(reason) => return Err(reason.clone()),
                Start::Starting | Start::Spawned { .. } => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("it did not say hello within {}s", wait.as_secs()));
            }
            start = self
                .started
                .wait_timeout(start, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    async fn wait_started_async(self: &Arc<Self>) -> Result<u32, String> {
        let conn = Arc::clone(self);
        match tokio::runtime::Handle::try_current() {
            Ok(_) => loop {
                {
                    let start = conn
                        .start
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match &*start {
                        Start::Ready { pid } => return Ok(*pid),
                        Start::Failed(reason) => return Err(reason.clone()),
                        Start::Starting | Start::Spawned { .. } => {}
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            },
            Err(_) => conn.wait_started(HANDSHAKE_WAIT),
        }
    }

    /// The worker process's pid once forked, without waiting for its handshake; `None` for a
    /// thread worker or a start that failed.
    fn wait_spawned(&self) -> Option<u32> {
        let mut start = self
            .start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*start {
                Start::Starting => {}
                Start::Spawned { child } => return *child,
                Start::Ready { pid } => return Some(*pid).filter(|pid| *pid != std::process::id()),
                Start::Failed(_) => return None,
            }
            start = self
                .started
                .wait_timeout(start, Duration::from_secs(5))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn pid(&self) -> Option<u32> {
        match &*self
            .start
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            Start::Ready { pid } => Some(*pid),
            _ => None,
        }
    }

    async fn request(&self, req: u64, message: ToWorker) -> Result<Reply, String> {
        let (tx, rx) = oneshot::channel();
        {
            if let Some(lost) = self
                .lost
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return Err(lost);
            }
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(req, tx);
        }
        if let Err(error) = self.send(&message) {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&req);
            return Err(self.lost_or(error));
        }
        rx.await
            .unwrap_or_else(|_| Err(self.lost_or("the guest worker went away".to_string())))
    }

    fn lost_or(&self, fallback: String) -> String {
        self.lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or(fallback)
    }

    /// Kills the worker process, when it is one. The reader then sees end of file and fails every
    /// pending request.
    #[allow(unsafe_code)]
    fn kill(&self) {
        if let Some(pid) = self.pid().filter(|pid| *pid != std::process::id()) {
            // SAFETY: plain syscall with integer arguments on a child this connection started.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        if let Some(writer) = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }

    fn call_description(&self) -> Option<String> {
        let call = self
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .next()
            .map(|entry| entry.description.clone());
        call.or_else(|| {
            self.loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
    }

    /// Marks the connection lost with `message`, fails every pending request with it and reports it
    /// into the session latch.
    fn mark_lost(&self, message: String) {
        *self
            .lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
        {
            let mut session = self
                .session_lost
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if session.is_none() {
                *session = Some(message.clone());
            }
        }
        let pending: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .collect();
        for (_, waiter) in pending {
            let _ = waiter.send(Err(message.clone()));
        }
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn reply_host(&self, host: u64, reply: HostReply) {
        let _ = self.send(&ToWorker::HostReply { host, reply });
    }

    /// Handles one frame from the worker. Runs on the connection's reader thread, so events are
    /// applied in arrival order, before any reply that arrived after them.
    fn dispatch(self: &Arc<Self>, message: FromWorker) {
        match message {
            FromWorker::Hello { .. } => {}
            FromWorker::Reply { req, reply } => {
                let waiter = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&req);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(Ok(reply));
                }
            }
            FromWorker::Event { call, event } => {
                let entry = self.call(call);
                if let Some(entry) = entry {
                    apply_event(&entry.events, event);
                }
            }
            FromWorker::RunStarted { call, deadline } => {
                if let Some(entry) = self.call(call) {
                    let started = match entry.process.try_lock() {
                        Ok(mut process) => {
                            process.begin_call(deadline);
                            true
                        }
                        Err(_) => false,
                    };
                    if !started {
                        self.host_runtime.spawn(async move {
                            entry.process.lock().await.begin_call(deadline);
                        });
                    }
                }
            }
            FromWorker::HostCall {
                host,
                call,
                request,
            } => {
                let Some(entry) = self.call(call) else {
                    self.reply_host(
                        host,
                        HostReply::Refused(format!(
                            "call {call} is not in flight on this guest worker's connection"
                        )),
                    );
                    return;
                };
                let conn = Arc::clone(self);
                self.host_runtime.spawn(async move {
                    conn.answer(host, entry, request).await;
                });
            }
        }
    }

    fn call(&self, call: u64) -> Option<Arc<CallEntry>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&call)
            .cloned()
    }

    async fn answer(self: Arc<Self>, host: u64, entry: Arc<CallEntry>, request: HostRequest) {
        match request {
            HostRequest::RequestInput { prompt } => {
                let answer = entry
                    .input
                    .request(prompt)
                    .await
                    .map_err(|error| error.to_string());
                self.reply_host(host, HostReply::Input(answer));
            }
            HostRequest::Process(call) => {
                let reply = entry.process.lock().await.answer(call).await;
                self.reply_host(host, HostReply::Process(reply));
            }
            HostRequest::Http(head) => {
                let sent = match request_from_head(head) {
                    Err(error) => Err(error),
                    Ok((request, options)) => {
                        let future = entry
                            .http
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .send_request(request, options, Box::new(std::future::pending()));
                        Box::into_pin(future).await
                    }
                };
                match sent {
                    Err(error) => self.reply_host(host, HostReply::Http(Err(to_wire(&error)))),
                    Ok((response, io)) => {
                        tokio::spawn(Box::into_pin(io));
                        self.reply_host(host, HostReply::Http(Ok(head_of(&response))));
                        let conn = Arc::clone(&self);
                        let body = tokio::spawn(async move {
                            let mut body = response.into_body();
                            loop {
                                match body.frame().await {
                                    None => {
                                        let _ = conn.send(&ToWorker::BodyEnd {
                                            stream: host,
                                            error: None,
                                        });
                                        break;
                                    }
                                    Some(Err(error)) => {
                                        let _ = conn.send(&ToWorker::BodyEnd {
                                            stream: host,
                                            error: Some(to_wire(&error)),
                                        });
                                        break;
                                    }
                                    Some(Ok(frame)) => {
                                        if let Ok(data) = frame.into_data() {
                                            if conn
                                                .send(&ToWorker::BodyChunk {
                                                    stream: host,
                                                    bytes: data.to_vec(),
                                                })
                                                .is_err()
                                            {
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        });
                        entry
                            .bodies
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(body.abort_handle());
                    }
                }
            }
        }
    }
}

fn apply_event(target: &StreamEventsTarget, event: StreamEvent) {
    use crate::stream_events;
    match event {
        StreamEvent::Chunk(chunk) => stream_events::emit_chunk(target, &chunk),
        StreamEvent::ThinkingChunk(chunk) => stream_events::emit_thinking_chunk(target, &chunk),
        StreamEvent::ToolCallStarted { id, name } => {
            stream_events::tool_call_started(target, &id, &name)
        }
        StreamEvent::ToolCallInputBytes { id, bytes } => {
            stream_events::tool_call_input_bytes(target, &id, bytes, Instant::now())
        }
    }
}

/// How a worker process ended, as the lost-worker message words it.
fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match status {
        None => "closed its connection".to_string(),
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("exited with status {code}"),
            (None, Some(signal)) => {
                format!("was killed by signal {signal} ({})", signal_name(signal))
            }
            _ => "ended".to_string(),
        },
    }
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGINT => "SIGINT",
        libc::SIGHUP => "SIGHUP",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGPIPE => "SIGPIPE",
        _ => "signal",
    }
}

/// The `E-RUN-065` text for a lost worker.
pub(crate) fn lost_message(
    kind: WorkerKind,
    pid: Option<u32>,
    call: Option<&str>,
    exit: &str,
) -> String {
    let pid = pid.map_or_else(|| "unknown".to_string(), |pid| pid.to_string());
    let call = call.unwrap_or("no call");
    format!(
        "the {kind} guest worker (pid {pid}) was lost while running {call}: it {exit}; the task \
         failed and the session ended",
        kind = kind.as_str()
    )
}

/// Each worker's guests by tool name: the id a guest has there once its hand-over finished.
type LoadedGuests = HashMap<(WorkerKind, String), Arc<tokio::sync::OnceCell<u64>>>;

/// The guest workers of one session: at most one per kind, each started on first need.
pub struct GuestWorkers {
    launcher: Launcher,
    conns: Mutex<HashMap<WorkerKind, Arc<WorkerConn>>>,
    /// The guests each worker holds, by tool name, with their guest ids.
    loaded: Mutex<LoadedGuests>,
    lost: Arc<Mutex<Option<String>>>,
    host_runtime: Mutex<Option<tokio::runtime::Runtime>>,
    host_handle: tokio::runtime::Handle,
    children: Mutex<Vec<std::process::Child>>,
    #[cfg(test)]
    tap: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
}

impl std::fmt::Debug for GuestWorkers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestWorkers")
            .field("kinds", &self.started_kinds())
            .finish()
    }
}

impl GuestWorkers {
    pub(crate) fn new(launcher: Launcher) -> Arc<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("guest-host-calls")
            .enable_all()
            .build()
            .expect("the guest workers' host-call runtime builds");
        let handle = runtime.handle().clone();
        Arc::new(Self {
            launcher,
            conns: Mutex::new(HashMap::new()),
            loaded: Mutex::new(HashMap::new()),
            lost: Arc::new(Mutex::new(None)),
            host_runtime: Mutex::new(Some(runtime)),
            host_handle: handle,
            children: Mutex::new(Vec::new()),
            #[cfg(test)]
            tap: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_tapped(launcher: Launcher, tap: Arc<Mutex<Vec<Vec<u8>>>>) -> Arc<Self> {
        let workers = Self::new(launcher);
        let mut workers = Arc::try_unwrap(workers).expect("fresh");
        workers.tap = Some(tap);
        Arc::new(workers)
    }

    /// The kinds started so far.
    pub fn started_kinds(&self) -> Vec<WorkerKind> {
        let mut kinds: Vec<_> = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        kinds.sort();
        kinds
    }

    /// The session's loss latch: the `E-RUN-065` message of the first worker lost, if any.
    pub fn lost(&self) -> Option<String> {
        self.lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Starts the worker of `kind` unless it is already started. Does not wait for it.
    pub(crate) fn start(self: &Arc<Self>, kind: WorkerKind) -> Arc<WorkerConn> {
        let mut conns = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(conn) = conns.get(&kind) {
            return Arc::clone(conn);
        }
        let conn = Arc::new(WorkerConn {
            kind,
            start: Mutex::new(Start::Starting),
            started: Condvar::new(),
            writer: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
            loading: Mutex::new(None),
            lost: Mutex::new(None),
            session_lost: Arc::clone(&self.lost),
            host_runtime: self.host_handle.clone(),
            #[cfg(test)]
            tap: self.tap.clone(),
        });
        conns.insert(kind, Arc::clone(&conn));
        drop(conns);
        self.launch(&conn);
        conn
    }

    #[allow(unsafe_code)]
    fn launch(self: &Arc<Self>, conn: &Arc<WorkerConn>) {
        let launcher = self.launcher.clone();
        let conn_thread = Arc::clone(conn);
        let workers = Arc::downgrade(self);
        let spawned = std::thread::Builder::new()
            .name(format!("guest-worker-{}-conn", conn.kind.as_str()))
            .spawn(move || {
                let kind = conn_thread.kind;
                // The worker is spawned from this thread, which lives as long as the connection, so
                // its parent-death signal is tied to the connection's life.
                let started = super::launch::start_worker(&launcher, kind);
                let (stream, child) = match started {
                    Ok(started) => started,
                    Err(reason) => {
                        *conn_thread
                            .start
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Start::Failed(reason);
                        conn_thread.started.notify_all();
                        return;
                    }
                };
                let child_pid = child.as_ref().map(std::process::Child::id);
                *conn_thread
                    .start
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Start::Spawned { child: child_pid };
                conn_thread.started.notify_all();
                if let (Some(child), Some(workers)) = (child, workers.upgrade()) {
                    workers
                        .children
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(child);
                }
                let handshake = handshake(kind, &stream);
                let mut reader = stream;
                match handshake {
                    Err(reason) => {
                        if let Some(pid) = child_pid {
                            // SAFETY: plain syscall on the child this thread started.
                            unsafe {
                                libc::kill(pid as libc::pid_t, libc::SIGKILL);
                            }
                        }
                        *conn_thread
                            .start
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Start::Failed(reason);
                        conn_thread.started.notify_all();
                        return;
                    }
                    Ok((pid, writer)) => {
                        *conn_thread
                            .writer
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
                        *conn_thread
                            .start
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Start::Ready { pid };
                        conn_thread.started.notify_all();
                    }
                }
                while let Ok(message) = read_frame::<FromWorker>(&mut reader) {
                    conn_thread.dispatch(message);
                }
                let call = conn_thread.call_description();
                let status = child_pid.and_then(|pid| reap(pid, workers.upgrade()));
                let message = lost_message(
                    kind,
                    conn_thread.pid(),
                    call.as_deref(),
                    &describe_exit(status),
                );
                conn_thread.mark_lost(message);
            });
        if let Err(error) = spawned {
            *conn
                .start
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Start::Failed(format!("its connection thread did not start: {error}"));
            conn.started.notify_all();
        }
    }

    /// The pids of every started worker process, waiting for each start to finish.
    pub fn pids(&self) -> Vec<u32> {
        let conns: Vec<_> = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        let own = std::process::id();
        conns
            .iter()
            .filter_map(|conn| conn.wait_started(HANDSHAKE_WAIT).ok())
            .filter(|pid| *pid != own)
            .collect()
    }

    /// The pids of every started worker process once forked, without waiting for handshakes:
    /// what moves with this process into its cgroup scope.
    pub(crate) fn spawned_pids(&self) -> Vec<u32> {
        let conns: Vec<_> = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        conns
            .iter()
            .filter_map(|conn| conn.wait_spawned())
            .collect()
    }

    /// Waits for every started worker to finish its handshake, or the `E-RUN-066` refusal of
    /// the first that did not.
    pub(crate) fn ready(&self) -> Result<(), RuntimeError> {
        let mut conns: Vec<_> = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        conns.sort_by_key(|conn| conn.kind);
        for conn in conns {
            conn.wait_started(HANDSHAKE_WAIT)
                .map_err(|reason| start_error(conn.kind, &reason))?;
        }
        Ok(())
    }

    /// Runs tool `name`'s `component` in the worker of `kind`: loads it there on first use,
    /// registers `host` as what the call's host imports reach, and records every wall-clock bound
    /// that fired in it into `call_deadlines` before returning.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_tool(
        self: &Arc<Self>,
        kind: WorkerKind,
        component: &Component,
        spec: ToolCallSpec,
        host: ToolCallHost,
        input: ToolInput,
        call_deadlines: &CallDeadlines,
    ) -> Result<ToolResult, String> {
        let conn = self.start(kind);
        if let Err(reason) = conn.wait_started_async().await {
            return Err(start_error(kind, &reason).to_string());
        }
        let name = spec.name.clone();
        let guest = self.ensure_loaded(&conn, &name, component).await?;
        let req = next_id();
        let description = format!("{} '{name}'", spec.role.wire_name());
        let entry = Arc::new(CallEntry {
            description,
            http: Mutex::new(host.http_hooks),
            events: host.events,
            input: host.input,
            process: tokio::sync::Mutex::new(RemoteProcessHost::new(host.process)),
            bodies: Mutex::new(Vec::new()),
        });
        conn.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(req, entry);
        let in_flight = InFlight {
            conn: Arc::clone(&conn),
            req,
            done: false,
        };
        // Instantiation and `run` are each bounded by the worker; past both bounds and the grace,
        // the worker is no longer answering.
        let bound = spec.limits.wall_bound() * 2 + UNRESPONSIVE_GRACE;
        let message = ToWorker::RunTool {
            req,
            guest,
            spec: Box::new(spec),
            input,
        };
        let replied = tokio::time::timeout(bound, conn.request(req, message)).await;
        let mut in_flight = in_flight;
        let reply = match replied {
            Ok(reply) => reply,
            Err(_) => {
                conn.kill();
                // The reader fails the request once it sees the worker gone.
                in_flight.done = true;
                conn.calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&req);
                return Err(wait_lost(&conn).await);
            }
        };
        in_flight.done = true;
        conn.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&req);
        match reply {
            Ok(Reply::Tool { result, deadlines }) => {
                for cut in deadlines {
                    call_deadlines.record(cut);
                }
                result
            }
            Ok(other) => Err(format!("the guest worker answered {other:?}")),
            Err(lost) => Err(lost),
        }
    }

    /// Registers a call in flight on the `kind` worker's connection, holding host objects that
    /// serve nothing, and returns its id.
    #[cfg(test)]
    pub(crate) fn register_test_call(&self, kind: WorkerKind) -> (u64, StreamEventsTarget) {
        let conn = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&kind)
            .cloned()
            .expect("the worker is started");
        let events = StreamEventsTarget {
            sse: None,
            task: None,
            chunks_emitted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tool_calls: None,
        };
        let entry = Arc::new(CallEntry {
            description: "tool 'probe'".to_string(),
            http: Mutex::new(NetworkPolicyHooks {
                network_allow_rules: Vec::new(),
                gateway: None,
                formation: None,
                task_provenance: None,
                member_requests: None,
            }),
            events: events.clone(),
            input: crate::runtime::InputWiring::unavailable(),
            process: tokio::sync::Mutex::new(RemoteProcessHost::new(
                crate::session_process::ProcessAccess::Unavailable(
                    crate::session_process::unavailable::NOT_AGENT_TOOL_CALL,
                ),
            )),
            bodies: Mutex::new(Vec::new()),
        });
        let id = next_id();
        conn.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, entry);
        (id, events)
    }

    /// Sends `message` as request `req` on the `kind` worker's connection and awaits its reply.
    #[cfg(test)]
    pub(crate) async fn request_for_test(
        &self,
        kind: WorkerKind,
        req: u64,
        message: ToWorker,
    ) -> Result<Reply, String> {
        let conn = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&kind)
            .cloned()
            .expect("the worker is started");
        conn.request(req, message).await
    }

    /// Guest `name`'s id in `conn`'s worker, handing it `component` first unless it holds it.
    /// Concurrent callers for one guest share one hand-over.
    pub(crate) async fn ensure_loaded(
        &self,
        conn: &Arc<WorkerConn>,
        name: &str,
        component: &Component,
    ) -> Result<u64, String> {
        let cell = Arc::clone(
            self.loaded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry((conn.kind, name.to_string()))
                .or_default(),
        );
        cell.get_or_try_init(|| load(conn, name, component))
            .await
            .copied()
    }

    /// Starts handing guest `name` to the `kind` worker in the background, so its first call does
    /// not wait for it.
    pub(crate) fn preload(self: &Arc<Self>, kind: WorkerKind, name: &str, component: &Component) {
        let workers = Arc::clone(self);
        let name = name.to_string();
        let component = component.clone();
        self.host_handle.spawn(async move {
            let conn = workers.start(kind);
            if conn.wait_started_async().await.is_ok() {
                let _ = workers.ensure_loaded(&conn, &name, &component).await;
            }
        });
    }

    /// Forgets tool `name` in every worker, so its next call loads the component it is given then.
    pub(crate) fn forget(&self, name: &str) {
        self.loaded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(_, loaded), _| loaded != name);
    }

    /// Ends every worker: `Shutdown`, up to 1 s to exit, then `SIGKILL`, and reaps each.
    pub fn shutdown(&self) {
        let conns: Vec<_> = self
            .conns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for conn in &conns {
            let _ = conn.send(&ToWorker::Shutdown);
        }
        let mut children: Vec<_> = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !children.is_empty() && Instant::now() < deadline {
            children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
            if !children.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }
        for conn in &conns {
            if let Some(writer) = conn
                .writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                let _ = writer.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

impl Drop for GuestWorkers {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(runtime) = self
            .host_runtime
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            runtime.shutdown_background();
        }
    }
}

/// Hands `component` to `conn`'s worker as a new guest named `name`.
async fn load(conn: &Arc<WorkerConn>, name: &str, component: &Component) -> Result<u64, String> {
    let form = component
        .serialize()
        .map_err(|error| format!("failed to hand '{name}' to the guest worker: {error}"))?;
    let guest = next_id();
    let req = next_id();
    *conn
        .loading
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(format!("the load of '{name}'"));
    let reply = conn
        .request(
            req,
            ToWorker::Load {
                req,
                guest,
                name: name.to_string(),
                form,
            },
        )
        .await;
    *conn
        .loading
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    match reply? {
        Reply::Loaded(Ok(())) => Ok(guest),
        Reply::Loaded(Err(error)) => Err(error),
        other => Err(format!("the guest worker answered {other:?}")),
    }
}

/// A guest call in flight; dropped before its reply, it tells the worker to drop the call.
struct InFlight {
    conn: Arc<WorkerConn>,
    req: u64,
    done: bool,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if !self.done {
            self.conn
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.req);
            self.conn
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.req);
            let _ = self.conn.send(&ToWorker::Cancel { req: self.req });
        }
    }
}

async fn wait_lost(conn: &Arc<WorkerConn>) -> String {
    for _ in 0..500 {
        if let Some(lost) = conn
            .lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return lost;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    lost_message(
        conn.kind,
        conn.pid(),
        None,
        "stopped answering and was killed",
    )
}

/// Reaps worker `pid`, returning how it ended.
fn reap(pid: u32, workers: Option<Arc<GuestWorkers>>) -> Option<std::process::ExitStatus> {
    let workers = workers?;
    let mut children = workers
        .children
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = children.iter().position(|child| child.id() == pid)?;
    let mut child = children.remove(index);
    drop(children);
    // The connection closed; give the process a moment to finish exiting.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                return child.wait().ok();
            }
        }
    }
}

/// Exchanges `Hello` and `Init` with a just-started worker. Returns its pid and the writer half.
fn handshake(kind: WorkerKind, stream: &UnixStream) -> Result<(u32, UnixStream), String> {
    let mut reader = stream
        .try_clone()
        .map_err(|error| format!("its connection could not be shared: {error}"))?;
    reader
        .set_read_timeout(Some(HANDSHAKE_WAIT))
        .map_err(|error| error.to_string())?;
    let hello = read_frame::<FromWorker>(&mut reader)
        .map_err(|error| format!("it did not say hello: {error}"))?;
    let pid = match hello {
        FromWorker::Hello {
            protocol,
            version,
            pid,
        } => {
            if protocol != PROTOCOL_VERSION {
                return Err(format!(
                    "it speaks guest-worker protocol {protocol} (mur {version}), not {PROTOCOL_VERSION}"
                ));
            }
            pid
        }
        other => return Err(format!("its first frame was not a hello: {other:?}")),
    };
    reader
        .set_read_timeout(None)
        .map_err(|error| error.to_string())?;
    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("its connection could not be shared: {error}"))?;
    write_frame(&mut writer, &ToWorker::Init { kind })?;
    Ok((pid, writer))
}

/// The `E-RUN-066` refusal for a worker of `kind` that did not start, for `reason`.
pub(crate) fn start_error(kind: WorkerKind, reason: &str) -> RuntimeError {
    RuntimeError::GuestWorkerStart(format!(
        "the {} guest worker could not be started: {reason}",
        kind.as_str()
    ))
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<GuestWorkers>();
    let _ = HashSet::<u8>::new();
}
