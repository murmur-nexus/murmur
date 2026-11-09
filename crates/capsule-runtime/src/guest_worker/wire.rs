//! The guest-worker wire format: one frame is a `u32` little-endian length followed by that many
//! bytes of a `postcard`-encoded message. Every message either side sends is one frame.

use std::io::{Read, Write};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::bindings::hook::exports::murmur::hook::lifecycle::{HookOutput, StageEvent};
use crate::bindings::hook::murmur::conversation::read::MessagePage;
use crate::bindings::hook::murmur::runtime::inference::{InferenceRequest, InferenceResponse};
use crate::bindings::hook::murmur::task_io::read::{IoError, TaskInputForm};
use crate::bindings::host::murmur::tool::run::{ToolInput, ToolResult};
use crate::hooks::{HookArg, HookSpec};
use crate::inference_import::HookInferencePurpose;
use crate::limits::CallDeadline;
use crate::process_driver::{DriverAnswer, DriverCall, DriverError};
use crate::runtime::{CapsuleCall, CapsuleReply, CapsuleRunError, CapsuleSpec, ToolCallSpec};
use crate::session_process::{ProcessCall, ProcessReply};

use super::WorkerKind;

/// The largest frame either side accepts. A length past it is refused before any buffer is
/// allocated for it, and the connection is treated as broken.
pub(crate) const MAX_FRAME_BYTES: u32 = 256 * 1024 * 1024;

/// A message the supervisor sends a worker.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ToWorker {
    /// The answer to [`FromWorker::Hello`]; no reply.
    Init { kind: WorkerKind },
    /// Hold `form`, a compiled component this binary serialized, as guest `guest`. Replied to
    /// with [`Reply::Loaded`].
    Load {
        req: u64,
        guest: u64,
        name: String,
        #[serde(with = "raw_bytes")]
        form: Vec<u8>,
    },
    /// Run guest `guest`'s `murmur:tool/run#run` with `input` in a store built from `spec`.
    /// `req` is also the call id every host call of this run names. Replied to with
    /// [`Reply::Tool`].
    RunTool {
        req: u64,
        guest: u64,
        spec: Box<ToolCallSpec>,
        input: ToolInput,
    },
    /// Instantiate guest `guest` as session-long hook instance `instance`, in a store built from
    /// `spec`. `req` is also the call id the instantiation's host calls name. Replied to with
    /// [`Reply::HookOpened`].
    HookOpen {
        req: u64,
        guest: u64,
        instance: u64,
        spec: Box<HookSpec>,
    },
    /// Call lifecycle function `fn_name` of hook instance `instance` with `arg`, `run-inference`
    /// calls made meanwhile being for `purpose`. `req` is also the call id. Replied to with
    /// [`Reply::Hook`].
    HookCall {
        req: u64,
        instance: u64,
        fn_name: String,
        arg: Box<HookArg>,
        purpose: HookInferencePurpose,
    },
    /// Call `on-stage` on a throwaway instance of guest `guest` built from `spec`. Replied to with
    /// [`Reply::Hook`].
    HookStage {
        req: u64,
        guest: u64,
        spec: Box<HookSpec>,
        event: StageEvent,
    },
    /// Drop hook instance `instance`; no reply.
    HookClose { instance: u64 },
    /// Instantiate guest `guest` as process driver instance `instance`. Replied to with
    /// [`Reply::Driver`].
    DriverOpen {
        req: u64,
        guest: u64,
        instance: u64,
        name: String,
        version: String,
    },
    /// Run `call` on process driver instance `instance`. Replied to with [`Reply::Driver`].
    DriverCall {
        req: u64,
        instance: u64,
        call: Box<DriverCall>,
    },
    /// Drop process driver instance `instance`; no reply.
    DriverClose { instance: u64 },
    /// Instantiate guest `guest` as the script capsule's root and call its
    /// `murmur:capsule/run#run`, in a store built from `spec`. `req` is also the call id.
    /// Replied to with [`Reply::Capsule`].
    RunCapsule {
        req: u64,
        guest: u64,
        spec: Box<CapsuleSpec>,
    },
    /// Drop the guest call `req`, as dropping its future in-process does; no reply.
    Cancel { req: u64 },
    /// The answer to the worker's [`FromWorker::HostCall`] `host`.
    HostReply { host: u64, reply: HostReply },
    /// The next bytes of the response body the supervisor streams as `stream`.
    BodyChunk {
        stream: u64,
        #[serde(with = "raw_bytes")]
        bytes: Vec<u8>,
    },
    /// The end of response body `stream`, with the error that ended it, if any.
    BodyEnd {
        stream: u64,
        error: Option<HttpError>,
    },
    /// End the worker; no reply.
    Shutdown,
}

