# How to enable context compaction for long-running tasks

An agent session accumulates tokens with every message. For tasks that take many turns — processing large files, iterating on a plan, or running inside a persistent capsule — the conversation history can approach the model's context window. Context compaction automatically condenses the message history so the session can continue without hitting a hard limit.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [context.max_tokens](../reference/manifest.md#field-context) | The context window compaction measures against; default: the window the driver's `models:` table declares for the model |
| [inference.compaction.threshold](../reference/manifest.md#field-inference) | Fraction of the context window that triggers compaction |
| [inference.compaction.model](../reference/manifest.md#field-inference) | Model used for the compaction call (optional override) |
| [inference.compaction.system_prompt](../reference/manifest.md#field-inference) | System prompt override for the compaction call (optional; hook picks its own default when unset) |
| [inference.compaction.system_prompt_file](../reference/manifest.md#field-inference) | Same override, loaded from a file next to the manifest (optional; mutually exclusive with `system_prompt`) |
| [inference.compaction.dump_summaries](../reference/manifest.md#field-inference) | When `true`, appends one JSON line per committed compaction to `out/compaction-summaries.jsonl` in the session's records directory (optional; default `false`) |
| [inference.compaction.max_tokens](../reference/manifest.md#field-inference) | Output cap of the compaction call (optional; default: the driver's ceiling for the model) |
| [inference.compaction.driver_config](../reference/manifest.md#field-inference) | Driver settings laid over `inference.driver.config` for the compaction call only (optional) |

---

## Step 1 — create murmur.yaml with context.max_tokens and the compaction artifact

Compaction requires two things: a context window, and `murmur/hook-compact` declared as a hook artifact. The window is the one the driver's [`models:`](../reference/manifest.md#driver-models) table declares for the model; setting `context.max_tokens` overrides it, and gives a window to a model the table does not list. Create a `murmur.yaml` file:

=== "Anthropic"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_anthropic }}

    artifacts:
      - name: murmur/driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}
      - name: murmur/hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

    inference:
      transport: http
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_openai }}

    artifacts:
      - name: murmur/driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}
      - name: murmur/hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

    inference:
      transport: http
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: my-agent
    version: "0.1.0"

    context:
      max_tokens: {{ v.max_tokens_deepseek }}

    artifacts:
      - name: murmur/driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}
      - name: murmur/hook-compact
        version: "{{ v.murmur_hook_compact }}"
        runtime: hook

    inference:
      transport: http
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur/driver-deepseek
    ```

Set `context.max_tokens` from your model's documentation when the driver does not list the model; most current frontier models support between 400,000 and 1,000,000 tokens. [`mur doctor`](../reference/cli.md#mur-doctor) prints the window each driver choice resolves to and where it came from.

Using `runtime: hook` ensures the model never sees `murmur/hook-compact` as a callable tool — the runtime invokes it directly at fixed lifecycle points.

---

## Step 2 — install dependencies

Both a context window and the compaction hook must be present for compaction to activate. Either one alone is not enough.

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

To install the compaction artifact directly without going through the manifest:

```bash
mur install murmur/hook-compact@{{ v.murmur_hook_compact }}
```

Compaction is not built into the Murmur runtime — it is externalized as an artifact by design. Everything in Murmur is composable, including something as fundamental as how context is condensed. `murmur/hook-compact` is the default implementation: it summarizes the message history using the configured model and replaces it with a compact representation. But compaction strategy is not one-size-fits-all. A coding agent might benefit from keeping the full tool call history and compacting only prose; a research agent might maintain a structured memory store rather than a rolling summary. You can build any of these as a hook artifact, package it once, and swap it in by changing a single line in the manifest. The runtime does not care which artifact provides compaction — only that one is declared with `runtime: hook` and responds to the compaction lifecycle event.

---

## Step 3 — set the compaction threshold

The threshold controls when compaction fires. It is a fraction of the context window. The default is `0.98` — compaction fires when the context reaches 98% of the context window.

The runtime checks it after each response, against the provider's own count of the call when the driver reports [usage](../reference/wit-interfaces.md#driver-usage), and against its own estimate when it does not. The estimate can be several percent above or below the provider's count, so a driver that reports nothing compacts somewhat earlier or later than one that does. The trace's `compaction` line says which number fired it in `tokens_before_source`: `"actual"` or `"estimate"`.

For long-running tasks where you want compaction to kick in earlier and leave headroom for recovery, add `compaction.threshold` under `inference`:

=== "Anthropic"

    ```yaml
    inference:
      transport: http
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
      compaction:
        threshold: 0.85
    ```

=== "OpenAI"

    ```yaml
    inference:
      transport: http
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
      compaction:
        threshold: 0.85
    ```

=== "DeepSeek"

    ```yaml
    inference:
      transport: http
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur/driver-deepseek
      compaction:
        threshold: 0.85
    ```

With `threshold: 0.85`, compaction fires when the session has consumed 85% of the context window.

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

Setting both `system_prompt` and `system_prompt_file` on the same `compaction:` block is a manifest error — pick one. `mur deploy run` uploads the referenced file alongside the manifest, so a remote run resolves it the same way.

If left unset, `compaction-event.system-prompt` arrives at the hook as `none`, and the hook falls back to its own built-in default prompt. `model` and the two prompt-source fields are independent — setting one does not require or affect the other.

---

## Step 4 — run and confirm the configuration

```bash
mur run
```

At startup the runtime writes a generated `MURMUR.md` into the session's [runtime directory](../reference/workdir.md#runtime-directory), which exists while the session runs. Its **Limits** section reports compaction status — check it from a shell command the capsule runs, or from the host while the session is up, to confirm compaction was configured correctly:

```bash
grep "Context budget" ~/.murmur/sessions/<session_id>/runtime/MURMUR.md
```

When the primary choice has a context window and a hook bound to `on-compaction` is staged, the status reads `compaction configured`:

```text
- Context budget: 1000000 tokens (compaction configured)
```

If either is missing — no window, or no `on-compaction` hook staged — it reads `compaction not configured`:

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

`inference.max_turns` counts inference calls, so a compaction hook that summarizes through `run-inference` spends one of the task's turns on each call it makes, and a hook that replaces the context without calling `run-inference` spends none. The model continues from where it left off with the condensed history.

### The task is kept { #the-task-is-kept }

The message that opened the current task stays in context through every compaction. When the
hook's replacement leaves it out, the runtime puts it back ahead of what the hook returned:

| The hook returns | The next turn starts from |
|---|---|
| A summary alone, as `murmur/hook-compact` does | The task message, then the summary |
| The task message among its own messages | What the hook returned, with the task message where the hook put it |

The task message is put back unchanged, under its own id, so the conversation record holds it once
and the next compaction hands it to the hook with the other messages. The token count after the
compaction includes it. On a reopened attempt that starts from a fresh context, the task message is
the rewritten task that attempt began with.

The launch-time `mur run --resume-mode compact` pass runs before the task message exists, so it
commits what the hook returned as it is.

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

Each committed compaction then appends one line to `out/compaction-summaries.jsonl` in the
session's [records directory](../reference/workdir.md#records-directory), holding what the hook
returned and not the task message the runtime kept ahead of it:

```json
{"turn":17,"tokens_before":81501,"tokens_after":334,"summary":"1. THE BUG: ..."}
```

See [Compaction summaries](../reference/workdir.md#compaction-summaries) for the
full field reference. The file is only created once the first compaction actually commits, and
a compaction the tool-call-pairing safety net rejects writes nothing.

---

## Step 7 — cap the compaction call and set its effort

The compaction call is an ordinary inference call through the primary driver, and by default it
asks for the model's whole output ceiling and runs at the agent's own driver settings. A model at
high reasoning effort can spend most of a compaction's time thinking about a summary that needs
little of it. Set the call apart from the agent's turns under `inference.compaction`:

```yaml
inference:
  transport: http
  model: glm-5.3
  max_tokens: 16000
  driver:
    artifact: murmur/driver-zai
    config:
      reasoning_effort: max
  compaction:
    max_tokens: 8000
    driver_config:
      reasoning_effort: low
