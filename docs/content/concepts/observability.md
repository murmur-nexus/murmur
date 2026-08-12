# Observability

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`,
written directly by the runtime regardless of whether any hooks are declared. When
`murmur-hook-eval` is configured with at least one scorer, that hook (not the runtime)
additionally writes `workdir/<session_id>/eval.jsonl` at session end. See [Session trace
(`trace.jsonl`)](../reference/cli.md#session-trace-tracejsonl) and [Structured evaluation
(`eval.jsonl`)](../reference/cli.md#structured-evaluation-evaljsonl) for the full schemas, and
[mur trace](../reference/cli.md#mur-trace) / [mur eval](../reference/cli.md#mur-eval) for how
to read them.
