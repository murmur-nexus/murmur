//! `member_request`: the sender's record of one request its runtime routed to a member's virtual
//! address (`http://<member>.formation.invalid`).
//!
//! Every such request — from a WASM tool, a hook, a `transport: http` driver or a script
//! capsule's own `wasi:http`, and a script capsule's `murmur:message/send` — is accounted for
//! exactly once, refusals included, in the session's [`MemberRequests`] latch, which the session's
//! trace writer drains into `member_request` lines. The runtime mints the request's id
//! ([`crate::member_call::mint_member_request_id`]) and sends it to the callee's door on
//! [`crate::member_call::MEMBER_REQUEST_ID_HEADER`]; the callee's `a2a_task_received` records it,
//! which is what joins the two traces into one call row in the formation view.
//!
//! A record names the member, the component and the outcome. It never holds a door address, a
//! token, a header value or any request or response body text.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

use crate::limits::ComponentRole;
use crate::member_call::MemberCallRefusal;

/// How much of a response body is kept to read the callee's task id from. The body itself passes
/// through unbounded; only this copy is capped.
pub(crate) const RESPONSE_COPY_LIMIT: usize = 256 * 1024;

/// Which surface sent a member request, as `member_request.via` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberRequestVia {
    /// A component's own `wasi:http` request.
    WasiHttp,
    /// A script capsule's `murmur:message/send`.
    MessageSend,
}

impl MemberRequestVia {
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::WasiHttp => "wasi_http",
            Self::MessageSend => "message_send",
        }
    }
}

/// Why a request to a virtual address was not sent: the address names no member in the shape
/// the runtime routes, or the routing rule refused the member it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRequestRefusal {
    /// A scheme other than `http`, a port, user info, or a host that is not exactly one label
    /// under the formation domain.
    NotAVirtualAddress,
    /// [`crate::member_call::resolve_member_call`]'s refusal.
    Member(MemberCallRefusal),
}

impl MemberRequestRefusal {
    /// The value `member_request.reason` carries for this refusal.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::NotAVirtualAddress => "not_a_virtual_address",
            Self::Member(MemberCallRefusal::NotACallee) => "not_a_callee",
            Self::Member(MemberCallRefusal::AddressNotArrived { .. }) => "address_not_arrived",
            Self::Member(MemberCallRefusal::EgressNotAllowed) => "egress_not_allowed",
            Self::Member(MemberCallRefusal::Unparseable) => "unreadable_address",
        }
    }
}

/// Why a request that was routed did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberRequestFailure {
    /// No response head arrived: the connection, or writing the request, failed.
    ConnectionFailed,
    /// A response head arrived, then reading the body failed.
    ResponseInterrupted,
}

impl MemberRequestFailure {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::ConnectionFailed => "connection_failed",
            Self::ResponseInterrupted => "response_interrupted",
        }
    }
}

/// How a member request ended. A non-2xx answer or a JSON-RPC error is `Sent`: the door answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberRequestOutcome {
    Sent,
    /// Refused before any connection, with [`MemberRequestRefusal::reason`].
    Refused(&'static str),
    Failed(MemberRequestFailure),
}

impl MemberRequestOutcome {
    /// The value `member_request.outcome` carries.
    pub(crate) fn wire_name(&self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Refused(_) => "refused",
            Self::Failed(_) => "failed",
        }
    }

    /// The value `member_request.reason` carries: present exactly for `refused` and `failed`.
    pub(crate) fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Sent => None,
            Self::Refused(reason) => Some(reason),
            Self::Failed(failure) => Some(failure.reason()),
        }
    }
}

/// One member request, as its `member_request` line records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemberRequest {
    /// `mrq_` and 32 lowercase hex digits, minted by the runtime.
    pub(crate) call_id: String,
    /// The task the request was sent for; `None` from a hook, a hook's `run-inference` driver, a
    /// script capsule, or the agent capsule's own store.
    pub(crate) task_id: Option<String>,
    /// The roster name the virtual address names; `None` when its host names no single member.
    pub(crate) member: Option<String>,
    /// The tool name of the artifact, the hook's name, or the capsule's manifest `name`.
    pub(crate) component: String,
    pub(crate) role: ComponentRole,
    pub(crate) via: MemberRequestVia,
    pub(crate) outcome: MemberRequestOutcome,
    /// The door's status, when a response head arrived.
    pub(crate) http_status: Option<u16>,
    /// The callee's task id, when the response carried a `SendMessageResponse` task.
    pub(crate) member_task_id: Option<String>,
    /// From the request reaching the runtime to it being accounted for.
    pub(crate) duration_ms: u64,
}

/// The session's record of every member request, filled by the stores that send them and drained
/// by the trace writer holding a clone — ahead of every line it writes, so never before
/// `session_start` and always before `session_end`.
///
/// Created once per session at staging and cloned into every store that can route a request to a
/// virtual address: the capsule store, each tool and driver store, each hook store.
#[derive(Debug, Clone, Default)]
pub(crate) struct MemberRequests(Arc<Mutex<Vec<MemberRequest>>>);

