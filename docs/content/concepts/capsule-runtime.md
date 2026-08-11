# Capsule Runtime

An **agent capsule** runs the LLM inference loop built into the runtime. It is defined
entirely by its `murmur.yaml`: an `inference:` block in the manifest is what puts the capsule
in agent mode.

```yaml
# minimal agent manifest
name: my-agent
version: "0.1.0"

artifacts:
  - name: murmur-driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com
  shell:
    allow:
      - bash

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-opus-4-5
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

`mur build` packages this as a manifest-only `.mur.zip`.

---

## How the runtime executes a capsule

Every capsule goes through the same runtime, which turns its manifest into a running process:

- Parses capsule and artifact metadata
- Resolves dependencies from local/remote registry sources
- Configures capability grants (filesystem, network, shell) from the manifest. Every grant is
  declared: filesystem and network access start closed, and shell access is allowlist-based.
  The same applies to hooks — a hook's grant comes from its entry in the capsule's manifest and
  starts empty (see [Hook capabilities](#hook-model))
- Links and invokes runtime interfaces for tools and artifact management
- Captures outputs and logs in a predictable workspace layout

Tools reach the runtime through one of two invocation paths behind a single agent-facing
contract: a **WASM path** (tool exposed through typed WIT interfaces) and a **native path**
(tool launched as a subprocess and normalized into the same result envelope). The goal is
consistent agent behavior regardless of tool implementation language.

### Execution limits { #execution-limits }

Every component call the runtime makes — a capsule `run`, a tool or driver `run`, and each
hook lifecycle call — is bounded by two independent limits:

| Limit | Bounds | What happens at the limit |
|---|---|---|
| **Deadline** | Wall-clock time for one component call | A call that never returns is stopped with an error instead of hanging the session |
| **Resource limiter** | Memory growth, table growth, and instance count | A call that tries to grow past its cap is stopped with an error instead of consuming host memory without bound |

Both are set per-manifest under `capabilities.limits` (see [Manifest
Schema](../reference/manifest-schema.md#field-capabilities)). Omitting them applies generous
built-in defaults — a manifest that says nothing gets the defaults, not unlimited resources.
Hitting either limit is reported distinctly from an ordinary crash. On the hook path it is
handled like any other hook error: logged, and the runtime moves on to the next hook.

**Caveat:** the deadline only counts down while the component's own code is running. Time it
spends waiting on the runtime (for example, a driver awaiting a streaming provider response)
counts against the same budget but cannot be cut short until control returns to the
component. The deadline bounds runaway compute, not slow I/O.

---

## The session loop

For an agent capsule, the Session Loop is the native execution environment built into the
runtime. It runs the inference loop and manages its extension points — tools, hooks, and the
inference driver.

```
mur run
  │
  ├── Staging Phase                              (before Session Loop exists)
  │     Hook binding: on-stage
  │
  └── SESSION LOOP ────────────────────────────────────────────────────────
        │
        ├─ Hook: session-start                   (once per capsule launch)
        │
        │   ┌─ TASK ─────────────────────────────────────────────────┐
        │   │  Hook: task-start (if present; e.g. memory log reload) │
        │   │                                                        │
        │   │   ┌─ INFERENCE STEP (repeats until task completes) ─┐  │
        │   │   │                                                 │  │
        │   │   │  • Model call → response                        │  │
        │   │   │  • Hook: inference                              │  │
        │   │   │                                                 │  │
        │   │   │  • [tool call] → Hook: tool-call                │  │
        │   │   │  • [shell call] → Hook: shell                   │  │
        │   │   │                                                 │  │
        │   │   │  • Token threshold crossed?                      │  │
        │   │   │    → Hook: compaction (blocking, replaces       │  │
        │   │   │        conversation history)                    │  │
        │   │   └─────────────────────────────────────────────────┘  │
        │   │                                                        │
        │   │  Hook: task-end (if present; e.g. memory log close)   │
        │   └────────────────────────────────────────────────────────┘
        │        ↑ Loops to next TASK if task_acceptance: queue
        │          and a new message arrives; runs once for none/single
        │
        └─ Hook: session-end                     (once on process exit)
