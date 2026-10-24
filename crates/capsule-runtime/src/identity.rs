use std::sync::{Arc, Mutex};

use murmur_artifact::{ConversationMode, NetworkAuthentication, TaskAcceptance};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::a2a::{
    read_send_params, A2aError, A2aTask, CancelOutcome, IncomingMessage, IncomingTask,
    JsonRpcRequest, JsonRpcResponse, ListCursor, MurmurError, Part, Role, StateFilter, TaskFilter,
    TaskRegistry, TaskState, TaskStatus, DATA_MEDIA_TYPE, INTERNAL_ERROR, INVALID_PARAMS,
    METHOD_NOT_FOUND, TEXT_MEDIA_TYPE,
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
pub(crate) use crate::streaming::STREAM_EXTENSION_URI;
use crate::streaming::{
    ending_results, message_carrier, sse_event, status_carrier, task_result, MurmurFrame,
    ReplayResult, SseBroadcast, SseEventBuffer, StreamEnd, StreamEvent, StreamFrame, StreamTask,
    SSE_HEARTBEAT_COMMENT, SSE_HEARTBEAT_INTERVAL,
};
use crate::types::{CapabilityPolicy, InstalledArtifactSummary};

#[derive(Debug, Clone)]
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
    WatchSession,
    GetTask,
    ListTasks,
    CancelTask,
    SubscribeToTask,
    StopSession,
    GetExtendedAgentCard,
}

impl DoorMethod {
    /// Every method, in the order the card lists them: A2A v1.0's order, with murmur's own
    /// methods beside the A2A methods they sit with.
    pub(crate) const ALL: [DoorMethod; 9] = [
        DoorMethod::SendMessage,
        DoorMethod::SendStreamingMessage,
        DoorMethod::WatchSession,
        DoorMethod::GetTask,
        DoorMethod::ListTasks,
        DoorMethod::CancelTask,
        DoorMethod::SubscribeToTask,
        DoorMethod::StopSession,
        DoorMethod::GetExtendedAgentCard,
    ];

    /// The method's name on the wire: the A2A v1.0 name of an A2A method, which is an rpc of
    /// `A2AService`, and `murmur.` followed by a PascalCase verb and object for murmur's own. No
    /// A2A rpc name holds a `.`, so a murmur name cannot collide with a method A2A adds.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            DoorMethod::SendMessage => "SendMessage",
            DoorMethod::SendStreamingMessage => "SendStreamingMessage",
            DoorMethod::WatchSession => "murmur.WatchSession",
            DoorMethod::GetTask => "GetTask",
            DoorMethod::ListTasks => "ListTasks",
            DoorMethod::CancelTask => "CancelTask",
            DoorMethod::SubscribeToTask => "SubscribeToTask",
            DoorMethod::StopSession => "murmur.StopSession",
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

    /// The method this door answers for a request's `method` string, or `None` when it does not
    /// serve one: `-32601`, or `-32004` for an A2A method the card says the door lacks.
    ///
    /// The only place a request's method is interpreted and the only place
    /// `lifecycle.task_acceptance` gates one: under `TaskAcceptance::None` neither task-starting
    /// method is served, and neither is `SubscribeToTask`, which streams exactly when
    /// `SendStreamingMessage` does. `GetExtendedAgentCard` is served only by an `authenticated`
    /// door, the only kind with an extended card. The match is exact — no case folding, no
    /// trimming, no other spelling — so a name the card lists is the name to send.
    pub(crate) fn resolve(
        method: &str,
        acceptance: &TaskAcceptance,
        authenticated: bool,
    ) -> Option<DoorMethod> {
        let resolved = Self::ALL.into_iter().find(|m| m.wire_name() == method)?;
        match (resolved, acceptance) {
            (
                DoorMethod::SendMessage
                | DoorMethod::SendStreamingMessage
                | DoorMethod::SubscribeToTask,
                TaskAcceptance::None,
            ) => None,
            (DoorMethod::GetExtendedAgentCard, _) if !authenticated => None,
            _ => Some(resolved),
        }
    }
}

/// The four A2A v1.0 push notification config methods. None is a [`DoorMethod`]: the door answers
/// each with `PushNotificationNotSupportedError` at the method step, before any scope or params
/// check, as A2A prescribes for an agent whose card says `capabilities.pushNotifications: false`.
pub(crate) const PUSH_CONFIG_METHODS: [&str; 4] = [
    "CreateTaskPushNotificationConfig",
    "GetTaskPushNotificationConfig",
    "ListTaskPushNotificationConfigs",
    "DeleteTaskPushNotificationConfig",
];

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

/// What the capsule's inference transport can do, beyond what the served method list already says:
/// which frames the stream extension lists. Every transport writes the A2A v1.0 task events and
/// can be stopped, so neither `capabilities.streaming` nor cancellation is here.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportCapabilities {
    /// The transport family, which decides the frames [`served_frames`] lists.
    pub kind: TransportKind,
}

