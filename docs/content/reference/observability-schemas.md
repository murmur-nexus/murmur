# Observability Schemas

Every session writes a structured record of what it did. This page documents the file
formats and the OpenTelemetry span tree they map onto.

---

## Session trace (`trace.jsonl`) schema { #session-trace-tracejsonl }

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`. The
runtime writes it directly: a capsule that declares no hook artifacts still produces one, and
nothing the capsule does can suppress or rewrite it.

**Format:** one JSON object per line (JSONL), UTF-8, line-terminated. Every line carries
`event_type` (discriminator), `session_id` (identical on every line in a session, and the name of
the session directory) and `timestamp` (Unix milliseconds).

**`session_start`** — written before the first inference call of an agent-loop attempt

| Field | Type | Notes |
|---|---|---|
| `capsule_name` | string | Manifest `name` |
| `capsule_version` | string | Manifest `version` |
| `model` | string | `inference.model` |
| `max_turns` | u32 | Turns this attempt may spend: `inference.max_turns`, less whatever earlier attempts of a reopened task already spent |
| `capabilities` | string[] | The capability categories the manifest granted anything under: `"network"`, `"filesystem"`, `"shell"` |
| `tools_declared` | string[] | Names of the tools offered to the model |
| `containment_declared` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the strongest class the manifest, workspace config or `--containment` asked for. Always present; `"advisory"` when none of them declared one |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the class this host can enforce, capped by `workdir_exec`. Nothing in a manifest can raise it. See [Containment](containment.md) |
| `userns_grant` | string \| null | Where this host's permission to create an unprivileged user namespace came from: `"apparmor_absent"`, `"restriction_disabled_host_wide"`, `"profile_confining"` or `"withheld"`. Always written; `null` only off Linux, where AppArmor does not exist. Recorded next to `containment_achieved` because the same achieved class reached through the shipped AppArmor profile and reached on a host whose unprivileged-userns hardening is off for every binary are two different security statements. See [`W-SEC-013`](diagnostics.md#w-sec-013) |
| `workdir_exec` | bool | `capabilities.filesystem.workdir_exec`, always written. `true` means the session workdir kept its `Execute` right, so `capabilities.shell.allow` was advisory inside it — and it is why `containment_achieved` can read `"advisory"` on a Landlock-capable host. See [`W-SEC-011`](diagnostics.md#w-sec-011) |
| `system_prompt_source` | string | `"manifest"` \| `"cli"` \| `"none"` — where the system prompt in effect came from. `"cli"` whenever [`mur run --system-prompt`](cli.md#mur-run) was passed, including when its value was empty and therefore cleared the prompt. Always written, so its absence identifies a trace from a runtime predating the field rather than a session with no prompt |
| `system_prompt_sha256` | string \| null | SHA-256 (lowercase hex) of the prompt as resolved — the manifest's or the override's own text, before the runtime prepends its `[Capsule]` identity block. `null` when no prompt was in effect. Always written, so two sessions can be compared for prompt equality without either trace carrying the prompt itself |
| `system_prompt` | string | The resolved prompt verbatim. Written **only** when the manifest sets `trace.include_tool_output: true`; omitted otherwise, on the same terms as tool output text. Omitted regardless when no prompt was in effect |
| `effective_grants` | object | The complete grant set this session ran under — the same object [`mur run --explain-scope --json`](../how-to/different-ways-to-run-murmur.md#step-5-inspect-the-capsules-reach-before-launching-it) prints for the same manifest on the same host: `declared_containment`, `achieved_containment`, `floor_met`, `shortfall_reason` (present only when `floor_met` is `false`), `enforcement_tier`, `userns_grant`, `filesystem_scope`, `workdir_exec`, `network_allow`, `unix_sockets`, `shell_allow`, `spawn_allow`, `env_allow`, `interpreter_runtime_grants` and `staged_runtime_grants`. Where `capabilities` above names categories, this names the actual destinations, binaries and paths |

**`inference`** — written after each driver response is parsed

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based turn index |
| `input_tokens` | u64 | |
| `output_tokens` | u64 | |
| `decision` | string | `"tool_call"` \| `"end_turn"` \| `"text"` |
| `tool_name` | string \| null | The tool the response asked for; `null` when it asked for none |
| `origin` | string | `hook:<hook name>` when a hook produced this completion through [`run-inference`](wit-interfaces.md#murmurruntimeinference). Absent for an ordinary agent-loop turn |
| `model` | string | The model this call was sent to. Written only alongside `origin` |

**`tool_call`** — written after each tool invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `tool_name` | string | |
| `input` | object | The tool input, as the model supplied it |
| `input_bytes` | u64 | Byte length of the serialized tool input |
| `output` | string | The tool output text. Written only when the manifest sets `trace.include_tool_output: true` (default `false`) |
| `output_bytes` | u64 | Byte length of the tool output text |
| `duration_ms` | u64 | |
| `status` | string | `"ok"` \| `"error"` |
| `state_effect` | string | `"read"` \| `"mutate"`, as the tool declared it. Absent when the tool declared none — see [`state_effect`](wit-interfaces.md#murmurtoolrun) |
| `resource_id` | string | The resource this call addressed, as the tool declared it. An opaque, tool-defined string. Absent when the tool declared none |

**`skill_call`** — written after each skill invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `skill_name` | string | |
| `output_bytes` | u64 | Byte length of the returned `skill.md` text |
| `duration_ms` | u64 | |
| `status` | string | `"ok"` \| `"error"` |

Skill calls are counted separately from tool calls: they never raise `total_tool_calls` or a
`task_end`'s `tool_calls`.

**`shell`** — written after each shell command returns (follows its `tool_call` line)

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `binary` | string | The program that ran — canonicalized absolute path when the invoked name resolved against the host `PATH` (e.g. `/usr/bin/pytest`), else the bare invoked name |
| `command` | string | The argument list alone; for a shell interpreter, the script text passed via `-c`. Read `binary` to know what ran |
| `exit_code` | i32 | Non-zero is data, not an error |
| `stdout_bytes` | u64 | |
| `stderr_bytes` | u64 | |
| `duration_ms` | u64 | |
| `resource_limit` | string | The `capabilities.resources` field this subprocess hit — `cpu_seconds`, `max_file_size_bytes`, `cgroup_memory_bytes` or `cgroup_pids_max`. Written only when the kernel's own evidence names exactly one limit, and omitted from the line otherwise — see [Which limit a subprocess hit](resource-limits.md#which-limit) |

**`compaction`** — written when context compaction fires

| Field | Type |
|---|---|
| `turn` | u32 |
| `tokens_before` | u64 |
| `tokens_after` | u64 |

**`session_end`** — written on every exit path of an agent-loop attempt

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

**`task_start`** — written at the start of each task, before the agent loop runs

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | UUID for this task (runtime-generated for A2A; synthesized for `task.md` path) |
| `context_id` | string | Context UUID for this task |
| `source` | string | `"a2a"` for A2A tasks; `"task_md"` for the task.md path |
| `message_parts_bytes` | u64 | Byte length of the task message text |

Resets all per-task counters. Follows `a2a_task_received` for A2A tasks; is the first event for
`task.md` tasks.

**`task_end`** — written after the agent loop returns and any hook-requested reopens are resolved,
for every task, on every exit path

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Matches the corresponding `task_start` |
| `exit_status` | string | `"ok"` if the last attempt succeeded; `"failed"` if it did not; `"reopen_budget_exhausted"` if an `on-task-end` hook still wanted to reopen the task after `lifecycle.max_task_reopens` (or the `inference.max_turns` ceiling) was reached |
| `duration_ms` | u64 | Wall-clock time from `task_start` to `task_end`, across every attempt |
| `turns` | u32 | Cumulative inference turns for this task across every attempt (reset at `task_start`) |
| `input_tokens` | u64 | Input tokens for this task only |
| `output_tokens` | u64 | Output tokens for this task only |
| `tool_calls` | u32 | Tool calls for this task only |
| `shell_calls` | u32 | Shell calls for this task only |
| `reopen_count` | u32 | Times an `on-task-end` hook reopened this task before it ended. `0` for a task that ran once (the common case). A reader that finds no `reopen_count` field should default it to `0` |

**`task_reopened`** — written once per reopen, between two agent-loop attempts of the same task,
when a blocking `on-task-end` hook (`commit_policy: reopen-task`) returns `reopen-task(reason)` and
the reopen is granted

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task being reopened |
| `hook_name` | string | Manifest name of the hook that requested the reopen |
| `reason` | string | Feedback text the hook asked to inject into the reopened task content |
| `reopen_number` | u32 | 1-based ordinal of this reopen within the task (first reopen = `1`) |

Appears zero or more times per task, always before the task's terminal `task_end`. See [Task
reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task) for the full
mechanism.

**`hook_dispatch_error`** — written when a hook call fails in a way the session survives

| Field | Type | Notes |
|---|---|---|
| `hook_name` | string | Manifest name of the hook the fault is attributed to |
| `event` | string | WIT lifecycle function name, e.g. `"on-tool-call"`, or `"drain"` for a fault raised by the session-end drain rather than by one call |
| `arm` | string | The unsupported [`hook-output` arm](wit-interfaces.md#what-each-handler-can-commit), e.g. `"write-manifests"`; or, for an async hook, `"error"` when the call returned an error, `"queue-overflow"` when its queue was full and its entry declares `on_overflow: drop`, and `"timeout"` when it was still working when the drain budget ran out |

Non-fatal: the session continues exactly as if the hook had returned `none`. A blocking hook is
recorded here when it returns an arm the event does not honor; an async hook is recorded for that
and for the three failures nothing else can surface. `on-stage` faults never reach the trace,
because staging runs before `trace.jsonl` exists. Every fault is also written to
`workdir/logs/hook-<name>.log`. Faults are flushed just before the `session_end` they precede, so
they always appear earlier in the file than the event that flushed them.

**Guarantees:**

- `trace.jsonl` exists after any capsule session, regardless of exit cause.
- Each task writes one `task_start`/`task_end` pair, with one `session_start`/`session_end` pair
  per agent-loop attempt nested inside it. A task an `on-task-end` hook reopens produces one such
  pair per attempt.
- `session_id` is identical on every line.
- Count fields in the last `session_end` are cumulative across every task and attempt in the
  session, and equal the sum of the corresponding per-task fields on every `task_end`.

**Non-obvious behaviour:**

- A trace write that fails ends the session with `E-RUN-007` (see [Diagnostics](diagnostics.md)).
  The one exception is the `compaction` event: that failure is logged to
  `workdir/logs/bootstrap.log` and the session continues.
- When the agent loop fails before `session_start` is written (a missing driver artifact, for
  example), `trace.jsonl` is created but empty. No `session_end` is written, because no session
  started.

---

## Structured evaluation (`eval.jsonl`) schema { #structured-evaluation-evaljsonl }

`murmur-hook-eval` writes `workdir/<session_id>/eval.jsonl` at session end when the capsule
declares the hook and [`observability.eval.scorers`](manifest.md#field-observability) holds at
least one scorer. The hook writes this file, not the runtime; it is a sibling of `trace.jsonl` in
the same session workdir and shares its session scope.

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

**Scorer types**, configured under
[`observability.eval.scorers`](manifest.md#field-observability):

| Type | Passes when |
|---|---|
| `exit_ok` | `exit_status == "ok"` |
| `max_turns` | `total_turns <= max` |
| `max_tokens` | `total_input_tokens + total_output_tokens <= max` |
| `tool_sequence` | `expected` list is a subsequence of observed tool calls |
| `llm_judge` | unimplemented: it logs a warning and emits no score |

---

## OTel span emission

Setting [`observability.otel_endpoint`](manifest.md#field-observability) turns on two independent
export paths:

| Path | Exports | Failures |
|---|---|---|
| The runtime's own emitter | Each span as an OTLP/HTTP JSON POST to `<otel_endpoint>/v1/traces`, sent as its event happens; the root `capsule.session` span goes last. Always present — no artifact required | Logged to `workdir/logs/otel.log` |
| Hook-side export | The runtime injects the endpoint as the `MURMUR_OTEL_ENDPOINT` environment variable into every hook component. `murmur-hook-grafana` (and any hook that reads it) uses this to export its own enriched span tree | Logged to `workdir/logs/hook-<name>.log` |

Neither path can suppress or corrupt the other, and a failure on either is non-fatal.

**Span schema** — how `trace.jsonl` events map to OTel span names and attributes:

| Span name | Source event | Attributes |
|---|---|---|
| `capsule.session` | `session_start` / `session_end` | `exit_status` |
| `capsule.inference` | `inference` | `turn`, `input_tokens`, `output_tokens`, `decision`, `tool_name` (when the response asked for one), plus `origin` and `model` for a hook-run completion |
| `capsule.tool_call` | `tool_call` | `tool_name`, `input_bytes`, `output_bytes`, `duration_ms`, `status` |
| `capsule.shell` | `shell` | `command` (first 200 characters), `exit_code`, `duration_ms` |
| `capsule.compaction` | `compaction` | `tokens_before`, `tokens_after` |

Every span carries two resource attributes: `service.name` (the capsule name) and
`service.version` (the manifest `version`). The `skill_call`, task and A2A events have no span of
their own — they appear in `trace.jsonl` alone.

**Non-obvious behaviour:**

- Each span is POSTed as its event happens, over a connection the agent loop waits on, so a slow
  endpoint slows the session down.
- `trace.jsonl` is written whether or not `observability.otel_endpoint` is set, and whether or not
  the endpoint is reachable.
- The `MURMUR_FORMATION_ID` host environment variable, when set, is forwarded into every hook's
  WASI environment and added as `murmur.formation_id` to the root span by `murmur-hook-grafana`.
