# How to pause your agent for human input

Some tasks reach a decision point the agent cannot resolve on its own — a target environment, an approval threshold, a destructive step that needs sign-off. `murmur-tool-request-input` gives the model a ready-made tool to pause at that point, surface a question to the operator, and resume exactly where it left off once the operator replies.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest.md#field-artifacts) | Whether a WASM artifact is a model-visible tool, inference driver, or hook |
| [lifecycle.input_timeout_secs](../reference/manifest.md#lifecycle-input-timeout-secs) | Maximum seconds to wait before failing the task if no reply arrives |

---

## Step 1 — write the manifest

Create a `murmur.yaml` file. Add `murmur-tool-request-input` to `artifacts` with `runtime: tool`, and write a `system_prompt` that tells the model when to pause:

=== "Anthropic"

    ```yaml
    name: my-agent
    version: "0.1.0"

    network:
      internal_port: 52222

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}
      - name: murmur-tool-request-input
        version: "{{ v.murmur_tool_request_input }}"
        runtime: tool

    inference:
      transport: http
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur-driver-anthropic
      system_prompt: |
        You are a deployment agent. When you reach a decision you cannot make on your own —
        such as which environment to target or whether to proceed with a destructive step —
        call murmur-tool-request-input with a clear, specific question for the operator.
    ```

=== "OpenAI"

    ```yaml
    name: my-agent
    version: "0.1.0"

    network:
      internal_port: 52222

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}
      - name: murmur-tool-request-input
        version: "{{ v.murmur_tool_request_input }}"
        runtime: tool

    inference:
      transport: http
      model: {{ v.model_openai }}
      driver:
        artifact: murmur-driver-openai
      system_prompt: |
        You are a deployment agent. When you reach a decision you cannot make on your own —
        such as which environment to target or whether to proceed with a destructive step —
        call murmur-tool-request-input with a clear, specific question for the operator.
    ```

=== "DeepSeek"

    ```yaml
    name: my-agent
    version: "0.1.0"

    network:
      internal_port: 52222

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}
      - name: murmur-tool-request-input
        version: "{{ v.murmur_tool_request_input }}"
        runtime: tool

    inference:
      transport: http
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur-driver-deepseek
      system_prompt: |
        You are a deployment agent. When you reach a decision you cannot make on your own —
        such as which environment to target or whether to proceed with a destructive step —
        call murmur-tool-request-input with a clear, specific question for the operator.
    ```

`network.internal_port` pins the worker capsule to a fixed port on every run. Without it the runtime picks an OS-assigned port at startup, which changes between runs and would invalidate the orchestrator capsule's allow list entry.

`runtime: tool` registers the WASM component with the capsule runtime. The model sees it as a callable tool named `murmur-tool-request-input` with one parameter:

| Parameter | Type | Required | Description |
|---|---|---|---|
| `prompt` | string | yes | The question to present to the operator |

The system prompt controls when the model pauses. A vague instruction ("ask when unsure") produces over-cautious behavior; a concrete rule ("call the tool before any destructive step") produces predictable escalation.

---

## Step 2 — install dependencies

With `murmur.yaml` in place, fetch all declared artifacts:

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

`murmur-tool-request-input` is a WASM artifact — no platform tag is required. `mur install` resolves the correct variant for every artifact in the manifest automatically.

---

## Step 3 — run the capsule and send a task

Start the capsule:

```
mur run
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
```

Note the URL. Set a shell variable for convenience, replacing `52222` with the port you see:

```bash
PORT=52222
```

Send a task that will require a human decision:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "msg-001",
        "role": "ROLE_USER",
        "parts": [{"text": "Deploy the latest build. Confirm the target environment before proceeding."}]
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
    "task": {
      "id": "tsk_01jw...",
      "contextId": "ctx_01jw...",
      "status": { "state": "TASK_STATE_SUBMITTED" }
    }
  }
}
```

Save the task `id` at `result.task.id`: you need it to detect the pause and to answer.

---

## Step 4 — detect when the agent is waiting

Poll `GetTask` with the task ID until the state changes to `TASK_STATE_INPUT_REQUIRED`:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{"jsonrpc":"2.0","id":2,"method":"GetTask","params":{"id":"<your_task_id>"}}'
```

While the agent is waiting, the response includes the question it formed:

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "id": "tsk_01jw...",
    "contextId": "ctx_01jw...",
    "status": { "state": "TASK_STATE_INPUT_REQUIRED" },
    "artifacts": [
      {
        "artifactId": "prompt",
        "name": "prompt",
        "parts": [
          {
            "text": "Which environment should I deploy to? (staging, production)",
            "mediaType": "text/plain"
          }
        ]
      }
    ]
  }
}
```

The agent's question is at `result.artifacts[0].parts[0].text`. Read it and decide your answer.

---

## Step 5 — send your answer

Send a message to the same capsule URL, naming the waiting task in `taskId`:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -H "A2A-Version: 1.0" \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "SendMessage",
    "params": {
      "message": {
        "messageId": "reply-001",
        "taskId": "<your_task_id>",
        "role": "ROLE_USER",
        "parts": [{"text": "staging"}]
      }
    }
  }'
```

The answer is delivered directly to the suspended tool call. The agent receives `"staging"` as the tool result and the loop resumes immediately:

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "task": {
      "id": "tsk_01jw...",
      "contextId": "ctx_01jw...",
      "status": { "state": "TASK_STATE_WORKING" }
    }
  }
}
```

A message without `taskId` starts a new task instead, and never reaches the waiting one. A reply
naming a task that is not waiting is refused: see
[Continuing a task](../reference/agent-card.md#task-id).

Poll `GetTask` again until `state` reaches `TASK_STATE_COMPLETED` or `TASK_STATE_FAILED`.

---

## Optional — set an input timeout

To fail the task automatically if no reply arrives within a deadline, add `lifecycle.input_timeout_secs` to the manifest:

```yaml
lifecycle:
  task_acceptance: single
  input_timeout_secs: 300
```

When the deadline passes, the tool call fails and the attempt ends. The task then ends `failed` with message `"input-timeout"`, unless an `on-task-end` hook [reopens it](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task). Omit the field to wait indefinitely.

---

## Summary

| Feature / setting | How it works |
|---|---|
| `murmur-tool-request-input` | WASM tool artifact; `runtime: tool`; platform-independent |
| `prompt` parameter | The question the model asks the operator; string, required |
| Task state while waiting | `"TASK_STATE_INPUT_REQUIRED"` |
| Where to read the question | `result.artifacts[0].parts[0].text` from `GetTask` |
| How to resume the agent | Send a message to the same capsule URL whose `taskId` names the waiting task |
| State after reply | `"TASK_STATE_WORKING"` immediately; poll until `"TASK_STATE_COMPLETED"` |
| `lifecycle.input_timeout_secs` | Integer seconds to wait for a reply; absent = wait indefinitely |
