# Agent Card

Every capsule serves an [A2A](https://a2a-protocol.org) v1.0 `AgentCard` at
`GET /.well-known/agent-card.json` on its HTTP listener. The card parses as `lf.a2a.v1.AgentCard`,
as defined by [`a2a.proto` at `v1.0.1`](https://github.com/a2aproject/A2A/blob/v1.0.1/specification/a2a.proto),
under a strict protobuf JSON parser. It names the session answering the address, states what the
capsule may do, lists what its listener answers, and names the frames its stream can send.

The card a capsule `my-agent` 0.1.0 serves on port 41873, with `lifecycle.task_acceptance: single`,
`bash` installed, shell and network granted, `exports.files` declared and no
[`network.authentication`](manifest.md#field-network-authentication):

```json
{
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
}
```

What murmur adds to the standard card sits in three [extensions](#extensions):

| Extension | Answers | Derived from |
|---|---|---|
| [Door](#murmur-door-v1) | What does this listener answer, and does it serve other capsules? | The methods `POST /` dispatches, and [`exports.peer_tasks`](manifest.md#field-exports-peer-tasks) |
| [Capsule](#murmur-capsule-v1) | Which session is this, and what may the capsule do? | The session, the installed artifacts, `capabilities.*` and the declared `exports` |
| [Stream](#stream-extension) | Which frames can its stream send? | The streaming methods the door serves |

A capsule that declares [`network.authentication`](manifest.md#field-network-authentication)
serves a different public card, and moves the capsule extension to an extended card only an
authenticated caller can read — see [Security](#security).

---

## Keys { #keys }

Every key below is present on every card this runtime serves.

| Key | Type | Value |
|---|---|---|
| `name` | string | The capsule's `name` from `murmur.yaml` |
| `description` | string | `Murmur capsule <name> <version>` |
| `version` | string | The capsule's `version` from `murmur.yaml` |
| `supportedInterfaces` | array of objects | One entry — see [`supportedInterfaces`](#supported-interfaces) |
| `capabilities` | object | See [`capabilities`](#capabilities) |
| `securitySchemes` | object | `{}`, or the `bearer` scheme on an authenticated door — see [Security](#security) |
| `securityRequirements` | array | `[]`, or one requirement on an authenticated door — see [Security](#security) |
| `defaultInputModes` | array of strings | `["text/plain", "application/json"]`. The door reads text parts and data parts — see [Parts](#parts) |
| `defaultOutputModes` | array of strings | `["text/plain", "application/json"]`. The door writes text parts, and data parts in a cancel's `residue` artifact |
| `skills` | array of objects | See [`skills`](#skills). Empty under `lifecycle.task_acceptance: none` |

The card carries no `provider`, `documentationUrl`, `iconUrl` or `signatures`.

## `supportedInterfaces` { #supported-interfaces }

One interface: the JSON-RPC door at `POST /`.

| Key | Value |
|---|---|
| `url` | `http://<host:port>` — the address this capsule's listener answers on. The door speaks plain HTTP |
| `protocolBinding` | `JSONRPC` |
| `protocolVersion` | `1.0` |

The door answers A2A v1.0 method names, and serves a JSON-RPC request only when it names version
`1.0` in its [`A2A-Version`](#a2a-version) header. Tasks, messages and parts have the A2A v1.0
ProtoJSON shape — see [Tasks and messages](#tasks-and-messages).

## `capabilities` { #capabilities }

| Key | Type | Value |
|---|---|---|
| `capabilities.streaming` | boolean | `true` exactly when the [door extension](#murmur-door-v1) lists `SendStreamingMessage` |
| `capabilities.pushNotifications` | boolean | `false` — see [Push notifications](#push-notifications), and [what is not supported](#conformance-not-supported) |
| `capabilities.extendedAgentCard` | boolean | `false` on a public door, where every caller gets this card. `true` on an [authenticated door](#security) |
| `capabilities.extensions` | array of objects | The [door extension](#murmur-door-v1), the [capsule extension](#murmur-capsule-v1), then the [stream extension](#stream-extension). An authenticated door's public card carries the door extension, then the stream extension |

A door that does not serve `SendStreamingMessage` —
[`lifecycle.task_acceptance: none`](manifest.md#lifecycle-task-acceptance) — reports `false` and
answers it and `SubscribeToTask` `-32004` `UnsupportedOperationError`.

## Security { #security }

Who may call the door is set by [`network.authentication`](manifest.md#field-network-authentication).
The security schemes the door does not offer are listed under [A2A v1.0 conformance](#conformance).

| Manifest | Door | `securitySchemes` | `securityRequirements` | Extended card |
|---|---|---|---|---|
| No `network.authentication` | Public: answers every caller that reaches the port, and ignores `Authorization` | `{}` | `[]` | None |
| `network.authentication.scheme: bearer` | Authenticated: every request but the public card presents a token | `bearer`, an HTTP `Bearer` scheme | Any valid token | The authenticated card with the capsule extension — see [The extended card](#extended-card) |

A public door's card is exactly the card at the top of this page.

### Tokens { #tokens }

An authenticated capsule's runtime mints its tokens at launch, and [`mur token`](cli.md#mur-token)
prints them. A token is valid until the session ends; a restart or a `--resume` mints new ones.

| Credential | Scopes | Minted |
|---|---|---|
| `operator` | Every scope | Always |
| Each name under `network.authentication.credentials` | The scopes that credential lists | One per declared credential |

A scope is a door method's name, or `resources/files` for the
[operator plane](resource-plane.md#operator-plane):

| Scope | Reaches |
|---|---|
| `SendMessage` | [`SendMessage`](#murmur-door-v1) |
| `SendStreamingMessage` | `SendStreamingMessage` |
| `murmur.WatchSession` | [`murmur.WatchSession`](#watch-session) |
| `GetTask` | [`GetTask`](#tasks-get) |
| `ListTasks` | [`ListTasks`](#list-tasks) |
| `CancelTask` | `CancelTask` |
| `SubscribeToTask` | [`SubscribeToTask`](#subscribe-to-task) |
| `murmur.StopSession` | [`murmur.StopSession`](#stop-session) |
| `resources/files` | The [operator plane](resource-plane.md#operator-plane) |

`GetExtendedAgentCard` needs no scope: every valid token may read the extended card. The four push
config methods need none either: every caller is answered that [push](#push-notifications) is not
supported.

A scope decides which methods a token reaches. Which tasks those methods reach is decided by the
credential — see [Who sees a task](#task-access).

### Who sees a task { #task-access }

Every method that names or lists a task reads one rule:

| Caller | Sees |
|---|---|
| Any caller of a public door | Every task of the session |
| The `operator` token | Every task of the session |
| A declared credential's token | The tasks a request carrying that credential started |
| A [formation token](roster.md#formation-token), credential `member:<caller>` | The tasks that member started |

The rule governs [`GetTask`](#tasks-get), [`ListTasks`](#list-tasks), `CancelTask`,
[`SubscribeToTask`](#subscribe-to-task), a message's [`taskId`](#task-id) and its
[`referenceTaskIds`](#reference-task-ids). A task the caller does not see is answered exactly as an
id the session never held: [`-32001`](#errors) with `metadata.taskId`, and
`not a task this capsule holds` in a reference block. A task the runtime starts itself, such as a
detached command's result, belongs to the session.

`murmur.WatchSession` and `murmur.StopSession` are the session's own methods. A credential holding either
reaches every task through it, whatever that credential started: `murmur.WatchSession` carries every
task's events, and `murmur.StopSession` cancels every live task.

### The public card of an authenticated door { #authenticated-public-card }

`my-agent` from the top of this page, declaring `network.authentication`:

```json
{
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
}
```

It differs from a public door's card in these keys:

| Key | Value |
|---|---|
| `capabilities.extendedAgentCard` | `true` |
| `capabilities.extensions` | The door extension, then the stream extension. The [capsule extension](#murmur-capsule-v1) is on the extended card |
| Door extension `params.methods` | Gains `GetExtendedAgentCard` |
| `securitySchemes` | `bearer`: an `httpAuthSecurityScheme` with scheme `Bearer` |
| `securityRequirements` | `[{"schemes": {"bearer": {"list": []}}}]`: any valid token |
| `task` skill `securityRequirements` | One requirement per served task-starting method, `SendMessage` then `SendStreamingMessage`, each naming the scope it needs |

### The extended card { #extended-card }

`GetExtendedAgentCard` returns the extended card as its JSON-RPC `result`, to any valid token. It
takes no `params`:

```bash
curl -s -X POST http://localhost:41873/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"GetExtendedAgentCard"}'
```

The result is an A2A v1.0 `AgentCard`: the
[public card of the authenticated door](#authenticated-public-card) with the
[capsule extension](#murmur-capsule-v1) added.

| Key | Value on the extended card |
|---|---|
| `capabilities.extensions` | The door extension, the capsule extension, then the stream extension |
| Every other key | As on the authenticated door's public card: the `bearer` scheme, `securityRequirements`, `capabilities.extendedAgentCard: true`, and `supportedInterfaces` declaring `protocolVersion` `1.0` |

The capsule extension's `params.sessionId` is at the same path on a public door's card and on the
extended card.

A public door has no extended card. It answers `GetExtendedAgentCard` with
[`-32004`](#errors) `UnsupportedOperationError`, and its card does not list the method.

## A2A v1.0 conformance { #conformance }

The door conforms to A2A v1.0 as
[`a2a.proto` at `v1.0.1`](https://github.com/a2aproject/A2A/blob/v1.0.1/specification/a2a.proto)
defines it. This section lists what it serves, what it answers as not supported, and the
extensions murmur adds.

### What is served { #conformance-served }

The door has one interface:

| Interface | Value |
|---|---|
| Transport | JSON-RPC 2.0 over HTTP: one [`POST /`](#door) per request |
| Address | The card's `supportedInterfaces[0].url` |
| `protocolBinding` | `JSONRPC` |
| `protocolVersion` | `1.0` |
| Version negotiation | Every JSON-RPC request sends [`A2A-Version: 1.0`](#a2a-version), or is answered `-32009` |

Every method of the proto's `service A2AService`, in its order, then murmur's own:

| Method | Answer | Where documented |
|---|---|---|
| `SendMessage` | Served under [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance) `single` or `queue`. `-32601` under `none` | [Messages](#message), [Answers](#send-message-response) |
| `SendStreamingMessage` | Served under `single` or `queue`. `-32004` under `none` | [Streaming Protocol](streaming-protocol.md#stream-order) |
| `GetTask` | Served under every `task_acceptance` | [`GetTask`](#tasks-get) |
| `ListTasks` | Served under every `task_acceptance` | [`ListTasks`](#list-tasks) |
| `CancelTask` | Served under every `task_acceptance`. `-32002` on a task that has ended | [`GetTask`](#tasks-get), [Errors](#errors) |
| `SubscribeToTask` | Served under `single` or `queue`. `-32004` under `none`, and on a task that has ended | [`SubscribeToTask`](#subscribe-to-task) |
| `CreateTaskPushNotificationConfig` | `-32003` | [Push notifications](#push-notifications) |
| `GetTaskPushNotificationConfig` | `-32003` | [Push notifications](#push-notifications) |
| `ListTaskPushNotificationConfigs` | `-32003` | [Push notifications](#push-notifications) |
| `GetExtendedAgentCard` | Served on an [authenticated door](#security). `-32004` on a public door | [The extended card](#extended-card) |
| `DeleteTaskPushNotificationConfig` | `-32003` | [Push notifications](#push-notifications) |
| `murmur.WatchSession` | Served under every `task_acceptance` | [`murmur.WatchSession`](#watch-session) |
| `murmur.StopSession` | Served under every `task_acceptance` | [`murmur.StopSession`](#stop-session) |

### What is not supported { #conformance-not-supported }

| Feature | How a request meets it | Why |
|---|---|---|
| Push notification configs: the four methods, and `configuration.taskPushNotificationConfig` in a message | [`-32003`](#push-notifications). The card says `capabilities.pushNotifications: false` | A capsule's egress is closed by default, and a webhook to a client's URL would be a new egress path |
| The gRPC and HTTP+JSON bindings | Not listed in `supportedInterfaces`: the door's one interface is `JSONRPC` | One binding is enough, and JSON-RPC is the one every A2A SDK speaks |
| The OAuth2, OpenID Connect, API key and mutual TLS security schemes | Not in `securitySchemes`: an authenticated door declares only [`bearer`](#security) | Murmur consumes credentials and never issues them. Door tokens are [minted per launch](#tokens) and presented as bearer tokens |
| Card signatures | The card carries no `signatures` | Signatures answer whether a provider issued a card, which matters when a card is discovered. A formation launches its own members, so the caller already knows where each card came from |
| Tenants | A request naming a `tenant` is answered [`-32602`](#tenant). The interface declares no `tenant` | A capsule is one session. A tenant is refused rather than ignored |

Inside the methods it serves, the door refuses these requests:

| Request | Answer |
|---|---|
| A message with a `raw` or `url` file part | [`-32005`](#parts) |
| `SendStreamingMessage` or `SubscribeToTask` to a door that starts no task | [`-32004`](#capabilities) |
| `SubscribeToTask` naming a task that has ended | [`-32004`](#subscribe-to-task) |
| A message whose `taskId` names a task that is not in `TASK_STATE_INPUT_REQUIRED` | [`-32004`](#task-id) |
| `GetExtendedAgentCard` to a public door | [`-32004`](#extended-card) |

### Extensions { #conformance-extensions }

Every extension murmur declares has `required: false`, so a client that ignores all three is served.

| URI | Adds | `required` | How a client activates it |
|---|---|---|---|
| [`https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1`](#murmur-door-v1) | The `murmur.` methods, the [`forgetSession`](#forget-session) message metadata field, and the `A2A-Extensions` echo of what a request used | `false` | By calling a `murmur.` method, or by setting `forgetSession` under the URI in `message.metadata`. A served answer names the URI in its `A2A-Extensions` header |
| [`https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1`](#murmur-capsule-v1) | Card `params` only: the session id and what the capsule may do | `false` | Nothing to activate. It is on a public door's card, and on an authenticated door's [extended card](#extended-card) |
| [`https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1`](streaming-protocol.md#murmur-stream-v1) | Murmur's [frames](streaming-protocol.md#murmur-stream-v1) on a stream, in each event's `metadata` under the URI | `false` | By sending `A2A-Extensions: <URI>` on `SendStreamingMessage` or `SubscribeToTask`; the response echoes it — see [Activation](streaming-protocol.md#activation). Always active on `murmur.WatchSession` |

## The JSON-RPC door { #door }

Every JSON-RPC request is one `POST /` with `content-type: application/json`, a JSON-RPC 2.0 body,
and an [`A2A-Version: 1.0`](#a2a-version) header:

```http
POST / HTTP/1.1
Content-Type: application/json
A2A-Version: 1.0

{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{"id":"tsk_…"}}
```

The methods are listed in the [door extension](#murmur-door-v1).

### What the door answers { #door-authentication }

The door checks, in this order, on every request:

1. `GET /.well-known/agent-card.json`, the [peer plane](resource-plane.md#peer-plane) under
   `/resources/peer/` and the [control surface](control-surface.md), which takes its own token,
   are served without a door token.
2. The token, on a door declaring `network.authentication`, before the path, the body or the
   method is read. A refusal here says nothing about whether a task, a file or a path exists.
3. Peer consent: a request carrying `x-murmur-task-origin: peer` is refused unless the capsule
   declares [`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks). Checked on
   every door, before the path, the body or the method is read.
4. The [operator plane](resource-plane.md#operator-plane) under `/resources/files`: the
   `resources/files` scope on a door declaring `network.authentication`, then the plane.
5. Anything but a `POST /` with `content-type: application/json` and a body is answered `404`.
6. A body that is not JSON: `-32700` `Invalid JSON payload`, with `id: null`.
7. A body that is not a [JSON-RPC 2.0 request](#jsonrpc-request): `-32600`
   `Request payload validation error`.
8. The [`A2A-Version`](#a2a-version) header: `-32009` unless it names `1.0`.
9. A [completion](roost-api.md#how-it-travels), a request carrying
   `x-murmur-task-origin: completion`: addressed to another session, `-31001`; otherwise the door
   answers it — see [Completion errors](#murmur-errors).
10. The method: `-32601` `Method not found` for a method the
    [door extension](#murmur-door-v1) does not list. A public door answers `GetExtendedAgentCard`
    with `-32004` instead, a door that starts no task answers `SendStreamingMessage` and
    `SubscribeToTask` with `-32004`, and every door answers the four
    [push config methods](#push-notifications) with `-32003`.
11. The scope of the method, on a door declaring `network.authentication`.
12. `params` that is present and is not an object: `-32602`.
13. A [`tenant`](#tenant) in `params` other than `""`: `-32602`.
14. The method itself, whose errors are listed under [Errors](#errors). On `SendMessage` and
    `SendStreamingMessage` that starts with reading the [message](#message), its
    [`forgetSession`](#forget-session) field included.

A refusal at steps 2, 3, 4 and 11 is an HTTP status and a JSON body, never a JSON-RPC envelope:

| Case | Status | `www-authenticate` | Body `error` |
|---|---|---|---|
| No `Authorization` header | `401` | `Bearer realm="<capsule name>"` | `unauthenticated` |
| An `Authorization` header that is not `Bearer <token>`, a token this session did not mint, or more than one `Authorization` header | `401` | `Bearer realm="<capsule name>", error="invalid_token"` | `invalid_token` |
| A peer-origin request to a capsule that does not accept peer tasks | `403` | None | `peer_not_accepted` |
| A valid token whose credential lacks the scope | `403` | `Bearer realm="<capsule name>", error="insufficient_scope", scope="<scope>"` | `insufficient_scope` |

The body is `{"error": "<code>", "message": "<sentence>"}`. The `insufficient_scope` message names
the credential and the scope it lacks: `credential 'watcher' does not reach SendMessage`. The
`peer_not_accepted` response is byte-identical for every request it refuses. The scheme name
`Bearer` matches in any case; the token matches exactly.

Every answer from step 6 on is a JSON-RPC response with HTTP status `200`, except a scope refusal at
step 11.

The door speaks plain HTTP, so a token sent across a network travels in clear text. Put a TLS
terminator in front of a door that is reached off the host.

### JSON-RPC request { #jsonrpc-request }

| Member | Required | Accepted |
|---|---:|---|
| `jsonrpc` | yes | Exactly `"2.0"` |
| `method` | yes | A string, matched exactly against the [door extension](#murmur-door-v1)'s method names |
| `id` | yes | A string or an integer. A request without one, a notification, is refused |
| `params` | no | An object. Omitted, it reads as `{}` |

The body is one object; a batch array is refused. Members beyond these four are ignored. A refusal
at step 7 echoes the request's `id` when the `id` itself is valid, and is `null` otherwise.

### `A2A-Version` { #a2a-version }

Every JSON-RPC request names the A2A version it speaks in the `A2A-Version` HTTP header. The door
speaks `1.0`.

| `A2A-Version` header | Door |
|---|---|
| `1.0`, or `1.0.<digits>` such as `1.0.1` | Serves the request |
| Absent or empty, which A2A reads as version `0.3` | `-32009` |
| Any other value: `0.3`, `1`, `1.1`, `2.0`, `1.0x` | `-32009` |
| More than one `A2A-Version` header line, whatever their values | `-32009` |

The header name matches in any case, and surrounding whitespace in the value is ignored. A query
parameter does not set the version.

Every method is negotiated: the A2A methods, `murmur.WatchSession`, `murmur.StopSession` and
completions alike. The refusal is one JSON body, also on `SendStreamingMessage` and
`murmur.WatchSession`, and nothing is
started or stopped:

```json
{"jsonrpc": "2.0", "id": 1,
 "error": {"code": -32009,
           "message": "A2A version '' is not supported; this agent speaks 1.0",
           "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                     "reason": "VERSION_NOT_SUPPORTED",
                     "domain": "a2a-protocol.org",
                     "metadata": {"requestedVersion": "", "supportedVersions": "1.0"}}]}}
```

`GET /.well-known/agent-card.json`, the [resource plane](resource-plane.md), the peer plane and the
[control surface](control-surface.md) read no `A2A-Version`.

Every request murmur itself sends to a door carries `A2A-Version: 1.0`: `mur ps`, `mur watch`,
`mur cancel`, `mur stop`, [`call-member`](runtime-provided-tools.md#call-member),
[`delegate-task`](roost-api.md#the-delegation-tool) and its completions, a
[plan's `capsule` step](plans.md), and [`murmur:message/send`](wit-interfaces.md#message-send).

### Errors { #errors }

The standard JSON-RPC errors carry no `data`:

| Code | Message | Answers |
|---|---|---|
| `-32700` | `Invalid JSON payload` | A body that is not JSON. `id` is `null` |
| `-32600` | `Request payload validation error` | A body that is not a [JSON-RPC 2.0 request](#jsonrpc-request) |
| `-32601` | `Method not found` | A method the door extension does not list |
| `-32602` | Names the parameter | `params` that is not an object, a [`tenant`](#tenant), a message the door cannot [read in full](#message), a `configuration.returnImmediately` that is not a boolean, a message naming a waiting task in another `contextId`, a `GetTask`, `CancelTask` or `SubscribeToTask` with no `id`, a [`ListTasks` parameter](#list-tasks) it cannot read, or a [`forgetSession`](#forget-session) that is malformed, names a `taskId`, or reaches a capsule not on `inference.transport: process` |
| `-32603` | Names the failure | A request the door accepted and could not hand to the session |

Every A2A error carries `error.data`, an array holding one `google.rpc.ErrorInfo`:

| Key | Value |
|---|---|
| `@type` | `type.googleapis.com/google.rpc.ErrorInfo` |
| `reason` | The error's reason, below |
| `domain` | `a2a-protocol.org` |
| `metadata` | An object of string values, below. `{}` when the error has none |

| Code | Error | `reason` | The door answers it | `metadata` |
|---|---|---|---|---|
| `-32001` | `TaskNotFoundError` | `TASK_NOT_FOUND` | On `GetTask`, `CancelTask`, `SubscribeToTask`, naming an id this session never held or a task the caller does not [see](#task-access), and on a message whose `taskId` names one | `taskId` |
| `-32002` | `TaskNotCancelableError` | `TASK_NOT_CANCELABLE` | On `CancelTask` for a task that had already ended. The task is left unchanged | `taskId`; `state`, the state it ended in, spelled as `GetTask` spells it: `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_REJECTED` or `TASK_STATE_CANCELED` |
| `-32003` | `PushNotificationNotSupportedError` | `PUSH_NOTIFICATION_NOT_SUPPORTED` | On the four push config methods, and on a message asking for push — see [Push notifications](#push-notifications) | `{}` |
| `-32004` | `UnsupportedOperationError` | `UNSUPPORTED_OPERATION` | On `GetExtendedAgentCard` to a public door, and on `SendStreamingMessage` and `SubscribeToTask` to a door that starts no task, with `{}`. On a message whose `taskId` names a task that is not waiting for input — see [Continuing a task](#task-id). On `SubscribeToTask` naming a task that has ended | `taskId` and `state`, on a message or a `SubscribeToTask` naming a task |
| `-32005` | `ContentTypeNotSupportedError` | `CONTENT_TYPE_NOT_SUPPORTED` | On a message carrying a `raw` or `url` part — see [Parts](#parts) | `partIndex`, the first file part's 0-based index; `mediaType`, when that part declared one |
| `-32006` | `InvalidAgentResponseError` | `INVALID_AGENT_RESPONSE` | Never | — |
| `-32007` | `ExtendedAgentCardNotConfiguredError` | `EXTENDED_AGENT_CARD_NOT_CONFIGURED` | Never | — |
| `-32008` | `ExtensionSupportRequiredError` | `EXTENSION_SUPPORT_REQUIRED` | Never | — |
| `-32009` | `VersionNotSupportedError` | `VERSION_NOT_SUPPORTED` | On any request whose [`A2A-Version`](#a2a-version) is not `1.0` | `requestedVersion`, the header's value as sent: `""` when it was absent, and every line's value joined with `, ` when there was more than one; `supportedVersions`, `1.0` |

```json
{"jsonrpc": "2.0", "id": 3,
 "error": {"code": -32002,
           "message": "Task cannot be canceled: it is already TASK_STATE_COMPLETED",
           "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                     "reason": "TASK_NOT_CANCELABLE",
                     "domain": "a2a-protocol.org",
                     "metadata": {"taskId": "tsk_…", "state": "TASK_STATE_COMPLETED"}}]}}
```

```json
{"jsonrpc": "2.0", "id": 4,
 "error": {"code": -32005,
           "message": "message.parts[1] is a url part; this agent reads text and data parts only",
           "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                     "reason": "CONTENT_TYPE_NOT_SUPPORTED",
                     "domain": "a2a-protocol.org",
                     "metadata": {"partIndex": "1", "mediaType": "application/pdf"}}]}}
```

A completion's errors, `-31001` and `-31002`, are murmur's own and are listed under
[Completion errors](#murmur-errors).

### Push notifications { #push-notifications }

The card says `capabilities.pushNotifications: false`. The door answers each request below
`-32003` `PushNotificationNotSupportedError`, with the message
`push notifications are not supported by this agent` and `metadata` `{}`:

| Request | Answered |
|---|---|
| `CreateTaskPushNotificationConfig`, `GetTaskPushNotificationConfig`, `ListTaskPushNotificationConfigs`, `DeleteTaskPushNotificationConfig` | At [step 10](#door-authentication), to every caller the door lets in, before any scope or params check. The door extension does not list them |
| `SendMessage` or `SendStreamingMessage` whose `configuration.taskPushNotificationConfig` is present and not `null` | Once the message is [read](#message), before any task starts. `SendStreamingMessage` answers it as one JSON body, with no stream |

```json
{"jsonrpc": "2.0", "id": 1,
 "error": {"code": -32003,
           "message": "push notifications are not supported by this agent",
           "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                     "reason": "PUSH_NOTIFICATION_NOT_SUPPORTED",
                     "domain": "a2a-protocol.org",
                     "metadata": {}}]}}
```

### Tenants { #tenant }

The card's interface names no `tenant`, and the door serves none. A request to any method the door
serves, murmur's own included, whose `params.tenant` is present and is anything but `""` is answered
`-32602` at [step 13](#door-authentication), before anything starts, stops or is cancelled:

```text
Invalid params: tenant "acme" names a tenant; this agent serves no tenants, so omit tenant
```

The value is shown as JSON, so `"tenant": 7` reads `tenant 7`. `""`, the unset value, is served as
if the member were absent. A method the door does not serve is still `-32601`, a push config method
still `-32003`, and a wrong `A2A-Version` still `-32009`.

### Tasks and messages { #tasks-and-messages }

Every task, message, part and artifact on the JSON-RPC door is A2A v1.0 ProtoJSON: camelCase field
names and enum value names. The door reads field names and enum values in that spelling only.

#### Task states { #task-states }

| `status.state` on the wire | Meaning | Murmur's word |
|---|---|---|
| `TASK_STATE_SUBMITTED` | Accepted and queued | `submitted` |
| `TASK_STATE_WORKING` | Running | `working` |
| `TASK_STATE_INPUT_REQUIRED` | Waiting for a reply — see [Continuing a task](#task-id) | `input-required` |
| `TASK_STATE_COMPLETED` | Ended with an answer | `completed` |
| `TASK_STATE_FAILED` | Ended without one | `failed` |
| `TASK_STATE_REJECTED` | Refused, busy or closing, and never run | `rejected` |
| `TASK_STATE_CANCELED` | Stopped by `CancelTask` or [`murmur.StopSession`](#stop-session) | `canceled` |

The door never produces `TASK_STATE_AUTH_REQUIRED`: it authenticates a request before any task
exists, so no task waits for authentication.

Murmur's word is the spelling murmur uses off the wire:
[`murmur:message/send`](wit-interfaces.md#message-send)'s `task-result.state`, `mur` output and the
trace. Every task status on the door and on its [streams](streaming-protocol.md) carries the
`TASK_STATE_*` name.

#### Status timestamps { #status-timestamp }

Every `status` the door writes carries `timestamp`: when that status was recorded, in RFC 3339 UTC
with milliseconds and a `Z`, such as `2026-10-13T09:00:00.123Z`.

| Status | `timestamp` |
|---|---|
| A task read from the session's task record: [`GetTask`](#tasks-get), [`ListTasks`](#list-tasks), [`CancelTask`](#tasks-get), [`SendMessage`'s answer](#send-message-response), and on a stream the opening `task` and the terminal status | The task's last change: the same change that moves it to the front of `ListTasks` |
| A refused `TASK_STATE_REJECTED` task the door never took | When the door refused it |
| A status a [stream](streaming-protocol.md#standard-events) writes as it happens: a `TASK_STATE_WORKING` progress status, a `TASK_STATE_INPUT_REQUIRED` status, a [carrier](streaming-protocol.md#carriers) | When it was written |

A `timestamp` is the exact instant the door compares [`statusTimestampAfter`](#list-tasks) with,
so a cutoff copied from a task's `status.timestamp` includes that task.

#### Messages { #message }

`SendMessage` and `SendStreamingMessage` take `params.message`, an A2A `Message`. The door reads it
in full or refuses it; no part is skipped.

| Field | Required | Accepted |
|---|---:|---|
| `messageId` | yes | A non-empty string |
| `role` | yes | `ROLE_USER` only. A message to this agent is from the user |
| `parts` | yes | A non-empty array of [parts](#parts) |
| `taskId` | no | A string naming the task this message continues — see [Continuing a task](#task-id). `""` names none |
| `contextId` | no | A string. A new task without one is given a fresh `ctx_…` |
| `referenceTaskIds` | no | An array of non-empty strings naming at most 16 distinct tasks — see [Referenced tasks](#reference-task-ids) |
| `metadata` | no | An object. The door reads one key, the [door extension](#murmur-door-v1)'s URI, which holds [`forgetSession`](#forget-session). Every other key, and a `metadata` that is not an object, is ignored |
| `extensions` | no | Ignored |

The checks run in this order, and the first failure answers:

1. `params.message` is an object. A bare message as `params` is refused.
2. `messageId`, `role`, then `parts`, as in the table.
3. Every part sets exactly one of `text`, `raw`, `url` and `data`.
4. `text`, `raw`, `url`, `mediaType` and `filename` are strings where present.
5. `taskId` and `contextId`, then `referenceTaskIds`, as in the table.
6. The door extension's key in `metadata`, then its `forgetSession` — see
   [`forgetSession`](#forget-session).
7. A `configuration.returnImmediately` that is present, not `null` and not a boolean — see
   [Answers](#send-message-response).
8. The first `raw` or `url` part: [`-32005`](#errors).
9. A `configuration.taskPushNotificationConfig` that is present and not `null`:
   [`-32003`](#push-notifications).
10. A `forgetSession: true` to a capsule not on `inference.transport: process`: `-32602`.

Steps 1 to 7 and 10 answer `-32602`. Every refusal comes before the door looks up a task or starts
one: a refused message starts no task, writes no trace record, reaches no model and delivers nothing
to a waiting task. Unknown fields are ignored.

#### Parts { #parts }

| Part | The door | The agent receives |
|---|---|---|
| `{"text": "…"}` | Reads it, whatever its `mediaType` | The text |
| `{"data": <any JSON>}` | Reads it, `null` included, whatever its `mediaType` | The value in a fenced block — see below |
| `{"raw": "…"}`, `{"url": "…"}` | Refuses the message with `-32005` | Nothing |

| Part field | The agent receives |
|---|---|
| `mediaType`, `filename` | On a data part, in the fence's opening line. Not on a text part |
| `metadata` | Never |

The agent reads the parts in order, joined by a newline. A data part is its value pretty-printed in a
Markdown code fence labelled `data`, followed by `media-type=<mediaType>` and `filename=<filename>`
when the part has them. Whitespace, backticks and `=` in those values become `_`. The fence is one
backtick longer than the longest run of backticks in the value, and at least three, so nothing in the
value closes it:

`````text
summarise
````data media-type=application/json
{
  "note": "a ``` b",
  "rows": [
    1,
    2
  ]
}
````
`````

A task whose [trust class](../concepts/access-control.md#task-origin-and-trust-class) is untrusted
has the whole text, fence included, wrapped in the [untrusted fence](untrusted-fence.md).

Every text part the door writes has `mediaType` `text/plain`, and every data part
`application/json`.

#### Continuing a task { #task-id }

`SendMessage` and `SendStreamingMessage` route a message by the task its `taskId` names:

| The message names | Answer |
|---|---|
| No task, or `""` | A new task: `TASK_STATE_SUBMITTED`, or `TASK_STATE_REJECTED` when the door has no room or the session is closing |
| A task this session never held, or one the caller does not [see](#task-access) | [`-32001`](#errors) |
| A task that has ended | [`-32004`](#errors), naming its state. Send a new message without `taskId`, in the same `contextId`, to start a new task |
| A `TASK_STATE_SUBMITTED` or `TASK_STATE_WORKING` task | `-32004`, naming its state. A task takes a message only while it waits in `TASK_STATE_INPUT_REQUIRED` |
| A `TASK_STATE_INPUT_REQUIRED` task, with a `contextId` other than the task's | `-32602` |
| A `TASK_STATE_INPUT_REQUIRED` task | The message's text is the task's reply, and the task is answered in `TASK_STATE_WORKING`. The door's capacity is not consulted |

A message that names no task never reaches a waiting one. `SendStreamingMessage` answers each
refusal as one JSON body before any event, except a new task refused because the capsule is busy
or its session is closing, which is a stream of one `TASK_STATE_REJECTED`
[`task` event](streaming-protocol.md#standard-events). How a stream ends and how a reply continues
it is under [What `SendStreamingMessage` writes](streaming-protocol.md#stream-order).

#### Referenced tasks { #reference-task-ids }

`referenceTaskIds` names tasks the message refers to. Each id is kept once, in first-occurrence
order, recorded on the task's [`task_start`](observability-schemas.md#task-start-record), and named to the
agent in a block after the parts, as each task stood when the message arrived:

`````text
<the parts>

Referenced tasks:
- tsk_a: completed
```response
four
```
- tsk_b: failed: the driver failed
- tsk_c: working
- tsk_d: not a task this capsule holds
`````

| Referenced task | Its line |
|---|---|
| Live | `- <id>: <state>` |
| `completed`, with a response | `- <id>: completed`, then the response in a fence labelled `response` |
| Ended otherwise, with a final status message | `- <id>: <state>: <message>`, the message on one line |
| Never held, or one the sender does not [see](#task-access) | `- <id>: not a task this capsule holds` |

States are [murmur's words](#task-states). A message without `referenceTaskIds` gets no block.

#### Answers { #send-message-response }

`SendMessage` answers a `SendMessageResponse`: `{"task": <Task>}` for a new, rejected or continued
task, and `{"message": <Message>}` for a [completion](#murmur-errors) the door accepted.

When a task is answered depends on `configuration.returnImmediately`:

| `configuration.returnImmediately` | The door answers |
|---|---|
| `false`, `null` or absent, or no `configuration` | Once the task settles, with the task as [`GetTask`](#tasks-get) reports it at that moment |
| `true` | Once the task is accepted: `TASK_STATE_SUBMITTED` for a new task, `TASK_STATE_WORKING` for a [reply](#task-id) to a waiting task |
| Anything else | `-32602` `configuration.returnImmediately must be a boolean`. Nothing starts |

A task settles when it ends in `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_CANCELED`
or `TASK_STATE_REJECTED`, or pauses in `TASK_STATE_INPUT_REQUIRED`. A completed task carries its
`response` artifact, and a paused one its `prompt` artifact. A reply to the paused task is answered
the same way.

These are answered at once, whatever `returnImmediately` says:

- every refusal, as a JSON-RPC error;
- a new task rejected because the door has no room or the session is closing, in
  `TASK_STATE_REJECTED`;
- a completion, with its `message`.

A bare `SendMessage` whose task completes:

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"task": {"id": "tsk_…", "contextId": "ctx_…",
                     "status": {"state": "TASK_STATE_COMPLETED",
                                "message": {"messageId": "msg_tsk_…_status",
                                            "contextId": "ctx_…", "taskId": "tsk_…",
                                            "role": "ROLE_AGENT",
                                            "parts": [{"text": "task completed",
                                                       "mediaType": "text/plain"}]},
                                "timestamp": "2026-10-13T09:00:04.512Z"},
                     "artifacts": [{"artifactId": "response", "name": "response",
                                    "parts": [{"text": "four", "mediaType": "text/plain"}]}]}}}
```

The same message with `"configuration": {"returnImmediately": true}`:

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"task": {"id": "tsk_…", "contextId": "ctx_…",
                     "status": {"state": "TASK_STATE_SUBMITTED",
                                "timestamp": "2026-10-13T09:00:00.123Z"}}}}
```

A completion:

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"message": {"messageId": "msg_dlg_…_received", "contextId": "ctx_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"data": {"delegation_id": "dlg_…", "received": true},
                                   "mediaType": "application/json"}]}}}
```

The door sets no deadline on a held request, and writes nothing until it answers. A client whose
HTTP timeout is shorter than its task sends `returnImmediately: true` and polls `GetTask`, or reads
the task's events with `SendStreamingMessage` or [`SubscribeToTask`](#subscribe-to-task). A client
that timed out never learned the task's id: [`ListTasks`](#list-tasks) filtered by the `contextId`
it put on the message finds the task.

A held request ends one of three ways:

| What happens | The held request |
|---|---|
| The task settles | Answered with the task |
| The door closes: the capsule exits, [`mur stop`](cli.md#mur-stop) runs, or [`murmur.StopSession`](#stop-session) is called | Answered with the task as `GetTask` reports it at that moment, whatever its state. Under `after_task: exit` the task has settled by then |
| The client disconnects | Nothing changes for the task: it runs on, and `GetTask`, `ListTasks` and `SubscribeToTask` see it. The door finds out the client left only when the answer fails to write. Nothing is cancelled and nothing is logged |

The door reads two members of `configuration`: `returnImmediately`, and
`taskPushNotificationConfig`, which it [refuses](#push-notifications). Every other member is
ignored. A `tenant` is [refused](#tenant).

A message from the agent — a task's `status.message`, a completion's acknowledgement — has `role`
`ROLE_AGENT`. A task's `status.message` also carries its `contextId` and `taskId`.

Every artifact carries an `artifactId`, which is its name: `response`, `prompt` or `residue`. Each
occurs at most once in a task, and its id is the same on every read.

## `skills` { #skills }

A door that serves `SendMessage` advertises one skill: running a task. Under
[`lifecycle.task_acceptance: none`](manifest.md#lifecycle-task-acceptance) no task can be started,
and `skills` is `[]`.

| Key | Value |
|---|---|
| `id` | `task` |
| `name` | `Run a task` |
| `description` | `Runs one task given as a text message and reports its outcome.` |
| `tags` | `["task"]` |

Installed tools are not skills: a caller cannot invoke a tool directly. They are listed on the
[capsule extension](#murmur-capsule-v1).

## Extensions { #extensions }

All three extensions are A2A `AgentExtension` objects with `required: false`: a standard A2A client
can call the capsule without understanding any of them. How a client uses each is under
[Extensions](#conformance-extensions).

| Key | Value |
|---|---|
| `uri` | The extension's identifier, which is the address of its section in these docs |
| `description` | What the extension carries |
| `required` | `false` |
| `params` | The extension's content, below |

### Door extension { #murmur-door-v1 }

URI: `https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1`

| Key | Type | Value |
|---|---|---|
| `params.methods` | array of strings | The JSON-RPC methods `POST /` answers, A2A and murmur alike |
| `params.peerTasks` | boolean | Whether the door serves a request another capsule's runtime sends, stamped `x-murmur-task-origin: peer`: the capsule's [`exports.peer_tasks.accept`](manifest.md#field-exports-peer-tasks), `false` when the block is absent. Always present, on the public and the extended card. When `false`, every such request is answered `403` [`peer_not_accepted`](#door-authentication) |

The extension also carries [`forgetSession`](#forget-session), a field a message's `metadata` may
hold under the extension's URI.

A method is listed exactly when `POST /` answers it with something other than `-32601 Method not
found`. Method names match exactly: case and surrounding whitespace are significant. Murmur's own
methods are the ones named `murmur.<Method>`; every other name is an A2A v1.0 method. Methods
appear in this order:

| Method | Answers | Listed when |
|---|---|---|
| `SendMessage` | Starts a task, or delivers input to a task waiting for it, and returns the task | `lifecycle.task_acceptance` is `single` or `queue` |
| `SendStreamingMessage` | Starts a task and streams its events as `text/event-stream` | `lifecycle.task_acceptance` is `single` or `queue` |
| [`murmur.WatchSession`](#watch-session) | Streams every task of this session as an observer, without starting a task | Always |
| `GetTask` | Returns the task named by `params.id` | Always |
| `ListTasks` | Returns one [page](#list-tasks) of the tasks the caller sees | Always |
| `CancelTask` | Cancels the task named by `params.id`. A task that had already ended is left unchanged and answered [`-32002`](#errors) | Always |
| `SubscribeToTask` | Streams the rest of the live task named by `params.id` as `text/event-stream` — see [`SubscribeToTask`](#subscribe-to-task) | `lifecycle.task_acceptance` is `single` or `queue` |
| [`murmur.StopSession`](#stop-session) | Cancels every live task and reports what the session leaves running | Always |
| `GetExtendedAgentCard` | Returns the [extended card](#extended-card) | The capsule declares [`network.authentication`](manifest.md#field-network-authentication) |

See [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance). Under `none`, `POST /`
answers `SendMessage` with `-32601`, and `SendStreamingMessage` and `SubscribeToTask` with `-32004`,
so none of the three is listed, `capabilities.streaming` is `false` and `skills` is `[]`.

| Door | `params.methods` |
|---|---|
| Public | `SendMessage`, `SendStreamingMessage`, `murmur.WatchSession`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, `murmur.StopSession` |
| Authenticated | The public door's eight, then `GetExtendedAgentCard` |
| Public, `task_acceptance: none` | `murmur.WatchSession`, `GetTask`, `ListTasks`, `CancelTask`, `murmur.StopSession` |
| Authenticated, `task_acceptance: none` | `murmur.WatchSession`, `GetTask`, `ListTasks`, `CancelTask`, `murmur.StopSession`, `GetExtendedAgentCard` |

The card endpoint itself is not listed, and neither are the four
[push config methods](#push-notifications), which the door answers but does not serve.

Calling a murmur method, or sending a message whose `metadata` holds the extension's URI, uses the
extension. A served answer names what it used in its `A2A-Extensions` response header; a refusal
carries none:

| Request | `A2A-Extensions` on the answer |
|---|---|
| `murmur.WatchSession` | The door URI, then the [stream extension](#stream-extension)'s URI |
| `murmur.StopSession` | The door URI |
| `SendMessage` whose `message.metadata` holds the door URI | The door URI |
| `SendStreamingMessage` whose `message.metadata` holds the door URI | The door URI, then the stream extension's URI when the request [activated](streaming-protocol.md#activation) it |
| A refusal: `-32009`, `-32601`, `403`, `-32602` and every other error | None |

#### `murmur.WatchSession` { #watch-session }

The observer stream of this session: every task's events, across turns and tasks, without sending
anything. `SendStreamingMessage` follows the one task its message starts or continues;
`murmur.WatchSession` follows them all, and stays open when a task ends. `params` is `{}`, and is
not read.

```bash
curl -N -X POST http://localhost:41873/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'Last-Event-ID: 41' \
  -d '{"jsonrpc":"2.0","id":1,"method":"murmur.WatchSession","params":{}}'
```

The answer is a `text/event-stream` with no `content-length`; the body ends when the connection
closes:

```text
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
connection: keep-alive
A2A-Extensions: https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1, https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1
```

It writes, in order:

1. A [`connection-ack`](streaming-protocol.md#event-connection-ack) frame in an unnumbered
   `message` event.
2. The [replay](streaming-protocol.md#replay) from `Last-Event-ID`, or from the start of the replay
   buffer when the header is absent: a [`gap`](streaming-protocol.md#event-gap) frame when the
   buffer cannot supply every event after that id, then the buffered events.
3. Every task's live events, murmur frames included, until the capsule's stream ends or the client
   disconnects.

The [stream extension](streaming-protocol.md#murmur-stream-v1) is always active on this stream,
whatever the request's `A2A-Extensions` says. The first two events of a reconnect from id `41`:

```text
data: {"jsonrpc":"2.0","id":1,"result":{"message":{"messageId":"msg_01a131fc15437521b334719530864c4f","role":"ROLE_AGENT","parts":[{"mediaType":"text/plain","text":"connection-ack"}],"metadata":{"https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1":{"frame":"connection-ack","role":"observer","conversationMode":"stateless"}}}}}

id: 42
data: {"jsonrpc":"2.0","id":1,"result":{"statusUpdate":{"taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"TASK_STATE_WORKING","message":{"messageId":"msg_0199c4e2f1b87a01b2c3d4e5f6071829","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","role":"ROLE_AGENT","parts":[{"mediaType":"text/plain","text":"inference turn 2"}]},"timestamp":"2026-10-13T09:00:00.123Z"}}}}

```

Every event and frame it can write, and the heartbeat, are under
[What `murmur.WatchSession` writes](streaming-protocol.md#watch-order).
[`mur watch`](cli.md#mur-watch) is a client of this method.

#### `murmur.StopSession` { #stop-session }

Cancels every task the session still holds, and reports what the session leaves running, in one
answer. `params` is `{}`, and is not read.

```bash
curl -s -X POST http://localhost:41873/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{"jsonrpc":"2.0","id":1,"method":"murmur.StopSession","params":{}}'
```

```text
HTTP/1.1 200 OK
content-type: application/json
content-length: 267
connection: close
A2A-Extensions: https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1

{"jsonrpc":"2.0","id":1,"result":{"canceled":["tsk_019ed5211c827f63a8fe4be623277c55"],"residue":[{"binary":"bash","command":"sleep 30","kind":"detached_shell","started_at_ms":1757068800123,"work_id":"wrk_9f2a1c"}],"session_id":"ses_019f01a940ce7761854e768ecbe3d399"}}
```

| Key | Type | Value |
|---|---|---|
| `session_id` | string | The session this door answers for |
| `canceled` | array of strings | The tasks this call moved to `TASK_STATE_CANCELED`, sorted. `[]` when none was live, so a second call answers `[]` |
| `residue` | array of objects | One [residue item](#residue-items) per thing the session leaves running. `[]` when nothing is |

All three keys are always present. `CancelTask` omits its `residue` artifact when nothing is
running; `murmur.StopSession` answers `"residue": []`.

`murmur.StopSession` ends no process and does not shut the door: the capsule keeps running and
answering. [`mur stop`](cli.md#mur-stop) calls it, then signals the process.

##### Residue items { #residue-items }

The items a `murmur.StopSession` answer lists, and the data parts of a `CancelTask` answer's
`residue` artifact. [`mur cancel`](cli.md#mur-cancel) and [`mur stop`](cli.md#mur-stop) print a
`detached_shell` item as `running:` and a `delegation` item as `ended:`.

| `kind` | Names | Keys |
|---|---|---|
| `detached_shell` | A [demoted shell command](manifest.md#lifecycle-shell-grace-secs), which runs until it finishes or its session ends, which kills it | `work_id` (`wrk_…`), `binary`, `command`, `started_at_ms` |
| `delegation` | A sub-capsule the cancelled task delegated to, which that task ends | `delegation_id` (`dlg_…`), `capsule`, `version`, `child_session_id`, `child_workdir` |

```json
{"kind": "delegation", "delegation_id": "dlg_7b31de", "capsule": "my-worker", "version": "0.1.0",
 "child_session_id": "ses_019ed2af76b57c42912b612f07dd4d51", "child_workdir": ".murmur/children/my-worker-7b31de"}
```

#### `forgetSession` { #forget-session }

A field of a `SendMessage` or `SendStreamingMessage` message that starts a task: it asks the
capsule to drop the harness session the message's context names, so the task starts a new
conversation under the same context id. It lives in the message's `metadata`, under the door
extension's URI:

| Path | Type | Value |
|---|---|---|
| `params.message.metadata["https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1"].forgetSession` | boolean | `true` asks for the forget. `false`, an absent field and an absent URI key ask for nothing |

Other keys in the URI's object, and other `metadata` keys, are ignored. List the door URI in
`message.extensions` too; the door does not read `extensions`.

```bash
curl -s -X POST http://localhost:41873/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "msg-1",
        "role": "ROLE_USER",
        "contextId": "ctx_0193f2a1c4d87e01b2c3d4e5f6a7b8c9",
        "parts": [{"text": "carry on"}],
        "extensions": ["https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1"],
        "metadata": {
          "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1": {"forgetSession": true}
        }
      }
    }
  }'
```

```text
HTTP/1.1 200 OK
content-type: application/json
content-length: 220
connection: close
A2A-Extensions: https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1

{"jsonrpc":"2.0","id":1,"result":{"task":{"contextId":"ctx_0193f2a1c4d87e01b2c3d4e5f6a7b8c9","id":"tsk_019ed5211c827f63a8fe4be623277c55","status":{"state":"TASK_STATE_SUBMITTED","timestamp":"2026-10-13T09:00:00.123Z"}}}}
```

The door refuses the message with `-32602`, starting no task, in this order. The first three come
after the other [message checks](#message) and before the `-32005` file-part check:

| Refusal | `error.message` |
|---|---|
| The URI's value is not an object | `Invalid params: message.metadata["https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1"] must be an object` |
| `forgetSession` is not a boolean | `Invalid params: message.metadata["https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1"].forgetSession must be a boolean` |
| `forgetSession: true` on a message that names a `taskId` | `Invalid params: forgetSession starts a new conversation, so it goes on a message that starts a task, not on one that continues task <id>` |
| `forgetSession: true` to a capsule not on [`inference.transport: process`](manifest.md#transport-process). Checked once the message is read, before it is routed | `forgetSession asks this capsule to forget the harness session this context names, and only a capsule on inference.transport: process has one` |

On `SendStreamingMessage` each refusal is one JSON body, before any stream opens.

On a `transport: process` capsule, the forget:

1. Drops the harness session the context names before the task's first attempt. The
   [`harness_session_forgotten`](observability-schemas.md#harness-session-forgotten) trace record
   names the context, the dropped id and `requested_by: "a2a"` — see
   [what removes an entry](workdir.md#what-removes-an-entry).
2. Starts the harness on a new session: the run's `harness_start` says `session_mode: "new"`.
3. Applies to that one task. The next message in the context resumes the new session.

A context that names no harness session has nothing to drop, and the task starts a new
conversation as it would have without the field. `mur run --context <id> --forget-session` asks for
the same forget from the CLI — see [`--forget-session`](cli.md#run-forget-session). Both are the
answer to [`E-RUN-036`](diagnostics.md#e-run-036).

#### Runtime headers { #runtime-headers }

Murmur's runtimes send each other these headers, and a capsule's runtime stamps them itself. A
client calling the door sends none of them:

| Header | Sent by | Read by | Effect |
|---|---|---|---|
| `x-murmur-task-origin` | A capsule's outbound A2A send, and a sub-capsule's [completion](roost-api.md#how-it-travels) | The door, before the body | The task's [origin](../concepts/access-control.md#task-origin-and-trust-class). On a door whose `peerTasks` is `false`, `peer` is refused `403` [`peer_not_accepted`](#door-authentication) on every method |
| `x-murmur-task-trust` | A capsule's outbound A2A send, beside `x-murmur-task-origin` | The door, before the body | The task's [trust class](../concepts/access-control.md#task-origin-and-trust-class) |
| `x-murmur-delegation-id` | A sub-capsule's completion | The door's completion path | Names the delegation the completion answers. One no task here awaits is refused [`-31002`](#murmur-errors) before its `params` is read |
| `x-murmur-completion-session` | A sub-capsule's completion | The door's completion path | Names the session the completion is for. A misaddressed one is refused [`-31001`](#murmur-errors) before its method or `params` is read |
| `x-murmur-audience` | A capsule redeeming a peer handle | The [peer plane](resource-plane.md#redeem) at `/resources/peer/<handle>` | Names the redeeming capsule, which the handle must have been issued to |
| `x-murmur-handle-id` | The peer plane, on its answer to a redeem | The redeeming client | Names the handle the bytes were served for |

#### Completion errors { #murmur-errors }

A [completion](roost-api.md#how-it-travels) is a `SendMessage` carrying
`x-murmur-task-origin: completion`, which a sub-capsule posts to the session that delegated to it.
The door refuses one with a murmur error:

| Code | `reason` | Answers a completion that | `metadata` |
|---|---|---|---|
| `-31001` | `COMPLETION_MISADDRESSED` | Names, in `x-murmur-completion-session`, a session other than the one running here | `addressedSession`: the session the completion named, `""` when it named none. Never the session running here |
| `-31002` | `COMPLETION_NOT_AWAITED` | Names no delegation in `x-murmur-delegation-id`, names one no task here waits for, or names one this session is ending | `delegationId`, when the completion named one |

Each carries `error.data` with one `google.rpc.ErrorInfo`, shaped as an [A2A error's](#errors), in
the domain `murmur.nexus`. The codes lie outside the range JSON-RPC 2.0 reserves, `-32768` to
`-32000`; A2A calls such codes JSON-RPC custom errors. The child records the refusal's `message`
as its `delivery_error`.

```json
{"jsonrpc": "2.0", "id": 1,
 "error": {"code": -31002,
           "message": "no task in this session is waiting for delegation dlg_…",
           "data": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                     "reason": "COMPLETION_NOT_AWAITED",
                     "domain": "murmur.nexus",
                     "metadata": {"delegationId": "dlg_…"}}]}}
```

A completion sent with any method but `SendMessage` is answered `-32601`.

#### `GetTask` { #tasks-get }

`GetTask` answers the task `params.id` names, running or finished, as an A2A `Task`. `params.id`
is required: a `GetTask` without one, or with `""`, is answered `-32602` `GetTask requires an id`.
`historyLength` is accepted and ignored; a task carries no `history`.

| Key | Present |
|---|---|
| `id`, `contextId`, `status.state` | Always |
| `status.message` | On a terminal task — `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_CANCELED` or `TASK_STATE_REJECTED` — whose final status said something: a `ROLE_AGENT` message with one text part, as the task's [terminal status](streaming-protocol.md#one-final-status) on a stream carries it |
| `artifacts: [{"artifactId": "response", …}]` | On a `TASK_STATE_COMPLETED` task that produced a response: its answer, as one text part |
| `artifacts: [{"artifactId": "prompt", …}]` | On a `TASK_STATE_INPUT_REQUIRED` task: the question it is waiting on |
| `metadata.murmur` | On a terminal task with something to say about answers it lacks — see [No-answer metadata](#no-answer-metadata) |

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_COMPLETED",
            "message": {"messageId": "msg_tsk_…_status", "contextId": "ctx_…", "taskId": "tsk_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "task completed", "mediaType": "text/plain"}]},
            "timestamp": "2026-10-13T09:00:00.123Z"},
 "artifacts": [{"artifactId": "response", "name": "response",
                "parts": [{"text": "four", "mediaType": "text/plain"}]}]}
```

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_INPUT_REQUIRED", "timestamp": "2026-10-13T09:00:00.123Z"},
 "artifacts": [{"artifactId": "prompt", "name": "prompt",
                "parts": [{"text": "Which branch?", "mediaType": "text/plain"}]}]}
```

`CancelTask` on a live task answers the task in `TASK_STATE_CANCELED`, with a `residue` artifact
naming what the session still runs, one data part per [residue item](#residue-items), when anything
is:

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_CANCELED", "timestamp": "2026-10-13T09:00:00.123Z"},
 "artifacts": [{"artifactId": "residue", "name": "residue",
                "parts": [{"data": {"kind": "detached_shell", "work_id": "wrk_…",
                                    "binary": "sleep", "command": "sleep 30",
                                    "started_at_ms": 1760000000000},
                           "mediaType": "application/json"}]}]}
```

`GetTask` and `CancelTask` answer only a task the caller [sees](#task-access). Any other id — another
credential's task, the operator's, or one the session never held — is answered
`-32001 Task not found`.

A `SendMessage` the door has no room for answers a `TASK_STATE_REJECTED` task whose
`status.message` says why: `task rejected: capsule is busy`, or
`task rejected: the session is closing`.

##### No-answer metadata { #no-answer-metadata }

A formation member's task says, in `metadata.murmur`, which answers it lacks. `metadata` is absent
when neither key is present.

| Key | Type | Present |
|---|---|---|
| `noAnswer` | `true` | On a `TASK_STATE_FAILED` task the member ended with [`end-without-answer`](runtime-provided-tools.md#end-without-answer). Absent otherwise |
| `noAnswerBelow` | array of `{"member": "<roster name>", "status": "<status>"}` | On a terminal task when a member it called gave it no answer: each such member, then each member those reported below them, each once, at most 8. `status` is a [`member_call`](observability-schemas.md#member-call) status other than `completed`. Absent when empty |

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_FAILED",
            "message": {"messageId": "msg_tsk_…_status", "contextId": "ctx_…", "taskId": "tsk_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "q gave no answer", "mediaType": "text/plain"}]},
            "timestamp": "2026-10-13T09:00:00.123Z"},
 "metadata": {"murmur": {"noAnswer": true,
                         "noAnswerBelow": [{"member": "q", "status": "timed_out"}]}}}
```

A completed task with `noAnswerBelow` keeps its `response` artifact. A caller that is not a
formation member can ignore `metadata`: the task reads as an ordinary `TASK_STATE_FAILED` or
`TASK_STATE_COMPLETED` one.

#### `ListTasks` { #list-tasks }

`ListTasks` answers one page of the tasks the caller [sees](#task-access), the most recently changed
first. Its `params` are a `ListTasksRequest`, read in this order:

| Param | Accepted | Refused |
|---|---|---|
| `contextId` | A string. `""` or absent: every context | Not a string: `-32602` |
| `status` | A [`TASK_STATE_*`](#task-states) name. `TASK_STATE_UNSPECIFIED` or absent: every state. `TASK_STATE_AUTH_REQUIRED` matches no task | Anything else: `-32602` |
| `pageSize` | An integer from `1`. Absent: `50`. Above `100`: `100` | Below `1`, or not an integer: `-32602` |
| `pageToken` | `""` or absent for the first page, or a `nextPageToken` this door answered | Anything else: `-32602` |
| `statusTimestampAfter` | An RFC 3339 timestamp, in any offset: only the tasks whose [`status.timestamp`](#status-timestamp) is at or after it. `null` or absent: every task | Anything else: `-32602` `statusTimestampAfter must be an RFC 3339 timestamp, such as 2026-10-13T09:00:00Z` |
| `includeArtifacts` | A boolean. Absent: `false` | Not a boolean: `-32602` |
| `historyLength` | Any value, ignored: a task carries no `history` | — |

Other members are ignored. The first parameter refused answers, naming it.

The answer is a `ListTasksResponse`, whose four members are always present:

| Member | Value |
|---|---|
| `tasks` | The page. Each task is exactly what `GetTask` answers for it, without `artifacts` unless `includeArtifacts` is `true`; then `artifacts` is the task's artifacts, or `[]` |
| `nextPageToken` | The `pageToken` of the next page. `""` on the last page |
| `pageSize` | The page size used |
| `totalSize` | How many tasks the caller sees that match the filters, across every page |

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"tasks": [{"id": "tsk_…", "contextId": "ctx_…",
                       "status": {"state": "TASK_STATE_WORKING",
                                  "timestamp": "2026-10-13T09:00:04.512Z"}},
                      {"id": "tsk_…", "contextId": "ctx_…",
                       "status": {"state": "TASK_STATE_COMPLETED",
                                  "message": {"messageId": "msg_tsk_…_status", "contextId": "ctx_…",
                                              "taskId": "tsk_…", "role": "ROLE_AGENT",
                                              "parts": [{"text": "task completed",
                                                         "mediaType": "text/plain"}]},
                                  "timestamp": "2026-10-13T09:00:00.123Z"}}],
            "nextPageToken": "MTI6dHNrXzAxYTE…",
            "pageSize": 2,
            "totalSize": 5}}
```

A task moves to the front of the order each time its state changes or its ending is recorded. A
`nextPageToken` names a position in that order and nothing else: the filters and
[who sees a task](#task-access) apply afresh to every page, so a token passed to another caller
shows that caller only its own tasks. A walk returns every matching task once while no matching task
changes state. A task that changes during a walk moves ahead of the cursor, and a fresh listing
shows it first.

#### `SubscribeToTask` { #subscribe-to-task }

`SubscribeToTask` streams the rest of one task the caller [sees](#task-access) as
`text/event-stream`, in the [events](streaming-protocol.md#standard-events)
`SendStreamingMessage` writes. `params.id` names the task:

| Request | Answer |
|---|---|
| No `id`, or `""` | `-32602` `SubscribeToTask requires an id` |
| A task the session never held, or one the caller does not see | `-32001`, `metadata.taskId` |
| A task that has ended | `-32004`, `metadata.taskId` and `state` |
| A door that starts no task | `-32004` |
| A live task | The stream |

Each refusal is one JSON body; no stream opens. The stream opens with the task as `GetTask` answers
it at that moment, then carries the task's later events, and ends after its terminal status. It
stays open through `TASK_STATE_INPUT_REQUIRED`: the reply arrives on a `SendMessage` naming the
task, and the stream carries the task's resumed `TASK_STATE_WORKING`. `Last-Event-ID` and
`A2A-Extensions` work as on `SendStreamingMessage` — see
[Streaming Protocol](streaming-protocol.md#subscribe-to-task).

```bash
curl -N -X POST http://localhost:41873/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"jsonrpc":"2.0","id":"sub-1","method":"SubscribeToTask","params":{"id":"tsk_…"}}'
```

### Capsule extension { #murmur-capsule-v1 }

URI: `https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1`

This extension is extended-card material: the session id and what the capsule may do. Nothing of
it appears anywhere else on the card.

| Door | Where the extension is |
|---|---|
| Public | The public card |
| Authenticated | The [extended card](#extended-card) only |

| Key | Type | Value |
|---|---|---|
| `params.sessionId` | string | The session answering this address. Compare it with the session you expect before sending a task |
| `params.tools` | array of strings | Names of the installed artifacts the model can call |
| `params.shell` | boolean | `true` when `capabilities.shell.allow` lists at least one command |
| `params.network` | boolean | `true` when `capabilities.network.allow` lists at least one destination |
| `params.planes` | array of strings | The HTTP planes that serve content — see [planes](#capsule-planes) |

#### Planes { #capsule-planes }

A plane is listed only when the manifest declares it. An undeclared plane answers every request
with a refusal. Planes appear in this order:

| Plane | Listed when | Serves |
|---|---|---|
| `files` | [`exports.files`](manifest.md#field-exports) is declared | The [operator plane](resource-plane.md#operator-plane) under `/resources/files` |
| `peer_files` | [`exports.peer_files`](manifest.md#field-exports) is declared | The [peer plane](resource-plane.md#peer-plane) under `/resources/peer/<handle>` |

### Stream extension { #stream-extension }

URI: `https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1`

`params.frames` lists the murmur frames a connection to the door can receive. Activation, the
frames and the events that carry them are under
[The stream extension](streaming-protocol.md#murmur-stream-v1).

## A card with no `supportedInterfaces` { #no-supported-interfaces }

A card without `supportedInterfaces` comes from a runtime at v0.4.0 or earlier, which serves the
[previous card](#migrating). A capsule serving it answers the method names of its own version, and:

| Reader | Result |
|---|---|
| [`mur ps`](cli.md#mur-ps), [`mur stop`](cli.md#mur-stop) | Lists the capsule as unreachable: its card names no session. The record is kept, never pruned |
| [Peer handle](resource-plane.md#audience) minting for the capsule | Fails with `peer_unreachable`: its card has no `JSONRPC` interface |

Both hold until that capsule ends.

## Migrating from the previous card { #migrating }

Where each key of the card served by v0.4.0 and earlier is on this card:

| Previous key | On this card |
|---|---|
| `name` | `name` |
| `version` | `version` |
| `url` (`localhost:41873`) | `supportedInterfaces[0].url`, with the scheme (`http://localhost:41873`) |
| `session_id` | Capsule extension `params.sessionId` |
| `capabilities.tools` | Capsule extension `params.tools` |
| `capabilities.shell` | Capsule extension `params.shell` |
| `capabilities.network` | Capsule extension `params.network` |
| `capabilities.streaming` | `capabilities.streaming`, derived the same way |
| `capabilities.cancellation` | Removed. It was `true` on every card; the door extension's `params.methods` lists `CancelTask` |
| `serves.methods` | Door extension `params.methods`, same values and order |
| `serves.planes` | Capsule extension `params.planes`, same values and order |

Find an extension by its `uri` in `capabilities.extensions`, not by its position.

## Migrating from A2A 0.3 { #migrating-a2a-0-3 }

The door speaks A2A v1.0 only — see [A2A v1.0 conformance](#conformance). A client written for a
door that declared `protocolVersion` `0.3` changes these:

| A2A 0.3 door | A2A v1.0 door |
|---|---|
| No `A2A-Version` header | `A2A-Version: 1.0` on every JSON-RPC request. A request without it is answered [`-32009`](#a2a-version) |
| `message/send` | `SendMessage` |
| `message/stream` | `SendStreamingMessage` |
| `tasks/get` | `GetTask` |
| `tasks/cancel` | `CancelTask` |
| `agent/getAuthenticatedExtendedCard`, answering an A2A 0.3 `AgentCard` | `GetExtendedAgentCard`, answering an A2A v1.0 `AgentCard` — see [The extended card](#extended-card). A public door answers it `-32004`, where a 0.3 door answered `-32007` |
| `supportedInterfaces[0].protocolVersion` `0.3` | `1.0` |
| `tasks/cancel` on a task that had already ended answers the task | `CancelTask` answers [`-32002`](#errors) and leaves the task unchanged |
| A completion refused with `-32004` | [`-31001` or `-31002`](#murmur-errors). `-32004` is `UnsupportedOperationError` only |
| Errors without `data` | Every A2A error carries an [`ErrorInfo`](#errors) |
| Credential scopes `message/send`, `message/stream`, `tasks/get`, `tasks/cancel` | `SendMessage`, `SendStreamingMessage`, `GetTask`, `CancelTask`. A manifest listing a 0.3 name is refused — see [`network.authentication.credentials`](manifest.md#field-network-authentication) |
| Task states `submitted`, `working`, `input-required`, `completed`, `failed`, `rejected`, `canceled` | [`TASK_STATE_SUBMITTED`, `TASK_STATE_WORKING`, `TASK_STATE_INPUT_REQUIRED`, `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_REJECTED`, `TASK_STATE_CANCELED`](#task-states) |
| `role: "user"`, `role: "agent"` | `ROLE_USER`, `ROLE_AGENT`. A message with `role: "user"` is refused |
| `message/send` answering a bare task | `SendMessage` answering [`{"task": …}`](#send-message-response) |
| A reply reaching whichever task waited for input | A reply names its task in [`taskId`](#task-id) |
| A part with `kind`, and a file or data part skipped | A part sets one of `text`, `raw`, `url` or `data`. A data part is [read](#parts); a file part is refused with `-32005` |
| Artifacts without an id | Every artifact carries an [`artifactId`](#send-message-response) |
| `tasks/get` without an `id` answering the active task | `GetTask` requires an [`id`](#tasks-get) |
| `stream/watch` | [`murmur.WatchSession`](#watch-session) |
| `session/stop` | [`murmur.StopSession`](#stop-session) |
| Credential scopes `stream/watch`, `session/stop` | `murmur.WatchSession`, `murmur.StopSession`. A manifest listing an old name is refused |
| Header `x-murmur-forget-session: true` | [`forgetSession: true`](#forget-session) in the message's `metadata`, under the door extension's URI. The header is not read |

Every old method name in the table is answered `-32601` `Method not found`: the door serves no alias.
The other `x-murmur-*` headers are unchanged — see [Runtime headers](#runtime-headers).