/// A message a worker sends the supervisor.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum FromWorker {
    /// The first frame a worker sends.
    Hello {
        protocol: u32,
        version: String,
        pid: u32,
    },
    /// The answer to the supervisor's request `req`.
    Reply { req: u64, reply: Reply },
    /// A host import the guest of call `call` made, answered with [`ToWorker::HostReply`]
    /// `host`. `host` ids are issued by the worker, per connection.
    HostCall {
        host: u64,
        call: u64,
        request: HostRequest,
    },
    /// A `murmur:stream/events` call of call `call`; no reply. Applied in arrival order, before
    /// any reply that arrives after it.
    Event { call: u64, event: StreamEvent },
    /// Call `call`'s `run` is starting under a deadline of `deadline`; no reply. Starts the
    /// call's `murmur:process` deadline clock, as the in-process store does at the same point.
    RunStarted { call: u64, deadline: Duration },
}

/// A worker's answer to a request.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum Reply {
    Loaded(Result<(), String>),
    /// The run's result, and every wall-clock bound that fired in it, to be recorded in the
    /// session's latch before the result is used.
    Tool {
        result: Result<ToolResult, String>,
        deadlines: Vec<CallDeadline>,
    },
    /// The hook instance's exported lifecycle functions, by WIT name.
    HookOpened {
        result: Result<Vec<String>, String>,
        deadlines: Vec<CallDeadline>,
    },
    /// A lifecycle call's output: `None` when the instance does not export the function.
    Hook {
        result: Result<Option<HookOutput>, String>,
        deadlines: Vec<CallDeadline>,
    },
    Driver {
        result: Result<DriverAnswer, DriverError>,
        deadlines: Vec<CallDeadline>,
    },
    Capsule {
        result: Result<(), CapsuleRunError>,
        deadlines: Vec<CallDeadline>,
    },
}

impl Reply {
    /// Every wall-clock bound that fired in the call, to be recorded in the session's latch.
    pub(crate) fn take_deadlines(&mut self) -> Vec<CallDeadline> {
        match self {
            Self::Loaded(_) => Vec::new(),
            Self::Tool { deadlines, .. }
            | Self::HookOpened { deadlines, .. }
            | Self::Hook { deadlines, .. }
            | Self::Driver { deadlines, .. }
            | Self::Capsule { deadlines, .. } => std::mem::take(deadlines),
        }
    }
}

/// A host import the supervisor answers.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum HostRequest {
    /// A `wasi:http` outgoing request, body included.
    Http(HttpRequestHead),
    /// `murmur:task/task#request-input`.
    RequestInput { prompt: String },
    /// A `murmur:process` function.
    Process(ProcessCall),
    /// A hook's `murmur:runtime/inference#run-inference`.
    Inference {
        purpose: HookInferencePurpose,
        request: InferenceRequest,
    },
    /// A hook's `murmur:task-io/read` function.
    TaskIo(TaskIoCall),
    /// A hook's `murmur:conversation/read#read-messages`.
    Conversation { cursor: Option<String>, limit: u32 },
    /// A script capsule root's `murmur:tool-registry/invoke`, `murmur:artifact-manager/manage` or
    /// `murmur:message/send` function.
    Capsule(Box<CapsuleCall>),
}

/// One `murmur:task-io/read` function.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum TaskIoCall {
    InputLen(TaskInputForm),
    ReadInput(TaskInputForm, u64, u64),
    OutputLen,
    ReadOutput(u64, u64),
}

/// A `murmur:task-io/read` function's answer.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum TaskIoReply {
    Len(Result<u64, IoError>),
    Text(Result<String, IoError>),
}

/// The supervisor's answer to a [`HostRequest`].
#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum HostReply {
    /// The response head; its body follows as `BodyChunk`/`BodyEnd` frames under the host id.
    Http(Result<HttpResponseHead, HttpError>),
    Input(Result<String, String>),
    Process(ProcessReply),
    Inference(Result<InferenceResponse, String>),
    TaskIo(TaskIoReply),
    Conversation(Result<MessagePage, String>),
    Capsule(Box<CapsuleReply>),
    /// Not answered: the call named is not in flight on this connection.
    Refused(String),
}

