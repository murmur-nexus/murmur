# WIT Interfaces

WASM artifacts — capsules, tools, drivers, and hooks — talk to the runtime through WIT
interfaces. This page lists each interface, who uses it, and what its behaviour means for you.
The type definitions themselves live in the repository under
[`crates/capsule-runtime/wit/`](https://github.com/murmur-nexus/murmur/tree/main/crates/capsule-runtime/wit);
each row below links to its source, and [Package versioning](#package-versioning) lists the
version each one carries.

## Interfaces

| Interface | Used by | Purpose |
|---|---|---|
| [`murmur:tool/run`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/tool.wit) | Exported by tool and driver components | The entrypoint the runtime calls to run a tool or an inference driver |
| [`murmur:tool-registry/invoke`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/tool-registry.wit) | Imported by capsule components | Call an allowlisted tool by name |
| [`murmur:capsule/run`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/capsule.wit) | Exported by capsule components | The capsule entrypoint |
| [`murmur:artifact-manager/manage`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/artifact-manager.wit) | Provided by the runtime to capsule components | List, describe, and pull artifacts during a session |
| [`murmur:shell/execute`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/shell-execute.wit) | Implemented natively by the runtime | Run an allowlisted shell binary |
| [`murmur:message/send`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/message/send.wit) | Provided by the runtime to capsule components | Send an A2A task to a peer capsule |
| [`murmur:task/task`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/deps/murmur-task/task.wit) | Imported by tool components | Pause the agent loop and wait for external input |
| [`murmur:text/chunks`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/deps/murmur-text/stream.wit) | Imported by tool and driver components | Emit response and thinking chunks to the session's SSE stream |
| [`murmur:hook/lifecycle`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/deps/murmur-hook/lifecycle.wit) | Exported by hook artifacts | The lifecycle handlers the runtime calls |
| [`murmur:runtime/inference`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/inference.wit) | Provided by the runtime to hook components | Run one LLM completion through the capsule's configured driver |
| [`murmur:runtime/tokens`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/tokens.wit) | Provided by the runtime to hook components | Count the tokens in a string the way the runtime counts them |
| [`murmur:task-io/read`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/deps/murmur-task-io/read.wit) | Provided by the runtime to hook components | Read the task's input text and the agent's result text |
| [`murmur:conversation/read`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/deps/murmur-conversation/read.wit) | Provided by the runtime to hook components | Read the capsule's durable conversation record |

## Worlds

A world is what your component's source compiles against with `wit_bindgen::generate!`.

| World | Imports | Exports | Defined in |
|---|---|---|---|
| `capsule` | `tool-registry/invoke` | `capsule/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `tool` | `task/task`, `text/chunks` | `tool/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `driver` | `text/chunks` | `tool/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `hook` | `runtime/inference`, `runtime/tokens`, `task-io/read`, `conversation/read` | `hook/lifecycle` | [`hook/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/worlds.wit) |
| `runtime-host` | `artifact-manager/manage`, `shell/execute`, `tool-registry/invoke`, `message/send` | — | [`host/host.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/host/host.wit) |

Agent capsules compile against no world — the agent loop runs inside the runtime, and the capsule
is defined by its manifest alone.

---

## `murmur:tool/run`

A `tool-result` carries a `metadata` list — a free-form key/value channel back to the runtime.
These keys are reserved:

| Key | Value | Effect |
|---|---|---|
| `continuation_id` | Opaque id string | Tells the runtime the driver is holding conversation state provider-side. See [Stateful driver continuation](#stateful-driver-continuation). |
| `state_effect` | `"read"` or `"mutate"` | How this call affected the resource it addressed. Recorded on the call's `tool_call` trace event. |
| `resource_id` | Opaque, tool-defined string | Which resource this call addressed. Recorded verbatim on the `tool_call` trace event. |

`state_effect` and `resource_id` together let `mur trace` spot redundant calls without knowing
anything about your tool. Omit either and the call counts as "unknown": it is never credited as a
redundant read.

For `resource_id`, scope the value by addressing scheme (for example `"sym:Foo::bar"`) so it
cannot collide with an unrelated resource of the same name. Two calls address the same resource
when their `resource_id` values are byte-equal. Without one, the redundancy detector guesses
from path-like input fields (`path`, `file`, `file_path`, `filepath`, `filename`) — so declare
the key if your resource is a symbol, URI, or query.

### Stateful driver continuation { #stateful-driver-continuation }

By default the runtime resends the full conversation history on every turn. A driver holding
conversation state provider-side can opt out by returning a non-empty `continuation_id`.

| Situation | What the runtime does |
|---|---|
| Driver returns a non-empty `continuation_id` | Holds the id for the rest of the session loop |
| Next turn, same `context_id` | Sends only the messages appended since the driver last acknowledged state, plus the held id |
| Next turn, different `context_id` | Full resend — a continuation is never reused across unrelated tasks |
| Driver omits the key or returns an empty string | Drops the held id; the next turn is a full resend |
| A hook commits `replace-context` (for example compaction) | Drops the held id; the next turn resends the post-compaction history |

Token accounting for the compaction threshold is always computed from the full conversation, not
the smaller payload actually sent, so continuation never changes when compaction fires.

### Prompt cache routing hint { #prompt-cache-key }

Every request the runtime hands a driver carries a top-level `prompt_cache_key` string. Its value
is fixed for every turn of a task, and it does not change when history is compacted or when a
held `continuation_id` is dropped:

| Task | `prompt_cache_key` |
|---|---|
| Has a context id | `<capsule-name>:<version>:<context-id>` |
| Has no context id | `<capsule-name>:<version>` |

A provider that routes on the value keeps a task's turns on one machine, so each turn reaches the
cache entry the previous turn warmed. Two runs of the same capsule get different values, because
a context id is minted per task.

A driver reads the field only if it declares it, so a driver that ignores it runs unchanged.
Forward it only where the provider defines a field of its own for it: the OpenAI Chat and
Responses APIs accept `prompt_cache_key` in the request body, and the Anthropic Messages API
rejects a body carrying any field it does not define.

### Reported token usage { #driver-usage }

A driver response may carry a top-level `usage` object holding the provider's own token counts
for that call:

```json
{
  "stop_reason": "end_turn",
  "content": [{"type": "text", "text": "..."}],
  "usage": {
    "input_tokens": 12043,
    "output_tokens": 218,
    "cached_tokens": 11780,
    "cache_write_tokens": 0
  }
}
```

| Member | Value |
|---|---|
| `input_tokens` | Tokens the provider billed for the request |
| `output_tokens` | Tokens the provider billed for the completion |
| `cached_tokens` | Request tokens served from the provider's prompt cache |
| `cache_write_tokens` | Request tokens written into the provider's prompt cache |

Every member is optional and every member is a non-negative integer. A driver reports whichever
members its provider returned; a provider with no prompt cache reports no cache members. Omit a
member the provider did not report rather than sending `0` — the runtime keeps the two apart, and
a `0` reads as a genuine cache miss on the trace.

Where the two provider shapes carry each number:

| Member | Anthropic Messages API | OpenAI Chat and Responses APIs |
|---|---|---|
| `input_tokens` | `usage.input_tokens` | `usage.prompt_tokens` |
| `output_tokens` | `usage.output_tokens` | `usage.completion_tokens` |
| `cached_tokens` | `usage.cache_read_input_tokens` | `usage.prompt_tokens_details.cached_tokens` |
| `cache_write_tokens` | `usage.cache_creation_input_tokens` | Not reported |

The runtime records the reported numbers on the call's `inference` trace event and its
`capsule.inference` span, and acts on none of them: the compaction threshold and every budget
decision keep running on the runtime's own pre-flight estimate. See
[Observability schemas](observability-schemas.md#session-trace-tracejsonl) for the field names.

A malformed report degrades to no report rather than failing the call:

| What the driver returns | What the runtime records |
|---|---|
| No `usage` | Nothing |
| A `usage` that is not an object | Nothing |
| A member that is not a non-negative integer | Nothing for that member; its well-formed siblings are still recorded |
| A member the runtime does not define | Nothing for that member |

An omitted number is absent from the trace event, never written as `0`.

---

## `murmur:artifact-manager/manage`

Lets a capsule component inspect and install artifacts mid-session: `list` and `describe` report
what is installed, `diagnostics` returns the session id alongside that list, and `pull` installs a
new artifact. `search` and `remove` are unimplemented: calling either returns an error.

`pull` resolves an artifact from the session's registry, verifies its bytes against the registry
hash and any pinned `murmur.lock` entry, then installs it under `<workdir>/tools/<name>/` and
updates `murmur.lock`. A hash mismatch, or a version or hash that conflicts with an existing
`murmur.lock` pin, returns an error before anything is written. A pulled artifact is immediately
visible to `list()` and `describe()` and, for WASM tools, callable via `invoke()`.

---

## `murmur:shell/execute`

The contract for running an allowlisted shell binary. The runtime implements it natively.

| Behaviour | Detail |
|---|---|
| Non-zero exit code | Data, not an error — the call only errs on spawn/IO failure |
| `truncated: true` | stdout or stderr exceeded 16 KiB and was cut; `full-output-path` points at the untruncated log in the workdir |
| Allowlist | `binary` must appear in `capabilities.shell.allow`; anything else is rejected before the process is spawned |

---

## `murmur:task/task`

`request-input` pauses the agent loop at a decision boundary and waits for a reply delivered by
`message/send`. From the component's perspective the call is synchronous: pass a prompt string,
receive the reply string.

Call it when the agent has reached a decision it cannot make on its own — ambiguous
requirements, or a consequential action that needs approval — and a human or supervisor capsule
is expected to reply.

It is available in the `tool` world only, and only while the capsule is running an A2A task.
Called anywhere else, it returns an error immediately.

**Timeout:** `lifecycle.input_timeout_secs` in the manifest (see
[lifecycle.input_timeout_secs](manifest.md#lifecycle-input-timeout-secs)). When it fires,
the component is aborted and the task transitions to `failed`.

---

## `murmur:message/send`

Sends a task to a peer capsule. `send` takes the peer URL and a message — a `message-id`, an
optional `context-id`, and the `text` — and returns the peer's `task-id`, `context-id`, and
`state`. The runtime handles the JSON-RPC 2.0 wire format, so the capsule never sees it.

| Behaviour | Detail |
|---|---|
| Allowlist | The peer URL must appear in the sender's `capabilities.network.allow`. Otherwise the call returns `Err("network policy: '...' not in capabilities.network.allow")` and no connection is made. |
| Tracing | With OTel configured, the runtime injects a W3C `traceparent` header so the peer's session span nests under the sender's. |
| Result state | `task-result.state` is the peer's response to the send: `submitted`, `working`, `input-required`, `completed`, `failed`, or `rejected`. Poll the peer's `tasks/get` endpoint for the final state. |

---

## `murmur:hook/lifecycle`

Hook artifacts (`runtime: hook`) export this interface. The runtime calls each handler
synchronously. Returning an error logs it to `logs/hook-<name>.log` in the workdir and the
session continues — except at `on-compaction`, where the error fails the session, because the
runtime has no other way back under the token budget.

| Handler | Fires |
|---|---|
| `on-stage` | Once at capsule staging, before any session starts |
| `on-session-start` | Once per capsule launch, before the first task's work begins |
| `on-task-start` | Once per task, before that task's first inference turn |
| `on-inference` | After each inference driver response is parsed |
| `on-tool-call` | After each model-requested tool invocation returns or errors |
| `on-shell` | After each allowed shell command returns |
| `on-compaction` | When the session token threshold is reached, before any history is replaced |
| `on-task-end` | Once per task, immediately after that task's agent loop returns |
| `on-session-end` | Once per capsule launch, after the task loop exits |

With `task_acceptance: none` or `single`, `on-task-start` coincides with `on-session-start`; with
`queue` it fires once per queued task. `session-id` is the session identifier — `ses_` followed by
32 hex characters — and is the same value in every hook event of a run.

`on-task-start` and `on-task-end` are optional exports: a component that omits them loads and is
simply never dispatched for them. The six session handlers are required — a component missing one
fails to load with an error naming it. A missing `on-stage` is logged rather than fatal, and
staging continues.

### What each handler can commit

A handler may return any `hook-output` arm, but the runtime commits only one arm per event:

| Handler | Committed arm |
|---|---|
| `on-stage` | `write-manifests` |
| `on-inference` | `artifact` |
| `on-compaction` | `replace-context` |
| `on-task-end` | `reopen-task` |
| `on-task-start` | `seed-context` |
| All others | none — only `none` is silent |

This table is also enforced ahead of time. A hook artifact declares in its own `murmur.yaml`
which handler it binds to (`binding:`) and what it expects committed (`commit_policy:`), and the
runtime checks the pair against this table when the capsule is staged. A `commit_policy` the
binding's handler cannot commit (say `commit_policy: reopen-task` on `binding: on-stage`) fails
staging with an error naming the binding, the declared policy, and the one this binding honors,
rather than becoming a mid-session dispatch fault. A hook with no `binding:` receives every
event, so any `commit_policy` is valid for it. See [Hook contract
fields](manifest.md#hook-contract-fields).

Returning `none` is always silent, and is the normal case for an observational hook. Returning
any other non-`none` arm is a loud but non-fatal fault:

- A line naming the hook, the handler, and the discarded arm is appended to
  `logs/hook-<name>.log`.
- A `hook_dispatch_error` event is written to `trace.jsonl` — see [Session trace
  schema](observability-schemas.md#session-trace-tracejsonl). The one exception is `on-stage`, which runs before the
  trace file exists. An `async` hook's fault reaches the trace by the same path; the trace and the
  log are the only places it appears, since the agent loop never sees an async hook's return value.

`reopen-task` is a control arm: returning it from `on-task-end` re-runs the task's agent loop
with the arm's string injected as feedback, subject to `lifecycle.max_task_reopens` and
`inference.max_turns`. See [Task
reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task).

`seed-context` is honored at `on-task-start` alone: the first bound blocking hook to return it
wins, and its messages are placed at the head of the task's context, ahead of any loaded history
and ahead of the task message. The list is chronological, oldest first. The host measures it
against `task-start-event.budget-tokens` — `context.max_tokens` × `context.seed_budget` — and
commits it whole, drops its oldest messages, summarizes the overflowing front through the
compaction hook, or refuses it; every outcome is recorded as a
[`context_seed`](observability-schemas.md#context-seed) event, and a refusal never fails the task.
Returning it from any other handler is the same loud-but-non-fatal fault as any other unhonored
arm. Only `binding: on-task-start` accepts it as a `commit_policy`.

### Event field notes

- `compaction-event.model` and `compaction-event.system-prompt` carry
  `inference.compaction.model` and `inference.compaction.system_prompt` verbatim, so a compaction
  hook knows which model and which prompt to use for its own summarization call. Setting
  `inference.compaction.system_prompt_file` instead delivers that file's contents. Either field
  is absent when the manifest leaves it unset, and the hook resolves its own default.
- `message.id` and `message.source-id` are runtime bookkeeping carried alongside a message's
  `role` and `content`. `id` is `msg_` followed by a uuid — an identity, so two byte-identical
  messages still carry different ids. `source-id` is opaque: whatever produced the content sets
  it, and the runtime records it verbatim without parsing it. Both are **stripped before the
  driver payload is built**, so neither reaches the provider — a uuid at the head of a cached
  prefix would break prompt-prefix caching on every request. The runtime mints an `id` for every
  message it builds out of a hook-returned message list, so an `id` a hook sets on a message it
  returns is replaced rather than kept; a `source-id` a hook sets is carried verbatim, and the
  field is absent when the hook set none.
- `task-start-event.context-window` is the capsule's
  [`context.max_tokens`](manifest.md#field-context), or `0` when the manifest declares no `context:`
  block. It is precomputed so a hook sizing its work against the window never has to know which
  model the session runs.
- `task-start-event.budget-tokens` is the ceiling a `seed-context` returned from this event is
  enforced against: [`context.max_tokens`](manifest.md#field-context) ×
  [`context.seed_budget`](manifest.md#field-context), rounded down.
  `task-start-event.prior-tokens` is the token count of the conversation history the task will
  load under [`lifecycle.conversation: threaded`](manifest.md#lifecycle-conversation), and `0`
  under every other conversation mode. Read `0` as "the runtime has not computed this" and
  decline, rather than as an unbounded budget.
- `shell-event.binary` is the program the shell tool actually invoked — a canonicalized absolute
  path (for example `/usr/bin/pytest`) when the runtime resolved the name against `PATH`, and the
  bare invoked name when nothing resolved. `shell-event.command` carries the argument list alone
  (for a shell interpreter, the script text passed via `-c`), so read `binary` to recognize what
  ran.

---

## `murmur:runtime/inference`

A runtime-provided import available to any hook component that declares it. Nothing needs to be
exported and nothing in the manifest changes.

`run-inference` runs exactly one LLM completion through the capsule's configured driver
(`inference.driver.artifact`).

| Case | Behaviour |
|---|---|
| No `model` given | Uses the manifest's `inference.model` |
| `model` given | Sends that model. If the driver or provider rejects it the call returns an error; the runtime never silently falls back. To retry on the primary model, call again without a model. |
| No `system-prompt` given | No system prompt is sent |
| No driver configured | Returns an error naming `inference.driver.artifact`. The import still links, so the hook itself runs. |

`model-used` is the model string the runtime actually sent. `input-tokens` and `output-tokens`
are runtime-side tiktoken counts of the request payload and the raw driver response.

Every call, success or failure, writes one `inference` record to `trace.jsonl` and one
`capsule.inference` OTel span carrying `origin: "hook:<hook name>"` and `model`.

---

## `murmur:runtime/tokens`

A runtime-provided import available to any hook component that declares it. It needs no
capability grant and no manifest entry: counting text reaches no resource.

`count` returns the runtime's own `cl100k_base` count of the string — the same number behind the
compaction trigger and the context-occupancy calculation. A hook measuring a payload against a
budget and the runtime enforcing that budget therefore agree on what the payload costs, which a
hook's own tokenizer would not guarantee.

---

## `murmur:task-io/read`

A runtime-provided import available to any hook component that declares it. It hands a hook the
text of the task its capsule was given and the result text the agent produced, so an output gate or
an archiver needs no filesystem grant to see either.

Reading is granted per hook with
[`capabilities.task_io.read: true`](manifest.md#hook-capabilities) on that hook's entry in the
capsule manifest. A hook without the key still links and still runs — every function returns
`not-granted`.

| Function | Returns |
|---|---|
| `input-len(form)` | Byte length of the task input |
| `read-input(form, offset, max-bytes)` | A window of the task input |
| `output-len()` | Byte length of the agent's result text |
| `read-output(offset, max-bytes)` | A window of the result text |

Ask for a length first, then read the window you can afford: the runtime imposes no truncation cap
of its own. A read returns the longest prefix of the value from `offset` that fits in `max-bytes`
and ends on a character boundary, so a multi-byte character is never split. Advance `offset` by the
byte length of what you got back. An empty return with `offset` still below the length means
`max-bytes` was too small for the next character.

`form` picks which rendering of the input to read:

| Form | Text |
|---|---|
| `as-given` | Exactly what this attempt's agent loop was handed, including any feedback a previous [reopen](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task) appended. What an output gate judges against. |
| `original` | The task before any reopen feedback was appended. What an archiver or cost-attribution hook wants. |

On a task that has never been reopened the two are identical.

### When a task is readable

A task is in scope from the moment one of its attempts enters the agent loop until the runtime
finishes with that task, reopens included.

| Lifecycle event | `input-*` | `output-*` |
|---|---|---|
| `on-stage` | `no-task` | `no-task` |
| `on-session-start` | `no-task` | `no-task` |
| `on-task-start` | `no-task` | `no-task` |
| `on-inference`, `on-tool-call`, `on-shell`, `on-compaction` | This attempt's input | `no-output` until the loop finishes |
| `on-task-end` | This attempt's input | This attempt's result text, or `no-output` if it ended without one |
| `on-session-end` | `no-task` | `no-task` |

`on-task-start` fires before the task enters the loop and `on-session-end` after the last task has
left it, so an archiver binds to `on-task-end`. On the second attempt of a reopened task the output
is cleared at attempt start: a hook judging attempt 2 never sees attempt 1's result.

An `execution_mode: async` hook reads whatever is in scope when its worker reaches the call, which
may be after the task has left scope. Bind a hook that reads the task as `blocking`.

| Error | Meaning |
|---|---|
| `not-granted` | The hook's entry does not declare `capabilities.task_io.read: true` |
| `no-task` | No task is in scope at this lifecycle event |
| `no-output` | A task is in scope but the loop produced no result text |
| `out-of-range` | `offset` is past the end of the value, or inside a multi-byte character |

---

## `murmur:conversation/read`

A runtime-provided import available to any hook component that declares it. It hands a hook the
capsule's [durable conversation record](workdir.md#the-conversation-record) — every message the
runtime put in front of the model, newest first, paged — with no filesystem grant and no other
artifact involved. Nothing is preopened under `~/.murmur/conversations/`; this interface is the
only way in. The runtime is the only writer, and there is no write function.

Reading is granted per hook with
[`capabilities.conversation.read: true`](manifest.md#hook-capabilities) on that hook's entry in the
capsule manifest. A hook without the key still links and still runs — `read-messages` returns
`not-granted`.

| Function | Returns |
|---|---|
| `read-messages(cursor, limit)` | One `message-page`: `messages`, `next-cursor`, `total` |

`messages[0]` is the most recently appended message, and each carries the `id` its record line
holds. `total` is the number of messages in the whole record, not in this page.

| Field | Meaning |
|---|---|
| `cursor` | Opaque and host-minted. `none` starts at the newest message; pass back the `next-cursor` of the page you just read. |
| `next-cursor` | `none` once a page has reached the oldest message in the record, which is what ends a paging loop. |
| `limit` | Clamped to `1..=100`, so `0` reads one message and a loop always makes progress. |

A cursor stays valid while the runtime appends to the record underneath a paging hook: it names a
position counted from the oldest message.

| Error | Meaning |
|---|---|
| `not-granted` | The hook's entry does not declare `capabilities.conversation.read: true` |
| `invalid-cursor` | A cursor the host did not mint, or one past the end of the record |
| `unavailable: <reason>` | The record exists and could not be read |

A record that does not exist is not an error: the read succeeds with no messages and `total: 0`.
That is what a hook sees before the first task of a launch, under
[`context.record: off`](manifest.md#context-record), and in a capsule running
`inference.transport: process` — whose CLI owns its own conversation, so the runtime puts no
message list in front of a model and writes no record.

The record in scope is the one belonging to the task the hook is being dispatched for, which is
the `context-id` on `task-start-event`. A hook bound to `on-task-start` therefore reads the
conversation its task is about to continue.

---

## Package versioning

Every `murmur:*` package declares an explicit `@x.y.z` version, so the contract a compiled
`.wasm` artifact was built against is readable from the component binary itself.

| Package | Version |
|---|---|
| `murmur:tool` | `0.1.0` |
| `murmur:tool-registry` | `0.1.0` |
| `murmur:capsule` | `0.1.0` |
| `murmur:artifact-manager` | `0.1.0` |
| `murmur:shell` | `0.1.0` |
| `murmur:message` | `0.1.0` |
| `murmur:task` | `0.1.0` |
| `murmur:task-io` | `0.1.0` |
| `murmur:conversation` | `0.1.0` |
| `murmur:text` | `0.1.0` |
| `murmur:hook` | `0.6.0` |
| `murmur:runtime` | `0.3.0` |
| `murmur:host` | `0.1.0` |
| `murmur:runtime-guest` | `0.1.0` |

| Tier | When |
|---|---|
| Patch | Doc or comment edits, no ABI change |
| Minor | A wholly new function or interface the runtime treats as optional to export |
| Breaking | Any signature change, any change to an existing `record` or `variant`, or removing or renaming a function or interface |

Every `murmur:*` package is pre-1.0, so the minor field is the breaking axis: a breaking change to
`0.4.0` ships as `0.5.0`, not `1.0.0`. Only a patch moves the patch field.

Adding a field to an existing `record` or a case to an existing `variant` is always breaking,
never minor: every field and case is positional in the binary encoding, so an addition changes
the shape of every call that carries the type.

A wholly new interface goes in a **new package at `0.1.0`** when the package that would otherwise
host it already has published consumers. The package version is part of every instance name in
that package, so a minor bump renames interfaces the new one has nothing to do with, and every
artifact importing one of them stops loading until rebuilt. The exception is a change set that
already forces that rebuild for another reason, in which case the new interface joins the existing
package instead: `murmur:runtime/tokens` sits in `murmur:runtime` because the same bump that
introduced it took `murmur:hook` to `0.6.0` and rebuilt every hook regardless.

**One accepted version per interface.** The runtime resolves each interface by its versioned name
and nothing else — there is no compatibility fallback for an earlier version or for an
unversioned name. An artifact built against a different version fails to load, with an error
naming the version the runtime expects and a rebuild hint (`mur install` for a default artifact,
or a source rebuild otherwise). When a package is bumped, every artifact exporting it must be
rebuilt and republished.

Full policy in
[`wit/VERSIONING.md`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/VERSIONING.md).
