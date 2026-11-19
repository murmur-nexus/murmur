//! `network_request`: the record of one HTTP request a guest sent on its own through `wasi:http`.
//!
//! Every request a WASM tool, a driver reached as a tool, a hook or a script capsule's own
//! component sends — straight to its host, through its artifact's own credential gateway, or
//! refused by the allow-list before any connection opens — is accounted for exactly once in the
//! session's [`NetworkRequests`] latch, which the session's trace writer drains into
//! `network_request` lines. Two routes are recorded elsewhere and never here: a model call
//! admitted to the metered inference gateway (its `inference` line) and a request to a member's
//! virtual address (its `member_request` line).
//!
//! A record names the component, the route, the scheme, the host and port, the method, the
//! outcome, the status, the two body sizes and the duration. It never holds a path, a query, a
//! header, a key or body text. Under `trace.capture: meta` it also holds the SHA-256 of each body,
//! and under `content` a copy of each body for the writer to store behind that hash.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use murmur_artifact::TraceCapture;

use crate::limits::ComponentRole;

/// The reason a request the allow-list refused carries.
pub(crate) const REASON_NOT_ALLOWED: &str = "not_allowed";
/// The reason a request to the metered inference gateway with no open spend admission carries.
pub(crate) const REASON_NOT_ADMITTED: &str = "not_admitted";
/// No response head arrived: the connection or writing the request failed, or the gateway's
/// injected credential was not supplied.
pub(crate) const REASON_CONNECTION_FAILED: &str = "connection_failed";
/// A response head arrived, then reading its body failed.
pub(crate) const REASON_RESPONSE_INTERRUPTED: &str = "response_interrupted";
/// No response head arrived before the request was abandoned: the component's call ended — its
/// deadline, a cancel or a lost guest worker — or the component dropped the request.
pub(crate) const REASON_INTERRUPTED: &str = "interrupted";

/// How a request left the runtime, as `network_request.route` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkRoute {
    /// Straight to the address the guest named.
    Direct,
    /// Through the artifact's own credential gateway, to that gateway's upstream.
    Gateway,
}

impl NetworkRoute {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Gateway => "gateway",
        }
    }
}

/// How a request ended. Any status the host answered with is `Sent`: the host answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkRequestOutcome {
    Sent,
    /// Refused before any connection, with [`REASON_NOT_ALLOWED`] or [`REASON_NOT_ADMITTED`].
    Refused(&'static str),
    /// With [`REASON_CONNECTION_FAILED`], [`REASON_RESPONSE_INTERRUPTED`] or
    /// [`REASON_INTERRUPTED`].
    Failed(&'static str),
}

impl NetworkRequestOutcome {
    /// The value `network_request.outcome` carries.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
        }
    }

    /// The value `network_request.reason` carries: present exactly for `refused` and `failed`.
    pub(crate) fn reason(self) -> Option<&'static str> {
        match self {
            Self::Sent => None,
            Self::Refused(reason) | Self::Failed(reason) => Some(reason),
        }
    }
}

/// What `trace.capture` keeps of one body beyond its size: its SHA-256 under `meta` and
/// `content`, and the bytes themselves only under `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedBody {
    /// 64 lowercase hex digits naming exactly the bytes counted.
    pub(crate) sha256: String,
    /// Every byte hashed, held until the trace writer stores it as `blobs/<sha256>`.
    pub(crate) bytes: Option<Vec<u8>>,
}

/// One guest request, as its `network_request` line records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NetworkRequest {
    /// The task the request was sent for; `None` from a hook, a hook's `run-inference` driver, a
    /// script capsule, or the agent capsule's own store.
    pub(crate) task_id: Option<String>,
    /// The tool name of the artifact, the hook's name, or the capsule's manifest `name`.
    pub(crate) component: String,
    pub(crate) role: ComponentRole,
    pub(crate) route: NetworkRoute,
    /// The scheme the request went out with: the URI's, or the gateway upstream's.
    pub(crate) scheme: String,
    /// The host, with `:port` when the address named one. `None` when the URI names no host.
    pub(crate) host: Option<String>,
    pub(crate) method: String,
    pub(crate) outcome: NetworkRequestOutcome,
    /// The host's status, when a response head arrived.
    pub(crate) http_status: Option<u16>,
    /// Request body bytes the runtime read; `None` for a refused request.
    pub(crate) request_bytes: Option<u64>,
    /// Response body bytes the guest read; `None` when no response head arrived.
    pub(crate) response_bytes: Option<u64>,
    /// `None` exactly when `request_bytes` is, or under `trace.capture: none`.
    pub(crate) request_body: Option<CapturedBody>,
    /// `None` exactly when `response_bytes` is, or under `trace.capture: none`.
    pub(crate) response_body: Option<CapturedBody>,
    /// From the request reaching the runtime to it being accounted for.
    pub(crate) duration_ms: u64,
}

