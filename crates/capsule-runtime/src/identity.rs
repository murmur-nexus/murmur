use std::sync::{Arc, Mutex};

use murmur_artifact::{ConversationMode, NetworkAuthentication, TaskAcceptance};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::a2a::{
    read_send_params, A2aError, A2aTask, CancelOutcome, IncomingMessage, IncomingTask,
    JsonRpcRequest, JsonRpcResponse, MurmurError, Part, Role, TaskRegistry, TaskState, TaskStatus,
    DATA_MEDIA_TYPE, INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND, TEXT_MEDIA_TYPE,
};
use crate::cancel::{Arrival, LiveDelegations, Residue};
use crate::control_plane::{handle_control_request, is_control_path, ControlPlane, ControlRequest};
use crate::delegation::{COMPLETION_SESSION_HEADER, DELEGATION_ID_HEADER};
use crate::detached::DetachedRegistry;
use crate::errors::RuntimeError;
use crate::origin::{self, TaskOrigin, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
use crate::peer_handoff::{handle_peer_request, is_peer_path, PeerPlane, AUDIENCE_HEADER};
use crate::peer_tasks;
use crate::resource_plane::{
    handle_resource_request, reason_phrase, ResourcePlane, ResourceResponse, RESOURCE_PATH_PREFIX,
};
use crate::streaming::{
    admit_live_frame, format_gap_event, format_lagged_event, format_unnumbered_sse_event, frame_id,
    is_final_status_for, ReplayResult, SseBroadcast, SseEventBuffer, StreamFrame, StreamStatus,
    TaskStatusUpdateEvent, SSE_HEARTBEAT_COMMENT, SSE_HEARTBEAT_INTERVAL,
};
use crate::types::{CapabilityPolicy, InstalledArtifactSummary};

pub(crate) struct CapsuleIdentity {
    pub capsule_name: String,
    pub capsule_version: String,
    pub session_id: String,
    pub capsule_url: String,
}

/// Bind a TCP listener on the given address.
///
/// When `internal_port` is `Some(p)`, binds strictly to `{addr}:{p}` and returns
/// [`RuntimeError::PortInUse`] if that port is already taken.  When `None`, binds
/// to `{addr}:0` and lets the OS assign a port.
///
pub(crate) async fn bind_local_port(
    addr: &str,
    internal_port: Option<u16>,
) -> Result<(TcpListener, u16), RuntimeError> {
    let port = internal_port.unwrap_or(0);
    match TcpListener::bind(format!("{addr}:{port}")).await {
        Ok(listener) => {
            let bound_port = listener
                .local_addr()
                .map_err(|e| RuntimeError::Runtime(format!("failed to read bound port: {e}")))?
                .port();
            Ok((listener, bound_port))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && internal_port.is_some() => {
            Err(RuntimeError::PortInUse {
                port: internal_port.unwrap(),
            })
        }
        Err(e) => Err(RuntimeError::Runtime(format!(
            "failed to bind agent-card port: {e}"
        ))),
    }
}

/// A JSON-RPC method the door at `POST /` answers.
///
/// This is the door's whole method table: dispatch resolves a request's `method` through
/// [`DoorMethod::resolve`], and the method list on the agent card's door extension is
/// [`served_methods`], which asks the same resolver. Adding a method means adding a variant, its
/// wire name and its `ALL` entry; the handler arm is then demanded by the exhaustive matches in the
/// dispatcher, and the card lists it without further change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoorMethod {
    SendMessage,
    SendStreamingMessage,
    StreamWatch,
    GetTask,
    CancelTask,
    SessionStop,
    GetExtendedAgentCard,
}

impl DoorMethod {
    /// Every method, in the order the card lists them.
    pub(crate) const ALL: [DoorMethod; 7] = [
        DoorMethod::SendMessage,
        DoorMethod::SendStreamingMessage,
        DoorMethod::StreamWatch,
        DoorMethod::GetTask,
        DoorMethod::CancelTask,
        DoorMethod::SessionStop,
        DoorMethod::GetExtendedAgentCard,
    ];

    /// The method's name on the wire: the A2A v1.0 name of an A2A method, and murmur's own name of
    /// `stream/watch` and `session/stop`.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            DoorMethod::SendMessage => "SendMessage",
            DoorMethod::SendStreamingMessage => "SendStreamingMessage",
            DoorMethod::StreamWatch => "stream/watch",
            DoorMethod::GetTask => "GetTask",
            DoorMethod::CancelTask => "CancelTask",
            DoorMethod::SessionStop => "session/stop",
            DoorMethod::GetExtendedAgentCard => "GetExtendedAgentCard",
        }
    }

    /// The door-token scope a caller must hold to call this method on an authenticated door: its
    /// wire name, one of [`murmur_artifact::DOOR_SCOPES`]. `None` for `GetExtendedAgentCard`,
    /// which every authenticated caller may call.
    pub(crate) fn scope(self) -> Option<&'static str> {
        match self {
            DoorMethod::GetExtendedAgentCard => None,
            method => Some(method.wire_name()),
        }
    }

    /// The method this door answers for a request's `method` string, or `None` when it answers
    /// `-32601`.
    ///
    /// The only place a request's method is interpreted and the only place
    /// `lifecycle.task_acceptance` gates one: under `TaskAcceptance::None` neither task-starting
    /// method is served. `GetExtendedAgentCard` is served only by an `authenticated` door, the only
    /// kind with an extended card. The match is exact — no case folding, no trimming, no other
    /// spelling — so a name the card lists is the name to send.
    pub(crate) fn resolve(
        method: &str,
        acceptance: &TaskAcceptance,
        authenticated: bool,
    ) -> Option<DoorMethod> {
        let resolved = Self::ALL.into_iter().find(|m| m.wire_name() == method)?;
        match (resolved, acceptance) {
            (DoorMethod::SendMessage | DoorMethod::SendStreamingMessage, TaskAcceptance::None) => {
                None
            }
            (DoorMethod::GetExtendedAgentCard, _) if !authenticated => None,
            _ => Some(resolved),
        }
    }
}

/// The wire names of every method [`DoorMethod::resolve`] serves under `acceptance` on a door that
/// is or is not `authenticated`, in `ALL` order.
pub(crate) fn served_methods(
    acceptance: &TaskAcceptance,
    authenticated: bool,
) -> Vec<&'static str> {
    DoorMethod::ALL
        .into_iter()
        .filter(|m| DoorMethod::resolve(m.wire_name(), acceptance, authenticated).is_some())
        .map(DoorMethod::wire_name)
        .collect()
}

/// Which HTTP planes the manifest declares. An undeclared plane is still routed, but answers only
/// refusals, so the card lists a plane only when it is declared here.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DeclaredPlanes {
    /// `exports.files` is declared: the operator plane under `/resources/files` serves content.
    pub files: bool,
    /// `exports.peer_files` is declared: the peer plane under `/resources/peer/{handle}` serves
    /// content.
    pub peer_files: bool,
}

/// Which family of inference transport a session runs, as far as the frames on its stream go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportKind {
    /// Every session but `inference.transport: process`: the runtime's own agent loop, including
    /// a session with no `inference:` block.
    Http,
    /// `inference.transport: process`, whose frames come from the harness events its driver reads.
    Process,
}

impl TransportKind {
    /// Every transport family.
    pub(crate) const ALL: [TransportKind; 2] = [TransportKind::Http, TransportKind::Process];
}

/// What the capsule's inference transport can actually do, beyond what the served method list
/// already says: whether the card claims `capabilities.streaming`, and which frames the stream
/// extension lists. Every transport can be stopped, so cancellation is not here: `CancelTask` is
/// served under every acceptance.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportCapabilities {
    /// Whether the transport emits streaming text frames.
    pub streams_text: bool,
    /// The transport family, which decides the frames [`served_frames`] lists.
    pub kind: TransportKind,
}

/// Whether a connection made with `method` can receive `frame` from a session on `transport`.
///
/// Each frame's arm states which streaming methods carry it and which transports write it. Only
/// `SendStreamingMessage` and `stream/watch` write frames; `stream/watch` alone writes the
/// observer's `connection-ack` and `capsule-closed`, and `SendStreamingMessage` alone answers a
/// request it cannot take with `error`.
pub(crate) fn writes_frame(
    method: DoorMethod,
    frame: StreamFrame,
    transport: TransportKind,
) -> bool {
    let (on_message_stream, on_stream_watch, transports): (bool, bool, &[TransportKind]) =
        match frame {
            StreamFrame::Status
            | StreamFrame::Artifact
            | StreamFrame::Text
            | StreamFrame::Thinking
            | StreamFrame::ToolCallStarted
            | StreamFrame::ToolCallProgress
            | StreamFrame::Gap
            | StreamFrame::Lagged => (true, true, &TransportKind::ALL),
            StreamFrame::ConnectionAck | StreamFrame::CapsuleClosed => {
                (false, true, &TransportKind::ALL)
            }
            StreamFrame::Error => (true, false, &TransportKind::ALL),
        };
    let carried = match method {
        DoorMethod::SendStreamingMessage => on_message_stream,
        DoorMethod::StreamWatch => on_stream_watch,
        DoorMethod::SendMessage
        | DoorMethod::GetTask
        | DoorMethod::CancelTask
        | DoorMethod::SessionStop
        | DoorMethod::GetExtendedAgentCard => false,
    };
    carried && transports.contains(&transport)
}

/// The wire names of every frame a connection to this door can receive, in `StreamFrame::ALL`
/// order: a frame is listed when a streaming method the door serves under `acceptance` carries
/// it and `transport` writes it.
///
/// Whether the door authenticates never changes which streaming methods it serves, so the list is
/// the same on the public and the extended card.
pub(crate) fn served_frames(
    acceptance: &TaskAcceptance,
    transport: TransportCapabilities,
) -> Vec<&'static str> {
    let methods: Vec<DoorMethod> = DoorMethod::ALL
        .into_iter()
        .filter(|m| DoorMethod::resolve(m.wire_name(), acceptance, false).is_some())
        .collect();
    StreamFrame::ALL
        .into_iter()
        .filter(|&frame| {
            methods
                .iter()
                .any(|&method| writes_frame(method, frame, transport.kind))
        })
        .map(StreamFrame::wire_name)
        .collect()
}

/// Asks the capsule to drop the harness session the request's context names before the turn it
/// starts, so that turn opens a new conversation under the same context id.
///
/// Carried on the request that starts a turn rather than served as a method of its own, because
/// that is where the context, the trace and the session plan already are: the door itself writes
/// no trace and holds no map. Only the value `true` asks for it — every other value, and the
/// header's absence, are the same request.
///
/// Refused with `-32602` by a capsule that keeps no harness session, which is every transport but
/// `inference.transport: process`.
pub(crate) const FORGET_SESSION_HEADER: &str = "x-murmur-forget-session";

/// What a door declaring `network.authentication` holds: the session's key and tokens, the realm
/// its challenges name, and the extended card an authenticated caller may read.
pub(crate) struct DoorGate {
    pub auth: Arc<crate::door_auth::DoorAuth>,
    /// The capsule name.
    pub realm: String,
    /// The extended card, from [`build_agent_cards`].
    pub extended_card: Value,
}

/// URI of the agent-card extension that lists every JSON-RPC method the door answers, murmur's
/// own methods among them. It is the address of that extension's section in the reference docs.
pub(crate) const DOOR_EXTENSION_URI: &str =
    murmur_artifact::docs_reference_url!("agent-card/#murmur-door-v1");

/// URI of the agent-card extension that carries the extended-card material: the session id and
/// what the capsule may do. It is the address of that extension's section in the reference docs.
pub(crate) const CAPSULE_EXTENSION_URI: &str =
    murmur_artifact::docs_reference_url!("agent-card/#murmur-capsule-v1");

/// URI of the agent-card extension that lists every SSE event type the capsule's stream can write.
/// It is the address of that extension's section in the reference docs, on the streaming protocol
/// page beside the frames it names.
pub(crate) const STREAM_EXTENSION_URI: &str =
    murmur_artifact::docs_reference_url!("streaming-protocol/#murmur-stream-v1");

/// What the door extension says about itself.
const DOOR_EXTENSION_DESCRIPTION: &str = "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.";

/// What the stream extension says about itself.
const STREAM_EXTENSION_DESCRIPTION: &str = "Every server-sent event type this capsule's SendStreamingMessage and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.";

/// The id of the one skill a door that starts tasks advertises: running a task.
pub(crate) const TASK_SKILL_ID: &str = "task";

/// The A2A protocol version the card's JSON-RPC interface declares: the one version the door
/// negotiates.
pub(crate) const INTERFACE_PROTOCOL_VERSION: &str = crate::a2a::A2A_PROTOCOL_VERSION;

/// The `protocolBinding` of the door's interface.
const JSONRPC_BINDING: &str = "JSONRPC";

/// The media types of the parts the door reads and writes: text, and JSON data.
const MEDIA_MODES: [&str; 2] = [TEXT_MEDIA_TYPE, DATA_MEDIA_TYPE];

/// Build the A2A v1.0 `AgentCard` the door serves at `/.well-known/agent-card.json`.
///
/// Every card this returns parses as `lf.a2a.v1.AgentCard` under a strict protobuf JSON parser.
/// It declares one `JSONRPC` interface at [`INTERFACE_PROTOCOL_VERSION`], whose `url` is
/// `capsule_url` with the `http://` scheme the door speaks. `securitySchemes` and
/// `securityRequirements` are present and empty, which declares a public agent.
///
/// `capabilities.extensions` holds three murmur extensions, in this order:
///
/// - [`DOOR_EXTENSION_URI`], whose `params.methods` is [`served_methods`] for the acceptance the
///   door is given, so the card cannot list a method the dispatcher refuses or omit one it serves,
///   and whose `params.peerTasks` is `accepts_peer_tasks`, always present, so a peer capsule
///   learns whether the door serves it before it sends a task.
/// - [`CAPSULE_EXTENSION_URI`], whose `params` are `sessionId`, `tools`, `shell`, `network` and
///   `planes`. This object is all of the card's extended-card material and none of it appears
///   anywhere else, so it can move to an extended card whole; the card conforms without it.
///   `sessionId` is served because the card is how a caller confirms that the capsule answering an
///   address is the session it went looking for. `planes` lists `files` then `peer_files`, each
///   only when declared.
/// - [`STREAM_EXTENSION_URI`], whose `params.frames` is [`served_frames`] for the acceptance and
///   the transport. It describes the stream protocol rather than the session, so it stays on the
///   public card of an authenticated door.
///
/// `capabilities.streaming` is read off the served methods and the transport together: `true`
/// when `SendStreamingMessage` is served and the transport streams text. A door that answers a method
/// whose effect its transport cannot deliver still lists the method and says so here.
///
/// `skills` is the one [`TASK_SKILL_ID`] skill when the door serves `SendMessage`, and empty
/// otherwise. Installed tools are not skills: a caller cannot invoke one directly.
pub(crate) fn build_agent_card(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
    task_acceptance: &TaskAcceptance,
    planes: DeclaredPlanes,
    accepts_peer_tasks: bool,
    transport: TransportCapabilities,
) -> serde_json::Value {
    let tools: Vec<&str> = installed_artifacts
        .iter()
        .filter(|a| a.runtime.is_llm_visible())
        .map(|a| a.name.as_str())
        .collect();

    let methods = served_methods(task_acceptance, false);
    let streaming =
        methods.contains(&DoorMethod::SendStreamingMessage.wire_name()) && transport.streams_text;
    let declared_planes: Vec<&str> = [(planes.files, "files"), (planes.peer_files, "peer_files")]
        .into_iter()
        .filter_map(|(declared, name)| declared.then_some(name))
        .collect();
    let skills: Vec<Value> = if methods.contains(&DoorMethod::SendMessage.wire_name()) {
        vec![serde_json::json!({
            "id": TASK_SKILL_ID,
            "name": "Run a task",
            "description": "Runs one task given as a text message and reports its outcome.",
            "tags": [TASK_SKILL_ID],
        })]
    } else {
        Vec::new()
    };

    serde_json::json!({
        "name": identity.capsule_name,
        "description": format!(
            "Murmur capsule {} {}",
            identity.capsule_name, identity.capsule_version
        ),
        "version": identity.capsule_version,
        "supportedInterfaces": [{
            "url": interface_url(&identity.capsule_url),
            "protocolBinding": JSONRPC_BINDING,
            "protocolVersion": INTERFACE_PROTOCOL_VERSION,
        }],
        "capabilities": {
            "streaming": streaming,
            "pushNotifications": false,
            "extendedAgentCard": false,
            "extensions": [
                {
                    "uri": DOOR_EXTENSION_URI,
                    "description": DOOR_EXTENSION_DESCRIPTION,
                    "required": false,
                    "params": { "methods": methods, "peerTasks": accepts_peer_tasks },
                },
                {
                    "uri": CAPSULE_EXTENSION_URI,
                    "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.",
                    "required": false,
                    "params": {
                        "sessionId": identity.session_id,
                        "tools": tools,
                        "shell": !capability_policy.shell_allow.is_empty(),
                        "network": !capability_policy.network_allow.is_empty(),
                        "planes": declared_planes,
                    },
                },
                {
                    "uri": STREAM_EXTENSION_URI,
                    "description": STREAM_EXTENSION_DESCRIPTION,
                    "required": false,
                    "params": { "frames": served_frames(task_acceptance, transport) },
                },
            ],
        },
        "securitySchemes": {},
        "securityRequirements": [],
        "defaultInputModes": MEDIA_MODES,
        "defaultOutputModes": MEDIA_MODES,
        "skills": skills,
    })
}

/// `capsule_url` as an absolute URL: the door speaks plain HTTP, so a bare `host:port` gains
/// `http://`. A trailing `/` is dropped.
fn interface_url(capsule_url: &str) -> String {
    let url = capsule_url.trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("http://{url}")
    }
}

/// The name the card gives the door's one security scheme.
pub(crate) const BEARER_SCHEME_NAME: &str = "bearer";

/// What the bearer scheme says about the token it takes.
const BEARER_SCHEME_DESCRIPTION: &str =
    "A token this capsule's runtime mints at launch and accepts until the session ends.";

/// The cards a door serves: the public card at `/.well-known/agent-card.json`, and the extended
/// card `GetExtendedAgentCard` returns when the door authenticates.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentCards {
    pub public: Value,
    pub extended: Option<Value>,
}

/// Build the cards of a door that does or does not declare `network.authentication`.
///
/// With `authentication` `None` the public card is exactly [`build_agent_card`]'s and there is no
/// extended card. With it declared, the v1.0 card [`build_agent_card`] returns gains
/// `capabilities.extendedAgentCard: true`, the [`BEARER_SCHEME_NAME`] HTTP scheme with a
/// requirement any valid token meets, `GetExtendedAgentCard` on the door extension's methods, and
/// one alternative requirement per served task-starting method on the [`TASK_SKILL_ID`] skill.
/// The extended card is that card, whole. The public card is that card without the capsule
/// extension, so it carries the door and stream extensions.
// Each argument feeds its own part of the card, and all but `authentication` pass straight through
// to `build_agent_card`; a wrapper struct would name the argument count rather than a concept.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_agent_cards(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
    task_acceptance: &TaskAcceptance,
    planes: DeclaredPlanes,
    accepts_peer_tasks: bool,
    transport: TransportCapabilities,
    authentication: Option<&NetworkAuthentication>,
) -> AgentCards {
    let card = build_agent_card(
        identity,
        installed_artifacts,
        capability_policy,
        task_acceptance,
        planes,
        accepts_peer_tasks,
        transport,
    );
    if authentication.is_none() {
        return AgentCards {
            public: card,
            extended: None,
        };
    }

    let mut card = authenticated_card(card, task_acceptance);
    let extended = card.clone();
    if let Some(extensions) = card["capabilities"]["extensions"].as_array_mut() {
        extensions.retain(|extension| extension["uri"] != CAPSULE_EXTENSION_URI);
    }
    AgentCards {
        public: card,
        extended: Some(extended),
    }
}