impl MemberRequests {
    pub(crate) fn push(&self, request: MemberRequest) {
        self.lock().push(request);
    }

    /// Takes every request recorded since the last take, oldest first.
    pub(crate) fn take(&self) -> Vec<MemberRequest> {
        std::mem::take(&mut *self.lock())
    }

    /// A poisoned latch is still read: a lost line costs the record, a panic would cost the
    /// session.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<MemberRequest>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Who sends from one store: the session's latch, and the component, role and calling task each
/// of that store's member requests is recorded under.
#[derive(Debug, Clone)]
pub(crate) struct MemberRequestScope {
    pub(crate) requests: MemberRequests,
    pub(crate) component: String,
    pub(crate) role: ComponentRole,
    pub(crate) task_id: Option<String>,
}

impl MemberRequestScope {
    pub(crate) fn new(
        requests: &MemberRequests,
        component: impl Into<String>,
        role: ComponentRole,
        task_id: Option<String>,
    ) -> Self {
        Self {
            requests: requests.clone(),
            component: component.into(),
            role,
            task_id,
        }
    }

    /// A capsule's own store: recorded under the capsule's manifest `name`, for no task.
    pub(crate) fn for_capsule(requests: &MemberRequests, capsule_name: impl Into<String>) -> Self {
        Self::new(requests, capsule_name, ComponentRole::Capsule, None)
    }

    /// A hook's store: recorded under the hook's name, for no task, since a hook runs for no task
    /// of its own.
    pub(crate) fn for_hook(requests: &MemberRequests, hook_name: impl Into<String>) -> Self {
        Self::new(requests, hook_name, ComponentRole::Hook, None)
    }
}

/// One member request in flight. It records into the scope's latch exactly once: through
/// [`Self::finish`], or, dropped unfinished — before any response head arrived — as
/// `failed`/`connection_failed`.
pub(crate) struct MemberRequestRecorder {
    pending: Option<(MemberRequests, MemberRequest)>,
    started: Instant,
}

impl MemberRequestRecorder {
    /// A recorder for the request `call_id` to `member`, started at `started`. With no `scope` it
    /// records nothing.
    pub(crate) fn begin(
        scope: Option<&MemberRequestScope>,
        call_id: &str,
        member: Option<&str>,
        via: MemberRequestVia,
        started: Instant,
    ) -> Self {
        let pending = scope.map(|scope| {
            (
                scope.requests.clone(),
                MemberRequest {
                    call_id: call_id.to_string(),
                    task_id: scope.task_id.clone(),
                    member: member.map(str::to_string),
                    component: scope.component.clone(),
                    role: scope.role,
                    via,
                    outcome: MemberRequestOutcome::Sent,
                    http_status: None,
                    member_task_id: None,
                    duration_ms: 0,
                },
            )
        });
        Self { pending, started }
    }

    /// Whether this request is still to be recorded.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Records the request with `outcome`; a no-op once it has been recorded.
    pub(crate) fn finish(
        &mut self,
        outcome: MemberRequestOutcome,
        http_status: Option<u16>,
        member_task_id: Option<String>,
    ) {
        let Some((requests, mut request)) = self.pending.take() else {
            return;
        };
        request.outcome = outcome;
        request.http_status = http_status;
        request.member_task_id = member_task_id;
        request.duration_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        requests.push(request);
    }
}

impl Drop for MemberRequestRecorder {
    fn drop(&mut self) {
        self.finish(
            MemberRequestOutcome::Failed(MemberRequestFailure::ConnectionFailed),
            None,
            None,
        );
    }
}

/// A response body passed through to the guest frame by frame, unchanged and unbuffered, that
/// records its member request once: `sent` at the end of the stream or when dropped,
/// `failed`/`response_interrupted` when the stream errors. It keeps a copy of at most
/// [`RESPONSE_COPY_LIMIT`] bytes, from which [`member_task_id`] reads the callee's task.
pub(crate) struct MemberRequestBody<B> {
    inner: B,
    recorder: MemberRequestRecorder,
    http_status: u16,
    copy: Vec<u8>,
}

impl<B> MemberRequestBody<B> {
    pub(crate) fn new(inner: B, recorder: MemberRequestRecorder, http_status: u16) -> Self {
        Self {
            inner,
            recorder,
            http_status,
            copy: Vec::new(),
        }
    }