/// Whether a connection made with `method` can receive `frame` from a session on `transport`.
///
/// Each frame's arm states which streaming methods carry it and which transports write it. Only
/// the two task streams, `SendStreamingMessage` and `SubscribeToTask`, on a connection that
/// activated the stream extension, and `murmur.WatchSession` write frames; `murmur.WatchSession` alone writes the
/// observer's `connection-ack` and `capsule-closed`.
pub(crate) fn writes_frame(
    method: DoorMethod,
    frame: StreamFrame,
    transport: TransportKind,
) -> bool {
    let (on_task_stream, on_watch_session, transports): (bool, bool, &[TransportKind]) = match frame
    {
        StreamFrame::Text
        | StreamFrame::Thinking
        | StreamFrame::ToolCallStarted
        | StreamFrame::ToolCallProgress
        | StreamFrame::Artifact
        | StreamFrame::Gap
        | StreamFrame::Lagged => (true, true, &TransportKind::ALL),
        StreamFrame::ConnectionAck | StreamFrame::CapsuleClosed => {
            (false, true, &TransportKind::ALL)
        }
    };
    let carried = match method {
        DoorMethod::SendStreamingMessage | DoorMethod::SubscribeToTask => on_task_stream,
        DoorMethod::WatchSession => on_watch_session,
        DoorMethod::SendMessage
        | DoorMethod::GetTask
        | DoorMethod::ListTasks
        | DoorMethod::CancelTask
        | DoorMethod::StopSession
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

/// What the door extension says about itself.
const DOOR_EXTENSION_DESCRIPTION: &str = "Every JSON-RPC method this door answers, A2A methods and murmur's own alike, the murmur ones named murmur.<Method>; whether it accepts tasks from peer capsules; and the forgetSession field a message's metadata may carry under this URI.";

/// What the stream extension says about itself.
const STREAM_EXTENSION_DESCRIPTION: &str = "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata keyed by this URI.";

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
/// `capabilities.streaming` is `true` exactly when `SendStreamingMessage` is served: every transport
/// writes the A2A v1.0 task events, and streamed text is the stream extension's.
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
    let streaming = methods.contains(&DoorMethod::SendStreamingMessage.wire_name());
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
    // Whether this capsule has a harness session to forget, which is the one thing a message's
    // `forgetSession` field needs to know about the transport behind the door.
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
    // A `murmur.WatchSession` or `SendStreamingMessage` connection otherwise outlives the session:
    // its handler holds a broadcast sender, so it never sees the channel close.
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
    let mut authorization: Option<String> = None;
    // Every `authorization` header, in order: a door token is refused when more than one is sent.
    let mut authorizations: Vec<String> = Vec::new();
    // Every `A2A-Version` header, in order: a JSON-RPC request is served only when there is one.
    let mut a2a_versions: Vec<String> = Vec::new();
    let a2a_version_prefix = format!("{}:", crate::a2a::A2A_VERSION_HEADER.to_ascii_lowercase());
    // Every `A2A-Extensions` header, in order: each activates the extensions it lists.
    let mut a2a_extensions: Vec<String> = Vec::new();
    let a2a_extensions_prefix = format!(
        "{}:",
        crate::a2a::A2A_EXTENSIONS_HEADER.to_ascii_lowercase()
    );

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
        } else if lower.starts_with(&a2a_extensions_prefix) {
            // Taken from the line as sent: an extension URI is compared exactly.
            let value = line.trim_end()[a2a_extensions_prefix.len()..]
                .trim()
                .to_string();
            a2a_extensions.push(value);
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
    // Which tasks the caller reaches, by the one rule every task-addressing method reads.
    let caller = TaskCaller::of(grant.as_ref());

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
        // and a door that starts no task does not stream, which its card's
        // `capabilities.streaming: false` says: A2A answers asking for either as an unsupported
        // operation. The card's `capabilities.pushNotifications: false` says the same of push,
        // whose four config methods A2A answers `PushNotificationNotSupportedError`.
        let Some(resolved) = DoorMethod::resolve(&req.method, &task_acceptance, gate.is_some())
        else {
            let streams = [
                DoorMethod::SendStreamingMessage.wire_name(),
                DoorMethod::SubscribeToTask.wire_name(),
            ];
            let response = if PUSH_CONFIG_METHODS.contains(&req.method.as_str()) {
                crate::a2a::push_notification_not_supported(req.id)
            } else if req.method == DoorMethod::GetExtendedAgentCard.wire_name() {
                JsonRpcResponse::a2a_error(
                    req.id,
                    A2aError::UnsupportedOperation,
                    "this agent serves no extended agent card",
                    &[],
                )
            } else if streams.contains(&req.method.as_str()) {
                JsonRpcResponse::a2a_error(
                    req.id,
                    A2aError::UnsupportedOperation,
                    "this agent does not stream: it starts no task",
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

        // The card's one interface declares no tenant, so a request naming one names something
        // this agent does not serve. `""` is the proto's unset value.
        if let Some(response) = tenant_refusal(&req) {
            let _ = writer_half
                .write_all(response.into_http_response().as_bytes())
                .await;
            return;
        }

        // `SendMessage` and the streaming methods own the connection; every other method answers
        // one JSON body at once.
        let response = match resolved {
            DoorMethod::SendMessage => {
                handle_message_send(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    MessageSender {
                        traceparent,
                        provenance,
                        caller_member,
                        caller,
                    },
                    forgettable_session,
                    sse_tx,
                    closing,
                )
                .await;
                return;
            }
            DoorMethod::SendStreamingMessage => {
                handle_message_stream(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    MessageSender {
                        traceparent,
                        provenance,
                        caller_member,
                        caller,
                    },
                    forgettable_session,
                    StreamRequest {
                        last_event_id,
                        activated: crate::a2a::activates_extension(
                            &a2a_extensions,
                            STREAM_EXTENSION_URI,
                        ),
                    },
                    sse_tx,
                    sse_buffer,
                    closing,
                )
                .await;
                return;
            }
            DoorMethod::SubscribeToTask => {
                handle_task_subscribe(
                    writer_half,
                    req,
                    &task_registry,
                    &caller,
                    StreamRequest {
                        last_event_id,
                        activated: crate::a2a::activates_extension(
                            &a2a_extensions,
                            STREAM_EXTENSION_URI,
                        ),
                    },
                    sse_tx,
                    sse_buffer,
                    closing,
                )
                .await;
                return;
            }
            DoorMethod::WatchSession => {
                handle_watch_session(
                    writer_half,
                    &req.id,
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
                caller,
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

/// What a `SendStreamingMessage` or `SubscribeToTask` request asked of its stream besides its
/// params.
pub(crate) struct StreamRequest {
    /// The `Last-Event-ID` the client resumes after.
    pub last_event_id: Option<u64>,
    /// Whether its `A2A-Extensions` headers activated [`STREAM_EXTENSION_URI`].
    pub activated: bool,
}

/// One streaming connection: its socket, the request every event answers, and how far into the
/// session's sequence it has got.
struct StreamConnection {
    writer: tokio::net::tcp::OwnedWriteHalf,
    /// The request's JSON-RPC id, as JSON, which every event's response carries verbatim.
    request_id: String,
    /// Whether the connection receives the murmur frames.
    activated: bool,
    /// The highest id of the session's sequence this connection has written or passed over. A
    /// live event at or below it was already in the replay: an event emitted between subscribing
    /// and reading the buffer arrives both ways.
    last_seen: Option<u64>,
}

impl StreamConnection {
    /// Writes the SSE response head, which lists `extensions` in `A2A-Extensions`: the extensions
    /// the request used, the stream extension last when the connection is `activated`. `Err`
    /// means the client is gone.
    async fn open(
        mut writer: tokio::net::tcp::OwnedWriteHalf,
        request_id: &Value,
        extensions: &[&str],
        activated: bool,
    ) -> Result<Self, ()> {
        use tokio::io::AsyncWriteExt;

        let echo = crate::a2a::extensions_echo(extensions);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: keep-alive\r\n{echo}\r\n"
        );
        writer.write_all(head.as_bytes()).await.map_err(|_| ())?;
        Ok(Self {
            writer,
            request_id: serde_json::to_string(request_id).unwrap_or_else(|_| "null".into()),
            activated,
            last_seen: None,
        })
    }

    /// Writes one event whose `result` is `result`, numbered `id` when it is in the session's
    /// sequence.
    async fn write(&mut self, id: Option<u64>, result: &str) -> Result<(), ()> {
        use tokio::io::AsyncWriteExt;

        let event = sse_event(id, &self.request_id, result);
        self.writer
            .write_all(event.as_bytes())
            .await
            .map_err(|_| ())
    }

    async fn heartbeat(&mut self) -> Result<(), ()> {
        use tokio::io::AsyncWriteExt;

        self.writer
            .write_all(SSE_HEARTBEAT_COMMENT)
            .await
            .map_err(|_| ())
    }

    /// Whether the event numbered `id` is new to this connection, and marks it seen when it is.
    fn admit(&mut self, id: u64) -> bool {
        if self.last_seen.is_some_and(|seen| id <= seen) {
            return false;
        }
        self.last_seen = Some(id);
        true
    }
}

/// The buffered events after `last_id`, and the first available id when the buffer cannot supply
/// all of them.
fn replay(
    sse_buffer: &Mutex<SseEventBuffer>,
    last_id: u64,
) -> (Option<u64>, Vec<Arc<StreamEvent>>) {
    match sse_buffer.lock().unwrap().replay_from(last_id) {
        ReplayResult::Complete(events) => (None, events),
        ReplayResult::WithGap {
            first_available_id,
            events,
        } => (Some(first_available_id), events),
    }
}

/// `SendStreamingMessage`: route the message as [`route_message`] does, then stream its task.
///
/// Every refusal [`read_message`] or the router answers with is one JSON-RPC body, written
/// before any SSE head. A new task the registry has no room for is answered as a stream of exactly
/// one unnumbered event, the `TASK_STATE_REJECTED` task `SendMessage` answers the same refusal
/// with.
///
/// The response head lists the door extension in `A2A-Extensions` when the message's metadata used
/// it, then the stream extension when the request activated it.
///
/// Otherwise the stream opens with the task as the registry held it when the message was taken,
/// unnumbered, and goes on as [`serve_task_stream`] serves it, ending after the task's terminal
/// status or a live `TASK_STATE_INPUT_REQUIRED`, which the client answers on a new request.
#[allow(clippy::too_many_arguments)]
async fn handle_message_stream(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
    forgettable_session: bool,
    request: StreamRequest,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before the message is routed, so no event the task writes once it is enqueued or
    // handed its input is missed. The sender is not kept: the channel closes once the session's
    // own senders are gone.
    let rx = sse_tx.subscribe();
    drop(sse_tx);

    let mut extensions = Vec::new();
    let routed = match read_message(&req.id, &req.params, forgettable_session) {
        Ok(message) => {
            if message.door_extension {
                extensions.push(DOOR_EXTENSION_URI);
            }
            route_message(&req.id, message, task_registry, task_tx, sender)
        }
        Err(response) => MessageRoute::Refused(response),
    };
    if request.activated {
        extensions.push(STREAM_EXTENSION_URI);
    }
    let (opening, rejected) = match routed {
        MessageRoute::Refused(response) => {
            let _ = writer
                .write_all(response.into_http_response().as_bytes())
                .await;
            return;
        }
        MessageRoute::Started(task) | MessageRoute::Continued(task) => (task, false),
        MessageRoute::Rejected(task) => (task, true),
    };

    let Ok(mut connection) =
        StreamConnection::open(writer, &req.id, &extensions, request.activated).await
    else {
        return;
    };
    // Unnumbered: the opening task is this connection's alone, never buffered, and an id would
    // move a client's resume cursor.
    if connection
        .write(None, &task_result(&opening))
        .await
        .is_err()
        || rejected
    {
        return;
    }
    serve_task_stream(
        connection,
        &opening,
        request.last_event_id,
        rx,
        task_registry,
        &sse_buffer,
        closing,
        true,
    )
    .await;
}

/// `SubscribeToTask`: stream the rest of one task the caller may see and that has not ended.
///
/// Every refusal is one JSON-RPC body, written before any SSE head: `-32602` for a missing or
/// empty `id`, `-32001` for a task the session never held or [`visible_to`] hides from `caller`,
/// and `-32004` naming the task and its state for a task that has ended.
///
/// Otherwise the stream opens with the task exactly as `GetTask` reports it at that moment,
/// unnumbered, and goes on as [`serve_task_stream`] serves it. It ends only after the task's
/// terminal status: a subscriber watches a task, and the reply to a task waiting for input
/// arrives on a request of its own, after which the subscriber sees the task resume.
#[allow(clippy::too_many_arguments)]
async fn handle_task_subscribe(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller: &TaskCaller,
    request: StreamRequest,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before the task is read, so no event emitted after the read is lost. One emitted
    // between the two is both in the opening task and delivered live, which the stream tolerates.
    let rx = sse_tx.subscribe();
    drop(sse_tx);

    let opening = match subscribable_task(&req, task_registry, caller) {
        Ok(task) => task,
        Err(response) => {
            let _ = writer
                .write_all(response.into_http_response().as_bytes())
                .await;
            return;
        }
    };
    let extensions: &[&str] = if request.activated {
        &[STREAM_EXTENSION_URI]
    } else {
        &[]
    };
    let Ok(mut connection) =
        StreamConnection::open(writer, &req.id, extensions, request.activated).await
    else {
        return;
    };
    if connection
        .write(None, &task_result(&opening))
        .await
        .is_err()
    {
        return;
    }
    serve_task_stream(
        connection,
        &opening,
        request.last_event_id,
        rx,
        task_registry,
        &sse_buffer,
        closing,
        false,
    )
    .await;
}

/// The task a `SubscribeToTask` request names, as `GetTask` reports it, or the error refusing the
/// request.
fn subscribable_task(
    req: &JsonRpcRequest,
    task_registry: &Mutex<TaskRegistry>,
    caller: &TaskCaller,
) -> Result<A2aTask, JsonRpcResponse> {
    let Some(task_id) = req
        .params
        .get("id")
        .and_then(Value::as_str)
        .filter(|task_id| !task_id.is_empty())
    else {
        return Err(JsonRpcResponse::err(
            req.id.clone(),
            INVALID_PARAMS,
            "SubscribeToTask requires an id",
        ));
    };
    let reg = task_registry.lock().unwrap();
    let Some(task) = reg
        .get_task(task_id)
        .filter(|_| visible_to(&reg, task_id, caller))
    else {
        return Err(task_not_found_response(req.id.clone(), Some(task_id)));
    };
    if task.status.state.is_terminal() {
        let state = task.status.state.wire_name();
        return Err(JsonRpcResponse::a2a_error(
            req.id.clone(),
            A2aError::UnsupportedOperation,
            &format!("task {task_id} has ended in {state}; there is nothing left to subscribe to"),
            &[
                ("taskId", task_id.to_string()),
                ("state", state.to_string()),
            ],
        ));
    }
    Ok(task)
}

/// The part of a task stream after its opening `task`, which both `SendStreamingMessage` and
/// `SubscribeToTask` write: for a `last_event_id`, the `gap` when one applies and the buffered
/// events of this task after the cursor, then its live events. Only this task's events are
/// written, and murmur frames, `gap` and `lagged` only to a connection that activated the stream
/// extension.
///
/// The stream ends after the task's terminal status, replayed or live, when the door closes, or
/// when the broadcast ends. With `ends_on_input_required` it also ends after a live
/// `TASK_STATE_INPUT_REQUIRED`; a replayed one never ends it, since the task has left that state by
/// the time a continuation replays it.
///
/// A connection that lagged may have lost the status that ends it, so from then on each heartbeat
/// reads the task from the registry. Once the task has ended, or waits for input when that ends the
/// stream, the connection writes that status as `GetTask` reports it, after the `response`
/// artifact when the task has one, both unnumbered, and ends.
#[allow(clippy::too_many_arguments)]
async fn serve_task_stream(
    mut connection: StreamConnection,
    opening: &A2aTask,
    last_event_id: Option<u64>,
    mut rx: tokio::sync::broadcast::Receiver<Arc<StreamEvent>>,
    task_registry: &Mutex<TaskRegistry>,
    sse_buffer: &Mutex<SseEventBuffer>,
    mut closing: watch::Receiver<bool>,
    ends_on_input_required: bool,
) {
    let task = StreamTask::new(&opening.id, &opening.context_id);
    // The state of the last status written for the task, which a carrier repeats.
    let mut state = opening.status.state.clone();
    let ends = |end: Option<StreamEnd>| match end {
        Some(StreamEnd::Terminal) => true,
        Some(StreamEnd::Interrupted) => ends_on_input_required,
        None => false,
    };

    if let Some(last_id) = last_event_id {
        let (gap, events) = replay(sse_buffer, last_id);
        if let (Some(first_available_id), true) = (gap, connection.activated) {
            let frame = MurmurFrame::Gap { first_available_id };
            if connection
                .write(None, &status_carrier(&task, state.clone(), &frame))
                .await
                .is_err()
            {
                return;
            }
        }
        for event in events {
            connection.admit(event.id);
            if event.task_id != task.task_id {
                continue;
            }
            let Some(result) = event.result(connection.activated) else {
                continue;
            };
            if connection.write(Some(event.id), result).await.is_err() {
                return;
            }
            if let Some(written) = &event.state {
                state = written.clone();
            }
            if event.ends == Some(StreamEnd::Terminal) {
                return;
            }
        }
    }

    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick
    let mut lagged = false;

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                if connection.heartbeat().await.is_err() {
                    return;
                }
                if !lagged {
                    continue;
                }
                let Some(results) =
                    lost_stream_end(task_registry, &task, ends_on_input_required)
                else {
                    continue;
                };
                for result in results {
                    if connection.write(None, &result).await.is_err() {
                        return;
                    }
                }
                return;
            }
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if !connection.admit(event.id) || event.task_id != task.task_id {
                            continue;
                        }
                        let Some(result) = event.result(connection.activated) else {
                            continue;
                        };
                        if connection.write(Some(event.id), result).await.is_err() {
                            return;
                        }
                        if let Some(written) = &event.state {
                            state = written.clone();
                        }
                        if ends(event.ends) {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        crate::runtime_err!("[capsule-runtime] SSE broadcast lagged by {n} events");
                        lagged = true;
                        // A v1.0 client has no word for a loss, so only an activated connection
                        // is told.
                        if connection.activated {
                            let frame = MurmurFrame::Lagged { missed: n };
                            if connection
                                .write(None, &status_carrier(&task, state.clone(), &frame))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
            () = door_closing(&mut closing) => return,
        }
    }
}

/// The events that end `task`'s stream, read from the registry for a connection that may have
/// lost them, or `None` while the task has not ended and, unless `ends_on_input_required`, while it
/// waits for input.
///
/// An ended task's are what [`crate::streaming::emit_task_ending`] writes, read from `GetTask`'s
/// view once its ending is recorded: a cancel records the state before the task stops, and the
/// ending after. A task waiting for input is its `input-required` status carrying the prompt.
fn lost_stream_end(
    task_registry: &Mutex<TaskRegistry>,
    task: &StreamTask,
    ends_on_input_required: bool,
) -> Option<Vec<String>> {
    let held = settled_task(task_registry, &task.task_id)?;
    if held.status.state.is_terminal() {
        return Some(ending_results(&held));
    }
    if !ends_on_input_required {
        return None;
    }
    let prompt = held
        .artifact(crate::a2a::PROMPT_ARTIFACT)
        .and_then(crate::a2a::A2aArtifact::first_text)
        .unwrap_or_default();
    Some(vec![crate::streaming::input_required_result(task, prompt)])
}

/// `murmur.WatchSession`: the passive observer of every task of this session.
///
/// Does not submit a task or call `can_accept()`. Calling it uses the door extension and the
/// stream extension, so the response head lists both, in that order; the stream extension is
/// always active, and the murmur frames of every task are written. The stream opens with an
/// unnumbered `connection-ack`, then replays the buffer after `Last-Event-ID` (from the start when
/// absent), preceded by a `gap` when the buffer cannot supply all of it, then forwards live events
/// until the capsule shuts down or the client disconnects. A task's terminal status does NOT
/// close this connection: the observer stays across tasks. The frames that belong to no task —
/// `connection-ack`, `gap`, `lagged` and `capsule-closed` — are `message` carriers.
///
/// Writes [`SSE_HEARTBEAT_COMMENT`] every [`SSE_HEARTBEAT_INTERVAL`], whether or not events
/// flowed in between, so an observer can tell an idle capsule from a dead socket. The handler
/// runs on the runtime's worker pool, off the thread running the agent loop, so synchronous work
/// inside a turn does not delay it.
async fn handle_watch_session(
    writer: tokio::net::tcp::OwnedWriteHalf,
    request_id: &Value,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    conversation_mode_str: String,
    mut closing: watch::Receiver<bool>,
) {
    // Subscribe before replaying so we don't miss events emitted between replay and loop start.
    // The sender is not kept, so the channel's close reaches this connection as `capsule-closed`.
    let mut rx = sse_tx.subscribe();
    drop(sse_tx);

    let Ok(mut connection) = StreamConnection::open(
        writer,
        request_id,
        &[DOOR_EXTENSION_URI, STREAM_EXTENSION_URI],
        true,
    )
    .await
    else {
        return;
    };

    // Says the capsule's conversation mode, so an observer need not read the manifest.
    let ack = MurmurFrame::ConnectionAck {
        role: "observer",
        conversation_mode: conversation_mode_str,
    };
    if connection
        .write(None, &message_carrier(&ack))
        .await
        .is_err()
    {
        return;
    }

    let (gap, events) = replay(&sse_buffer, last_event_id.unwrap_or(0));
    if let Some(first_available_id) = gap {
        let frame = MurmurFrame::Gap { first_available_id };
        if connection
            .write(None, &message_carrier(&frame))
            .await
            .is_err()
        {
            return;
        }
    }
    for event in events {
        connection.admit(event.id);
        if connection
            .write(Some(event.id), &event.extended)
            .await
            .is_err()
        {
            return;
        }
    }

    let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_INTERVAL);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                // A failed write means the observer is gone; there is nothing left to serve.
                if connection.heartbeat().await.is_err() {
                    return;
                }
            }
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if !connection.admit(event.id) {
                            continue;
                        }
                        if connection.write(Some(event.id), &event.extended).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let _ = connection
                            .write(None, &message_carrier(&MurmurFrame::CapsuleClosed))
                            .await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        crate::runtime_err!("[capsule-runtime] murmur.WatchSession: SSE broadcast lagged by {n} events");
                        let frame = MurmurFrame::Lagged { missed: n };
                        if connection.write(None, &message_carrier(&frame)).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // An exiting capsule ends an observer's stream without `capsule-closed`: that frame
            // means the event stream closed while the capsule still serves the connection.
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
fn handle_jsonrpc(
    method: DoorMethod,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller: TaskCaller,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let id = req.id;
    match method {
        DoorMethod::GetTask => handle_tasks_get(id, &req.params, task_registry, &caller),
        DoorMethod::ListTasks => handle_tasks_list(id, &req.params, task_registry, &caller),
        DoorMethod::CancelTask => handle_tasks_cancel(
            id,
            &req.params,
            task_registry,
            &caller,
            detached,
            live_delegations,
        ),
        DoorMethod::StopSession => {
            handle_stop_session(id, task_registry, detached, live_delegations, session_id)
        }
        DoorMethod::SendMessage
        | DoorMethod::SendStreamingMessage
        | DoorMethod::SubscribeToTask
        | DoorMethod::WatchSession
        | DoorMethod::GetExtendedAgentCard => {
            unreachable!(
                "handle_connection answers SendMessage, the streaming methods and the extended card"
            )
        }
    }
}

/// What the door read off a message's request besides its body: who sent it, and the headers the
/// task it starts carries.
pub(crate) struct MessageSender {
    pub traceparent: Option<String>,
    pub provenance: TaskProvenance,
    /// The formation member that sent it, when the door let it in on a formation token. Recorded
    /// on the task it starts for the trace, and decides nothing.
    pub caller_member: Option<String>,
    /// Which tasks the sender reaches, and so whom a task it starts belongs to.
    pub caller: TaskCaller,
}

/// Who is calling, as far as which tasks it reaches: the one rule [`visible_to`] reads for every
/// method that names or lists a task.
///
/// | Caller | Reaches |
/// |---|---|
/// | any caller of a public door | every task of the session |
/// | the operator token | every task of the session |
/// | a declared credential's token | the tasks a request carrying that credential started |
/// | a formation token, credential `member:<from>` | the tasks that member started |
///
/// The session's own methods, `murmur.WatchSession` and `murmur.StopSession`, are not task
/// methods: whoever holds their scope reaches every task through them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TaskCaller {
    /// A caller of a public door, or the holder of the operator token.
    Session,
    /// The holder of any other token, by the credential it was minted for.
    Credential(String),
}

impl TaskCaller {
    /// The caller a request is, from the grant the door let it in on, or `None` on a public door.
    pub(crate) fn of(grant: Option<&crate::door_auth::DoorGrant>) -> TaskCaller {
        match grant {
            Some(grant) if grant.credential() != murmur_artifact::OPERATOR_CREDENTIAL => {
                TaskCaller::Credential(grant.credential().to_string())
            }
            _ => TaskCaller::Session,
        }
    }
}

/// What [`route_message`] did with a message.
#[derive(Debug)]
enum MessageRoute {
    /// A new task, enqueued and handed to the task loop, in `TASK_STATE_SUBMITTED`.
    Started(A2aTask),
    /// A new task refused because the capsule has no room for it or its session is closing, in
    /// `TASK_STATE_REJECTED`, its status message saying which. Nothing was enqueued.
    Rejected(A2aTask),
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
/// | a task the registry does not hold, or one [`visible_to`] hides from `sender` | `-32001` `TaskNotFoundError` |
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
    let caller = &sender.caller;
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
            status: TaskStatus::at(
                TaskState::Rejected,
                Some(crate::a2a::StatusMessage::agent(
                    &task_id,
                    &context_id,
                    refusal,
                )),
                crate::a2a::status_now(),
            ),
            id: task_id,
            context_id,
            artifacts: None,
            metadata: None,
        };
        return MessageRoute::Rejected(task);
    }
    reg.enqueue(&task_id, &context_id);
    if let TaskCaller::Credential(credential) = &sender.caller {
        reg.record_owner(&task_id, credential);
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
        forget_session: message.forget_session,
        caller_member: sender.caller_member,
        reference_task_ids: message.reference_task_ids,
        part_kinds,
    };
    // Capacity was checked under this lock, so the channel has room; a failure is unexpected and
    // takes the task back out of the registry.
    if task_tx.try_send(incoming).is_err() {
        reg.withdraw(&task_id);
        return MessageRoute::Refused(JsonRpcResponse::err(
            id.clone(),
            INTERNAL_ERROR,
            "internal error: queue send failed",
        ));
    }

    // Still under the lock it was enqueued under, so this is the `submitted` view, carrying the
    // registry's timestamp for it.
    MessageRoute::Started(reg.get_task(&task_id).unwrap_or_else(|| A2aTask {
        id: task_id,
        context_id,
        status: TaskStatus::of(TaskState::Submitted),
        artifacts: None,
        metadata: None,
    }))
}

/// The message a `SendMessage` or `SendStreamingMessage` request's `params` carries, as
/// [`read_send_params`] reads it, or the error answering request `id`.
///
/// A message asking for a forget is refused `-32602` by a capsule that keeps no harness session,
/// which is every transport but `inference.transport: process`: on the request that carried it,
/// before it is routed, rather than dropped into a task that would ignore it. A caller that asked
/// for a conversation to be dropped and was answered `completed` would read that as the drop
/// having happened.
fn read_message(
    id: &Value,
    params: &Value,
    forgettable_session: bool,
) -> Result<IncomingMessage, JsonRpcResponse> {
    let message = read_send_params(params).map_err(|refusal| refusal.into_response(id.clone()))?;
    if message.forget_session && !forgettable_session {
        return Err(JsonRpcResponse::err(
            id.clone(),
            INVALID_PARAMS,
            "forgetSession asks this capsule to forget the harness session this context names, \
             and only a capsule on inference.transport: process has one",
        ));
    }
    Ok(message)
}

/// How often a held `SendMessage` reads its task from the registry, besides on each of the task's
/// events. It covers a settling the broadcast did not carry to this request, such as one the
/// receiver lagged past.
const HELD_SEND_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// What `SendMessage` answers a message with, once it is read and routed.
enum SendAnswer {
    /// Written at once: a refusal, a rejected task, or the task as routed when the message asked
    /// for `configuration.returnImmediately`.
    Now(String),
    /// Written once the task settles or the door closes, as [`hold_until_settled`] reads it.
    Held(Box<HeldMessage>),
}

/// A `SendMessage` request the door answers once its task settles.
struct HeldMessage {
    /// The request's JSON-RPC id.
    id: Value,
    /// The task as routed.
    task: A2aTask,
    /// The extensions the answer lists in `A2A-Extensions`.
    extensions: &'static [&'static str],
}

impl HeldMessage {
    /// The answer holding `task`, as `GetTask` reports it.
    fn answer(self, task: &A2aTask) -> String {
        JsonRpcResponse::ok(self.id, serde_json::json!({ "task": task }))
            .into_http_response_echoing(self.extensions)
    }
}

/// The message in a `SendMessage` request's `params`, read as [`read_message`] reads it and routed
/// as [`route_message`] routes it.
///
/// A refusal and a rejected task are answered at once. So is a started or continued task whose
/// message set `configuration.returnImmediately`, as the task stood when it was routed. Any other
/// task is held. The answer lists the door extension in `A2A-Extensions` when the message's
/// metadata used it.
fn accept_message_send(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
    forgettable_session: bool,
) -> SendAnswer {
    let message = match read_message(&id, params, forgettable_session) {
        Ok(message) => message,
        Err(response) => return SendAnswer::Now(response.into_http_response()),
    };
    let extensions: &'static [&'static str] = if message.door_extension {
        &[DOOR_EXTENSION_URI]
    } else {
        &[]
    };
    let return_immediately = message.return_immediately;
    let task = match route_message(&id, message, task_registry, task_tx, sender) {
        MessageRoute::Refused(response) => return SendAnswer::Now(response.into_http_response()),
        MessageRoute::Rejected(task) => task,
        MessageRoute::Started(task) | MessageRoute::Continued(task) if !return_immediately => {
            return SendAnswer::Held(Box::new(HeldMessage {
                id,
                task,
                extensions,
            }))
        }
        MessageRoute::Started(task) | MessageRoute::Continued(task) => task,
    };
    SendAnswer::Now(
        JsonRpcResponse::ok(id, serde_json::json!({ "task": task }))
            .into_http_response_echoing(extensions),
    )
}