/// The v1.0 extended card of an authenticated door: `card`, a [`build_agent_card`] output for
/// `task_acceptance`, with the bearer scheme and requirements the door enforces.
fn authenticated_card(mut card: Value, task_acceptance: &TaskAcceptance) -> Value {
    let methods = served_methods(task_acceptance, true);
    card["capabilities"]["extendedAgentCard"] = Value::Bool(true);
    card["securitySchemes"] = serde_json::json!({
        BEARER_SCHEME_NAME: {
            "httpAuthSecurityScheme": {
                "scheme": "Bearer",
                "description": BEARER_SCHEME_DESCRIPTION,
            },
        },
    });
    card["securityRequirements"] = serde_json::json!([bearer_requirement(&[])]);
    if let Some(extensions) = card["capabilities"]["extensions"].as_array_mut() {
        for extension in extensions.iter_mut() {
            if extension["uri"] == DOOR_EXTENSION_URI {
                extension["params"]["methods"] = serde_json::json!(methods);
            }
        }
    }
    let task_requirements: Vec<Value> = [DoorMethod::SendMessage, DoorMethod::SendStreamingMessage]
        .into_iter()
        .map(DoorMethod::wire_name)
        .filter(|method| methods.contains(method))
        .map(|method| bearer_requirement(&[method]))
        .collect();
    if let Some(skills) = card["skills"].as_array_mut() {
        for skill in skills.iter_mut() {
            if skill["id"] == TASK_SKILL_ID {
                skill["securityRequirements"] = Value::Array(task_requirements.clone());
            }
        }
    }
    card
}

/// One v1.0 `SecurityRequirement` naming the bearer scheme with `scopes`.
fn bearer_requirement(scopes: &[&str]) -> Value {
    serde_json::json!({"schemes": {BEARER_SCHEME_NAME: {"list": scopes}}})
}

/// The `params` object of the extension in `card.capabilities.extensions` whose `uri` is `uri`.
///
/// This and the two readers below are the only code that knows where a card keeps what murmur
/// reads back out of it.
pub(crate) fn extension_params<'a>(
    card: &'a Value,
    uri: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    card.get("capabilities")?
        .get("extensions")?
        .as_array()?
        .iter()
        .find(|extension| extension.get("uri").and_then(Value::as_str) == Some(uri))?
        .get("params")?
        .as_object()
}

/// The non-empty `sessionId` the card's capsule extension names, or `None` for a card that has no
/// capsule extension or names no session.
pub(crate) fn session_id_from_card(card: &Value) -> Option<&str> {
    extension_params(card, CAPSULE_EXTENSION_URI)?
        .get("sessionId")?
        .as_str()
        .filter(|session_id| !session_id.is_empty())
}

/// The non-empty `url` of the first `supportedInterfaces` entry whose `protocolBinding` is
/// `JSONRPC`: the address the door answers JSON-RPC on.
pub(crate) fn jsonrpc_interface_url(card: &Value) -> Option<&str> {
    card.get("supportedInterfaces")?
        .as_array()?
        .iter()
        .find(|interface| {
            interface.get("protocolBinding").and_then(Value::as_str) == Some(JSONRPC_BINDING)
        })?
        .get("url")?
        .as_str()
        .filter(|url| !url.is_empty())
}

/// Serve the agent-card endpoint and A2A JSON-RPC endpoints until shutdown.
///
/// Spawned onto the session runtime's worker pool, and each accepted connection is handled in
/// its own task on that pool, so the door answers while the thread running the agent loop is
/// held by synchronous work inside a turn. No handler waits on the agent loop.
///
/// Returns once `shutdown_rx` fires or its sender is dropped, having closed the listener and
/// ended every connection still open, so the door closes with the session rather than with the
/// runtime. An accept error closes the listener but leaves open connections served until then.
///
/// A streaming connection is not cut off mid-queue: the session's last frames — a task's final
/// status among them — are broadcast just before the shutdown signal, and a handler not yet
/// scheduled to write them would otherwise lose them. Each one is told the door is closing, writes
/// what it has already been sent and returns; [`CONNECTION_DRAIN_GRACE`] bounds a connection that
/// cannot, such as one whose client stopped reading.
// Everything after the listener and the shutdown channel is A2A server state that is cloned
// once per accepted connection and handed to `handle_connection` unchanged. A wrapper struct
// would name the argument count rather than a concept.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_http(
    listener: TcpListener,
    mut shutdown_rx: oneshot::Receiver<()>,
    card_json: String,
    task_registry: Arc<Mutex<TaskRegistry>>,
    task_tx: mpsc::Sender<IncomingTask>,
    task_acceptance: TaskAcceptance,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode: ConversationMode,
    resource_plane: Arc<ResourcePlane>,
    peer_plane: Arc<PeerPlane>,
    control_plane: Arc<ControlPlane>,
    // This capsule's own session id. The door refuses a completion addressed to any other
    // session, which is what stops a child's outcome landing on whatever session answers the
    // parent's old address after a restart.
    session_id: String,
    // The two registries a cancel snapshots its residue from. `detached` is `None` for a launch
    // that demotes nothing, which contributes no shell items rather than an empty set. The
    // delegation set is also where every completion is handed.
    detached: Option<Arc<DetachedRegistry>>,
    live_delegations: Arc<LiveDelegations>,
    // Whether this capsule has a harness session to forget, which is the one thing
    // `FORGET_SESSION_HEADER` needs to know about the transport behind the door.
    forgettable_session: bool,
    // The manifest's own `exports.peer_tasks.accept`. `false` refuses every peer-origin request
    // past the token check with `peer_tasks::refusal`.
    accepts_peer_tasks: bool,
    // `Some` when the manifest declares `network.authentication`: every request but the public
    // card and the two planes with their own authorisers must present one of its tokens.
    gate: Option<Arc<DoorGate>>,
) {
    let conversation_mode_str = match conversation_mode {
        ConversationMode::Stateless => "stateless",
        ConversationMode::Threaded => "threaded",
    };
    let mut connections = tokio::task::JoinSet::new();
    let (closing_tx, closing_rx) = watch::channel(false);
    let accept_failed = loop {
        tokio::select! {
            _ = &mut shutdown_rx => break false,
            // Reaps finished connections, which a `JoinSet` otherwise keeps until joined.
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            result = listener.accept() => {
                match result {
                    Ok((stream, peer_addr)) => {
                        let card = card_json.clone();
                        let registry = Arc::clone(&task_registry);
                        let tx = task_tx.clone();
                        let acceptance = task_acceptance.clone();
                        let sse = sse_tx.clone();
                        let buf = Arc::clone(&sse_buffer);
                        let mode_str = conversation_mode_str.to_string();
                        let plane = Arc::clone(&resource_plane);
                        let peer = Arc::clone(&peer_plane);
                        let control = Arc::clone(&control_plane);
                        let session = session_id.clone();
                        let detached_for_conn = detached.clone();
                        let live = Arc::clone(&live_delegations);
                        let closing = closing_rx.clone();
                        let gate_for_conn = gate.clone();
                        connections.spawn(async move {
                            handle_connection(stream, peer_addr, card, registry, tx, acceptance, sse, buf, mode_str, plane, peer, control, session, detached_for_conn, live, forgettable_session, accepts_peer_tasks, gate_for_conn, closing).await;
                        });
                    }
                    Err(e) => {
                        crate::runtime_err!("[capsule-runtime] HTTP accept error: {e}");
                        break true;
                    }
                }
            }
        }
    };
    drop(listener);
    if accept_failed {
        let _ = shutdown_rx.await;
    }
    // A `stream/watch` or `SendStreamingMessage` connection otherwise outlives the session: its
    // handler holds a broadcast sender, so it never sees the channel close.
    let _ = closing_tx.send(true);
    let drained = async { while connections.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(CONNECTION_DRAIN_GRACE, drained).await;
    connections.shutdown().await;
}

/// A real door for in-crate tests, served through [`serve_http`] with `accepts_peer_tasks` as
/// given, no authentication and no planes declared, on an ephemeral loopback port. Returns its
/// `host:port`, the sender whose drop closes it, and the receiving end of its task channel.
#[cfg(test)]
pub(crate) async fn serve_test_door(
    accepts_peer_tasks: bool,
) -> (String, oneshot::Sender<()>, mpsc::Receiver<IncomingTask>) {
    serve_test_door_with(
        TaskAcceptance::Queue,
        Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue))),
        Arc::new(LiveDelegations::new()),
        accepts_peer_tasks,
    )
    .await
}

/// [`serve_test_door`] under `acceptance`, over the given registry and delegation set, for session
/// `ses_door`.
#[cfg(test)]
pub(crate) async fn serve_test_door_with(
    acceptance: TaskAcceptance,
    task_registry: Arc<Mutex<TaskRegistry>>,
    live_delegations: Arc<LiveDelegations>,
    accepts_peer_tasks: bool,
) -> (String, oneshot::Sender<()>, mpsc::Receiver<IncomingTask>) {
    use std::sync::atomic::AtomicU64;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind an ephemeral port");
    let addr = listener.local_addr().unwrap().to_string();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (task_tx, task_rx) = mpsc::channel(4);
    let (sse_tx, _) = tokio::sync::broadcast::channel(4);
    let session_id = "ses_door".to_string();
    let containment = murmur_artifact::ContainmentClass::Advisory;
    let nowhere = std::path::Path::new("/nonexistent");
    tokio::spawn(serve_http(
        listener,
        shutdown_rx,
        "{}".to_string(),
        task_registry,
        task_tx,
        acceptance,
        sse_tx,
        Arc::new(Mutex::new(SseEventBuffer::new(8))),
        ConversationMode::Stateless,
        Arc::new(ResourcePlane::new(
            nowhere,
            None,
            containment,
            Arc::new(AtomicU64::new(0)),
            None,
        )),
        Arc::new(PeerPlane::new(
            nowhere,
            None,
            session_id.clone(),
            containment,
            Arc::new(AtomicU64::new(0)),
            None,
        )),
        Arc::new(ControlPlane::undeclared(session_id.clone())),
        session_id,
        None,
        live_delegations,
        false,
        accepts_peer_tasks,
        None,
    ));
    (addr, shutdown_tx, task_rx)
}

/// How long a closing door waits for its open connections to finish before ending them.
///
/// A connection that drains normally is gone within one scheduling round, so this is paid only by
/// a connection that cannot finish: a client that stopped reading, or one that connected and
/// never sent its request.
const CONNECTION_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Resolves once the door is closing. The handler's receive loop polls this after the broadcast
/// receiver, so it fires only when every frame already sent to the connection has been taken.
async fn door_closing(closing: &mut watch::Receiver<bool>) {
    // A dropped sender means `serve_http` itself is gone, which closes the door just the same.
    let _ = closing.wait_for(|&closing| closing).await;
}

