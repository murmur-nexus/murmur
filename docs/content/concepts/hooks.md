# Hooks

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
| `on-compaction` | When session tokens reach the [compaction threshold](context.md). |
| `on-task-end` | Immediately after that task's agent loop returns. Once per task. |
| `on-session-end` | After the task loop exits (idle timeout, shutdown, or explicit exit). Once per capsule launch. |
| *(omitted)* | All session events (`on-session-start` through `on-session-end`). Does not include `on-stage`. |

**Execution mode** — whether the agent loop waits for the hook. **`blocking`** (the default) stops
the loop until the hook returns; **`async`** enqueues the event and lets the loop continue. If the
loop needs the hook's answer, use `blocking`; otherwise `async` keeps the work off the critical
path — which matters for a hook doing network I/O on every event, since a `blocking` one pays that
round trip inline every time. An async hook is one reused instance per session, so in-memory state
persists across calls; its calls are serialized in dispatch order and finish before the session
ends. See [Async hook execution](../reference/manifest.md#hook-overflow) for overflow rules.

**Commit policy** — what the runtime does with the output: **`none`** (discarded; used for
observability hooks), **`replace-context`** (runtime replaces conversation history; used for
compaction), **`write-manifests`** (runtime writes tool manifest records to
`workdir/tools/<binary>/murmur.yaml`, overwriting any existing file; used for shell tool
enrichment during staging), or **`reopen-task`** (runtime re-runs the task's agent loop with the
hook's feedback instead of finalizing it — see [Task
reopening](session-loop.md#task-reopening-commit_policy-reopen-task)), or **`seed-context`**
(runtime places the hook's messages at the head of the task's first message list, under the
`context.seed_budget` ceiling; used for memory). `binding` is the single
source of truth for what a hook commits: each binding honors exactly one arm, so it admits that one
policy plus `none`, and a `commit_policy` the binding cannot honor fails at capsule-staging time —
before the hook component is compiled — with an error naming the binding, the declared policy, and
the policy the binding honors. A hook with no `binding:` receives every event, so any policy is
valid for it — see [Hook contract fields](../reference/manifest.md#hook-contract-fields).

The two fields meet in one rule: a binding that commits an arm must be blocking, because every
committable arm is a decision the agent loop is blocked on — the context it continues from, the
manifests it stages with, the feedback that reopens a task. `async` is available exactly where a
hook commits nothing, so it requires `commit_policy: none`, and `on-stage` may never be `async`.

Of the shipped hooks, `murmur-hook-debug` is the only `async` one, and it is stateless — it appends
one JSON line per event and nothing waits on it. `murmur-hook-grafana` and `murmur-hook-eval`
declare no `execution_mode` and so run `blocking` even though both commit nothing: each buffers
session state in memory across every call and exports it once at `on-session-end`.

```yaml
name: murmur-hook-example
version: 1.0.0
runtime: hook
binding: on-compaction           # when it fires
execution_mode: blocking         # blocking or async
commit_policy: replace-context   # none, replace-context, write-manifests, reopen-task, seed-context
description: "Hook description."
```

A hook's capability grant is read from the capsule's manifest, not from the hook's own — see
[Hook capabilities](access-control.md#hook-capabilities).