```

| Key | What the compaction call gets |
|---|---|
| `max_tokens` | This output cap, in place of the `max_output_tokens` the driver's `models:` table declares for the compaction model. Spend ceilings reserve this amount for the call |
| `driver_config` | `inference.driver.config` with these top-level keys replacing or added to its own. Here the call runs at `reasoning_effort: low`; every agent turn keeps `max` |

Both apply to every `run-inference` call a hook makes while it serves `on-compaction`: the
threshold trigger, `mur run --resume … --resume-mode compact`, and a seed's overflow summary. Agent
turns, alternates and calls a hook makes for any other event keep `inference.max_tokens` and
`inference.driver.config`. Which keys `driver_config` takes, and which values, is the driver's
own: see the driver's README.

A `max_tokens` above the driver's ceiling for the compaction model refuses the launch with
[`E-RUN-054`](../reference/diagnostics.md#e-run-054), and [`mur doctor`](../reference/cli.md#mur-doctor)
reports the same. Under `transport: process` both keys are refused: the harness runs its own
compaction.

A slow compaction call counts against the compaction hook's own deadline, 30 seconds by default.
To give the hook longer without changing the deadline of every other call, set it on the hook's
entry:

```yaml
artifacts:
  - name: murmur/hook-compact
    version: "{{ v.murmur_hook_compact }}"
    runtime: hook
    capabilities:
      limits:
        deadline_seconds: 120