// Receives `serve_http`'s state verbatim and splits it across the three request handlers; see
// the note on `serve_http` for why it is not bundled.
#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    stream: tokio::net::TcpStream,
    // Read by the control plane alone, which accepts a secret only from a loopback peer.
    peer_addr: std::net::SocketAddr,
    card_json: String,
    task_registry: Arc<Mutex<TaskRegistry>>,
    task_tx: mpsc::Sender<IncomingTask>,
    task_acceptance: TaskAcceptance,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode_str: String,
    resource_plane: Arc<ResourcePlane>,
    peer_plane: Arc<PeerPlane>,
    control_plane: Arc<ControlPlane>,
    session_id: String,
    detached: Option<Arc<DetachedRegistry>>,
    live_delegations: Arc<LiveDelegations>,
    forgettable_session: bool,
    accepts_peer_tasks: bool,
    gate: Option<Arc<DoorGate>>,
    closing: watch::Receiver<bool>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = BufReader::new(reader_half);

    // Read request line
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await.is_err() {
        return;
    }

    let mut parts_iter = request_line.split_whitespace();
    let method = parts_iter.next().unwrap_or("").to_string();
    let path = parts_iter.next().unwrap_or("").to_string();

    // Read all headers
    let mut content_length: usize = 0;
    let mut is_json = false;
    let mut traceparent: Option<String> = None;
    let mut last_event_id: Option<u64> = None;
    let mut audience: Option<String> = None;
    let mut task_origin: Option<String> = None;
    let mut task_trust: Option<String> = None;
    let mut delegation_id: Option<String> = None;
    let mut completion_session: Option<String> = None;
    let mut forget_session = false;
    let mut authorization: Option<String> = None;
    // Every `authorization` header, in order: a door token is refused when more than one is sent.
    let mut authorizations: Vec<String> = Vec::new();
    // Every `A2A-Version` header, in order: a JSON-RPC request is served only when there is one.
    let mut a2a_versions: Vec<String> = Vec::new();
    let a2a_version_prefix = format!("{}:", crate::a2a::A2A_VERSION_HEADER.to_ascii_lowercase());

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        let lower = lower.trim_end();
        if lower.starts_with("authorization:") {
            // Taken from the line as sent: a control token and a door token are base64url and
            // case-sensitive.
            let value = line.trim_end()["authorization:".len()..].trim().to_string();
            authorizations.push(value.clone());
            authorization = Some(value);
        } else if lower.starts_with(&a2a_version_prefix) {
            // Taken from the line as sent, so a refusal names the version exactly as requested.
            let value = line.trim_end()[a2a_version_prefix.len()..]
                .trim()
                .to_string();
            a2a_versions.push(value);
        } else if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        } else if lower.starts_with("content-type:") && lower.contains("application/json") {
            is_json = true;
        } else if let Some(rest) = lower.strip_prefix("traceparent:") {
            traceparent = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix("last-event-id:") {
            last_event_id = rest.trim().parse().ok();
        } else if let Some(rest) = lower.strip_prefix(&format!("{AUDIENCE_HEADER}:")) {
            // Already lowercased with the rest of the line, which is exactly the form an audience
            // takes: both sides build it with `to_lowercase`.
            audience = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{PEER_ORIGIN_HEADER}:")) {
            // Lowercased with the rest of the line, which is the only spelling
            // `origin::from_wire` accepts — a peer that shouts `PEER` is read as `peer`.
            task_origin = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{PEER_TRUST_HEADER}:")) {
            task_trust = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{DELEGATION_ID_HEADER}:")) {
            // Both id spellings are lowercase hex, so lowercasing the whole line loses nothing.
            delegation_id = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{COMPLETION_SESSION_HEADER}:")) {
            completion_session = Some(rest.trim().to_string());
        } else if let Some(rest) = lower.strip_prefix(&format!("{FORGET_SESSION_HEADER}:")) {
            // One value asks for a forget. Anything else reads as the header not being there:
            // dropping a conversation on a value nobody meant as `true` is the failure this
            // header exists to recover from.
            forget_session = rest.trim() == "true";
        }
    }

    // Classified once at the door, so both task-starting paths below read the same rule rather
    // than each interpreting the headers for itself.
    let provenance = origin::from_wire(task_origin.as_deref(), task_trust.as_deref());

    // Both delegation headers mean something only on the completion path, which reads them below
    // and starts no task. On every other path they are ignored.
    let is_completion = provenance.origin() == TaskOrigin::Completion;

    // Routed ahead of the operator plane on its own segment, and answering every method under it
    // including the ones it refuses: a `PUT` that fell through would leave no record of somebody
    // trying to write, and a peer request that fell through to `/resources/` would be answered by
    // the wrong authoriser. Ahead of the door gate too: the handle is a peer's credential, and a
    // peer consuming one holds no token for this door.
    if is_peer_path(&path) {
        let response = handle_peer_request(&peer_plane, &method, &path, audience.as_deref()).await;
        let _ = writer_half.write_all(&framed_bytes(&response)).await;
        return;
    }

    // Routed on its prefix ahead of the JSON-RPC door, answering every method under it including
    // the ones it refuses, and reading the body itself: a request it refuses unauthenticated, or
    // for a declared length over its cap, must never have its body read at all. Ahead of the door
    // gate too: its caller presents the control token in the same `authorization` header, and
    // neither credential is accepted in the other's place.
    if is_control_path(&path) {
        let request = ControlRequest {
            method: &method,
            path: &path,
            authorization: authorization.as_deref(),
            peer: Some(peer_addr.ip()),
            content_length,
            is_json,
        };
        let response = handle_control_request(&control_plane, request, &mut reader).await;
        let _ = writer_half.write_all(&framed_bytes(&response)).await;
        return;
    }

    // The public card, which A2A requires be readable by anyone.
    if method == "GET" && path == "/.well-known/agent-card.json" {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            card_json.len(),
            card_json,
        );
        let _ = writer_half.write_all(response.as_bytes()).await;
        return;
    }

    // Everything past this point is gated on an authenticated door, and the token is checked
    // before the path, the body or the method is looked at, so no refusal here depends on whether
    // a named task, file or path exists. A public door ignores `authorization`.
    let grant = match gate.as_deref() {
        None => None,
        Some(gate) => {
            let headers: Vec<&str> = authorizations.iter().map(String::as_str).collect();
            match gate.auth.verify(&headers) {
                Ok(grant) => Some(grant),
                Err(refusal) => {
                    let response = framed_bytes(&refusal.response(&gate.realm));
                    write_refusal(writer_half, reader, &response).await;
                    return;
                }
            }
        }
    };

    // Who the roster says is calling, when the door let a formation token in. It is recorded on
    // the task this request starts, and decides nothing.
    let caller_member = grant
        .as_ref()
        .and_then(|grant| grant.formation_caller())
        .map(str::to_string);

    // A capsule that does not consent to peer tasks refuses a peer-origin request here: after the
    // token, so an unauthenticated caller learns nothing the public card does not say, and before
    // the scope, the path, the method or the body, so the refusal is the same bytes whatever was
    // asked for. A completion is not a peer task and passes; the peer plane, the control plane
    // and the card were routed above and never reach this.
    if peer_tasks::refuses(provenance, accepts_peer_tasks) {
        write_refusal(writer_half, reader, &framed_bytes(&peer_tasks::refusal())).await;
        return;
    }

    let refuse_scope = |scope: &str| -> Option<Vec<u8>> {
        let (gate, grant) = gate.as_deref().zip(grant.as_ref())?;
        let refusal = grant.require(scope).err()?;
        Some(framed_bytes(&refusal.response(&gate.realm)))
    };

    // The resource plane is routed on its prefix alone and answers every method under it,
    // including the ones it refuses: a `PUT` that fell through to the bare 404 below would leave
    // no trace record of somebody trying to write.
    if path.starts_with(RESOURCE_PATH_PREFIX) {
        if let Some(refused) = refuse_scope(crate::door_auth::RESOURCES_FILES_SCOPE) {
            write_refusal(writer_half, reader, &refused).await;
            return;
        }
        let response = handle_resource_request(&resource_plane, &method, &path).await;
        let _ = writer_half.write_all(&framed_bytes(&response)).await;
        return;
    }

    if method == "POST" && (path == "/" || path.is_empty()) && is_json && content_length > 0 {
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let body_str = String::from_utf8_lossy(&body).to_string();

        // The JSON-RPC envelope first: a body that is not a JSON-RPC 2.0 request is a request of
        // no A2A version.
        let req = match JsonRpcRequest::from_body(&body_str) {
            Ok(req) => req,
            Err(refusal) => {
                let _ = writer_half
                    .write_all(refusal.into_http_response().as_bytes())
                    .await;
                return;
            }
        };

        // Then the version, on every method the door serves, murmur's own and completions among
        // them: ahead of method resolution, so a client of another version learns that its
        // version is what is refused rather than its method name, and ahead of the scope, since a
        // method of an unknown version has no scope to judge. Answered as one JSON body even on a
        // streaming method, and nothing is started or stopped.
        if !crate::a2a::accepts_a2a_version(&a2a_versions) {
            let refusal = crate::a2a::version_not_supported(req.id, &a2a_versions);
            let _ = writer_half
                .write_all(refusal.into_http_response().as_bytes())
                .await;
            return;
        }

        // A completion is addressed to one session, and this door answers for one session. An
        // address that has outlived the session that made the delegation — a parent that
        // restarted onto the same port — is refused here rather than delivered to whoever
        // answers now. The refusal names the addressed session and not this one: a caller that
        // guessed wrong learns nothing about who is actually here.
        if is_completion && completion_session.as_deref() != Some(session_id.as_str()) {
            let addressed = completion_session.as_deref().unwrap_or("");
            let shown = if addressed.is_empty() {
                "<unaddressed>"
            } else {
                addressed
            };
            let response = JsonRpcResponse::murmur_error(
                req.id,
                MurmurError::CompletionMisaddressed,
                &format!(
                    "completion is addressed to session {shown}, which is not the session \
                     running here"
                ),
                &[("addressedSession", addressed.to_string())],
            )
            .into_http_response();
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        // A sub-capsule's outcome belongs to the task that delegated to it, so it is handed to the
        // session's delegation set here and never becomes a task: ahead of method resolution,
        // which serves no `SendMessage` under `task_acceptance: none`, and ahead of the task
        // registry, which refuses a second task under `single`.
        if is_completion {
            let response = handle_completion(req, delegation_id.as_deref(), &live_delegations);
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        // An unserved method is refused before its scope is judged: it is `-32601` to anyone the
        // door let in, since the card lists what it serves. A public door has no extended card,
        // and A2A answers asking one for it as an unsupported operation.
        let Some(resolved) = DoorMethod::resolve(&req.method, &task_acceptance, gate.is_some())
        else {
            let response = if req.method == DoorMethod::GetExtendedAgentCard.wire_name() {
                JsonRpcResponse::a2a_error(
                    req.id,
                    A2aError::UnsupportedOperation,
                    "this agent serves no extended agent card",
                    &[],
                )
            } else {
                JsonRpcResponse::err(req.id, METHOD_NOT_FOUND, "Method not found")
            };
            let _ = writer_half
                .write_all(response.into_http_response().as_bytes())
                .await;
            return;
        };
        if let Some(refused) = resolved.scope().and_then(refuse_scope) {
            let _ = writer_half.write_all(&refused).await;
            return;
        }

        // A forget writes to this capsule's harness session map, and only a `transport: process`
        // capsule has one. Refused here, on the request that carried it, rather than dropped into
        // a task that would ignore it: a caller that asked for a conversation to be dropped and
        // was answered `completed` would read that as the drop having happened.
        let starts_a_turn = matches!(
            resolved,
            DoorMethod::SendMessage | DoorMethod::SendStreamingMessage
        );
        if forget_session && starts_a_turn && !forgettable_session {
            let response = JsonRpcResponse::err(
                req.id,
                INVALID_PARAMS,
                &format!(
                    "{FORGET_SESSION_HEADER} asks this capsule to forget the harness session \
                     this context names, and only a capsule on inference.transport: process has \
                     one"
                ),
            )
            .into_http_response();
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        // Every method the door serves takes its parameters as one object.
        if !req.params.is_object() {
            let response = JsonRpcResponse::err(
                req.id,
                INVALID_PARAMS,
                "Invalid parameters: params must be an object",
            )
            .into_http_response();
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        // The streaming methods own the connection; every other method answers one JSON body.
        let response = match resolved {
            DoorMethod::SendStreamingMessage => {
                handle_message_stream(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    MessageSender {
                        traceparent,
                        provenance,
                        forget_session,
                        caller_member,
                    },
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                    closing,
                )
                .await;
                return;
            }
            DoorMethod::StreamWatch => {
                handle_stream_watch(
                    writer_half,
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                    conversation_mode_str,
                    closing,
                )
                .await;
                return;
            }
            DoorMethod::GetExtendedAgentCard => match gate.as_deref() {
                Some(gate) => JsonRpcResponse::ok(req.id, &gate.extended_card).into_http_response(),
                None => {
                    unreachable!("resolve serves the extended card only on an authenticated door")
                }
            },
            door_method => handle_jsonrpc(
                door_method,
                req,
                &task_registry,
                &task_tx,
                traceparent,
                provenance,
                forget_session,
                caller_member,
                detached.as_ref(),
                &live_delegations,
                &session_id,
            ),
        };
        let _ = writer_half.write_all(response.as_bytes()).await;
        return;
    }

    let response =
        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string();
    let _ = writer_half.write_all(response.as_bytes()).await;
}

/// The most unread request bytes a refused connection discards before it closes.
const REFUSAL_DRAIN_LIMIT: usize = 64 * 1024;

/// How long a refused connection waits for the caller's unread bytes before it closes.
const REFUSAL_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Writes a gate refusal and closes the connection, discarding what the caller already sent.
///
/// The door refuses before reading the request's body. A socket closed with unread bytes in its
/// receive buffer is answered with a reset, which can reach the caller before it has read the
/// refusal, so up to [`REFUSAL_DRAIN_LIMIT`] bytes are read and dropped, for at most
/// [`REFUSAL_DRAIN_GRACE`], once the refusal is written. Nothing drained is looked at.
async fn write_refusal<R: tokio::io::AsyncRead + Unpin>(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    mut reader: R,
    response: &[u8],
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if writer.write_all(response).await.is_err() {
        return;
    }
    let _ = writer.shutdown().await;
    let drain = async {
        let mut sink = [0u8; 4096];
        let mut left = REFUSAL_DRAIN_LIMIT;
        while left > 0 {
            match reader.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(read) => left = left.saturating_sub(read),
            }
        }
    };
    let _ = tokio::time::timeout(REFUSAL_DRAIN_GRACE, drain).await;
}

/// One plane response as bytes on the wire. `connection: close` is appended here rather than by
/// either plane: framing is the transport's business, and both planes already carry their own
/// `content-length`.
fn framed_bytes(response: &ResourceResponse) -> Vec<u8> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason_phrase(response.status)
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(&response.body);
    bytes
}

/// `SendStreamingMessage`: route the message as [`route_message`] does, then stream the session's
/// frames until the task it started or continued reaches its final status.
///
/// Every refusal [`read_send_params`] or the router answers with is one JSON-RPC body, written
/// before any SSE head. A new task the registry has no room for is the one refusal written as a
/// stream: a `rejected` status frame.
#[allow(clippy::too_many_arguments)]
async fn handle_message_stream(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    mut closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before the message is routed, so no frame the task writes once it is enqueued or
    // handed its input is missed.
    let mut rx = sse_tx.subscribe();

    let routed = match read_send_params(&req.params) {
        Ok(message) => route_message(&req.id, message, task_registry, task_tx, sender),
        Err(refusal) => MessageRoute::Refused(refusal.into_response(req.id.clone())),
    };
    let (task_id, rejected) = match routed {
        MessageRoute::Refused(response) => {
            let _ = writer
                .write_all(response.into_http_response().as_bytes())
                .await;
            return;
        }
        MessageRoute::Started(task) | MessageRoute::Continued(task) => (task.id, None),
        MessageRoute::Rejected { task, refusal } => (task.id.clone(), Some((task, refusal))),
    };

    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    if writer.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    // Replay buffered events for reconnecting clients. The highest id written is where the live
    // loop picks up, so a frame both replayed and received live is written once.
    let mut last_written = None;
    if let Some(last_id) = last_event_id {
        match write_replay(&mut writer, &sse_buffer, last_id).await {
            Ok(written) => last_written = written,
            Err(()) => return,
        }
    }

    if let Some((task, refusal)) = rejected {
        let rejected_event = TaskStatusUpdateEvent {
            id: task.id,
            context_id: Some(task.context_id),
            status: StreamStatus {
                state: TaskState::Rejected.as_str().into(),
                message: refusal.into(),
                response: None,
                reopen: None,
            },
            r#final: true,
        };
        let _ = writer
            .write_all(format_rejected_event(&rejected_event).as_bytes())
            .await;
        return;
    }

    // Forward broadcast events to the SSE stream
    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                if writer.write_all(SSE_HEARTBEAT_COMMENT).await.is_err() {
                    return;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event_arc) => {
                        if !admit_live_frame(&mut last_written, &event_arc) {
                            continue;
                        }
                        // Other tasks' frames are forwarded too; only this task's own final
                        // status ends the connection.
                        let is_final = is_final_status_for(&event_arc, &task_id);
                        if writer.write_all(event_arc.as_bytes()).await.is_err() {
                            return;
                        }
                        if is_final {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        crate::runtime_err!("[capsule-runtime] SSE broadcast lagged by {n} events");
                        if writer
                            .write_all(format_lagged_event(n).as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                }
            }
            () = door_closing(&mut closing) => return,
        }
    }
}

/// Write the replay of the frames after `last_id` — preceded by a `gap` frame when the buffer
/// cannot supply all of them — and return the highest id written, or `None` when the replay was
/// empty. `Err` means the client is gone.
async fn write_replay(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    sse_buffer: &Mutex<SseEventBuffer>,
    last_id: u64,
) -> Result<Option<u64>, ()> {
    use tokio::io::AsyncWriteExt;

    let replay = sse_buffer.lock().unwrap().replay_from(last_id);
    let events = match replay {
        ReplayResult::Complete(events) => events,
        ReplayResult::WithGap {
            first_available_id,
            events,
        } => {
            let gap = format_gap_event(first_available_id);
            writer.write_all(gap.as_bytes()).await.map_err(|_| ())?;
            events
        }
    };
    let mut last_written = None;
    for event in events {
        writer.write_all(event.as_bytes()).await.map_err(|_| ())?;
        last_written = frame_id(&event).or(last_written);
    }
    Ok(last_written)
}

/// The `rejected` status written to a `SendStreamingMessage` connection the door refuses, busy or
/// closing. It goes to that one connection and is never buffered, so it carries no `id:` line:
/// it has no place in the session's sequence, and an id would move a client's resume cursor.
fn format_rejected_event(event: &TaskStatusUpdateEvent) -> String {
    let data = serde_json::to_string(event).unwrap_or_default();
    format_unnumbered_sse_event(StreamFrame::Status, &data)
}

/// Passive observer handler for `stream/watch`.
///
/// Does not submit a task or call `can_accept()`. Subscribes to the broadcast channel,
/// replays buffered events, then forwards live events until the capsule shuts down or
/// the client disconnects. A `final: true` status event ends one task turn but does NOT
/// close this connection — the observer stays alive across turns.
///
/// Writes [`SSE_HEARTBEAT_COMMENT`] every [`SSE_HEARTBEAT_INTERVAL`], whether or not events
/// flowed in between, so an observer can tell an idle capsule from a dead socket. The handler
/// runs on the runtime's worker pool, off the thread running the agent loop, so synchronous work
/// inside a turn does not delay it.
async fn handle_stream_watch(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode_str: String,
    mut closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before replaying so we don't miss events emitted between replay and loop start.
    let mut rx = sse_tx.subscribe();

    let headers = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n\r\n";
    if writer.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    // Emit connection-ack so observers know the capsule's conversation mode without reading manifest.
    let ack = format_unnumbered_sse_event(
        StreamFrame::ConnectionAck,
        &format!("{{\"role\":\"observer\",\"conversation_mode\":\"{conversation_mode_str}\"}}"),
    );
    if writer.write_all(ack.as_bytes()).await.is_err() {
        return;
    }

    let Ok(mut last_written) =
        write_replay(&mut writer, &sse_buffer, last_event_id.unwrap_or(0)).await
    else {
        return;
    };

    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                // A failed write means the observer is gone; there is nothing left to serve.
                if writer.write_all(SSE_HEARTBEAT_COMMENT).await.is_err() {
                    return;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event_arc) => {
                        if !admit_live_frame(&mut last_written, &event_arc) {
                            continue;
                        }
                        if writer.write_all(event_arc.as_bytes()).await.is_err() {
                            return;
                        }
                        // Do NOT exit on a final status — it ends one task, not the capsule.
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let closed = format_unnumbered_sse_event(StreamFrame::CapsuleClosed, "{}");
                        let _ = writer.write_all(closed.as_bytes()).await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        crate::runtime_err!("[capsule-runtime] stream/watch: SSE broadcast lagged by {n} events");
                        if writer
                            .write_all(format_lagged_event(n).as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        continue;
                    }
                }
            }
            // An exiting capsule ends an observer's stream without `capsule-closed`: that frame
            // means the frame stream closed while the capsule still serves the connection.
            () = door_closing(&mut closing) => return,
        }
    }
}

/// The door's whole answer to a sub-capsule's completion: hand it to the delegation set, and say
/// whether a task here was waiting for it.
///
/// Success for a delegation the running task started and has not accounted for, and for one
/// whose outcome already arrived — a repeat is the watcher retrying a post it could not confirm.
/// Anything else is [`MurmurError::CompletionNotAwaited`] naming the delegation, whose message the
/// child records as its `delivery_error`. The outcome itself is read from the child's
/// `completion.json` by the task that receives it, never from this message.
fn handle_completion(
    req: JsonRpcRequest,
    delegation_id: Option<&str>,
    live_delegations: &LiveDelegations,
) -> String {
    if req.method != DoorMethod::SendMessage.wire_name() {
        return JsonRpcResponse::err(
            req.id,
            METHOD_NOT_FOUND,
            &format!(
                "a completion is posted with {}",
                DoorMethod::SendMessage.wire_name()
            ),
        )
        .into_http_response();
    }
    let Some(delegation_id) = delegation_id.filter(|id| !id.is_empty()) else {
        return JsonRpcResponse::murmur_error(
            req.id,
            MurmurError::CompletionNotAwaited,
            &format!(
                "completion names no delegation in {DELEGATION_ID_HEADER}, so no task in this \
                 session is waiting for it"
            ),
            &[],
        )
        .into_http_response();
    };
    let not_awaited = |message: String| {
        JsonRpcResponse::murmur_error(
            req.id.clone(),
            MurmurError::CompletionNotAwaited,
            &message,
            &[("delegationId", delegation_id.to_string())],
        )
        .into_http_response()
    };
    match live_delegations.arrive(delegation_id) {
        Arrival::Delivered | Arrival::AlreadyDelivered => JsonRpcResponse::ok(
            req.id.clone(),
            serde_json::json!({ "message": completion_received(&req.params, delegation_id) }),
        )
        .into_http_response(),
        Arrival::NotOutstanding => not_awaited(format!(
            "no task in this session is waiting for delegation {delegation_id}"
        )),
        Arrival::BeingEnded => not_awaited(format!(
            "delegation {delegation_id} is being ended by this session; its launcher records the \
             outcome"
        )),
    }
}

/// The message acknowledging a completion for `delegation_id` whose request carried `params`: from
/// the agent, in the completion's own `contextId` when it named one, holding one data part.
fn completion_received(params: &Value, delegation_id: &str) -> Value {
    let mut message = serde_json::json!({
        "messageId": format!("msg_{delegation_id}_received"),
        "role": Role::Agent,
        "parts": [Part::data(serde_json::json!({
            "delegation_id": delegation_id,
            "received": true,
        }))],
    });
    if let Some(context_id) = params.pointer("/message/contextId").and_then(Value::as_str) {
        message["contextId"] = Value::String(context_id.to_string());
    }
    message
}

/// Answers a resolved method that replies with one JSON-RPC body.
#[allow(clippy::too_many_arguments)]
fn handle_jsonrpc(
    method: DoorMethod,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    forget_session: bool,
    caller_member: Option<String>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let id = req.id;
    match method {
        DoorMethod::SendMessage => handle_message_send(
            id,
            &req.params,
            task_registry,
            task_tx,
            MessageSender {
                traceparent,
                provenance,
                forget_session,
                caller_member,
            },
        ),
        DoorMethod::GetTask => {
            handle_tasks_get(id, &req.params, task_registry, caller_member.as_deref())
        }
        DoorMethod::CancelTask => handle_tasks_cancel(
            id,
            &req.params,
            task_registry,
            caller_member.as_deref(),
            detached,
            live_delegations,
        ),
        DoorMethod::SessionStop => {
            handle_session_stop(id, task_registry, detached, live_delegations, session_id)
        }
        DoorMethod::SendStreamingMessage
        | DoorMethod::StreamWatch
        | DoorMethod::GetExtendedAgentCard => {
            unreachable!("handle_connection answers the streaming methods and the extended card")
        }
    }
}

/// What the door read off a message's request besides its body: who sent it, and the headers the
/// task it starts carries.
pub(crate) struct MessageSender {
    pub traceparent: Option<String>,
    pub provenance: TaskProvenance,
    pub forget_session: bool,
    /// The formation member that sent it, when the door let it in on a formation token.
    pub caller_member: Option<String>,
}

/// What [`route_message`] did with a message.
#[derive(Debug)]
enum MessageRoute {
    /// A new task, enqueued and handed to the task loop, in `TASK_STATE_SUBMITTED`.
    Started(A2aTask),
    /// A new task refused because the capsule has no room for it or its session is closing, in
    /// `TASK_STATE_REJECTED`. `refusal` is its status message. Nothing was enqueued.
    Rejected {
        task: A2aTask,
        refusal: &'static str,
    },
    /// The message went to the task it names, which was waiting for input and is now
    /// `TASK_STATE_WORKING`.
    Continued(A2aTask),
    /// The error answering the message. Nothing was started or delivered.
    Refused(JsonRpcResponse),
}