    fn finish(&mut self, outcome: MemberRequestOutcome) {
        if self.recorder.is_pending() {
            let task = member_task_id(&self.copy);
            self.recorder.finish(outcome, Some(self.http_status), task);
        }
    }
}

impl<B> Body for MemberRequestBody<B>
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
                    let room = RESPONSE_COPY_LIMIT.saturating_sub(this.copy.len());
                    this.copy.extend_from_slice(&data[..data.len().min(room)]);
                }
                if this.inner.is_end_stream() {
                    this.finish(MemberRequestOutcome::Sent);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finish(MemberRequestOutcome::Failed(
                    MemberRequestFailure::ResponseInterrupted,
                ));
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish(MemberRequestOutcome::Sent);
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

impl<B> Drop for MemberRequestBody<B> {
    fn drop(&mut self) {
        self.finish(MemberRequestOutcome::Sent);
    }
}

/// The callee's task id a response body names: a JSON body's `result`, or an event stream's
/// first `data:` line's, read as a `SendMessageResponse` whose `task` member has an `id`. `None`
/// for anything else, a body cut off at [`RESPONSE_COPY_LIMIT`] included.
pub(crate) fn member_task_id(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim_start();
    let response: serde_json::Value = if text.starts_with('{') {
        serde_json::from_str(text).ok()?
    } else {
        let data = text
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))?;
        serde_json::from_str(data.trim()).ok()?
    };
    let result = response.get("result")?;
    match crate::a2a::send_message_member(result).ok()? {
        ("task", task) => task.get("id")?.as_str().map(str::to_string),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(requests: &MemberRequests) -> MemberRequestScope {
        MemberRequestScope::new(requests, "probe", ComponentRole::Tool, Some("tsk_a".into()))
    }

    #[test]
    fn member_request_task_id_is_read_from_a_json_answer_or_an_event_stream() {
        let json = br#"{"jsonrpc":"2.0","id":"x","result":{"task":{"id":"tsk_1"}}}"#;
        assert_eq!(member_task_id(json).as_deref(), Some("tsk_1"));
        let stream = b"event: x\r\ndata: {\"result\":{\"task\":{\"id\":\"tsk_2\"}}}\r\n\r\n";
        assert_eq!(member_task_id(stream).as_deref(), Some("tsk_2"));
        for none in [
            &br#"{"name":"coder","url":"x"}"#[..],
            br#"{"result":{"message":{"messageId":"m"}}}"#,
            br#"{"error":{"code":-32600}}"#,
            br#"{"result":{"task":{"id":"tsk_1""#,
            b"",
        ] {
            assert_eq!(
                member_task_id(none),
                None,
                "{}",
                String::from_utf8_lossy(none)
            );
        }
    }

    #[test]
    fn a_member_request_recorder_dropped_unfinished_records_connection_failed_once() {
        let requests = MemberRequests::default();
        let recorder = MemberRequestRecorder::begin(
            Some(&scope(&requests)),
            "mrq_1",
            Some("coder"),
            MemberRequestVia::WasiHttp,
            Instant::now(),
        );
        drop(recorder);
        let lines = requests.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].outcome,
            MemberRequestOutcome::Failed(MemberRequestFailure::ConnectionFailed)
        );
        assert_eq!(lines[0].task_id.as_deref(), Some("tsk_a"));
        assert!(requests.take().is_empty());
    }

    #[test]
    fn a_member_request_body_forwards_every_frame_and_records_sent_once_at_its_end() {
        use http_body_util::{BodyExt, Full};
        let requests = MemberRequests::default();
        let recorder = MemberRequestRecorder::begin(
            Some(&scope(&requests)),
            "mrq_2",
            Some("coder"),
            MemberRequestVia::WasiHttp,
            Instant::now(),
        );
        let answer = r#"{"result":{"task":{"id":"tsk_9"}}}"#;
        let body = MemberRequestBody::new(Full::new(Bytes::from(answer)), recorder, 200);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let collected = rt.block_on(body.collect()).unwrap().to_bytes();
        assert_eq!(collected, Bytes::from(answer));
        let lines = requests.take();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].outcome, MemberRequestOutcome::Sent);
        assert_eq!(lines[0].http_status, Some(200));
        assert_eq!(lines[0].member_task_id.as_deref(), Some("tsk_9"));
    }

    #[test]
    fn a_member_request_body_keeps_at_most_the_bound() {
        use http_body_util::{BodyExt, Full};
        let requests = MemberRequests::default();
        let recorder = MemberRequestRecorder::begin(
            Some(&scope(&requests)),
            "mrq_3",
            None,
            MemberRequestVia::WasiHttp,
            Instant::now(),
        );
        let big = vec![b' '; RESPONSE_COPY_LIMIT * 2];
        let mut body = MemberRequestBody::new(Full::new(Bytes::from(big.clone())), recorder, 200);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let frame = rt.block_on(body.frame()).unwrap().unwrap();
        assert_eq!(frame.into_data().unwrap().len(), big.len());
        assert_eq!(body.copy.len(), RESPONSE_COPY_LIMIT);
        drop(body);
        assert_eq!(requests.take().len(), 1);
    }
}
