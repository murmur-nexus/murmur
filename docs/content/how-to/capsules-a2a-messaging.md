# How to connect two capsules with A2A messaging

Murmur capsules can send tasks to each other directly — an orchestrator capsule can delegate work to a worker capsule by sending it a message. This guide walks through launching a worker capsule, reading its URL, and calling it from an orchestrator capsule. It also covers the network policy that governs what each capsule is allowed to contact.

??? definition "What is A2A?"
    **A2A (Agent-to-Agent)** is a JSON-RPC 2.0 protocol for structured communication between autonomous agents. One capsule sends a message to another over HTTP and receives a task ID in return. It can then poll for results or stream live events while the receiving capsule processes the work — without either side knowing about the other's internal implementation.

    Key concepts:

    - **Task** — a unit of work submitted via an incoming message, identified by a task ID
    - **Context** — a conversation thread shared across multiple tasks (`contextId`)
    - **Orchestrator capsule** — the capsule that sends tasks to other capsules
    - **Worker capsule** — the capsule that accepts and processes incoming tasks

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest.md#lifecycle-task-acceptance) | Whether and how many A2A tasks the capsule accepts |
| [lifecycle.after_task](../reference/manifest.md#lifecycle-after-task) | What the capsule does after a task completes |
| [network.internal_port](../reference/manifest.md#field-capabilities) | Pins the worker capsule to a fixed port so the orchestrator capsule allow list stays stable |
| [capabilities.network.allow](../reference/manifest.md#field-capabilities) | Host/URL patterns the capsule may connect to — must include peer capsule URLs |

---

## Step 1 — export PORT and write the worker capsule manifest

The worker capsule is a long-running capsule that accepts messages and processes them one at a time.

Export a fixed port so every subsequent command can reference it by name:

```bash
export PORT=52222
```

Create `worker/murmur.yaml`:

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

    network:
      internal_port: 52222

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

    network:
      internal_port: 52222

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

    network:
      internal_port: 52222

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
    ```

`network.internal_port` pins the worker capsule to a fixed port on every run. Without it the runtime picks an OS-assigned port at startup, which changes between runs and would invalidate the orchestrator capsule's allow list entry. `task_acceptance: queue` and `after_task: sleep` keep the capsule alive between tasks so the orchestrator capsule can send multiple messages to the same session.

---

## Step 2 — install dependencies

From the `worker/` directory, fetch all declared artifacts:

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — start the worker capsule

```bash
mur run --manifest worker/murmur.yaml
```

The worker capsule starts and prints its URL to stderr:

```
mur run --manifest worker/murmur.yaml
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
```

Because `network.internal_port: 52222` is set, the URL is stable across restarts and matches `$PORT`.

!!! warning "Locking a port is for development convenience"
    Specifying `network.internal_port` makes the worker capsule's URL predictable, which simplifies the orchestrator capsule's allow list during development. In production, omit it and let the OS assign a port — fixed ports can cause bind failures when multiple capsule sessions run on the same host or when the port is already in use by another process.

---

## Step 4 — write the orchestrator capsule manifest

The orchestrator capsule must declare the worker capsule's URL in `capabilities.network.allow`. Without this the runtime refuses the outgoing connection before any TCP packet is sent.

Create `orchestrator/murmur.yaml`:

=== "Anthropic"

    ```yaml
    name: my-orchestrator
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.anthropic.com
          - localhost:52222

    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: my-orchestrator
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.openai.com
          - localhost:52222

    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: my-orchestrator
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.deepseek.com
          - localhost:52222

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

---

## Step 5 — send a message to the worker capsule

With the worker capsule running, the orchestrator capsule (or any HTTP client) can send it a message. The worker capsule's endpoint is `http://localhost:<port>`:

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
        "parts": [{"text": "Acknowledge message from orchestrator capsule."}]
      }
    }
  }'
```

Response:

```json
{"jsonrpc":"2.0","id":1,"result":{"contextId":"ctx-001","id":"tsk_019ed33d1f6873b09d25f0dc5ce387c4","status":{"state":"submitted"}}}
```

Save the returned `id` — that is the **task ID** you will use to poll status.

When the orchestrator capsule is itself a WASM capsule component, it sends messages via the host's message interface rather than raw HTTP. The host handles the JSON-RPC wire format transparently and enforces the network policy check before making any connection.

---

## Alternative: stream events instead of polling

Use `mur watch localhost:$PORT` from a second terminal to observe live progress — inference heartbeats, tool results, and completion state — as the agent loop runs.

```bash
mur watch localhost:$PORT
```

---

## Step 6 — poll the task status

Use the task ID returned in step 5 with `tasks/get`:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tasks/get","params":{"id":"<your-task-id>"}}'
```

`state` progresses through: `submitted` → `working` → `completed` | `failed`.

```json
{
    "jsonrpc": "2.0",
    "id": 2,
    "result":
    {
        "contextId": "ctx-001",
        "id": "tsk_019ed5211c827f63a8fe4be623277c55",
        "status":
        {
            "state": "completed"
        }
    }
}
```

Poll until `state` is `completed` or `failed`. The task registry on the worker capsule remembers completed tasks, so you can query a task ID after the task has already finished.

!!! note "The HTTP server shuts down with the session"
    Once the worker capsule exits (idle timeout, or after the last queued task), its HTTP server is released. Final status is always available in `trace.jsonl` in the worker capsule's workdir.

---

## Step 7 — inspect both sides in the trace

After the session, both the orchestrator capsule and worker capsule produce `trace.jsonl` files. Each captures its own side of the exchange.

**Worker capsule trace** — shows the incoming task and the agent loop that ran it. Because two capsule sessions are running, pass the last four characters of the worker capsule session ID printed in step 3 to target the correct session:

```bash
mur trace show 34af
```

--8<-- "includes/mur-trace-show-info.md"

The worker capsule trace includes an `a2a_task_received` event for each message it accepted, followed by the normal `session_start` / `inference` / `session_end` block for that task:

```text
── Session ──────────────────────────────────────
session:    ses_019ed2af53da75c2aefee84ee10c34af
capsule:    my-worker v0.1.0
...
── Tasks ───────────────────────────────────────
task 1  tsk_019ed33d  turns: 3  in: 892  out: 241  ok  4.1s
```

**Orchestrator capsule trace** — if the orchestrator capsule is a WASM script capsule, its trace includes an `a2a_send` event for each outgoing message it dispatched:

```json
{"event_type":"a2a_send","peer_url":"localhost:52222","message_id":"msg-001","task_id":"tsk_019ed33d1f6873b09d25f0dc5ce387c4","context_id":"ctx-001"}
```

When OTel tracing is configured, the `traceparent` header links the worker capsule session span as a child of the orchestrator capsule's span, giving you a unified trace across both capsules.

---

## Summary

| Concept | What to configure |
|---|---|
| Worker capsule stays alive | `lifecycle.task_acceptance: queue` + `lifecycle.after_task: sleep` |
| Fixed worker capsule port | `network.internal_port` in the worker capsule manifest; errors if port is already in use |
| Orchestrator capsule can reach worker capsule | `capabilities.network.allow` must include the worker capsule's URL |
| Network policy enforcement | Any peer URL not in `network.allow` is rejected before TCP connection |
| Task ID | Returned by the message call; use it with `tasks/get` to poll status |
| Trace | Both capsules write independent `trace.jsonl` files; `a2a_task_received` appears on the worker capsule side, `a2a_send` on the orchestrator capsule side |
