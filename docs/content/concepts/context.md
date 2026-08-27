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

- **No hook bound to `on-compaction`, or a replacement the runtime rejected** — non-fatal. The
  runtime writes a `compaction_declined` line to `trace.jsonl` naming the turn and the reason, and
  continues the session with the uncompacted history.
- **A bound hook ran and returned an error** — fatal. There is no fallback compactor behind a
  declared compaction hook, so the runtime ends the session as failed rather than continuing on
  a context it already knows is over budget: `out/result.txt` records the error, the trace and
  OTel (if configured) record `session_end` as `"failed"`, and the SSE stream (if the session has
  a `task_id`) emits a final `status` event with `state: "failed"`. No further turns run.

Compaction requires both `context.max_tokens` to be set and a hook bound to `on-compaction` to be
staged — see [Enable context compaction](../how-to/context-compaction.md) for the full
configuration and protocol.
