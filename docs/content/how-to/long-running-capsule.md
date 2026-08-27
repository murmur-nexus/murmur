# How to configure a long-running capsule

By default a Murmur capsule exits as soon as its task finishes. This is default by design. This guide shows how to keep a capsule alive across multiple tasks.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest.md#lifecycle-task-acceptance) | Whether and how many A2A tasks the capsule accepts |
| [lifecycle.after_task](../reference/manifest.md#lifecycle-after-task) | What the capsule does after a task completes |
| [lifecycle.queue_depth](../reference/manifest.md#field-lifecycle) | How many tasks can be buffered while one is running |
| [lifecycle.conversation](../reference/manifest.md#lifecycle-conversation) | Whether tasks with the same contextId accumulate conversation history |

---

## Step 1 — write the manifest

Start from a working agent manifest (inference block, driver artifact, network allow). Add a `lifecycle:` block:

=== "Anthropic"

    ```yaml
    name: my-worker
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.anthropic.com

    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 2
      conversation: threaded
    ```

=== "OpenAI"

    ```yaml
    name: my-worker
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.openai.com

    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 2
      conversation: threaded
    ```

=== "DeepSeek"

    ```yaml
    name: my-worker
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.deepseek.com

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 2
      conversation: threaded
    ```

**What each field does:**

- `task_acceptance: queue` — the capsule accepts incoming messages even while a task is running, up to `queue_depth` pending tasks at a time. Without this, a second call while the capsule is busy gets `state: "rejected"` immediately.
- `after_task: sleep` — instead of exiting when a task finishes, the capsule parks on the queue channel and waits for the next task. This is what keeps it alive between tasks.
- `queue_depth: 2` — sets the buffer to 2 pending tasks; you can use any positive integer. While one task is running, incoming messages queue up to this limit; any message that arrives when the buffer is full is rejected immediately.
- `conversation: threaded` — when two tasks share the same `contextId`, the second task picks up the full conversation history left by the first, giving the model a continuous thread. Omit this field (or set it to `stateless`) to keep every task independent regardless of `contextId`.

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — run

```bash
mur run
```

The capsule starts and waits for the first incoming task. You will see no immediate agent activity — this is expected.

!!! note "No `task.md` needed"
    In queue mode the capsule does not read a `task.md` at startup — all tasks arrive as A2A messages.

---

## Step 4 — send tasks

The capsule URL is printed to stderr at startup. Note the port — you will use it in every request:

```text
MURMUR_CAPSULE_URL: localhost:52322
```

Set a shell variable for convenience, replacing `52322` with the port you see:

```bash
PORT=52322
```

Send the first task:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "message/send",
    "params": {
      "message": {
        "messageId": "msg-001",
        "contextId": "ctx-001",
        "role": "user",
        "parts": [{"text": "Here are some scores: alice=92, bob=85, carol=78, dave=91. How many entries are there?"}]
      }
    }
  }'
```

Response:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "id": "tsk_019e23bd122f77e291a065f9481325cb",
    "contextId": "ctx-001",
    "status": { "state": "submitted" }
  }
}
```

Save the returned `id` — that is the **task ID** you will use to poll status.

Send a second task while the first is still running. Use the same `contextId` to continue the conversation thread:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 2,
    "method": "message/send",
    "params": {
      "message": {
        "messageId": "msg-002",
        "contextId": "ctx-001",
        "role": "user",
        "parts": [{"text": "Who has the highest score?"}]
      }
    }
  }'
```

Because `queue_depth: 2`, this second task is accepted and queued. A third task sent at the same moment would receive `state: "rejected"`.

When task 2 runs, the agent receives the full history from task 1 — the scores list and the answer about the count — so it can answer the follow-up without the data being repeated. Each completed task also writes a per-task result file alongside the shared `out/result.txt`:

```
workdir/out/result.txt                              ← always updated to the latest result
workdir/out/result_tsk_019e23bd122f77e291a065f9481325cb.txt   ← task 1 result
workdir/out/result_tsk_019e24bc7a3f6810b5c2d9e0a71438fd.txt   ← task 2 result
```

To start a separate, independent thread, use a different `contextId`. Each `contextId` maintains its own history; threads do not share state with each other.

---

## Step 5 — poll task status

Use `tasks/get` with the task ID from step 3:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tasks/get","params":{"id":"<your_task_id>"}}'
```

`state` progresses through: `submitted` → `working` → `completed` | `failed`. If you poll after the task has finished you will see `completed` or `failed` directly — `working` is only visible during active processing.

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "id": "tsk_019e23bd122f77e291a065f9481325cb",
    "contextId": "ctx-001",
    "status": { "state": "working" }
  }
}
```

`tasks/get` can look up **any task by ID** from the capsule's `TaskRegistry`, including tasks that have already finished — you are not limited to the currently-running task.

---

## Step 6 — watch the trace

Use `mur trace show` to summarize the session. With no argument it picks the most recent session automatically:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

For persistent capsules the output adds a **Tasks** section with a row per task:

```text
── Session ──────────────────────────────────────
session:    ses_6801f81dd28b4a9daf434e8324c4793e
capsule:    my-worker v0.1.0
model:      claude-sonnet-4-6
status:     ok
duration:   198.5s

