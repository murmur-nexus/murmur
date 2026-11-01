# Streaming Protocol

The server-sent event streams a capsule serves on its A2A address: the three endpoints, the A2A
v1.0 events they write, the murmur frames the stream extension adds, event ids, replay and the
heartbeat.

[`mur watch`](cli.md#mur-watch) is a client for one of the endpoints. What it implements is listed
under [What `mur watch` implements](#mur-watch).

What the door serves and refuses, and every extension, is listed under
[A2A v1.0 conformance](agent-card.md#conformance).

---

## Endpoints { #endpoints }

Each endpoint is a JSON-RPC method sent as `POST /` to the capsule's address, with
`Content-Type: application/json`, `A2A-Version: 1.0` and a non-empty body. A `POST /` without a
JSON content type or without a body is answered `404 Not Found`. A request without
[`A2A-Version: 1.0`](agent-card.md#a2a-version) is answered `-32009` as one JSON body, and no
stream opens.

| Method | Purpose | Params | Closes when |
|---|---|---|---|
| `SendStreamingMessage` | Send a message — a new task, or the reply to a task waiting for input — and stream that task | `params.message`, read as `SendMessage` reads it: see [Agent Card: Messages](agent-card.md#message) | After the task's terminal status, or after a live `TASK_STATE_INPUT_REQUIRED` status of the task. Also when the client disconnects or the capsule exits. Closing the connection does not cancel the task: use `CancelTask` |
| `SubscribeToTask` | Stream the rest of one live task the caller [sees](agent-card.md#task-access) | `params.id`, the task's id — see [Agent Card: `SubscribeToTask`](agent-card.md#subscribe-to-task) | After the task's terminal status. A `TASK_STATE_INPUT_REQUIRED` status does not close it. Also when the client disconnects or the capsule exits. Closing the connection does not cancel the task |
| [`murmur.WatchSession`](agent-card.md#watch-session) | Observe every task the capsule runs, without sending anything | `{}` — nothing is read | The capsule's stream ends, or the client disconnects. A task's terminal status does not close it |

`SendStreamingMessage` and `SubscribeToTask` are the task streams: each carries one task.

On a capsule declaring [`network.authentication`](manifest.md#field-network-authentication), every
method requires `Authorization: Bearer <token>` with a token holding the method's scope. A request
without one is refused with `401` or `403` and a JSON body before the stream opens. See
[Agent Card: What the door answers](agent-card.md#door-authentication).

Request a `SendStreamingMessage`, with the stream extension activated:

```bash
curl -N -X POST http://localhost:52222/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'A2A-Extensions: https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1' \
  -d '{"jsonrpc":"2.0","id":"r1","method":"SendStreamingMessage","params":{"message":{"messageId":"msg-1","role":"ROLE_USER","parts":[{"text":"Summarise README.md"}]}}}'
```

Subscribe to a task already running:

```bash
curl -N -X POST http://localhost:52222/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -d '{"jsonrpc":"2.0","id":"sub-1","method":"SubscribeToTask","params":{"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394"}}'
```

Request a `murmur.WatchSession`:

```bash
curl -N -X POST http://localhost:52222/ \
  -H 'Content-Type: application/json' \
  -H 'A2A-Version: 1.0' \
  -H 'Last-Event-ID: 0' \
  -d '{"jsonrpc":"2.0","id":1,"method":"murmur.WatchSession","params":{}}'
```

Each answers with these response headers, then the event stream. There is no `content-length`: the
body ends when the connection closes. The `A2A-Extensions` line names what the answer used, as
[Activation](#activation) lists. On `murmur.WatchSession` it is always:

```text
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
connection: keep-alive
A2A-Extensions: https://docs.murmur.nexus/reference/agent-card/#murmur-door-v1, https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1
```

Everything that can refuse a `SendStreamingMessage` is answered as one JSON-RPC body,
`content-type: application/json`, before any stream opens:

| Refusal | Answer |
|---|---|
| The capsule's [`lifecycle.task_acceptance`](manifest.md#lifecycle-task-acceptance) is `none`, so its card says `capabilities.streaming: false` | `-32004` `UnsupportedOperationError` |
| The message is malformed, carries a file part, or names a task it cannot go to | The error [`SendMessage`](agent-card.md#message) answers the same message with |
| The task could not be handed to the capsule's queue | `-32603`, and the task is not kept |

A new task the capsule has no room for, or refuses because its session is closing, is answered
as a stream of exactly one unnumbered event: the [`task`](#standard-events) in
`TASK_STATE_REJECTED`, whose `status.message` is `task rejected: capsule is busy` or
`task rejected: the session is closing` — the task `SendMessage` answers the same refusal with.
The stream then ends.

Everything that can refuse a `SubscribeToTask` — no `id`, a task the caller does not see, a task
that has ended, a door whose `task_acceptance` is `none` — is answered the same way, as one JSON-RPC
body before any stream opens. The table is under
[Agent Card: `SubscribeToTask`](agent-card.md#subscribe-to-task).

`murmur.WatchSession` is served whatever `task_acceptance` says.

---

## Event form { #event-form }

Every event either endpoint writes is an optional `id:` line, one `data:` line, then a blank line.
Lines end with `\n`. No event carries an `event:` line.

| Line | Form | On |
|---|---|---|
| Id | `id: <unsigned 64-bit integer>` | Every event in the session's sequence — see [Event ids](#event-ids) |
| Data | `data: <JSON-RPC response>` | Every event. Always exactly one line |
| Comment | `:<text>` | The [heartbeat](#heartbeat). Stands alone between blank lines and is not an event |

The `data:` line is a JSON-RPC 2.0 response to the request: its `id` is the request's `id`,
verbatim, string or integer, and its `result` is one A2A v1.0 `StreamResponse`, in ProtoJSON
camelCase.

```text
id: 3
data: {"jsonrpc":"2.0","id":"r1","result":{"statusUpdate":{"taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"TASK_STATE_WORKING","message":{"messageId":"msg_0199c4e2f1b87a01b2c3d4e5f6071829","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","role":"ROLE_AGENT","parts":[{"mediaType":"text/plain","text":"inference turn 2"}]},"timestamp":"2026-10-13T09:00:00.123Z"}}}}

```

---

## Standard events { #standard-events }

A `StreamResponse` sets exactly one member. A connection that did not activate the stream
extension receives only `task`, `statusUpdate` and `artifactUpdate` events of its own task, and no
`metadata` keyed by the extension's URI.

| Member | Written | Shape |
|---|---|---|
| `task` | First, on every task stream, unnumbered. On `SendStreamingMessage`, the task as [`GetTask`](agent-card.md#tasks-get) would have answered when the message was taken: `TASK_STATE_SUBMITTED` for a new task, `TASK_STATE_WORKING` for one the message continued. On `SubscribeToTask`, the task exactly as `GetTask` answers when the stream opens, in any state but a terminal one | An A2A `Task` |
| `statusUpdate` | Every change of a task's state, and every step the runtime reports while the task works | `taskId`, `contextId`, `status`, and `metadata` on an [annotated](#murmur-stream-v1) event |
| `artifactUpdate` | Immediately before a `TASK_STATE_COMPLETED` status, when `GetTask` reports a `response` artifact for the task | `taskId`, `contextId`, `artifact` — the `response` artifact exactly as `GetTask` reports it — and `lastChunk: true` |
| `message` | Only on `murmur.WatchSession`, carrying a [murmur frame](#murmur-stream-v1) that belongs to no task | An A2A `Message` from `ROLE_AGENT` |

A `status` holds `state`, `timestamp` and, when the runtime had something to say, `message`: an
A2A `Message` from `ROLE_AGENT` naming the task and its context, with one text part. A non-terminal
status's `messageId` is `msg_` and a UUIDv7, unique within the session. A terminal status is exactly
the `status` `GetTask` reports for the task afterwards. `Task.history` is never written.

`timestamp` is when the status was recorded: the task's last change for the opening `task` and the
terminal status, and the moment it was written for every other status. See
[Agent Card: Status timestamps](agent-card.md#status-timestamp).

| `status.state` | Murmur's word | `status.message` |
|---|---|---|
| `TASK_STATE_SUBMITTED` | `submitted` | On the opening `task` of a new task only. No message |
| `TASK_STATE_WORKING` | `working` | One of the [working messages](#working-messages). No message on the opening `task` of a continued task |
| `TASK_STATE_INPUT_REQUIRED` | `input-required` | The prompt a tool passed to [`request-input`](wit-interfaces.md#murmurtasktask) |
| `TASK_STATE_COMPLETED` | `completed` | `task completed` |
| `TASK_STATE_FAILED` | `failed` | `task failed (<cause>): <reason>` when the driver failed or rejected the credential, its response could not be acted on, or a compaction hook failed — `<cause>` is the [`task_failed`](observability-schemas.md#task-failed) cause and `<reason>` its reason; `driver invocation failed: <error>` when the driver could not be called; `max_turns exceeded: …` naming `inference.max_turns`; the spend refusal when a spend ceiling stopped the task; `input-timeout` when a `request-input` wait timed out; the refusal naming [`lifecycle.max_task_reopens`](manifest.md#field-lifecycle) or `inference.max_turns` when an `on-task-end` hook still wanted a reopen that limit did not allow; the error, as `error[<code>]: <message>` or its text, when the task ended in a runtime error |
| `TASK_STATE_CANCELED` | `canceled` | `task canceled` for a running task; `task canceled before it started` for a queued one; `task canceled; the harness was killed and its session may not resume cleanly` when a [`transport: process`](#transports) harness had to be killed |
| `TASK_STATE_REJECTED` | `rejected` | Refused at the door: `task rejected: capsule is busy` or `task rejected: the session is closing`, as the one unnumbered `task` event of its stream. Refused when the session ended: `task rejected: the session ended before this task started`, or `task rejected: the session was stopped before this task started` under [`mur stop`](cli.md#mur-stop), as a numbered `statusUpdate` |

### Working status messages { #working-messages }

A `TASK_STATE_WORKING` status says what the task is doing:

| `status.message` | Written when |
|---|---|
| `inference turn <n>` | At the start of each inference turn, counted from 1 across every attempt of the task |
| `resumed` | A `TASK_STATE_INPUT_REQUIRED` wait is answered |
| `reopened by hook <hook>` | An `on-task-end` hook reopens the task |
| `waiting on call-member: <n> call(s) outstanding` | [`call-member`](runtime-provided-tools.md#call-member) calls are unanswered |
| `waiting on delegate-task: <n> delegation(s) outstanding` | Delegations the task started have not reported back |
| `continuing with <n> member answer(s)` | The task continues with the member answers that arrived |
| `continuing with <n> delegation outcome(s)` | The task continues with the delegation outcomes that arrived |
| `keep working <n> of <limit>` | [`lifecycle.keep_working`](manifest.md#lifecycle-keep-working) sends its message after a turn end |
| `done check failed, reopened <n> of <limit>` | [`lifecycle.done_check`](manifest.md#lifecycle-done-check)'s command failed and the task reopens |

When both kinds are outstanding, or both kinds arrived, the two messages are joined with `; ` in
one status.

### What `SendStreamingMessage` writes, in order { #stream-order }

1. The response headers.
2. The `task`, unnumbered.
3. When the request carried `Last-Event-ID`: a [`gap`](#event-gap) when one applies and the
   connection activated the extension, then every buffered event of this task with an id above
   the cursor — see [Replay](#replay). A replayed terminal status ends the stream; a replayed
   `TASK_STATE_INPUT_REQUIRED` does not, since the task has left that state.
4. The task's live events, [`lagged`](#event-lagged) frames when activated, and heartbeats, until
   the task's terminal status or a live `TASK_STATE_INPUT_REQUIRED` status.

Only events whose `taskId` is the connection's task are written.

To answer a stream that ended at `TASK_STATE_INPUT_REQUIRED`:

1. Read the prompt from that status's `status.message`.
2. Send a `SendStreamingMessage` whose `message.taskId` is the task's id. A `message.contextId`, if
   sent, must be the task's, or the request is answered `-32602`. Set `Last-Event-ID` to the last
   id the first stream carried to receive anything of the task written since.
3. The new stream opens with the task in `TASK_STATE_WORKING` and continues as above.

A `SendMessage` naming the task delivers the same reply without a stream.

### What `SubscribeToTask` writes, in order { #subscribe-to-task }

1. The response headers.
2. The `task`, unnumbered, as `GetTask` answers it when the stream opens. The connection starts
   receiving events before the task is read, so no later event is lost; an event written between
   the two is in the `task` and is also delivered.
3. When the request carried `Last-Event-ID`: the replay, exactly as on `SendStreamingMessage`.
4. The task's live events, `lagged` frames when activated, and heartbeats, until the task's
   terminal status.

The one difference from `SendStreamingMessage` is the end: a `TASK_STATE_INPUT_REQUIRED` status
does not end a subscription. The reply comes on a `SendMessage` or `SendStreamingMessage` naming the
task, and the subscription then carries the task's `TASK_STATE_WORKING` status with message
`resumed`.

The launch's own task — `mur run --task`, or a `task.md` in the workdir — is a live task from the
moment the door is announced, so a subscription to it, found with
[`ListTasks`](agent-card.md#list-tasks), carries its events to its terminal status like any other.

### What `murmur.WatchSession` writes, in order { #watch-order }

1. The response headers.
2. A [`connection-ack`](#event-connection-ack) frame.
3. The [replay](#replay), from `Last-Event-ID`, or from `0` when the header is absent: a `gap`
   frame if one applies, then every buffered event.
4. Every task's live events, murmur frames included, `lagged` frames and heartbeats, until the
   capsule's stream ends. A capsule process that exits closes the connection without a
   [`capsule-closed`](#event-capsule-closed) frame.

### One final status { #one-final-status }

Every task the capsule accepts ends in exactly one terminal `statusUpdate`, whether it ran or not,
and it is the task's last event. It is written after [`GetTask`](agent-card.md#tasks-get) answers
the same state, and its `status` is the one `GetTask` reports. For a task that ran, it is written
after every `on-task-end` hook has run.

| How the task ended | `status.state` | Before it |
|---|---|---|
| An attempt completed and no hook reopened it | `TASK_STATE_COMPLETED` | The `response` `artifactUpdate`, when the answer is not empty |
| A hook still wanted a reopen that `lifecycle.max_task_reopens` or `inference.max_turns` did not allow | `TASK_STATE_FAILED` | — |
| The last attempt failed | `TASK_STATE_FAILED` | — |
| The task was cancelled | `TASK_STATE_CANCELED` | — |
| The session ended before the task started | `TASK_STATE_REJECTED` | — |

A `request-input` wait that times out ends the attempt, not the task: the tool that asked fails,
and the task's terminal status — `TASK_STATE_FAILED` with message `input-timeout` — follows once
the hooks have run, unless a hook reopens the task.

### Reopened tasks { #reopened-tasks }

An `on-task-end` hook that [reopens a task](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task)
starts a new attempt of the same task id. Between the two attempts the stream carries one
`TASK_STATE_WORKING` status with message `reopened by hook <hook>`. An activated connection reads
the reopen's ordinal, counted from 1, as `metadata[<URI>].reopen` on that status — the only
annotation the extension makes on a standard event:

```json
{"statusUpdate":{"taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"TASK_STATE_WORKING","message":{"messageId":"msg_0199c4e2f2117c0aa1b2c3d4e5f60718","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","role":"ROLE_AGENT","parts":[{"mediaType":"text/plain","text":"reopened by hook review-gate"}]},"timestamp":"2026-10-13T09:00:00.123Z"},"metadata":{"https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1":{"reopen":1}}}}
```

| What a client sees | Reading |
|---|---|
| `text`, `thinking` and `artifact` frames of a rejected attempt | Written live and never withdrawn |
| The boundary status | The rejected attempt is over; the next attempt starts with `inference turn <n>`, numbered on from the rejected attempt's last turn |
| The `response` artifact | The task's answer |

Never join `text` frames across a boundary: the joined text mixes a rejected answer with the
accepted one.

---

## The stream extension { #murmur-stream-v1 }

The murmur frames — streamed text and reasoning, tool calls being written, tool results, and the
connection-level frames — are an A2A extension with URI
`https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1`. It is listed in the
agent card's `capabilities.extensions` after the door and capsule extensions, with
`required: false` — see [Agent Card: Extensions](agent-card.md#extensions). On the public card of a
door that declares `network.authentication` it follows the door extension, and the extended card
carries it too.

```json
{
  "uri": "https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1",
  "description": "The murmur frames this capsule writes in addition to the A2A v1.0 events: sent on SendStreamingMessage and SubscribeToTask when the request's A2A-Extensions header names this URI, and always on murmur.WatchSession. Each is carried in a StreamResponse under metadata keyed by this URI.",
  "required": false,
  "params": {"frames": ["text", "thinking", "tool-call-started", "tool-call-progress", "artifact", "gap", "lagged", "connection-ack", "capsule-closed"]}
}
```

| Key | Type | Notes |
|---|---|---|
| `params.frames` | array of strings | The frames a connection to this door can receive, in the order of the table below |

| Frame | A task stream, activated | `murmur.WatchSession` |
|---|---|---|
| [`text`](#event-text) | yes | yes |
| [`thinking`](#event-thinking) | yes | yes |
| [`tool-call-started`](#event-tool-call-started) | yes | yes |
| [`tool-call-progress`](#event-tool-call-progress) | yes | yes |
| [`artifact`](#event-artifact) | yes | yes |
| [`quota`](#event-quota) | yes, on a `transport: process` capsule | yes, on a `transport: process` capsule |
| [`suggested-prompt`](#event-suggested-prompt) | yes, on a `transport: process` capsule | yes, on a `transport: process` capsule |
| [`gap`](#event-gap) | yes, when the request sent `Last-Event-ID` | yes |
| [`lagged`](#event-lagged) | yes | yes |
| [`connection-ack`](#event-connection-ack) | no | yes, first event |
| [`capsule-closed`](#event-capsule-closed) | no | yes, as the last event when it is written |

Read the list from the capsule's card rather than from this table. The example above is a
`transport: http` capsule's; a `transport: process` capsule's lists `quota` and `suggested-prompt`
after `artifact`.

### Activation { #activation }

| Request | Connection |
|---|---|
| `SendStreamingMessage` or `SubscribeToTask` whose `A2A-Extensions` headers name the URI | Activated. The response head's `A2A-Extensions` names the URI |
| `SendStreamingMessage` or `SubscribeToTask` without it | Plain: the [standard events](#standard-events) of its task and nothing else. The response head does not name the URI |
| `murmur.WatchSession` | Always activated, whatever the request says. The response head's `A2A-Extensions` names the door extension's URI, then this one |
| `SendMessage`, `GetTask`, `ListTasks`, `CancelTask`, `murmur.StopSession`, `GetExtendedAgentCard` | Never activated. The response head does not name the URI |

The door reads every `A2A-Extensions` request line, header name case-insensitive, splits each
value on `,`, trims each entry and drops empty ones. The connection is activated when one entry
equals the URI exactly. Any other URI in the header is ignored, the door's and capsule's own
extension URIs included. No method refuses a request for not activating the extension.

A response names the [door extension](agent-card.md#murmur-door-v1)'s URI when the request used
that extension, whatever its `A2A-Extensions` said: on `murmur.WatchSession`, and on a
`SendStreamingMessage` whose `message.metadata` holds the door URI, which carries
[`forgetSession`](agent-card.md#forget-session). The door URI comes first:
`A2A-Extensions: <door URI>, <stream URI>`.

### Carriers { #carriers }

An activated connection receives each murmur frame inside an event that still parses as an A2A
v1.0 `StreamResponse`, under `metadata` keyed by the URI. The object there always has a `frame`
key naming the frame; the reopen annotation has none, which is how a client tells them apart.

| The frame belongs to | Carrier |
|---|---|
| A task (`text`, `thinking`, `tool-call-started`, `tool-call-progress`, `artifact`, and `gap` and `lagged` on a task stream) | A `statusUpdate` of the task repeating its state with no `status.message`, its `status.timestamp` the moment the carrier was written. A task's frames are written only while it works, so the state is `TASK_STATE_WORKING`. A `gap` repeats the opening `task`'s state; a `lagged` repeats the state of the last `task` or status the connection wrote |
| No task (`connection-ack`, `capsule-closed`, and `gap` and `lagged` on `murmur.WatchSession`) | A `message` from `ROLE_AGENT`, `messageId` `msg_` and a UUIDv7, no `contextId`, and one text part holding the frame's name |

```json
{"statusUpdate":{"taskId":"tsk_0199c4e2f1b7712a9d3e4f5061728394","contextId":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"TASK_STATE_WORKING","timestamp":"2026-10-13T09:00:00.123Z"},"metadata":{"https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1":{"frame":"text","text":"README.md describes","final":false}}}}
```

```json
{"message":{"messageId":"msg_0199c4e2f0a77e11a2b3c4d5e6f70819","role":"ROLE_AGENT","parts":[{"mediaType":"text/plain","text":"connection-ack"}],"metadata":{"https://docs.murmur.nexus/reference/streaming-protocol/#murmur-stream-v1":{"frame":"connection-ack","role":"observer","conversationMode":"stateless"}}}}
```

The frame sections below show the object under `metadata[<URI>]`.

---

## Transports { #transports }

A task writes the same events whatever the capsule's [`inference.transport`](manifest.md#inference-config):
the same kinds, the same keys and the same order for the same work, apart from the differences
listed below. On a `transport: http` capsule, the `text`, `thinking`, `tool-call-started` and
`tool-call-progress` frames come from the driver's
[`murmur:stream/events`](wit-interfaces.md#stream-events) calls. On a `transport: process`
capsule they come from the events its [process driver](manifest.md#process-driver) reads out of the
harness:

| The harness | Events |
|---|---|
| Starts text, reasoning or a tool call | A `TASK_STATE_WORKING` status, message `inference turn <n>` |
| Streams a fragment of text | [`text`](#event-text), `"final":false` |
| Reports the complete text of what it streamed | [`text`](#event-text), `"final":true`, empty — the client keeps the fragments it was sent |
| Streams a fragment of reasoning, or reports reasoning nothing streamed | [`thinking`](#event-thinking) |
| Begins writing a tool call | [`tool-call-started`](#event-tool-call-started) |
| Reports how much of it is written | [`tool-call-progress`](#event-tool-call-progress) |
| Answers a tool call | [`artifact`](#event-artifact) |
| Ends the turn | One [`artifact`](#event-artifact) frame for each artifact an `on-inference` hook returned for that turn, then the whole result as one `"final":true` [`text`](#event-text) frame when the last turn streamed nothing, then the `response` artifact and the task's `TASK_STATE_COMPLETED` status |
| Fails the turn | The task's `TASK_STATE_FAILED` status |
| Reports its subscription windows | [`quota`](#event-quota) |
| Predicts the person's next prompt | [`suggested-prompt`](#event-suggested-prompt) |

A process task ends in the same [one final status](#one-final-status) as an http task, on every
path out of its last attempt, including the ones a runtime diagnostic ends: the status message is
that diagnostic, under the code [`mur` reports it as](diagnostics.md).

On a process capsule the harness runs the turn, so its `artifact` frames differ in two keys:

| Key | On a process capsule | Why |
|---|---|---|
| `fenceSource` | Always `null` | The harness receives tool output unfenced |
| `exitCode`, `truncated` | Always `null` and `false` | A process driver reports no exit status or truncation flag |
| `attachments` | Always `[]` | The harness's model is shown no image, and nothing is stored |

A provider retry writes no event on either transport; the driver reports it to the trace as
`harness_retry`.

### Event order within a task { #task-frame-order }

One attempt of a task writes its events in this order:

1. A `TASK_STATE_WORKING` status, message `inference turn <n>`, at the start of each inference
   turn, before the driver is called.
2. While the turn runs, [`thinking`](#event-thinking) and [`text`](#event-text) `"final":false`
   frames, in the order they are produced. A
   [`tool-call-started`](#event-tool-call-started) frame for each tool call the model begins
   writing, and that call's [`tool-call-progress`](#event-tool-call-progress) frames, come after
   the turn's `TASK_STATE_WORKING` status and before the call's `artifact`. On a
   `transport: http` capsule they come before the turn's `"final":true` `text` frame of step 3.
3. When the turn streamed text, one `text` `"final":true` frame with empty text once the driver
   returns.
4. One [`artifact`](#event-artifact) per tool call, written when the call returns, in dispatch
   order. A tool's own streamed `text` and `thinking` frames, and the `TASK_STATE_INPUT_REQUIRED`
   and `TASK_STATE_WORKING` `resumed` statuses of its `request-input` wait, come before its
   `artifact`.
5. The next turn repeats from step 1.
6. When the attempt completes, one `artifact` per hook artifact, then the whole reply as one
   `text` `"final":true` frame when the last turn streamed no text.
7. On a `transport: process` capsule, [`quota`](#event-quota) and
   [`suggested-prompt`](#event-suggested-prompt) frames wherever the harness reports them,
   including after the turn's end while the harness exits, and always before the steps below.
8. A reopen writes the [reopen boundary](#reopened-tasks), and the next attempt starts at step 1.
9. The task's [one final status](#one-final-status) is its last event, after its `response`
   artifact when it completed with one.

A plain connection receives steps 1, 4's statuses, 8 and 9 only.

---

## `text` { #event-text }

A piece of the model's reply.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `text` |
| `text` | string | The piece of reply text |
| `final` | bool | See below |

| `final` | `text` | Written when |
|---|---|---|
| `false` | A chunk | A streaming driver, or a tool, emits a chunk through [`murmur:stream/events`](wit-interfaces.md#stream-events) |
| `true` | `""` | A streaming driver's inference call returned, or a [`transport: process`](#transports) harness reported the complete text of fragments it streamed. Marks the end of that turn's chunks |
| `true` | The whole reply | An attempt completed with a non-empty reply and no chunk was emitted during its last inference turn |

`final` on a `text` frame ends nothing: the task goes on to its statuses.

```json
{"frame":"text","text":"README.md describes","final":false}
```

---

## `thinking` { #event-thinking }

A piece of the model's reasoning, emitted by a streaming driver or a tool through
[`murmur:stream/events`](wit-interfaces.md#stream-events).

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `thinking` |
| `text` | string | The piece of reasoning |
| `final` | bool | Always `false` |

```json
{"frame":"thinking","text":"The file names two targets","final":false}
```

---

## `tool-call-started` { #event-tool-call-started }

A tool call the model has begun writing, before its input is complete and before the tool runs.
Written when the capsule's driver reports calls as they are written:

| Transport | Source |
|---|---|
| `http` | The inference driver calls [`tool-call-started`](wit-interfaces.md#stream-events) |
| `process` | The [process driver](wit-interfaces.md#process-tool-call-progress) sends the `tool-call-started` event |

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `tool-call-started` |
| `toolCallId` | string | The call's id: the `toolCallId` of the [`artifact`](#event-artifact) that answers it |
| `toolName` | string | The capsule's tool name, with no harness prefix: the `toolName` of the answering `artifact` |

| Property | Value |
|---|---|
| Once per call | At most one frame per `toolCallId`, before any `tool-call-progress` for the call and before its `artifact` |
| End of the call | The `artifact` with the same `toolCallId` ends the call. A call no `artifact` answers ends with the task's terminal status |
| Input | The frame carries none of the call's input |
| Turns | Opens no inference turn and does not count toward `inference.max_turns` |
| Optional | A call's `artifact` may arrive with no `tool-call-started` before it. A client treats the start as a hint, never as a prerequisite |

```json
{"frame":"tool-call-started","toolCallId":"toolu_01","toolName":"write_file"}
```

---

## `tool-call-progress` { #event-tool-call-progress }

How many bytes of a started call's input the model has written so far. Follows the call's
[`tool-call-started`](#event-tool-call-started) frame. On `transport: http` it comes from the
driver's [`tool-call-input-bytes`](wit-interfaces.md#stream-events) calls; on `transport: process`,
from the process driver's [`tool-call-progress`](wit-interfaces.md#process-tool-call-progress)
events.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `tool-call-progress` |
| `toolCallId` | string | The call's id, as on its `tool-call-started` frame |
| `inputBytes` | u64 | Bytes of the call's input JSON written so far, cumulative for the call. Never smaller than an earlier frame's for the same `toolCallId` |

| Property | Value |
|---|---|
| Rate | At most one frame per call per 250 ms, plus one when the call's input is complete if the largest size reported has not been written yet |
| Count | Any number, including none. A client works with `tool-call-started` alone |
| Input | The frame carries a size and none of the call's input |
| Turns | Opens no inference turn and does not count toward `inference.max_turns` |

```json
{"frame":"tool-call-progress","toolCallId":"toolu_01","inputBytes":3172}
```

---

## `artifact` { #event-artifact }

One tool call's result, or one hook artifact, as the model received it. It is a murmur frame, never
an A2A artifact: `GetTask` reports none of them. The runtime writes one frame for each tool call it
dispatches, in dispatch order.

A hook artifact is what an `on-inference` hook returned as
[`artifact`](wit-interfaces.md#what-each-handler-can-commit) for the attempt's last inference
turn. The runtime writes one frame for each, in hook-registration order, only when the attempt
completes: after that turn's streamed text, before its whole-result [`text`](#event-text) frame,
and ahead of the task's terminal status or the [reopen boundary](#reopened-tasks). A turn that
called a tool forwards none, and neither does an attempt that is canceled, fails or stops at a
spend ceiling.

What a refused call writes depends on what refused it and on the transport:

| Refused by | [`transport: http`](manifest.md#transport-http) | [`transport: process`](manifest.md#transport-process) |
|---|---|---|
| A policy hook or `capabilities.filesystem.read_only` | No frame | One frame with `isError` `true`: the harness reports the refusal it was handed as its own failed tool call |
| A missing [required field](manifest.md#input-schema) | One frame with `isError` `true` and the refusal text as `content`, the same frame as for a call that never reached a tool | One frame with `isError` `true`, from the harness's report |

Every key is present on every frame, in this order. A key that does not apply to the frame is
`null`, never absent.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `artifact` |
| `toolName` | string | The tool, skill or hook the frame reports |
| `content` | string | Exactly the bytes the model received, never truncated. Fenced when `fenceSource` is a string, unfenced when it is `null`. The fence grammar is in [Untrusted Fence](untrusted-fence.md#grammar) |
| `fenceSource` | string \| null | `tool:<name>` for an ordinary tool result. `null` for a skill result, a call that never reached a tool, and a hook artifact |
| `toolCallId` | string \| null | The provider's id for the call. `null` on a hook artifact, and on a call the provider gave no id or an empty one |
| `isError` | bool | `true` when the tool returned a status other than `passed`, or the call never reached a tool. Matches `"status":"error"` on the call's `tool_call` or `skill_call` [trace event](observability-schemas.md#session-trace-tracejsonl). `false` on a hook artifact |
| `durationMs` | u64 \| null | Time the dispatch took, equal to the `duration_ms` on the call's trace event. `null` on a hook artifact |
| `exitCode` | i32 \| null | The exit status of the subprocess the call ran to completion. `null` for a WASM tool, a skill, a command moved to the background, a call that never reached a tool, and a hook artifact |
| `truncated` | bool | The tool result's own `truncated` flag, unchanged. `false` on a call that never reached a tool and on a hook artifact |
| `attachments` | array | The images the call [returned](wit-interfaces.md#tool-attachments) that the runtime accepted, in the order the call named them, each `{"mediaType", "sizeBytes", "sha256", "url"}`. `url` is `/attachments/<task-id>/<sha256>`, where the door serves the bytes: see [Attachment bytes](#attachment-bytes). A refused attachment is not listed. `[]` on every other frame |

`isError` and `exitCode` are separate facts. A shell command that runs to completion is a
successful call whatever its exit status, so `bash` reports `"isError":false` beside
`"exitCode":3`. Branch on `exitCode` to act on the command's own outcome.

| Frame | Example |
|---|---|
| A successful tool call | `{"frame":"artifact","toolName":"bash","content":"<untrusted-content source=tool:bash>\nExit code: 0\nStdout:\nhello\n\n</untrusted-content>","fenceSource":"tool:bash","toolCallId":"toolu_01","isError":false,"durationMs":12,"exitCode":0,"truncated":false,"attachments":[]}` |
| A call that never reached a tool | `{"frame":"artifact","toolName":"no-such-tool","content":"tool 'no-such-tool' is not declared in manifest allowlist","fenceSource":null,"toolCallId":"toolu_03","isError":true,"durationMs":0,"exitCode":null,"truncated":false,"attachments":[]}` |
| A tool call that returned an image | `{"frame":"artifact","toolName":"screenshot","content":"<untrusted-content source=tool:screenshot>\nsaved shot.png\n</untrusted-content>","fenceSource":"tool:screenshot","toolCallId":"toolu_02","isError":false,"durationMs":40,"exitCode":null,"truncated":false,"attachments":[{"mediaType":"image/png","sizeBytes":48213,"sha256":"9f2c…","url":"/attachments/tsk_0199c4e2…/9f2c…"}]}` |
| A hook artifact | `{"frame":"artifact","toolName":"my-hook","content":"{\"reviewed\":true}","fenceSource":null,"toolCallId":null,"isError":false,"durationMs":null,"exitCode":null,"truncated":false,"attachments":[]}` |

### Attachment bytes { #attachment-bytes }

`GET /attachments/<task-id>/<sha256>` answers with the bytes of an image a tool call of that task
returned, the `url` its `artifact` frame lists. On a capsule declaring
[`network.authentication`](manifest.md#field-network-authentication) it takes a token holding the
`GetTask` scope, and reaches only the tasks that token [sees](agent-card.md#task-access); a public
door serves it as it serves `GetTask`.

| Request | Status | Body |
|---|---|---|
| A task the caller sees, and an image one of its tool calls returned | `200`, `content-type` the image's media type, and `content-length` | The image's bytes |
| No token, or one that is not valid | `401` | The door's refusal |
| A token without the `GetTask` scope | `403` | The door's refusal |
| A task the registry does not hold or the caller does not see, or a hash none of its calls returned | `404` | Empty |
| Any method but `GET` | `405`, `allow: GET` | Empty |

---

## `quota` { #event-quota }

One report of the subscription windows a `transport: process` capsule's harness says are used, as
its [process driver](wit-interfaces.md#process-quota) sent it. It is a snapshot of the windows it
names: a later frame replaces a window of the same name and leaves the others standing. The
merged view is the `quota` field of [`session-status`](runtime-provided-tools.md#session-status).

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `quota` |
| `windows` | array | At least one window |
| `windows[].name` | string | The window's name, such as `five-hour` |
| `windows[].used` | f64 | Fraction of the window used, 0 to 1. Absent when the harness did not report it |
| `windows[].resetsAtMs` | u64 | When the window resets, unix milliseconds. Absent when not reported |
| `windows[].windowMinutes` | u64 | The window's length in minutes. Absent when not reported |
| `windows[].state` | string | `ok`, `warning` or `exhausted`. Absent when not reported |

| Property | Value |
|---|---|
| Carrier | A `TASK_STATE_WORKING` status; the task keeps working |
| Turns | Opens no inference turn |
| Effect | None: the capsule reports a window and acts on none, however exhausted |
| Canceled task | Still written |

```json
{"frame":"quota","windows":[{"name":"five-hour","used":0.42,"resetsAtMs":1793491200000,"windowMinutes":300,"state":"ok"},{"name":"seven-day","used":0.1}]}
```

---

## `suggested-prompt` { #event-suggested-prompt }

What a `transport: process` capsule's harness predicts the person will ask next, as its
[process driver](wit-interfaces.md#process-suggested-prompt) sent it.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `suggested-prompt` |
| `text` | string | The predicted prompt, never empty |

| Property | Value |
|---|---|
| Carrier | A `TASK_STATE_WORKING` status; the task keeps working |
| When | Often after the turn's end, read while the harness exits; always before the task's final status |
| Turns | Opens no inference turn |
| Canceled task | Not written, as a canceled task delivers no answer |

```json
{"frame":"suggested-prompt","text":"Run the test suite"}
```

---

## `gap` { #event-gap }

Written before a [replay](#replay) when events written after the client's `Last-Event-ID` are no
longer in the replay buffer, or when that id was never issued by this capsule session. Unnumbered.
On a task stream it is written only to an activated connection.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `gap` |
| `firstAvailableId` | u64 | The id of the oldest event in the replay buffer |

```json
{"frame":"gap","firstAvailableId":37}
```

---

## `lagged` { #event-lagged }

Written to a connection that fell more than 128 events behind and [lost live events](#lagged),
before the next event it receives. Unnumbered, and only that connection receives it, once per loss.
The count is not cumulative: a client that wants a total adds them up. On a task stream it is
written only to an activated connection.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `lagged` |
| `missed` | u64 | The events this connection lost since the last event it received. At least `1` |

```json
{"frame":"lagged","missed":40}
```

---

## `connection-ack` { #event-connection-ack }

The first event on every `murmur.WatchSession` connection. Unnumbered.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `connection-ack` |
| `role` | string | Always `observer` |
| `conversationMode` | string | `stateless` or `threaded` — the capsule's [`lifecycle.conversation`](manifest.md#lifecycle-conversation) |

```json
{"frame":"connection-ack","role":"observer","conversationMode":"stateless"}
```

---

## `capsule-closed` { #event-capsule-closed }

The last event on a `murmur.WatchSession` connection whose capsule closed its event stream while still
serving the connection. Unnumbered.

| Key | Type | Notes |
|---|---|---|
| `frame` | string | `capsule-closed`. No other key |

```json
{"frame":"capsule-closed"}
```

A capsule process that exits closes the connection without writing this frame, whether it was
stopped with [`mur stop`](cli.md#mur-stop), ended after its task, or was killed. A client reads end
of stream without `capsule-closed` as a lost connection: the capsule may have exited or may still
be running, and only a new connection tells the two apart.

---

## Events a connection misses { #lagged }

Each connection reads live events from a queue of 128. A connection that falls more than 128
events behind loses the oldest of them; an activated connection is written a
[`lagged`](#event-lagged) frame naming how many before the next event it receives. The connection
stays open. The capsule also writes the count to its own stderr, as
`SSE broadcast lagged by <n> events`.

On a task stream, a lost event may be the status that ends the stream. From the first loss on, the
connection checks its task's state at each [heartbeat](#heartbeat), and writes what it finds,
unnumbered, then ends:

| The task | Written |
|---|---|
| Has ended | Its ending as `GetTask` reports it: the `response` artifact when it has one, then the terminal status |
| Waits for input, on `SendStreamingMessage` | A `TASK_STATE_INPUT_REQUIRED` status carrying the prompt. A `SubscribeToTask` connection writes nothing and stays open |

A `murmur.WatchSession` client that needs every event reconnects with the id of the last event it
received and [replays](#replay) the rest, which recovers them while they are still in the replay
buffer.

---

## Heartbeat { #heartbeat }

A comment written to every open connection on both endpoints, so an idle capsule can be told apart
from a closed connection by reading bytes.

| Property | Value |
|---|---|
| Wire form | `:heartbeat\n\n` — the comment line `:heartbeat`, then a blank line |
| Interval | 15 seconds, measured from when the connection was opened. The first one arrives about 15 seconds after connecting |
| Reset by events | No. A heartbeat comes due every 15 seconds whether or not events were written in between |
| Event id | None |
| Replay buffer | Never entered, so no replay contains one |

The heartbeat is written apart from the capsule's turns, so it keeps its cadence while a turn is running — through an inference call, a tool call, or a shell
command the capsule is waiting on. Events wait on the call: a turn waiting on one call writes no
event until the call returns, so heartbeats may be the only bytes on the connection for a while.

A pause in the heartbeat longer than 15 seconds means the capsule's process is alive but not
being given time to write it:

- the process is suspended, for example by `SIGSTOP` or a debugger
- the host is too loaded to schedule the process
- the capsule is too busy serving other connections

A pause does not mean the capsule is gone. The connection closing is what ends the stream; what
that tells a client is under [`capsule-closed`](#event-capsule-closed).

---

## Event ids { #event-ids }

| Event | Carries `id:` |
|---|---|
| Every `statusUpdate` and `artifactUpdate` of a task | yes |
| Every murmur frame of a task: `text`, `thinking`, `tool-call-started`, `tool-call-progress`, `artifact` | yes |
| The opening `task` of a task stream, and a `TASK_STATE_REJECTED` `task` refused at the door | no |
| `gap`, `lagged`, `connection-ack`, `capsule-closed` | no |
| The ending or `TASK_STATE_INPUT_REQUIRED` status a [lagged](#lagged) task stream writes in place of a lost one | no |
| Heartbeat | no |

A capsule session numbers its events from one sequence:

| Property | Value |
|---|---|
| First id | `1` |
| Next id | Exactly one higher than the previous numbered event's, across every task, every reopened attempt and every event kind, in the order events are written |
| Uniqueness | Unique within a capsule session. A new session, including one started with `mur run --resume`, starts again at `1` |
| On one connection | Strictly ascending, never repeated, and not contiguous: a task stream skips other tasks' ids, and a plain one skips its own task's murmur frames |

An event without an id never takes a number, so it leaves no hole in the sequence. A
[`lagged`](#event-lagged) frame is what reports live events a connection lost.

---

## Replay { #replay }

Each session keeps its most recent 512 events with ids in one replay buffer shared by both
endpoints, in the order they were written. When the buffer is full, a new event evicts the oldest.
The heartbeat and every unnumbered event — the opening `task`, a `TASK_STATE_REJECTED` `task`,
`connection-ack`, `gap`, `lagged` and `capsule-closed` — never enter it.

| Endpoint | `Last-Event-ID` sent | `Last-Event-ID` absent | `Last-Event-ID` not a number |
|---|---|---|---|
| `SendStreamingMessage`, `SubscribeToTask` | Replays this task's events after that id, following the opening `task` | No replay | No replay |
| `murmur.WatchSession` | Replays every event after that id, following `connection-ack` | Replays from `0` | Replays from `0` |

The header name is case-insensitive. A replay from id `N`:

| Buffer | Written |
|---|---|
| Empty | Nothing |
| `N` was never issued by this session: it is the id the next event will take, or higher | A `gap` naming the oldest id, then every event in the buffer |
| The oldest event's id is greater than `N + 1`: events after `N` were evicted | A `gap` naming the oldest id, then every event in the buffer |
| Otherwise | Every event in the buffer whose id is greater than `N`, in order |

A task stream writes the replay's events of its own task only, and its `gap` only when activated.
A replay from `0` of a buffer that has evicted nothing is every event the session has written,
starting at id `1`. Live events follow the replay on the same connection, starting after the
highest id the replay held.

To reconnect a `murmur.WatchSession`, send the id of the last event received as `Last-Event-ID`. Without
a `gap`, the replay is exactly the events written after that id. With one, the events between that
id and `firstAvailableId` are gone, or the id came from an earlier capsule session, whose ids this
session numbers again from `1`.

A `SendStreamingMessage` connection lost while its task works is not reopened by sending the message
again: a message naming a working task is refused `-32004`. Send a `SubscribeToTask` naming the task,
with the last id received as `Last-Event-ID`.

---

## Unknown frames and keys { #unknown-events }

| On receiving | A client |
|---|---|
| A `StreamResponse` member it does not know | Ignores the event |
| A `frame` it does not know | Ignores the event |
| A JSON key it does not know, at any depth | Ignores the key |
| A line beginning with `:` | Skips the line |

These rules are what let the capsule add frames, keys and comments without breaking a client that
follows them.

---

## What `mur watch` implements { #mur-watch }

[`mur watch`](cli.md#mur-watch) is a `murmur.WatchSession` client.

| Protocol feature | `mur watch` |
|---|---|
| `A2A-Version` | Sends `A2A-Version: 1.0` |
| `Last-Event-ID` | Sends `Last-Event-ID: 0`, so it replays the buffer on attach |
| `id:` lines | Read, only to report where a lost connection stopped |
| Reconnecting | Does not reconnect |
| `statusUpdate` | Printed to stdout as `[<state>]  <message>`, the state in murmur's word; a task's terminal status as `[<state>]` alone |
| `artifactUpdate` `response` | Held, and printed wrapped under its task's `[completed]` line |
| `task` | Ignored |
| `text`, `artifact` | Printed to stdout; text is printed as it arrives, and the next status or artifact line after text that did not end its line starts on a new line |
| `thinking` | Not shown |
| `tool-call-started`, `tool-call-progress` | Not shown |
| `quota`, `suggested-prompt` | Not shown |
| `connection-ack` | Read for `conversationMode`, which sets how status lines are labelled |
| Heartbeat | Read and discarded |
| `gap` | A warning on stderr naming `firstAvailableId` |
| `lagged` | A warning on stderr naming `missed` |
| Unknown members, frames and keys | Ignored |
| `capsule-closed` | Prints `[murmur] capsule closed` to stderr and exits `0` |
| Connection lost | Exits `1` with `E-IO-003`: `connection to <session id or address> lost after event id <n>`, or `lost before any event`, followed by `the capsule may still be running; run mur watch again to reattach` |