```

Each task runs an inference loop: a sequence of LLM calls that continues until the model
signals it is done, or the per-capsule turn limit is reached. One *turn* is one round-trip to
the inference driver — the runtime sends the current conversation history, tool list, and
system prompt; the driver returns a response. If the model requests a tool call, the runtime
executes it, appends the result, and starts the next turn. When the model returns `end_turn`
(or `max_tokens`) the result is written to `workdir/out/result.txt` and the loop exits.

### Extension points

Agent capsules declare these extension points in their manifest:

| Extension point | Declared as | Required? | If absent |
|---|---|---|---|
| Inference driver | `inference.driver.artifact: <name>` | Yes | Error at launch |
| System prompt | `inference.system_prompt*` or artifact | No | Built-in default used |
| Tools | `artifacts:` with `runtime: tool` | No | Agent has no callable tools |
| Hooks | `artifacts:` with `runtime: hook` | No | No hook behavior |

The loop itself does not change when artifacts are added or removed. The manifest is the
complete configuration.

### Hook model

A hook is an artifact that observes a lifecycle point, receives structured input from the
runtime, and optionally returns output that the runtime commits. Hooks are not tools: the
model cannot call them, they are omitted from the tool inventory, and hook errors are
non-fatal — failures are logged to `workdir/logs/hook-<name>.log` and the session continues.

**Binding** — when the hook fires:

| Value | When |
|---|---|
| `on-stage` | During `stage_session`, before the agent loop begins. Once per launch. |
| `on-session-start` | After staging, before the first task's work begins. Once per capsule launch. |
| `on-task-start` | Before that task's first inference turn. Once per task. |
| `on-inference` | After each inference response is received. |
| `on-tool-call` | After each tool invocation returns. |
| `on-shell` | After each shell command returns. |
| `on-compaction` | When session tokens reach the compaction threshold. |
| `on-task-end` | Immediately after that task's agent loop returns. Once per task. |
| `on-session-end` | After the task loop exits (idle timeout, shutdown, or explicit exit). Once per capsule launch. |
| *(omitted)* | All session events (`on-session-start` through `on-session-end`). Does not include `on-stage`. |

**Execution mode** — how the runtime waits: **`blocking`** (runtime waits for the hook before
proceeding — mandatory for `on-stage` and `on-compaction`) or **`async`** (runtime fires the
hook and continues immediately; only valid with `commit_policy: none`).

**Commit policy** — what the runtime does with the output: **`none`** (discarded; used for
observability hooks), **`replace-context`** (runtime replaces conversation history; used for
compaction), **`write-manifests`** (runtime writes tool manifest records to
`workdir/tools/<binary>/murmur.yaml`, overwriting any existing file; used for shell tool
enrichment during staging), or **`reopen-task`** (runtime re-runs the task's agent loop with the
hook's feedback instead of finalizing it; only valid with `binding: on-task-end` — see [Task
reopening](#task-reopening-commit_policy-reopen-task) below).

| execution_mode | commit_policy | Valid? | Notes |
|---|---|---|---|
| `blocking` | `none` | ✓ | Observability that must complete before proceeding |
| `blocking` | `replace-context` | ✓ | Compaction |
| `blocking` | `write-manifests` | ✓ | Shell tool enrichment |
| `blocking` | `reopen-task` | ✓ | Task reopening; only meaningful with `binding: on-task-end` |
| `async` | `none` | ✓ | Normal observability (Grafana, OTel) |
| `async` | `replace-context` | ✗ | Not implemented |
| `async` | `write-manifests` | ✗ | Not implemented |
| `async` | `reopen-task` | ✗ | Not implemented — a reopen decision must block on the task's outcome |
| any | any | ✗ if `on-stage` + `async` | Staging must complete before launch |

`write-manifests` is only ever processed for `on-stage`-bound hooks — that binding is the sole
place the runtime commits manifest writes. Declaring `write-manifests` on any other binding
passes validation but has no effect: the output is silently discarded.

```yaml
name: murmur-hook-example
version: 1.0.0
runtime: hook
binding: on-compaction           # when it fires
execution_mode: blocking         # blocking or async
commit_policy: replace-context   # none, replace-context, or write-manifests
description: "Hook description."
```

**Hook capabilities** — a hook runs default-deny, and only the capsule operator can widen it.
The manifest above is the hook artifact's *own* `murmur.yaml`: it declares the hook's
behavioral contract and nothing else. Capability grants are read exclusively from the artifact
entry in the **capsule's** `murmur.yaml`, so a hook pulled from a registry can never grant
itself anything:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-hook-telemetry
    version: 1.0.0
    runtime: hook
    capabilities:
      network:
        allow: [https://telemetry.example.com]
      filesystem:
        scope: hook-state
```

