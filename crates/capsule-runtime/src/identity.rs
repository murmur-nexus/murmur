use std::sync::{Arc, Mutex};

use murmur_artifact::{ConversationMode, NetworkAuthentication, TaskAcceptance};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

use crate::a2a::{
    A2aMessage, A2aTask, CancelOutcome, IncomingTask, JsonRpcRequest, JsonRpcResponse,
    TaskRegistry, TaskState, TaskStatus,
};
use crate::cancel::{LiveDelegations, Residue};
use crate::control_plane::{handle_control_request, is_control_path, ControlPlane, ControlRequest};
use crate::delegation::{COMPLETION_SESSION_HEADER, DELEGATION_ID_HEADER};
use crate::detached::DetachedRegistry;
use crate::errors::RuntimeError;
use crate::origin::{self, TaskOrigin, TaskProvenance, PEER_ORIGIN_HEADER, PEER_TRUST_HEADER};
use crate::peer_handoff::{handle_peer_request, is_peer_path, PeerPlane, AUDIENCE_HEADER};
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
    MessageSend,
    MessageStream,
    StreamWatch,
    TasksGet,
    TasksCancel,
    SessionStop,
    GetAuthenticatedExtendedCard,
}

impl DoorMethod {
    /// Every method, in the order the card lists them.
    pub(crate) const ALL: [DoorMethod; 7] = [
        DoorMethod::MessageSend,
        DoorMethod::MessageStream,
        DoorMethod::StreamWatch,
        DoorMethod::TasksGet,
        DoorMethod::TasksCancel,
        DoorMethod::SessionStop,
        DoorMethod::GetAuthenticatedExtendedCard,
    ];

    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            DoorMethod::MessageSend => "message/send",
            DoorMethod::MessageStream => "message/stream",
            DoorMethod::StreamWatch => "stream/watch",
            DoorMethod::TasksGet => "tasks/get",
            DoorMethod::TasksCancel => "tasks/cancel",
            DoorMethod::SessionStop => "session/stop",
            DoorMethod::GetAuthenticatedExtendedCard => "agent/getAuthenticatedExtendedCard",
        }
    }

    /// The door-token scope a caller must hold to call this method on an authenticated door: its
    /// wire name, one of [`murmur_artifact::DOOR_SCOPES`]. `None` for
    /// `agent/getAuthenticatedExtendedCard`, which every authenticated caller may call.
    pub(crate) fn scope(self) -> Option<&'static str> {
        match self {
            DoorMethod::GetAuthenticatedExtendedCard => None,
            method => Some(method.wire_name()),
        }
    }

    /// The method this door answers for a request's `method` string, or `None` when it answers
    /// `-32601`.
    ///
    /// The only place a request's method is interpreted and the only place
    /// `lifecycle.task_acceptance` gates one: under `TaskAcceptance::None` neither task-starting
    /// method is served. `agent/getAuthenticatedExtendedCard` is served only by an `authenticated`
    /// door, the only kind with an extended card. The match is exact — no case folding, no
    /// trimming — so a name the card lists is the name to send.
    pub(crate) fn resolve(
        method: &str,
        acceptance: &TaskAcceptance,
        authenticated: bool,
    ) -> Option<DoorMethod> {
        let resolved = Self::ALL.into_iter().find(|m| m.wire_name() == method)?;
        match (resolved, acceptance) {
            (DoorMethod::MessageSend | DoorMethod::MessageStream, TaskAcceptance::None) => None,
            (DoorMethod::GetAuthenticatedExtendedCard, _) if !authenticated => None,
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
/// extension lists. Every transport can be stopped, so cancellation is not here: `tasks/cancel`
/// is served under every acceptance.
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
/// `message/stream` and `stream/watch` write frames; `stream/watch` alone writes the observer's
/// `connection-ack` and `capsule-closed`, and `message/stream` alone answers a request it cannot
/// take with `error`.
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
            | StreamFrame::Gap
            | StreamFrame::Lagged => (true, true, &TransportKind::ALL),
            StreamFrame::ConnectionAck | StreamFrame::CapsuleClosed => {
                (false, true, &TransportKind::ALL)
            }
            StreamFrame::Error => (true, false, &TransportKind::ALL),
        };
    let carried = match method {
        DoorMethod::MessageStream => on_message_stream,
        DoorMethod::StreamWatch => on_stream_watch,
        DoorMethod::MessageSend
        | DoorMethod::TasksGet
        | DoorMethod::TasksCancel
        | DoorMethod::SessionStop
        | DoorMethod::GetAuthenticatedExtendedCard => false,
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

/// A2A 0.3's `AuthenticatedExtendedCardNotConfiguredError`: what a door with no extended card
/// answers `agent/getAuthenticatedExtendedCard` with.
pub(crate) const EXTENDED_CARD_NOT_CONFIGURED: i32 = -32007;

/// What a door declaring `network.authentication` holds: the session's key and tokens, the realm
/// its challenges name, and the extended card an authenticated caller may read.
pub(crate) struct DoorGate {
    pub auth: Arc<crate::door_auth::DoorAuth>,
    /// The capsule name.
    pub realm: String,
    /// The A2A 0.3 extended card, from [`build_agent_cards`].
    pub extended_card: Value,
}

/// URI of the agent-card extension that lists every JSON-RPC method the door answers, murmur's
/// own methods among them. It is the address of that extension's section in the reference docs.
pub(crate) const DOOR_EXTENSION_URI: &str =
    "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1";

/// URI of the agent-card extension that carries the extended-card material: the session id and
/// what the capsule may do. It is the address of that extension's section in the reference docs.
pub(crate) const CAPSULE_EXTENSION_URI: &str =
    "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1";

/// URI of the agent-card extension that lists every SSE event type the capsule's stream can write.
/// It is the address of that extension's section in the reference docs, on the streaming protocol
/// page beside the frames it names.
pub(crate) const STREAM_EXTENSION_URI: &str =
    "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1";

/// What the stream extension says about itself.
const STREAM_EXTENSION_DESCRIPTION: &str = "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.";

/// The id of the one skill a door that starts tasks advertises: running a task.
pub(crate) const TASK_SKILL_ID: &str = "task";

/// The A2A protocol version the card's JSON-RPC interface declares.
///
/// The card's shape is A2A v1.0, but the door answers the 0.3 method names (`message/send`,
/// `tasks/get`, …) and 0.3 task states, which v1.0 renamed. An `AgentInterface` carries its own
/// `protocolVersion` so that a v1.0 card can declare an interface at another version.
pub(crate) const INTERFACE_PROTOCOL_VERSION: &str = "0.3";

/// The `protocolBinding` of the door's interface.
const JSONRPC_BINDING: &str = "JSONRPC";

/// The media type of every part the door reads and writes.
const TEXT_MODE: &str = "text/plain";

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
///   door is given, so the card cannot list a method the dispatcher refuses or omit one it serves.
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
/// when `message/stream` is served and the transport streams text. A door that answers a method
/// whose effect its transport cannot deliver still lists the method and says so here.
///
/// `skills` is the one [`TASK_SKILL_ID`] skill when the door serves `message/send`, and empty
/// otherwise. Installed tools are not skills: a caller cannot invoke one directly.
pub(crate) fn build_agent_card(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
    task_acceptance: &TaskAcceptance,
    planes: DeclaredPlanes,
    transport: TransportCapabilities,
) -> serde_json::Value {
    let tools: Vec<&str> = installed_artifacts
        .iter()
        .filter(|a| a.runtime.is_llm_visible())
        .map(|a| a.name.as_str())
        .collect();

    let methods = served_methods(task_acceptance, false);
    let streaming =
        methods.contains(&DoorMethod::MessageStream.wire_name()) && transport.streams_text;
    let declared_planes: Vec<&str> = [(planes.files, "files"), (planes.peer_files, "peer_files")]
        .into_iter()
        .filter_map(|(declared, name)| declared.then_some(name))
        .collect();
    let skills: Vec<Value> = if methods.contains(&DoorMethod::MessageSend.wire_name()) {
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
                    "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods.",
                    "required": false,
                    "params": { "methods": methods },
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
        "defaultInputModes": [TEXT_MODE],
        "defaultOutputModes": [TEXT_MODE],
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

/// The A2A protocol version the extended card declares. `agent/getAuthenticatedExtendedCard`
/// answers on the door's 0.3 interface, so its result is an A2A 0.3 `AgentCard`.
pub(crate) const EXTENDED_CARD_PROTOCOL_VERSION: &str = "0.3.0";

/// What the bearer scheme says about the token it takes.
const BEARER_SCHEME_DESCRIPTION: &str =
    "A token this capsule's runtime mints at launch and accepts until the session ends.";

/// The cards a door serves: the public card at `/.well-known/agent-card.json`, and the extended
/// card `agent/getAuthenticatedExtendedCard` returns when the door authenticates.
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
/// requirement any valid token meets, `agent/getAuthenticatedExtendedCard` on the door
/// extension's methods, and one alternative requirement per served task-starting method on the
/// [`TASK_SKILL_ID`] skill. The public card is that card without the capsule extension, so it
/// carries the door and stream extensions; the extended card is that card, whole, in 0.3 shape
/// through [`v03_agent_card`].
pub(crate) fn build_agent_cards(
    identity: &CapsuleIdentity,
    installed_artifacts: &[InstalledArtifactSummary],
    capability_policy: &CapabilityPolicy,
    task_acceptance: &TaskAcceptance,
    planes: DeclaredPlanes,
    transport: TransportCapabilities,
    authentication: Option<&NetworkAuthentication>,
) -> AgentCards {
    let card = build_agent_card(
        identity,
        installed_artifacts,
        capability_policy,
        task_acceptance,
        planes,
        transport,
    );
    if authentication.is_none() {
        return AgentCards {
            public: card,
            extended: None,
        };
    }

    let mut card = authenticated_card(card, task_acceptance);
    let extended = v03_agent_card(&card);
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
    let task_requirements: Vec<Value> = [DoorMethod::MessageSend, DoorMethod::MessageStream]
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

/// The A2A 0.3 `AgentCard` a v1.0 card describes, for the door's 0.3 interface.
///
/// Mechanical: `url` is the card's [`jsonrpc_interface_url`], `preferredTransport` is `JSONRPC`,
/// `capabilities` loses `extendedAgentCard`, which 0.3 carries as
/// `supportsAuthenticatedExtendedCard`, each `httpAuthSecurityScheme` becomes a
/// `{"type": "http", …}` scheme, and each list of `securityRequirements`, the card's and every
/// skill's, becomes a 0.3 `security` list of `{scheme: scopes}` objects. Every other field is
/// copied. The capsule extension keeps its place, so [`session_id_from_card`] reads either shape.
pub(crate) fn v03_agent_card(card: &Value) -> Value {
    let mut capabilities = card["capabilities"].clone();
    let supports_extended = capabilities
        .as_object_mut()
        .and_then(|capabilities| capabilities.remove("extendedAgentCard"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let security_schemes: serde_json::Map<String, Value> = card["securitySchemes"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, scheme)| {
            let http = scheme.get("httpAuthSecurityScheme")?;
            let mut v03 = serde_json::Map::new();
            v03.insert("type".to_string(), Value::from("http"));
            for field in ["scheme", "description", "bearerFormat"] {
                if let Some(value) = http.get(field) {
                    v03.insert(field.to_string(), value.clone());
                }
            }
            Some((name.clone(), Value::Object(v03)))
        })
        .collect();

    let skills: Vec<Value> = card["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|skill| {
            let mut skill = skill.clone();
            if let Some(object) = skill.as_object_mut() {
                if let Some(requirements) = object.remove("securityRequirements") {
                    object.insert("security".to_string(), v03_security(&requirements));
                }
            }
            skill
        })
        .collect();

    serde_json::json!({
        "protocolVersion": EXTENDED_CARD_PROTOCOL_VERSION,
        "name": card["name"],
        "description": card["description"],
        "url": jsonrpc_interface_url(card).unwrap_or_default(),
        "preferredTransport": JSONRPC_BINDING,
        "version": card["version"],
        "capabilities": capabilities,
        "securitySchemes": security_schemes,
        "security": v03_security(&card["securityRequirements"]),
        "defaultInputModes": card["defaultInputModes"],
        "defaultOutputModes": card["defaultOutputModes"],
        "skills": skills,
        "supportsAuthenticatedExtendedCard": supports_extended,
    })
}

/// v1.0 `[{"schemes": {name: {"list": [..]}}}]` as 0.3 `[{name: [..]}]`.
fn v03_security(requirements: &Value) -> Value {
    requirements
        .as_array()
        .into_iter()
        .flatten()
        .map(|requirement| {
            let flattened: serde_json::Map<String, Value> = requirement["schemes"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, list)| (name.clone(), list["list"].clone()))
                .collect();
            Value::Object(flattened)
        })
        .collect()
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
    // that demotes nothing, which contributes no shell items rather than an empty set.
    detached: Option<Arc<DetachedRegistry>>,
    live_delegations: Arc<LiveDelegations>,
    // Whether this capsule has a harness session to forget, which is the one thing
    // `FORGET_SESSION_HEADER` needs to know about the transport behind the door.
    forgettable_session: bool,
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
                            handle_connection(stream, peer_addr, card, registry, tx, acceptance, sse, buf, mode_str, plane, peer, control, session, detached_for_conn, live, forgettable_session, gate_for_conn, closing).await;
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
    // A `stream/watch` or `message/stream` connection otherwise outlives the session: its
    // handler holds a broadcast sender, so it never sees the channel close.
    let _ = closing_tx.send(true);
    let drained = async { while connections.join_next().await.is_some() {} };
    let _ = tokio::time::timeout(CONNECTION_DRAIN_GRACE, drained).await;
    connections.shutdown().await;
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

    // Both delegation headers mean something only on the completion path. On every other path
    // they are ignored rather than carried: a `peer` message claiming a delegation id would put a
    // value on the receiver's `task_start` that no delegation of its own produced.
    let is_completion = provenance.origin() == TaskOrigin::Completion;
    let delegation_id = if is_completion { delegation_id } else { None };

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

        // A completion is addressed to one session, and this door answers for one session. An
        // address that has outlived the session that made the delegation — a parent that
        // restarted onto the same port — is refused here rather than delivered to whoever
        // answers now. The refusal names the addressed session and not this one: a caller that
        // guessed wrong learns nothing about who is actually here.
        if is_completion && completion_session.as_deref() != Some(session_id.as_str()) {
            let id = serde_json::from_str::<JsonRpcRequest>(&body_str)
                .map(|req| req.id)
                .unwrap_or(Value::Null);
            let addressed = completion_session.as_deref().unwrap_or("<unaddressed>");
            let response = JsonRpcResponse::err(
                id,
                -32004,
                &format!(
                    "completion is addressed to session {addressed}, which is not the session \
                     running here"
                ),
            )
            .into_http_response();
            let _ = writer_half.write_all(response.as_bytes()).await;
            return;
        }

        let req = match serde_json::from_str::<JsonRpcRequest>(&body_str) {
            Ok(req) => req,
            Err(_) => {
                let response =
                    JsonRpcResponse::err(Value::Null, -32700, "Parse error").into_http_response();
                let _ = writer_half.write_all(response.as_bytes()).await;
                return;
            }
        };

        let resolved = DoorMethod::resolve(&req.method, &task_acceptance, gate.is_some());
        // Scope is checked once the method is known to be served and before its handler runs: an
        // unserved method is `-32601` to anyone the door let in, since the card lists what it
        // serves.
        if let Some(scope) = resolved.and_then(DoorMethod::scope) {
            if let Some(refused) = refuse_scope(scope) {
                let _ = writer_half.write_all(&refused).await;
                return;
            }
        }

        // A forget writes to this capsule's harness session map, and only a `transport: process`
        // capsule has one. Refused here, on the request that carried it, rather than dropped into
        // a task that would ignore it: a caller that asked for a conversation to be dropped and
        // was answered `completed` would read that as the drop having happened.
        let starts_a_turn = req.method == DoorMethod::MessageSend.wire_name()
            || req.method == DoorMethod::MessageStream.wire_name();
        if forget_session && starts_a_turn && !forgettable_session {
            let response = JsonRpcResponse::err(
                req.id,
                -32602,
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

        // The streaming methods own the connection; every other method answers one JSON body.
        let response = match resolved {
            Some(DoorMethod::MessageStream) => {
                handle_message_stream(
                    writer_half,
                    req,
                    &task_registry,
                    &task_tx,
                    traceparent,
                    provenance,
                    delegation_id,
                    forget_session,
                    last_event_id,
                    sse_tx,
                    sse_buffer,
                    closing,
                )
                .await;
                return;
            }
            Some(DoorMethod::StreamWatch) => {
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
            Some(DoorMethod::GetAuthenticatedExtendedCard) => match gate.as_deref() {
                Some(gate) => JsonRpcResponse::ok(req.id, &gate.extended_card).into_http_response(),
                None => {
                    unreachable!("resolve serves the extended card only on an authenticated door")
                }
            },
            Some(door_method) => handle_jsonrpc(
                door_method,
                req,
                &task_registry,
                &task_tx,
                traceparent,
                provenance,
                delegation_id,
                forget_session,
                detached.as_ref(),
                &live_delegations,
                &session_id,
            ),
            // A2A 0.3's answer from an agent with no extended card. The card does not list the
            // method, since the door does not serve it.
            None if req.method == DoorMethod::GetAuthenticatedExtendedCard.wire_name() => {
                JsonRpcResponse::err(
                    req.id,
                    EXTENDED_CARD_NOT_CONFIGURED,
                    "Authenticated Extended Card is not configured",
                )
                .into_http_response()
            }
            None => JsonRpcResponse::err(req.id, -32601, "Method not found").into_http_response(),
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

#[allow(clippy::too_many_arguments)]
async fn handle_message_stream(
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
    forget_session: bool,
    last_event_id: Option<u64>,
    sse_tx: SseBroadcast,
    sse_buffer: Arc<Mutex<SseEventBuffer>>,
    mut closing: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    // Subscribe to broadcast BEFORE writing headers so we don't miss events
    // emitted between enqueue and the start of our receive loop.
    let mut rx = sse_tx.subscribe();

    // Write SSE response headers immediately
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

    // Parse the A2A message from params
    let msg_value = req.params.get("message").unwrap_or(&req.params);
    let message: A2aMessage = match serde_json::from_value(msg_value.clone()) {
        Ok(m) => m,
        Err(e) => {
            let error_data = format!("{{\"error\":\"Invalid params: {e}\"}}");
            let event_text = format_unnumbered_sse_event(StreamFrame::Error, &error_data);
            let _ = writer.write_all(event_text.as_bytes()).await;
            return;
        }
    };

    let text = message.extract_text();
    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("ctx_{}", uuid::Uuid::now_v7().simple()));

    // Capacity check and enqueue — release lock before any await. `refusal` is the refused
    // frame's message, read under the lock that decided the refusal.
    let refusal = {
        let mut reg = task_registry.lock().unwrap();
        if reg.can_accept() {
            reg.enqueue(&task_id, &context_id);
            None
        } else if reg.is_closed() {
            Some(crate::a2a::REJECTED_SESSION_CLOSING_MESSAGE)
        } else {
            Some(REJECTED_BUSY_MESSAGE)
        }
    };
    if let Some(refusal) = refusal {
        let rejected_event = TaskStatusUpdateEvent {
            id: task_id.clone(),
            context_id: Some(context_id.clone()),
            status: StreamStatus {
                state: "rejected".into(),
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

    // Send to agent loop via mpsc
    let incoming = IncomingTask {
        task_id: task_id.clone(),
        context_id,
        message_id: message.message_id.clone(),
        message_text: text,
        traceparent,
        provenance,
        source: crate::a2a::SOURCE_A2A,
        delegation_id,
        forget_session,
    };
    if task_tx.try_send(incoming).is_err() {
        {
            let mut reg = task_registry.lock().unwrap();
            reg.pending_count -= 1;
            reg.history.remove(&task_id);
        } // lock dropped before await
        let event_text = format_unnumbered_sse_event(
            StreamFrame::Error,
            "{\"error\":\"internal error: queue send failed\"}",
        );
        let _ = writer.write_all(event_text.as_bytes()).await;
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

/// The `status.message` of a `message/stream` refusal from a capsule with no room for the task.
const REJECTED_BUSY_MESSAGE: &str = "task rejected: capsule is busy";

/// The `rejected` status written to a `message/stream` connection the door refuses, busy or
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

/// Answers a resolved method that replies with one JSON-RPC body.
#[allow(clippy::too_many_arguments)]
fn handle_jsonrpc(
    method: DoorMethod,
    req: JsonRpcRequest,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
    forget_session: bool,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
    session_id: &str,
) -> String {
    let id = req.id;
    match method {
        DoorMethod::MessageSend => handle_message_send(
            id,
            &req.params,
            task_registry,
            task_tx,
            traceparent,
            provenance,
            delegation_id,
            forget_session,
        ),
        DoorMethod::TasksGet => handle_tasks_get(id, &req.params, task_registry),
        DoorMethod::TasksCancel => {
            handle_tasks_cancel(id, &req.params, task_registry, detached, live_delegations)
        }
        DoorMethod::SessionStop => {
            handle_session_stop(id, task_registry, detached, live_delegations, session_id)
        }
        DoorMethod::MessageStream
        | DoorMethod::StreamWatch
        | DoorMethod::GetAuthenticatedExtendedCard => {
            unreachable!("handle_connection answers the streaming methods and the extended card")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_message_send(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    task_tx: &mpsc::Sender<IncomingTask>,
    traceparent: Option<String>,
    provenance: TaskProvenance,
    delegation_id: Option<String>,
    forget_session: bool,
) -> String {
    let msg_value = params.get("message").unwrap_or(params);
    let message: A2aMessage = match serde_json::from_value(msg_value.clone()) {
        Ok(m) => m,
        Err(e) => {
            return JsonRpcResponse::err(id, -32602, &format!("Invalid params: {e}"))
                .into_http_response();
        }
    };

    let text = message.extract_text();

    // Check if the active task is waiting for input — deliver to it instead of enqueuing.
    {
        let mut reg = task_registry.lock().unwrap();
        if let Some(active_task_id) = reg.active_input_required_task_id() {
            if reg.deliver_input(&active_task_id, text.clone()).is_ok() {
                if let Some(task) = reg.get_task(&active_task_id) {
                    return JsonRpcResponse::ok(id, task).into_http_response();
                }
            }
        }
    }

    let task_id = format!("tsk_{}", uuid::Uuid::now_v7().simple());
    let context_id = message
        .context_id
        .clone()
        .unwrap_or_else(|| format!("ctx_{}", uuid::Uuid::now_v7().simple()));

    // Capacity check and enqueue under lock
    {
        let mut reg = task_registry.lock().unwrap();
        if !reg.can_accept() {
            let task = A2aTask {
                id: task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Rejected,
                },
                artifacts: None,
            };
            return JsonRpcResponse::ok(id, task).into_http_response();
        }
        reg.enqueue(&task_id, &context_id);
    }

    // Send to mpsc (should always succeed — capacity was checked under the same lock)
    let incoming = IncomingTask {
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        message_id: message.message_id.clone(),
        message_text: text,
        traceparent,
        provenance,
        source: crate::a2a::SOURCE_A2A,
        delegation_id,
        forget_session,
    };
    if task_tx.try_send(incoming).is_err() {
        // Unexpected path — roll back pending count
        let mut reg = task_registry.lock().unwrap();
        reg.pending_count -= 1;
        reg.history.remove(&task_id);
        return JsonRpcResponse::err(id, -32603, "internal error: queue send failed")
            .into_http_response();
    }

    let task = A2aTask {
        id: task_id,
        context_id,
        status: TaskStatus {
            state: TaskState::Submitted,
        },
        artifacts: None,
    };
    JsonRpcResponse::ok(id, task).into_http_response()
}

fn handle_tasks_get(id: Value, params: &Value, task_registry: &Arc<Mutex<TaskRegistry>>) -> String {
    let requested_id = params.get("id").and_then(Value::as_str).map(str::to_string);

    let Some(task_id) = requested_id else {
        // Backward compat: if no id provided, return the active slot's task if any
        let reg = task_registry.lock().unwrap();
        return match &reg.active_slot {
            crate::a2a::TaskSlotState::Empty => {
                JsonRpcResponse::err(id, -32001, "Task not found").into_http_response()
            }
            _ => {
                // Use get_task on the active slot's task_id
                let active_id = match &reg.active_slot {
                    crate::a2a::TaskSlotState::Running { task_id, .. }
                    | crate::a2a::TaskSlotState::Done { task_id, .. } => task_id.clone(),
                    crate::a2a::TaskSlotState::Empty => unreachable!(),
                };
                match reg.get_task(&active_id) {
                    Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
                    None => JsonRpcResponse::err(id, -32001, "Task not found").into_http_response(),
                }
            }
        };
    };

    let reg = task_registry.lock().unwrap();
    match reg.get_task(&task_id) {
        Some(task) => JsonRpcResponse::ok(id, task).into_http_response(),
        None => JsonRpcResponse::err(id, -32001, "Task not found").into_http_response(),
    }
}

/// `tasks/cancel`: stop one task and report what is still running.
///
/// Runs on the connection task and never waits for the agent loop: it takes the registry lock,
/// records the cancellation, snapshots the two work registries and answers. Whether the loop has
/// noticed yet is not the caller's question — the recorded state is already `canceled`.
///
/// Every outcome but one is a JSON-RPC `result`. A task that had already ended is returned
/// unchanged, because "do no more work on this" is already true of it. Only an id this capsule
/// never held is an error, and it is the same `-32001` `tasks/get` answers with.
fn handle_tasks_cancel(
    id: Value,
    params: &Value,
    task_registry: &Arc<Mutex<TaskRegistry>>,
    detached: Option<&Arc<DetachedRegistry>>,
    live_delegations: &LiveDelegations,
) -> String {
    let Some(task_id) = params.get("id").and_then(Value::as_str).map(str::to_string) else {
        return JsonRpcResponse::err(id, -32602, "Invalid params: tasks/cancel requires an id")
            .into_http_response();
    };

    let (outcome, task) = {
        let mut reg = task_registry.lock().unwrap();
        let outcome = reg.request_cancel(&task_id);
        (outcome, reg.get_task(&task_id))
    };

    match (outcome, task) {
        (CancelOutcome::Unknown, _) | (_, None) => {
            JsonRpcResponse::err(id, -32001, "Task not found").into_http_response()
        }
        (CancelOutcome::AlreadyTerminal, Some(task)) => {
            JsonRpcResponse::ok(id, task).into_http_response()
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
/// own lifecycle and a delegated child is still going, and once the process is gone so is the
/// registry that knew about either — so the question is asked while the door is still up, and
/// ending the process is left to the signal that follows.
///
/// Nothing here shuts the door down. A method that killed its own process would be racing its
/// response out of it.
///
/// Three keys, all three always present. `canceled` lists only the tasks this call moved to
/// `canceled`, so a second stop answers `[]` rather than an error. `residue` is `[]` when nothing
/// is running — unlike `tasks/cancel`, which omits the key: a session stop has to be able to say
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
        if let Err(errors) = crate::a2a_card_conformance::check_agent_card(card) {
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
                        let card = build_agent_card(
                            &identity(),
                            tools,
                            &CapabilityPolicy::default(),
                            acceptance,
                            DeclaredPlanes { files, peer_files },
                            transport,
                        );
                        assert_conforms(&card);
                        checked += 1;
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
                    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" }
                ],
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extendedAgentCard": false,
                    "extensions": [
                        {
                            "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1",
                            "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods.",
                            "required": false,
                            "params": {
                                "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop"]
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
                            "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
                            "required": false,
                            "params": {
                                "frames": ["status", "artifact", "text", "thinking", "gap", "lagged", "connection-ack", "capsule-closed", "error"]
                            }
                        }
                    ]
                },
                "securitySchemes": {},
                "securityRequirements": [],
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
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
    fn a2a_card_interface_is_the_door_url_over_http_at_0_3() {
        let card = full_card(&TaskAcceptance::Single);
        assert_eq!(
            card["supportedInterfaces"],
            serde_json::json!([{
                "url": "http://localhost:41873",
                "protocolBinding": "JSONRPC",
                "protocolVersion": "0.3",
            }])
        );
        assert_eq!(INTERFACE_PROTOCOL_VERSION, "0.3");
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
            assert_eq!(card["defaultInputModes"], serde_json::json!(["text/plain"]));
            assert_eq!(
                card["defaultOutputModes"],
                serde_json::json!(["text/plain"])
            );
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

    fn bearer_authentication() -> NetworkAuthentication {
        NetworkAuthentication {
            scheme: murmur_artifact::AuthenticationScheme::Bearer,
            credentials: vec![murmur_artifact::DoorCredential {
                name: "watcher".to_string(),
                scopes: vec!["tasks/get".to_string(), "stream/watch".to_string()],
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
                                let planes = DeclaredPlanes { files, peer_files };
                                let cards = build_agent_cards(
                                    &identity(),
                                    tools,
                                    policy,
                                    acceptance,
                                    planes,
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
                                    transport,
                                );
                                match authenticated {
                                    None => {
                                        assert_eq!(cards.public, base);
                                        assert_eq!(cards.extended, None);
                                    }
                                    Some(_) => {
                                        let extended_v1 = authenticated_card(base, acceptance);
                                        assert_conforms(&extended_v1);
                                        assert_eq!(
                                            cards.extended,
                                            Some(v03_agent_card(&extended_v1))
                                        );
                                        let mut stripped = extended_v1.clone();
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
                    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" }
                ],
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extendedAgentCard": true,
                    "extensions": [
                        {
                            "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1",
                            "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods.",
                            "required": false,
                            "params": {
                                "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop", "agent/getAuthenticatedExtendedCard"]
                            }
                        },
                        {
                            "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
                            "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
                            "required": false,
                            "params": {
                                "frames": ["status", "artifact", "text", "thinking", "gap", "lagged", "connection-ack", "capsule-closed", "error"]
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
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "skills": [
                    {
                        "id": "task",
                        "name": "Run a task",
                        "description": "Runs one task given as a text message and reports its outcome.",
                        "tags": ["task"],
                        "securityRequirements": [
                            { "schemes": { "bearer": { "list": ["message/send"] } } },
                            { "schemes": { "bearer": { "list": ["message/stream"] } } }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn a2a_card_extended_card_is_the_documented_0_3_document() {
        let cards = full_cards(&TaskAcceptance::Single, Some(&bearer_authentication()));
        let extended = cards
            .extended
            .expect("an authenticated door has an extended card");
        println!("{extended:#}");
        assert_eq!(
            extended,
            serde_json::json!({
                "protocolVersion": "0.3.0",
                "name": "my-agent",
                "description": "Murmur capsule my-agent 0.1.0",
                "url": "http://localhost:41873",
                "preferredTransport": "JSONRPC",
                "version": "0.1.0",
                "capabilities": {
                    "streaming": true,
                    "pushNotifications": false,
                    "extensions": [
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1", "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods.", "required": false,
                          "params": { "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop", "agent/getAuthenticatedExtendedCard"] } },
                        { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1", "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.", "required": false,
                          "params": { "sessionId": "ses_019f01a940ce7761854e768ecbe3d399", "tools": ["bash"], "shell": true, "network": true, "planes": ["files"] } },
                        { "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1", "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.", "required": false,
                          "params": { "frames": ["status", "artifact", "text", "thinking", "gap", "lagged", "connection-ack", "capsule-closed", "error"] } }
                    ]
                },
                "securitySchemes": {
                    "bearer": { "type": "http", "scheme": "Bearer", "description": "A token this capsule's runtime mints at launch and accepts until the session ends." }
                },
                "security": [ { "bearer": [] } ],
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "skills": [
                    {
                        "id": "task",
                        "name": "Run a task",
                        "description": "Runs one task given as a text message and reports its outcome.",
                        "tags": ["task"],
                        "security": [ { "bearer": ["message/send"] }, { "bearer": ["message/stream"] } ]
                    }
                ],
                "supportsAuthenticatedExtendedCard": true
            })
        );
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
                "tasks/get",
                "tasks/cancel",
                "session/stop",
                "agent/getAuthenticatedExtendedCard"
            ]
        );
        let extended = cards.extended.unwrap();
        assert_eq!(extended["skills"], serde_json::json!([]));
        assert_eq!(extended["security"], serde_json::json!([{"bearer": []}]));
    }

    #[test]
    fn a2a_card_public_door_cards_are_build_agent_card() {
        for acceptance in &ACCEPTANCES {
            let cards = full_cards(acceptance, None);
            assert_eq!(cards.public, full_card(acceptance));
            assert_eq!(cards.extended, None);
            assert!(!door_methods(&cards.public).contains(&"agent/getAuthenticatedExtendedCard"));
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
                { "url": "http://localhost:1", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" },
                { "url": "http://localhost:2", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" },
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
                "supportedInterfaces": [{ "url": "", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" }]
            }),
            serde_json::json!({ "url": "localhost:41873" }),
        ] {
            assert_eq!(jsonrpc_interface_url(&card), None, "{card}");
        }
    }

    /// `streaming` is the served method AND the transport's answer, so a door that answers
    /// `message/stream` over a transport that streams nothing advertises the method and not the
    /// capability. `tasks/cancel` is served whatever the transport: every transport can be stopped.
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
            assert!(methods.contains(&"message/stream"), "{card}");
            assert!(methods.contains(&"tasks/cancel"), "{card}");
        }
    }

    /// A door that starts no task streams nothing, whatever its transport can do: neither
    /// task-starting method is served under `TaskAcceptance::None`. `tasks/cancel` is served
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
            assert!(door_methods(&card).contains(&"tasks/cancel"), "{card}");
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
                "role": "user",
                "parts": [{"text": "hello"}]
            }
        })
    }

    fn response_json(response: &str) -> Value {
        let body = &response[response.find("\r\n\r\n").expect("header terminator") + 4..];
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    /// A closed registry refuses `message/send` whatever room its queue has, and the refused
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
            None,
            TaskProvenance::derive(TaskOrigin::User, None),
            None,
            false,
        ));

        assert_eq!(
            response["result"]["status"]["state"], "rejected",
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

    /// A task refused when the session closed reads `rejected` over `tasks/get`.
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
        );

        assert!(response.contains(r#""state":"rejected""#), "{response}");
        let response = response_json(&response);
        assert_eq!(response["result"]["id"], "tsk_refused");
        assert_eq!(response["result"]["contextId"], "ctx_refused");
    }

    /// A `message/stream` the door refuses because the session is closing is told so, in the
    /// same unnumbered final frame a busy refusal uses; a busy refusal keeps its own message.
    #[tokio::test]
    async fn message_stream_refused_by_a_closed_registry_says_the_session_is_closing() {
        for (close, expected) in [
            (true, crate::a2a::REJECTED_SESSION_CLOSING_MESSAGE),
            (false, REJECTED_BUSY_MESSAGE),
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
                jsonrpc: "2.0".to_string(),
                id: serde_json::json!(1),
                method: "message/stream".to_string(),
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
                None,
                TaskProvenance::derive(TaskOrigin::User, None),
                None,
                false,
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
            ["stream/watch", "tasks/get", "tasks/cancel", "session/stop"]
        );
        for acceptance in [TaskAcceptance::Single, TaskAcceptance::Queue] {
            assert_eq!(
                served_methods(&acceptance, false),
                [
                    "message/send",
                    "message/stream",
                    "stream/watch",
                    "tasks/get",
                    "tasks/cancel",
                    "session/stop"
                ]
            );
        }
    }

    #[test]
    fn door_method_only_an_authenticated_door_serves_the_extended_card() {
        for acceptance in &ACCEPTANCES {
            assert_eq!(
                DoorMethod::resolve("agent/getAuthenticatedExtendedCard", acceptance, false),
                None
            );
            assert_eq!(
                DoorMethod::resolve("agent/getAuthenticatedExtendedCard", acceptance, true),
                Some(DoorMethod::GetAuthenticatedExtendedCard)
            );
            let mut expected = served_methods(acceptance, false);
            expected.push("agent/getAuthenticatedExtendedCard");
            assert_eq!(served_methods(acceptance, true), expected);
        }
    }

    #[test]
    fn door_method_resolve_matches_names_exactly() {
        for acceptance in &ACCEPTANCES {
            for name in [
                "tasks/list",
                "",
                "Tasks/Get",
                "TASKS/GET",
                " tasks/get",
                "tasks/get ",
                "tasks/get\n",
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
                methods.contains(&"message/stream"),
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
        // Grab a free port from the OS, release it, then explicitly request it.
        let port = {
            let (l, p) = bind_local_port("127.0.0.1", None).await.unwrap();
            drop(l);
            p
        };
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

    /// A `message/stream` connection that falls behind is told how many live frames it lost, keeps
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
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "message/stream".to_string(),
            params: serde_json::json!({
                "message": {
                    "messageId": "msg_lagged",
                    "role": "user",
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
                None,
                TaskProvenance::derive(TaskOrigin::User, None),
                None,
                false,
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

    /// The session's last frames are broadcast just before the door closes. A `message/stream`
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
            jsonrpc: "2.0".to_string(),
            id: serde_json::json!(1),
            method: "message/stream".to_string(),
            params: serde_json::json!({
                "message": {
                    "messageId": "msg_closing",
                    "role": "user",
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
                None,
                TaskProvenance::derive(TaskOrigin::User, None),
                None,
                false,
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
}