/// The session's record of every guest request, filled by the stores that send them and drained
/// by the trace writer holding a clone — ahead of every line it writes, so never before
/// `session_start` and always before `session_end`.
///
/// Created once per session at staging, with the session's `trace.capture`, and cloned into every
/// store that sends through `wasi:http`: the capsule store, each tool and driver store, each hook
/// store.
#[derive(Debug, Clone, Default)]
pub(crate) struct NetworkRequests {
    requests: Arc<Mutex<Vec<NetworkRequest>>>,
    capture: TraceCapture,
}

impl NetworkRequests {
    pub(crate) fn new(capture: TraceCapture) -> Self {
        Self {
            requests: Arc::default(),
            capture,
        }
    }

    /// The session's `trace.capture`, which decides what each recorded body keeps.
    pub(crate) fn capture(&self) -> TraceCapture {
        self.capture
    }

    pub(crate) fn push(&self, request: NetworkRequest) {
        self.lock().push(request);
    }

    /// Takes every request recorded since the last take, oldest first.
    pub(crate) fn take(&self) -> Vec<NetworkRequest> {
        std::mem::take(&mut *self.lock())
    }

    /// A poisoned latch is still read: a lost line costs the record, a panic would cost the
    /// session.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<NetworkRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Who sends from one store: the session's latch, the component, role and calling task each of
/// that store's requests is recorded under, and the requests it has in flight.
#[derive(Debug, Clone)]
pub(crate) struct NetworkRequestScope {
    pub(crate) requests: NetworkRequests,
    pub(crate) component: String,
    pub(crate) role: ComponentRole,
    pub(crate) task_id: Option<String>,
    in_flight: InFlight,
}

impl NetworkRequestScope {
    pub(crate) fn new(
        requests: &NetworkRequests,
        component: impl Into<String>,
        role: ComponentRole,
        task_id: Option<String>,
    ) -> Self {
        Self {
            requests: requests.clone(),
            component: component.into(),
            role,
            task_id,
            in_flight: InFlight::default(),
        }
    }

    /// Records every request sent under this scope that is not yet accounted for, oldest first:
    /// one whose response head arrived as `sent` with the response bytes read so far, any other
    /// as `failed`/`interrupted`. Called when the store's call ends, so each line is in the latch
    /// before the call's own; the request's recorder or body, dropped later, records nothing.
    pub(crate) fn interrupt_in_flight(&self) {
        self.in_flight.interrupt();
    }

    /// A capsule's own store: recorded under the capsule's manifest `name`, for no task.
    pub(crate) fn for_capsule(requests: &NetworkRequests, capsule_name: impl Into<String>) -> Self {
        Self::new(requests, capsule_name, ComponentRole::Capsule, None)
    }

