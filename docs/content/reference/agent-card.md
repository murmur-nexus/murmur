# Agent Card

Every capsule serves an [A2A](https://a2a-protocol.org) v1.0 `AgentCard` at
`GET /.well-known/agent-card.json` on its HTTP listener. The card parses as `lf.a2a.v1.AgentCard`,
as defined by [`a2a.proto` at `v1.0.0`](https://github.com/a2aproject/A2A/blob/v1.0.0/specification/a2a.proto),
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
    { "url": "http://localhost:41873", "protocolBinding": "JSONRPC", "protocolVersion": "0.3" }
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
          "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop"],
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
        "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
        "required": false,
        "params": {
          "frames": ["status", "artifact", "text", "thinking", "tool-call-started", "tool-call-progress", "gap", "lagged", "connection-ack", "capsule-closed", "error"]
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
| `defaultInputModes` | array of strings | `["text/plain"]`. The door reads text parts only |
| `defaultOutputModes` | array of strings | `["text/plain"]`. The door writes text parts only |
| `skills` | array of objects | See [`skills`](#skills). Empty under `lifecycle.task_acceptance: none` |

The card carries no `provider`, `documentationUrl`, `iconUrl` or `signatures`.

## `supportedInterfaces` { #supported-interfaces }

One interface: the JSON-RPC door at `POST /`.

| Key | Value |
|---|---|
| `url` | `http://<host:port>` — the address this capsule's listener answers on. The door speaks plain HTTP |
| `protocolBinding` | `JSONRPC` |
| `protocolVersion` | `0.3` |

The card's shape is A2A v1.0; the interface is declared at A2A `0.3` because the door answers the
0.3 method names and task states:

| The door answers | A2A v1.0 name, answered with `-32601 Method not found` |
|---|---|
| `message/send` | `SendMessage` |
| `message/stream` | `SendStreamingMessage` |
| `tasks/get` | `GetTask` |
| `tasks/cancel` | `CancelTask` |

Task states are kebab-case (`input-required`), as in A2A 0.3.

## `capabilities` { #capabilities }

| Key | Type | Value |
|---|---|---|
| `capabilities.streaming` | boolean | `true` when the [door extension](#murmur-door-v1) lists `message/stream` and the capsule's inference transport streams text |
| `capabilities.pushNotifications` | boolean | `false` |
| `capabilities.extendedAgentCard` | boolean | `false` on a public door, where every caller gets this card. `true` on an [authenticated door](#security) |
| `capabilities.extensions` | array of objects | The [door extension](#murmur-door-v1), the [capsule extension](#murmur-capsule-v1), then the [stream extension](#stream-extension). An authenticated door's public card carries the door extension, then the stream extension |

`streaming` is the door's answer and the transport's together: a door that answers
`message/stream` over a transport that streams nothing lists the method on the door extension and
reports `false` here.

| Transport | `streaming` |
|---|---|
| `http` | `true` when `message/stream` is served |
| `process` | `true` when `message/stream` is served and the [process driver](manifest.md#process-driver) reports `streams-text` |

## Security { #security }

Who may call the door is set by [`network.authentication`](manifest.md#field-network-authentication).

| Manifest | Door | `securitySchemes` | `securityRequirements` | Extended card |
|---|---|---|---|---|
| No `network.authentication` | Public: answers every caller that reaches the port, and ignores `Authorization` | `{}` | `[]` | None |
| `network.authentication.scheme: bearer` | Authenticated: every request but the public card presents a token | `bearer`, an HTTP `Bearer` scheme | Any valid token | The public card plus the capsule extension, in A2A 0.3 shape |

A public door's card is exactly the card at the top of this page.

### Tokens { #tokens }

An authenticated capsule's runtime mints its tokens at launch, and [`mur run`](cli.md#mur-run)
prints them. A token is valid until the session ends; a restart or a `--resume` mints new ones.

| Credential | Scopes | Minted |
|---|---|---|
| `operator` | Every scope | Always |
| Each name under `network.authentication.credentials` | The scopes that credential lists | One per declared credential |

A scope is a door method's name, or `resources/files` for the
[operator plane](resource-plane.md#operator-plane). `agent/getAuthenticatedExtendedCard` needs no
scope: every valid token may read the extended card.

Every authenticated caller shares the session's one task and context space. A scope limits which
methods a token reaches, not which tasks: a credential holding `message/stream`, `tasks/get` or
`stream/watch` sees every task on the session. The one exception is a
[formation token](roster.md#formation-token): it reaches [`tasks/get`](#tasks-get) only for the
tasks its member submitted, and does not reach `message/stream`.

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
4. The scope of a method the door serves, or of the operator plane, on a door declaring
   `network.authentication`.
5. The request itself, whose errors — `-32001 Task not found`, `-32601`, `-32602`, `-32004`, HTTP
   `404` — reach an admitted caller unchanged.

A refusal at steps 2 to 4 is an HTTP status and a JSON body, never a JSON-RPC envelope:

| Case | Status | `www-authenticate` | Body `error` |
|---|---|---|---|
| No `Authorization` header | `401` | `Bearer realm="<capsule name>"` | `unauthenticated` |
| An `Authorization` header that is not `Bearer <token>`, a token this session did not mint, or more than one `Authorization` header | `401` | `Bearer realm="<capsule name>", error="invalid_token"` | `invalid_token` |
| A peer-origin request to a capsule that does not accept peer tasks | `403` | None | `peer_not_accepted` |
| A valid token whose credential lacks the scope | `403` | `Bearer realm="<capsule name>", error="insufficient_scope", scope="<scope>"` | `insufficient_scope` |

The body is `{"error": "<code>", "message": "<sentence>"}`. The `insufficient_scope` message names
the credential and the scope it lacks: `credential 'watcher' does not reach message/send`. The
`peer_not_accepted` response is byte-identical for every request it refuses. The scheme name
`Bearer` matches in any case; the token matches exactly.

The door speaks plain HTTP, so a token sent across a network travels in clear text. Put a TLS
terminator in front of a door that is reached off the host.

### The public card of an authenticated door { #authenticated-public-card }

`my-agent` from the top of this page, declaring `network.authentication`:

```json
{
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
        "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.",
        "required": false,
        "params": {
          "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop", "agent/getAuthenticatedExtendedCard"],
          "peerTasks": false
        }
      },
      {
        "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
        "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.",
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
}
```

It differs from a public door's card in these keys:

| Key | Value |
|---|---|
| `capabilities.extendedAgentCard` | `true` |
| `capabilities.extensions` | The door extension, then the stream extension. The [capsule extension](#murmur-capsule-v1) is on the extended card |
| Door extension `params.methods` | Gains `agent/getAuthenticatedExtendedCard` |
| `securitySchemes` | `bearer`: an `httpAuthSecurityScheme` with scheme `Bearer` |
| `securityRequirements` | `[{"schemes": {"bearer": {"list": []}}}]`: any valid token |
| `task` skill `securityRequirements` | One alternative per served task-starting method, `message/send` then `message/stream`, each naming the scope it needs |

### The extended card { #extended-card }

`agent/getAuthenticatedExtendedCard` returns the extended card as its JSON-RPC `result`, to any
valid token. The method answers on the door's A2A 0.3 interface, so the result is an A2A 0.3
`AgentCard`: an A2A client that picks the `0.3` interface validates it as one. It is the
authenticated door's v1.0 card with the capsule extension kept, converted to 0.3 field names:

```json
{
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
      { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1", "description": "Every JSON-RPC method this door answers, including the murmur methods stream/watch and session/stop, which are not A2A methods, and whether it accepts tasks from peer capsules.", "required": false,
        "params": { "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop", "agent/getAuthenticatedExtendedCard"], "peerTasks": false } },
      { "uri": "https://docs.murmur.nexus/reference/agent-card/#murmur-capsule-v1", "description": "The session answering this address and what the capsule may do. Served only to authenticated callers once the door authenticates.", "required": false,
        "params": { "sessionId": "ses_019f01a940ce7761854e768ecbe3d399", "tools": ["bash"], "shell": true, "network": true, "planes": ["files"] } },
      { "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1", "description": "Every server-sent event type this capsule's message/stream and stream/watch connections can write. Only status and artifact correspond to A2A events; the others are murmur frames.", "required": false,
        "params": { "frames": ["status", "artifact", "text", "thinking", "tool-call-started", "tool-call-progress", "gap", "lagged", "connection-ack", "capsule-closed", "error"] } }
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
}
```

| 0.3 key | From the v1.0 card |
|---|---|
| `url` | `supportedInterfaces[0].url` |
| `preferredTransport` | `JSONRPC` |
| `protocolVersion` | `0.3.0` |
| `capabilities` | `capabilities`, without `extendedAgentCard` |
| `supportsAuthenticatedExtendedCard` | `capabilities.extendedAgentCard` |
| `securitySchemes.bearer` | `{"type": "http", "scheme": …, "description": …}` |
| `security`, and each skill's `security` | `securityRequirements`, each `{"schemes": {name: {"list": […]}}}` as `{name: […]}` |

The capsule extension's `params.sessionId` is at the same path in both shapes.

A public door has no extended card. It answers `agent/getAuthenticatedExtendedCard` with the A2A
0.3 error `-32007 Authenticated Extended Card is not configured`, and its card does not list the
method.

## `skills` { #skills }

A door that serves `message/send` advertises one skill: running a task. Under
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
| `message/send` | Starts a task, or delivers input to a task waiting for it, and returns the task | `lifecycle.task_acceptance` is `single` or `queue` |
| `message/stream` | Starts a task and streams its events as `text/event-stream` | `lifecycle.task_acceptance` is `single` or `queue` |
| `stream/watch` | Streams this session's events as an observer, without starting a task | Always |
| `tasks/get` | Returns the task named by `params.id` | Always |
| `tasks/cancel` | Cancels the task named by `params.id` | Always |
| `session/stop` | Cancels every live task and reports what the session leaves running | Always |
| `agent/getAuthenticatedExtendedCard` | Returns the [extended card](#extended-card) | The capsule declares [`network.authentication`](manifest.md#field-network-authentication) |

See [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance). Under `none`, `POST /`
answers `message/send` and `message/stream` with `-32601`, so neither is listed,
`capabilities.streaming` is `false` and `skills` is `[]`.

The card endpoint itself is not listed.

#### Request headers { #request-headers }

The door reads `x-murmur-*` request headers alongside the JSON-RPC body. One of them changes what
the methods that start a turn do:

| Header | Read on | Effect |
|---|---|---|
| `x-murmur-forget-session` | `message/send`, `message/stream` | `true` drops the harness session this request's context names before the turn, so the turn starts a new conversation under the same context id |

Only the value `true` asks for it; every other value, and the header's absence, are the same
request. A capsule on any transport but
[`inference.transport: process`](manifest.md#transport-process) keeps no harness session, and
answers the header with `-32602` without starting a task — see
[`E-RUN-039`](diagnostics.md#e-run-039). What the forget does, and what it records, is
[what removes an entry](workdir.md#what-removes-an-entry).

#### `tasks/get` { #tasks-get }

`tasks/get` answers the task `params.id` names, running or finished, as an A2A `Task`:

| Key | Present |
|---|---|
| `id`, `contextId`, `status.state` | Always |
| `status.message` | On a terminal task — `completed`, `failed`, `canceled` or `rejected` — whose final status said something: a message from the agent with one text part, the same text as the task's final [`status` frame](streaming-protocol.md#one-final-status) |
| `artifacts: [{"name": "response", …}]` | On a `completed` task that produced a response: its answer, as one text part |
| `artifacts: [{"name": "prompt", …}]` | On an `input-required` task: the question it is waiting on |

```json
{"id": "tsk_…", "contextId": "ctx_…",
 "status": {"state": "completed",
            "message": {"messageId": "msg_tsk_…_status", "role": "agent", "parts": [{"text": "session ended"}]}},
 "artifacts": [{"name": "response", "parts": [{"text": "four"}]}]}
```

On a [formation token](roster.md#formation-token), `tasks/get` answers only the tasks the calling
member submitted. Any other id — another member's task, the operator's, or one the session never
held — is answered `-32001 Task not found`. Every other credential reads every task.

A `message/send` the door has no room for answers a `rejected` task whose `status.message` says
why: `task rejected: capsule is busy`, or `task rejected: the session is closing`.

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

`params.frames` lists every frame type the capsule's `message/stream` and `stream/watch`
connections can write. Its key and the frame list are on the streaming protocol page, under
[Frame vocabulary](streaming-protocol.md#murmur-stream-v1). It is on the public card of every door,
authenticated or not.

## A card with no `supportedInterfaces` { #no-supported-interfaces }

A card without `supportedInterfaces` comes from a runtime at v0.4.0 or earlier, which serves the
[previous card](#migrating). A capsule serving it is reachable over the same methods, and:

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
| `capabilities.cancellation` | Removed. It was `true` on every card; the door extension's `params.methods` lists `tasks/cancel` |
| `serves.methods` | Door extension `params.methods`, same values and order |
| `serves.planes` | Capsule extension `params.planes`, same values and order |

Find an extension by its `uri` in `capabilities.extensions`, not by its position.