/// Route one message `SendMessage` or `SendStreamingMessage` read, under one registry lock.
///
/// | The message names | Route |
/// |---|---|
/// | no `taskId` | a new task: [`MessageRoute::Rejected`] when the registry cannot accept one, otherwise [`MessageRoute::Started`] |
/// | a task the registry does not hold, or one `sender` may not see | `-32001` `TaskNotFoundError` |
/// | an ended task | `-32004` `UnsupportedOperationError` naming its state |
/// | a `submitted` or `working` task | `-32004` naming its state: a task takes a message only while it waits for input |
/// | an `input-required` task in another `contextId` | `-32602` |
/// | an `input-required` task | [`MessageRoute::Continued`]: the text goes to its waiter, and capacity is not consulted |
///
/// The text delivered is [`IncomingMessage::agent_text`] followed by the
/// [`TaskRegistry::reference_block`] for its `referenceTaskIds`, rendered under the same lock, so
/// each referenced task is named as it stood when the message arrived. `id` is the request's.
fn route_message(
    id: &Value,
    message: IncomingMessage,
    task_registry: &Mutex<TaskRegistry>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
) -> MessageRoute {
    let mut reg = task_registry.lock().unwrap();
    let caller = sender.caller_member.as_deref();
    let mut text = message.agent_text();
    if let Some(block) = reg.reference_block(&message.reference_task_ids, |task_id| {
        visible_to(&reg, task_id, caller)
    }) {
        text = format!("{text}\n\n{block}");
    }

    let Some(task_id) = message.task_id.clone() else {
        return start_task(id, message, text, &mut reg, task_tx, sender);
    };
    let Some((state, task_context)) = reg
        .history
        .get(&task_id)
        .filter(|_| visible_to(&reg, &task_id, caller))
        .cloned()
    else {
        return MessageRoute::Refused(task_not_found_response(id.clone(), Some(&task_id)));
    };
    let unsupported = |message: String| {
        MessageRoute::Refused(JsonRpcResponse::a2a_error(
            id.clone(),
            A2aError::UnsupportedOperation,
            &message,
            &[
                ("taskId", task_id.clone()),
                ("state", state.wire_name().to_string()),
            ],
        ))
    };
    if state.is_terminal() {
        return unsupported(format!(
            "task {task_id} has ended in {}; send a new message without taskId, in the same \
             contextId, to start a new task",
            state.wire_name()
        ));
    }
    if state != TaskState::InputRequired {
        return unsupported(format!(
            "task {task_id} is {}; this agent takes a message on a task only while it waits in \
             {}",
            state.wire_name(),
            TaskState::InputRequired.wire_name()
        ));
    }
    if let Some(context_id) = message.context_id.as_deref().filter(|c| *c != task_context) {
        return MessageRoute::Refused(JsonRpcResponse::err(
            id.clone(),
            INVALID_PARAMS,
            &format!(
                "Invalid params: message.contextId {context_id} does not match task {task_id}, \
                 whose contextId is {task_context}"
            ),
        ));
    }
    if reg.deliver_input(&task_id, text).is_err() {
        return unsupported(format!("task {task_id} is no longer waiting for input"));
    }
    match reg.get_task(&task_id) {
        Some(task) => MessageRoute::Continued(task),
        None => MessageRoute::Refused(task_not_found_response(id.clone(), Some(&task_id))),
    }
}

/// [`route_message`] for a message that names no task: a new task, minted, checked against the
/// registry's capacity, enqueued and handed to the task loop, all under the caller's lock.
fn start_task(
    id: &Value,
    message: IncomingMessage,
    text: String,
    reg: &mut TaskRegistry,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
) -> MessageRoute {
    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("ctx_{}", uuid::Uuid::now_v7().simple()));

    if !reg.can_accept() {
        let refusal = if reg.is_closed() {
            crate::a2a::REJECTED_SESSION_CLOSING_MESSAGE
        } else {
            crate::a2a::REJECTED_BUSY_MESSAGE
        };
        let task = A2aTask {
            status: TaskStatus {
                state: TaskState::Rejected,
                message: Some(crate::a2a::StatusMessage::agent(
                    &task_id,
                    &context_id,
                    refusal,
                )),
            },
            id: task_id,
            context_id,
            artifacts: None,
            metadata: None,
        };
        return MessageRoute::Rejected { task, refusal };
    }
    reg.enqueue(&task_id, &context_id);
    if let Some(member) = &sender.caller_member {
        reg.record_submitter(&task_id, member);
    }

    let part_kinds = message.part_kinds();
    let incoming = IncomingTask {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        message_id: message.message_id,
        message_text: text,
        traceparent: sender.traceparent,
        provenance: sender.provenance,
        source: crate::a2a::SOURCE_A2A,
        forget_session: sender.forget_session,
        caller_member: sender.caller_member,
        reference_task_ids: message.reference_task_ids,
        part_kinds,
    };
    // Capacity was checked under this lock, so the channel has room; a failure is unexpected and
    // takes the task back out of the registry.
    if task_tx.try_send(incoming).is_err() {
        reg.pending_count -= 1;
        reg.history.remove(&task_id);
        return MessageRoute::Refused(JsonRpcResponse::err(
            id.clone(),
            INTERNAL_ERROR,
            "internal error: queue send failed",
        ));
    }

    MessageRoute::Started(A2aTask {
        id: task_id,
        context_id,
        status: TaskStatus::of(TaskState::Submitted),
        artifacts: None,
        metadata: None,
    })
}

/// `SendMessage`: the message routed as [`route_message`] routes it, answered as a
/// `SendMessageResponse` holding the task, or as the error refusing it.
fn handle_message_send(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
) -> String {
    let message = match read_send_params(params) {
        Ok(message) => message,
        Err(refusal) => return refusal.into_response(id).into_http_response(),
    };
    let task = match route_message(&id, message, task_registry, task_tx, sender) {
        MessageRoute::Started(task)
        | MessageRoute::Continued(task)
        | MessageRoute::Rejected { task, .. } => task,
        MessageRoute::Refused(response) => return response.into_http_response(),
    };
    JsonRpcResponse::ok(id, serde_json::json!({ "task": task })).into_http_response()
}

/// Whether `caller_member` may read or cancel `task_id`: the operator (`None`) reaches every
/// task, a formation member only the tasks it submitted itself.
fn visible_to(reg: &TaskRegistry, task_id: &str, caller_member: Option<&str>) -> bool {
    caller_member.is_none_or(|member| reg.submitter(task_id) == Some(member))
}

/// The `TaskNotFoundError` answering a request that named `task_id`, or named none.
fn task_not_found(id: Value, task_id: Option<&str>) -> String {
    task_not_found_response(id, task_id).into_http_response()
}

/// [`task_not_found`] as a response.
fn task_not_found_response(id: Value, task_id: Option<&str>) -> JsonRpcResponse {
    let metadata: Vec<(&str, String)> = task_id
        .map(|task_id| ("taskId", task_id.to_string()))
        .into_iter()
        .collect();
    JsonRpcResponse::a2a_error(id, A2aError::TaskNotFound, "Task not found", &metadata)
}

