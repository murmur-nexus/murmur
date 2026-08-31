# Session Loop

For an agent capsule, the Session Loop is the native execution environment built into the
runtime. It runs the inference loop and manages its extension points — [tools](tools.md),
[hooks](hooks.md), and the [inference driver](artifacts.md).

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

## `MURMUR.md`

For agent capsules, the runtime writes `MURMUR.md` to the capsule workdir root — an
onboarding file covering the capsule's identity, model and context budget, directory layout,
installed tools and skills, how to call a tool, available shell commands, and how to write
checkpoints.

## Turn limit (`inference.max_turns`)

The turn limit caps how many inference calls a single task may make. It defaults to **10**
and is set per-capsule in the manifest. When the limit is reached, the loop exits with
`exit_status: "max_turns_reached"`.

## Task reopening (`commit_policy: reopen-task`) { #task-reopening-commit_policy-reopen-task }

A hook bound to `on-task-end` with `commit_policy: reopen-task` can veto a task's outcome
instead of just observing it. When it returns `reopen-task(reason)`, the runtime does not
finalize the task: it re-runs the task's agent loop with `reason` injected into the task
content as feedback, then fires `on-task-end` again so the hook can re-inspect the new result.

This repeats up to the reopen limit set by `lifecycle.max_task_reopens` (default **1**; `0`
disables reopening entirely — unlike `inference.max_turns`, an explicit `0` is accepted).
Reopening never grants extra turns: every attempt of a task shares one cumulative turn count
against the capsule's `inference.max_turns` limit, so a task cannot out-run its turn budget just
because a hook keeps asking for another try.

If the reopen limit or the turn limit is used up while a hook still wants to reopen, the task
ends with its own exit status — `exit_status: "reopen_budget_exhausted"` rather than an
ordinary `"ok"`/`"failed"` — and the task registry / A2A task state records it like any other
failed task.

