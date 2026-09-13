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