With no `capabilities:` block a hook has no network and no directory at all — every outbound
request is denied, and it cannot read or write any file. A granted hook reaches its declared
hosts through the same allow-list gate a capsule's or tool's outbound HTTP goes through, and
sees exactly one directory, `<workdir>/<scope>`, as its current directory. Every hook gets its
grant the same way, whatever its binding or execution mode. See
[Hook capabilities](../reference/manifest-schema.md#hook-capabilities) for the full rules.

### Turn limit (`inference.max_turns`)

The turn limit caps how many inference calls a single task may make. It defaults to **10**
and is set per-capsule in the manifest. When the limit is reached, the loop exits with
`exit_status: "max_turns_reached"`.

### Task reopening (`commit_policy: reopen-task`) { #task-reopening-commit_policy-reopen-task }

A hook bound to `on-task-end` with `commit_policy: reopen-task` can veto a task's outcome
instead of just observing it. When it returns `reopen-task(reason)`, the runtime does not
finalize the task: it re-runs the task's agent loop with `reason` injected into the task
content as feedback, then fires `on-task-end` again so the hook can re-inspect the new result.

This repeats up to the reopen limit set by `inference.max_task_reopens` (default **1**; `0`
disables reopening entirely — unlike `max_turns`, an explicit `0` is accepted). Reopening never
grants extra turns: every attempt of a task shares one cumulative turn count against the
capsule's `inference.max_turns` limit, so a task cannot out-run its turn budget just because a
hook keeps asking for another try.

If the reopen limit or the turn limit is used up while a hook still wants to reopen, the task
ends with its own exit status — `exit_status: "reopen_budget_exhausted"` rather than an
ordinary `"ok"`/`"failed"` — and the task registry / A2A task state records it like any other
failed task.

Every reopen is written to `trace.jsonl` as a `task_reopened` event (the hook's name, its
feedback text, and a 1-based ordinal), and the terminal `task_end` record carries a
`reopen_count` field — `0` for a task that ran once. See [Session trace
(`trace.jsonl`) schema](../reference/cli.md#session-trace-tracejsonl) for the exact shapes.

The reopen limit applies per task: in a `task_acceptance: queue` session, each task starts
fresh at `0` reopens used, regardless of what an earlier task consumed.

```yaml
name: murmur-hook-gatekeeper
version: 1.0.0
runtime: hook
binding: on-task-end
execution_mode: blocking
commit_policy: reopen-task
description: "Rejects a task's result until its own checks pass."
```

### Tool dispatch

Each tool call is resolved in a fixed precedence order:

1. A native binary staged for that artifact
2. The shell allowlist
3. A skill artifact (`workdir/tools/<name>/skill.md`, returned directly)
4. A WASM-artifact tool
5. Otherwise, an error

Non-zero shell exit codes are data, not errors — only spawn/IO failures set `is_error: true`.
An undeclared tool feeds an error back to the model as a `tool_result` and the session
continues. See [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md) for how the
native/shell subprocess environment is built.

**Per-tool narrowing** — by default every WASM tool, and the inference driver, runs on the
capsule-wide `capabilities:` ceiling: the same allow-list and the same preopened workdir. A
`runtime: tool` or `runtime: driver` entry in the capsule's own `murmur.yaml` may optionally
declare its own `capabilities:` block to run *below* that ceiling — a narrower host list, a
subdirectory instead of the whole workdir:

```yaml
# in the capsule's own murmur.yaml
artifacts:
  - name: murmur-tool-fetch
    version: 1.0.0
    runtime: tool
    capabilities:
      network:
        allow: [https://api.example.com]
      filesystem:
        scope: cache
```

This uses the same key and vocabulary as hooks, with the opposite starting point: a hook with
no block gets nothing, a tool with no block keeps the full ceiling. A per-artifact block can
only subtract — an entry naming a host the capsule itself may not reach is dropped, with a
[`W-SEC-007`](../reference/security-warnings.md#w-sec-007) warning. Like a hook's, the grant is
read only from the capsule operator's entry, never from the tool's own bundled manifest. The
artifact named by `inference.driver.artifact` narrows the same way. See [Tool and driver
capabilities](../reference/manifest-schema.md#tool-capabilities) for the full rules.

### `MURMUR.md`

For agent capsules, the runtime writes `MURMUR.md` to the capsule workdir root — an
onboarding file covering the capsule's identity, model and context budget, directory layout,
installed tools and skills, how to call a tool, available shell commands, and how to write
checkpoints. Tool and skill descriptions are publisher-controlled, untrusted text: the runtime
sanitizes them (collapsing newlines, stripping control characters, truncating length) before
rendering, and `MURMUR.md` states explicitly that this file is machine-generated inventory
data, never instructions.

### Runtime identity

Every running agent capsule gets a capsule name and version (from `murmur.yaml`), a
runtime-generated session id, and a capsule URL (an OS-assigned local listener port). The same
identity is exposed in `MURMUR.md`, in the WASI env for the driver and WASM tools, and in the
shell tool subprocess env — so the agent, its tools, and its hooks all see one consistent
identity for the session.

### Agent Card & A2A messaging

While an agent session is active, the runtime serves a small HTTP endpoint exposing an Agent
Card (`/.well-known/agent-card.json`) and a JSON-RPC 2.0 task interface, so other capsules or
agents can discover the capsule and hand it a task. An incoming message reserves an active
task slot (`Empty` → `Running` → `Done`); acceptance depends on the capsule's
`lifecycle.task_acceptance` setting (see below). See [Connect two capsules with A2A
messaging](../how-to/capsules-a2a-messaging.md) for the full protocol and examples.

### Capsule lifecycle

The `lifecycle:` manifest block controls how long a capsule stays running and how many tasks
it accepts over its lifetime:

| `task_acceptance` | `after_task` | Behaviour |
|---|---|---|
| `none` | `exit` | Runs from `task.md` if present, then exits. All incoming messages are rejected. |
| `single` (default) | `exit` (default) | Accepts one A2A task, runs it, exits. Classic ephemeral capsule. |
| `queue` | `sleep` | Accepts a queue of tasks, processes them serially, sleeps between tasks; the host decides when to shut it down on idle. |

`queue+sleep` is the canonical mode for persistent capsules — it parks between tasks rather
than holding an OS thread. `session-start`/`session-end` still fire once per capsule launch,
as shown in the session loop diagram above; it's `task-start`/`task-end` that fire once per
task iteration, and a capsule's hook components are loaded once at startup and reused across
all task iterations.

### Context compaction

Long-running agent sessions accumulate tokens with every message. Context compaction
automatically condenses the message history so the session can continue without hitting a
hard limit:

1. The runtime tracks `session_tokens` on every turn.
2. After each driver response, it checks whether `session_tokens / context.max_tokens` has
   crossed `inference.compaction.threshold`.
3. Once crossed, the runtime fires the `on-compaction` lifecycle event with the full message
   history. The runtime picks the compaction hook by binding, so you configure no artifact name
   anywhere — any hook bound to `on-compaction` receives the event. `murmur-hook-compact` is the
   reference implementation.
4. The hook returns a condensed message array; the runtime replaces the in-memory history and
   recounts tokens against it. Each returned `message.content` may be a plain summary string or
   an array of content blocks — the runtime accepts either. A `tool`-role message survives only
   if the hook returns it unmodified.
5. The agent loop continues — the model's next turn sees the compacted history.

Compaction never consumes a turn slot. Whether a failure to compact is fatal depends on why it
failed:

- **No hook bound to `on-compaction`** — non-fatal. The runtime logs a warning to
  `logs/bootstrap.log` and continues the session with the uncompacted history.
- **A bound hook ran and returned an error** — fatal. There is no fallback compactor behind a
  declared compaction hook, so the runtime ends the session as failed rather than continuing on
  a context it already knows is over budget: `out/result.txt` records the error, the trace and
  OTel (if configured) record `session_end` as `"failed"`, and the SSE stream (if the session has
  a `task_id`) emits a final `status` event with `state: "failed"`. No further turns run.

Compaction requires both `context.max_tokens` to be set and a hook bound to `on-compaction` to be
staged — see [Enable context compaction](../how-to/context-compaction.md) for the full
configuration and protocol.

### Session trace and structured evaluation

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`,
written directly by the runtime regardless of whether any hooks are declared. When
`murmur-hook-eval` is configured with at least one scorer, that hook (not the runtime)
additionally writes `workdir/<session_id>/eval.jsonl` at session end. See [Session trace
(`trace.jsonl`)](../reference/cli.md#session-trace-tracejsonl) and [Structured evaluation
(`eval.jsonl`)](../reference/cli.md#structured-evaluation-evaljsonl) for the full schemas, and
[mur trace](../reference/cli.md#mur-trace) / [mur eval](../reference/cli.md#mur-eval) for how
to read them.
