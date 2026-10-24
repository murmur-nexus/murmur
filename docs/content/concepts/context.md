# Context

Long-running agent sessions accumulate tokens with every message. Context compaction
automatically condenses the message history so the session can continue without hitting a
hard limit:

1. After each driver response, the runtime measures how much context the call used, its
   `session_tokens`:

    | The response | `session_tokens` |
    |---|---|
    | Reported [usage](../reference/wit-interfaces.md#driver-usage) with both `input_tokens` and `output_tokens` | The provider's count: `input_tokens + cached_tokens + cache_write_tokens + output_tokens` |
    | Reported nothing | The runtime's estimate of the request plus its count of the response |

2. It checks whether `session_tokens` over the call's
   [input budget](../reference/manifest.md#inference-max-tokens) has crossed
   `inference.compaction.threshold`. The input budget is the most input the provider accepts for
   the model, or the context window less the output cap the call sent, whichever is smaller:

    | Term | Value |
    |---|---|
    | Context window | [`context.max_tokens`](../reference/manifest.md#field-context), or the `context_window` the serving driver's [`models:`](../reference/manifest.md#driver-models) table declares for the model |
    | Output cap | The `max_tokens` the call sent |
    | Input ceiling | The `max_input_tokens` the serving driver's table declares for the model, when it declares one |

    A provider whose input limit is below its window less the output cap — 922000 input tokens of
    a 1050000-token window with a 128000 cap — declares that limit as `max_input_tokens`, and a
    session on it compacts at `threshold` × 922000, before a request would exceed what the
    provider accepts. A launch whose output cap leaves no input budget is refused with
    [`E-RUN-055`](../reference/diagnostics.md#e-run-055).
3. Once crossed, the runtime fires the `on-compaction` lifecycle event with the full message
   history. The runtime picks the compaction hook by binding, so you configure no artifact name
   anywhere — any hook bound to `on-compaction` receives the event. `murmur/hook-compact` is the
   reference implementation.
4. The hook returns a condensed message array; the runtime replaces the in-memory history and
   recounts tokens against it. Each returned `message.content` may be a plain summary string or
   an array of content blocks — the runtime accepts either. A `tool`-role message survives only
   if the hook returns it unmodified.
5. The agent loop continues — the model's next turn sees the compacted history.

Compaction never consumes a turn slot. Whether a failure to compact is fatal depends on why it
failed:

| Why compaction did not replace the context | Outcome |
|---|---|
| No hook bound to `on-compaction`, or a replacement the runtime rejected | Non-fatal. A `compaction_declined` line in `trace.jsonl` names the turn and the reason, and the session continues with the uncompacted history |
| A bound hook returned an error after a spend ceiling refused its `run-inference` call | The session ends with `exit_status: "spend_ceiling_reached"`. `out/result.txt` in the session's [records directory](../reference/workdir.md#records-directory) reads `stopped: spend ceiling reached: …`, and the trace carries one `spend_ceiling_reached` line tagged with the hook's `origin` |
| A bound hook returned any other error | The session ends with `exit_status: "failed"`. `out/result.txt` in the records directory records the error, and the SSE stream (if the session has a `task_id`) emits a final `status` event with `state: "failed"` |

A hook error is fatal because there is no fallback compactor behind a declared compaction hook, and
another turn would run on a context already known to be over budget. No further turns run.

Compaction requires both a context window and a hook bound to `on-compaction` to be
staged — see [Enable context compaction](../how-to/context-compaction.md) for the full
configuration and protocol.

## Switching drivers { #switching-drivers }

A capsule that declares [`inference.alternates`](../reference/manifest.md#inference-alternates) can
move its agent loop between driver choices while it runs — to a cheaper model of the same provider,
or to another provider. One rule governs the conversation, whether the switch lands between tasks
or in the middle of one.

**The history is carried unchanged.** The runtime keeps the conversation in one provider-neutral
format — text, tool calls and their results, images, reasoning — and every driver translates it
into its own provider's shape on every request. The next driver receives the same messages the
previous one did. A switch between tasks and a switch mid-task differ only in how much history
there is. Tool call ids, tool results, fences, images, the system prompt, the tool list and the
prompt-cache routing key are all unchanged by a switch.

**What belongs to one provider does not cross.** Only the runtime knows a switch happened, because a
driver sees one request at a time and cannot tell which model wrote a message in it.

| Provider-bound state | Across a switch |
|---|---|
| A held [`continuation_id`](../reference/wit-interfaces.md#stateful-driver-continuation) | Held for the choice it was returned under. The first call under another choice is a full resend |
| A `thinking` block, which carries its provider's signature for the model that wrote it | Sent only to the driver and model that produced it, and left out of every other choice's request. Sent again after a switch back |

To keep reasoning with its author, every assistant message records its producer under
[`produced_by`](../reference/workdir.md#the-conversation-record). The stored history and the
conversation record are never edited: what changes is only what each request is sent. A resumed
conversation keeps those marks, so reasoning an alternate wrote is not replayed to the primary.

**A switch between a tool call and its result** hands the new model a call it did not make. The
conversation carries that call and its result faithfully; a provider that insists on seeing its own
reasoning on that turn is its driver's to accommodate.

**Token counts are the provider's when it reports them, and an estimate otherwise.** The
estimate counts with one tokenizer (`cl100k`), whatever the provider, and can be several percent
above or below the provider's own count. Where each number comes from:

| Number | Source |
|---|---|
| The compaction trigger's `session_tokens` | The provider's count of the call when it reported one, the estimate otherwise |
| A request's estimate, before it is sent | The provider's count from the last call that reported one, plus the estimate of each message added since. The whole request is estimated when no count applies: a task's first request, after a compaction or seed replaced the context, after the tool list changed, on another driver choice, and for a driver that reports nothing |
| Spend charged for a call | The provider's count when it reported one, the estimate otherwise |
| Spend reserved before a call | The estimate, plus the call's output cap |

A switch to another choice measures its first request whole. Each choice has its own
[output cap, context window and input ceiling](../reference/manifest.md#inference-max-tokens),
resolved at launch, and the compaction trigger measures against the input budget of the choice that
served the turn.
Each turn's `inference` line in `trace.jsonl` names the choice that served it, beside the
provider's own counts in `input_tokens_actual` and `output_tokens_actual`, so the gap between the
estimate and each model's own count is visible per model.

Only agent-loop turns follow a switch; see
[what follows a switch](../reference/manifest.md#inference-alternates) for the calls that stay on
the primary.
