# Streaming Protocol

The server-sent event stream a capsule serves on its A2A address: the two endpoints, every frame
they write, event ids, replay and the heartbeat.

[`mur watch`](cli.md#mur-watch) is a client for one of the endpoints. What it implements is listed
under [What `mur watch` implements](#mur-watch).

---

## Endpoints { #endpoints }

Both endpoints are JSON-RPC methods sent as `POST /` to the capsule's address, with
`Content-Type: application/json` and a non-empty body. A `POST /` without a JSON content type or
without a body is answered `404 Not Found`.

| Method | Purpose | Params | Closes when |
|---|---|---|---|
| `message/stream` | Submit a task and stream the capsule's frames while it runs | An A2A message, under `params.message` or as `params` itself: `messageId` (string, required), `role` (string, required), `parts` (array of `{"text": …}`, required), `contextId` (string, optional) | The first live `status` frame with `"final":true` is written, whatever task it belongs to. Also after an `error` frame and after a `rejected` status |
| `stream/watch` | Observe every frame the capsule writes, without submitting anything | `{}` — nothing is read | The capsule's stream ends, or the client disconnects. A `final` status does not close it |

Request a `stream/watch`:

```bash
curl -N -X POST http://localhost:52222/ \
  -H 'Content-Type: application/json' \
  -H 'Last-Event-ID: 0' \
  -d '{"jsonrpc":"2.0","id":1,"method":"stream/watch","params":{}}'
```

Request a `message/stream`:

```bash
curl -N -X POST http://localhost:52222/ \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"message/stream","params":{"message":{"messageId":"msg-1","role":"user","parts":[{"text":"Summarise README.md"}]}}}'
```

Both answer with these response headers, then the event stream. There is no `content-length`: the
body ends when the connection closes.

```text
HTTP/1.1 200 OK
content-type: text/event-stream
cache-control: no-cache
connection: keep-alive
```

A `message/stream` sent to a capsule with
[`lifecycle.task_acceptance: none`](manifest.md#lifecycle-task-acceptance) is answered with a plain
JSON-RPC error instead — `HTTP/1.1 200 OK`, `content-type: application/json`, and
`{"jsonrpc":"2.0","id":…,"error":{"code":-32601,"message":"Method not found"}}`. `stream/watch`
is served whatever `task_acceptance` says.

Which frames each endpoint can deliver:

| Frame | `message/stream` | `stream/watch` |
|---|---|---|
| [`status`](#event-status) | yes | yes |
| [`status`](#event-status) with state `rejected` | yes | no |
| [`artifact`](#event-artifact) | yes | yes |
| [`text`](#event-text) | yes | yes |
| [`thinking`](#event-thinking) | yes | yes |
| [`gap`](#event-gap) | yes, only when the request sent `Last-Event-ID` | yes |
| [`lagged`](#event-lagged) | yes | yes |
| [`connection-ack`](#event-connection-ack) | no | yes, first frame |
| [`capsule-closed`](#event-capsule-closed) | no | yes, as the last frame when it is written |
| [`error`](#event-error) | yes | no |
| [Heartbeat](#heartbeat) | yes | yes |

Neither endpoint filters by task. Every `status`, `artifact`, `text` and `thinking` frame the
capsule writes reaches every open connection on either endpoint, so a `message/stream` connection
carries the frames of other tasks running or queued on the same capsule, and closes on the first
`final` status of any of them. Read the `id` key in each frame's `data` to tell tasks apart.

A client that sends a message and waits for its own reply cannot match on that task id: the id is
minted when the task is accepted and appears only in the frames, so it is not known when the request
is made. Send a `contextId` of your own in the message instead — the capsule uses it verbatim and
mints one only when it is absent — then match each frame on `context_id` in its `data` and ignore
the rest. Without this, two senders that overlap both return on whichever task finishes first, and
one of them reads a reply to a message it never sent.

### What `message/stream` writes, in order

1. The response headers.
2. When the request carried `Last-Event-ID`, the [replay](#replay): a `gap` frame if one applies,
   then the buffered frames. A replayed `final` status does not close the connection.
3. When the params do not parse as a message, an [`error`](#event-error) frame, and the
   connection closes.
4. When the capsule cannot accept another task, a `rejected` [`status`](#event-status), and the
   connection closes.
5. When the task cannot be handed to the capsule's queue, an [`error`](#event-error) frame, and
   the connection closes.
6. Live frames, [`lagged`](#event-lagged) frames and heartbeats, until the first delivered
   `status` frame with `"final":true`.

The task id and context id minted for the task appear only in the frames' `data`: `tsk_` and a
UUIDv7 for the task, and the message's `contextId` or `ctx_` and a UUIDv7 for the context.

### What `stream/watch` writes, in order

1. The response headers.
2. A [`connection-ack`](#event-connection-ack) frame.
3. The [replay](#replay), from `Last-Event-ID`, or from `0` when the header is absent: a `gap`
   frame if one applies, then the buffered frames.
4. Live frames, [`lagged`](#event-lagged) frames and heartbeats, until the capsule's stream ends.
   A capsule process that exits closes the connection without a
   [`capsule-closed`](#event-capsule-closed) frame.

### Frames a connection misses { #lagged }

Each connection reads live frames from a queue of 128. A connection that falls more than 128
frames behind loses the oldest of them, and a [`lagged`](#event-lagged) frame naming how many is
written before the next frame the connection receives. The connection stays open. The capsule also
writes the count to its own stderr, as `SSE broadcast lagged by <n> events`; nothing about a lag is
written to `trace.jsonl`.

On `message/stream`, a lost frame may be the `final` status the client is waiting for. The
connection then stays open until the next `final` status it is delivered, from any task.

A client that needs every frame reconnects with the id of the last frame it received and
[replays](#replay) the rest, which recovers them while they are still in the replay buffer.

---

## Frame format { #frame-format }

A frame is a group of lines ending with a blank line (`\n\n`). Lines end with `\n`, never `\r\n`.

| Line | Form | On |
|---|---|---|
| Id | `id: <unsigned 64-bit integer>` | `status` except `rejected`, `artifact`, `text`, `thinking` — see [Event ids](#event-ids) |
| Event type | `event: <type>` | Every frame |
| Data | `data: <JSON object>` | Every frame. Always exactly one line |
| Comment | `:<text>` | The [heartbeat](#heartbeat). Stands alone between blank lines and is not a frame |

A frame with an id writes its lines in the order `id`, `event`, `data`. A frame without one writes
`event`, `data`.

```text
id: 3
event: status
data: {"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394","context_id":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"working","message":"inference turn 2"},"final":false}

```

---

## Transports { #transports }

A task writes the same frames whatever the capsule's [`inference.transport`](manifest.md#inference-config):
the same types, the same keys and the same order for the same work. On a `transport: process`
capsule the frames come from the events its [process driver](manifest.md#process-driver) reads out
of the harness:

| The harness | Frames |
|---|---|
| Starts text, reasoning or a tool call | `status` `working`, message `inference turn <n>` |
| Streams a fragment of text | [`text`](#event-text), `"final":false` |
| Reports the complete text of what it streamed | [`text`](#event-text), `"final":true`, empty — the client keeps the fragments it was sent |
| Streams a fragment of reasoning, or reports reasoning nothing streamed | [`thinking`](#event-thinking) |
| Answers a tool call | [`artifact`](#event-artifact) |
| Ends the turn | The whole result as one `"final":true` [`text`](#event-text) frame when the last turn streamed nothing, then `status` `completed` |
| Fails the turn | `status` `failed` |

Every attempt of a process task ends in exactly one `final:true` `status` frame, on every path out
of the attempt, including the ones a runtime diagnostic ends: the frame's `status.message` is that
diagnostic, under the code [`mur` reports it as](diagnostics.md).

The two transports' streams differ in three things, each because the harness, not this runtime,
ran the turn:

| Difference | Why |
|---|---|
| `artifact.fence_source` is `null` on every frame a process capsule writes | The tool bridge returns tool output to the harness unfenced, so the content carries no fence |
| `artifact.exit_code` is `null` and `artifact.truncated` is `false` on every frame a process capsule writes | The driver contract's tool result carries neither an exit status nor a truncation flag |
| A provider retry writes no frame on either transport | A retry happens inside the driver, which reports it to the trace as `harness_retry` and to the stream not at all |

---

## `status` { #event-status }

A task's state. Written by the agent loop at the start of every inference turn, when a task waits
for input and resumes, and when a task ends.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `id` | string | Never | The task id, `tsk_…` |
| `context_id` | string | On the `input-required` frame, the `working` frame with message `resumed`, and the `failed` frame with message `input-timeout` | The task's context id |
| `status` | object | Never | |
| `status.state` | string | Never | One of the states below |
| `status.message` | string | Never | See the states below |
| `status.response` | string | On every state except `completed` | The task's result text, as written to `out/result.txt` |
| `final` | bool | Never | `true` on the states that end a task |

| `status.state` | `final` | `status.message` |
|---|---|---|
| `working` | `false` | `inference turn <n>`, counted from 1, at the start of each inference turn. `resumed` when an `input-required` wait is answered |
| `input-required` | `false` | The prompt a tool passed to [`request-input`](wit-interfaces.md#murmurtasktask) |
| `completed` | `true` | `session ended` |
| `failed` | `true` | `session ended` when the driver or its response failed, or compaction failed; `driver invocation failed: <error>` when the driver could not be called; `max_turns exceeded: the task used all <n> inference turns`; the spend refusal when a spend ceiling stopped the task; `input-timeout` when a `request-input` wait timed out; `error[<code>]: <message>` when a diagnostic ended a [`transport: process`](#transports) attempt |
| `canceled` | `true` | `task canceled` for a running task; `task canceled before it started` for a queued one; `task canceled; the harness was killed and its session may not resume cleanly` when a [`transport: process`](#transports) harness had to be killed rather than stopping when it was asked |
| `rejected` | `true` | `task rejected: capsule is busy`. Written only to the `message/stream` connection that submitted the task, with no `id:` line, and never buffered |

A `final` status is the last frame a task writes in the ordinary case, with two exceptions that
keep writing frames for the same task id afterwards:

| After | What follows |
|---|---|
| `completed` or `failed`, when an `on-task-end` hook reopens the task ([`lifecycle.max_task_reopens`](manifest.md#field-lifecycle)) | A new attempt: `working` from `inference turn 1`. Its frames continue the session's [id sequence](#event-ids) |
| `failed` with message `input-timeout` | The tool that asked for input fails, the model receives that failure as an `artifact`, and the task goes on to its own final status |

A task that ends in an error the agent loop does not report — a driver response that is not JSON,
a trace write failure, a workdir size breach — writes no final status for that attempt. Unless an
`on-task-end` hook reopens the task, a `message/stream` connection on it stays open until another
task's final status or the capsule's exit.

```json
{"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394","context_id":"ctx_0199c4e2f1b7712a9d3e4f50617283a1","status":{"state":"completed","message":"session ended","response":"README.md describes the build."},"final":true}
```

---

## `artifact` { #event-artifact }

One tool call's result, or one hook artifact. The runtime writes one frame for each tool call it
dispatches, in dispatch order, and one for each hook artifact before the task's `completed`
status. On [`transport: http`](manifest.md#transport-http) a call a policy hook refuses writes
no frame. On [`transport: process`](manifest.md#transport-process) the harness reports the
refusal it was handed as its own failed tool call, and that report writes a frame with
`artifact.is_error` set to `true`.

Every key is present on every frame, in this order. A key that does not apply to the frame is
`null`, never absent.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `id` | string | Never | The task id |
| `artifact` | object | Never | |
| `artifact.tool_name` | string | Never | The tool, skill or hook the frame reports |
| `artifact.content` | string | Never | Exactly the bytes the model received, never truncated. Fenced when `fence_source` is a string, unfenced when it is `null`. The fence grammar is in [Untrusted Fence](untrusted-fence.md#grammar) |
| `artifact.fence_source` | string \| null | Never | `tool:<name>` for an ordinary tool result. `null` for a skill result, a call that never reached a tool, and a hook artifact |
| `artifact.tool_call_id` | string \| null | Never | The provider's id for the call. `null` on a hook artifact, and on a call the provider gave no id or an empty one |
| `artifact.is_error` | bool | Never | `true` when the tool returned a status other than `passed`, or the call never reached a tool. Matches `"status":"error"` on the call's `tool_call` or `skill_call` [trace event](observability-schemas.md#session-trace-tracejsonl). `false` on a hook artifact |
| `artifact.duration_ms` | u64 \| null | Never | Time the dispatch took, equal to the `duration_ms` on the call's trace event. `null` on a hook artifact |
| `artifact.exit_code` | i32 \| null | Never | The exit status of the subprocess the call ran to completion. `null` for a WASM tool, a skill, a command moved to the background, a call that never reached a tool, and a hook artifact |
| `artifact.truncated` | bool | Never | The tool result's own `truncated` flag, unchanged. `false` on a call that never reached a tool and on a hook artifact |

There is no `summary` key.

`is_error` and `exit_code` are separate facts. A shell command that runs to completion is a
successful call whatever its exit status, so `bash` reports `"is_error":false` beside
`"exit_code":3`. Branch on `exit_code` to act on the command's own outcome.

| Frame | Example `artifact` |
|---|---|
| A successful tool call | `{"tool_name":"bash","content":"<untrusted-content source=tool:bash>\n$ echo hello\nExit code: 0\nStdout:\nhello\n\nStderr:\n\n</untrusted-content>","fence_source":"tool:bash","tool_call_id":"toolu_01","is_error":false,"duration_ms":12,"exit_code":0,"truncated":false}` |
| A failed tool call | `{"tool_name":"jsonl-line-count","content":"<untrusted-content source=tool:jsonl-line-count>\nfailed to read '{\"data\":\"missing.jsonl\"}': No such file or directory (os error 44)\n</untrusted-content>","fence_source":"tool:jsonl-line-count","tool_call_id":"toolu_02","is_error":true,"duration_ms":5,"exit_code":null,"truncated":false}` |
| A call that never reached a tool | `{"tool_name":"no-such-tool","content":"tool 'no-such-tool' is not declared in manifest allowlist","fence_source":null,"tool_call_id":"toolu_03","is_error":true,"duration_ms":0,"exit_code":null,"truncated":false}` |
| A hook artifact | `{"tool_name":"my-hook","content":"{\"reviewed\":true}","fence_source":null,"tool_call_id":null,"is_error":false,"duration_ms":null,"exit_code":null,"truncated":false}` |

```json
{"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394","artifact":{"tool_name":"no-such-tool","content":"tool 'no-such-tool' is not declared in manifest allowlist","fence_source":null,"tool_call_id":"toolu_03","is_error":true,"duration_ms":0,"exit_code":null,"truncated":false}}
```

---

## `text` { #event-text }

A piece of the model's reply.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `id` | string | Never | The task id |
| `text` | string | Never | The piece of reply text |
| `final` | bool | Never | See below |

| `final` | `text` | Written when |
|---|---|---|
| `false` | A chunk | A streaming driver, or a tool, emits a chunk through [`murmur:text/chunks`](wit-interfaces.md#text-chunks) |
| `true` | `""` | A streaming driver's inference call returned, or a [`transport: process`](#transports) harness reported the complete text of fragments it streamed. Marks the end of that turn's chunks |
| `true` | The whole reply | A task completed with a non-empty reply and no chunk was emitted during its last inference turn |

`final` on a `text` frame ends nothing: the task goes on to its `status` frames.

```json
{"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394","text":"README.md describes","final":false}
```

---

## `thinking` { #event-thinking }

A piece of the model's reasoning, emitted by a streaming driver or a tool through
[`murmur:text/chunks`](wit-interfaces.md#text-chunks).

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `id` | string | Never | The task id |
| `text` | string | Never | The piece of reasoning |
| `final` | bool | Never | Always `false` |

```json
{"id":"tsk_0199c4e2f1b7712a9d3e4f5061728394","text":"The file names two targets","final":false}
```

---

## `gap` { #event-gap }

Written before a [replay](#replay) when frames written after the client's `Last-Event-ID` are no
longer in the replay buffer, or when that id was never issued by this capsule session. The replay
that follows is the whole buffer.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `first_available_id` | u64 | Never | The id of the oldest frame in the replay buffer |

```json
{"first_available_id":37}
```

---

## `lagged` { #event-lagged }

Written to a connection that fell more than 128 frames behind and [lost live frames](#lagged),
before the next frame it receives. Only that connection receives it, once per loss. The count is
not cumulative: a client that wants a total adds them up.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `missed` | u64 | Never | The frames this connection lost since the last frame it received. At least `1` |

```json
{"missed":40}
```

---

## `connection-ack` { #event-connection-ack }

The first frame on every `stream/watch` connection.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `role` | string | Never | Always `observer` |
| `conversation_mode` | string | Never | `stateless` or `threaded` — the capsule's [`lifecycle.conversation`](manifest.md#lifecycle-conversation) |

```json
{"role":"observer","conversation_mode":"stateless"}
```

---

## `capsule-closed` { #event-capsule-closed }

The last frame on a `stream/watch` connection whose capsule closed its frame stream while still
serving the connection. Its data is an empty object.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| — | — | — | No keys |

```json
{}
```

A capsule process that exits closes the connection without writing this frame, whether it was
stopped with [`mur stop`](cli.md#mur-stop), ended after its task, or was killed. A client reads end
of stream without `capsule-closed` as a lost connection: the capsule may have exited or may still
be running, and only a new connection tells the two apart.

---

## `error` { #event-error }

Written on `message/stream` when a request cannot become a task. The connection closes after it.

| Key | Type | Absent when | Notes |
|---|---|---|---|
| `error` | string | Never | `Invalid params: <parse error>` when the params are not a message; `internal error: queue send failed` when the task could not be queued |

The parse error is written into the string without escaping. A parse error that quotes the
offending value, such as `invalid type: string "x", expected a sequence`, makes `data` invalid
JSON; a client that fails to parse an `error` frame's data still has an `error` frame.

```json
{"error":"Invalid params: missing field `messageId`"}
```

---

## Heartbeat { #heartbeat }

A comment written to every open connection on both endpoints, so an idle capsule can be told apart
from a closed connection by reading bytes.

| Property | Value |
|---|---|
| Wire form | `:heartbeat\n\n` — the comment line `:heartbeat`, then a blank line |
| Interval | 15 seconds, measured from when the connection was opened. The first one arrives about 15 seconds after connecting |
| Reset by frames | No. A heartbeat comes due every 15 seconds whether or not frames were written in between |
| Event id | None |
| Replay buffer | Never entered, so no replay contains one |

The heartbeat is written apart from the capsule's turns, so it
keeps its cadence while a turn is running — through an inference call, a tool call, or a shell
command the capsule is waiting on. Frames wait on the call: a turn waiting on one call writes no
frame until the call returns, so heartbeats may be the only bytes on the connection for a while.

A pause in the heartbeat longer than 15 seconds means the capsule's process is alive but not
being given time to write it:

- the process is suspended, for example by `SIGSTOP` or a debugger
- the host is too loaded to schedule the process
- every thread the capsule serves connections on is occupied

A pause does not mean the capsule is gone. The connection closing is what ends the stream; what
that tells a client is under [`capsule-closed`](#event-capsule-closed).

---

## Event ids { #event-ids }

| Frame | Carries `id:` | Id source |
|---|---|---|
| `status` from the agent loop (`working` per turn, `completed`, `failed`, `canceled`) | yes | The session's sequence |
| `artifact` | yes | The session's sequence |
| `text`, `thinking` | yes | The session's sequence |
| `status` `input-required`, `working` `resumed`, `failed` `input-timeout` | yes | The session's sequence |
| `status` `canceled` with message `task canceled before it started` | yes | The session's sequence |
| `status` `rejected` | no | — |
| `gap` | no | — |
| `lagged` | no | — |
| `connection-ack` | no | — |
| `capsule-closed` | no | — |
| `error` | no | — |
| Heartbeat | no | — |

A capsule session numbers its frames from one sequence:

| Property | Value |
|---|---|
| First id | `1` |
| Next id | Exactly one higher than the previous frame's, across every task, every reopened attempt and every frame kind, in the order frames are written |
| Uniqueness | Unique within a capsule session. A new session, including one started with `mur run --resume`, starts again at `1` |
| On one connection | Strictly ascending. A connection never receives the same id twice |

Frames without an id never take a number, so they leave no hole in the sequence. A
[`lagged`](#event-lagged) frame is what reports live frames a connection lost.

---

## Replay { #replay }

Each session keeps its most recent 512 frames with ids in one replay buffer shared by both
endpoints, in the order they were written. When the buffer is full, a new frame evicts the oldest.
The heartbeat, `connection-ack`, `gap`, `lagged`, `capsule-closed`, `error` and the `rejected`
status never enter it.

| Endpoint | `Last-Event-ID` sent | `Last-Event-ID` absent | `Last-Event-ID` not a number |
|---|---|---|---|
| `message/stream` | Replays from that id, before the task is submitted | No replay | No replay |
| `stream/watch` | Replays from that id, after `connection-ack` | Replays from `0` | Replays from `0` |

The header name is case-insensitive. A replay from id `N`:

| Buffer | Written |
|---|---|
| Empty | Nothing |
| `N` was never issued by this session: it is the id the next frame will take, or higher | A `gap` frame naming the oldest id, then every frame in the buffer |
| The oldest frame's id is greater than `N + 1`: frames after `N` were evicted | A `gap` frame naming the oldest id, then every frame in the buffer |
| Otherwise | Every frame in the buffer whose id is greater than `N`, in order |

A replay from `0` of a buffer that has evicted nothing is every frame the session has written,
starting at id `1`. Live frames follow the replay on the same connection, starting after the
highest id the replay wrote.

To reconnect, send the id of the last frame received as `Last-Event-ID` — a browser `EventSource`
does this on its own, from the `id:` lines. Without a `gap`, the replay is exactly the frames
written after that id, whichever task they belong to. With one, the frames between that id and
`first_available_id` are gone, or the id came from an earlier capsule session, whose ids this
session numbers again from `1`.

---

## Unknown frames and keys { #unknown-events }

| On receiving | A client |
|---|---|
| An event type it does not know | Ignores the frame |
| A JSON key it does not know, at any depth | Ignores the key |
| A line beginning with `:` | Skips the line |

These three rules are what let the capsule add frame types, keys and comments without breaking a
client that follows them.

---

## What `mur watch` implements { #mur-watch }

[`mur watch`](cli.md#mur-watch) is a `stream/watch` client.

| Protocol feature | `mur watch` |
|---|---|
| `Last-Event-ID` | Sends `Last-Event-ID: 0`, so it replays the buffer on attach |
| `id:` lines | Read, only to report where a lost connection stopped |
| Reconnecting | Does not reconnect |
| `status`, `artifact`, `text` | Printed to stdout |
| `thinking` | Not shown |
| `connection-ack` | Read for `conversation_mode`, which sets how `status` lines are labelled |
| Heartbeat | Read and discarded |
| `error`, `rejected` status | Never delivered to this endpoint |
| `gap` | A warning on stderr naming `first_available_id` |
| `lagged` | A warning on stderr naming `missed` |
| Unknown event types and keys | Ignored |
| `capsule-closed` | Prints `[murmur] capsule closed` to stderr and exits `0` |
| Connection lost | Exits `1` with `E-IO-003`: `connection to <session id or address> lost after event id <n>`, or `lost before any event`, followed by `the capsule may still be running; run mur watch again to reattach` |
