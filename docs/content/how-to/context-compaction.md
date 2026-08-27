# How to enable context compaction for long-running tasks

An agent session accumulates tokens with every message. For tasks that take many turns — processing large files, iterating on a plan, or running inside a persistent capsule — the conversation history can approach the model's context window. Context compaction automatically condenses the message history so the session can continue without hitting a hard limit.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [context.max_tokens](../reference/manifest.md#field-context) | Token budget for the session; required to enable compaction |
| [inference.compaction.threshold](../reference/manifest.md#field-inference) | Fraction of `context.max_tokens` that triggers compaction |
| [inference.compaction.model](../reference/manifest.md#field-inference) | Model used for the compaction call (optional override) |
| [inference.compaction.system_prompt](../reference/manifest.md#field-inference) | System prompt override for the compaction call (optional; hook picks its own default when unset) |
| [inference.compaction.system_prompt_file](../reference/manifest.md#field-inference) | Same override, loaded from a file next to the manifest (optional; mutually exclusive with `system_prompt`) |
| [inference.compaction.dump_summaries](../reference/manifest.md#field-inference) | When `true`, appends one JSON line per committed compaction to `out/compaction-summaries.jsonl` (optional; default `false`) |

---

## Step 1 — create murmur.yaml with context.max_tokens and the compaction artifact

Compaction requires two things: `context.max_tokens` set to match your model's actual context window, and `murmur-hook-compact` declared as a hook artifact. Create a `murmur.yaml` file:

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
      - name: murmur-hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

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
      - name: murmur-hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

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
      - name: murmur-hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

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

`context.max_tokens` is a manifest value — the runtime does not query the API to discover the context window. Use the number from your model's documentation; most current frontier models support between 400,000 and 1,000,000 tokens.

Using `runtime: hook` ensures the model never sees `murmur-hook-compact` as a callable tool — the runtime invokes it directly at fixed lifecycle points.

---

## Step 2 — install dependencies

Both `context.max_tokens` and the compaction hook must be present for compaction to activate. Either one alone is not enough.

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

To install the compaction artifact directly without going through the manifest:

```bash
mur install murmur-hook-compact@{{ v.murmur_hook_compact }}
```

Compaction is not built into the Murmur runtime — it is externalized as an artifact by design. Everything in Murmur is composable, including something as fundamental as how context is condensed. `murmur-hook-compact` is the default implementation: it summarizes the message history using the configured model and replaces it with a compact representation. But compaction strategy is not one-size-fits-all. A coding agent might benefit from keeping the full tool call history and compacting only prose; a research agent might maintain a structured memory store rather than a rolling summary. You can build any of these as a hook artifact, package it once, and swap it in by changing a single line in the manifest. The runtime does not care which artifact provides compaction — only that one is declared with `runtime: hook` and responds to the compaction lifecycle event.

---

## Step 3 — set the compaction threshold

The threshold controls when compaction fires. It is a fraction of `context.max_tokens`. The default is `0.98` — compaction fires when the session has consumed 98% of the token budget.

For long-running tasks where you want compaction to kick in earlier and leave headroom for recovery, add `compaction.threshold` under `inference`:

=== "Anthropic"

    ```yaml
    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
      compaction:
        threshold: 0.85
    ```

=== "OpenAI"

    ```yaml
    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
      compaction:
        threshold: 0.85
    ```

=== "DeepSeek"

    ```yaml
    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
      compaction:
        threshold: 0.85
    ```

With `threshold: 0.85`, compaction fires when the session has consumed 85% of `context.max_tokens`.

To use a smaller, faster model for compaction calls (saving cost while keeping your primary model for inference):

=== "Anthropic"

    ```yaml
    inference:
      compaction:
        threshold: 0.85
        model: {{ v.model_anthropic_small }}
    ```

=== "OpenAI"

    ```yaml
    inference:
      compaction:
        threshold: 0.85
        model: {{ v.model_openai_small }}
    ```

=== "DeepSeek"

    ```yaml
    inference:
      compaction:
        threshold: 0.85
        model: {{ v.model_deepseek_small }}
    ```

To steer what the compaction call preserves, set `inference.compaction.system_prompt`. The string is passed to the compaction hook verbatim — no trimming, no length limit, no templating:

```yaml
inference:
  compaction:
    threshold: 0.85
    system_prompt: |
      task = X, currently editing Y, already tried Z.
      Preserve this nuance when summarizing.
```

For a longer, versioned instruction set, keep it in its own file and point at it with `system_prompt_file` instead. The path is resolved relative to the manifest directory and read when the session launches, and the file's contents are passed to the hook verbatim:

```yaml
inference:
  compaction:
    threshold: 0.85
    system_prompt_file: compaction-instructions.md
```

Setting both `system_prompt` and `system_prompt_file` on the same `compaction:` block is a manifest error — pick one. `mur deploy` uploads the referenced file alongside the manifest, so a remote run resolves it the same way.

If left unset, `compaction-event.system-prompt` arrives at the hook as `none`, and the hook falls back to its own built-in default prompt. `model` and the two prompt-source fields are independent — setting one does not require or affect the other.

---

## Step 4 — run and confirm the configuration

```bash
mur run
```

At startup the runtime writes a generated `MURMUR.md` to the session workdir. Its **Capsule** section reports compaction status — check it to confirm compaction was configured correctly:

```bash
grep "Context budget" workdir/<session_id>/MURMUR.md
```

When both `context.max_tokens` and a hook bound to `on-compaction` are staged, the status reads `compaction configured`:

```text
- Context budget: 1000000 tokens (compaction configured)
```

If either is missing — `context.max_tokens` is unset, or no `on-compaction` hook is staged — it reads `compaction not configured`:

```text
- Context budget: 1000000 tokens (compaction not configured)
```

The runtime selects the compaction hook by its `on-compaction` binding, not by name, so there is no artifact-name setting to get wrong: any staged hook with that binding satisfies the check.

---

## Step 5 — verify compaction ran using the trace

Run a task that you expect to exceed the threshold, then check the trace:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

If compaction fired:

```text
── Compaction ───────────────────────────────────
fired:      yes  at turn 1  (12,325 → 5,488 tokens)
```

If it did not fire:

```text
── Compaction ───────────────────────────────────
fired:      no
```

Compaction **does not consume a turn slot** — `inference.max_turns` counts inference calls, not compaction events. The model continues from where it left off with the condensed history.

---

## Step 6 — inspect the compaction summaries

`trace.jsonl`'s `compaction` event records only the token counts, not the summary text the model
produced. To persist the summary itself — for understanding what context the agent retained
versus discarded, or debugging cases where the agent appears to "forget" earlier work after
compaction — set `dump_summaries: true`:

```yaml
inference:
  compaction:
    threshold: 0.85
    dump_summaries: true
```

Each committed compaction then appends one line to `out/compaction-summaries.jsonl`:

```json
{"turn":17,"tokens_before":81501,"tokens_after":334,"summary":"1. THE BUG: ..."}
```

See [out/compaction-summaries.jsonl](../reference/workdir.md#compaction-summaries) for the
full field reference. The file is only created once the first compaction actually commits, and
a compaction the tool-call-pairing safety net rejects writes nothing.

---

## Compaction failure modes

Three situations can prevent compaction from replacing the context, and the runtime treats them
differently:

**No hook bound to `on-compaction`.** This is not a failure — it means compaction was never
configured to begin with. The runtime logs `no hook returned replace-context` to
`logs/bootstrap.log`, writes a `compaction_declined` line to `trace.jsonl` with
`reason: "no_hook_replacement"`, and the session continues with the uncompacted history.

**A bound hook returned a replacement the runtime rejected.** A compacted history whose tool calls
and tool results no longer pair up would make the next request malformed, so the runtime discards
the replacement whole and keeps the original, larger history. Non-fatal: it logs
`compacted result has an unresolved tool_call` to `logs/bootstrap.log`, writes a
`compaction_declined` line with `reason: "unresolved_tool_call"`, and the session continues over
budget. Recurring `unresolved_tool_call` declines mean the hook is dropping one half of a
call/result pair.

**A bound hook ran and returned an error.** This *is* treated as a failure, and it ends the
session: there is no fallback compactor behind a declared compaction hook, so continuing would
mean another inference turn on a context already known to be over budget. `out/result.txt`
records the error, the session's `trace.jsonl` records `session_end` with `exit_status: "failed"`,
OTel emits `session_end` as `"failed"` (if `observability.otel_endpoint` is configured), and — if
the session has a `task_id` — the final SSE `status` event reports `state: "failed"`. The agent
loop does not attempt another turn after this.

If your compaction hook can fail (for example, the model it calls for summarization is
unreachable), account for the fact that this ends the session rather than silently skipping
compaction.

---

## Summary

| Manifest setting | Effect |
|---|---|
| `context.max_tokens: N` | Sets the token budget; required to enable compaction |
| `murmur-hook-compact` with `runtime: hook` (or any hook bound to `on-compaction`) | Provides the compaction hook; required to enable compaction |
| `inference.compaction.threshold: 0.85` | Compaction fires when session tokens reach 85% of the budget |
| `inference.compaction.model: claude-haiku-4-5` | Uses a different (typically cheaper) model for compaction calls |
| `inference.compaction.dump_summaries: true` | Appends every committed compaction's summary to `out/compaction-summaries.jsonl`; default `false` |
| No hook bound to `on-compaction` | Non-fatal — session continues with the uncompacted history; `compaction_declined` written to the trace |
| A bound hook returns a replacement with an unpaired tool call | Non-fatal — the replacement is discarded and the session continues with the uncompacted history; `compaction_declined` written to the trace |
| A bound hook returns an error | Fatal — the session ends as failed; see [Compaction failure modes](#compaction-failure-modes) |
| Token count after compaction | Reset to the count of the new (compacted) history, not to zero |
