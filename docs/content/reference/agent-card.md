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
}
```

What murmur adds to the standard card sits in three [extensions](#extensions):

| Extension | Answers | Derived from |
|---|---|---|
| [Door](#murmur-door-v1) | What does this listener answer, and does it serve other capsules? | The methods `POST /` dispatches, and [`exports.peer_tasks`](manifest.md#field-exports-peer-tasks) |
| [Capsule](#murmur-capsule-v1) | Which session is this, and what may the capsule do? | The session, the installed artifacts, `capabilities.*` and the declared `exports` |
| [Stream](#stream-extension) | Which frames can its stream send? | The streaming methods the door serves and the inference transport |

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
| `capabilities.streaming` | boolean | `true` when the [door extension](#murmur-door-v1) lists `SendStreamingMessage` and the capsule's inference transport streams text |
| `capabilities.pushNotifications` | boolean | `false` |
| `capabilities.extendedAgentCard` | boolean | `false` on a public door, where every caller gets this card. `true` on an [authenticated door](#security) |
| `capabilities.extensions` | array of objects | The [door extension](#murmur-door-v1), the [capsule extension](#murmur-capsule-v1), then the [stream extension](#stream-extension). An authenticated door's public card carries the door extension, then the stream extension |

`streaming` is the door's answer and the transport's together: a door that answers
`SendStreamingMessage` over a transport that streams nothing lists the method on the door extension
and reports `false` here.

| Transport | `streaming` |
|---|---|
| `http` | `true` when `SendStreamingMessage` is served |
| `process` | `true` when `SendStreamingMessage` is served and the [process driver](manifest.md#process-driver) reports `streams-text` |

## Security { #security }

Who may call the door is set by [`network.authentication`](manifest.md#field-network-authentication).

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
| `stream/watch` | `stream/watch` |
| `GetTask` | [`GetTask`](#tasks-get) |
| `CancelTask` | `CancelTask` |
| `session/stop` | `session/stop` |
| `resources/files` | The [operator plane](resource-plane.md#operator-plane) |

`GetExtendedAgentCard` needs no scope: every valid token may read the extended card.

Every authenticated caller shares the session's one task and context space. A scope limits which
methods a token reaches, not which tasks: a credential holding `SendStreamingMessage`, `GetTask` or
`stream/watch` sees every task on the session. The one exception is a
[formation token](roster.md#formation-token): it reaches [`GetTask`](#tasks-get) and
`CancelTask` only for the tasks its member submitted, and does not reach `SendStreamingMessage`.

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
    with `-32004` instead.
11. The scope of the method, on a door declaring `network.authentication`.
12. On `SendMessage` and `SendStreamingMessage`, an
    [`x-murmur-forget-session: true`](#request-headers) the capsule cannot act on: `-32602`.
13. `params` that is present and is not an object: `-32602`.
14. The method itself, whose errors are listed under [Errors](#errors).

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

Every method is negotiated: the A2A methods, `stream/watch`, `session/stop` and completions alike.
The refusal is one JSON body, also on `SendStreamingMessage` and `stream/watch`, and nothing is
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
| `-32602` | Names the parameter | `params` that is not an object, a message the door cannot [read in full](#message), a message naming a waiting task in another `contextId`, a `GetTask` or `CancelTask` with no `id`, or an `x-murmur-forget-session` the capsule cannot act on |
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
| `-32001` | `TaskNotFoundError` | `TASK_NOT_FOUND` | On `GetTask`, `CancelTask`, and a message whose `taskId` names an id this session never held or a [formation token](roster.md#formation-token) does not reach | `taskId` |
| `-32002` | `TaskNotCancelableError` | `TASK_NOT_CANCELABLE` | On `CancelTask` for a task that had already ended. The task is left unchanged | `taskId`; `state`, the state it ended in, spelled as `GetTask` spells it: `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_REJECTED` or `TASK_STATE_CANCELED` |
| `-32003` | `PushNotificationNotSupportedError` | `PUSH_NOTIFICATION_NOT_SUPPORTED` | Never | — |
| `-32004` | `UnsupportedOperationError` | `UNSUPPORTED_OPERATION` | On `GetExtendedAgentCard` to a public door, with `{}`. On a message whose `taskId` names a task that is not waiting for input — see [Continuing a task](#task-id) | `taskId` and `state`, on a message naming a task |
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
| `TASK_STATE_CANCELED` | Stopped by `CancelTask` or `session/stop` | `canceled` |

The door never produces `TASK_STATE_AUTH_REQUIRED`: it authenticates a request before any task
exists, so no task waits for authentication.

Murmur's word is the spelling of every murmur interface: the [stream frames](streaming-protocol.md),
[`murmur:message/send`](wit-interfaces.md#message-send)'s `task-result.state`, `mur` output and the
trace.

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
| `metadata`, `extensions` | no | Ignored |

The checks run in this order, and the first failure answers:

1. `params.message` is an object. A bare message as `params` is refused.
2. `messageId`, `role`, then `parts`, as in the table.
3. Every part sets exactly one of `text`, `raw`, `url` and `data`.
4. `text`, `raw`, `url`, `mediaType` and `filename` are strings where present.
5. `taskId` and `contextId`, then `referenceTaskIds`, as in the table.
6. The first `raw` or `url` part: [`-32005`](#errors).

Steps 1 to 5 answer `-32602`. Every refusal comes before the door looks up a task or starts one: a
refused message starts no task, writes no trace record, reaches no model and delivers nothing to a
waiting task. Unknown fields are ignored.

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
| A task this session never held, or one a formation token does not reach | [`-32001`](#errors) |
| A task that has ended | [`-32004`](#errors), naming its state. Send a new message without `taskId`, in the same `contextId`, to start a new task |
| A `TASK_STATE_SUBMITTED` or `TASK_STATE_WORKING` task | `-32004`, naming its state. A task takes a message only while it waits in `TASK_STATE_INPUT_REQUIRED` |
| A `TASK_STATE_INPUT_REQUIRED` task, with a `contextId` other than the task's | `-32602` |
| A `TASK_STATE_INPUT_REQUIRED` task | The message's text is the task's reply, and the task is answered in `TASK_STATE_WORKING`. The door's capacity is not consulted |

A message that names no task never reaches a waiting one. `SendStreamingMessage` answers each
refusal as one JSON body before any event, except a new task refused for room, which is a `rejected`
[status frame](streaming-protocol.md). A stream that continues a task ends on that task's final
status.

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
| Never held, or one a formation token does not reach | `- <id>: not a task this capsule holds` |

States are [murmur's words](#task-states). A message without `referenceTaskIds` gets no block.

#### Answers { #send-message-response }

`SendMessage` answers a `SendMessageResponse`: `{"task": <Task>}` for a new, rejected or continued
task, and `{"message": <Message>}` for a [completion](#murmur-errors) the door accepted:

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"task": {"id": "tsk_…", "contextId": "ctx_…",
                     "status": {"state": "TASK_STATE_SUBMITTED"}}}}
```

