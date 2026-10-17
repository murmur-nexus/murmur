# Observability

Every agent session produces a structured trace, `trace.jsonl`, in its
[records directory](../reference/workdir.md#records-directory) under `~/.murmur/records`, written
directly by the runtime regardless of whether any hooks are declared, and kept after the session.
The capsule's own commands can read it and cannot change it. When `murmur/hook-eval` is configured
with at least one scorer and granted `capabilities.filesystem.records: true`, that hook (not the
runtime) additionally writes `hooks/eval.jsonl` there at session end. See [Session trace
(`trace.jsonl`)](../reference/observability-schemas.md#session-trace-tracejsonl) and [Structured evaluation
(`eval.jsonl`)](../reference/observability-schemas.md#structured-evaluation-evaljsonl) for the full schemas, and
[mur trace](../reference/cli.md#mur-trace) / [mur eval](../reference/cli.md#mur-eval) for how
to read them.