/// `SendMessage`: the message accepted as [`accept_message_send`] accepts it, answered with one
/// `SendMessageResponse` holding the task, or with the error refusing it.
///
/// A held request writes nothing until its task settles, as [`settled_task`] decides, and then
/// answers the task as `GetTask` reports it. No deadline applies. The connection's read half is
/// never watched: a client that leaves is found out only when the answer fails to write, which is
/// ignored, and the task runs on untouched. When the door closes first, the request is answered
/// with the task as `GetTask` reports it at that moment, whatever its state.
#[allow(clippy::too_many_arguments)]
async fn handle_message_send(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    sender: MessageSender,
    forgettable_session: bool,
    sse_tx: SseBroadcast,
    closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe before the message is routed, so no event of a task that settles straight after
    // it is enqueued or handed its input is missed. The sender is not kept: the channel closes
    // once the session's own senders are gone.
    let rx = sse_tx.subscribe();
    drop(sse_tx);

    let response = match accept_message_send(
        req.id,
        &req.params,
        task_registry,
        task_tx,
        sender,
        forgettable_session,
    ) {
        SendAnswer::Now(response) => response,
        SendAnswer::Held(held) => {
            let task = hold_until_settled(&held.task, rx, task_registry, closing).await;
            held.answer(&task)
        }
    };
    let _ = writer.write_all(response.as_bytes()).await;
}

/// Waits for `routed` to settle and returns it as `GetTask` reports it then, or as it stands when
/// the door closes or the broadcast ends first.
///
/// The registry is read once at the start, then on each of the task's events, on a lag, and every
/// [`HELD_SEND_RECHECK_INTERVAL`]. An event already received wins over the door closing.
async fn hold_until_settled(
    routed: &A2aTask,
    mut rx: tokio::sync::broadcast::Receiver<Arc<StreamEvent>>,
    task_registry: &Mutex<TaskRegistry>,
    mut closing: watch::Receiver<bool>,
) -> A2aTask {
    use tokio::sync::broadcast::error::RecvError;

    let task_id = routed.id.as_str();
    let mut recheck = tokio::time::interval_at(
        tokio::time::Instant::now() + HELD_SEND_RECHECK_INTERVAL,
        HELD_SEND_RECHECK_INTERVAL,
    );
    'held: loop {
        if let Some(task) = settled_task(task_registry, task_id) {
            return task;
        }
        // Another task's event says nothing about this one, so it is skipped without a read.
        loop {
            tokio::select! {
                biased;
                received = rx.recv() => match received {
                    Ok(event) if event.task_id != task_id => {}
                    Ok(_) | Err(RecvError::Lagged(_)) => continue 'held,
                    Err(RecvError::Closed) => break 'held,
                },
                () = door_closing(&mut closing) => break 'held,
                _ = recheck.tick() => continue 'held,
            }
        }
    }
    let held = task_registry.lock().unwrap().get_task(task_id);
    held.unwrap_or_else(|| routed.clone())
}

/// `task_id` as `GetTask` reports it once it has settled, or `None` while it runs.
///
/// A task has settled once the registry holds it terminal with its ending recorded, or waiting in
/// `TASK_STATE_INPUT_REQUIRED`. A terminal state alone is not enough: a cancel records the state
/// before the task stops and its ending after, and a task's hooks decide before its ending is
/// recorded.
fn settled_task(task_registry: &Mutex<TaskRegistry>, task_id: &str) -> Option<A2aTask> {
    let reg = task_registry.lock().unwrap();
    let task = reg.get_task(task_id)?;
    let state = &task.status.state;
    let settled =
        (state.is_terminal() && reg.has_ending(task_id)) || *state == TaskState::InputRequired;
    settled.then_some(task)
}

/// Whether `caller` reaches `task_id`, by the rule [`TaskCaller`] states: a session caller every
/// task, any other credential only the tasks it started.
///
/// Every path that reads, lists, cancels, continues, subscribes to or references a task decides
/// here, and answers a task it hides exactly as one the session never held.
fn visible_to(reg: &TaskRegistry, task_id: &str, caller: &TaskCaller) -> bool {
    match caller {
        TaskCaller::Session => true,
        TaskCaller::Credential(credential) => reg.owner(task_id) == Some(credential.as_str()),
    }
}

