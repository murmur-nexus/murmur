# WIT Interfaces

This page documents the WIT interfaces currently defined under `crates/capsule-runtime/wit/`.

!!! info "Implemented now"
    `murmur:tool/run`, `murmur:tool-registry/invoke`, `murmur:capsule/run`, `murmur:artifact-manager/manage`, `murmur:shell/execute`, `murmur:message/send`, `murmur:task/task`, `murmur:hook/lifecycle`, `murmur:runtime/inference`, plus guest worlds `capsule`, `tool`, `driver`, the `hook` world, and the host-side `runtime-host` world.

---

## `murmur:tool/run`

Defined in `wit/tool.wit`.

```wit
package murmur:tool@0.1.0;

interface run {
  enum status {
    passed,
    failed,
    error,
  }

  record tool-input {
    data: option<string>,
    log-path: option<string>,
  }

  record tool-result {
    status: status,
    summary: option<string>,
    data: option<string>,
    data-path: option<string>,
    truncated: bool,
    metadata: list<tuple<string, string>>,
  }

  run: func(input: tool-input) -> tool-result;
}
```

Use this for tool component implementations.

`metadata` is a free-form key/value channel back to the host. Reserved keys today:

- **`continuation_id`** — an inference driver that holds conversation state provider-side
  (so the host need not resend the full transcript on every turn) opts in by returning this
  key with a non-empty value on any turn's response. See [Stateful driver
  continuation](#stateful-driver-continuation) below for the full protocol. Every driver that
  doesn't set this key sees no behavior change.
- **`state_effect`** — how this call affected the resource it addressed: `"read"` (only
  observed state) or `"mutate"` (changed state). The host records the declared value on the
  call's `tool_call` trace event so tool-agnostic observability — e.g. `mur trace`
  redundant-call detection — can reason about state effects without knowing which tool this
  is. The key is per-call, so a tool with several operations declares the effect appropriate
  to each. Omitting it, or any unrecognized value, means "unknown": the host treats the call
  conservatively (like a mutate for invalidation) and never credits it as a redundant read.
  A tool that declares nothing is never misreported.
- **`resource_id`** — which resource this call addressed, as an opaque, tool-defined string.
  The host records it verbatim on the call's `tool_call` trace event and never parses,
  normalizes, or validates it — two calls address the same resource exactly when their
  `resource_id` values are byte-equal. This is the companion to `state_effect`: `state_effect`
  says *how* a call affected a resource, `resource_id` says *what* resource it was; `mur trace`
  redundant-call detection needs both to credit a call as a redundant read. Without a declared
  `resource_id`, the detector falls back to sniffing a handful of English path-like field names
  (`path`, `file`, `file_path`, `filepath`, `filename`) out of the call's input — a tool whose
  resource is a symbol, URI, or query should declare this key instead, since no input-guessing
  heuristic can recognize its addressing scheme. Omitting the key, or returning an empty-string
  value, means "undeclared" and triggers that fallback. If the list carries more than one
  `resource_id` tuple, the host uses the first in list order. Scope the value by addressing
  scheme (e.g. `"sym:Foo::bar"`) so it can't collide with an unrelated resource that happens to
  share a name — conversely, two different tools that agree on the same scoped string for the
  same underlying resource get correct cross-tool redundancy detection. A tool that never sets
  this key (every tool shipped today) sees no behavior change.

### Stateful driver continuation { #stateful-driver-continuation }

By default the runtime resends the full conversation history on every turn. A driver can opt
out of this by returning a `continuation_id` in `tool-result.metadata` — a signal that it is
holding conversation state provider-side and only needs to be told what changed.

- **Opting in:** any turn's response sets `metadata = [("continuation_id", "<id>")]` with a
  non-empty value. The runtime holds that id for the rest of the current session loop (one
  capsule launch).
- **Next turn:** while a continuation id is held **and** was established under the same
  `context_id` as the turn about to be sent, the runtime sends only the messages appended
  since the driver last acknowledged state, plus the held `continuation_id` as a top-level
  request field — not the full `messages` array.
- **Opting out / dropping:** a response that omits the key, or returns it with an empty
  string, is treated as "not continuing" — the runtime drops the held id and the next turn is
  a full resend.
- **Compaction always drops it:** any `replace-context` hook commit (e.g. compaction) clears
  the held continuation id unconditionally, regardless of which hook produced the
  replacement. The next turn is a full resend of the *post-compaction* history.
- **Cross-context safety:** a continuation established under one `context_id` is never reused
  for a different, unrelated task sharing the same capsule launch — its first turn is always
  a full resend.

A driver that never sets `continuation_id` (every driver shipped today) sees zero behavior
change — this is purely additive. `session_tokens` accounting (used for the compaction
threshold check) is always computed from the full logical `messages` array, never from the
smaller wire payload actually transmitted, so enabling continuation never affects when
compaction fires.

---

## `murmur:tool-registry/invoke`

Defined in `wit/tool-registry.wit`.

```wit
package murmur:tool-registry@0.1.0;

interface invoke {
  use murmur:tool/run@0.1.0.{tool-input, tool-result, status};

  invoke: func(name: string, input: tool-input) -> result<tool-result, string>;
}
```

Capsules import this interface to invoke allowlisted tools by name.

---

## `murmur:capsule/run`

Defined in `wit/capsule.wit`.

```wit
package murmur:capsule@0.1.0;

interface run {
  run: func();
}
```

Capsule component must export this entrypoint.

---

## `murmur:artifact-manager/manage`

Defined in `wit/artifact-manager.wit`.

The host implements this interface for **script capsules** (WASM components that import it). The native agent loop does not use this interface — the runtime accesses artifact state directly in Rust.

`pull` resolves the requested artifact from the session's registry, verifies its
bytes against both the registry's self-reported hash and (if one already exists) the pinned
`murmur.lock` entry for that artifact, then extracts and installs it under
`<workdir>/tools/<name>/` and upserts the result into `murmur.lock` — a mismatch at either
verification step returns `Err` before anything is written to disk or to the lock. The
installed artifact is immediately reflected in `list()`/`describe()` (and, for WASM tools, is
callable via `invoke()`) within the same session. `search` and `remove` remain unimplemented.

```wit
package murmur:artifact-manager@0.1.0;

interface manage {
  enum runtime-type {
    wasm,
    native,
  }

  record artifact-summary {
    name: string,
    version: string,
    runtime: runtime-type,
  }

  record artifact-info {
    name: string,
    version: string,
    description: string,
    tags: list<string>,
    runtime: runtime-type,
    input-schema: option<string>,
    output-schema: option<string>,
  }

  record runtime-state {
    capsule-id: string,
    installed: list<artifact-summary>,
    capabilities: string,
  }

  %list: func() -> list<artifact-summary>;
  describe: func(name: string) -> result<artifact-info, string>;
  search: func(query: string) -> result<list<artifact-summary>, string>;
  pull: func(name: string, version: string) -> result<artifact-summary, string>;
  remove: func(name: string) -> result<bool, string>;
  diagnostics: func() -> result<runtime-state, string>;
}
```

---

## `murmur:shell/execute`

Defined in `wit/shell-execute.wit`.

```wit
package murmur:shell@0.1.0;

interface execute {
  record shell-result {
    exit-code: s32,
    stdout: string,
    stderr: string,
    truncated: bool,
    full-output-path: option<string>,
  }

  run: func(
    binary: string,
    args: list<string>,
    env: list<tuple<string, string>>,
  ) -> result<shell-result, string>;
}
```

This interface describes the contract for native shell execution. Currently the host implements this directly in Rust (not via a WASM component); the WIT definition exists so future guest implementations can bind against the same type shapes.

Key semantics:

- A non-zero `exit-code` is **data**, not an error — `result<shell-result, string>` only errs on spawn/IO failure
- `truncated: true` means output exceeded 16 KiB and was cut; `full-output-path` points to the untruncated log in the workdir
- `binary` must be listed in `capabilities.shell.allow`; callers not in the allowlist are rejected before `run` is invoked

---

## `murmur:task/task`

Defined in `wit/guest/deps/murmur-task/task.wit`.

```wit
package murmur:task@0.1.0;

interface task {
    /// Suspend the agent loop and request external input.
    /// Returns the message content from the follow-up message/send call.
    /// Traps if the optional input_timeout_secs elapses with no response.
    request-input: func(prompt: string) -> string;
}
```

WASM tool components import this interface to suspend the agent loop at a decision boundary
and wait for external input via `message/send`. From the component's perspective the call is
synchronous: pass a prompt string, receive the user's reply string.

**Usage from a tool component:**

```rust
// imports: murmur:task/task
use murmur::task::task::request_input;

let answer = request_input("Which branch should I target?");
// agent loop is suspended here until a message/send delivers the reply
```

**When to call it:**

- The agent has reached a decision it cannot make autonomously (ambiguous requirements, consequential action needing approval, etc.)
- The caller expects a `message/send` follow-up from a human or supervisor capsule before continuing
- Do **not** call it inside a script capsule (`murmur:capsule/run` world) — it is only available
  in the `tool` world and only when the capsule is running an A2A task. Calling it outside that
  context returns an error immediately.

**Timeout:** controlled by `lifecycle.input_timeout_secs` in the manifest (see
[lifecycle.input_timeout_secs](manifest-schema.md#lifecycle-input-timeout-secs)). When the timeout fires the
host function returns a trap, the WASM guest aborts, and the task transitions to `failed`.

---

## `murmur:message/send`

Defined in `wit/message/send.wit`.

```wit
package murmur:message@0.1.0;

interface send {
  record message {
    message-id: string,
    context-id: option<string>,
    text: string,
  }

  record task-result {
    task-id: string,
    context-id: string,
    state: string,
  }

  send: func(peer-url: string, message: message) -> result<task-result, string>;
}
```

Script capsule components (WASM) import this interface to send tasks to peer capsules. The host implements the interface — the capsule never sees the JSON-RPC 2.0 wire format.

**Usage from a capsule component:**

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

**Key behaviours:**

- The peer URL must appear in `capabilities.network.allow` in the sender's manifest. If not, the call returns `Err("network policy: '...' not in capabilities.network.allow")` immediately — no outgoing TCP connection is made.
- The host wraps the call as a JSON-RPC 2.0 `message/send` POST to the peer capsule's endpoint.
- When OTel is configured, the host injects a W3C `traceparent` header on the outgoing request so the peer session span appears as a child of the sender's span.
- `task-result.state` reflects the peer's immediate response (`submitted` or `rejected`). Poll the peer's `tasks/get` endpoint for final state.

---

## `murmur:hook/lifecycle`

Defined in `wit/hook/deps/murmur-hook/lifecycle.wit`.

Hook artifacts export this interface when declared with `runtime: hook`. The native agent loop calls each handler synchronously. Returning `Err(string)` logs the error to `workdir/logs/hook-<name>.log` and does not abort the loop.

```wit
package murmur:hook@0.4.0;

interface lifecycle {
  record message {
    role: string,
    content: string,
  }

  record tool-manifest {
    binary-name: string,
    content: string,
  }

  variant hook-output {
    none,
    replace-context(list<message>),
    write-manifests(list<tool-manifest>),
    artifact(string),
    reopen-task(string),
  }

  record session-context {
    capsule-name: string,
    capsule-version: string,
    session-id: string,
    model: string,
    capabilities: list<string>,
  }

  record stage-event {
    shell-allow: list<string>,
  }

  record task-start-event {
    task-id: string,
    context-id: string,
    source: string,
    input-bytes: u64,
  }

  record task-end-event {
    task-id: string,
    exit-status: string,
  }

  record inference-event {
    turn: u32,
    input-tokens: u64,
    output-tokens: u64,
    decision: string,
    tool-name: option<string>,
    prompt: option<string>,
    output: option<string>,
    tools: option<string>,
  }

  record tool-event {
    turn: u32,
    tool-name: string,
    input-bytes: u64,
    output-bytes: u64,
    duration-ms: u64,
    status: string,
  }

  record shell-event {
    turn: u32,
    command: string,
    exit-code: s32,
    stdout: string,
    stderr: string,
    stdout-bytes: u64,
    stderr-bytes: u64,
    duration-ms: u64,
  }

  record compaction-event {
    messages: list<message>,
    session-tokens: u64,
    threshold: f64,
    model: option<string>,
    system-prompt: option<string>,
  }

  record session-end-event {
    total-turns: u32,
    total-input-tokens: u64,
    total-output-tokens: u64,
    total-tool-calls: u32,
    total-shell-calls: u32,
    duration-ms: u64,
    exit-status: string,
  }

  on-stage: func(event: stage-event) -> result<hook-output, string>;
  on-session-start: func(ctx: session-context) -> result<hook-output, string>;
  on-task-start: func(event: task-start-event) -> result<hook-output, string>;
  on-inference: func(event: inference-event) -> result<hook-output, string>;
  on-tool-call: func(event: tool-event) -> result<hook-output, string>;
  on-shell: func(event: shell-event) -> result<hook-output, string>;
  on-compaction: func(event: compaction-event) -> result<hook-output, string>;
  on-task-end: func(event: task-end-event) -> result<hook-output, string>;
  on-session-end: func(event: session-end-event) -> result<hook-output, string>;
}
```

Event points:

| Handler | Emitted |
|---|---|
| `on-stage` | Once at capsule staging, before any session starts |
| `on-session-start` | Once per capsule launch, before the first task's work begins |
| `on-task-start` | Once per task, before that task's first inference turn. Coincides with `on-session-start` for `task_acceptance: none`/`single`; fires once per queued task for `task_acceptance: queue` |
| `on-inference` | After each inference driver response is parsed |
| `on-tool-call` | After each model-requested tool invocation returns or errors |
| `on-shell` | After each allowed shell command returns |
| `on-compaction` | After a successful context compaction updates message history |
| `on-task-end` | Once per task, immediately after that task's agent loop returns |
| `on-session-end` | Once per capsule launch, after the task loop exits (idle timeout, shutdown, or explicit exit) |

`session-id` is generated once per runtime session as a UUID and remains stable for all hook events in that run.

`on-task-start`/`on-task-end` are optional exports: a hook component compiled before these events existed instantiates normally and is simply never dispatched for them. All other handlers, including `on-stage` through `on-session-end` from the original vocabulary, remain required — a component missing one of those fails instantiation with an error naming the missing function.

**Honored `hook-output` arm per event.** A hook may return any `hook-output` arm from any handler, but the runtime only *commits* one specific arm per event — every other non-`none` arm is discarded. This mapping is fixed and applies uniformly across all nine handlers:

| Handler | Honored arm |
|---|---|
| `on-stage` | `write-manifests` |
| `on-session-start` | none honored (only `none` is silent) |
| `on-task-start` | none honored |
| `on-inference` | `artifact` |
| `on-tool-call` | none honored |
| `on-shell` | none honored |
| `on-compaction` | `replace-context` |
| `on-task-end` | `reopen-task` |
| `on-session-end` | none honored |

`reopen-task` is a *control* arm, not a data arm: returning it from `on-task-end` does not
finalize the task — the runtime re-runs the task's agent loop with the arm's string payload
injected as feedback, subject to `inference.max_task_reopens`/`inference.max_turns`. See [Task
reopening](../concepts/capsule-runtime.md#task-reopening-commit_policy-reopen-task) for the full
mechanism. Returning `reopen-task` from any handler other than `on-task-end` is not honored —
it falls through to the unsupported-arm fault path described next, exactly like any other
non-`none` arm the firing handler does not honor.

Returning `none` from any handler is always silent — this is the normal case for a purely observational hook. Returning the handler's honored arm commits it exactly as described above. Returning any other non-`none` arm (e.g. `write-manifests` from `on-task-end`, or `replace-context` from `on-tool-call`) is **not** an error and does not abort the session — instead it is a loud, non-fatal fault:

- One line is appended to `workdir/logs/hook-<name>.log` naming the hook, the handler, and the discarded arm.
- For every handler except `on-stage` (which runs during capsule staging, before the session's trace file exists), the same fault is also written to `trace.jsonl` as a `hook_dispatch_error` event — see [Session trace (`trace.jsonl`) schema](cli.md#session-trace-tracejsonl).
- An async hook (`execution_mode: async`) that returns an unsupported arm is logged the same way but never produces a trace record, matching how a genuine `Err` from an async hook is already handled today.

`compaction-event.model` / `compaction-event.system-prompt` tell a compaction
hook which model and system prompt to use for its own summarization call.
`model` carries `inference.compaction.model` verbatim (`none` when the manifest
leaves it unset — the receiving hook, not the host, resolves that to a default).
`system-prompt` is still always `none`; manifest wiring for it lands in a later
slice.

**Three accepted lifecycle versions.** The host resolves the lifecycle instance export by
trying `murmur:hook/lifecycle@0.4.0` first, then `@0.3.0`, then `@0.2.0`, and remembers which
matched. Two independent shape differences are handled this way:

- `on-compaction`'s record shape: 5-field on `@0.4.0`/`@0.3.0`, 3-field on `@0.2.0`. A `@0.2.0`
  hook is dispatched the 3-field record; every later version gets the 5-field one.
- `hook-output`'s shape: the current 5-case variant (with `reopen-task`) on `@0.4.0`, the
  pre-`reopen-task` 4-case variant on `@0.3.0`/`@0.2.0`. `TypedFunc::typed` is structural, so a
  4-case guest return cannot be lifted directly against the current 5-case host type — the host
  lifts a `@0.3.0`/`@0.2.0` hook's return through a hand-authored 4-case twin, then widens it
  into the current type. A `@0.2.0`/`@0.3.0` hook can therefore never produce `reopen-task`,
  which is correct: `reopen-task` is a `@0.4.0` capability.

Every other handler's records are shape-identical across all three versions and need no
special-casing. This is a transitional exception scoped to exactly these versions of one
package, **not** a return of the removed unversioned fallback: a hook exporting the bare
`murmur:hook/lifecycle` name still fails hard.

---

## `murmur:runtime/inference`

Defined in `wit/hook/inference.wit`. A **host-provided import**, available to any
hook component that declares it — nothing needs to be exported and nothing else
in the manifest changes.

```wit
package murmur:runtime@0.2.0;

interface inference {
  use murmur:hook/lifecycle@0.4.0.{message};

  record inference-request {
    messages: list<message>,
    system-prompt: option<string>,
    model: option<string>,
  }

  record inference-response {
    text: string,
    model-used: string,
    input-tokens: u64,
    output-tokens: u64,
  }

  run-inference: func(request: inference-request) -> result<inference-response, string>;
}
```

`run-inference` runs exactly one LLM completion through the capsule's
already-configured inference driver (`inference.driver.artifact`), reusing the
same driver-component invocation an ordinary agent turn uses — there is no second
HTTP client and no separate credential path.

- `model: none` resolves to the manifest's primary `inference.model`.
- `model: some(m)` sends `m`. If the driver or provider rejects it the call
  returns `err`; the host **never** silently falls back. A caller that wants
  fallback-on-failure calls `run-inference` again with `none`, so each attempt
  leaves its own truthful trace record and OTel span.
- `system-prompt: none` sends no system prompt.
- `model-used` is the model string the host actually sent, and `input-tokens` /
  `output-tokens` are host-side tiktoken counts of the request payload and the
  raw driver response — the driver wire format carries neither a usage block nor
  a model confirmation (see the inference message format reference).
- With no driver configured, `run-inference` returns a clear `err` naming
  `inference.driver.artifact`; the import still links, so the hook itself runs.

Every call, success or failure, writes one `inference` record to `trace.jsonl`
and one `capsule.inference` OTel span carrying `origin: "hook:<hook name>"` and
`model`. An ordinary agent-loop turn writes neither field, so its records are
byte-identical to what they were before this interface existed.

---

## Runtime worlds

Guest-side worlds — the worlds WASM artifact source code compiles against with
`wit_bindgen::generate!` — are defined in `wit/guest/worlds.wit`:

```wit
package murmur:runtime-guest@0.1.0;

world capsule {
  import murmur:tool-registry/invoke@0.1.0;
  export murmur:capsule/run@0.1.0;
}

world tool {
  import murmur:task/task@0.1.0;
  import murmur:text/chunks@0.1.0;
  export murmur:tool/run@0.1.0;
}

world driver {
  import murmur:text/chunks@0.1.0;
  export murmur:tool/run@0.1.0;
}
```

The hook world is defined in `wit/hook/worlds.wit`:

```wit
package murmur:runtime@0.2.0;

world hook {
  import inference;
  export murmur:hook/lifecycle@0.4.0;
}
```

`inference` is `murmur:runtime/inference@0.2.0`, declared in the same package as
the world (`wit/hook/inference.wit`), which is why it is referenced by its bare
interface name.

The host side — the interfaces the runtime provides, compiled by
`wasmtime::component::bindgen!` in `src/bindings.rs` — is the `runtime-host`
world in `wit/host/host.wit`:

```wit
package murmur:host@0.1.0;

world runtime-host {
  import murmur:artifact-manager/manage@0.1.0;
  import murmur:shell/execute@0.1.0;
  import murmur:tool-registry/invoke@0.1.0;
  import murmur:message/send@0.1.0;
}
```

!!! note "`world agent` was removed"
    Previously, a `world agent` world existed for the WASM-based agent bootstrap. It has
    been removed. The inference loop now runs as native Rust inside the capsule runtime; agent
    capsules are manifest-only and do not compile against any WIT world.

---

## Package versioning

Every `murmur:*` package declares an explicit `@x.y.z` version, so the interface
contract a compiled `.wasm` artifact was built against is readable from the
component binary itself.

| Package                   | Version |
| -------------------------- | ------- |
| `murmur:tool`               | `0.1.0` |
| `murmur:tool-registry`      | `0.1.0` |
| `murmur:capsule`            | `0.1.0` |
| `murmur:artifact-manager`   | `0.1.0` |
| `murmur:shell`              | `0.1.0` |
| `murmur:message`            | `0.1.0` |
| `murmur:task`                | `0.1.0` |
| `murmur:hook`                | `0.4.0` |
| `murmur:runtime`            | `0.2.0` |
| `murmur:runtime-guest`      | `0.1.0` |

**Version-bump policy** (full policy in `crates/capsule-runtime/wit/VERSIONING.md`):

- **Patch** (`x.y.Z`) — doc/comment-only edits, no ABI change.
- **Minor** (`x.Y.0`) — a wholly new function or interface the host treats as
  optional to export. Never a field added to an existing `record` or a case
  inserted into an existing `variant` — the Component Model's canonical ABI is
  positional, so those are always breaking.
- **Major** (`X.0.0`) — any signature change to an existing function, any shape
  change to an existing `record`/`variant`, or removing/renaming a function or
  interface.

**Unversioned artifacts (transition window closed):** a transitional dual-accept runtime
resolved the three dynamically-instantiated guest
interfaces — `murmur:capsule/run`, `murmur:tool/run`, `murmur:hook/lifecycle` —
by trying the versioned instance name first, then falling back to the legacy
unversioned name, so artifacts built before versioning kept running unmodified.
That fallback has since been **removed**: the host now resolves each interface by
its versioned instance name only (`murmur:capsule/run@0.1.0`,
`murmur:tool/run@0.1.0`, `murmur:hook/lifecycle@0.4.0`). A tool, capsule, or hook
artifact that still exports only an unversioned `package
murmur:tool;`/`murmur:capsule;`/`murmur:hook;` interface no longer instantiates —
it fails with a hard error naming the versioned interface the host expected and a
rebuild hint (`mur install` for a default artifact, or a source rebuild
otherwise). Rebuild and republish any artifact still exporting only the
unversioned name. The one exception is the `murmur:hook/lifecycle@0.4.0` →
`@0.3.0` → `@0.2.0` fallback described above, which is scoped to three specific
versions of a single package and does not reopen the unversioned window.

See `crates/capsule-runtime/wit/README.md` for which build consumes each copy
of the tree, and `crates/capsule-runtime/wit/VERSIONING.md` for the version
policy.