/// `GetTask`: one task's state, and how it ended once it has. `params.id` is required, and
/// `historyLength` is ignored: the door keeps no message history, so a task carries none.
///
/// A formation member — `caller_member`, from the formation token the door let in — reads only
/// the tasks it submitted itself. Every other id, another member's task or the operator's
/// included, gets the same [`A2aError::TaskNotFound`] an id this capsule never held gets. A
/// formation token reaches no `SendStreamingMessage`, whose connection forwards every task's
/// frames, so `GetTask` is the only way a member reads a task.
fn handle_tasks_get(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller_member: Option<&str>,
) -> String {
    let Some(task_id) = params
        .get("id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.is_empty())
        .map(str::to_string)
    else {
        return JsonRpcResponse::err(id, INVALID_PARAMS, "GetTask requires an id")
            .into_http_response();
    };

    let reg = task_registry.lock().unwrap();
    match reg
        .get_task(&task_id)
        .filter(|_| visible_to(&reg, &task_id, caller_member))
    {
        Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
        None => task_not_found(id, Some(&task_id)),
    }
}

/// `CancelTask`: stop one task and report what is still running.
///
/// Runs on the connection task and never waits for the agent loop: it takes the registry lock,
/// records the cancellation, snapshots the two work registries and answers. Whether the loop has
/// noticed yet is not the caller's question — the recorded state is already `canceled`.
///
/// A live task is answered with its `canceled` state and the residue. A task that had already
/// ended is left unchanged and answered [`A2aError::TaskNotCancelable`], whose `ErrorInfo` names
/// the task and the state it ended in, spelled as `GetTask` spells it. An id this capsule never
/// held is the same [`A2aError::TaskNotFound`] `GetTask` answers with.
///
/// A formation member — `caller_member` — cancels only the tasks it submitted itself, by the
/// same [`visible_to`] rule `GetTask` reads by: any other id gets that same `TaskNotFound`, and
/// nothing is cancelled.
fn handle_tasks_cancel(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller_member: Option<&str>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
) -> String {
    let Some(task_id) = params.get("id").and_then(Value::as_str).map(str::to_string) else {
        return JsonRpcResponse::err(
            id,
            INVALID_PARAMS,
            "Invalid params: CancelTask requires an id",
        )
        .into_http_response();
    };

    let (outcome, task) = {
        let mut reg = task_registry.lock().unwrap();
        if !visible_to(&reg, &task_id, caller_member) {
            return task_not_found(id, Some(&task_id));
        }
        let outcome = reg.request_cancel(&task_id);
        (outcome, reg.get_task(&task_id))
    };

    match (outcome, task) {
        (CancelOutcome::Unknown, _) | (_, None) => task_not_found(id, Some(&task_id)),
        (CancelOutcome::AlreadyTerminal, Some(task)) => {
            let state = task.status.state.wire_name();
            JsonRpcResponse::a2a_error(
                id,
                A2aError::TaskNotCancelable,
                &format!("Task cannot be canceled: it is already {state}"),
                &[("taskId", task_id.clone()), ("state", state.to_string())],
            )
            .into_http_response()
        }
        (CancelOutcome::Accepted, Some(mut task)) => {
            // Read after the state is recorded, so nothing this snapshot names can have been
            // started by the cancelled task afterwards.
            task.artifacts = Residue::snapshot(detached, live_delegations)
                .into_artifact()
                .map(|artifact| vec![artifact]);
            JsonRpcResponse::ok(id, task).into_http_response()
        }
    }
}

/// `session/stop`: cancel everything this session still holds and report what it leaves running.
///
/// The one moment anything can ask a capsule for that account. A detached shell command keeps its
/// own lifecycle, and a delegated child runs until the cancelled task that started it ends it;
/// once the process is gone so is the registry that knew about either — so the question is asked
/// while the door is still up, and ending the process is left to the signal that follows.
///
/// Nothing here shuts the door down. A method that killed its own process would be racing its
/// response out of it.
///
/// Three keys, all three always present. `canceled` lists only the tasks this call moved to
/// `canceled`, so a second stop answers `[]` rather than an error. `residue` is `[]` when nothing
/// is running — unlike `CancelTask`, which omits the key: a session stop has to be able to say
/// "nothing" as a positive fact, because that is the whole answer the operator asked for.
fn handle_session_stop(
    id: Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let canceled = task_registry.lock().unwrap().cancel_every_live();

    // Read after every cancellation is recorded, so nothing this snapshot names can have been
    // started by a task afterwards.
    let residue = Residue::snapshot(detached, live_delegations).into_json_items();

    JsonRpcResponse::ok(
        id,
        serde_json::json!({
            "session_id": session_id,
            "canceled": canceled,
            "residue": residue,
        }),
    )
    .into_http_response()
}

#[cfg(test)]
#[allow(clippy::print_stdout, clippy::print_stderr)]
mod tests {
    use super::*;
    use crate::errors::RuntimeError;
    use crate::streaming::format_sse_event;

    const ACCEPTANCES: [TaskAcceptance; 3] = [
        TaskAcceptance::None,
        TaskAcceptance::Single,
        TaskAcceptance::Queue,
    ];

    /// The card an http capsule serves: that transport streams text.
    const HTTP_TRANSPORT: TransportCapabilities = TransportCapabilities {
        streams_text: true,
        kind: TransportKind::Http,
    };

    /// Clients match on the extension URIs as identifiers, so a change to the docs reference base
    /// that would rename one fails here.
    #[test]
    fn extension_uris_keep_their_identifier_values() {
        assert_eq!(
            DOOR_EXTENSION_URI,
            "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1"
        );
        assert_eq!(
            CAPSULE_EXTENSION_URI,
            "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1"
        );
        assert_eq!(
            STREAM_EXTENSION_URI,
            "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1"
        );
    }

    /// The stream extension's `params.frames` a capsule on each transport serves under each
    /// acceptance. Written once per transport, so a frame one transport alone writes is an edit to
    /// that transport's table.
    const HTTP_FRAMES: [(TaskAcceptance, &[&str]); 3] = [
        (
            TaskAcceptance::None,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
            ],
        ),
        (
            TaskAcceptance::Single,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
                "error",
            ],
        ),
        (
            TaskAcceptance::Queue,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
                "error",
            ],
        ),
    ];

    const PROCESS_FRAMES: [(TaskAcceptance, &[&str]); 3] = [
        (
            TaskAcceptance::None,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
            ],
        ),
        (
            TaskAcceptance::Single,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
                "error",
            ],
        ),
        (
            TaskAcceptance::Queue,
            &[
                "status",
                "artifact",
                "text",
                "thinking",
                "tool-call-started",
                "tool-call-progress",
                "gap",
                "lagged",
                "connection-ack",
                "capsule-closed",
                "error",
            ],
        ),
    ];

    /// Every `TransportCapabilities` a session can stage.
    fn every_transport() -> Vec<TransportCapabilities> {
        TransportKind::ALL
            .into_iter()
            .flat_map(|kind| {
                [false, true].map(|streams_text| TransportCapabilities { streams_text, kind })
            })
            .collect()
    }

    fn card_for(acceptance: &TaskAcceptance, planes: DeclaredPlanes) -> Value {
        card_for_transport(acceptance, planes, HTTP_TRANSPORT)
    }

    fn card_for_transport(
        acceptance: &TaskAcceptance,
        planes: DeclaredPlanes,
        transport: TransportCapabilities,
    ) -> Value {
        let identity = CapsuleIdentity {
            capsule_name: "probe".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_probe".to_string(),
            capsule_url: "http://127.0.0.1:1".to_string(),
        };
        build_agent_card(
            &identity,
            &[],
            &CapabilityPolicy::default(),
            acceptance,
            planes,
            false,
            transport,
        )
    }

    fn identity() -> CapsuleIdentity {
        CapsuleIdentity {
            capsule_name: "my-agent".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_019f01a940ce7761854e768ecbe3d399".to_string(),
            capsule_url: "localhost:41873".to_string(),
        }
    }

    fn tool(name: &str) -> InstalledArtifactSummary {
        InstalledArtifactSummary {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            runtime: murmur_artifact::ArtifactRuntime::Tool,
            implementation: None,
            origin: murmur_artifact::LockOrigin::Operator,
        }
    }

    /// The card for `my-agent` 0.1.0 on port 41873 with `bash` installed, shell and network
    /// granted and `exports.files` declared.
    fn full_card(acceptance: &TaskAcceptance) -> Value {
        let policy = CapabilityPolicy {
            shell_allow: vec!["git".to_string()],
            network_allow: vec!["api.example.com".to_string()],
            ..CapabilityPolicy::default()
        };
        build_agent_card(
            &identity(),
            &[tool("bash")],
            &policy,
            acceptance,
            DeclaredPlanes {
                files: true,
                peer_files: false,
            },
            false,
            HTTP_TRANSPORT,
        )
    }

    fn door_params(card: &Value) -> &serde_json::Map<String, Value> {
        extension_params(card, DOOR_EXTENSION_URI).expect("the card has the door extension")
    }

    fn capsule_params(card: &Value) -> &serde_json::Map<String, Value> {
        extension_params(card, CAPSULE_EXTENSION_URI).expect("the card has the capsule extension")
    }

    fn door_methods(card: &Value) -> Vec<&str> {
        door_params(card)["methods"]
            .as_array()
            .expect("the door extension's methods are an array")
            .iter()
            .map(|m| m.as_str().expect("each method is a string"))
            .collect()
    }

    fn keys(value: &Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    fn assert_conforms(card: &Value) {
        if let Err(errors) = crate::a2a_conformance::check_agent_card(card) {
            panic!("the card does not conform to lf.a2a.v1.AgentCard: {errors:#?}\n{card:#}");
        }
    }

    #[test]
    fn a2a_card_every_builder_output_conforms() {
        let plane_sets = [(false, false), (true, false), (false, true), (true, true)];
        let tool_sets: [&[InstalledArtifactSummary]; 2] = [&[], &[tool("bash")]];
        let mut checked = 0;
        for acceptance in &ACCEPTANCES {
            for transport in every_transport() {
                for (files, peer_files) in plane_sets {
                    for tools in tool_sets {
                        for accepts_peer_tasks in [false, true] {
                            let card = build_agent_card(
                                &identity(),
                                tools,
                                &CapabilityPolicy::default(),
                                acceptance,
                                DeclaredPlanes { files, peer_files },
                                accepts_peer_tasks,
                                transport,
                            );
                            assert_conforms(&card);
                            assert_eq!(door_params(&card)["peerTasks"], accepts_peer_tasks);
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 3 * 2 * 2 * 4 * 2 * 2);
    }

    #[test]
    fn a2a_card_is_the_documented_document() {
        let card = full_card(&TaskAcceptance::Single);
        println!("{card:#}");
        assert_eq!(
            card,
            serde_json::json!({
                "name": "my-agent",
                "description": "Murmur capsule my-agent 0.1.0",
                "version": "0.1.0",
                "supportedInterfaces": [
                    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }
                ],
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extendedAgentCard": false,
                    "extensions": [
                        {
                            "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1",
                            "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.",
                            "required": false,
                            "params": {
                                "methods": ["SendMessage", "SendStreamingMessage", "stream/watch", "GetTask", "CancelTask", "session/stop"],
                                "peerTasks": false
                            }
                        },
                        {
                            "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1",
                            "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.",
                            "required": false,
                            "params": {
                                "sessionId": "ses_019f01a940ce7761854e768ecbe3d399",
                                "tools": ["bash"],
                                "shell": true,
                                "network": true,
                                "planes": ["files"]
                            }
                        },
                        {
                            "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
                            "description": "Every server-sent event type this capsule's SendStreamingMessage and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
                            "required": false,
                            "params": {
                                "frames": ["status", "artifact", "text", "thinking", "tool-call-started", "tool-call-progress", "gap", "lagged", "connection-ack", "capsule-closed", "error"]
                            }
                        }
                    ]
                },
                "securitySchemes": {},
                "securityRequirements": [],
                "defaultInputModes": ["text/plain", "application/json"],
                "defaultOutputModes": ["text/plain", "application/json"],
                "skills": [
                    {
                        "id": "task",
                        "name": "Run a task",
                        "description": "Runs one task given as a text message and reports its outcome.",
                        "tags": ["task"]
                    }
                ]
            })
        );
    }

    #[test]
    fn a2a_card_keeps_no_key_of_the_previous_shape() {
        for acceptance in &ACCEPTANCES {
            let card = full_card(acceptance);
            for key in ["url", "session_id", "serves"] {
                assert!(card.get(key).is_none(), "top-level {key}: {card}");
            }
            assert!(
                !card.to_string().contains("cancellation"),
                "no cancellation anywhere: {card}"
            );
            assert_eq!(
                keys(&card),
                [
                    "capabilities",
                    "defaultInputModes",
                    "defaultOutputModes",
                    "description",
                    "name",
                    "securityRequirements",
                    "securitySchemes",
                    "skills",
                    "supportedInterfaces",
                    "version"
                ]
            );
            assert_eq!(
                keys(&card["capabilities"]),
                [
                    "extendedAgentCard",
                    "extensions",
                    "pushNotifications",
                    "streaming"
                ]
            );
        }
    }

    #[test]
    fn a2a_card_name_version_and_description_come_from_the_identity() {
        let card = full_card(&TaskAcceptance::Single);
        assert_eq!(card["name"], "my-agent");
        assert_eq!(card["version"], "0.1.0");
        assert_eq!(card["description"], "Murmur capsule my-agent 0.1.0");
    }

    #[test]
    fn a2a_card_interface_is_the_door_url_over_http_at_1_0() {
        let card = full_card(&TaskAcceptance::Single);
        assert_eq!(
            card["supportedInterfaces"],
            serde_json::json!([{
                "url": "http://localhost:41873",
                "protocolBinding": "JSONRPC",
                "protocolVersion": "1.0",
            }])
        );
        assert_eq!(INTERFACE_PROTOCOL_VERSION, "1.0");
        assert_eq!(jsonrpc_interface_url(&card), Some("http://localhost:41873"));
    }

    #[test]
    fn a2a_card_interface_url_is_not_given_a_second_scheme() {
        for (capsule_url, expected) in [
            ("localhost:41873", "http://localhost:41873"),
            ("localhost:41873/", "http://localhost:41873"),
            ("http://127.0.0.1:1", "http://127.0.0.1:1"),
            ("http://127.0.0.1:1/", "http://127.0.0.1:1"),
            ("https://agent.example.com", "https://agent.example.com"),
        ] {
            let identity = CapsuleIdentity {
                capsule_url: capsule_url.to_string(),
                ..identity()
            };
            let card = build_agent_card(
                &identity,
                &[],
                &CapabilityPolicy::default(),
                &TaskAcceptance::Single,
                DeclaredPlanes::default(),
                false,
                HTTP_TRANSPORT,
            );
            assert_eq!(
                card["supportedInterfaces"][0]["url"], expected,
                "{capsule_url}"
            );
        }
    }

    #[test]
    fn a2a_card_declares_a_public_agent_with_no_extended_card_or_push() {
        for acceptance in &ACCEPTANCES {
            let card = full_card(acceptance);
            assert_eq!(card["securitySchemes"], serde_json::json!({}));
            assert_eq!(card["securityRequirements"], serde_json::json!([]));
            assert_eq!(card["capabilities"]["extendedAgentCard"], false);
            assert_eq!(card["capabilities"]["pushNotifications"], false);
            for modes in ["defaultInputModes", "defaultOutputModes"] {
                assert_eq!(
                    card[modes],
                    serde_json::json!(["text/plain", "application/json"])
                );
            }
        }
    }

    #[test]
    fn a2a_card_skills_are_the_task_skill_only_when_a_task_can_start() {
        for acceptance in [TaskAcceptance::Single, TaskAcceptance::Queue] {
            let card = full_card(&acceptance);
            let skills = card["skills"].as_array().expect("skills is an array");
            assert_eq!(skills.len(), 1, "{acceptance:?}");
            assert_eq!(skills[0]["id"], TASK_SKILL_ID);
            assert_eq!(skills[0]["tags"], serde_json::json!([TASK_SKILL_ID]));
        }
        assert_eq!(TASK_SKILL_ID, "task");
        let card = full_card(&TaskAcceptance::None);
        assert_eq!(card["skills"], serde_json::json!([]));
    }

    fn extension_uris(card: &Value) -> Vec<&str> {
        card["capabilities"]["extensions"]
            .as_array()
            .expect("extensions is an array")
            .iter()
            .map(|e| e["uri"].as_str().expect("each uri is a string"))
            .collect()
    }

    #[test]
    fn a2a_card_extensions_are_the_door_the_capsule_then_the_stream() {
        for acceptance in &ACCEPTANCES {
            let card = full_card(acceptance);
            assert_eq!(
                extension_uris(&card),
                [
                    DOOR_EXTENSION_URI,
                    CAPSULE_EXTENSION_URI,
                    STREAM_EXTENSION_URI
                ]
            );
            for extension in card["capabilities"]["extensions"].as_array().unwrap() {
                assert_eq!(extension["required"], false, "{extension}");
                assert_eq!(
                    keys(extension),
                    ["description", "params", "required", "uri"],
                    "{extension}"
                );
            }
            let stream = extension_params(&card, STREAM_EXTENSION_URI)
                .expect("the card has the stream extension");
            assert_eq!(keys(&Value::Object(stream.clone())), ["frames"]);
        }

        let cards = full_cards(&TaskAcceptance::Single, Some(&bearer_authentication()));
        assert_eq!(
            extension_uris(&cards.public),
            [DOOR_EXTENSION_URI, STREAM_EXTENSION_URI]
        );
        assert_eq!(
            extension_uris(cards.extended.as_ref().expect("an extended card")),
            [
                DOOR_EXTENSION_URI,
                CAPSULE_EXTENSION_URI,
                STREAM_EXTENSION_URI
            ]
        );
    }

    fn stream_frames(card: &Value) -> Vec<&str> {
        extension_params(card, STREAM_EXTENSION_URI).expect("the card has the stream extension")
            ["frames"]
            .as_array()
            .expect("the stream extension's frames are an array")
            .iter()
            .map(|f| f.as_str().expect("each frame is a string"))
            .collect()
    }

    #[test]
    fn stream_extension_frames_are_pinned_per_transport() {
        for (kind, table) in [
            (TransportKind::Http, HTTP_FRAMES),
            (TransportKind::Process, PROCESS_FRAMES),
        ] {
            for (acceptance, expected) in &table {
                for streams_text in [false, true] {
                    let transport = TransportCapabilities { streams_text, kind };
                    assert_eq!(
                        served_frames(acceptance, transport),
                        *expected,
                        "{kind:?} under {acceptance:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn stream_extension_on_the_card_is_served_frames() {
        let authentication = bearer_authentication();
        for acceptance in &ACCEPTANCES {
            for transport in every_transport() {
                let expected = served_frames(acceptance, transport);
                let card = card_for_transport(acceptance, DeclaredPlanes::default(), transport);
                assert_eq!(
                    stream_frames(&card),
                    expected,
                    "{acceptance:?} {transport:?}"
                );

                let cards = build_agent_cards(
                    &identity(),
                    &[],
                    &CapabilityPolicy::default(),
                    acceptance,
                    DeclaredPlanes::default(),
                    false,
                    transport,
                    Some(&authentication),
                );
                assert_eq!(stream_frames(&cards.public), expected);
                assert_eq!(stream_frames(cards.extended.as_ref().unwrap()), expected);
            }
        }
    }

    #[test]
    fn stream_frame_every_variant_is_listed() {
        for kind in TransportKind::ALL {
            let transport = TransportCapabilities {
                streams_text: true,
                kind,
            };
            let frames = served_frames(&TaskAcceptance::Single, transport);
            for frame in StreamFrame::ALL {
                assert!(
                    frames.contains(&frame.wire_name()),
                    "{} is never listed on {kind:?}: {frames:?}",
                    frame.wire_name()
                );
            }
        }
    }

    #[test]
    fn stream_extension_docs_name_every_frame() {
        let page = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/content/reference/streaming-protocol.md"
        ))
        .expect("the streaming protocol reference page");
        assert!(
            page.contains("{ #murmur-stream-v1 }"),
            "no stream extension section"
        );
        assert!(page.contains("params.frames"), "no params.frames key");
        assert!(
            STREAM_EXTENSION_URI.ends_with("/reference/streaming-protocol/#murmur-stream-v1"),
            "the URI addresses the section"
        );
        for frame in StreamFrame::ALL {
            let anchor = format!("{{ #event-{} }}", frame.wire_name());
            assert!(
                page.lines()
                    .any(|line| line.starts_with('#') && line.trim_end().ends_with(&anchor)),
                "no heading for {}",
                frame.wire_name()
            );
        }
    }

    #[test]
    fn a2a_card_capsule_extension_carries_the_session_and_permissions() {
        let card = full_card(&TaskAcceptance::Single);
        let params = capsule_params(&card);
        assert_eq!(
            keys(&Value::Object(params.clone())),
            ["network", "planes", "sessionId", "shell", "tools"]
        );
        assert_eq!(params["sessionId"], "ses_019f01a940ce7761854e768ecbe3d399");
        assert_eq!(params["tools"], serde_json::json!(["bash"]));
        assert_eq!(params["shell"], true);
        assert_eq!(params["network"], true);
        assert_eq!(
            session_id_from_card(&card),
            Some("ses_019f01a940ce7761854e768ecbe3d399")
        );

        let bare = card_for(&TaskAcceptance::Single, DeclaredPlanes::default());
        let params = capsule_params(&bare);
        assert_eq!(params["tools"], serde_json::json!([]));
        assert_eq!(params["shell"], false);
        assert_eq!(params["network"], false);
    }

    #[test]
    fn a2a_card_tools_are_the_llm_visible_artifacts() {
        let hook = InstalledArtifactSummary {
            runtime: murmur_artifact::ArtifactRuntime::Hook,
            ..tool("guard")
        };
        let card = build_agent_card(
            &identity(),
            &[tool("bash"), hook, tool("search")],
            &CapabilityPolicy::default(),
            &TaskAcceptance::Single,
            DeclaredPlanes::default(),
            false,
            HTTP_TRANSPORT,
        );
        assert_eq!(
            capsule_params(&card)["tools"],
            serde_json::json!(["bash", "search"])
        );
    }

    #[test]
    fn a_card_without_the_capsule_extension_still_conforms() {
        for acceptance in &ACCEPTANCES {
            let mut card = full_card(acceptance);
            card["capabilities"]["extensions"]
                .as_array_mut()
                .expect("extensions is an array")
                .retain(|extension| extension["uri"] != CAPSULE_EXTENSION_URI);
            assert_conforms(&card);
            assert!(extension_params(&card, DOOR_EXTENSION_URI).is_some());
            assert!(extension_params(&card, CAPSULE_EXTENSION_URI).is_none());
            assert!(card["capabilities"]["streaming"].is_boolean());
            let serialised = card.to_string();
            for key in ["sessionId", "tools", "shell", "network", "planes"] {
                assert!(
                    !serialised.contains(key),
                    "{key} is extended-card material and belongs to the capsule extension alone: {serialised}"
                );
            }
        }
    }

    /// `peerTasks` is on the door extension, which every card carries, and is always present: a
    /// capsule that never mentions peer tasks says `false` rather than nothing.
    #[test]
    fn a2a_card_door_extension_states_the_peer_task_posture() {
        for accepts_peer_tasks in [false, true] {
            let card = build_agent_card(
                &identity(),
                &[],
                &CapabilityPolicy::default(),
                &TaskAcceptance::Single,
                DeclaredPlanes::default(),
                accepts_peer_tasks,
                HTTP_TRANSPORT,
            );
            assert_eq!(
                keys(&Value::Object(door_params(&card).clone())),
                ["methods", "peerTasks"]
            );
            assert_eq!(door_params(&card)["peerTasks"], accepts_peer_tasks);
            assert_eq!(
                capsule_params(&card).get("peerTasks"),
                None,
                "the posture is public, so it is not capsule-extension material"
            );
            assert_eq!(card["securitySchemes"], serde_json::json!({}));
        }
    }

    /// `authenticated_card` rewrites only `params.methods`, so the posture reaches both cards of
    /// an authenticated door unchanged, and declaring it changes nothing about the door's
    /// security schemes.
    #[test]
    fn a2a_card_peer_task_posture_survives_onto_both_cards_of_an_authenticated_door() {
        let authentication = bearer_authentication();
        let cards_for = |accepts_peer_tasks: bool| {
            build_agent_cards(
                &identity(),
                &[],
                &CapabilityPolicy::default(),
                &TaskAcceptance::Single,
                DeclaredPlanes::default(),
                accepts_peer_tasks,
                HTTP_TRANSPORT,
                Some(&authentication),
            )
        };
        for accepts_peer_tasks in [false, true] {
            let cards = cards_for(accepts_peer_tasks);
            assert_eq!(door_params(&cards.public)["peerTasks"], accepts_peer_tasks);
            let extended = cards
                .extended
                .as_ref()
                .expect("an authenticated door has one");
            assert_eq!(door_params(extended)["peerTasks"], accepts_peer_tasks);
            assert!(door_methods(&cards.public).contains(&"GetExtendedAgentCard"));
        }
        let (refusing, accepting) = (cards_for(false), cards_for(true));
        assert_eq!(
            refusing.public["securitySchemes"],
            accepting.public["securitySchemes"]
        );
        assert_eq!(
            refusing.public["securityRequirements"],
            accepting.public["securityRequirements"]
        );
    }

    fn bearer_authentication() -> NetworkAuthentication {
        NetworkAuthentication {
            scheme: murmur_artifact::AuthenticationScheme::Bearer,
            credentials: vec![murmur_artifact::DoorCredential {
                name: "watcher".to_string(),
                scopes: vec!["GetTask".to_string(), "stream/watch".to_string()],
            }],
        }
    }

    /// The cards for `my-agent` 0.1.0 on port 41873 with `bash` installed, shell and network
    /// granted and `exports.files` declared, as `full_card` builds them.
    fn full_cards(
        acceptance: &TaskAcceptance,
        authentication: Option<&NetworkAuthentication>,
    ) -> AgentCards {
        let policy = CapabilityPolicy {
            shell_allow: vec!["git".to_string()],
            network_allow: vec!["api.example.com".to_string()],
            ..CapabilityPolicy::default()
        };
        build_agent_cards(
            &identity(),
            &[tool("bash")],
            &policy,
            acceptance,
            DeclaredPlanes {
                files: true,
                peer_files: false,
            },
            false,
            HTTP_TRANSPORT,
            authentication,
        )
    }

    #[test]
    fn a2a_card_every_public_and_extended_card_conforms() {
        let plane_sets = [(false, false), (true, false), (false, true), (true, true)];
        let tool_sets: [&[InstalledArtifactSummary]; 2] = [&[], &[tool("bash")]];
        let policies = [
            CapabilityPolicy::default(),
            CapabilityPolicy {
                shell_allow: vec!["git".to_string()],
                network_allow: vec!["api.example.com".to_string()],
                ..CapabilityPolicy::default()
            },
        ];
        let authentication = bearer_authentication();
        let mut checked = 0;
        for authenticated in [None, Some(&authentication)] {
            for acceptance in &ACCEPTANCES {
                for transport in every_transport() {
                    for (files, peer_files) in plane_sets {
                        for tools in tool_sets {
                            for policy in &policies {
                                for accepts_peer_tasks in [false, true] {
                                    let planes = DeclaredPlanes { files, peer_files };
                                    let cards = build_agent_cards(
                                        &identity(),
                                        tools,
                                        policy,
                                        acceptance,
                                        planes,
                                        accepts_peer_tasks,
                                        transport,
                                        authenticated,
                                    );
                                    assert_conforms(&cards.public);
                                    let base = build_agent_card(
                                        &identity(),
                                        tools,
                                        policy,
                                        acceptance,
                                        planes,
                                        accepts_peer_tasks,
                                        transport,
                                    );
                                    match authenticated {
                                        None => {
                                            assert_eq!(cards.public, base);
                                            assert_eq!(cards.extended, None);
                                        }
                                        Some(_) => {
                                            let extended = authenticated_card(base, acceptance);
                                            assert_conforms(&extended);
                                            assert_eq!(cards.extended.as_ref(), Some(&extended));
                                            let mut stripped = extended.clone();
                                            stripped["capabilities"]["extensions"]
                                                .as_array_mut()
                                                .unwrap()
                                                .retain(|e| e["uri"] != CAPSULE_EXTENSION_URI);
                                            assert_eq!(cards.public, stripped);
                                        }
                                    }
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 2 * 3 * 2 * 2 * 4 * 2 * 2 * 2);
    }

    #[test]
    fn a2a_card_authenticated_public_card_is_the_documented_document() {
        let cards = full_cards(&TaskAcceptance::Single, Some(&bearer_authentication()));
        println!("{:#}", cards.public);
        assert_eq!(
            cards.public,
            serde_json::json!({
                "name": "my-agent",
                "description": "Murmur capsule my-agent 0.1.0",
                "version": "0.1.0",
                "supportedInterfaces": [
                    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }
                ],
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extendedAgentCard": true,
                    "extensions": [
                        {
                            "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1",
                            "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.",
                            "required": false,
                            "params": {
                                "methods": ["SendMessage", "SendStreamingMessage", "stream/watch", "GetTask", "CancelTask", "session/stop", "GetExtendedAgentCard"],
                                "peerTasks": false
                            }
                        },
                        {
                            "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
                            "description": "Every server-sent event type this capsule's SendStreamingMessage and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
                            "required": false,
                            "params": {
                                "frames": ["status", "artifact", "text", "thinking", "tool-call-started", "tool-call-progress", "gap", "lagged", "connection-ack", "capsule-closed", "error"]
                            }
                        }
                    ]
                },
                "securitySchemes": {
                    "bearer": {
                        "httpAuthSecurityScheme": {
                            "scheme": "Bearer",
                            "description": "A token this capsule's runtime mints at launch and accepts until the session ends."
                        }
                    }
                },
                "securityRequirements": [ { "schemes": { "bearer": { "list": [] } } } ],
                "defaultInputModes": ["text/plain", "application/json"],
                "defaultOutputModes": ["text/plain", "application/json"],
                "skills": [
                    {
                        "id": "task",
                        "name": "Run a task",
                        "description": "Runs one task given as a text message and reports its outcome.",
                        "tags": ["task"],
                        "securityRequirements": [
                            { "schemes": { "bearer": { "list": ["SendMessage"] } } },
                            { "schemes": { "bearer": { "list": ["SendStreamingMessage"] } } }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn a2a_card_extended_card_is_the_documented_v1_document() {
        let cards = full_cards(&TaskAcceptance::Single, Some(&bearer_authentication()));
        let extended = cards
            .extended
            .expect("an authenticated door has an extended card");
        println!("{extended:#}");
        assert_eq!(
            extended,
            serde_json::json!({
                "name": "my-agent",
                "description": "Murmur capsule my-agent 0.1.0",
                "version": "0.1.0",
                "supportedInterfaces": [
                    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }
                ],
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extendedAgentCard": true,
                    "extensions": [
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1", "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.", "required": false,
                          "params": { "methods": ["SendMessage", "SendStreamingMessage", "stream/watch", "GetTask", "CancelTask", "session/stop", "GetExtendedAgentCard"], "peerTasks": false } },
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1", "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.", "required": false,
                          "params": { "sessionId": "ses_019f01a940ce7761854e768ecbe3d399", "tools": ["bash"], "shell": true, "network": true, "planes": ["files"] } },
                        { "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1", "description": "Every server-sent event type this capsule's SendStreamingMessage and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.", "required": false,
                          "params": { "frames": ["status", "artifact", "text", "thinking", "tool-call-started", "tool-call-progress", "gap", "lagged", "connection-ack", "capsule-closed", "error"] } }
                    ]
                },
                "securitySchemes": {
                    "bearer": {
                        "httpAuthSecurityScheme": {
                            "scheme": "Bearer",
                            "description": "A token this capsule's runtime mints at launch and accepts until the session ends."
                        }
                    }
                },
                "securityRequirements": [ { "schemes": { "bearer": { "list": [] } } } ],
                "defaultInputModes": ["text/plain", "application/json"],
                "defaultOutputModes": ["text/plain", "application/json"],
                "skills": [
                    {
                        "id": "task",
                        "name": "Run a task",
                        "description": "Runs one task given as a text message and reports its outcome.",
                        "tags": ["task"],
                        "securityRequirements": [
                            { "schemes": { "bearer": { "list": ["SendMessage"] } } },
                            { "schemes": { "bearer": { "list": ["SendStreamingMessage"] } } }
                        ]
                    }
                ]
            })
        );
        assert_conforms(&extended);
        assert_eq!(
            session_id_from_card(&extended),
            Some("ses_019f01a940ce7761854e768ecbe3d399")
        );
    }

    #[test]
    fn a2a_card_authenticated_public_card_discloses_no_capsule_material() {
        let authentication = bearer_authentication();
        for acceptance in &ACCEPTANCES {
            let cards = full_cards(acceptance, Some(&authentication));
            let serialised = cards.public.to_string();
            for key in ["sessionId", "tools", "shell", "network", "planes"] {
                assert!(
                    !serialised.contains(key),
                    "{key} belongs to the extended card alone: {serialised}"
                );
            }
            assert_eq!(session_id_from_card(&cards.public), None);
            assert!(extension_params(&cards.public, CAPSULE_EXTENSION_URI).is_none());
        }
    }

    #[test]
    fn a2a_card_a_door_that_starts_no_task_has_no_skill_requirements() {
        let cards = full_cards(&TaskAcceptance::None, Some(&bearer_authentication()));
        assert_eq!(cards.public["skills"], serde_json::json!([]));
        assert_eq!(
            door_methods(&cards.public),
            [
                "stream/watch",
                "GetTask",
                "CancelTask",
                "session/stop",
                "GetExtendedAgentCard"
            ]
        );
        let extended = cards.extended.unwrap();
        assert_eq!(extended["skills"], serde_json::json!([]));
        assert_eq!(
            extended["securityRequirements"],
            serde_json::json!([{"schemes": {"bearer": {"list": []}}}])
        );
    }

    #[test]
    fn a2a_card_public_door_cards_are_build_agent_card() {
        for acceptance in &ACCEPTANCES {
            let cards = full_cards(acceptance, None);
            assert_eq!(cards.public, full_card(acceptance));
            assert_eq!(cards.extended, None);
            assert!(!door_methods(&cards.public).contains(&"GetExtendedAgentCard"));
        }
    }

    #[test]
    fn session_id_from_card_reads_the_capsule_extension_only() {
        let card = full_card(&TaskAcceptance::Single);
        assert_eq!(
            session_id_from_card(&card),
            Some("ses_019f01a940ce7761854e768ecbe3d399")
        );

        let previous = serde_json::json!({
            "name": "my-agent",
            "version": "0.1.0",
            "url": "localhost:41873",
            "session_id": "ses_019f01a940ce7761854e768ecbe3d399",
        });
        assert_eq!(session_id_from_card(&previous), None);

        let mut without = card.clone();
        without["capabilities"]["extensions"]
            .as_array_mut()
            .unwrap()
            .retain(|extension| extension["uri"] != CAPSULE_EXTENSION_URI);
        assert_eq!(session_id_from_card(&without), None);

        let mut empty = card.clone();
        empty["capabilities"]["extensions"][1]["params"]["sessionId"] = Value::from("");
        assert_eq!(session_id_from_card(&empty), None);

        assert_eq!(session_id_from_card(&Value::from("not a card")), None);
    }

    #[test]
    fn jsonrpc_interface_url_picks_the_first_jsonrpc_interface() {
        let card = serde_json::json!({
            "supportedInterfaces": [
                { "url": "https://grpc.example.com", "protocolBinding": "GRPC", "protocolVersion": "1.0" },
                { "url": "http://localhost:1", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" },
                { "url": "http://localhost:2", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" },
            ]
        });
        assert_eq!(jsonrpc_interface_url(&card), Some("http://localhost:1"));

        for card in [
            serde_json::json!({
                "supportedInterfaces": [
                    { "url": "https://grpc.example.com", "protocolBinding": "GRPC", "protocolVersion": "1.0" },
                ]
            }),
            serde_json::json!({ "supportedInterfaces": [] }),
            serde_json::json!({
                "supportedInterfaces": [{ "url": "", "protocolBinding": "JSONRPC", "protocolVersion": "1.0" }]
            }),
            serde_json::json!({ "url": "localhost:41873" }),
        ] {
            assert_eq!(jsonrpc_interface_url(&card), None, "{card}");
        }
    }

    /// `streaming` is the served method AND the transport's answer, so a door that answers
    /// `SendStreamingMessage` over a transport that streams nothing advertises the method and not the
    /// capability. `CancelTask` is served whatever the transport: every transport can be stopped.
    #[test]
    fn card_capabilities_are_the_method_and_the_transport() {
        for (transport, streaming) in [
            (
                TransportCapabilities {
                    streams_text: false,
                    kind: TransportKind::Process,
                },
                false,
            ),
            (HTTP_TRANSPORT, true),
        ] {
            let card = card_for_transport(
                &TaskAcceptance::Single,
                DeclaredPlanes::default(),
                transport,
            );
            assert_eq!(card["capabilities"]["streaming"], streaming, "{card}");
            let methods = door_methods(&card);
            assert!(methods.contains(&"SendStreamingMessage"), "{card}");
            assert!(methods.contains(&"CancelTask"), "{card}");
        }
    }

    /// A door that starts no task streams nothing, whatever its transport can do: neither
    /// task-starting method is served under `TaskAcceptance::None`. `CancelTask` is served
    /// under every acceptance.
    #[test]
    fn a_door_that_starts_no_task_advertises_no_streaming() {
        for transport in [
            HTTP_TRANSPORT,
            TransportCapabilities {
                streams_text: false,
                kind: TransportKind::Process,
            },
        ] {
            let card =
                card_for_transport(&TaskAcceptance::None, DeclaredPlanes::default(), transport);
            assert_eq!(card["capabilities"]["streaming"], false, "{card}");
            assert!(door_methods(&card).contains(&"CancelTask"), "{card}");
        }
    }

    #[test]
    fn rejected_status_carries_no_event_id() {
        let frame = format_rejected_event(&TaskStatusUpdateEvent {
            id: "tsk_1".into(),
            context_id: Some("ctx_1".into()),
            status: StreamStatus {
                state: "rejected".into(),
                message: "task rejected: capsule is busy".into(),
                response: None,
                reopen: None,
            },
            r#final: true,
        });
        assert!(frame.starts_with("event: status\ndata: {"), "{frame}");
        assert!(!frame.contains("id: "), "{frame}");
        assert!(frame.contains(r#""state":"rejected""#), "{frame}");
        assert!(frame.ends_with("\n\n"), "{frame}");
    }

    fn message_send_params(message_id: &str) -> Value {
        serde_json::json!({
            "message": {
                "messageId": message_id,
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        })
    }

    /// The sender of a message the operator sent, carrying no headers.
    fn operator() -> MessageSender {
        member(None)
    }

    /// The sender of a message the formation member `caller` sent, or the operator for `None`.
    fn member(caller: Option<&str>) -> MessageSender {
        MessageSender {
            traceparent: None,
            provenance: TaskProvenance::derive(TaskOrigin::User, None),
            forget_session: false,
            caller_member: caller.map(str::to_string),
        }
    }

    fn response_json(response: &str) -> Value {
        let body = &response[response.find("\r\n\r\n").expect("header terminator") + 4..];
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    /// A closed registry refuses `SendMessage` whatever room its queue has, and the refused
    /// message never reaches the task loop or the registry.
    #[test]
    fn a_closed_registry_answers_message_send_rejected_and_enqueues_nothing() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        assert!(task_registry.lock().unwrap().close_to_new_work().is_empty());
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);

        let response = response_json(&handle_message_send(
            serde_json::json!(1),
            &message_send_params("msg_late"),
            &task_registry,
            &task_tx,
            operator(),
        ));

        assert_eq!(
            response["result"]["task"]["status"]["state"], "TASK_STATE_REJECTED",
            "{response}"
        );
        assert!(
            task_rx.try_recv().is_err(),
            "nothing was handed to the loop"
        );
        let reg = task_registry.lock().unwrap();
        assert_eq!(reg.pending_count, 0);
        assert!(reg.history.is_empty(), "the refused id is not held");
    }

    /// A formation member cancels a task it submitted itself, and gets `-32001` — with nothing
    /// cancelled — for a task another member or the operator submitted. Its own task, once ended,
    /// gets `-32002`.
    #[test]
    fn a_formation_member_cancels_only_the_tasks_it_submitted() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        {
            let mut reg = task_registry.lock().unwrap();
            reg.enqueue("tsk_mine", "ctx_mine");
            reg.record_submitter("tsk_mine", "planner");
            reg.enqueue("tsk_theirs", "ctx_theirs");
            reg.record_submitter("tsk_theirs", "reviewer");
            reg.enqueue("tsk_operator", "ctx_operator");
        }
        let live = LiveDelegations::new();
        let cancel = |task_id: &str| {
            response_json(&handle_tasks_cancel(
                serde_json::json!(1),
                &serde_json::json!({"id": task_id}),
                &task_registry,
                Some("planner"),
                None,
                &live,
            ))
        };

        let canceled = cancel("tsk_mine");
        assert_eq!(
            canceled["result"]["status"]["state"], "TASK_STATE_CANCELED",
            "{canceled}"
        );
        for other in ["tsk_theirs", "tsk_operator"] {
            let refused = cancel(other);
            assert_eq!(refused["error"]["code"], -32001, "{refused}");
            assert_eq!(refused["error"]["message"], "Task not found", "{refused}");
            let state = task_registry.lock().unwrap().get_task(other).unwrap();
            assert_ne!(
                serde_json::to_value(&state).unwrap()["status"]["state"],
                "TASK_STATE_CANCELED",
                "{other} was cancelled by a member that did not submit it"
            );
        }

        // Its own task, once ended, is not cancelable, and is left as it ended.
        let ended = cancel("tsk_mine");
        assert_eq!(ended["error"]["code"], -32002, "{ended}");
        let info = &ended["error"]["data"][0];
        assert_eq!(info["reason"], "TASK_NOT_CANCELABLE", "{ended}");
        assert_eq!(
            info["metadata"],
            serde_json::json!({"taskId": "tsk_mine", "state": "TASK_STATE_CANCELED"})
        );
        let unknown = cancel("tsk_never");
        assert_eq!(unknown["error"]["code"], -32001, "{unknown}");
        assert_eq!(
            unknown["error"]["data"][0]["metadata"],
            serde_json::json!({"taskId": "tsk_never"})
        );
    }

    /// A task refused when the session closed reads `rejected` over `GetTask`.
    #[test]
    fn tasks_get_serves_a_refused_task_as_rejected() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        task_registry
            .lock()
            .unwrap()
            .enqueue("tsk_refused", "ctx_refused");
        task_registry.lock().unwrap().close_to_new_work();

        let response = handle_tasks_get(
            serde_json::json!(1),
            &serde_json::json!({"id": "tsk_refused"}),
            &task_registry,
            None,
        );

        assert!(
            response.contains(r#""state":"TASK_STATE_REJECTED""#),
            "{response}"
        );
        let response = response_json(&response);
        assert_eq!(response["result"]["id"], "tsk_refused");
        assert_eq!(response["result"]["contextId"], "ctx_refused");
    }

    /// `SendMessage` with `message` as the formation member `caller`, or the operator, answered.
    fn send(
        registry: &Arc<Mutex<TaskRegistry>>,
        task_tx: &mpsc::Sender<IncomingTask>,
        caller: Option<&str>,
        message: Value,
    ) -> Value {
        response_json(&handle_message_send(
            serde_json::json!(1),
            &serde_json::json!({ "message": message }),
            registry,
            task_tx,
            member(caller),
        ))
    }

    fn naming(task_id: &str, context_id: Option<&str>, text: &str) -> Value {
        let mut message = serde_json::json!({"messageId": "msg_reply", "role": "ROLE_USER",
            "taskId": task_id, "parts": [{"text": text}]});
        if let Some(context_id) = context_id {
            message["contextId"] = Value::from(context_id);
        }
        message
    }

    /// A registry holding `tsk_wait`, which `planner` submitted and which waits for input in
    /// `ctx_wait`, with `tsk_done` completed and `tsk_queued` submitted, both by the operator. Its queue is full, so it has no room for a new task.
    fn routing_registry() -> (Arc<Mutex<TaskRegistry>>, oneshot::Receiver<String>) {
        let mut reg = TaskRegistry::new(4, TaskAcceptance::Queue);
        reg.enqueue("tsk_done", "ctx_done");
        reg.start_task(
            "tsk_done".to_string(),
            "ctx_done".to_string(),
            crate::lanes::TaskLane::User,
        );
        reg.finish_task(TaskState::Completed);
        reg.record_ending("tsk_done", "done", Some("four"), false, &[]);
        reg.enqueue("tsk_wait", "ctx_wait");
        reg.record_submitter("tsk_wait", "planner");
        reg.start_task(
            "tsk_wait".to_string(),
            "ctx_wait".to_string(),
            crate::lanes::TaskLane::User,
        );
        let (tx, rx) = oneshot::channel();
        reg.set_input_required("tsk_wait", "which branch?".to_string(), tx)
            .unwrap();
        reg.enqueue("tsk_queued", "ctx_q");
        reg.queue_depth = 1;
        (Arc::new(Mutex::new(reg)), rx)
    }

    fn error_of(answer: &Value) -> (i64, Value) {
        (
            answer["error"]["code"].as_i64().unwrap_or_default(),
            answer["error"]["data"][0]["metadata"].clone(),
        )
    }

    /// Each row of the router's table, against one registry.
    #[test]
    fn a_message_is_routed_by_the_task_it_names() {
        let (registry, mut waiter) = routing_registry();
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let state = |task_id: &str| {
            registry
                .lock()
                .unwrap()
                .get_task(task_id)
                .unwrap()
                .status
                .state
        };

        // No task, or `""`: a new task, refused here because the registry has no room.
        for task_id in [None, Some("")] {
            let mut message = serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                "parts": [{"text": "hi"}]});
            if let Some(task_id) = task_id {
                message["taskId"] = Value::from(task_id);
            }
            let answer = send(&registry, &task_tx, None, message);
            assert_eq!(
                answer["result"]["task"]["status"]["state"], "TASK_STATE_REJECTED",
                "{answer}"
            );
            assert_eq!(state("tsk_wait"), TaskState::InputRequired);
        }

        // An unknown id, and the operator's view of a task is not a member's.
        let answer = send(&registry, &task_tx, None, naming("tsk_nope", None, "x"));
        assert_eq!(
            error_of(&answer),
            (-32001, serde_json::json!({"taskId": "tsk_nope"}))
        );
        let answer = send(
            &registry,
            &task_tx,
            Some("reviewer"),
            naming("tsk_wait", None, "x"),
        );
        assert_eq!(
            error_of(&answer),
            (-32001, serde_json::json!({"taskId": "tsk_wait"}))
        );

        // An ended task, and a live one that waits for nothing.
        let answer = send(&registry, &task_tx, None, naming("tsk_done", None, "x"));
        assert_eq!(
            error_of(&answer),
            (
                -32004,
                serde_json::json!({"taskId": "tsk_done", "state": "TASK_STATE_COMPLETED"})
            )
        );
        assert!(
            answer["error"]["message"]
                .as_str()
                .unwrap()
                .contains("without taskId"),
            "{answer}"
        );
        let answer = send(&registry, &task_tx, None, naming("tsk_queued", None, "x"));
        assert_eq!(
            error_of(&answer),
            (
                -32004,
                serde_json::json!({"taskId": "tsk_queued", "state": "TASK_STATE_SUBMITTED"})
            )
        );

        // The waiting task in another context.
        let answer = send(
            &registry,
            &task_tx,
            None,
            naming("tsk_wait", Some("ctx_other"), "x"),
        );
        assert_eq!(answer["error"]["code"], -32602, "{answer}");
        let text = answer["error"]["message"].as_str().unwrap();
        assert!(
            text.contains("ctx_other") && text.contains("ctx_wait"),
            "{text}"
        );
        assert!(waiter.try_recv().is_err(), "nothing was delivered yet");

        // The waiting task, by the member that submitted it, in its own context.
        let answer = send(
            &registry,
            &task_tx,
            Some("planner"),
            naming("tsk_wait", Some("ctx_wait"), "main"),
        );
        assert_eq!(
            answer["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
            "{answer}"
        );
        assert_eq!(answer["result"]["task"]["id"], "tsk_wait");
        assert_eq!(waiter.try_recv().unwrap(), "main");
        assert!(task_rx.try_recv().is_err(), "no task was started");

        // Now working, it takes no further message.
        let answer = send(&registry, &task_tx, None, naming("tsk_wait", None, "x"));
        assert_eq!(
            error_of(&answer),
            (
                -32004,
                serde_json::json!({"taskId": "tsk_wait", "state": "TASK_STATE_WORKING"})
            )
        );
    }

    /// A message that names no task starts a new one and never reaches the one waiting for input.
    #[test]
    fn a_message_naming_no_task_never_reaches_a_waiting_one() {
        let (registry, mut waiter) = routing_registry();
        registry.lock().unwrap().queue_depth = 4;
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let answer = send(
            &registry,
            &task_tx,
            None,
            serde_json::json!({"messageId": "m", "role": "ROLE_USER", "parts": [{"text": "hi"}]}),
        );
        assert_eq!(
            answer["result"]["task"]["status"]["state"], "TASK_STATE_SUBMITTED",
            "{answer}"
        );
        assert!(answer["result"].get("id").is_none(), "{answer}");
        assert!(waiter.try_recv().is_err());
        let incoming = task_rx.try_recv().unwrap();
        assert_eq!(incoming.message_text, "hi");
        assert_eq!(incoming.part_kinds, ["text"]);
    }

    /// A continuation's text, and a new task's, carry the reference block, rendered with the
    /// sender's view of the registry.
    #[test]
    fn referenced_tasks_reach_the_task_as_the_sender_may_see_them() {
        let (registry, mut waiter) = routing_registry();
        registry.lock().unwrap().queue_depth = 4;
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let mut message = naming("tsk_wait", None, "go");
        message["referenceTaskIds"] = serde_json::json!(["tsk_done", "tsk_done", "tsk_none"]);
        let answer = send(&registry, &task_tx, Some("planner"), message);
        assert_eq!(
            answer["result"]["task"]["status"]["state"], "TASK_STATE_WORKING",
            "{answer}"
        );
        assert_eq!(
            waiter.try_recv().unwrap(),
            "go\n\nReferenced tasks:\n\
             - tsk_done: not a task this capsule holds\n\
             - tsk_none: not a task this capsule holds"
        );

        let answer = send(
            &registry,
            &task_tx,
            None,
            serde_json::json!({"messageId": "m", "role": "ROLE_USER",
                "parts": [{"text": "sum"}, {"data": [1, 2]}],
                "referenceTaskIds": ["tsk_done", "tsk_done"]}),
        );
        assert_eq!(
            answer["result"]["task"]["status"]["state"], "TASK_STATE_SUBMITTED",
            "{answer}"
        );
        let incoming = task_rx.try_recv().unwrap();
        assert_eq!(
            incoming.message_text,
            "sum\n```data\n[\n  1,\n  2\n]\n```\n\nReferenced tasks:\n\
             - tsk_done: completed\n```response\nfour\n```"
        );
        assert_eq!(incoming.reference_task_ids, ["tsk_done"]);
        assert_eq!(incoming.part_kinds, ["text", "data"]);
    }

    /// A message the door cannot read in full is refused before it is routed: no task, and
    /// nothing delivered to a waiting one.
    #[test]
    fn a_refused_message_starts_nothing_and_delivers_nothing() {
        let (registry, mut waiter) = routing_registry();
        registry.lock().unwrap().queue_depth = 4;
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let held = registry.lock().unwrap().history.len();
        for (parts, code) in [
            (
                serde_json::json!([{"url": "https://example.com/x.pdf"}]),
                -32005,
            ),
            (serde_json::json!([{"raw": "aGk="}]), -32005),
            (serde_json::json!([{"text": 1}]), -32602),
        ] {
            for task_id in [None, Some("tsk_wait")] {
                let mut message =
                    serde_json::json!({"messageId": "m", "role": "ROLE_USER", "parts": parts});
                if let Some(task_id) = task_id {
                    message["taskId"] = Value::from(task_id);
                }
                let answer = send(&registry, &task_tx, Some("planner"), message);
                assert_eq!(answer["error"]["code"], code, "{answer}");
            }
        }
        assert!(waiter.try_recv().is_err());
        assert!(task_rx.try_recv().is_err());
        let reg = registry.lock().unwrap();
        assert_eq!(reg.history.len(), held);
        assert_eq!(
            reg.get_task("tsk_wait").unwrap().status.state,
            TaskState::InputRequired
        );
    }

    /// `GetTask` names the task it reads, and nothing else stands in for one.
    #[test]
    fn get_task_requires_an_id() {
        let (registry, _waiter) = routing_registry();
        for params in [
            serde_json::json!({}),
            serde_json::json!({"id": ""}),
            serde_json::json!({"id": 7}),
        ] {
            let answer = response_json(&handle_tasks_get(
                serde_json::json!(1),
                &params,
                &registry,
                None,
            ));
            assert_eq!(answer["error"]["code"], -32602, "{answer}");
            assert_eq!(answer["error"]["message"], "GetTask requires an id");
        }
        let answer = response_json(&handle_tasks_get(
            serde_json::json!(1),
            &serde_json::json!({"id": "tsk_done", "historyLength": 3}),
            &registry,
            None,
        ));
        assert_eq!(answer["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    }

    /// A completion is acknowledged with a message from the agent, in the completion's context.
    #[test]
    fn a_completion_is_acknowledged_with_an_agent_message() {
        let params = serde_json::json!({"message": {"contextId": "ctx_parent"}});
        assert_eq!(
            completion_received(&params, "dlg_1"),
            serde_json::json!({
                "messageId": "msg_dlg_1_received",
                "contextId": "ctx_parent",
                "role": "ROLE_AGENT",
                "parts": [{"data": {"delegation_id": "dlg_1", "received": true},
                           "mediaType": "application/json"}],
            })
        );
        assert!(completion_received(&serde_json::json!({}), "dlg_1")
            .get("contextId")
            .is_none());
        let answer = serde_json::json!({ "message": completion_received(&params, "dlg_1") });
        crate::a2a_conformance::check_message(&answer, "lf.a2a.v1.SendMessageResponse")
            .unwrap_or_else(|errors| panic!("{answer}: {errors:?}"));
    }

    /// A `SendStreamingMessage` the door refuses because the session is closing is told so, in the
    /// same unnumbered final frame a busy refusal uses; a busy refusal keeps its own message.
    #[tokio::test]
    async fn message_stream_refused_by_a_closed_registry_says_the_session_is_closing() {
        for (close, expected) in [
            (true, crate::a2a::REJECTED_SESSION_CLOSING_MESSAGE),
            (false, crate::a2a::REJECTED_BUSY_MESSAGE),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (client, lines) = spawn_sse_line_reader(
                listener.local_addr().unwrap(),
                std::time::Duration::from_secs(30),
            );
            let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(4);
            let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
            let task_registry = Arc::new(Mutex::new(TaskRegistry::new(1, TaskAcceptance::Single)));
            if close {
                task_registry.lock().unwrap().close_to_new_work();
            } else {
                task_registry
                    .lock()
                    .unwrap()
                    .enqueue("tsk_busy", "ctx_busy");
            }
            let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
            let req = JsonRpcRequest {
                id: serde_json::json!(1),
                method: "SendStreamingMessage".to_string(),
                params: message_send_params("msg_refused"),
            };

            let (sock, _peer) = listener.accept().await.unwrap();
            let (_read_half, write_half) = sock.into_split();
            let (_closing_tx, closing) = watch::channel(false);
            handle_message_stream(
                write_half,
                req,
                &task_registry,
                &task_tx,
                operator(),
                None,
                sse_tx,
                Arc::clone(&sse_buffer),
                closing,
            )
            .await;
            let mut received = String::new();
            collect_sse_lines(&lines, &mut received, None).await;
            client.join().unwrap();

            let body = sse_body(&received);
            let data = body
                .strip_prefix("event: status\ndata: ")
                .and_then(|rest| rest.strip_suffix("\n\n"))
                .unwrap_or_else(|| panic!("one unnumbered status frame expected:\n{body}"));
            let frame: Value = serde_json::from_str(data).unwrap();
            assert_eq!(frame["status"]["state"], "rejected", "{frame}");
            assert_eq!(frame["status"]["message"], expected, "{frame}");
            assert_eq!(frame["final"], true, "{frame}");
            assert!(
                task_rx.try_recv().is_err(),
                "nothing was handed to the loop"
            );
            assert!(
                matches!(
                    sse_buffer.lock().unwrap().replay_from(0),
                    ReplayResult::Complete(frames) if frames.is_empty()
                ),
                "a door refusal is never buffered"
            );
        }
    }

    #[test]
    fn door_method_served_methods_are_exactly_what_resolve_serves() {
        for acceptance in &ACCEPTANCES {
            for authenticated in [false, true] {
                let served = served_methods(acceptance, authenticated);
                for method in DoorMethod::ALL {
                    let resolved =
                        DoorMethod::resolve(method.wire_name(), acceptance, authenticated);
                    assert_eq!(
                        served.contains(&method.wire_name()),
                        resolved.is_some(),
                        "{} under {acceptance:?}, authenticated {authenticated}",
                        method.wire_name()
                    );
                    if let Some(resolved) = resolved {
                        assert_eq!(resolved, method, "{acceptance:?}");
                    }
                }
                let listed_in_all_order: Vec<&str> = DoorMethod::ALL
                    .into_iter()
                    .map(DoorMethod::wire_name)
                    .filter(|name| served.contains(name))
                    .collect();
                assert_eq!(served, listed_in_all_order, "{acceptance:?}");
            }
        }
    }

    #[test]
    fn door_method_acceptance_none_serves_no_task_starting_method() {
        assert_eq!(
            served_methods(&TaskAcceptance::None, false),
            ["stream/watch", "GetTask", "CancelTask", "session/stop"]
        );
        for acceptance in [TaskAcceptance::Single, TaskAcceptance::Queue] {
            assert_eq!(
                served_methods(&acceptance, false),
                [
                    "SendMessage",
                    "SendStreamingMessage",
                    "stream/watch",
                    "GetTask",
                    "CancelTask",
                    "session/stop"
                ]
            );
        }
    }

    #[test]
    fn door_method_only_an_authenticated_door_serves_the_extended_card() {
        for acceptance in &ACCEPTANCES {
            assert_eq!(
                DoorMethod::resolve("GetExtendedAgentCard", acceptance, false),
                None
            );
            assert_eq!(
                DoorMethod::resolve("GetExtendedAgentCard", acceptance, true),
                Some(DoorMethod::GetExtendedAgentCard)
            );
            let mut expected = served_methods(acceptance, false);
            expected.push("GetExtendedAgentCard");
            assert_eq!(served_methods(acceptance, true), expected);
        }
    }

    #[test]
    fn door_method_resolve_matches_names_exactly() {
        for acceptance in &ACCEPTANCES {
            for name in [
                "tasks/list",
                "",
                "getTask",
                "GETTASK",
                "message/send",
                "message/stream",
                "tasks/get",
                "tasks/cancel",
                "agent/getAuthenticatedExtendedCard",
                " GetTask",
                "GetTask ",
                "GetTask\n",
                "session/stop/",
            ] {
                for authenticated in [false, true] {
                    assert_eq!(
                        DoorMethod::resolve(name, acceptance, authenticated),
                        None,
                        "{name:?} under {acceptance:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn door_method_card_advertises_what_the_resolver_serves() {
        for acceptance in &ACCEPTANCES {
            let card = card_for(acceptance, DeclaredPlanes::default());
            let methods = door_methods(&card);
            assert_eq!(methods, served_methods(acceptance, false), "{acceptance:?}");
            assert_eq!(
                card["capabilities"]["streaming"],
                methods.contains(&"SendStreamingMessage"),
                "{acceptance:?}"
            );
        }
    }

    #[test]
    fn door_method_card_lists_declared_planes_in_order() {
        let cases = [
            (false, false, serde_json::json!([])),
            (true, false, serde_json::json!(["files"])),
            (false, true, serde_json::json!(["peer_files"])),
            (true, true, serde_json::json!(["files", "peer_files"])),
        ];
        for (files, peer_files, expected) in cases {
            let card = card_for(
                &TaskAcceptance::Single,
                DeclaredPlanes { files, peer_files },
            );
            assert_eq!(capsule_params(&card)["planes"], expected);
        }
    }

    #[tokio::test]
    async fn bind_local_port_os_assigned_when_none() {
        let (listener, port) = bind_local_port("127.0.0.1", None).await.unwrap();
        assert!(port > 0);
        // Listener is bound — a second bind on the same port should fail.
        let err = bind_local_port("127.0.0.1", Some(port)).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PortInUse { port: p } if p == port),
            "expected PortInUse, got {err}"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn bind_local_port_uses_specified_port() {
        // A port no other process can be handed between the claim and the bind below.
        let port = crate::pinned_port::claim();
        let (_, bound_port) = bind_local_port("127.0.0.1", Some(port)).await.unwrap();
        assert_eq!(bound_port, port);
    }

    #[tokio::test]
    async fn bind_local_port_returns_port_in_use_when_taken() {
        let (_listener, port) = bind_local_port("127.0.0.1", None).await.unwrap();
        // _listener is still alive — the port is occupied.
        let err = bind_local_port("127.0.0.1", Some(port)).await.unwrap_err();
        assert!(
            matches!(err, RuntimeError::PortInUse { port: p } if p == port),
            "expected PortInUse {{ port: {port} }}, got {err}"
        );
    }

    /// The door runs on the runtime's worker pool while the agent loop holds its own thread. This
    /// reproduces that shape — `handle_stream_watch` spawned onto the pool, as `serve_http` spawns
    /// every connection, beside a `LocalSet` task that holds the calling thread with
    /// `std::thread::sleep` across a due tick — and asserts what a client actually reads: a
    /// heartbeat while the thread is still held.
    ///
    /// `protocol_page_says_the_heartbeat_keeps_its_cadence_during_a_turn` holds the protocol page to
    /// this behaviour and must change with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stream_watch_heartbeat_continues_while_session_thread_is_blocked() {
        use std::time::{Duration, Instant};

        // Longer than one heartbeat interval, so a tick falls due inside the blocked stretch.
        const BLOCK: Duration = Duration::from_secs(20);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Read on its own OS thread, so what the client records does not depend on the thread
        // about to be held.
        let (client, line_rx) = spawn_sse_line_reader(addr, BLOCK + Duration::from_secs(10));

        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(16);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (_closing_tx, closing) = watch::channel(false);
        let local = tokio::task::LocalSet::new();
        let (connected_at, blocked_at, watcher) = local
            .run_until(async {
                let (sock, _peer) = listener.accept().await.unwrap();
                let connected_at = Instant::now();
                // Hold the read half so the socket stays open for the whole measurement.
                let (read_half, write_half) = sock.into_split();
                let sse = sse_tx.clone();
                let buffer = Arc::clone(&sse_buffer);
                let watcher = tokio::spawn(async move {
                    let _read_half = read_half;
                    handle_stream_watch(
                        write_half,
                        Some(0),
                        sse,
                        buffer,
                        "stateless".to_string(),
                        closing,
                    )
                    .await;
                });

                // Let the handler write its preamble and settle into the receive loop before
                // the thread is held.
                tokio::time::sleep(Duration::from_millis(250)).await;

                let blocked_at = Instant::now();
                tokio::task::spawn_local(async { std::thread::sleep(BLOCK) })
                    .await
                    .unwrap();
                (connected_at, blocked_at, watcher)
            })
            .await;

        // Ending the handler closes the socket, which ends the reader.
        watcher.abort();
        let _ = watcher.await;
        drop(local);
        drop(sse_tx);
        client.join().unwrap();

        let released_at = blocked_at + BLOCK;
        let heartbeats: Vec<Instant> = line_rx
            .try_iter()
            .filter(|(_, line)| line.trim_end() == ":heartbeat")
            .map(|(at, _)| at)
            .collect();

        let arrivals: Vec<Duration> = heartbeats
            .iter()
            .map(|at| at.duration_since(connected_at))
            .collect();
        // Reported so the measurement can be re-read rather than re-derived.
        eprintln!(
            "[heartbeat] thread held from {:?} to {:?} after connect; heartbeats arrived at {arrivals:?}",
            blocked_at.duration_since(connected_at),
            released_at.duration_since(connected_at)
        );
        let Some(&first) = heartbeats.first() else {
            panic!("no heartbeat arrived while the session thread was blocked");
        };
        assert!(
            first < released_at,
            "the first heartbeat waited for the session thread: it arrived at {:?} after \
             connect, and the thread was released at {:?} after connect",
            first.duration_since(connected_at),
            released_at.duration_since(connected_at)
        );
        assert!(
            first.duration_since(connected_at) < SSE_HEARTBEAT_INTERVAL + Duration::from_secs(2),
            "the tick due at {SSE_HEARTBEAT_INTERVAL:?} was late: the first heartbeat arrived at \
             {:?} after connect",
            first.duration_since(connected_at)
        );
    }

    /// Reads an SSE connection on its own OS thread, forwarding each line with the moment it
    /// arrived. The sender drops when the server closes the socket or a read times out.
    fn spawn_sse_line_reader(
        addr: std::net::SocketAddr,
        read_timeout: std::time::Duration,
    ) -> (
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<(std::time::Instant, String)>,
    ) {
        use std::io::{BufRead, BufReader};

        let (line_tx, line_rx) = std::sync::mpsc::channel();
        let client = std::thread::spawn(move || {
            let sock = std::net::TcpStream::connect(addr).unwrap();
            sock.set_read_timeout(Some(read_timeout)).unwrap();
            let mut reader = BufReader::new(sock);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if line_tx.send((std::time::Instant::now(), line)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (client, line_rx)
    }

    /// Appends received lines to `received` until it contains `needle`, or, with `None`, until the
    /// server closes the connection. Sleeps between polls so the handler under test can run.
    async fn collect_sse_lines(
        lines: &std::sync::mpsc::Receiver<(std::time::Instant, String)>,
        received: &mut String,
        needle: Option<&str>,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            loop {
                match lines.try_recv() {
                    Ok((_, line)) => received.push_str(&line),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        assert!(
                            needle.is_none_or(|n| received.contains(n)),
                            "the connection closed before {needle:?} arrived; received:\n{received}"
                        );
                        return;
                    }
                }
            }
            if needle.is_some_and(|n| received.contains(n)) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {needle:?}; received:\n{received}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Waits until the handler under test has subscribed, so it is the channel's only receiver.
    async fn wait_for_subscriber(sse_tx: &SseBroadcast) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while sse_tx.receiver_count() != 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "the handler never subscribed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    /// The body of an HTTP response: everything after the header block.
    fn sse_body(received: &str) -> &str {
        let at = received
            .find("\r\n\r\n")
            .unwrap_or_else(|| panic!("no header terminator in:\n{received}"));
        &received[at + 4..]
    }

    /// Capacity of the broadcast channel in the lag tests; each handler's queue holds this many.
    const LAG_QUEUE: usize = 4;
    /// Frames beyond [`LAG_QUEUE`] in the burst, and so the count the `lagged` frame must carry.
    const LAG_MISSED: usize = 3;

    /// The burst sent to a lagging connection: `LAG_QUEUE + LAG_MISSED` non-final frames with ids
    /// from 1.
    fn lag_burst() -> Vec<String> {
        (1..=(LAG_QUEUE + LAG_MISSED) as u64)
            .map(|id| {
                format_sse_event(
                    id,
                    StreamFrame::Text,
                    &format!("{{\"n\":{id},\"final\":false}}"),
                )
            })
            .collect()
    }

    fn assert_single_lagged_frame(body: &str) {
        let expected = format!("event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n");
        assert_eq!(
            body.matches("event: lagged").count(),
            1,
            "expected exactly one lagged frame in:\n{body}"
        );
        assert!(
            body.contains(&expected),
            "the lagged frame is not byte-exact `{expected:?}` in:\n{body}"
        );
        let before = &body[..body.find(&expected).unwrap()];
        assert!(
            before.is_empty() || before.ends_with("\n\n"),
            "the lagged frame does not start its own block, so it carries an id line:\n{body}"
        );
    }

    fn assert_buffer_has_no_lagged_frame(sse_buffer: &Arc<Mutex<SseEventBuffer>>) {
        let events = match sse_buffer.lock().unwrap().replay_from(0) {
            ReplayResult::Complete(events) => events,
            ReplayResult::WithGap { events, .. } => events,
        };
        assert!(
            events.iter().all(|e| !e.contains("event: lagged")),
            "a lagged frame entered the replay buffer"
        );
    }

    /// A `stream/watch` connection that falls behind is told how many live frames it lost, in one
    /// id-less `lagged` frame written before the oldest retained frame, and stays open.
    ///
    /// Current-thread flavour: the burst is sent with no await between sends, so the handler cannot
    /// drain its queue part-way through and the lost count is exact.
    #[tokio::test]
    async fn stream_watch_writes_lagged_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (_closing_tx, closing) = watch::channel(false);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_stream_watch(
                write_half,
                Some(0),
                sse,
                buffer,
                "stateless".to_string(),
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        let burst = lag_burst();
        for frame in &burst {
            sse_tx.send(Arc::new(frame.clone())).unwrap();
        }
        let retained = &burst[LAG_MISSED..];
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some(retained.last().unwrap())).await;

        let last = format_sse_event(100, StreamFrame::Text, "{\"n\":\"last\",\"final\":false}");
        sse_tx.send(Arc::new(last.clone())).unwrap();
        collect_sse_lines(&lines, &mut received, Some(&last)).await;

        watcher.abort();
        let _ = watcher.await;
        drop(sse_tx);
        client.join().unwrap();

        let body = sse_body(&received);
        assert_single_lagged_frame(body);
        let expected = format!(
            "event: connection-ack\ndata: {{\"role\":\"observer\",\"conversation_mode\":\"stateless\"}}\n\n\
             event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n{}{last}",
            retained.concat()
        );
        assert_eq!(body, expected);
        assert_buffer_has_no_lagged_frame(&sse_buffer);
    }

    /// A `SendStreamingMessage` connection that falls behind is told how many live frames it lost, keeps
    /// receiving — another task's final status included — and closes on its own task's first live
    /// `final` status.
    ///
    /// Current-thread flavour for the same reason as `stream_watch_writes_lagged_frame`.
    #[tokio::test]
    async fn message_stream_writes_lagged_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let req = JsonRpcRequest {
            id: serde_json::json!(1),
            method: "SendStreamingMessage".to_string(),
            params: serde_json::json!({
                "message": {
                    "messageId": "msg_lagged",
                    "role": "ROLE_USER",
                    "parts": [{"text": "hello"}]
                }
            }),
        };

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (_closing_tx, closing) = watch::channel(false);
        let handler = tokio::spawn(async move {
            let _read_half = read_half;
            handle_message_stream(
                write_half,
                req,
                &task_registry,
                &task_tx,
                operator(),
                None,
                sse,
                buffer,
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        let burst = lag_burst();
        for frame in &burst {
            sse_tx.send(Arc::new(frame.clone())).unwrap();
        }
        let retained = &burst[LAG_MISSED..];
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some(retained.last().unwrap())).await;

        let task_id = task_rx
            .recv()
            .await
            .expect("the task was submitted")
            .task_id;
        let other_final = format_sse_event(
            100,
            StreamFrame::Status,
            "{\"id\":\"tsk_other\",\"final\":true}",
        );
        sse_tx.send(Arc::new(other_final.clone())).unwrap();
        let final_status = format_sse_event(
            101,
            StreamFrame::Status,
            &format!("{{\"id\":\"{task_id}\",\"final\":true}}"),
        );
        sse_tx.send(Arc::new(final_status.clone())).unwrap();
        collect_sse_lines(&lines, &mut received, None).await;

        handler.await.unwrap();
        client.join().unwrap();

        let body = sse_body(&received);
        assert_single_lagged_frame(body);
        let expected = format!(
            "event: lagged\ndata: {{\"missed\":{LAG_MISSED}}}\n\n{}{other_final}{final_status}",
            retained.concat()
        );
        assert_eq!(body, expected);
        assert_buffer_has_no_lagged_frame(&sse_buffer);
    }

    /// The session's last frames are broadcast just before the door closes. A `SendStreamingMessage`
    /// handler that has not yet been scheduled when the close lands still writes the final status
    /// it was sent, and then ends the connection.
    ///
    /// Current-thread flavour: the frame and the close are sent with no await between them, so
    /// the handler sees both at once — the shape of a busy host, where it is scheduled late.
    #[tokio::test]
    async fn message_stream_writes_queued_frames_before_the_door_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let req = JsonRpcRequest {
            id: serde_json::json!(1),
            method: "SendStreamingMessage".to_string(),
            params: serde_json::json!({
                "message": {
                    "messageId": "msg_closing",
                    "role": "ROLE_USER",
                    "parts": [{"text": "hello"}]
                }
            }),
        };

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (closing_tx, closing) = watch::channel(false);
        let handler = tokio::spawn(async move {
            let _read_half = read_half;
            handle_message_stream(
                write_half,
                req,
                &task_registry,
                &task_tx,
                operator(),
                None,
                sse,
                buffer,
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;
        let task_id = task_rx
            .recv()
            .await
            .expect("the task was submitted")
            .task_id;

        let final_status = format_sse_event(
            1,
            StreamFrame::Status,
            &format!("{{\"id\":\"{task_id}\",\"final\":true}}"),
        );
        sse_tx.send(Arc::new(final_status.clone())).unwrap();
        closing_tx.send(true).unwrap();

        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        handler.await.unwrap();
        client.join().unwrap();

        assert_eq!(sse_body(&received), final_status);
    }

    /// A `stream/watch` observer is written every frame it was sent before the door closed, rather
    /// than having its socket dropped mid-queue, and the stream then ends without
    /// `capsule-closed`, as it does for any capsule that exits.
    #[tokio::test]
    async fn stream_watch_writes_queued_frames_before_the_door_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<String>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (closing_tx, closing) = watch::channel(false);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_stream_watch(
                write_half,
                Some(0),
                sse,
                buffer,
                "stateless".to_string(),
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        let text = format_sse_event(1, StreamFrame::Text, "{\"n\":\"last\",\"final\":false}");
        let final_status = format_sse_event(
            2,
            StreamFrame::Status,
            "{\"id\":\"tsk_closing\",\"final\":true}",
        );
        sse_tx.send(Arc::new(text.clone())).unwrap();
        sse_tx.send(Arc::new(final_status.clone())).unwrap();
        closing_tx.send(true).unwrap();

        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        watcher.await.unwrap();
        client.join().unwrap();

        let expected = format!(
            "event: connection-ack\ndata: {{\"role\":\"observer\",\"conversation_mode\":\"stateless\"}}\n\n\
             {text}{final_status}"
        );
        assert_eq!(sse_body(&received), expected);
    }

    const PROTOCOL_PAGE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/content/reference/streaming-protocol.md"
    );

    fn protocol_page() -> String {
        std::fs::read_to_string(PROTOCOL_PAGE_PATH)
            .unwrap_or_else(|e| panic!("cannot read {PROTOCOL_PAGE_PATH}: {e}"))
    }

    /// The section of the protocol page whose heading carries `anchor`, from the start of that
    /// heading line up to the next line consisting only of `---`.
    fn protocol_page_section<'a>(page: &'a str, anchor: &str) -> &'a str {
        let marker = format!("{{ #{anchor} }}");
        let at = page
            .find(&marker)
            .unwrap_or_else(|| panic!("{PROTOCOL_PAGE_PATH} has no heading with anchor {marker}"));
        let start = page[..at].rfind('\n').map_or(0, |i| i + 1);
        let len = page[start..].find("\n---\n").unwrap_or_else(|| {
            panic!("the {marker} section of {PROTOCOL_PAGE_PATH} has no closing `---` line")
        });
        &page[start..start + len]
    }

    /// Every number written immediately before the word `seconds`, in order.
    fn numbers_before_seconds(text: &str) -> Vec<u64> {
        let words: Vec<&str> = text.split_whitespace().collect();
        words
            .windows(2)
            .filter(|pair| {
                pair[1]
                    .trim_end_matches(|c: char| !c.is_alphanumeric())
                    .eq("seconds")
            })
            .filter_map(|pair| {
                pair[0]
                    .trim_start_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok()
            })
            .collect()
    }

    /// The Heartbeat section states `SSE_HEARTBEAT_INTERVAL` in its Interval row, and every
    /// duration in seconds anywhere in the section is that interval.
    #[test]
    fn protocol_page_heartbeat_interval_is_the_runtime_interval() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "heartbeat");
        let interval = SSE_HEARTBEAT_INTERVAL.as_secs();
        let row = format!("| Interval | {interval} seconds");
        assert!(
            section.contains(&row),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} has no `{row}` row; \
             SSE_HEARTBEAT_INTERVAL is {interval} seconds"
        );
        let stated = numbers_before_seconds(section);
        let wrong: Vec<u64> = stated.iter().copied().filter(|&n| n != interval).collect();
        assert!(
            wrong.is_empty(),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} states {wrong:?} \
             seconds; SSE_HEARTBEAT_INTERVAL is {interval} seconds"
        );
    }

    /// The Frames-a-connection-misses section states `SSE_BROADCAST_CAPACITY` as the size of each
    /// connection's live queue.
    #[test]
    fn protocol_page_broadcast_queue_is_the_runtime_capacity() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "lagged");
        let capacity = crate::runtime::SSE_BROADCAST_CAPACITY;
        for phrase in [
            format!("queue of {capacity}"),
            format!("more than {capacity}"),
        ] {
            assert!(
                section.contains(&phrase),
                "the Frames a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} \
                 does not say `{phrase}`; SSE_BROADCAST_CAPACITY is {capacity}"
            );
        }
    }

    /// The page documents the frame `stream_watch_writes_lagged_frame` and
    /// `message_stream_writes_lagged_frame` hold the handlers to: its key, its place in the
    /// endpoint and event-id tables, and its absence from replay.
    #[test]
    fn protocol_page_documents_the_lagged_frame() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "event-lagged");
        for required in ["`missed`", "{\"missed\":"] {
            assert!(
                section.contains(required),
                "the `lagged` section ({{ #event-lagged }}) of {PROTOCOL_PAGE_PATH} does not \
                 contain `{required}`"
            );
        }
        for row in [
            "| [`lagged`](#event-lagged) | yes | yes |",
            "| `lagged` | no | — |",
        ] {
            assert!(
                page.contains(row),
                "{PROTOCOL_PAGE_PATH} has no `{row}` row"
            );
        }
        assert!(
            protocol_page_section(&page, "replay").contains("`lagged`"),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not list `lagged`"
        );
        assert!(
            !protocol_page_section(&page, "lagged").contains("Nothing is written in their place"),
            "the Frames a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} says \
             nothing is written for lost frames"
        );
    }

    /// The Event ids section states the id a session's first frame takes, and that a replay from
    /// `0` of an unevicted buffer starts there.
    #[test]
    fn protocol_page_first_event_id_is_the_runtimes() {
        let page = protocol_page();
        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        let buffer = std::sync::Arc::new(Mutex::new(SseEventBuffer::new(1)));
        let first = crate::streaming::emit_frame(&tx, &buffer, StreamFrame::Status, "{}");

        let phrase = format!("| First id | `{first}` |");
        assert!(
            protocol_page_section(&page, "event-ids").contains(&phrase),
            "the Event ids section ({{ #event-ids }}) of {PROTOCOL_PAGE_PATH} does not say \
             `{phrase}`; a session's first frame takes id {first}"
        );
        let phrase = format!("starting at id `{first}`");
        assert!(
            protocol_page_section(&page, "replay").contains(&phrase),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not say `{phrase}`"
        );
    }

    /// The Replay section states `SSE_REPLAY_CAPACITY` as the number of frames a session keeps.
    #[test]
    fn protocol_page_replay_capacity_is_the_runtime_capacity() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "replay");
        let capacity = crate::runtime::SSE_REPLAY_CAPACITY;
        let phrase = format!("most recent {capacity} frames");
        assert!(
            section.contains(&phrase),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not say `{phrase}`; \
             SSE_REPLAY_CAPACITY is {capacity}"
        );
    }

    /// The Heartbeat section's liveness claim is held against
    /// `stream_watch_heartbeat_continues_while_session_thread_is_blocked`: the heartbeat keeps its
    /// cadence while the thread running the agent loop is held.
    #[test]
    fn protocol_page_says_the_heartbeat_keeps_its_cadence_during_a_turn() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "heartbeat");
        let required = "keeps its cadence while a turn is running";
        assert!(
            section.contains(required),
            "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} does not say \
             `{required}`"
        );
        for forbidden in ["same thread", "holds the heartbeat"] {
            assert!(
                !section.contains(forbidden),
                "the Heartbeat section ({{ #heartbeat }}) of {PROTOCOL_PAGE_PATH} says \
                 `{forbidden}`, but the heartbeat is served off the agent loop's thread"
            );
        }
    }

    // ── The completion route ─────────────────────────────────────────────────

    /// One child's completion posted to the test door, as the child's own reporter posts it.
    async fn post_completion(addr: &str, delegation_id: &str) -> Result<(), String> {
        let handle = crate::delegation::SpawnerHandle {
            session_id: "ses_door".to_string(),
            context_id: "ctx_parent".to_string(),
            delegation_id: delegation_id.to_string(),
            report_to: Some(crate::delegation::CompletionAddress {
                url: format!("http://{addr}"),
            }),
        };
        let outcome = crate::delegation::DelegationOutcome {
            delegation_id: delegation_id.to_string(),
            capsule_name: "worker".to_string(),
            capsule_version: "0.1.0".to_string(),
            session_id: "ses_child".to_string(),
            status: crate::delegation::DelegationStatus::Ok,
            result_path: None,
            workdir: "/tmp/child".to_string(),
            duration_ms: 1,
            detail: None,
            reported_by: crate::delegation::Reporter::Child,
            delivered: false,
            delivery_error: None,
        };
        tokio::task::spawn_blocking(move || {
            let address = handle.report_to.clone().unwrap();
            crate::delegation::deliver_completion(&handle, &address, &outcome)
        })
        .await
        .unwrap()
    }

    fn outstanding(task_id: &str) -> crate::cancel::LiveDelegation {
        crate::cancel::LiveDelegation {
            task_id: task_id.to_string(),
            workdir: std::path::PathBuf::from("/tmp/child"),
            capsule: "worker".to_string(),
            version: "0.1.0".to_string(),
            child_session_id: "ses_child".to_string(),
            child_workdir: ".murmur/children/worker-1".to_string(),
            started: std::time::Instant::now(),
            child: None,
            arrived: false,
        }
    }

    /// Under every acceptance mode, and with a `single` task busy, a completion for the running
    /// task's delegation is received into the delegation set and starts no task; a repeat is
    /// answered as the success the first post was; and one nobody is waiting for is refused,
    /// naming the delegation, still starting no task.
    #[tokio::test]
    async fn a_completion_is_handed_to_the_delegation_set_and_never_becomes_a_task() {
        for acceptance in ACCEPTANCES {
            let registry = Arc::new(Mutex::new(TaskRegistry::new(1, acceptance.clone())));
            // A task already running: a `single` door refuses a second one.
            {
                let mut reg = registry.lock().unwrap();
                reg.enqueue("tsk_running", "ctx_parent");
                reg.start_task(
                    "tsk_running".to_string(),
                    "ctx_parent".to_string(),
                    crate::lanes::TaskLane::User,
                );
            }
            let live = Arc::new(LiveDelegations::new());
            let _scope = live.scope_task("tsk_running");
            assert!(live.register("dlg_waited".to_string(), outstanding("tsk_running")));
            let (addr, _shutdown, mut task_rx) = serve_test_door_with(
                acceptance.clone(),
                Arc::clone(&registry),
                Arc::clone(&live),
                false,
            )
            .await;

            post_completion(&addr, "dlg_waited")
                .await
                .unwrap_or_else(|error| panic!("{acceptance:?}: {error}"));
            assert_eq!(live.counts(), (0, 1), "{acceptance:?}");
            post_completion(&addr, "dlg_waited")
                .await
                .unwrap_or_else(|error| panic!("{acceptance:?}: a repeat: {error}"));

            let refused = post_completion(&addr, "dlg_nobody")
                .await
                .expect_err("nobody is waiting for it");
            assert!(
                refused.contains("dlg_nobody") && refused.contains("no task"),
                "{acceptance:?}: {refused}"
            );

            assert!(
                task_rx.try_recv().is_err(),
                "{acceptance:?}: a completion started a task"
            );
            assert_eq!(live.take_arrived().len(), 1, "{acceptance:?}");
        }
    }

    /// The session check still comes first: a completion addressed to another session is refused
    /// whatever the delegation set holds.
    #[tokio::test]
    async fn a_completion_for_another_session_is_refused_before_the_delegation_set() {
        let live = Arc::new(LiveDelegations::new());
        let _scope = live.scope_task("tsk_running");
        live.register("dlg_waited".to_string(), outstanding("tsk_running"));
        let (addr, _shutdown, mut task_rx) = serve_test_door_with(
            TaskAcceptance::Queue,
            Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue))),
            Arc::clone(&live),
            false,
        )
        .await;
        let handle = crate::delegation::SpawnerHandle {
            session_id: "ses_elsewhere".to_string(),
            context_id: "ctx_parent".to_string(),
            delegation_id: "dlg_waited".to_string(),
            report_to: Some(crate::delegation::CompletionAddress {
                url: format!("http://{addr}"),
            }),
        };
        let refused = tokio::task::spawn_blocking(move || {
            let address = handle.report_to.clone().unwrap();
            let outcome = crate::delegation::DelegationOutcome {
                delegation_id: "dlg_waited".to_string(),
                capsule_name: "worker".to_string(),
                capsule_version: "0.1.0".to_string(),
                session_id: "ses_child".to_string(),
                status: crate::delegation::DelegationStatus::Ok,
                result_path: None,
                workdir: "/tmp/child".to_string(),
                duration_ms: 1,
                detail: None,
                reported_by: crate::delegation::Reporter::Child,
                delivered: false,
                delivery_error: None,
            };
            crate::delegation::deliver_completion(&handle, &address, &outcome)
        })
        .await
        .unwrap()
        .expect_err("addressed to another session");
        assert!(refused.contains("ses_elsewhere"), "{refused}");
        assert_eq!(live.counts(), (1, 0));
        assert!(task_rx.try_recv().is_err());
    }

    /// One completion posted to the door at `addr` with `headers` beside the version and the
    /// completion origin: the door's whole JSON-RPC answer.
    async fn completion_answer(addr: &str, headers: &[(&'static str, &'static str)]) -> Value {
        let url = format!("http://{addr}/");
        let mut sent = vec![
            (
                crate::a2a::A2A_VERSION_HEADER,
                crate::a2a::A2A_PROTOCOL_VERSION,
            ),
            (PEER_ORIGIN_HEADER, "completion"),
        ];
        sent.extend_from_slice(headers);
        let body = serde_json::json!({"jsonrpc": "2.0", "id": "c1", "method": "SendMessage",
            "params": {"message": {"messageId": "m", "role": "ROLE_USER", "parts": [{"text": "done"}]}}})
        .to_string();
        tokio::task::spawn_blocking(move || {
            crate::http_client::http_json("POST", &url, Some(&body), &sent).unwrap()
        })
        .await
        .unwrap()
    }

    /// A completion the door refuses is answered with one of the two murmur codes, outside the
    /// A2A range, carrying an `ErrorInfo` in the murmur domain: `-31001` naming the session it
    /// was addressed to and never this one, and `-31002` naming the delegation when it named one.
    /// No refusal starts a task, and an outstanding delegation is still delivered.
    #[tokio::test]
    async fn a_completion_is_refused_with_the_murmur_completion_codes() {
        let live = Arc::new(LiveDelegations::new());
        let _scope = live.scope_task("tsk_running");
        live.register("dlg_waited".to_string(), outstanding("tsk_running"));
        let (addr, _shutdown, mut task_rx) = serve_test_door_with(
            TaskAcceptance::Queue,
            Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue))),
            Arc::clone(&live),
            false,
        )
        .await;
        let info = |answer: &Value| answer["error"]["data"][0].clone();

        let misaddressed = completion_answer(
            &addr,
            &[
                (COMPLETION_SESSION_HEADER, "ses_elsewhere"),
                (DELEGATION_ID_HEADER, "dlg_waited"),
            ],
        )
        .await;
        assert_eq!(misaddressed["id"], "c1");
        assert_eq!(misaddressed["error"]["code"], -31001, "{misaddressed}");
        assert_eq!(info(&misaddressed)["reason"], "COMPLETION_MISADDRESSED");
        assert_eq!(info(&misaddressed)["domain"], "murmur.nexus");
        assert_eq!(
            info(&misaddressed)["metadata"],
            serde_json::json!({"addressedSession": "ses_elsewhere"})
        );
        assert!(
            !misaddressed.to_string().contains("ses_door"),
            "{misaddressed}"
        );

        let unaddressed = completion_answer(&addr, &[(DELEGATION_ID_HEADER, "dlg_waited")]).await;
        assert_eq!(unaddressed["error"]["code"], -31001, "{unaddressed}");
        assert_eq!(
            info(&unaddressed)["metadata"],
            serde_json::json!({"addressedSession": ""})
        );

        let unnamed = completion_answer(&addr, &[(COMPLETION_SESSION_HEADER, "ses_door")]).await;
        assert_eq!(unnamed["error"]["code"], -31002, "{unnamed}");
        assert_eq!(info(&unnamed)["reason"], "COMPLETION_NOT_AWAITED");
        assert_eq!(info(&unnamed)["metadata"], serde_json::json!({}));

        let unknown = completion_answer(
            &addr,
            &[
                (COMPLETION_SESSION_HEADER, "ses_door"),
                (DELEGATION_ID_HEADER, "dlg_nobody"),
            ],
        )
        .await;
        assert_eq!(unknown["error"]["code"], -31002, "{unknown}");
        assert_eq!(
            info(&unknown)["metadata"],
            serde_json::json!({"delegationId": "dlg_nobody"})
        );
        assert_eq!(live.counts(), (1, 0), "no refusal touched the delegation");

        let delivered = completion_answer(
            &addr,
            &[
                (COMPLETION_SESSION_HEADER, "ses_door"),
                (DELEGATION_ID_HEADER, "dlg_waited"),
            ],
        )
        .await;
        assert_eq!(
            delivered["result"]["message"]["parts"][0]["data"],
            serde_json::json!({"delegation_id": "dlg_waited", "received": true})
        );
        assert_eq!(delivered["result"]["message"]["role"], "ROLE_AGENT");
        assert_eq!(live.counts(), (0, 1));
        assert!(task_rx.try_recv().is_err(), "a completion started a task");
    }
}