/// The `-32602` answering a request whose `params.tenant` is present and is anything but `""`,
/// the proto's unset value, or `None`. The door's one interface declares no tenant.
fn tenant_refusal(req: &JsonRpcRequest) -> Option<JsonRpcResponse> {
    let tenant = req.params.get("tenant").filter(|tenant| *tenant != "")?;
    Some(JsonRpcResponse::err(
        req.id.clone(),
        INVALID_PARAMS,
        &format!(
            "Invalid params: tenant {tenant} names a tenant; this agent serves no tenants, so \
             omit tenant"
        ),
    ))
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
/// A task [`visible_to`] hides from `caller` gets the same [`A2aError::TaskNotFound`] an id this
/// capsule never held gets.
fn handle_tasks_get(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller: &TaskCaller,
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
        .filter(|_| visible_to(&reg, &task_id, caller))
    {
        Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
        None => task_not_found(id, Some(&task_id)),
    }
}

/// How many tasks a `ListTasks` page holds when the request names no `pageSize`.
const DEFAULT_LIST_PAGE_SIZE: u64 = 50;

/// The most tasks a `ListTasks` page holds: a larger `pageSize` is lowered to this, as A2A lets a
/// server return fewer than asked.
const MAX_LIST_PAGE_SIZE: u64 = 100;

/// What a `statusTimestampAfter` that is not an RFC 3339 timestamp is answered with, after
/// `Invalid params: `.
const STATUS_TIMESTAMP_AFTER_INVALID: &str =
    "statusTimestampAfter must be an RFC 3339 timestamp, such as 2026-10-13T09:00:00Z";

/// What a `ListTasks` request asks for, as [`read_list_params`] reads it.
#[derive(Debug, Clone, PartialEq)]
struct ListRequest {
    filter: TaskFilter,
    /// After lowering to [`MAX_LIST_PAGE_SIZE`].
    page_size: u64,
    /// The position the page starts after, or `None` for the first page.
    after: Option<ListCursor>,
    include_artifacts: bool,
}

/// The `ListTasksRequest` in `params`, or the error answering request `id`.
///
/// Read by hand, in this order, each failure `-32602` naming the parameter: `contextId` is a
/// string, `""` for no filter; `status` is a v1.0 `TaskState` name, `TASK_STATE_UNSPECIFIED` for
/// no filter; `pageSize` is an integer from 1, [`DEFAULT_LIST_PAGE_SIZE`] when absent; `pageToken`
/// is `""` or a token this door issued; `statusTimestampAfter` is an RFC 3339 timestamp in any
/// offset, absent or `null` for no filter; `includeArtifacts` is a boolean. `historyLength` and
/// unknown members are ignored.
fn read_list_params(id: &Value, params: &Value) -> Result<ListRequest, JsonRpcResponse> {
    let invalid = |message: &str| {
        Err(JsonRpcResponse::err(
            id.clone(),
            INVALID_PARAMS,
            &format!("Invalid params: {message}"),
        ))
    };
    let context_id = match params.get("contextId") {
        None => None,
        Some(Value::String(context_id)) => Some(context_id.clone()).filter(|c| !c.is_empty()),
        Some(_) => return invalid("contextId must be a string"),
    };
    let state = match params.get("status") {
        None => StateFilter::Any,
        Some(Value::String(state)) => match StateFilter::from_wire_name(state) {
            Some(filter) => filter,
            None => {
                return invalid(&format!(
                    "status \"{state}\" is not an A2A v1.0 TaskState name, such as {}",
                    TaskState::Working.wire_name()
                ))
            }
        },
        Some(_) => return invalid("status must be an A2A v1.0 TaskState name"),
    };
    let page_size = match params.get("pageSize") {
        None => DEFAULT_LIST_PAGE_SIZE,
        Some(size) => match size.as_u64().filter(|size| *size >= 1) {
            Some(size) => size.min(MAX_LIST_PAGE_SIZE),
            None => {
                return invalid(&format!(
                    "pageSize must be an integer of at least 1; a size above \
                     {MAX_LIST_PAGE_SIZE} is answered with {MAX_LIST_PAGE_SIZE}"
                ))
            }
        },
    };
    let after = match params.get("pageToken") {
        None => None,
        Some(Value::String(token)) if token.is_empty() => None,
        Some(Value::String(token)) => match ListCursor::decode(token) {
            Some(cursor) => Some(cursor),
            None => return invalid("pageToken is not a nextPageToken this agent issued"),
        },
        Some(_) => return invalid("pageToken must be a string"),
    };
    let status_after = match params.get("statusTimestampAfter") {
        None | Some(Value::Null) => None,
        Some(Value::String(after)) => match chrono::DateTime::parse_from_rfc3339(after) {
            Ok(after) => Some(after.with_timezone(&chrono::Utc)),
            Err(_) => return invalid(STATUS_TIMESTAMP_AFTER_INVALID),
        },
        Some(_) => return invalid(STATUS_TIMESTAMP_AFTER_INVALID),
    };
    let include_artifacts = match params.get("includeArtifacts") {
        None => false,
        Some(Value::Bool(include)) => *include,
        Some(_) => return invalid("includeArtifacts must be a boolean"),
    };
    Ok(ListRequest {
        filter: TaskFilter {
            context_id,
            state,
            status_after,
        },
        page_size,
        after,
        include_artifacts,
    })
}

/// `ListTasks`: one page of the tasks [`visible_to`] lets `caller` see, newest change first, as a
/// `ListTasksResponse` whose four members are always present.
///
/// Each task is exactly what `GetTask` answers for it, except that `artifacts` is absent unless
/// `includeArtifacts` is `true`, and then is the task's artifacts or `[]`. `nextPageToken` is `""`
/// on the last page, `pageSize` is the size used, and `totalSize` counts every visible task that
/// matches the filters. The filters and visibility apply afresh to every page: a token names a
/// position, nothing more.
fn handle_tasks_list(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller: &TaskCaller,
) -> String {
    let request = match read_list_params(&id, params) {
        Ok(request) => request,
        Err(response) => return response.into_http_response(),
    };
    let page = {
        let reg = task_registry.lock().unwrap();
        reg.list_tasks(
            &request.filter,
            request.after.as_ref(),
            usize::try_from(request.page_size).unwrap_or(usize::MAX),
            |task_id| visible_to(&reg, task_id, caller),
        )
    };
    let tasks: Vec<A2aTask> = page
        .tasks
        .into_iter()
        .map(|mut task| {
            task.artifacts = request
                .include_artifacts
                .then(|| task.artifacts.unwrap_or_default());
            task
        })
        .collect();
    JsonRpcResponse::ok(
        id,
        serde_json::json!({
            "tasks": tasks,
            "nextPageToken": page.next.as_ref().map(ListCursor::encode).unwrap_or_default(),
            "pageSize": request.page_size,
            "totalSize": page.total,
        }),
    )
    .into_http_response()
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
/// A task [`visible_to`] hides from `caller` gets that same `TaskNotFound`, and nothing is
/// cancelled.
fn handle_tasks_cancel(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    caller: &TaskCaller,
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
        if !visible_to(&reg, &task_id, caller) {
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

/// `murmur.StopSession`: cancel everything this session still holds and report what it leaves
/// running. The answer lists the door extension in `A2A-Extensions`: calling a murmur method is
/// the use of it.
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
fn handle_stop_session(
    id: Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let canceled = task_registry.lock().unwrap().cancel_every_live(false);

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
    .into_http_response_echoing(&[DOOR_EXTENSION_URI])
}

#[cfg(test)]
#[allow(clippy::print_stdout, clippy::print_stderr)]
mod tests {
    use super::*;
    use crate::errors::RuntimeError;

    const ACCEPTANCES: [TaskAcceptance; 3] = [
        TaskAcceptance::None,
        TaskAcceptance::Single,
        TaskAcceptance::Queue,
    ];

    /// The card an http capsule serves.
    const HTTP_TRANSPORT: TransportCapabilities = TransportCapabilities {
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

    /// Every murmur frame: what `murmur.WatchSession` carries, and so the stream extension's
    /// `params.frames` under every acceptance, since `murmur.WatchSession` is served under each.
    const ALL_FRAMES: &[&str] = &[
        "text",
        "thinking",
        "tool-call-started",
        "tool-call-progress",
        "artifact",
        "gap",
        "lagged",
        "connection-ack",
        "capsule-closed",
    ];

    /// The stream extension's `params.frames` a capsule on each transport serves under each
    /// acceptance. Written once per transport, so a frame one transport alone writes is an edit to
    /// that transport's table.
    const HTTP_FRAMES: [(TaskAcceptance, &[&str]); 3] = [
        (TaskAcceptance::None, ALL_FRAMES),
        (TaskAcceptance::Single, ALL_FRAMES),
        (TaskAcceptance::Queue, ALL_FRAMES),
    ];

    const PROCESS_FRAMES: [(TaskAcceptance, &[&str]); 3] = [
        (TaskAcceptance::None, ALL_FRAMES),
        (TaskAcceptance::Single, ALL_FRAMES),
        (TaskAcceptance::Queue, ALL_FRAMES),
    ];

    /// What an activated `SendStreamingMessage` carries: every frame but the observer's own.
    #[test]
    fn stream_extension_send_streaming_message_carries_the_first_seven_frames() {
        for transport in TransportKind::ALL {
            for method in [
                DoorMethod::SendStreamingMessage,
                DoorMethod::SubscribeToTask,
            ] {
                let carried: Vec<&str> = StreamFrame::ALL
                    .into_iter()
                    .filter(|&frame| writes_frame(method, frame, transport))
                    .map(StreamFrame::wire_name)
                    .collect();
                assert_eq!(carried, ALL_FRAMES[..7], "{method:?} {transport:?}");
            }
            let watched: Vec<&str> = StreamFrame::ALL
                .into_iter()
                .filter(|&frame| writes_frame(DoorMethod::WatchSession, frame, transport))
                .map(StreamFrame::wire_name)
                .collect();
            assert_eq!(watched, ALL_FRAMES, "{transport:?}");
        }
    }

    #[test]
    fn stream_extension_description_says_what_activation_does() {
        let card = card_for(&TaskAcceptance::Queue, DeclaredPlanes::default());
        let stream = card["capabilities"]["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|extension| extension["uri"] == STREAM_EXTENSION_URI)
            .unwrap();
        assert_eq!(
            stream["description"],
            "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on \
             SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header \
             names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under \
             metadata keyed by this URI."
        );
        assert_eq!(stream["required"], false);
    }

    /// Every `TransportCapabilities` a session can stage.
    fn every_transport() -> Vec<TransportCapabilities> {
        TransportKind::ALL
            .into_iter()
            .map(|kind| TransportCapabilities { kind })
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
        assert_eq!(checked, 3 * 2 * 2 * 4 * 2);
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
                            "description": "Every JSON-RPC method this door answers, A2A methods and murmur's own alike, the murmur ones named murmur.<Method>; whether it accepts tasks from peer capsules; and the forgetSession field a message's metadata may carry under this URI.",
                            "required": false,
                            "params": {
                                "methods": ["SendMessage", "SendStreamingMessage", "murmur.WatchSession", "GetTask", "ListTasks", "CancelTask", "SubscribeToTask", "murmur.StopSession"],
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
                            "description": "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata keyed by this URI.",
                            "required": false,
                            "params": {
                                "frames": ["text", "thinking", "tool-call-started", "tool-call-progress", "artifact", "gap", "lagged", "connection-ack", "capsule-closed"]
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
                let transport = TransportCapabilities { kind };
                assert_eq!(
                    served_frames(acceptance, transport),
                    *expected,
                    "{kind:?} under {acceptance:?}"
                );
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
            let transport = TransportCapabilities { kind };
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
                scopes: vec!["GetTask".to_string(), "murmur.WatchSession".to_string()],
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
        assert_eq!(checked, 2 * 3 * 2 * 2 * 4 * 2 * 2);
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
                            "description": "Every JSON-RPC method this door answers, A2A methods and murmur's own alike, the murmur ones named murmur.<Method>; whether it accepts tasks from peer capsules; and the forgetSession field a message's metadata may carry under this URI.",
                            "required": false,
                            "params": {
                                "methods": ["SendMessage", "SendStreamingMessage", "murmur.WatchSession", "GetTask", "ListTasks", "CancelTask", "SubscribeToTask", "murmur.StopSession", "GetExtendedAgentCard"],
                                "peerTasks": false
                            }
                        },
                        {
                            "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
                            "description": "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata keyed by this URI.",
                            "required": false,
                            "params": {
                                "frames": ["text", "thinking", "tool-call-started", "tool-call-progress", "artifact", "gap", "lagged", "connection-ack", "capsule-closed"]
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
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1", "description": "Every JSON-RPC method this door answers, A2A methods and murmur's own alike, the murmur ones named murmur.<Method>; whether it accepts tasks from peer capsules; and the forgetSession field a message's metadata may carry under this URI.", "required": false,
                          "params": { "methods": ["SendMessage", "SendStreamingMessage", "murmur.WatchSession", "GetTask", "ListTasks", "CancelTask", "SubscribeToTask", "murmur.StopSession", "GetExtendedAgentCard"], "peerTasks": false } },
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1", "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.", "required": false,
                          "params": { "sessionId": "ses_019f01a940ce7761854e768ecbe3d399", "tools": ["bash"], "shell": true, "network": true, "planes": ["files"] } },
                        { "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1", "description": "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata keyed by this URI.", "required": false,
                          "params": { "frames": ["text", "thinking", "tool-call-started", "tool-call-progress", "artifact", "gap", "lagged", "connection-ack", "capsule-closed"] } }
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
                "murmur.WatchSession",
                "GetTask",
                "ListTasks",
                "CancelTask",
                "murmur.StopSession",
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

    /// `streaming` is the served `SendStreamingMessage` alone: every transport writes the A2A
    /// v1.0 task events, a process capsule whose driver streams no text included. `CancelTask` is
    /// served whatever the transport: every transport can be stopped.
    #[test]
    fn card_capabilities_are_the_method_on_every_transport() {
        for transport in every_transport() {
            let card = card_for_transport(
                &TaskAcceptance::Single,
                DeclaredPlanes::default(),
                transport,
            );
            assert_eq!(card["capabilities"]["streaming"], true, "{card}");
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
        for transport in every_transport() {
            let card =
                card_for_transport(&TaskAcceptance::None, DeclaredPlanes::default(), transport);
            assert_eq!(card["capabilities"]["streaming"], false, "{card}");
            assert!(door_methods(&card).contains(&"CancelTask"), "{card}");
        }
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

    /// `SendMessage` with `params`, answered at acceptance: the message asks for
    /// `configuration.returnImmediately`.
    fn send_now(
        id: Value,
        params: &Value,
        task_registry: &Arc<Mutex<TaskRegistry>>,
        task_tx: &mpsc::Sender<IncomingTask>,
        sender: MessageSender,
        forgettable_session: bool,
    ) -> String {
        let mut params = params.clone();
        params["configuration"] = serde_json::json!({ "returnImmediately": true });
        match accept_message_send(
            id,
            &params,
            task_registry,
            task_tx,
            sender,
            forgettable_session,
        ) {
            SendAnswer::Now(response) => response,
            SendAnswer::Held(_) => unreachable!("a message returning immediately is never held"),
        }
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
            caller_member: caller.map(str::to_string),
            caller: caller.map_or(TaskCaller::Session, |member| {
                TaskCaller::Credential(format!("member:{member}"))
            }),
        }
    }

    /// The caller holding a token of `credential`.
    fn credential(credential: &str) -> TaskCaller {
        TaskCaller::Credential(credential.to_string())
    }

    fn response_json(response: &str) -> Value {
        let body = &response[response.find("\r\n\r\n").expect("header terminator") + 4..];
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    /// `message_send_params` whose message carries `forgetSession: forget` under the door
    /// extension's key.
    fn forget_params(message_id: &str, forget: bool) -> Value {
        let mut params = message_send_params(message_id);
        params["message"]["metadata"] =
            serde_json::json!({ DOOR_EXTENSION_URI: { "forgetSession": forget } });
        params
    }

    /// A capsule with no harness session refuses a forget before the message is routed: no task
    /// is registered or handed to the loop, and the refusal echoes no extension.
    #[test]
    fn a_forget_asked_of_a_capsule_with_no_harness_session_is_refused_before_routing() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);

        let raw = send_now(
            serde_json::json!(1),
            &forget_params("msg_forget", true),
            &task_registry,
            &task_tx,
            operator(),
            false,
        );

        let response = response_json(&raw);
        assert_eq!(response["error"]["code"], INVALID_PARAMS, "{response}");
        assert_eq!(
            response["error"]["message"],
            "forgetSession asks this capsule to forget the harness session this context names, \
             and only a capsule on inference.transport: process has one"
        );
        assert!(response["error"].get("data").is_none(), "{response}");
        assert!(!split_response(&raw).0.contains("A2A-Extensions"), "{raw}");
        assert!(
            task_rx.try_recv().is_err(),
            "nothing was handed to the loop"
        );
        assert!(task_registry.lock().unwrap().history.is_empty());

        // `false` asks for nothing, so the same capsule serves it and echoes the extension the
        // message used.
        let raw = send_now(
            serde_json::json!(2),
            &forget_params("msg_keep", false),
            &task_registry,
            &task_tx,
            operator(),
            false,
        );
        assert_eq!(
            response_json(&raw)["result"]["task"]["status"]["state"],
            "TASK_STATE_SUBMITTED"
        );
        assert!(
            split_response(&raw)
                .0
                .contains(&format!("\r\nA2A-Extensions: {DOOR_EXTENSION_URI}")),
            "{raw}"
        );
        assert!(!task_rx.try_recv().unwrap().forget_session);
    }

    /// A capsule that keeps a harness session carries the forget to the task the message starts,
    /// and the answer echoes the door extension. A message that does not use the extension is
    /// answered with no echo.
    #[test]
    fn a_forget_reaches_the_task_its_message_starts_and_echoes_the_door_extension() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);

        let raw = send_now(
            serde_json::json!(1),
            &forget_params("msg_forget", true),
            &task_registry,
            &task_tx,
            operator(),
            true,
        );
        assert_eq!(
            response_json(&raw)["result"]["task"]["status"]["state"],
            "TASK_STATE_SUBMITTED"
        );
        assert!(
            split_response(&raw)
                .0
                .contains(&format!("\r\nA2A-Extensions: {DOOR_EXTENSION_URI}")),
            "{raw}"
        );
        assert!(task_rx.try_recv().unwrap().forget_session);

        let raw = send_now(
            serde_json::json!(2),
            &message_send_params("msg_plain"),
            &task_registry,
            &task_tx,
            operator(),
            true,
        );
        assert!(!split_response(&raw).0.contains("A2A-Extensions"), "{raw}");
        assert!(!task_rx.try_recv().unwrap().forget_session);
    }

    /// A closed registry refuses `SendMessage` whatever room its queue has, and the refused
    /// message never reaches the task loop or the registry.
    #[test]
    fn a_closed_registry_answers_message_send_rejected_and_enqueues_nothing() {
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        assert!(task_registry.lock().unwrap().close_to_new_work().is_empty());
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);

        let response = response_json(&send_now(
            serde_json::json!(1),
            &message_send_params("msg_late"),
            &task_registry,
            &task_tx,
            operator(),
            false,
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
            reg.record_owner("tsk_mine", "member:planner");
            reg.enqueue("tsk_theirs", "ctx_theirs");
            reg.record_owner("tsk_theirs", "member:reviewer");
            reg.enqueue("tsk_operator", "ctx_operator");
        }
        let live = LiveDelegations::new();
        let cancel = |task_id: &str| {
            response_json(&handle_tasks_cancel(
                serde_json::json!(1),
                &serde_json::json!({"id": task_id}),
                &task_registry,
                &credential("member:planner"),
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
            &TaskCaller::Session,
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
        response_json(&send_now(
            serde_json::json!(1),
            &serde_json::json!({ "message": message }),
            registry,
            task_tx,
            member(caller),
            false,
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

    /// A registry holding `tsk_wait`, which `planner` started and which waits for input in
    /// `ctx_wait`, with `tsk_done` completed and `tsk_queued` submitted, both by the operator. Its
    /// queue is full, so it has no room for a new task.
    fn routing_registry() -> (Arc<Mutex<TaskRegistry>>, oneshot::Receiver<String>) {
        let mut reg = TaskRegistry::new(4, TaskAcceptance::Queue);
        reg.enqueue("tsk_done", "ctx_done");
        reg.start_task(
            "tsk_done".to_string(),
            "ctx_done".to_string(),
            crate::lanes::TaskLane::User,
        );
        reg.finish_task(TaskState::Completed);
        reg.record_ending("tsk_done", "done", Some("four"), false, &[], &[]);
        reg.enqueue("tsk_wait", "ctx_wait");
        reg.record_owner("tsk_wait", "member:planner");
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
                &TaskCaller::Session,
            ));
            assert_eq!(answer["error"]["code"], -32602, "{answer}");
            assert_eq!(answer["error"]["message"], "GetTask requires an id");
        }
        let answer = response_json(&handle_tasks_get(
            serde_json::json!(1),
            &serde_json::json!({"id": "tsk_done", "historyLength": 3}),
            &registry,
            &TaskCaller::Session,
        ));
        assert_eq!(answer["result"]["status"]["state"], "TASK_STATE_COMPLETED");
    }

    /// One `ListTasks` answer for `caller`.
    fn list(registry: &Arc<Mutex<TaskRegistry>>, caller: &TaskCaller, params: Value) -> Value {
        response_json(&handle_tasks_list(
            serde_json::json!(1),
            &params,
            registry,
            caller,
        ))
    }

    /// `statusTimestampAfter` takes an RFC 3339 timestamp in any offset and keeps the tasks
    /// changed at or after it; `null` keeps every task.
    #[test]
    fn list_status_timestamp_after_keeps_tasks_changed_at_or_after_it() {
        let (registry, _waiter) = routing_registry();
        let total = |after: Value| {
            let answer = list(
                &registry,
                &TaskCaller::Session,
                serde_json::json!({"statusTimestampAfter": after}),
            );
            answer["result"]["totalSize"]
                .as_u64()
                .unwrap_or_else(|| panic!("{after}: {answer}"))
        };
        assert_eq!(total(Value::Null), 3);
        assert_eq!(total(serde_json::json!("2000-01-01T00:00:00Z")), 3);
        assert_eq!(total(serde_json::json!("2000-01-01T02:00:00+02:00")), 3);
        assert_eq!(total(serde_json::json!("2999-01-01T00:00:00Z")), 0);
        let newest = registry.lock().unwrap().get_task("tsk_queued").unwrap();
        let cutoff = newest.status.timestamp.clone().unwrap();
        let answer = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"statusTimestampAfter": cutoff}),
        );
        let tasks = answer["result"]["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["id"], "tsk_queued", "{answer}");
        assert!(tasks
            .iter()
            .all(|task| { task["status"]["timestamp"].as_str().unwrap() >= cutoff.as_str() }));
    }

    /// Each parameter is read in its documented order, and each refusal names it.
    #[test]
    fn list_params_are_refused_by_name() {
        let (registry, _waiter) = routing_registry();
        for (params, code, names) in [
            (serde_json::json!({"contextId": 7}), -32602, "contextId"),
            (serde_json::json!({"status": "done"}), -32602, "status"),
            (serde_json::json!({"status": 3}), -32602, "status"),
            (serde_json::json!({"pageSize": 0}), -32602, "pageSize"),
            (serde_json::json!({"pageSize": -1}), -32602, "pageSize"),
            (serde_json::json!({"pageSize": 2.5}), -32602, "pageSize"),
            (serde_json::json!({"pageSize": "2"}), -32602, "pageSize"),
            (
                serde_json::json!({"pageToken": "not-a-token"}),
                -32602,
                "pageToken",
            ),
            (serde_json::json!({"pageToken": 1}), -32602, "pageToken"),
            (
                serde_json::json!({"statusTimestampAfter": "yesterday"}),
                -32602,
                "statusTimestampAfter",
            ),
            (
                serde_json::json!({"statusTimestampAfter": 42}),
                -32602,
                "statusTimestampAfter",
            ),
            (
                serde_json::json!({"statusTimestampAfter": true}),
                -32602,
                "statusTimestampAfter",
            ),
            (
                serde_json::json!({"statusTimestampAfter": "2026-13-01T00:00:00Z"}),
                -32602,
                "statusTimestampAfter",
            ),
            (
                serde_json::json!({"includeArtifacts": "yes"}),
                -32602,
                "includeArtifacts",
            ),
            // The first refused parameter in reading order is the one named.
            (
                serde_json::json!({"includeArtifacts": 1, "status": "done"}),
                -32602,
                "status",
            ),
        ] {
            let answer = list(&registry, &TaskCaller::Session, params.clone());
            assert_eq!(answer["error"]["code"], code, "{params}: {answer}");
            let message = answer["error"]["message"].as_str().unwrap();
            assert!(message.contains(names), "{params}: {message}");
        }
        let not_a_timestamp = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"statusTimestampAfter": "yesterday"}),
        );
        assert_eq!(
            not_a_timestamp["error"]["message"],
            "Invalid params: statusTimestampAfter must be an RFC 3339 timestamp, such as \
             2026-10-13T09:00:00Z"
        );
        for accepted in [
            serde_json::json!({}),
            serde_json::json!({"contextId": "", "status": "TASK_STATE_UNSPECIFIED",
                "pageToken": "", "statusTimestampAfter": null, "historyLength": 3,
                "includeArtifacts": false, "somethingElse": true}),
        ] {
            let answer = list(&registry, &TaskCaller::Session, accepted.clone());
            assert_eq!(answer["result"]["totalSize"], 3, "{accepted}: {answer}");
            assert_eq!(answer["result"]["pageSize"], 50, "{accepted}: {answer}");
            assert_eq!(
                answer["result"]["nextPageToken"], "",
                "{accepted}: {answer}"
            );
        }
        let lowered = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"pageSize": 500}),
        );
        assert_eq!(lowered["result"]["pageSize"], 100, "{lowered}");
        let nothing = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"status": "TASK_STATE_AUTH_REQUIRED"}),
        );
        assert_eq!(
            nothing["result"],
            serde_json::json!({"tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0})
        );
    }

    /// A credential lists only the tasks it started; a session caller lists every task. Each
    /// listed task is `GetTask`'s, with `artifacts` only when asked for.
    #[test]
    fn list_tasks_shows_each_caller_what_it_may_see() {
        let (registry, _waiter) = routing_registry();
        let ids = |answer: &Value| -> Vec<String> {
            answer["result"]["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|task| task["id"].as_str().unwrap().to_string())
                .collect()
        };
        let planner = list(
            &registry,
            &credential("member:planner"),
            serde_json::json!({}),
        );
        assert_eq!(ids(&planner), ["tsk_wait"]);
        assert_eq!(planner["result"]["totalSize"], 1);
        let alice = list(&registry, &credential("alice"), serde_json::json!({}));
        assert_eq!(
            alice["result"],
            serde_json::json!({"tasks": [], "nextPageToken": "", "pageSize": 50, "totalSize": 0})
        );
        let session = list(&registry, &TaskCaller::Session, serde_json::json!({}));
        assert_eq!(ids(&session), ["tsk_queued", "tsk_wait", "tsk_done"]);

        let reg = registry.lock().unwrap();
        for task in session["result"]["tasks"].as_array().unwrap() {
            let mut held =
                serde_json::to_value(reg.get_task(task["id"].as_str().unwrap())).unwrap();
            held.as_object_mut().unwrap().remove("artifacts");
            assert_eq!(*task, held);
        }
        drop(reg);
        let with = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"includeArtifacts": true}),
        );
        let tasks = with["result"]["tasks"].as_array().unwrap();
        assert_eq!(tasks[0]["artifacts"], serde_json::json!([]), "{with}");
        assert_eq!(tasks[1]["artifacts"][0]["artifactId"], "prompt", "{with}");
        assert_eq!(tasks[2]["artifacts"][0]["artifactId"], "response", "{with}");

        let first = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"pageSize": 2}),
        );
        assert_eq!(ids(&first), ["tsk_queued", "tsk_wait"]);
        let token = first["result"]["nextPageToken"].as_str().unwrap();
        assert!(!token.is_empty());
        let second = list(
            &registry,
            &TaskCaller::Session,
            serde_json::json!({"pageSize": 2, "pageToken": token}),
        );
        assert_eq!(ids(&second), ["tsk_done"]);
        assert_eq!(second["result"]["nextPageToken"], "");
        let foreign = list(
            &registry,
            &credential("member:planner"),
            serde_json::json!({"pageSize": 2, "pageToken": token}),
        );
        assert_eq!(
            ids(&foreign),
            Vec::<String>::new(),
            "a token is a position only"
        );
    }

    /// A tenant other than the proto's unset `""` is refused, and nothing else is.
    #[test]
    fn tenant_is_refused_unless_unset() {
        let req = |params: Value| JsonRpcRequest {
            id: serde_json::json!(1),
            method: "GetTask".to_string(),
            params,
        };
        for tenant in [
            serde_json::json!("acme"),
            serde_json::json!(7),
            serde_json::json!(null),
            serde_json::json!({}),
        ] {
            let refused = tenant_refusal(&req(serde_json::json!({"tenant": tenant})))
                .expect("a tenant is refused");
            let answer = serde_json::to_value(&refused).unwrap();
            assert_eq!(answer["error"]["code"], -32602, "{answer}");
            assert_eq!(
                answer["error"]["message"],
                format!(
                    "Invalid params: tenant {tenant} names a tenant; this agent serves no \
                     tenants, so omit tenant"
                )
            );
        }
        assert!(tenant_refusal(&req(serde_json::json!({"tenant": ""}))).is_none());
        assert!(tenant_refusal(&req(serde_json::json!({"id": "tsk_1"}))).is_none());
    }

    /// The operator is the session; every other token, a formation token's included, is its
    /// credential.
    #[test]
    fn task_caller_of_a_grant_is_the_session_only_for_the_operator() {
        let auth = crate::door_auth::DoorAuth::mint(&murmur_artifact::NetworkAuthentication {
            scheme: murmur_artifact::AuthenticationScheme::Bearer,
            credentials: vec![murmur_artifact::DoorCredential {
                name: "alice".to_string(),
                scopes: vec!["ListTasks".to_string()],
            }],
        })
        .unwrap();
        let grant_of = |name: &str| {
            let token = &auth.tokens().iter().find(|(n, _)| n == name).unwrap().1;
            auth.verify(&[&crate::door_auth::bearer_header(token)])
                .unwrap()
        };
        assert_eq!(TaskCaller::of(None), TaskCaller::Session);
        assert_eq!(
            TaskCaller::of(Some(&grant_of("operator"))),
            TaskCaller::Session
        );
        assert_eq!(
            TaskCaller::of(Some(&grant_of("alice"))),
            credential("alice")
        );
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
            [
                "murmur.WatchSession",
                "GetTask",
                "ListTasks",
                "CancelTask",
                "murmur.StopSession"
            ]
        );
        for acceptance in [TaskAcceptance::Single, TaskAcceptance::Queue] {
            assert_eq!(
                served_methods(&acceptance, false),
                [
                    "SendMessage",
                    "SendStreamingMessage",
                    "murmur.WatchSession",
                    "GetTask",
                    "ListTasks",
                    "CancelTask",
                    "SubscribeToTask",
                    "murmur.StopSession"
                ]
            );
        }
    }

    /// `SubscribeToTask` streams exactly when `SendStreamingMessage` does, and `ListTasks` is
    /// served wherever `GetTask` is.
    #[test]
    fn door_method_subscribe_streams_exactly_when_send_streaming_message_does() {
        for acceptance in &ACCEPTANCES {
            for authenticated in [false, true] {
                let resolves = |method: DoorMethod| {
                    DoorMethod::resolve(method.wire_name(), acceptance, authenticated).is_some()
                };
                assert_eq!(
                    resolves(DoorMethod::SubscribeToTask),
                    resolves(DoorMethod::SendStreamingMessage),
                    "{acceptance:?}"
                );
                assert_eq!(
                    resolves(DoorMethod::ListTasks),
                    resolves(DoorMethod::GetTask),
                    "{acceptance:?}"
                );
            }
        }
    }

    /// The push notification config methods are answered, never served: no `DoorMethod` takes
    /// their names, and the card lists none of them.
    #[test]
    fn door_method_push_config_methods_are_not_door_methods() {
        for name in PUSH_CONFIG_METHODS {
            for acceptance in &ACCEPTANCES {
                for authenticated in [false, true] {
                    assert_eq!(DoorMethod::resolve(name, acceptance, authenticated), None);
                    assert!(!served_methods(acceptance, authenticated).contains(&name));
                }
            }
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

    /// A murmur method is named `murmur.` and a PascalCase word, which no A2A rpc can be, and an
    /// A2A method is named as its rpc, which holds no `.`. A new murmur method under a bare name
    /// fails here before it can collide with a method A2A adds.
    #[test]
    fn door_method_murmur_methods_are_namespaced() {
        let rpcs = crate::a2a_conformance::v1_methods().expect("the vendored a2a.proto reads");
        for method in DoorMethod::ALL {
            let name = method.wire_name();
            if rpcs.iter().any(|rpc| rpc.name == name) {
                assert!(!name.contains('.'), "{name} is an A2A rpc");
                continue;
            }
            let word = name.strip_prefix("murmur.").unwrap_or_else(|| {
                panic!("{name} is not an A2A rpc, so it is named murmur.<Method>")
            });
            let mut chars = word.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_uppercase())
                    && chars.all(|c| c.is_ascii_alphanumeric()),
                "{name}: murmur.<Method> takes an ASCII PascalCase word"
            );
        }
    }

    #[test]
    fn door_method_resolve_matches_names_exactly() {
        for acceptance in &ACCEPTANCES {
            for name in [
                "tasks/list",
                "tasks/resubscribe",
                "listTasks",
                "subscribeToTask",
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
                "stream/watch",
                "session/stop",
                "murmur.watchsession",
                " murmur.StopSession",
                "murmur.StopSession/",
                "WatchSession",
                "StopSession",
                "murmur/StopSession",
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
    /// reproduces that shape — `handle_watch_session` spawned onto the pool, as `serve_http` spawns
    /// every connection, beside a `LocalSet` task that holds the calling thread with
    /// `std::thread::sleep` across a due tick — and asserts what a client actually reads: a
    /// heartbeat while the thread is still held.
    ///
    /// `protocol_page_says_the_heartbeat_keeps_its_cadence_during_a_turn` holds the protocol page to
    /// this behaviour and must change with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watch_session_heartbeat_continues_while_session_thread_is_blocked() {
        use std::time::{Duration, Instant};

        // Longer than one heartbeat interval, so a tick falls due inside the blocked stretch.
        const BLOCK: Duration = Duration::from_secs(20);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Read on its own OS thread, so what the client records does not depend on the thread
        // about to be held.
        let (client, line_rx) = spawn_sse_line_reader(addr, BLOCK + Duration::from_secs(10));

        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
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
                    handle_watch_session(
                        write_half,
                        &serde_json::json!(1),
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

    /// The head and the body of an HTTP response.
    fn split_response(received: &str) -> (&str, &str) {
        let at = received
            .find("\r\n\r\n")
            .unwrap_or_else(|| panic!("no header terminator in:\n{received}"));
        (&received[..at + 2], &received[at + 4..])
    }

    /// The body of an HTTP response: everything after the header block.
    fn sse_body(received: &str) -> &str {
        split_response(received).1
    }

    /// Every event of an SSE body as its `id:` and the JSON-RPC response its `data:` carries,
    /// heartbeats skipped. Every event is an optional `id:` line and one `data:` line; nothing
    /// carries an `event:` line.
    fn events_of(body: &str) -> Vec<(Option<u64>, Value)> {
        assert!(body.is_empty() || body.ends_with("\n\n"), "{body}");
        body.split("\n\n")
            .filter(|block| !block.is_empty() && *block != ":heartbeat")
            .map(|block| {
                let lines: Vec<&str> = block.lines().collect();
                let (id, data) = match lines.as_slice() {
                    [data] => (None, *data),
                    [id, data] => (
                        Some(id.strip_prefix("id: ").unwrap().parse().unwrap()),
                        *data,
                    ),
                    _ => panic!("an event is an optional id line and a data line: {block:?}"),
                };
                let data: Value = serde_json::from_str(data.strip_prefix("data: ").unwrap())
                    .unwrap_or_else(|e| panic!("data is not JSON ({e}): {block}"));
                assert_eq!(data["jsonrpc"], "2.0", "{block}");
                (id, data)
            })
            .collect()
    }

    /// One buffered event of `task_id` whose result is `{"n": id}`: a standard event both kinds of
    /// connection receive, or a murmur frame only an activated one does.
    fn stream_event(
        id: u64,
        task_id: &str,
        murmur: bool,
        ends: Option<StreamEnd>,
    ) -> Arc<StreamEvent> {
        let result = format!("{{\"n\":{id}}}");
        Arc::new(StreamEvent {
            id,
            task_id: task_id.to_string(),
            plain: (!murmur).then(|| result.clone()),
            extended: result,
            state: (!murmur).then_some(TaskState::Working),
            ends,
        })
    }

    /// The `n` each event's result carries, for events [`stream_event`] made.
    fn ns(events: &[(Option<u64>, Value)]) -> Vec<u64> {
        events
            .iter()
            .filter_map(|(_, data)| data["result"]["n"].as_u64())
            .collect()
    }

    /// A `SendStreamingMessage` request carrying `params`, with JSON-RPC id `id`.
    fn stream_request(id: Value, params: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            id,
            method: "SendStreamingMessage".to_string(),
            params,
        }
    }

    /// Serves one `SendStreamingMessage` connection on a fresh listener, returning the handler, the
    /// client's line reader and the client thread.
    async fn serve_message_stream(
        req: JsonRpcRequest,
        task_registry: Arc<Mutex<TaskRegistry>>,
        task_tx: mpsc::Sender<IncomingTask>,
        request: StreamRequest,
        sse_tx: &SseBroadcast,
        sse_buffer: &Arc<Mutex<SseEventBuffer>>,
        closing: watch::Receiver<bool>,
    ) -> (
        tokio::task::JoinHandle<()>,
        std::sync::mpsc::Receiver<(std::time::Instant, String)>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );
        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(sse_buffer);
        let handler = tokio::spawn(async move {
            let _read_half = read_half;
            handle_message_stream(
                write_half,
                req,
                &task_registry,
                &task_tx,
                operator(),
                false,
                request,
                sse,
                buffer,
                closing,
            )
            .await;
        });
        (handler, lines, client)
    }

    const PLAIN: StreamRequest = StreamRequest {
        last_event_id: None,
        activated: false,
    };

    const ACTIVATED: StreamRequest = StreamRequest {
        last_event_id: None,
        activated: true,
    };

    /// A `SendStreamingMessage` the door refuses because the session is closing is told so, in the
    /// same one unnumbered `TASK_STATE_REJECTED` task a busy refusal is answered with; a busy
    /// refusal keeps its own message.
    #[tokio::test]
    async fn message_stream_refused_by_a_closed_registry_says_the_session_is_closing() {
        for (close, expected) in [
            (true, crate::a2a::REJECTED_SESSION_CLOSING_MESSAGE),
            (false, crate::a2a::REJECTED_BUSY_MESSAGE),
        ] {
            let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(4);
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
            let (_closing_tx, closing) = watch::channel(false);
            let (handler, lines, client) = serve_message_stream(
                stream_request(serde_json::json!(1), message_send_params("msg_refused")),
                task_registry,
                task_tx,
                PLAIN,
                &sse_tx,
                &sse_buffer,
                closing,
            )
            .await;
            handler.await.unwrap();
            let mut received = String::new();
            collect_sse_lines(&lines, &mut received, None).await;
            client.join().unwrap();

            let (head, body) = split_response(&received);
            assert!(head.contains("content-type: text/event-stream"), "{head}");
            let events = events_of(body);
            assert_eq!(events.len(), 1, "one event: {body}");
            let (id, data) = &events[0];
            assert_eq!(*id, None, "a refusal is unnumbered");
            assert_eq!(data["id"], 1);
            let task = &data["result"]["task"];
            assert_eq!(task["status"]["state"], "TASK_STATE_REJECTED", "{data}");
            assert_eq!(
                task["status"]["message"]["parts"][0]["text"], expected,
                "{data}"
            );
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

    /// A `SendStreamingMessage` whose task cannot be handed to the task loop is answered with one
    /// JSON-RPC `-32603` body before any SSE head, and the task it would have started is taken
    /// back out of the registry.
    #[tokio::test]
    async fn message_stream_whose_hand_off_fails_is_refused_before_the_head() {
        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(4);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, task_rx) = mpsc::channel::<IncomingTask>(4);
        drop(task_rx);
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_message_stream(
            stream_request(serde_json::json!(5), message_send_params("msg_lost")),
            Arc::clone(&task_registry),
            task_tx,
            ACTIVATED,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        handler.await.unwrap();
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let (head, body) = split_response(&received);
        assert!(head.contains("content-type: application/json"), "{head}");
        assert!(!head.contains("text/event-stream"), "{head}");
        let answer: Value = serde_json::from_str(body).unwrap();
        assert_eq!(answer["id"], 5);
        assert_eq!(answer["error"]["code"], INTERNAL_ERROR);
        let reg = task_registry.lock().unwrap();
        assert!(reg.history.is_empty(), "the enqueue was rolled back");
        assert_eq!(reg.pending_count, 0);
    }

    /// A plain `SendStreamingMessage` opens with its task, writes only its own task's standard
    /// events with their ids and the request's id, and ends on its terminal status. Another task's
    /// events, its own murmur frames and the `A2A-Extensions` echo are absent.
    #[tokio::test]
    async fn message_stream_writes_only_its_own_tasks_standard_events() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_message_stream(
            stream_request(serde_json::json!("req-7"), message_send_params("msg_own")),
            task_registry,
            task_tx,
            PLAIN,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        wait_for_subscriber(&sse_tx).await;
        let task_id = task_rx.recv().await.unwrap().task_id;

        for event in [
            stream_event(1, &task_id, false, None),
            stream_event(2, &task_id, true, None),
            stream_event(3, "tsk_other", false, Some(StreamEnd::Terminal)),
            stream_event(4, &task_id, false, Some(StreamEnd::Terminal)),
            stream_event(5, &task_id, false, None),
        ] {
            sse_tx.send(event).unwrap();
        }
        handler.await.unwrap();
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let (head, body) = split_response(&received);
        assert!(
            !head.to_ascii_lowercase().contains("a2a-extensions"),
            "{head}"
        );
        let events = events_of(body);
        assert!(
            events.iter().all(|(_, data)| data["id"] == "req-7"),
            "{body}"
        );
        assert_eq!(events[0].0, None, "the opening task is unnumbered");
        assert_eq!(events[0].1["result"]["task"]["id"], task_id.as_str());
        assert_eq!(
            events[0].1["result"]["task"]["status"]["state"],
            "TASK_STATE_SUBMITTED"
        );
        assert_eq!(
            events[1..].iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [Some(1), Some(4)]
        );
        assert_eq!(ns(&events), [1, 4]);
    }

    /// The burst sent to a lagging connection: `LAG_QUEUE + LAG_MISSED` events of `task_id` with
    /// ids from 1, none of which ends a stream.
    fn lag_burst(task_id: &str, murmur: bool) -> Vec<Arc<StreamEvent>> {
        (1..=(LAG_QUEUE + LAG_MISSED) as u64)
            .map(|id| stream_event(id, task_id, murmur, None))
            .collect()
    }

    /// Capacity of the broadcast channel in the lag tests; each handler's queue holds this many.
    const LAG_QUEUE: usize = 4;
    /// Events beyond [`LAG_QUEUE`] in the burst, and so the count the `lagged` frame must carry.
    const LAG_MISSED: usize = 3;

    /// A `murmur.WatchSession` connection that falls behind is told how many live events it lost,
    /// in one unnumbered `lagged` message carrier written before the oldest retained event, and
    /// stays open.
    ///
    /// Current-thread flavour: the burst is sent with no await between sends, so the handler cannot
    /// drain its queue part-way through and the lost count is exact.
    #[tokio::test]
    async fn watch_session_writes_lagged_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (_closing_tx, closing) = watch::channel(false);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_watch_session(
                write_half,
                &serde_json::json!(1),
                Some(0),
                sse,
                buffer,
                "stateless".to_string(),
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        for event in lag_burst("tsk_a", true) {
            sse_tx.send(event).unwrap();
        }
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some("{\"n\":7}")).await;
        sse_tx
            .send(stream_event(100, "tsk_b", false, Some(StreamEnd::Terminal)))
            .unwrap();
        collect_sse_lines(&lines, &mut received, Some("{\"n\":100}")).await;

        watcher.abort();
        let _ = watcher.await;
        drop(sse_tx);
        client.join().unwrap();

        let (head, body) = split_response(&received);
        assert!(
            head.contains(&format!(
                "A2A-Extensions: {DOOR_EXTENSION_URI}, {STREAM_EXTENSION_URI}\r\n"
            )),
            "murmur.WatchSession always echoes both extensions: {head}"
        );
        let events = events_of(body);
        let frames: Vec<Value> = events
            .iter()
            .map(|(_, data)| crate::streaming::frame_of(&data["result"]))
            .collect();
        assert_eq!(
            frames[0],
            serde_json::json!({"frame": "connection-ack", "role": "observer", "conversationMode": "stateless"})
        );
        assert_eq!(
            frames[1],
            serde_json::json!({"frame": "lagged", "missed": LAG_MISSED})
        );
        for (id, data) in &events[..2] {
            assert_eq!(*id, None, "{data}");
            assert_eq!(data["result"]["message"]["role"], "ROLE_AGENT", "{data}");
            assert!(
                data["result"]["message"].get("contextId").is_none(),
                "{data}"
            );
        }
        assert_eq!(ns(&events), [4, 5, 6, 7, 100]);
        assert!(
            matches!(
                sse_buffer.lock().unwrap().replay_from(0),
                ReplayResult::Complete(events) if events.is_empty()
            ),
            "a lagged frame never enters the replay buffer"
        );
    }

    /// An activated `SendStreamingMessage` connection that falls behind is told how many live
    /// events it lost, in a `lagged` carrier of its task repeating the state it last wrote, keeps
    /// receiving, and closes on its own task's terminal status, not another task's.
    ///
    /// Current-thread flavour for the same reason as `watch_session_writes_lagged_frame`.
    #[tokio::test]
    async fn message_stream_writes_lagged_frame() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_message_stream(
            stream_request(serde_json::json!(1), message_send_params("msg_lagged")),
            task_registry,
            task_tx,
            ACTIVATED,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        wait_for_subscriber(&sse_tx).await;
        let task_id = task_rx.recv().await.unwrap().task_id;

        for event in lag_burst(&task_id, true) {
            sse_tx.send(event).unwrap();
        }
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some("{\"n\":7}")).await;
        sse_tx
            .send(stream_event(
                100,
                "tsk_other",
                false,
                Some(StreamEnd::Terminal),
            ))
            .unwrap();
        sse_tx
            .send(stream_event(
                101,
                &task_id,
                false,
                Some(StreamEnd::Terminal),
            ))
            .unwrap();
        handler.await.unwrap();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let (head, body) = split_response(&received);
        assert!(
            head.contains(&format!("A2A-Extensions: {STREAM_EXTENSION_URI}\r\n")),
            "{head}"
        );
        let events = events_of(body);
        assert!(events[0].1["result"].get("task").is_some(), "{body}");
        let (id, lagged) = &events[1];
        assert_eq!(*id, None);
        assert_eq!(
            lagged["result"]["statusUpdate"]["taskId"],
            task_id.as_str(),
            "{lagged}"
        );
        // A carrier repeats the state the connection last wrote.
        crate::a2a::assert_bare_status(
            &lagged["result"]["statusUpdate"]["status"],
            "TASK_STATE_SUBMITTED",
        );
        assert_eq!(
            crate::streaming::frame_of(&lagged["result"]),
            serde_json::json!({"frame": "lagged", "missed": LAG_MISSED})
        );
        assert_eq!(ns(&events), [4, 5, 6, 7, 101]);
    }

    /// A plain `SendStreamingMessage` connection that falls behind is told nothing of it, and a
    /// connection that lost its task's terminal status still ends: on a heartbeat after the loss
    /// it reads the task from the registry and writes how the task ended, as `GetTask` reports
    /// it, unnumbered.
    #[tokio::test]
    async fn message_stream_that_lost_its_ending_writes_it_from_the_registry() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(LAG_QUEUE);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_message_stream(
            stream_request(serde_json::json!(1), message_send_params("msg_lost")),
            Arc::clone(&task_registry),
            task_tx,
            PLAIN,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        wait_for_subscriber(&sse_tx).await;
        let incoming = task_rx.recv().await.unwrap();
        let task_id = incoming.task_id.clone();
        {
            let mut reg = task_registry.lock().unwrap();
            reg.start_task(
                task_id.clone(),
                incoming.context_id.clone(),
                crate::lanes::TaskLane::User,
            );
            reg.finish_task(TaskState::Completed);
            reg.record_ending(&task_id, "task completed", Some("four"), false, &[], &[]);
        }

        let lost_at = std::time::Instant::now();
        for event in lag_burst("tsk_other", false) {
            sse_tx.send(event).unwrap();
        }
        handler.await.unwrap();
        assert!(
            lost_at.elapsed() <= SSE_HEARTBEAT_INTERVAL + std::time::Duration::from_secs(2),
            "the ending was written within one heartbeat of the loss"
        );
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let events = events_of(sse_body(&received));
        assert_eq!(events.len(), 3, "{received}");
        assert!(events.iter().all(|(id, _)| id.is_none()), "{received}");
        let reported = task_registry.lock().unwrap().get_task(&task_id).unwrap();
        assert_eq!(
            events[1].1["result"]["artifactUpdate"]["artifact"],
            serde_json::to_value(reported.artifact("response").unwrap()).unwrap()
        );
        assert_eq!(events[1].1["result"]["artifactUpdate"]["lastChunk"], true);
        assert_eq!(
            events[2].1["result"]["statusUpdate"]["status"],
            serde_json::to_value(&reported.status).unwrap()
        );
        assert!(!received.contains(STREAM_EXTENSION_URI), "{received}");
    }

    /// A continuation resuming after an evicted cursor is told of the gap in a carrier of its task
    /// when it activated the extension, and told nothing when it did not. Either way it opens
    /// with the task, now working, and replays none of another task's events.
    #[tokio::test]
    async fn message_stream_continuation_past_an_evicted_cursor_writes_a_gap_only_when_activated() {
        for activated in [true, false] {
            let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
            let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(2)));
            let sink = Some((sse_tx.clone(), Arc::clone(&sse_buffer)));
            let other = StreamTask::new("tsk_other", "ctx_other");
            for turn in 1..=3 {
                crate::streaming::emit_status(
                    &sink,
                    &other,
                    TaskState::Working,
                    &format!("inference turn {turn}"),
                    None,
                );
            }
            let (registry, _waiter) = routing_registry();
            // The task's ending is already buffered, after an evicted stretch: the replay writes
            // it, and it ends the stream.
            let mut ended = registry.lock().unwrap().get_task("tsk_wait").unwrap();
            ended.status = crate::a2a::TaskStatus::of(TaskState::Failed);
            crate::streaming::emit_task_ending(&sink, &ended);
            let (task_tx, _task_rx) = mpsc::channel::<IncomingTask>(4);
            let (_closing_tx, closing) = watch::channel(false);
            let (handler, lines, client) = serve_message_stream(
                stream_request(
                    serde_json::json!(1),
                    serde_json::json!({"message": naming("tsk_wait", None, "main")}),
                ),
                Arc::clone(&registry),
                task_tx,
                StreamRequest {
                    last_event_id: Some(0),
                    activated,
                },
                &sse_tx,
                &sse_buffer,
                closing,
            )
            .await;
            handler.await.unwrap();
            let mut received = String::new();
            collect_sse_lines(&lines, &mut received, None).await;
            client.join().unwrap();

            let events = events_of(sse_body(&received));
            let opening = &events[0].1["result"]["task"];
            assert_eq!(opening["id"], "tsk_wait");
            assert_eq!(opening["status"]["state"], "TASK_STATE_WORKING");
            let gaps: Vec<&(Option<u64>, Value)> = events
                .iter()
                .filter(|(_, data)| crate::streaming::frame_of(&data["result"])["frame"] == "gap")
                .collect();
            if activated {
                assert_eq!(gaps.len(), 1, "{received}");
                let (id, gap) = gaps[0];
                assert_eq!(*id, None);
                assert_eq!(gap["result"]["statusUpdate"]["taskId"], "tsk_wait");
                assert_eq!(
                    crate::streaming::frame_of(&gap["result"]),
                    serde_json::json!({"frame": "gap", "firstAvailableId": 3})
                );
            } else {
                assert!(gaps.is_empty(), "{received}");
                assert!(!received.contains(STREAM_EXTENSION_URI), "{received}");
            }
            let last = events.last().unwrap();
            assert_eq!(last.0, Some(4));
            assert_eq!(
                last.1["result"]["statusUpdate"]["status"]["state"],
                "TASK_STATE_FAILED"
            );
            assert!(!received.contains("tsk_other"), "{received}");
        }
    }

    /// The session's last events are broadcast just before the door closes. A
    /// `SendStreamingMessage` handler that has not yet been scheduled when the close lands still
    /// writes the terminal status it was sent, and then ends the connection.
    ///
    /// Current-thread flavour: the event and the close are sent with no await between them, so
    /// the handler sees both at once — the shape of a busy host, where it is scheduled late.
    #[tokio::test]
    async fn message_stream_writes_queued_frames_before_the_door_closes() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let task_registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let (closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_message_stream(
            stream_request(serde_json::json!(1), message_send_params("msg_closing")),
            task_registry,
            task_tx,
            PLAIN,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        wait_for_subscriber(&sse_tx).await;
        let task_id = task_rx.recv().await.unwrap().task_id;

        sse_tx
            .send(stream_event(1, &task_id, false, Some(StreamEnd::Terminal)))
            .unwrap();
        closing_tx.send(true).unwrap();

        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        handler.await.unwrap();
        client.join().unwrap();

        let events = events_of(sse_body(&received));
        assert_eq!(events.len(), 2, "{received}");
        assert_eq!(events[1].0, Some(1));
        assert_eq!(ns(&events), [1]);
    }

    /// A `murmur.WatchSession` observer is written every event it was sent before the door closed,
    /// rather than having its socket dropped mid-queue, and the stream then ends without
    /// `capsule-closed`, as it does for any capsule that exits.
    #[tokio::test]
    async fn watch_session_writes_queued_frames_before_the_door_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );

        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));

        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(&sse_buffer);
        let (closing_tx, closing) = watch::channel(false);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_watch_session(
                write_half,
                &serde_json::json!("watch-1"),
                Some(0),
                sse,
                buffer,
                "stateless".to_string(),
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;

        sse_tx
            .send(stream_event(1, "tsk_closing", true, None))
            .unwrap();
        sse_tx
            .send(stream_event(
                2,
                "tsk_closing",
                false,
                Some(StreamEnd::Terminal),
            ))
            .unwrap();
        closing_tx.send(true).unwrap();

        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        watcher.await.unwrap();
        client.join().unwrap();

        let events = events_of(sse_body(&received));
        assert!(events.iter().all(|(_, data)| data["id"] == "watch-1"));
        assert_eq!(
            events.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [None, Some(1), Some(2)]
        );
        assert_eq!(
            crate::streaming::frame_of(&events[0].1["result"])["frame"],
            "connection-ack"
        );
        assert!(!received.contains("capsule-closed"), "{received}");
    }

    /// A `murmur.WatchSession` connection whose session's broadcast closes is told so in a
    /// `capsule-closed` message carrier, and ends.
    #[tokio::test]
    async fn watch_session_writes_capsule_closed_when_the_broadcast_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(30),
        );
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let (_closing_tx, closing) = watch::channel(false);
        let watcher = tokio::spawn(async move {
            let _read_half = read_half;
            handle_watch_session(
                write_half,
                &serde_json::json!(1),
                None,
                sse,
                sse_buffer,
                "threaded".to_string(),
                closing,
            )
            .await;
        });
        wait_for_subscriber(&sse_tx).await;
        drop(sse_tx);
        watcher.await.unwrap();
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let frames: Vec<Value> = events_of(sse_body(&received))
            .iter()
            .map(|(_, data)| crate::streaming::frame_of(&data["result"]))
            .collect();
        assert_eq!(
            frames,
            [
                serde_json::json!({"frame": "connection-ack", "role": "observer", "conversationMode": "threaded"}),
                serde_json::json!({"frame": "capsule-closed"}),
            ]
        );
    }

    const PROTOCOL_PAGE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/content/reference/streaming-protocol.md"
    );

    /// Serves one `SubscribeToTask` for `caller` on a fresh socket, as [`serve_message_stream`]
    /// serves a `SendStreamingMessage`.
    async fn serve_subscription(
        task_id: &str,
        task_registry: Arc<Mutex<TaskRegistry>>,
        caller: TaskCaller,
        sse_tx: &SseBroadcast,
        sse_buffer: &Arc<Mutex<SseEventBuffer>>,
        closing: watch::Receiver<bool>,
    ) -> (
        tokio::task::JoinHandle<()>,
        std::sync::mpsc::Receiver<(std::time::Instant, String)>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, lines) = spawn_sse_line_reader(
            listener.local_addr().unwrap(),
            std::time::Duration::from_secs(60),
        );
        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let sse = sse_tx.clone();
        let buffer = Arc::clone(sse_buffer);
        let req = JsonRpcRequest {
            id: serde_json::json!("sub-1"),
            method: "SubscribeToTask".to_string(),
            params: serde_json::json!({"id": task_id}),
        };
        let handler = tokio::spawn(async move {
            let _read_half = read_half;
            handle_task_subscribe(
                write_half,
                req,
                &task_registry,
                &caller,
                PLAIN,
                sse,
                buffer,
                closing,
            )
            .await;
        });
        (handler, lines, client)
    }

    /// A registry holding `tsk_run`, running in `ctx_run`.
    fn running_task_registry() -> Arc<Mutex<TaskRegistry>> {
        let mut reg = TaskRegistry::new(4, TaskAcceptance::Queue);
        reg.enqueue("tsk_run", "ctx_run");
        reg.start_task(
            "tsk_run".to_string(),
            "ctx_run".to_string(),
            crate::lanes::TaskLane::User,
        );
        Arc::new(Mutex::new(reg))
    }

    /// Every refusal a subscription can meet is one JSON-RPC error, decided before any stream.
    #[test]
    fn subscribe_refusals_are_errors_before_any_stream() {
        let registry = running_task_registry();
        {
            let mut reg = registry.lock().unwrap();
            reg.enqueue("tsk_alice", "ctx_a");
            reg.record_owner("tsk_alice", "alice");
            reg.enqueue("tsk_done", "ctx_done");
            reg.request_cancel("tsk_done");
        }
        let refusal = |params: Value, caller: &TaskCaller| {
            let req = JsonRpcRequest {
                id: serde_json::json!(1),
                method: "SubscribeToTask".to_string(),
                params,
            };
            let response = subscribable_task(&req, &registry, caller).unwrap_err();
            serde_json::to_value(&response).unwrap()
        };
        for params in [
            serde_json::json!({}),
            serde_json::json!({"id": ""}),
            serde_json::json!({"id": 7}),
        ] {
            let answer = refusal(params, &TaskCaller::Session);
            assert_eq!(answer["error"]["code"], -32602, "{answer}");
            assert_eq!(answer["error"]["message"], "SubscribeToTask requires an id");
        }
        let never = refusal(
            serde_json::json!({"id": "tsk_never_held"}),
            &credential("bob"),
        );
        for hidden in ["tsk_run", "tsk_alice"] {
            let answer = refusal(serde_json::json!({"id": hidden}), &credential("bob"));
            assert_eq!(answer["error"]["code"], -32001, "{answer}");
            let mut as_never = never.clone();
            as_never["error"]["data"][0]["metadata"]["taskId"] = Value::from(hidden);
            assert_eq!(answer, as_never, "a hidden task reads as one never held");
        }
        let ended = refusal(serde_json::json!({"id": "tsk_done"}), &TaskCaller::Session);
        assert_eq!(ended["error"]["code"], -32004, "{ended}");
        assert_eq!(
            ended["error"]["data"][0]["metadata"],
            serde_json::json!({"taskId": "tsk_done", "state": "TASK_STATE_CANCELED"})
        );
        let req = |id: &str| JsonRpcRequest {
            id: serde_json::json!(1),
            method: "SubscribeToTask".to_string(),
            params: serde_json::json!({"id": id}),
        };
        assert!(subscribable_task(&req("tsk_alice"), &registry, &credential("alice")).is_ok());
        assert!(subscribable_task(&req("tsk_run"), &registry, &TaskCaller::Session).is_ok());
    }

    /// A subscription opens with the task as `GetTask` reports it, and a live `input-required`
    /// does not end it: only the terminal status does.
    #[tokio::test]
    async fn subscribe_writes_through_input_required_to_the_terminal_status() {
        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(8);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let registry = running_task_registry();
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_subscription(
            "tsk_run",
            Arc::clone(&registry),
            TaskCaller::Session,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some("TASK_STATE_WORKING")).await;
        sse_tx
            .send(stream_event(
                1,
                "tsk_run",
                false,
                Some(StreamEnd::Interrupted),
            ))
            .unwrap();
        sse_tx
            .send(stream_event(
                2,
                "tsk_other",
                false,
                Some(StreamEnd::Terminal),
            ))
            .unwrap();
        sse_tx
            .send(stream_event(3, "tsk_run", false, Some(StreamEnd::Terminal)))
            .unwrap();
        handler.await.unwrap();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let events = events_of(sse_body(&received));
        assert_eq!(ns(&events), [1, 3], "{received}");
        assert_eq!(events[0].0, None, "the opening task is unnumbered");
        let opening = registry.lock().unwrap().get_task("tsk_run").unwrap();
        assert_eq!(
            events[0].1,
            serde_json::json!({"jsonrpc": "2.0", "id": "sub-1",
                "result": {"task": serde_json::to_value(&opening).unwrap()}})
        );
        assert!(events.iter().all(|(_, event)| event["id"] == "sub-1"));
    }

    /// A subscription that lost its task's ending in a lag writes it from the registry, unnumbered,
    /// within one heartbeat, and ends.
    #[tokio::test]
    async fn subscribe_that_lagged_ends_with_the_terminal_status_from_the_registry() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(1);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let registry = running_task_registry();
        let (_closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_subscription(
            "tsk_run",
            Arc::clone(&registry),
            TaskCaller::Session,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some("TASK_STATE_WORKING")).await;
        {
            let mut reg = registry.lock().unwrap();
            reg.finish_task(TaskState::Completed);
            reg.record_ending("tsk_run", "task completed", Some("four"), false, &[], &[]);
        }
        let lost_at = std::time::Instant::now();
        for id in 1..=3 {
            sse_tx
                .send(stream_event(id, "tsk_other", false, None))
                .unwrap();
        }
        handler.await.unwrap();
        assert!(
            lost_at.elapsed() <= SSE_HEARTBEAT_INTERVAL + std::time::Duration::from_secs(2),
            "the ending was written within one heartbeat of the loss"
        );
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();

        let events = events_of(sse_body(&received));
        assert_eq!(events.len(), 3, "{received}");
        assert!(events.iter().all(|(id, _)| id.is_none()), "{received}");
        let reported = registry.lock().unwrap().get_task("tsk_run").unwrap();
        assert_eq!(
            events[1].1["result"]["artifactUpdate"]["artifact"],
            serde_json::to_value(reported.artifact("response").unwrap()).unwrap()
        );
        assert_eq!(
            events[2].1["result"]["statusUpdate"]["status"],
            serde_json::to_value(&reported.status).unwrap()
        );
    }

    /// A subscription that lagged while its task waits for input stays open: a subscriber watches
    /// the task, and the reply arrives on a request of its own.
    #[tokio::test]
    async fn subscribe_that_lagged_on_an_input_required_task_stays_open() {
        let (sse_tx, sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(1);
        drop(sse_rx);
        let sse_buffer = Arc::new(Mutex::new(SseEventBuffer::new(8)));
        let registry = running_task_registry();
        let (_waiter_tx, waiter_rx) = oneshot::channel();
        registry
            .lock()
            .unwrap()
            .set_input_required("tsk_run", "which branch?".to_string(), _waiter_tx)
            .unwrap();
        let (closing_tx, closing) = watch::channel(false);
        let (handler, lines, client) = serve_subscription(
            "tsk_run",
            Arc::clone(&registry),
            TaskCaller::Session,
            &sse_tx,
            &sse_buffer,
            closing,
        )
        .await;
        let mut received = String::new();
        collect_sse_lines(&lines, &mut received, Some("TASK_STATE_INPUT_REQUIRED")).await;
        for id in 1..=3 {
            sse_tx
                .send(stream_event(id, "tsk_other", false, None))
                .unwrap();
        }
        tokio::time::sleep(SSE_HEARTBEAT_INTERVAL + std::time::Duration::from_secs(2)).await;
        assert!(
            !handler.is_finished(),
            "a lagged subscription ended on input-required"
        );
        closing_tx.send(true).unwrap();
        handler.await.unwrap();
        collect_sse_lines(&lines, &mut received, None).await;
        client.join().unwrap();
        drop(waiter_rx);

        let events = events_of(sse_body(&received));
        assert_eq!(events.len(), 1, "only the opening task: {received}");
        assert!(received.contains(":heartbeat"), "{received}");
    }

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

    /// The Events-a-connection-misses section states `SSE_BROADCAST_CAPACITY` as the size of each
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
                "the Events a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} \
                 does not say `{phrase}`; SSE_BROADCAST_CAPACITY is {capacity}"
            );
        }
    }

    /// The page documents the frame `watch_session_writes_lagged_frame` and
    /// `message_stream_writes_lagged_frame` hold the handlers to: its key, its place in the
    /// endpoint and event-id tables, and its absence from replay.
    #[test]
    fn protocol_page_documents_the_lagged_frame() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "event-lagged");
        for required in ["`missed`", "\"frame\":\"lagged\",\"missed\":"] {
            assert!(
                section.contains(required),
                "the `lagged` section ({{ #event-lagged }}) of {PROTOCOL_PAGE_PATH} does not \
                 contain `{required}`"
            );
        }
        let row = "| [`lagged`](#event-lagged) | yes | yes |";
        assert!(
            page.contains(row),
            "{PROTOCOL_PAGE_PATH} has no `{row}` row"
        );
        assert!(
            protocol_page_section(&page, "event-ids")
                .lines()
                .any(|line| line.contains("`lagged`") && line.ends_with("| no |")),
            "the Event ids section of {PROTOCOL_PAGE_PATH} does not list `lagged` as unnumbered"
        );
        assert!(
            protocol_page_section(&page, "replay").contains("`lagged`"),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not list `lagged`"
        );
        assert!(
            !protocol_page_section(&page, "lagged").contains("Nothing is written in their place"),
            "the Events a connection misses section ({{ #lagged }}) of {PROTOCOL_PAGE_PATH} says \
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
        crate::streaming::emit_status(
            &Some((tx, Arc::clone(&buffer))),
            &StreamTask::new("tsk_1", "ctx_1"),
            TaskState::Working,
            "inference turn 1",
            None,
        );
        let first = match buffer.lock().unwrap().replay_from(0) {
            ReplayResult::Complete(events) => events[0].id,
            ReplayResult::WithGap { .. } => panic!("one event fits"),
        };

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

    /// The Replay section states `SSE_REPLAY_CAPACITY` as the number of events a session keeps.
    #[test]
    fn protocol_page_replay_capacity_is_the_runtime_capacity() {
        let page = protocol_page();
        let section = protocol_page_section(&page, "replay");
        let capacity = crate::runtime::SSE_REPLAY_CAPACITY;
        let phrase = format!("most recent {capacity} events");
        assert!(
            section.contains(&phrase),
            "the Replay section ({{ #replay }}) of {PROTOCOL_PAGE_PATH} does not say `{phrase}`; \
             SSE_REPLAY_CAPACITY is {capacity}"
        );
    }

    /// The Heartbeat section's liveness claim is held against
    /// `watch_session_heartbeat_continues_while_session_thread_is_blocked`: the heartbeat keeps its
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
            child_workdir: "/home/test/.murmur/records/work/ses_parent/children/worker-1"
                .to_string(),
            child_records_dir: std::path::PathBuf::from(
                "/home/test/.murmur/records/work/ses_child",
            ),
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

    /// Posts a `SendMessage` with `params` to the door at `addr` from a blocking client, which
    /// returns the full answer and the instant it finished reading it.
    fn post_send_message(
        addr: &str,
        params: Value,
    ) -> std::thread::JoinHandle<(std::time::Instant, String)> {
        use std::io::{Read, Write};

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "SendMessage", "params": params,
        })
        .to_string();
        let addr = addr.to_string();
        std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(&addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            write!(
                stream,
                "POST / HTTP/1.1\r\nHost: {addr}\r\ncontent-type: application/json\r\n\
                 A2A-Version: 1.0\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            (std::time::Instant::now(), answer)
        })
    }

    /// A door over a fresh `queue` registry, holding a bare `SendMessage` whose task it has handed
    /// to the loop. Returns the registry, the task, the door's shutdown sender, its task channel
    /// and the client.
    async fn held_send_message_door() -> (
        Arc<Mutex<TaskRegistry>>,
        IncomingTask,
        oneshot::Sender<()>,
        mpsc::Receiver<IncomingTask>,
        std::thread::JoinHandle<(std::time::Instant, String)>,
    ) {
        let registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (addr, shutdown, mut task_rx) = serve_test_door_with(
            TaskAcceptance::Queue,
            Arc::clone(&registry),
            Arc::new(LiveDelegations::new()),
            false,
        )
        .await;
        let client = post_send_message(&addr, message_send_params("msg_held"));
        let incoming = tokio::time::timeout(std::time::Duration::from_secs(10), task_rx.recv())
            .await
            .expect("the task reached the loop")
            .unwrap();
        (registry, incoming, shutdown, task_rx, client)
    }

    /// Whether the client has read an answer within `wait`.
    async fn answered_within(
        client: &std::thread::JoinHandle<(std::time::Instant, String)>,
        wait: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + wait;
        while std::time::Instant::now() < deadline {
            if client.is_finished() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        client.is_finished()
    }

    /// A bare `SendMessage` whose task never settles is held, and answered with the task as the
    /// registry holds it, `TASK_STATE_SUBMITTED`, once the door closes, within the drain grace.
    #[tokio::test(flavor = "multi_thread")]
    async fn held_send_message_answers_the_registry_s_task_when_the_door_closes() {
        let (registry, incoming, shutdown, _task_rx, client) = held_send_message_door().await;
        assert!(
            !answered_within(&client, std::time::Duration::from_millis(300)).await,
            "a task that has not settled is not answered"
        );

        let closed_at = std::time::Instant::now();
        drop(shutdown);
        let (answered_at, answer) = client.join().unwrap();
        assert!(
            answered_at.duration_since(closed_at) < CONNECTION_DRAIN_GRACE,
            "answered {:?} after the door closed",
            answered_at.duration_since(closed_at)
        );
        let task = &response_json(&answer)["result"]["task"];
        assert_eq!(task["status"]["state"], "TASK_STATE_SUBMITTED", "{answer}");
        let reported = registry
            .lock()
            .unwrap()
            .get_task(&incoming.task_id)
            .unwrap();
        assert_eq!(*task, serde_json::to_value(&reported).unwrap());
    }

    /// A held `SendMessage` is not answered while its task is terminal with no ending recorded,
    /// and is answered with the task as `GetTask` reports it, `response` artifact and all, once
    /// the ending is.
    #[tokio::test(flavor = "multi_thread")]
    async fn held_send_message_answers_once_the_ending_is_recorded() {
        let (registry, incoming, _shutdown, _task_rx, client) = held_send_message_door().await;
        let task_id = incoming.task_id.clone();
        {
            let mut reg = registry.lock().unwrap();
            reg.start_task(
                task_id.clone(),
                incoming.context_id.clone(),
                crate::lanes::TaskLane::User,
            );
            reg.finish_task(TaskState::Completed);
        }
        assert!(
            !answered_within(&client, HELD_SEND_RECHECK_INTERVAL * 2).await,
            "a terminal task with no ending is not answered"
        );

        registry
            .lock()
            .unwrap()
            .record_ending(&task_id, "done", Some("four"), false, &[], &[]);
        let (_, answer) = client.join().unwrap();
        let task = &response_json(&answer)["result"]["task"];
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{answer}");
        let reported = registry.lock().unwrap().get_task(&task_id).unwrap();
        assert!(reported.artifact(crate::a2a::RESPONSE_ARTIFACT).is_some());
        assert_eq!(*task, serde_json::to_value(&reported).unwrap());
    }

    /// A held `SendMessage` whose task pauses for input is answered `TASK_STATE_INPUT_REQUIRED`,
    /// carrying the prompt as a `prompt` artifact.
    #[tokio::test(flavor = "multi_thread")]
    async fn held_send_message_answers_a_task_waiting_for_input() {
        let (registry, incoming, _shutdown, _task_rx, client) = held_send_message_door().await;
        let (waiter, _input) = oneshot::channel();
        {
            let mut reg = registry.lock().unwrap();
            reg.start_task(
                incoming.task_id.clone(),
                incoming.context_id.clone(),
                crate::lanes::TaskLane::User,
            );
            reg.set_input_required(&incoming.task_id, "which branch?".to_string(), waiter)
                .unwrap();
        }
        let (_, answer) = client.join().unwrap();
        let task = &response_json(&answer)["result"]["task"];
        assert_eq!(
            task["status"]["state"], "TASK_STATE_INPUT_REQUIRED",
            "{answer}"
        );
        let reported = registry
            .lock()
            .unwrap()
            .get_task(&incoming.task_id)
            .unwrap();
        assert_eq!(
            reported
                .artifact(crate::a2a::PROMPT_ARTIFACT)
                .and_then(crate::a2a::A2aArtifact::first_text),
            Some("which branch?")
        );
        assert_eq!(*task, serde_json::to_value(&reported).unwrap());
    }

    /// A held `SendMessage` wakes on its own task's event rather than its next re-check, and not
    /// on another task's.
    #[tokio::test(flavor = "multi_thread")]
    async fn held_send_message_answers_on_its_task_s_event() {
        let (sse_tx, _sse_rx) = tokio::sync::broadcast::channel::<Arc<StreamEvent>>(16);
        let registry = Arc::new(Mutex::new(TaskRegistry::new(4, TaskAcceptance::Queue)));
        let (task_tx, mut task_rx) = mpsc::channel::<IncomingTask>(4);
        let (_closing_tx, closing) = watch::channel(false);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = post_send_message(
            &listener.local_addr().unwrap().to_string(),
            message_send_params("msg_held"),
        );
        let (sock, _peer) = listener.accept().await.unwrap();
        let (read_half, write_half) = sock.into_split();
        let handler = {
            let (registry, sse) = (Arc::clone(&registry), sse_tx.clone());
            tokio::spawn(async move {
                let _read_half = read_half;
                handle_message_send(
                    write_half,
                    JsonRpcRequest {
                        id: serde_json::json!(7),
                        method: "SendMessage".to_string(),
                        params: message_send_params("msg_held"),
                    },
                    &registry,
                    &task_tx,
                    operator(),
                    false,
                    sse,
                    closing,
                )
                .await;
            })
        };
        let incoming = task_rx.recv().await.unwrap();
        let task_id = incoming.task_id.clone();
        // Past the check made right after routing, so only an event or a re-check can answer.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        {
            let mut reg = registry.lock().unwrap();
            reg.start_task(
                task_id.clone(),
                incoming.context_id.clone(),
                crate::lanes::TaskLane::User,
            );
            reg.finish_task(TaskState::Failed);
            reg.record_ending(&task_id, "failed", None, false, &[], &[]);
        }
        sse_tx
            .send(stream_event(
                1,
                "tsk_other",
                false,
                Some(StreamEnd::Terminal),
            ))
            .unwrap();
        assert!(
            !answered_within(&client, std::time::Duration::from_millis(200)).await,
            "another task's event answered the held request"
        );
        let ended_at = std::time::Instant::now();
        sse_tx
            .send(stream_event(1, &task_id, false, Some(StreamEnd::Terminal)))
            .unwrap();
        handler.await.unwrap();
        let (answered_at, answer) = client.join().unwrap();
        assert!(
            answered_at.duration_since(ended_at) < HELD_SEND_RECHECK_INTERVAL / 2,
            "answered {:?} after the event",
            answered_at.duration_since(ended_at)
        );
        assert_eq!(
            response_json(&answer)["result"]["task"]["status"]["state"],
            "TASK_STATE_FAILED",
            "{answer}"
        );
    }
}
