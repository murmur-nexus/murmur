# Observability Schemas

Every session writes a structured record of what it did. This page documents the file
formats and the OpenTelemetry span tree they map onto.

---

## Session trace (`trace.jsonl`) schema { #session-trace-tracejsonl }

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`. The
file is written by the runtime — not by the capsule — and cannot be suppressed or falsified
by the capsule. It exists even when no hook artifacts are declared.

**Format:** one JSON object per line (JSONL), UTF-8, line-terminated. Every line carries
`event_type` (discriminator), `session_id` (runtime-generated UUID v7, identical on every line
in a session), and `timestamp` (Unix milliseconds).

**Event types** — six standard events, plus two A2A events, three task events, and a hook-fault event:

**`session_start`** — written before the first inference call

| Field | Type |
|---|---|
| `capsule_name` | string |
| `capsule_version` | string |
| `model` | string |
| `max_turns` | u32 |
| `capabilities` | string[] |
| `tools_declared` | string[] |
| `containment_declared` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the effective declared floor, always present even when no manifest/config/flag ever declared one (defaults to `"advisory"`) |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the host's probed kernel capability, capped by `workdir_exec` below. Nothing in a manifest can raise it |
| `workdir_exec` | bool | `capabilities.filesystem.workdir_exec`, always written. `true` means the session workdir kept its Landlock `Execute` right, so `capabilities.shell.allow` was advisory inside it — and it is why `containment_achieved` can read `"advisory"` on a Landlock-capable host. See [`W-SEC-011`](diagnostics.md#w-sec-011) |
| `effective_grants` | object | The complete grant set this session ran under — same object, field for field, as [`mur run --explain-scope --json`](../how-to/different-ways-to-run-murmur.md#step-5-inspect-the-capsules-reach-before-launching-it) prints for the same manifest on the same host: `declared_containment`, `achieved_containment`, `floor_met`, `enforcement_tier`, `filesystem_scope`, `workdir_exec`, `network_allow`, `unix_sockets`, `shell_allow`, `spawn_allow`, `env_allow`, `interpreter_runtime_grants`, `staged_runtime_grants`, and `shortfall_reason` (present only when `floor_met` is `false`). Unlike `capabilities` above, which only names *categories* (`"network"`, `"shell"`, ...), this field names the actual destinations, binaries and paths granted — the property an auditor reading a finished trace needs, without re-parsing the manifest |

**`inference`** — written after each driver response is parsed

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based turn index |
| `input_tokens` | u64 | |
| `output_tokens` | u64 | |
| `decision` | string | `"tool_call"` \| `"end_turn"` \| `"text"` |
| `tool_name` | string \| null | Present only when `decision` is `"tool_call"` |

**`tool_call`** — written after each tool invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `tool_name` | string | |
| `input_bytes` | u64 | Byte length of the serialized tool input |
| `output_bytes` | u64 | Byte length of the tool output text |
| `duration_ms` | u64 | |
| `status` | string | `"ok"` \| `"error"` |

**`shell`** — written after each shell command returns (follows its `tool_call` line)

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `binary` | string | The program that ran — canonicalized absolute path when the invoked name resolved against the host `PATH` (e.g. `/usr/bin/pytest`), else the bare invoked name |
| `command` | string | First 200 characters |
| `exit_code` | i32 | Non-zero is data, not an error |
| `stdout_bytes` | u64 | |
| `stderr_bytes` | u64 | |
| `duration_ms` | u64 | |

**`compaction`** — written when context compaction fires

| Field | Type |
|---|---|
| `turn` | u32 |
| `tokens_before` | u64 |
| `tokens_after` | u64 |

**`session_end`** — always the last line; written on every exit path

| Field | Type | Notes |
|---|---|---|
| `total_turns` | u32 | Equals the count of `inference` lines |
| `total_input_tokens` | u64 | |
| `total_output_tokens` | u64 | |
| `total_tool_calls` | u32 | Equals the count of `tool_call` lines |
| `total_shell_calls` | u32 | Equals the count of `shell` lines |
| `duration_ms` | u64 | Wall-clock time from session start |
| `exit_status` | string | `"ok"` \| `"failed"` \| `"max_turns_reached"` |

**`a2a_task_received`** — written when an incoming message reserves the task slot

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Runtime-generated UUID |
| `context_id` | string | Echoed or generated `contextId` |
| `message_id` | string | `messageId` from the incoming A2A Message |
| `traceparent_from_caller` | string \| null | W3C `traceparent` header from the incoming request |

**`a2a_send`** — written when a capsule component calls `murmur:message/send`

| Field | Type | Notes |
|---|---|---|
| `peer_url` | string | Target capsule URL |
| `message_id` | string | `message-id` from the outgoing Message |
| `task_id` | string | Task ID returned by the peer |
| `context_id` | string | Context ID returned by the peer |
| `traceparent` | string \| null | W3C `traceparent` injected on the outgoing request |

**`task_start`** — written at the start of each task, before `run_agent_loop`

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | UUID for this task (runtime-generated for A2A; synthesized for `task.md` path) |
| `context_id` | string | Context UUID for this task |
| `source` | string | `"a2a"` for A2A tasks; `"task_md"` for the task.md path |
| `message_parts_bytes` | u64 | Byte length of the task message text |

Resets all per-task counters. Follows `a2a_task_received` for A2A tasks; is the first event for `task.md` tasks.

**`task_end`** — written after `run_agent_loop` returns (and any hook-requested reopens are resolved), for every task

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Matches the corresponding `task_start` |
| `exit_status` | string | `"ok"` if the last attempt returned `Ok(())`; `"failed"` if it returned `Err(_)`; `"reopen_budget_exhausted"` if an `on-task-end` hook still wanted to reopen the task after `inference.max_task_reopens` (or the `inference.max_turns` ceiling) was reached |
| `duration_ms` | u64 | Wall-clock time from `task_start` to `task_end`, across every attempt |
| `turns` | u32 | Cumulative inference turns for this task across every attempt (reset at `task_start`) |
| `input_tokens` | u64 | Input tokens for this task only |
| `output_tokens` | u64 | Output tokens for this task only |
| `tool_calls` | u32 | Tool calls for this task only |
| `shell_calls` | u32 | Shell calls for this task only |
| `reopen_count` | u32 | Times an `on-task-end` hook reopened this task before it ended. `0` for a task that ran once (the common case). A reader that finds no `reopen_count` field should default it to `0`. |

Written unconditionally after the task's last attempt, even on error exit (exit_status will be `"failed"` or `"reopen_budget_exhausted"`). Always follows the corresponding `session_end`.

**`task_reopened`** — written once per reopen, between two agent-loop attempts of the same task, when a blocking `on-task-end` hook (`commit_policy: reopen-task`) returns `reopen-task(reason)` and the reopen is granted

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task being reopened |
| `hook_name` | string | Manifest name of the hook that requested the reopen |
| `reason` | string | Feedback text the hook asked to inject into the reopened task content |
| `reopen_number` | u32 | 1-based ordinal of this reopen within the task (first reopen = `1`) |

Appears zero or more times per task, always before the task's terminal `task_end`. See [Task
reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task) for the full
mechanism.

**`hook_dispatch_error`** — written when a hook returns a `hook-output` arm the lifecycle event it fired from does not honor (see [Honored `hook-output` arm per event](wit-interfaces.md#murmurhooklifecycle))

| Field | Type | Notes |
|---|---|---|
| `hook_name` | string | Manifest name of the hook that returned the unsupported arm |
| `event` | string | WIT lifecycle function name, e.g. `"on-tool-call"` |
| `arm` | string | The unsupported `hook-output` arm name, e.g. `"write-manifests"` |

Non-fatal: the session continues exactly as if the hook had returned `none`. Written just before the `session_end`/`task_end` it precedes, so it always appears earlier in the file than the event that flushed it. Never written for `on-stage` (staging runs before `trace.jsonl` exists) or for async hooks (fire-and-forget; logged to `workdir/logs/hook-<name>.log` only) — both still get a log line, just no trace record.

**Guarantees:**

- `trace.jsonl` exists after any capsule session, regardless of exit cause.
- For single-task (ephemeral) sessions: `session_start` is the first event and `session_end` is the last.
- For multi-task (persistent) sessions: `task_start` precedes `session_start` for each task; `task_end` follows `session_end` for each task. Each task produces exactly one `task_start`/`task_end` pair.
- `session_id` is identical on every line and matches `StagedSession.session_id`.
- Count fields in the last `session_end` equal the cumulative per-event totals across all tasks in the session.
- For multi-task sessions, the last `session_end` total fields equal the sum of all `task_end` per-task fields.
- Field names are snake_case translations of the hook WIT kebab-case field names (e.g. `input-tokens` → `input_tokens`).

**Non-obvious behaviour:**

- The trace is written by direct file append; it does **not** route through the `murmur:hook/lifecycle` WIT interface. A capsule that declares no hook artifacts still produces `trace.jsonl`.
- Compaction trace write errors are non-fatal (logged to `bootstrap.log`) because `try_compact_messages` itself is non-fatal. All other trace write errors surface as `RuntimeError::AgentLoopFailed`.
- If `run_agent_loop` returns `Err` before `session_start` is written (e.g. driver artifact missing), `trace.jsonl` is created but empty. `session_end` is not written because no session started.

---

## Structured evaluation (`eval.jsonl`) schema { #structured-evaluation-evaljsonl }

When `murmur-hook-eval` is declared in the capsule manifest with at least one scorer configured, the hook writes `workdir/<session_id>/eval.jsonl` at `session_end`. The file is **not** written by the runtime itself — it is written by the hook component. `trace.jsonl` is always written by the runtime; `eval.jsonl` is only written when `murmur-hook-eval` is active and has at least one scorer. The two files are siblings in the same session workdir and share the same session scope.

**Format:** one JSON object per line (JSONL). Two record types, distinguished by `record_type`.

**Per-event score** (`record_type = "event_score"`) — one line per scorer:

| Field | Type | Notes |
|---|---|---|
| `record_type` | `"event_score"` | discriminator |
| `ts` | u64 | Unix milliseconds |
| `turn` | u32 | Turn count at the time of scoring |
| `event_type` | string | Lifecycle event that triggered the score (e.g. `"session_end"`) |
| `scorer` | string | Scorer name from manifest |
| `result` | `"pass"` \| `"fail"` | Binary outcome |
| `score` | f64 | `1.0` = pass, `0.0` = fail |
| `reason` | string | Human-readable explanation (e.g. `"turns=3 max=5"`) |

**Dataset run summary** (`record_type = "dataset_run"`) — one line per session, always last:

| Field | Type | Notes |
|---|---|---|
| `record_type` | `"dataset_run"` | discriminator |
| `ts` | u64 | Unix milliseconds |
| `dataset_id` | string \| null | From `observability.eval.dataset_id` |
| `case_id` | string \| null | From `MURMUR_CASE_ID` (set by `mur eval run`) |
| `overall` | `"pass"` \| `"fail"` \| `"no_scores"` | `fail` if any scorer fails; `no_scores` if no scores were emitted |
| `scores` | object | Map of scorer name → float score |

Example:

```jsonl
{"record_type":"event_score","ts":1778161473790,"turn":2,"event_type":"session_end","scorer":"turn_limit","result":"pass","score":1.0,"reason":"turns=2 max=5"}
{"record_type":"dataset_run","ts":1778161473790,"dataset_id":"my-ds","case_id":"case_001","overall":"pass","scores":{"turn_limit":1.0,"success_check":1.0}}
```

**Scorer types:**

| Type | Passes when |
|---|---|
| `exit_ok` | `exit_status == "ok"` |
| `max_turns` | `total_turns <= max` |
| `max_tokens` | `total_input_tokens + total_output_tokens <= max` |
| `tool_sequence` | `expected` list is a subsequence of observed tool calls |
| `llm_judge` | recognized but not implemented — logs a warning, emits no score |

---

## OTel span emission

When `observability.otel_endpoint` is set, the runtime runs two independent emission paths at `session_end`:

1. **Native `OtelEmitter`** (host process) — collects lifecycle spans in memory and POSTs them as a single OTLP/HTTP JSON batch to `<otel_endpoint>/v1/traces`. This path is always present; no artifact is required.

2. **Hook-side emission via `MURMUR_OTEL_ENDPOINT`** — the runtime injects the endpoint as a WASI environment variable into every hook component. `murmur-hook-grafana` (and any hook that reads `MURMUR_OTEL_ENDPOINT`) uses this to export its own enriched span tree.

The two paths are completely independent: an error in one cannot suppress or corrupt the other. Hook OTLP failures are logged to `workdir/logs/hook-<name>.log` and are non-fatal.

**Span schema** — how `trace.jsonl` events map to OTel span names and attributes:

| Span name | Source event | Key attributes |
|---|---|---|
| `capsule.session` | `session_start` / `session_end` | `service.name` (capsule name), `service.version`, `model`, `exit_status`, `murmur.session_id` |
| `capsule.inference` | `inference` | `turn`, `input_tokens`, `output_tokens`, `decision`, `tool_name` |
| `capsule.tool_call` | `tool_call` | `tool_name`, `input_bytes`, `output_bytes`, `duration_ms`, `status` |
| `capsule.shell` | `shell` | `command` (first 200 chars), `exit_code`, `duration_ms` |
| `capsule.compaction` | `compaction` | `tokens_before`, `tokens_after` |

**Non-obvious behaviour:**

- Export is **batched at session end** — the root span's duration is known before any POST fires. There are no partial traces.
- OTel emission is **non-blocking from the agent loop**. The single synchronous TCP call happens after `run_agent_loop` returns, so a slow or unreachable endpoint never stalls inference.
- `trace.jsonl` is **always written** regardless of `otel_endpoint` — it is not conditional on a reachable endpoint.
- `service.version` is the manifest `version` for sessions launched with the current runtime.
- The `MURMUR_FORMATION_ID` host environment variable, when set, is forwarded into every hook's WASI env and added as `murmur.formation_id` to the root span by `murmur-hook-grafana`.