── Turns ────────────────────────────────────────
count:      2  (max: 10)

── Tokens ───────────────────────────────────────
input:      403  (avg 202/turn)
output:     476  (avg 238/turn)
total:      879

── Tool calls ───────────────────────────────────
count:      0

── Skill calls ───────────────────────────────────
count:      0

── Shell calls ──────────────────────────────────
count:      0

── Compaction ───────────────────────────────────
fired:      no
── Tasks ───────────────────────────────────────
task 1  tsk_19e23bd  turns: 1  in: 213  out: 68  ok  1.4s
task 2  tsk_019e24bc  turns: 1  in: 190 out: 408  ok  5.6s
```

The task count is the number of rows in the Tasks section. Each row shows the first 12 characters of the task ID, per-task turns and tokens, exit status, and duration.

The underlying `trace.jsonl` event sequence for a persistent session looks like:

```
session_start       ← the capsule launched
a2a_task_received   ← task-1 arrived
task_start          ← task-1 begins
inference           ← model calls for task-1
task_end            ← task-1 metrics written

a2a_task_received   ← task-2 arrived (was queued)
task_start          ← task-2 begins
inference           ← model calls for task-2
task_end            ← task-2 metrics written

session_end         ← the capsule shut down
```

One `session_start` / `session_end` pair frames the whole launch, however many tasks it handles. Per-task token and turn counts are in each `task_end` line; the `session_end` totals are the launch-wide sum.

---

## Step 7 — shutdown

A `queue+sleep` capsule waits indefinitely after the last task completes. It does not self-terminate — shutdown is external, driven by the host (mur-roost closes the task channel) or by a `SIGTERM` when the process is stopped directly.

`MURMUR_A2A_TIMEOUT_SECS` does **not** apply in `queue+sleep` mode. It only takes effect when `after_task: exit` or `task_acceptance: single`, where self-termination after an idle period is the intended behavior.

---

## Step 8 — override lifecycle from the CLI

You can switch a manifest to persistent mode without editing the file:

```bash
mur run --manifest murmur.yaml \
  --lifecycle-task-acceptance queue \
  --lifecycle-after-task sleep
```

This is useful for running the same capsule in both ephemeral mode (CI) and persistent mode (local server) without maintaining two manifests.

---

## Summary

| Manifest setting | Effect |
|---|---|
| `task_acceptance: queue` + `after_task: sleep` | Capsule persists, processes tasks from an mpsc channel one at a time, loops back to wait after each task |
| `queue_depth: N` | Buffers up to N pending tasks; any positive integer — tasks beyond the limit are rejected immediately |
| `conversation: threaded` | Tasks sharing a `contextId` accumulate history — each task sees the full prior exchange for that thread |
| `conversation: stateless` (default) | Every task starts with a blank context regardless of `contextId` |
| Shutdown | External only — host closes the task channel or process receives SIGTERM; no idle self-exit |
| `tasks/get` + task ID | Look up any task — current or historical — from the `TaskRegistry` |