```

---

## Compaction failure modes

Several situations can prevent compaction from replacing the context, and the runtime treats them
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

**A bound hook ran and returned an error, with no refused `run-inference` call behind it.** This
*is* treated as a failure, and it ends the
session: there is no fallback compactor behind a declared compaction hook, so continuing would
mean another inference turn on a context already known to be over budget. `out/result.txt` in
the session's records directory records the error, the session's `trace.jsonl` records `session_end` with `exit_status: "failed"`,
OTel emits `session_end` as `"failed"` (if `observability.otel_endpoint` is configured), and — if
the session has a `task_id` — the final SSE `status` event reports `state: "failed"`. The agent
loop does not attempt another turn after this.

**The driver refused a key of `inference.compaction.driver_config`.** The driver validates the
merged settings on the compaction call before anything is sent, so an unknown key or a bad value
fails that call, the hook returns its error, and the session ends as failed. The error names the
key in the driver's own words, which call the merged settings `inference.driver.config`:

```text
error[E-RUN-040]: the task ended failed: compaction hook failed: hook-compact: … driver: unrecognised inference.driver.config key(s): bogus_key. Accepted keys: reasoning_effort, thinking …
```

Agent turns before the compaction run normally, since they never see the overlay. Fix the key
under `inference.compaction.driver_config`.

**The hook ran past its deadline.** The call is interrupted and the session ends as failed, with
the deadline that fired and the key that sets it:

```text
error[E-RUN-040]: the task ended failed: compaction hook failed: hook-compact: hook 'hook-compact' on-compaction exceeded its 30s execution deadline (capabilities.limits.deadline_seconds on the artifacts: entry 'murmur/hook-compact') and was interrupted
```

Raise `capabilities.limits.deadline_seconds` on the hook's entry, as in
[Step 7](#step-7-cap-the-compaction-call-and-set-its-effort), or lower
`inference.compaction.max_tokens` and the effort so the call finishes sooner.

**A bound hook returned an error after a spend ceiling refused its `run-inference` call.** This
ends the session as a spend stop, not as a failure. The hook's call crossed
[`inference.max_session_tokens`](../reference/manifest.md#inference-max-session-tokens) or
`spend.machine_tokens_per_day`, so nothing was sent:

| Surface | What it records |
|---|---|
| `trace.jsonl` | One `spend_ceiling_reached` line with `origin: "hook:<name>"`; `task_end` and `session_end` with `exit_status: "spend_ceiling_reached"` |
| `out/result.txt` in the records directory | `stopped: spend ceiling reached: …` |
| OTel | `session_end` as `"spend_ceiling_reached"` |
| SSE | A final `status` event with `state: "failed"` and the refusal text as its message |

If your compaction hook can fail (for example, the model it calls for summarization is
unreachable), account for the fact that this ends the session rather than silently skipping
compaction.

---

## Summary

| Manifest setting | Effect |
|---|---|
| `context.max_tokens: N` | Sets the context window, over the driver's `models:` entry |
| `murmur/hook-compact` with `runtime: hook` (or any hook bound to `on-compaction`) | Provides the compaction hook; required to enable compaction |
| `inference.compaction.threshold: 0.85` | Compaction fires when the context reaches 85% of the context window |
| `inference.compaction.model: claude-haiku-4-5` | Uses a different (typically cheaper) model for compaction calls |
| `inference.compaction.dump_summaries: true` | Appends every committed compaction's summary to `out/compaction-summaries.jsonl` in the session's records directory; default `false` |
| `inference.compaction.max_tokens: 8000` | Caps the compaction call at 8000 output tokens; above the driver's ceiling for the compaction model, the launch is refused with `E-RUN-054` |
| `inference.compaction.driver_config: {reasoning_effort: low}` | Runs the compaction call with `reasoning_effort: low` laid over `inference.driver.config`; agent turns keep their own |
| `capabilities.limits.deadline_seconds` on the hook's entry | That hook's call deadline, ahead of the capsule-wide value and the 30-second default |
| No hook bound to `on-compaction` | Non-fatal — session continues with the uncompacted history; `compaction_declined` written to the trace |
| A bound hook returns a replacement with an unpaired tool call | Non-fatal — the replacement is discarded and the session continues with the uncompacted history; `compaction_declined` written to the trace |
| A bound hook returns an error with no refused `run-inference` call behind it | Fatal — the session ends as failed; see [Compaction failure modes](#compaction-failure-modes) |
| The driver refuses a key of `inference.compaction.driver_config` | Fatal — the session ends as failed with `E-RUN-040`, naming the key; see [Compaction failure modes](#compaction-failure-modes) |
| The compaction hook runs past its deadline | Fatal — the session ends as failed with `E-RUN-040`, naming `capabilities.limits.deadline_seconds` on the hook's entry |
| A bound hook returns an error after a spend ceiling refused its `run-inference` call | Fatal — the session ends with `exit_status: "spend_ceiling_reached"`, not `failed`; see [Compaction failure modes](#compaction-failure-modes) |
| Token count the threshold compares | The provider's count of the call when the driver reports usage, the runtime's estimate otherwise |
| Token count after compaction | Reset to the estimate of the new (compacted) history, not to zero; the next call's reported count replaces it |
| A replacement that leaves out the current task's message | The task message is put back at its head, unchanged; see [The task is kept](#the-task-is-kept) |