    /// A hook's store: recorded under the hook's name, for no task, since a hook runs for no task
    /// of its own.
    pub(crate) fn for_hook(requests: &NetworkRequests, hook_name: impl Into<String>) -> Self {
        Self::new(requests, hook_name, ComponentRole::Hook, None)
    }
}

/// `uri`'s host with `:port` when the URI names a port. User info, path and query are dropped.
pub(crate) fn host_of(uri: &http::Uri) -> Option<String> {
    let host = uri.host()?;
    Some(match uri.port_u16() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// The size, digest and copy of one body, kept as its frames pass.
struct Tally {
    bytes: u64,
    /// `None` under `trace.capture: none`.
    digest: Option<murmur_artifact::Sha256Hasher>,
    /// `Some` only under `trace.capture: content`.
    copy: Option<Vec<u8>>,
}

impl Tally {
    fn new(capture: TraceCapture) -> Self {
        Self {
            bytes: 0,
            digest: capture
                .captures_hashes()
                .then(murmur_artifact::Sha256Hasher::new),
            copy: capture.captures_content().then(Vec::new),
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.bytes = self
            .bytes
            .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        if let Some(digest) = self.digest.as_mut() {
            digest.update(data);
        }
        if let Some(copy) = self.copy.as_mut() {
            copy.extend_from_slice(data);
        }
    }

    /// The byte count and what `trace.capture` keeps, as of every frame counted so far.
    fn finish(self) -> (u64, Option<CapturedBody>) {
        let captured = self.digest.map(|digest| CapturedBody {
            sha256: digest.finish_hex(),
            bytes: self.copy,
        });
        (self.bytes, captured)
    }
}

/// A request body's [`Tally`], shared between the body the runtime reads and the record that
/// reads the tally when the request is accounted for.
type SharedTally = Arc<Mutex<Tally>>;

fn lock_tally(tally: &SharedTally) -> std::sync::MutexGuard<'_, Tally> {
    tally
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One request not yet accounted for.
struct Pending {
    requests: NetworkRequests,
    request: NetworkRequest,
    started: Instant,
    /// Set once the request body is wrapped.
    request_tally: Option<SharedTally>,
    /// The status and the response body's tally, once a response head arrived.
    response: Option<(u16, Tally)>,
}

impl Pending {
    /// Pushes the request into the session's latch with `outcome`, the status and both tallies
    /// as of now.
    fn record(self, outcome: NetworkRequestOutcome) {
        let Self {
            requests,
            mut request,
            started,
            request_tally,
            response,
        } = self;
        if !matches!(outcome, NetworkRequestOutcome::Refused(_)) {
            // A body the runtime never read — a connection that failed before writing it — is
            // still a request of zero bytes counted, so its size and hash are present together.
            // Nothing reads the tally once the request is recorded, so it is taken, not copied.
            let tally = request_tally.map_or_else(
                || Tally::new(requests.capture()),
                |tally| std::mem::replace(&mut *lock_tally(&tally), Tally::new(TraceCapture::None)),
            );
            let (bytes, captured) = tally.finish();
            request.request_bytes = Some(bytes);
            request.request_body = captured;
            if let Some((status, tally)) = response {
                let (bytes, captured) = tally.finish();
                request.http_status = Some(status);
                request.response_bytes = Some(bytes);
                request.response_body = captured;
            }
        }
        request.outcome = outcome;
        request.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        requests.push(request);
    }
}

/// One request's [`Pending`] record, shared by its recorder (then its response body) and the
/// [`InFlight`] list of the store that sent it: whichever ends the request first records it, and
/// the slot is empty from then on.
type Slot = Arc<Mutex<Option<Pending>>>;

/// A [`Slot`] as an [`InFlight`] list holds it: gone once the recorder and body have both dropped.
type WeakSlot = std::sync::Weak<Mutex<Option<Pending>>>;

fn lock_slot(slot: &Slot) -> std::sync::MutexGuard<'_, Option<Pending>> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Records the request in `slot` with `outcome`, unless it has been recorded already.
fn finish_slot(slot: &Slot, outcome: NetworkRequestOutcome) {
    let pending = lock_slot(slot).take();
    if let Some(pending) = pending {
        pending.record(outcome);
    }
}

/// The requests one store has sent and not yet accounted for, oldest first, so the end of the
/// store's call can account for each one still in flight.
#[derive(Debug, Clone, Default)]
struct InFlight(Arc<Mutex<Vec<WeakSlot>>>);

impl InFlight {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<WeakSlot>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(&self, slot: &Slot) {
        let mut slots = self.lock();
        slots.retain(|slot| slot.strong_count() > 0);
        slots.push(Arc::downgrade(slot));
    }

    fn interrupt(&self) {
        let slots = std::mem::take(&mut *self.lock());
        for slot in slots.iter().filter_map(std::sync::Weak::upgrade) {
            let pending = lock_slot(&slot).take();
            if let Some(pending) = pending {
                let outcome = if pending.response.is_some() {
                    NetworkRequestOutcome::Sent
                } else {
                    NetworkRequestOutcome::Failed(REASON_INTERRUPTED)
                };
                pending.record(outcome);
            }
        }
    }
}

/// One request in flight. It records into the scope's latch exactly once: through
/// [`Self::refuse`] or [`Self::fail`], through the response body [`Self::wrap_response`] returns,
/// through [`NetworkRequestScope::interrupt_in_flight`], or, dropped before any response head
/// arrived, as `failed`/`interrupted`.
pub(crate) struct NetworkRequestRecorder {
    /// `None` when the store records nothing.
    slot: Option<Slot>,
}

impl NetworkRequestRecorder {
    /// A recorder for one request, started at `started`. With no `scope` it records nothing.
    pub(crate) fn begin(
        scope: Option<&NetworkRequestScope>,
        route: NetworkRoute,
        scheme: &str,
        host: Option<String>,
        method: &http::Method,
        started: Instant,
    ) -> Self {
        let slot = scope.map(|scope| {
            let slot = Arc::new(Mutex::new(Some(Pending {
                requests: scope.requests.clone(),
                request: NetworkRequest {
                    task_id: scope.task_id.clone(),
                    component: scope.component.clone(),
                    role: scope.role,
                    route,
                    scheme: scheme.to_string(),
                    host,
                    method: method.as_str().to_string(),
                    outcome: NetworkRequestOutcome::Sent,
                    http_status: None,
                    request_bytes: None,
                    response_bytes: None,
                    request_body: None,
                    response_body: None,
                    duration_ms: 0,
                },
                started,
                request_tally: None,
                response: None,
            })));
            scope.in_flight.register(&slot);
            slot
        });
        Self { slot }
    }

    /// Records the request as refused before any connection: no status, no sizes, no hashes.
    pub(crate) fn refuse(mut self, reason: &'static str) {
        if let Some(slot) = self.slot.take() {
            finish_slot(&slot, NetworkRequestOutcome::Refused(reason));
        }
    }

    /// Records the request as failed before any response head arrived, with the request bytes
    /// read so far.
    pub(crate) fn fail(mut self, reason: &'static str) {
        if let Some(slot) = self.slot.take() {
            finish_slot(&slot, NetworkRequestOutcome::Failed(reason));
        }
    }

    /// `request` with its body counted, and hashed and copied as `trace.capture` asks, frame by
    /// frame as the runtime reads it. Unchanged when this recorder records nothing.
    pub(crate) fn wrap_request(
        &mut self,
        request: http::Request<wasmtime_wasi_http::WasiBody>,
    ) -> http::Request<wasmtime_wasi_http::WasiBody> {
        use http_body_util::BodyExt;
        let Some(slot) = self.slot.as_ref() else {
            return request;
        };
        let tally = {
            let mut pending = lock_slot(slot);
            let Some(pending) = pending.as_mut() else {
                return request;
            };
            let tally = Arc::new(Mutex::new(Tally::new(pending.requests.capture())));
            pending.request_tally = Some(Arc::clone(&tally));
            tally
        };
        request.map(|body| CountedRequestBody { inner: body, tally }.boxed_unsync())
    }

    /// `body`, the response to this request, passed through to the guest and accounted for as it
    /// is read. Unchanged when this recorder records nothing or the request was already
    /// accounted for.
    pub(crate) fn wrap_response(
        mut self,
        body: wasmtime_wasi_http::WasiBody,
        http_status: u16,
    ) -> wasmtime_wasi_http::WasiBody {
        use http_body_util::BodyExt;
        let Some(slot) = self.slot.take() else {
            return body;
        };
        {
            let mut pending = lock_slot(&slot);
            let Some(pending) = pending.as_mut() else {
                return body;
            };
            pending.response = Some((http_status, Tally::new(pending.requests.capture())));
        }
        NetworkRequestBody { inner: body, slot }.boxed_unsync()
    }
}

impl Drop for NetworkRequestRecorder {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            finish_slot(&slot, NetworkRequestOutcome::Failed(REASON_INTERRUPTED));
        }
    }
}

/// A request body passed through to the connection frame by frame, unchanged, whose data frames
/// are tallied as the runtime reads them.
struct CountedRequestBody<B> {
    inner: B,
    tally: SharedTally,
}

impl<B> Body for CountedRequestBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled {
            if let Some(data) = frame.data_ref() {
                lock_tally(&this.tally).update(data);
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// A response body passed through to the guest frame by frame, unchanged and unbuffered beyond
/// the `trace.capture: content` copy, that records its request once: `sent` at the end of the
/// stream or when dropped, with the bytes read so far, and `failed`/`response_interrupted` when
/// the stream errors.
struct NetworkRequestBody<B> {
    inner: B,
    slot: Slot,
}

impl<B> Body for NetworkRequestBody<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    if let Some(Pending {
                        response: Some((_, tally)),
                        ..
                    }) = lock_slot(&this.slot).as_mut()
                    {
                        tally.update(data);
                    }
                }
                if this.inner.is_end_stream() {
                    finish_slot(&this.slot, NetworkRequestOutcome::Sent);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                finish_slot(
                    &this.slot,
                    NetworkRequestOutcome::Failed(REASON_RESPONSE_INTERRUPTED),
                );
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                finish_slot(&this.slot, NetworkRequestOutcome::Sent);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B> Drop for NetworkRequestBody<B> {
    fn drop(&mut self) {
        finish_slot(&self.slot, NetworkRequestOutcome::Sent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    fn scope(requests: &NetworkRequests) -> NetworkRequestScope {
        NetworkRequestScope::new(
            requests,
            "fetcher",
            ComponentRole::Tool,
            Some("tsk_a".into()),
        )
    }

    fn recorder(requests: &NetworkRequests) -> NetworkRequestRecorder {
        NetworkRequestRecorder::begin(
            Some(&scope(requests)),
            NetworkRoute::Direct,
            "https",
            Some("api.example.com".into()),
            &http::Method::GET,
            Instant::now(),
        )
    }

    fn wasi_body(bytes: &'static [u8]) -> wasmtime_wasi_http::WasiBody {
        Full::new(Bytes::from_static(bytes))
            .map_err(|never| match never {})
            .boxed_unsync()
    }

    /// A body that yields its one frame and then never ends.
    struct Stalling(Option<&'static [u8]>);

    impl Body for Stalling {
        type Data = Bytes;
        type Error = std::convert::Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
            match self.0.take() {
                Some(bytes) => Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(bytes))))),
                None => Poll::Pending,
            }
        }
    }

    #[test]
    fn network_request_host_keeps_an_explicit_port_and_drops_user_info_path_and_query() {
        let uri: http::Uri = "https://user:pw@api.example.com:8443/v1/x?key=secret"
            .parse()
            .unwrap();
        assert_eq!(host_of(&uri).as_deref(), Some("api.example.com:8443"));
        let uri: http::Uri = "https://api.example.com/v1/x".parse().unwrap();
        assert_eq!(host_of(&uri).as_deref(), Some("api.example.com"));
        let uri: http::Uri = "/relative".parse().unwrap();
        assert_eq!(host_of(&uri), None);
    }

    #[test]
    fn network_request_recorder_dropped_unfinished_records_interrupted_once() {
        let requests = NetworkRequests::default();
        drop(recorder(&requests));
        let lines = requests.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].outcome,
            NetworkRequestOutcome::Failed(REASON_INTERRUPTED)
        );
        assert_eq!(lines[0].task_id.as_deref(), Some("tsk_a"));
        assert_eq!(lines[0].request_bytes, Some(0));
        assert_eq!(lines[0].response_bytes, None);
        assert!(requests.take().is_empty());
    }