```json
{"jsonrpc": "2.0", "id": 1,
 "result": {"message": {"messageId": "msg_dlg_…_received", "contextId": "ctx_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"data": {"delegation_id": "dlg_…", "received": true},
                                   "mediaType": "application/json"}]}}}
```

`SendMessage` answers as soon as the task is accepted, whatever `configuration.returnImmediately`
says: A2A v1.0 has a request without it wait for a terminal or interrupted state. Poll
[`GetTask`](#tasks-get), or read the task's frames with `SendStreamingMessage`. The door ignores
`configuration`, push notification configs and `tenant`.

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
can call the capsule without understanding any of them. The door does not require an
`A2A-Extensions` header to activate them.

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
| `params.methods` | array of strings | The JSON-RPC methods `POST /` answers, standard and murmur alike |
| `params.peerTasks` | boolean | Whether the door serves a request another capsule's runtime sends, stamped `x-murmur-task-origin: peer`: the capsule's [`exports.peer_tasks.accept`](manifest.md#field-exports-peer-tasks), `false` when the block is absent. Always present, on the public and the extended card. When `false`, every such request is answered `403` [`peer_not_accepted`](#door-authentication) |

A method is listed exactly when `POST /` answers it with something other than `-32601 Method not
found`. Method names match exactly: case and surrounding whitespace are significant. `stream/watch`
and `session/stop` are murmur methods, not A2A methods. Methods appear in this order:

| Method | Answers | Listed when |
|---|---|---|
| `SendMessage` | Starts a task, or delivers input to a task waiting for it, and returns the task | `lifecycle.task_acceptance` is `single` or `queue` |
| `SendStreamingMessage` | Starts a task and streams its events as `text/event-stream` | `lifecycle.task_acceptance` is `single` or `queue` |
| `stream/watch` | Streams this session's events as an observer, without starting a task | Always |
| `GetTask` | Returns the task named by `params.id` | Always |
| `CancelTask` | Cancels the task named by `params.id`. A task that had already ended is left unchanged and answered [`-32002`](#errors) | Always |
| `session/stop` | Cancels every live task and reports what the session leaves running | Always |
| `GetExtendedAgentCard` | Returns the [extended card](#extended-card) | The capsule declares [`network.authentication`](manifest.md#field-network-authentication) |

See [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance). Under `none`, `POST /`
answers `SendMessage` and `SendStreamingMessage` with `-32601`, so neither is listed,
`capabilities.streaming` is `false` and `skills` is `[]`.

| Door | `params.methods` |
|---|---|
| Public | `SendMessage`, `SendStreamingMessage`, `stream/watch`, `GetTask`, `CancelTask`, `session/stop` |
| Authenticated | The public door's six, then `GetExtendedAgentCard` |
| Public, `task_acceptance: none` | `stream/watch`, `GetTask`, `CancelTask`, `session/stop` |
| Authenticated, `task_acceptance: none` | `stream/watch`, `GetTask`, `CancelTask`, `session/stop`, `GetExtendedAgentCard` |

The card endpoint itself is not listed.

#### Request headers { #request-headers }

The door reads `x-murmur-*` request headers alongside the JSON-RPC body and the
[`A2A-Version`](#a2a-version) header. One of them changes what the methods that start a turn do:

| Header | Read on | Effect |
|---|---|---|
| `x-murmur-forget-session` | `SendMessage`, `SendStreamingMessage` | `true` drops the harness session this request's context names before the turn, so the turn starts a new conversation under the same context id |

Only the value `true` asks for it; every other value, and the header's absence, are the same
request. A capsule on any transport but
[`inference.transport: process`](manifest.md#transport-process) keeps no harness session, and
answers the header with `-32602` without starting a task — see
[`E-RUN-039`](diagnostics.md#e-run-039). What the forget does, and what it records, is
[what removes an entry](workdir.md#what-removes-an-entry).

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
| `status.message` | On a terminal task — `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, `TASK_STATE_CANCELED` or `TASK_STATE_REJECTED` — whose final status said something: a `ROLE_AGENT` message with one text part, the same text as the task's final [`status` frame](streaming-protocol.md#one-final-status) |
| `artifacts: [{"artifactId": "response", …}]` | On a `TASK_STATE_COMPLETED` task that produced a response: its answer, as one text part |
| `artifacts: [{"artifactId": "prompt", …}]` | On a `TASK_STATE_INPUT_REQUIRED` task: the question it is waiting on |
| `metadata.murmur` | On a terminal task with something to say about answers it lacks — see [No-answer metadata](#no-answer-metadata) |

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_COMPLETED",
            "message": {"messageId": "msg_tsk_…_status", "contextId": "ctx_…", "taskId": "tsk_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "session ended", "mediaType": "text/plain"}]}},
 "artifacts": [{"artifactId": "response", "name": "response",
                "parts": [{"text": "four", "mediaType": "text/plain"}]}]}
```

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_INPUT_REQUIRED"},
 "artifacts": [{"artifactId": "prompt", "name": "prompt",
                "parts": [{"text": "Which branch?", "mediaType": "text/plain"}]}]}
```

`CancelTask` on a live task answers the task in `TASK_STATE_CANCELED`, with a `residue` artifact
naming what the session still runs, one data part per item, when anything is:

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_CANCELED"},
 "artifacts": [{"artifactId": "residue", "name": "residue",
                "parts": [{"data": {"kind": "detached_shell", "work_id": "wrk_…",
                                    "binary": "sleep", "command": "sleep 30",
                                    "started_at_ms": 1760000000000},
                           "mediaType": "application/json"}]}]}
```

On a [formation token](roster.md#formation-token), `GetTask` answers only the tasks the calling
member submitted. Any other id — another member's task, the operator's, or one the session never
held — is answered `-32001 Task not found`. Every other credential reads every task.

A `SendMessage` the door has no room for answers a `TASK_STATE_REJECTED` task whose
`status.message` says why: `task rejected: capsule is busy`, or
`task rejected: the session is closing`.

##### No-answer metadata { #no-answer-metadata }

A formation member's task says, in `metadata.murmur`, which answers it lacks. `metadata` is absent
when neither key is present.

| Key | Type | Present |
|---|---|---|
| `noAnswer` | `true` | On a `failed` task the member ended with [`end-without-answer`](runtime-provided-tools.md#end-without-answer). Absent otherwise |
| `noAnswerBelow` | array of `{"member": "<roster name>", "status": "<status>"}` | On a terminal task when a member it called gave it no answer: each such member, then each member those reported below them, each once, at most 8. `status` is a [`member_call`](observability-schemas.md#member-call) status other than `completed`. Absent when empty |

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "TASK_STATE_FAILED",
            "message": {"messageId": "msg_tsk_…_status", "contextId": "ctx_…", "taskId": "tsk_…",
                        "role": "ROLE_AGENT",
                        "parts": [{"text": "q gave no answer", "mediaType": "text/plain"}]}},
 "metadata": {"murmur": {"noAnswer": true,
                         "noAnswerBelow": [{"member": "q", "status": "timed_out"}]}}}
```

A completed task with `noAnswerBelow` keeps its `response` artifact. A caller that is not a
formation member can ignore `metadata`: the task reads as an ordinary `failed` or `completed` one.

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

`params.frames` lists every frame type the capsule's `SendStreamingMessage` and `stream/watch`
connections can write. Its key and the frame list are on the streaming protocol page, under
[Frame vocabulary](streaming-protocol.md#murmur-stream-v1). It is on the public card of every door,
authenticated or not.

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

The door speaks A2A v1.0 only. A client written for a door that declared `protocolVersion` `0.3`
changes these:

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
| Task states `submitted`, `working`, `input-required`, `completed`, `failed`, `rejected`, `canceled` | [`TASK_STATE_*`](#task-states) |
| `role: "user"`, `role: "agent"` | `ROLE_USER`, `ROLE_AGENT`. A message with `role: "user"` is refused |
| `message/send` answering a bare task | `SendMessage` answering [`{"task": …}`](#send-message-response) |
| A reply reaching whichever task waited for input | A reply names its task in [`taskId`](#task-id) |
| A part with `kind`, and a file or data part skipped | A part sets one of `text`, `raw`, `url` or `data`. A data part is [read](#parts); a file part is refused with `-32005` |
| Artifacts without an id | Every artifact carries an [`artifactId`](#send-message-response) |
| `tasks/get` without an `id` answering the active task | `GetTask` requires an [`id`](#tasks-get) |

The 0.3 method names are answered `-32601` `Method not found`. `stream/watch`, `session/stop` and
the `x-murmur-*` headers keep their names.