Every reopen is written to `trace.jsonl` as a `task_reopened` event (the hook's name, its
feedback text, and a 1-based ordinal), and the terminal `task_end` record carries a
`reopen_count` field — `0` for a task that ran once. See [Session trace
(`trace.jsonl`) schema](../reference/observability-schemas.md#session-trace-tracejsonl) for the exact shapes.

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

## Context seeding (`commit_policy: seed-context`) { #context-seeding-commit_policy-seed-context }

A hook bound to `on-task-start` with `commit_policy: seed-context` gives a capsule memory. It
returns a list of messages, oldest first, and the runtime places them at the head of the task's
message list — ahead of any conversation history the task loads and ahead of the task itself — so
they are in the very first request the driver sees. The first bound hook to return a seed wins.

Two manifest keys govern how much a seed may occupy:

| Key | Controls |
|---|---|
| [`context.seed_budget`](../reference/manifest.md#field-context) | Fraction of `context.max_tokens` the seed may occupy. The product, rounded down, is the seed's ceiling and is sent to the hook as `task-start-event.budget-tokens` |
| [`context.seed_overflow_margin`](../reference/manifest.md#field-context) | How far over that ceiling a seed may go before the runtime spends an inference call summarizing it instead of simply dropping its oldest messages |

What the runtime does with a proposal, checked in this order:

| Condition | Result |
|---|---|
| The session runs `inference.transport: process` | Nothing is seeded — that transport's CLI owns its own conversation |
| The capsule declares no `context.max_tokens` | Nothing is seeded — there is no ceiling to enforce |
| One message alone is wider than the whole ceiling | Nothing is seeded — no trim can fit it |
| The proposal is more than three times the ceiling over it | Nothing is seeded — a seed that far over is a broken hook, not a full memory |
| The proposal fits the ceiling | All of it is seeded |
| It is over by no more than `context.seed_overflow_margin` of the ceiling | The oldest messages are dropped from the front until the rest fits |
| It is over by more than that, and a `on-compaction` hook is bound | The overflowing front is summarized by that hook, and the summary becomes the seed's first message |
| It is over by more than that, and no `on-compaction` hook answers | The oldest messages are dropped from the front until the rest fits |

A seed that cannot be committed never fails the task: the task runs without it. Every outcome is
written to `trace.jsonl` as a
[`context_seed` event](../reference/observability-schemas.md#context-seed) and to
`workdir/logs/bootstrap.log`.

```yaml
name: murmur-hook-memory
version: 1.0.0
runtime: hook
binding: on-task-start
execution_mode: blocking
commit_policy: seed-context
description: "Reloads what earlier sessions recorded."
```

## Capsule lifecycle

The `lifecycle:` manifest block controls how long a capsule stays running and how many tasks
it accepts over its lifetime:

| `task_acceptance` | `after_task` | Behaviour |
|---|---|---|
| `none` | `exit` | Runs from `task.md` if present, then exits. All incoming messages are rejected. |
| `single` (default) | `exit` (default) | Accepts one [A2A task](formations.md), runs it, exits. Classic ephemeral capsule. |
| `queue` | `sleep` | Accepts a queue of tasks, processes them serially, sleeps between tasks; the host decides when to shut it down on idle. |

`queue+sleep` is the canonical mode for persistent capsules — it parks between tasks rather
than holding an OS thread. `session-start`/`session-end` still fire once per capsule launch,
as shown in the session loop diagram above; it's `task-start`/`task-end` that fire once per
task iteration, and a capsule's hook components are loaded once at startup and reused across
all task iterations.

### Queue lanes

A queued task waits in one of three lanes, read off its
[origin](access-control.md#task-origin-and-trust-class):

| Lane | Origins | What is waiting on the task |
|---|---|---|
| `user` | `user` | A person |
| `peer` | `peer` | Another capsule, with a task of its own blocked on the answer |
| `bg` | `schedule`, `event`, `completion`, `system` | Nothing |

The runtime takes the front of the highest non-empty lane: every waiting `user` task runs before
any `peer` task, and every `peer` task before any `bg` task. Within one lane the order is arrival.

A running task is never interrupted. A task that outranks it waits until it finishes, so lanes
decide which task starts next and never which task stops.

A capsule with one source of tasks puts every task in the same lane and runs them in arrival
order; lanes matter in proportion to how many sources a capsule takes work from at once.

Only `peer` and `completion` are accepted from an inbound request's `x-murmur-task-origin` header,
so an HTTP caller cannot put itself in the `user` lane — a request claiming `user` is read as
`event` and waits in `bg`. The lane each task ran in is on its `task_start` record and on the task
row of `mur trace steps`.

### Detached shell

A shell command that takes longer than
[`lifecycle.shell_grace_secs`](../reference/manifest.md#lifecycle-shell-grace-secs) — 10 seconds
by default — is demoted to the background and the turn carries on without it. There is no flag
and the model predicts no duration: every command starts in the foreground, and one that finishes
inside the grace period behaves exactly as it always has.

A demoted command hands the turn a handle of the form `wrk_<id>` and the fact that it is still
running. Its output is not in the turn and never will be: the full stdout and stderr go to
`logs/<work_id>.log` under the [capsule workdir](../reference/workdir.md).

The handle cannot be polled. No tool, host import or CLI subcommand takes a work id. When the
command finishes, the runtime enqueues a task on the capsule itself with origin `completion`,
carrying the exit code and the output path, under the same `contextId` the command was started
from. It waits in the `bg` lane like any other completion, so a person's request or a peer's
never queues behind it.

A failure arrives the same way a success does. A non-zero exit, a signal kill and an attributed
resource limit all produce a completion, told apart by the `status` field on the
[`shell_completed`](../reference/observability-schemas.md#session-trace-tracejsonl) record.

Only a capsule that outlives the task which started the command receives its completion. Under
the default `after_task: exit` the session ends with the task, and a command still running is
recorded as `shell_abandoned` — once in `trace.jsonl` and once on stderr — and its result is
lost. Nothing survives a restart yet: `queue` + `sleep` is what keeps a capsule around long
enough to hear back.
