# Agent Card

Every capsule serves an agent card at `GET /.well-known/agent-card.json` on its HTTP listener. The
card names the session answering the address, states what the capsule may do, and lists what its
listener answers.

```json
{
  "name": "my-agent",
  "version": "0.1.0",
  "url": "localhost:41873",
  "session_id": "ses_019f01a940ce7761854e768ecbe3d399",
  "capabilities": {
    "tools": ["bash"],
    "shell": true,
    "network": true,
    "streaming": true,
    "cancellation": true
  },
  "serves": {
    "methods": ["message/send", "message/stream", "stream/watch", "tasks/get", "tasks/cancel", "session/stop"],
    "planes": ["files"]
  }
}
```

The card answers two separate questions:

| Block | Answers | Derived from |
|---|---|---|
| [`capabilities`](#capabilities) | What may this capsule do? | The capsule's permissions: installed artifacts and `capabilities.*` in the manifest |
| [`serves`](#serves) | What does this listener answer? | The methods `POST /` dispatches and the declared `exports` |

---

## Keys { #keys }

Every key below is present on every card this runtime serves.

| Key | Type | Meaning |
|---|---|---|
| `name` | string | The capsule's `name` from `murmur.yaml` |
| `version` | string | The capsule's `version` from `murmur.yaml` |
| `url` | string | The `host:port` this capsule's listener answers on |
| `session_id` | string | The session answering this address. Compare it with the session you expect before sending a task |
| `capabilities` | object | See [`capabilities`](#capabilities) |
| `serves` | object | See [`serves`](#serves) |

## `capabilities` { #capabilities }

This capsule's permissions.

| Key | Type | Meaning |
|---|---|---|
| `capabilities.tools` | array of strings | Names of the installed artifacts the model can call |
| `capabilities.shell` | boolean | `true` when `capabilities.shell.allow` lists at least one command |
| `capabilities.network` | boolean | `true` when `capabilities.network.allow` lists at least one destination |
| `capabilities.streaming` | boolean | `true` when [`serves.methods`](#serves-methods) contains `message/stream` and the capsule's inference transport streams text |
| `capabilities.cancellation` | boolean | `true` when [`serves.methods`](#serves-methods) contains `tasks/cancel` |

`streaming` is the door's answer and the transport's together: a door that answers
`message/stream` over a transport that streams nothing lists the method under
[`serves.methods`](#serves-methods) and reports `false` here. `cancellation` is the served method
alone, because every transport can be stopped.

| Transport | `streaming` | `cancellation` |
|---|---|---|
| `http` | `true` when `message/stream` is served | `true` |
| `process` | `true` when `message/stream` is served and the [process driver](manifest.md#process-driver) reports `streams-text` | `true` |

## `serves` { #serves }

What this listener answers. Both keys are always present; `serves.planes` may be empty.

| Key | Type | Meaning |
|---|---|---|
| `serves.methods` | array of strings | The JSON-RPC methods `POST /` answers — see [`serves.methods`](#serves-methods) |
| `serves.planes` | array of strings | The HTTP planes that serve content — see [`serves.planes`](#serves-planes) |

The card endpoint itself is not listed.

### `serves.methods` { #serves-methods }

A method is listed exactly when `POST /` answers it with something other than `-32601 Method not
found`. Method names match exactly: case and surrounding whitespace are significant. Methods appear
in this order:

| Method | Answers | Listed when |
|---|---|---|
| `message/send` | Starts a task, or delivers input to a task waiting for it, and returns the task | `lifecycle.task_acceptance` is `single` or `queue` |
| `message/stream` | Starts a task and streams its events as `text/event-stream` | `lifecycle.task_acceptance` is `single` or `queue` |
| `stream/watch` | Streams this session's events as an observer, without starting a task | Always |
| `tasks/get` | Returns the task named by `params.id` | Always |
| `tasks/cancel` | Cancels the task named by `params.id` | Always |
| `session/stop` | Cancels every live task and reports what the session leaves running | Always |

See [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance). Under `none`, `POST /`
answers `message/send` and `message/stream` with `-32601`, so neither is listed and
`capabilities.streaming` is `false`. `tasks/cancel` is answered under every acceptance, so
`capabilities.cancellation` is `true` on every card.

### Request headers { #request-headers }

Per-request control that is not part of the A2A message schema rides on an `x-murmur-*` request
header. One is read on the methods that start a turn:

| Header | Read on | Effect |
|---|---|---|
| `x-murmur-forget-session` | `message/send`, `message/stream` | `true` drops the harness session this request's context names before the turn, so the turn starts a new conversation under the same context id |

Only the value `true` asks for it; every other value, and the header's absence, are the same
request. A capsule on any transport but
[`inference.transport: process`](manifest.md#transport-process) keeps no harness session, and
answers the header with `-32602` without starting a task — see
[`E-RUN-039`](diagnostics.md#e-run-039). What the forget does, and what it records, is
[what removes an entry](workdir.md#what-removes-an-entry).

### `serves.planes` { #serves-planes }

A plane is listed only when the manifest declares it. An undeclared plane answers every request
with a refusal. Planes appear in this order:

| Plane | Listed when | Serves |
|---|---|---|
| `files` | [`exports.files`](manifest.md#field-exports) is declared | The [operator plane](resource-plane.md#operator-plane) under `/resources/files` |
| `peer_files` | [`exports.peer_files`](manifest.md#field-exports) is declared | The [peer plane](resource-plane.md#peer-plane) under `/resources/peer/<handle>` |

## A card with no `serves` key { #no-serves-key }

A card without `serves` comes from a runtime at v0.3.0 or earlier, which predates discovery. Gate on
the runtime version for such a capsule; for any card that has `serves`, read `serves` instead.
