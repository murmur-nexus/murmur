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
