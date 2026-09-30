# Context

Long-running agent sessions accumulate tokens with every message. Context compaction
automatically condenses the message history so the session can continue without hitting a
hard limit:

1. The runtime tracks `session_tokens` on every turn.
2. After each driver response, it checks whether `session_tokens / context.max_tokens` has
   crossed `inference.compaction.threshold`.
3. Once crossed, the runtime fires the `on-compaction` lifecycle event with the full message
   history. The runtime picks the compaction hook by binding, so you configure no artifact name
   anywhere — any hook bound to `on-compaction` receives the event. `murmur-hook-compact` is the
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
| A bound hook returned an error after a spend ceiling refused its `run-inference` call | The session ends with `exit_status: "spend_ceiling_reached"`. `out/result.txt` reads `stopped: spend ceiling reached: …`, and the trace carries one `spend_ceiling_reached` line tagged with the hook's `origin` |
| A bound hook returned any other error | The session ends with `exit_status: "failed"`. `out/result.txt` records the error, and the SSE stream (if the session has a `task_id`) emits a final `status` event with `state: "failed"` |

A hook error is fatal because there is no fallback compactor behind a declared compaction hook, and
another turn would run on a context already known to be over budget. No further turns run.

Compaction requires both `context.max_tokens` to be set and a hook bound to `on-compaction` to be
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

**Token counts are an estimate, and a switch makes them a worse one.** Context occupancy, the
compaction trigger and the spend admission all count the request with one tokenizer (`cl100k`),
whatever the provider. After a switch they count against a model whose tokenizer and context
window may differ more from that estimate. There is no per-model window: `context.max_tokens` and
`inference.max_tokens` are one number each for the session, and must suit every declared model.
Each turn's `inference` line in `trace.jsonl` names the choice that served it, beside the
provider's own counts in `input_tokens_actual` and `output_tokens_actual`, so the gap between the
estimate and each model's own count is visible per model.

Only agent-loop turns follow a switch. Compaction, a hook's `run-inference` and seed
summarization stay on the primary driver, and a switch does not survive a restart.
