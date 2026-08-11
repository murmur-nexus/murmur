# WIT Interfaces

WASM artifacts — capsules, tools, drivers, and hooks — talk to the runtime through WIT
interfaces. This page lists each interface, who uses it, and what its behaviour means for you.
The type definitions themselves live in the repository under
[`crates/capsule-runtime/wit/`](https://github.com/murmur-nexus/murmur/tree/main/crates/capsule-runtime/wit);
each row below links to its source.

## Interfaces

| Interface | Version | Used by | Purpose |
|---|---|---|---|
| [`murmur:tool/run`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/tool.wit) | `0.1.0` | Exported by tool and driver components | The entrypoint the runtime calls to run a tool or an inference driver |
| [`murmur:tool-registry/invoke`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/tool-registry.wit) | `0.1.0` | Imported by capsule components | Call an allowlisted tool by name |
| [`murmur:capsule/run`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/capsule.wit) | `0.1.0` | Exported by capsule components | The capsule entrypoint |
| [`murmur:artifact-manager/manage`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/artifact-manager.wit) | `0.1.0` | Provided by the runtime to capsule components | List, describe, and pull artifacts during a session |
| [`murmur:shell/execute`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/shell-execute.wit) | `0.1.0` | Provided by the runtime | Run an allowlisted shell binary |
| [`murmur:message/send`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/message/send.wit) | `0.1.0` | Provided by the runtime to capsule components | Send an A2A task to a peer capsule |
| [`murmur:task/task`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/deps/murmur-task/task.wit) | `0.1.0` | Imported by tool components | Pause the agent loop and wait for external input |
| [`murmur:text/chunks`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/deps/murmur-text/stream.wit) | `0.1.0` | Imported by tool and driver components | Emit response and thinking chunks to the session's SSE stream |
| [`murmur:hook/lifecycle`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/deps/murmur-hook/lifecycle.wit) | `0.5.0` | Exported by hook artifacts | The lifecycle handlers the runtime calls |
| [`murmur:runtime/inference`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/inference.wit) | `0.2.0` | Provided by the runtime to hook components | Run one LLM completion through the capsule's configured driver |

## Worlds

A world is what your component's source compiles against with `wit_bindgen::generate!`.

| World | Imports | Exports | Defined in |
|---|---|---|---|
| `capsule` | `tool-registry/invoke` | `capsule/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `tool` | `task/task`, `text/chunks` | `tool/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `driver` | `text/chunks` | `tool/run` | [`guest/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/guest/worlds.wit) |
| `hook` | `runtime/inference` | `hook/lifecycle` | [`hook/worlds.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/hook/worlds.wit) |
| `runtime-host` | The interfaces the runtime provides to guests | — | [`host/host.wit`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/host/host.wit) |

Agent capsules compile against no world — the inference loop is native Rust inside the runtime,
and the capsule is defined by its manifest alone.

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
anything about your tool. Omit either and the call counts as "unknown": the runtime treats it
conservatively and never credits it as a redundant read, so a tool that declares nothing is
never misreported.

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

---

## `murmur:artifact-manager/manage`

Lets a capsule component inspect and install artifacts mid-session. `pull` resolves an artifact
from the session's registry, verifies its bytes against the registry hash and any pinned
`murmur.lock` entry, then installs it under `<workdir>/tools/<name>/` and updates
`murmur.lock`. A hash mismatch returns an error before anything is written. A pulled artifact is
immediately visible to `list()` and `describe()` and, for WASM tools, callable via `invoke()`.

`search` and `remove` are not implemented yet.

The native agent loop does not use this interface; it accesses artifact state directly.

---

## `murmur:shell/execute`

The contract for running an allowlisted shell binary. The runtime implements it in Rust; the WIT
definition exists so future guest implementations bind against the same types.

| Behaviour | Detail |
|---|---|
| Non-zero exit code | Data, not an error — the call only errs on spawn/IO failure |
| `truncated: true` | Output exceeded 16 KiB and was cut; `full-output-path` points at the untruncated log in the workdir |
| Allowlist | `binary` must appear in `capabilities.shell.allow`; anything else is rejected before `run` is reached |

---

## `murmur:task/task`

`request-input` pauses the agent loop at a decision boundary and waits for a reply delivered by
`message/send`. From the component's perspective the call is synchronous: pass a prompt string,
receive the reply string.

```rust
// imports: murmur:task/task
use murmur::task::task::request_input;

let answer = request_input("Which branch should I target?");
// the agent loop is suspended here until a message/send delivers the reply
```

Call it when the agent has reached a decision it cannot make on its own — ambiguous
requirements, or a consequential action that needs approval — and a human or supervisor capsule
is expected to reply.

It is available in the `tool` world only, and only while the capsule is running an A2A task.
Called anywhere else, it returns an error immediately.

**Timeout:** `lifecycle.input_timeout_secs` in the manifest (see
[lifecycle.input_timeout_secs](manifest-schema.md#lifecycle-input-timeout-secs)). When it fires,
the component is aborted and the task transitions to `failed`.

---

## `murmur:message/send`

Sends a task to a peer capsule. The runtime handles the JSON-RPC 2.0 wire format, so the capsule
never sees it.

```rust
// imports: murmur:message/send
let result = send::send(
    "localhost:52322",
    send::Message {
        message_id: "m1".to_string(),
        context_id: Some("ctx-1".to_string()),
        text: "Summarise the file at /data/report.csv".to_string(),
    },
)?;
// result.state == "submitted" | "rejected" | "working" | "completed" | "failed"
```

| Behaviour | Detail |
|---|---|
| Allowlist | The peer URL must appear in the sender's `capabilities.network.allow`. Otherwise the call returns `Err("network policy: '...' not in capabilities.network.allow")` and no connection is made. |
| Tracing | With OTel configured, the runtime injects a W3C `traceparent` header so the peer's session span nests under the sender's. |
| Result state | `task-result.state` is the peer's immediate response (`submitted` or `rejected`). Poll the peer's `tasks/get` endpoint for the final state. |

---

## `murmur:hook/lifecycle`

Hook artifacts (`runtime: hook`) export this interface. The runtime calls each handler
synchronously. Returning an error logs it to `workdir/logs/hook-<name>.log` and the session
continues.

| Handler | Fires |
|---|---|
| `on-stage` | Once at capsule staging, before any session starts |
| `on-session-start` | Once per capsule launch, before the first task's work begins |
| `on-task-start` | Once per task, before that task's first inference turn |
| `on-inference` | After each inference driver response is parsed |
| `on-tool-call` | After each model-requested tool invocation returns or errors |
| `on-shell` | After each allowed shell command returns |
| `on-compaction` | After a successful context compaction updates message history |
| `on-task-end` | Once per task, immediately after that task's agent loop returns |
| `on-session-end` | Once per capsule launch, after the task loop exits |

With `task_acceptance: none` or `single`, `on-task-start` coincides with `on-session-start`; with
`queue` it fires once per queued task. `session-id` is a UUID generated once per run and stable
across every hook event in that run.

`on-task-start` and `on-task-end` are optional exports — a hook compiled before these events
existed still loads and is simply never dispatched for them. Every other handler is required; a
component missing one fails to load with an error naming it.

### What each handler can commit

A handler may return any `hook-output` arm, but the runtime commits only one arm per event:

| Handler | Committed arm |
|---|---|
| `on-stage` | `write-manifests` |
| `on-inference` | `artifact` |
| `on-compaction` | `replace-context` |
| `on-task-end` | `reopen-task` |
| All others | none — only `none` is silent |

Returning `none` is always silent, and is the normal case for an observational hook. Returning
any other non-`none` arm is a loud but non-fatal fault:

- A line naming the hook, the handler, and the discarded arm is appended to
  `workdir/logs/hook-<name>.log`.
- A `hook_dispatch_error` event is written to `trace.jsonl` — see [Session trace
  schema](cli.md#session-trace-tracejsonl). The exceptions are `on-stage`, which runs before the
  trace file exists, and `async` hooks, which never produce a trace record.

`reopen-task` is a control arm: returning it from `on-task-end` re-runs the task's agent loop
with the arm's string injected as feedback, subject to `inference.max_task_reopens` and
`inference.max_turns`. See [Task
reopening](../concepts/capsule-runtime.md#task-reopening-commit_policy-reopen-task).

### Event field notes

- `compaction-event.model` carries `inference.compaction.model` verbatim so a compaction hook
  knows which model to use for its own summarization call. It is absent when the manifest leaves
  the field unset, and the hook resolves its own default. `compaction-event.system-prompt` is
  always absent today; manifest wiring for it lands later.
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
(`inference.driver.artifact`), reusing the same driver invocation an ordinary agent turn uses —
one HTTP client, one credential path.

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
| `murmur:text` | `0.1.0` |
| `murmur:hook` | `0.5.0` |
| `murmur:runtime` | `0.2.0` |
| `murmur:runtime-guest` | `0.1.0` |

| Bump | When |
|---|---|
| Patch (`x.y.Z`) | Doc or comment edits, no ABI change |
| Minor (`x.Y.0`) | A wholly new function or interface the runtime treats as optional to export |
| Major (`X.0.0`) | Any signature change, any change to an existing `record` or `variant`, or removing or renaming a function or interface |

Adding a field to an existing `record` or a case to an existing `variant` is always a major bump:
the Component Model's ABI is positional, so both are breaking.

**One accepted version per interface.** The runtime resolves each interface by its versioned name
and nothing else — there is no compatibility fallback for an earlier version or for an
unversioned name. An artifact built against a different version fails to load, with an error
naming the version the runtime expects and a rebuild hint (`mur install` for a default artifact,
or a source rebuild otherwise). When a package is bumped, every artifact exporting it must be
rebuilt and republished.

Full policy in
[`wit/VERSIONING.md`](https://github.com/murmur-nexus/murmur/blob/main/crates/capsule-runtime/wit/VERSIONING.md).