/// One `murmur:stream/events` call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum StreamEvent {
    Chunk(String),
    ThinkingChunk(String),
    ToolCallStarted { id: String, name: String },
    ToolCallInputBytes { id: String, bytes: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HttpRequestHead {
    pub(crate) method: String,
    pub(crate) uri: String,
    pub(crate) headers: Vec<(String, Vec<u8>)>,
    pub(crate) connect_timeout: Option<Duration>,
    pub(crate) first_byte_timeout: Option<Duration>,
    pub(crate) between_bytes_timeout: Option<Duration>,
    pub(crate) has_options: bool,
    #[serde(with = "raw_bytes")]
    pub(crate) body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HttpResponseHead {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, Vec<u8>)>,
}

/// `wasmtime_wasi_http::Error` as a guest sees it: every variant whose guest-facing code is
/// fixed crosses as itself; the I/O-bearing variants cross as the code the guest would be shown.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum HttpError {
    HttpRequestBodySize(Option<u64>),
    DnsTimeout,
    DnsError {
        rcode: Option<String>,
        info_code: Option<u16>,
    },
    DestinationNotFound,
    DestinationUnavailable,
    DestinationIpProhibited,
    DestinationIpUnroutable,
    ConnectionRefused,
    ConnectionTerminated,
    ConnectionTimeout,
    ConnectionReadTimeout,
    ConnectionWriteTimeout,
    ConnectionLimitReached,
    TlsProtocolError,
    TlsCertificateError,
    TlsAlertReceived {
        alert_id: Option<u8>,
        alert_message: Option<String>,
    },
    HttpRequestDenied,
    HttpRequestLengthRequired,
    HttpRequestMethodInvalid,
    HttpRequestUriInvalid,
    HttpRequestUriTooLong,
    HttpRequestHeaderSectionSize(Option<u32>),
    HttpRequestHeaderSize {
        field_name: Option<String>,
        field_size: Option<u32>,
    },
    HttpRequestTrailerSectionSize(Option<u32>),
    HttpRequestTrailerSize {
        field_name: Option<String>,
        field_size: Option<u32>,
    },
    HttpResponseIncomplete,
    HttpResponseHeaderSectionSize(Option<u32>),
    HttpResponseHeaderSize {
        field_name: Option<String>,
        field_size: Option<u32>,
    },
    HttpResponseBodySize(Option<u64>),
    HttpResponseTrailerSectionSize(Option<u32>),
    HttpResponseTrailerSize {
        field_name: Option<String>,
        field_size: Option<u32>,
    },
    HttpResponseTransferCoding(Option<String>),
    HttpResponseContentCoding(Option<String>),
    HttpResponseTimeout,
    HttpUpgradeFailed,
    HttpProtocolError,
    LoopDetected,
    ConfigurationError,
    InternalError(Option<String>),
}

/// A byte buffer as one length-prefixed run of bytes on the wire. Serde's default for a
/// `Vec<u8>` is a sequence of `u8`s, which postcard encodes and decodes one element at a time: a
/// serialized component of several megabytes would cost tens of milliseconds each way.
mod raw_bytes {
    use serde::de::{Deserializer, Error, Visitor};
    use serde::Serializer;

    pub(crate) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(bytes)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        struct Bytes;
        impl<'de> Visitor<'de> for Bytes {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a byte buffer")
            }

            fn visit_bytes<E: Error>(self, bytes: &[u8]) -> Result<Vec<u8>, E> {
                Ok(bytes.to_vec())
            }

            fn visit_byte_buf<E: Error>(self, bytes: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(bytes)
            }
        }
        deserializer.deserialize_byte_buf(Bytes)
    }
}

/// Why a frame could not be read.
#[derive(Debug)]
pub(crate) enum FrameError {
    /// The peer closed the connection at a frame boundary.
    Closed,
    Io(std::io::Error),
    /// The length prefix named more than [`MAX_FRAME_BYTES`].
    Oversize(u32),
    Decode(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "the connection closed"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Oversize(len) => write!(
                f,
                "a frame of {len} bytes exceeds the {MAX_FRAME_BYTES}-byte limit"
            ),
            Self::Decode(error) => write!(f, "a frame did not decode: {error}"),
        }
    }
}

/// Encodes `message` as one frame.
pub(crate) fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, String> {
    let body = postcard::to_stdvec(message).map_err(|error| error.to_string())?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= MAX_FRAME_BYTES)
        .ok_or_else(|| format!("a {}-byte frame exceeds the limit", body.len()))?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes `message` as one frame.
pub(crate) fn write_frame<T: Serialize>(
    writer: &mut impl Write,
    message: &T,
) -> Result<(), String> {
    let frame = encode(message)?;
    writer
        .write_all(&frame)
        .map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

/// Reads one frame and decodes it. The length is checked against [`MAX_FRAME_BYTES`] before the
/// body's buffer is allocated.
pub(crate) fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut impl Read,
) -> Result<T, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match reader.read(&mut len[got..]) {
            Ok(0) if got == 0 => return Err(FrameError::Closed),
            Ok(0) => {
                return Err(FrameError::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )))
            }
            Ok(read) => got += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::Oversize(len));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).map_err(FrameError::Io)?;
    postcard::from_bytes(&body).map_err(|error| FrameError::Decode(error.to_string()))
}