    #[test]
    fn network_request_refused_keeps_no_sizes_or_hashes() {
        let requests = NetworkRequests::default();
        let mut recorder = recorder(&requests);
        let request = recorder.wrap_request(http::Request::new(wasi_body(b"abc")));
        drop(request);
        recorder.refuse(REASON_NOT_ALLOWED);
        let lines = requests.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].outcome.reason(), Some("not_allowed"));
        assert_eq!(lines[0].request_bytes, None);
        assert_eq!(lines[0].request_body, None);
        assert_eq!(lines[0].http_status, None);
    }

    #[test]
    fn network_request_bodies_pass_through_unchanged_and_are_counted_and_captured() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for capture in [
            TraceCapture::None,
            TraceCapture::Meta,
            TraceCapture::Content,
        ] {
            let requests = NetworkRequests::new(capture);
            let mut recorder = recorder(&requests);
            let request = recorder.wrap_request(http::Request::new(wasi_body(b"ping!")));
            let sent = rt.block_on(request.into_body().collect()).unwrap();
            assert_eq!(sent.to_bytes(), Bytes::from_static(b"ping!"));
            let body = recorder.wrap_response(wasi_body(b"a longer answer"), 201);
            let read = rt.block_on(body.collect()).unwrap();
            assert_eq!(read.to_bytes(), Bytes::from_static(b"a longer answer"));

            let lines = requests.take();
            assert_eq!(lines.len(), 1, "{capture:?}");
            let line = &lines[0];
            assert_eq!(line.outcome, NetworkRequestOutcome::Sent);
            assert_eq!(line.http_status, Some(201));
            assert_eq!(line.request_bytes, Some(5));
            assert_eq!(line.response_bytes, Some(15));
            let want = |bytes: &[u8]| {
                capture.captures_hashes().then(|| CapturedBody {
                    sha256: murmur_artifact::sha256_hex(bytes),
                    bytes: capture.captures_content().then(|| bytes.to_vec()),
                })
            };
            assert_eq!(line.request_body, want(b"ping!"), "{capture:?}");
            assert_eq!(line.response_body, want(b"a longer answer"), "{capture:?}");
        }
    }

    #[test]
    fn network_request_with_no_scope_passes_both_bodies_through_unchanged() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut recorder = NetworkRequestRecorder::begin(
            None,
            NetworkRoute::Direct,
            "https",
            None,
            &http::Method::GET,
            Instant::now(),
        );
        let request = recorder.wrap_request(http::Request::new(wasi_body(b"x")));
        let sent = rt.block_on(request.into_body().collect()).unwrap();
        assert_eq!(sent.to_bytes(), Bytes::from_static(b"x"));
        let body = recorder.wrap_response(wasi_body(b"y"), 200);
        let read = rt.block_on(body.collect()).unwrap();
        assert_eq!(read.to_bytes(), Bytes::from_static(b"y"));
    }

    /// The end of a store's call accounts for each request still in flight once, in the order
    /// sent: one with no response head as `interrupted`, one mid-body as `sent` with the bytes
    /// read so far. Neither its recorder nor its body, dropped afterwards, adds a line.
    #[test]
    fn network_request_in_flight_at_call_end_is_recorded_once() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let requests = NetworkRequests::new(TraceCapture::Content);
        let scope = scope(&requests);
        let begin = || {
            NetworkRequestRecorder::begin(
                Some(&scope),
                NetworkRoute::Direct,
                "https",
                Some("api.example.com".into()),
                &http::Method::GET,
                Instant::now(),
            )
        };
        let mut waiting = begin();
        let request = waiting.wrap_request(http::Request::new(wasi_body(b"abc")));
        rt.block_on(request.into_body().collect()).unwrap();
        let reading = begin();
        let mut body = reading.wrap_response(
            Stalling(Some(b"part"))
                .map_err(|never| match never {})
                .boxed_unsync(),
            200,
        );
        rt.block_on(async {
            let frame = body.frame().await.unwrap().unwrap();
            assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"part"));
        });

        scope.interrupt_in_flight();
        let lines = requests.take();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0].outcome,
            NetworkRequestOutcome::Failed(REASON_INTERRUPTED)
        );
        assert_eq!(lines[0].http_status, None);
        assert_eq!(lines[0].request_bytes, Some(3));
        assert_eq!(lines[0].response_bytes, None);
        assert_eq!(lines[1].outcome, NetworkRequestOutcome::Sent);
        assert_eq!(lines[1].http_status, Some(200));
        assert_eq!(lines[1].response_bytes, Some(4));
        assert_eq!(
            lines[1]
                .response_body
                .as_ref()
                .and_then(|b| b.bytes.as_deref()),
            Some(&b"part"[..])
        );

        drop(waiting);
        drop(body);
        scope.interrupt_in_flight();
        assert!(requests.take().is_empty());
    }
}
