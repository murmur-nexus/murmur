# How to give a capsule memory

By default every task a capsule runs starts from nothing: the model sees only the current task and whatever tools it calls during that task. This guide covers two independent ways to change that — seeding a task with the relevant part of an earlier conversation, and giving a capsule a durable place to write its own notes.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [context.record](../reference/manifest.md#field-context) | Whether the runtime keeps a durable conversation record for this capsule |
| [context.record_store](../reference/manifest.md#field-context) | Directory under `~/.murmur/conversations/` the record lives in |
| [context.seed_budget](../reference/manifest.md#field-context) | Fraction of `context.max_tokens` an `on-task-start` hook's seed may occupy |
| [context.seed_overflow_margin](../reference/manifest.md#field-context) | Slack above the seed budget before the runtime trims or summarizes instead of seeding it whole |
| [capabilities.conversation.read](../reference/manifest.md#hook-capabilities) | Grants a hook read access to the conversation record |
| [capabilities.state](../reference/manifest.md#field-capabilities) | Durable store an artifact's own file tools cannot reach |

---

## Step 1 — create murmur.yaml with the memory hook

Every task on `inference.transport: http` already appends the messages it sends to a durable conversation record — this is on by default and needs no configuration. What is not automatic is putting any of that record back in front of the model. `murmur-hook-memory` is the artifact that does it: bound to `on-task-start`, it reads the record, selects a budget-bounded chronological slice, and seeds it at the head of the new task's context.

Create a `murmur.yaml` file:

=== "Anthropic"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_anthropic }}

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
      - name: murmur-hook-memory
        version: "{{ v.murmur_hook_memory }}"
        runtime: hook
        capabilities:
          conversation:
            read: true

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
    ```

=== "OpenAI"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_openai }}

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
      - name: murmur-hook-memory
        version: "{{ v.murmur_hook_memory }}"
        runtime: hook
        capabilities:
          conversation:
            read: true

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
    ```

=== "DeepSeek"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_deepseek }}

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
      - name: murmur-hook-memory
        version: "{{ v.murmur_hook_memory }}"
        runtime: hook
        capabilities:
          conversation:
            read: true

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
    ```

`capabilities.conversation.read: true` on the hook's own entry is required, not inferred — a `murmur-hook-memory` declared without it fails every read and seeds nothing. `context.max_tokens` is required too: it is what `context.seed_budget` is a fraction of, and without it a seed the hook returns is refused.

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — bound what the hook may seed

`context.seed_budget` and `context.seed_overflow_margin` both default to `0.10` — a seed may use up to 10% of `context.max_tokens`, with another 10% of *that* as slack before the runtime intervenes. Raise the budget for a capsule that leans on its history more than on the current task:

```yaml
context:
  max_tokens: {{ v.max_tokens_anthropic }}
  seed_budget: 0.15
  seed_overflow_margin: 0.10
```

A proposed seed that fits the budget is seeded whole. One that overflows by no more than the margin has its oldest messages dropped until it fits. One that overflows by more is handed to a bound `on-compaction` hook to summarize, if the capsule declares one — otherwise its oldest messages are dropped the same way. None of this can fail the task: a seed the runtime cannot commit is skipped, and the task runs without it. See [Context seeding](../concepts/session-loop.md#context-seeding-commit_policy-seed-context) for the full decision order.

---

## Step 4 — continue a conversation across separate runs

Two `mur run` launches with no session directory in common still continue one conversation if you give them the same context id. Create a `task.md` file:

```markdown
Our nightly build started failing on ARM runners after upgrading the Docker base image to 3.20. Note that down before you look at anything else.
```

Run it under a named context:

```bash
mur run --task task.md --context nightly-build-investigation
```

Later, replace `task.md` with a follow-up:

```markdown
What did we learn last time about the nightly build failures?
```

Run it under the same context id:

```bash
mur run --task task.md --context nightly-build-investigation
```

The second run's `murmur-hook-memory` reads the record the first run wrote under that context id and seeds the relevant slice — the model answers from what the first run actually recorded, not from a fresh start. Omit `--context` and each run gets its own fresh id, so nothing carries over.

---

## Step 5 — verify the hook seeded something

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

A run whose memory hook committed a seed shows a **Context** section:

```text
── Context ──────────────────────────────────────
memory-hook  committed  842 tokens  (budget 1,600)
  seeded from: msg_01a04900754b7183b66c11e744612e2d, msg_01a04900754b7183b66c11e744612e3e
```

No **Context** section at all means no `on-task-start` hook returned a seed — check that `capabilities.conversation.read` is on the hook's own entry, not the capsule-wide `capabilities` block, where it is silently inert.

---

## Step 6 — turn the record off, or keep it separate per capsule

`context.record: off` stops the runtime from writing anything under `~/.murmur/conversations/` at all — useful for a capsule that should never retain what it saw, agent-to-agent handoffs included. `murmur-hook-memory` gracefully reads an empty page in that case rather than failing.

```yaml
context:
  max_tokens: {{ v.max_tokens_anthropic }}
  record: off
```

Two capsules that share a directory get separate records by default, keyed by capsule name. Give `context.record_store` an explicit name to point two different capsules at the same record, or to keep multiple variants of the same capsule (a staging build and a production build, say) from reading each other's history:

```yaml
context:
  max_tokens: {{ v.max_tokens_anthropic }}
  record_store: my-agent-staging
```

---

## Step 7 — give the capsule a place to write its own notes

The conversation record and the memory hook cover what the model *said*. For structured notes the agent writes on purpose — findings, decisions, anything worth keeping past the task that produced it — declare a durable state store instead. `murmur-tool-corpus` is the shipped implementation: an append-only, schema-validated record store the capsule's own `murmur-tool-editor` or shell grant cannot reach, because it lives outside the workdir entirely.

```yaml
artifacts:
  - name: murmur-tool-corpus
    version: "{{ v.murmur_tool_corpus }}"
    runtime: tool
    capabilities:
      state: {}
    config:
      config_version: 1
      types:
        finding:
          schema_version: 1
          schema:
            type: object
            required: [text]
            properties:
              text: { type: string }
            additionalProperties: false
```

See [`murmur-tool-corpus`](../reference/default-artifacts.md#murmur-tool-corpus) for its five operations, and [Durable state store](../reference/workdir.md#state-store) for where the grant actually points and how it differs from the conversation record. Confirm the grant resolved before relying on it:

```bash
mur run --explain-scope
```

```text
  state stores:
    - murmur-tool-corpus: my-agent -> /home/dev/.murmur/state/my-agent
```

---

## Summary

| Manifest setting | Effect |
|---|---|
| `context.record: on` (default) | Every task appends its messages to a durable conversation record |
| `context.record: off` | Nothing is written; a hook granted `capabilities.conversation.read` reads an empty page |
| `context.record_store: <name>` | Names the record's directory explicitly; default is the capsule name |
| `murmur-hook-memory` with `capabilities.conversation.read: true` on its own entry | Seeds each task with the relevant slice of prior conversation; the capsule-wide `capabilities` block cannot grant this |
| `context.seed_budget` (default `0.10`) | Fraction of `context.max_tokens` a seed may occupy |
| `context.seed_overflow_margin` (default `0.10`) | Slack before an over-budget seed is trimmed or summarized instead of seeded whole |
| `mur run --context <id>` | Two runs with the same id share one conversation record |
| `capabilities.state: {}` on an artifact entry | Grants that artifact a durable directory outside the workdir, keyed by capsule |
| `murmur-tool-corpus` | Ships as the append-only, schema-validated store for an agent's own notes |
