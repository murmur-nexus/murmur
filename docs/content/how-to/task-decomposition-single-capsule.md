# How to decompose tasks recursively within a single capsule

When a task contains multiple sequential steps that depend on each other's output, a single capsule can decompose it on the fly: the first turn acts as a router that identifies the steps and dispatches each as a sub-task back to its own queue; every subsequent turn executes one sub-task independently. All sub-tasks run in the same capsule session and share the same workdir filesystem, so output from one step is immediately available as input to the next. This eliminates the need for a separate orchestrator capsule when the work is sequential and state flows through files.

??? definition "What is A2A?"
    **A2A (Agent-to-Agent)** is a JSON-RPC 2.0 protocol for structured communication between autonomous agents. One agent sends a message to another over HTTP and receives a task ID in return. In this guide, the capsule sends messages to itself — its own A2A endpoint — to queue sub-tasks without spawning a second capsule.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest-schema.md#lifecycle-task-acceptance) | Whether and how many A2A tasks the capsule accepts |
| [lifecycle.after_task](../reference/manifest-schema.md#lifecycle-after-task) | What the capsule does after a task completes |
| [lifecycle.queue_depth](../reference/manifest-schema.md#field-lifecycle) | How many tasks can be buffered while one is running |
| [network.internal_port](../reference/manifest-schema.md#field-capabilities) | Pins the capsule to a fixed port so self-dispatched sub-tasks always find it |
| [capabilities.network.allow](../reference/manifest-schema.md#field-capabilities) | Host/URL patterns the capsule may connect to — must include `localhost:52222` for self-dispatch |
| [capabilities.shell.allow](../reference/manifest-schema.md#field-capabilities) | Shell binaries the capsule may call — must include `curl` for self-dispatch |

---

## Step 1 — Create the manifest

Six manifest fields work together to make the recursive pattern possible. Create `murmur.yaml`:

=== "Anthropic"

    ```yaml
    name: poem-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_anthropic }}

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.anthropic.com
          - localhost:52222
      filesystem:
        allow:
          - .
      shell:
        allow:
          - curl

    network:
      internal_port: 52222

    inference:
      max_turns: 100
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
      system_prompt_file: instructions.md

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 20
    ```

=== "OpenAI"

    ```yaml
    name: poem-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_openai }}

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.openai.com
          - localhost:52222
      filesystem:
        allow:
          - .
      shell:
        allow:
          - curl

    network:
      internal_port: 52222

    inference:
      max_turns: 100
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
      system_prompt_file: instructions.md

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 20
    ```

=== "DeepSeek"

    ```yaml
    name: poem-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_deepseek }}

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.deepseek.com
          - localhost:52222
      filesystem:
        allow:
          - .
      shell:
        allow:
          - curl

    network:
      internal_port: 52222

    inference:
      max_turns: 100
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
      system_prompt_file: instructions.md

    lifecycle:
      task_acceptance: queue
      after_task: sleep
      queue_depth: 20
    ```

**Self-targeting.** `network.internal_port: 52222` locks the capsule to a predictable port. Capsules normally let the OS assign a free port at startup — but the router must know its own URL in advance to embed it in curl dispatches. Fixing the port solves this. `capabilities.network.allow` must include that same address: the runtime blocks any outbound connection not on the allow list, including connections back to the capsule itself.

**Task creation.** `capabilities.shell.allow: curl` exposes `curl` as a named tool so the router can POST new tasks to the A2A endpoint. Those tasks land in the capsule's own queue, which is enabled by `lifecycle.task_acceptance: queue`. `lifecycle.after_task: sleep` keeps the capsule alive to drain it rather than exiting after the first task. `lifecycle.queue_depth: 20` gives the router room to dispatch all sub-tasks in parallel without any being rejected.

**Iteration headroom.** `inference.max_turns: 100` raises the per-task turn limit from the default of 10. The iteration sub-task may rewrite the poem and both reviews several times before both scores reach 10/10; without extra headroom it would be cut off mid-iteration.

**The key ingredient.** `inference.system_prompt_file: instructions.md` is what makes the capsule recursive. It points to the file that tells the model to decompose incoming tasks into `[EXECUTE]:`-prefixed sub-tasks rather than executing everything in one context. Without it the capsule is just a standard single-task agent. As a best practice, the prompt also instructs the model to issue all curl dispatches as parallel tool calls in a single turn, avoiding unnecessary round trips.

---

## Step 2 — Install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — Write the system prompt

The system prompt makes a single capsule serve as both router and executor. It uses an `[EXECUTE]:` prefix to distinguish the two roles: any incoming message without that prefix triggers routing; any message with it triggers direct execution.

Create `instructions.md`:

```
If this task starts with [EXECUTE]: strip the prefix, execute it directly,
save all output using operation write_file to the named file, exit.

Otherwise you are a router. The task is already in this message — do not read any files.
Identify the ordered sequential steps in the goal, including any iteration or
quality-gating as an explicit final step. For each step, craft a self-contained sub-task
message that names every file to read and every file to write explicitly.
Every sub-task must save its output to a named file.
Dispatch all sub-tasks as parallel tool calls in turn 0 — call curl once per sub-task,
all in a single response. Prefix each sub-task text with [EXECUTE].

Router rules:
- Call only curl. Do not call any other tool under any circumstances.
- The output files do not exist during dispatch — sub-tasks run after your session ends.
  Attempting to read them will produce errors. This is expected; do not react to it.
- After all dispatches return {"state":"submitted"}, write "Done." and stop immediately.
- If a dispatch returns an error, report it in text and stop. Do not retry.

Dispatch shape:
-s -X POST http://localhost:52222 -H "Content-Type: application/json" -d '{"jsonrpc":"2.0","method":"message/send","params":{"message":{"messageId":"task-N","role":"user","parts":[{"text":"[EXECUTE] ..."}]}},"id":N}'
```

The router completes in two turns: one to fire all dispatches in parallel, one to confirm submission and stop. It never calls any tool other than curl and never reads files — the task text arrives in the incoming message itself.

Four rules make this pattern reliable:

1. **Every sub-task is self-contained.** The router names every input and output file explicitly in the sub-task message. An executor never has to infer what to read.
2. **Sequential ordering is implicit.** The queue processes sub-tasks one at a time. A later sub-task can safely read files written by an earlier one.
3. **Iteration is itself a sub-task.** Quality gating (e.g. "redo if not 10/10") is dispatched as a dedicated sub-task at the end of the queue. It reads the review files and rewrites the main output until the threshold is met — the router does not need to know how many iterations are required.
4. **Parallel dispatch is faster and safer.** Firing all curl calls in a single turn eliminates the intermediate turns where the model could drift off-script between dispatches.

---

## Step 4 — Write the task

Create `task.md` with the goal the router will decompose:

```markdown
Write a short poem about Colombia (8–12 lines). Save it to poem.md. Then write a
review of the artistic nature, and separately an assessment of the accuracy of the poem's
content. Iterate all three if either the review or assessment is not 10/10.
Final version remains in poem.md.
```

---

## Step 5 — Run the capsule

```bash
mur run
```

```
mur run
murmur: url localhost:52222
session: ses_019f01a940ce7761854e768ecbe3d399
```

The capsule reads `task.md` as its first task. Because the message does not start with `[EXECUTE]:`, the model enters router mode. In turn 0 it fires all four curl dispatches in parallel — write, review, assess, iterate — each prefixed with `[EXECUTE]:`. In turn 1 it confirms all submissions and stops. The router uses two turns total and calls no tool other than curl.

The capsule then processes each queued sub-task in order:

1. Writes `poem.md`
2. Reads `poem.md`, writes `review_artistic.md`
3. Reads `poem.md`, writes `assessment_accuracy.md`
4. Reads all three files, iterates revisions until both scores reach 10/10, then saves the final poem to `poem.md`

When the queue drains and the last sub-task completes, the capsule exits.

!!! note "The capsule is its own queue"
    Because `lifecycle.task_acceptance: queue` and `after_task: sleep` are set, the capsule's A2A endpoint at `localhost:52222` acts as the queue. The router dispatches to the same process, the same session, the same workdir. There is no second capsule.

---

## Step 6 — Inspect the trace

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

The Tool calls section shows the parallel dispatch in turn 0, and the Tasks section shows each sub-task's turn count, token usage, and duration:

```
── Session ──────────────────────────────────────
session:    ses_019f01a940ce7761854e768ecbe3d399
capsule:    poem-agent v0.1.0
model:      deepseek-v4-flash
status:     ok
duration:   94.6s

── Turns ────────────────────────────────────────
count:      15  (max: 100)

── Tokens ───────────────────────────────────────
input:      32,116  (avg 2141/turn)
output:     7,128  (avg 475/turn)
total:      39,244

── Tool calls ───────────────────────────────────
count:      16  (16 ok, 0 error)  success 100.0%
latency:    avg 24ms
  turn 0  curl 16ms ✓  curl 6ms ✓  curl 6ms ✓  curl 5ms ✓  ...
  ...
  turn 4  end_turn

── Tasks ───────────────────────────────────────
task 1  tsk_019f01a9  turns: 2  in:  5,156  out: 2,250  ok  25.0s
task 2  tsk_019f01a9  turns: 2  in:  2,114  out:   512  ok   8.2s
task 3  tsk_019f01a9  turns: 3  in:  4,025  out:   818  ok  13.3s
task 4  tsk_019f01a9  turns: 3  in:  4,555  out: 1,267  ok  16.5s
task 5  tsk_019f01a9  turns: 5  in: 16,266  out: 2,281  ok  31.4s
```

Turn 0 opens with four curl calls in parallel — the router dispatching all sub-tasks at once. Task 1 (the router) completes in 2 turns: one to dispatch, one to confirm and stop. Tasks 2–5 are the executor sub-tasks, with task 5 spending the most turns iterating the poem and both review files until both scores reached 10/10.

---

## Summary

| Feature / setting | How it works |
|---|---|
| Single capsule as router and executor | The system prompt routes untagged tasks by dispatching `[EXECUTE]:`-prefixed sub-tasks to its own A2A queue |
| Sub-tasks share the workdir | All sub-tasks run in the same session with the same working directory — files written by one step are readable by the next |
| Capsule stays alive between sub-tasks | `lifecycle.after_task: sleep` keeps the capsule running until the queue is fully drained |
| Queue capacity | `lifecycle.queue_depth: 20` buffers all router-dispatched sub-tasks without rejection |
| Self-dispatch | `capabilities.shell.allow: [curl]` and `capabilities.network.allow: [localhost:52222]` let the router POST to its own A2A endpoint |
| Quality iteration | The router dispatches a dedicated iteration sub-task at the end of the queue that loops on the review files until the threshold is met — no external orchestration required |
