# Observability Schemas

Every session writes a structured record of what it did. This page documents the file
formats and the OpenTelemetry span tree they map onto.

---

## Session trace (`trace.jsonl`) schema { #session-trace-tracejsonl }

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`. The
runtime writes it directly: a capsule that declares no hook artifacts still produces one, and
nothing the capsule does can suppress or rewrite it.

**Format:** one JSON object per line (JSONL), UTF-8, line-terminated. Every line carries these
five fields, in this order, before its own payload:

| Field | Type | Notes |
|---|---|---|
| `event_type` | string | Discriminator |
| `event_id` | string | `evt_` followed by a UUID v7 in undashed lowercase hex. Unique within the file, and unique across files: an id is minted at the moment the line is written and never reused, derived from content, or reconstructed. Ids sort by mint time and carry their own millisecond timestamp |
| `parent_id` | string \| null | The `event_id` of the event this one hangs off. Always present; `null` only on `session_start` |
| `session_id` | string | Identical on every line in a session, and the name of the session directory |
| `timestamp` | u64 | Unix milliseconds |

**The event tree.** `parent_id` makes the file walkable: every non-null `parent_id` names an
`event_id` that appears earlier in the same file, and following them upward from any line
terminates at `session_start`. The tree is session → task → turn → the turn's own events:

| Event | Parents to |
|---|---|
| `session_start` | Nothing — its `event_id` is the session node |
| `task_start` | The session node. Its `event_id` is the task node |
| `task_end`, `task_reopened` | The task node |
| `inference` (agent loop's own) | The task node, or the session node between tasks. Its `event_id` is the turn node — a turn has no line of its own |
| `inference` (a hook's, carrying `origin`), `tool_call`, `skill_call`, `shell`, `compaction`, `compaction_declined` | The turn node, falling back to the task node and then the session node |
| `session_end`, `a2a_task_received`, `a2a_send`, `hook_dispatch_error` | The session node |
| `resource_list`, `resource_read`, `peer_handle_mint`, `peer_handle_redeem`, `peer_file_fetch` | The session node |

A trace with no `session_start` line — a script capsule flushing buffered `a2a_send` records into
a file that has no session frame — writes `parent_id: null` on every line, rather than naming a
parent that has no line behind it.

**`session_start`** — written once per launch, before the `on-session-start` hooks fire and
before the first task begins

| Field | Type | Notes |
|---|---|---|
| `capsule_name` | string | Manifest `name` |
| `capsule_version` | string | Manifest `version` |
| `model` | string | `inference.model` |
| `max_turns` | u32 | `inference.max_turns` — the turn ceiling each task of this launch runs under |
| `capabilities` | string[] | The capability categories the manifest granted anything under: `"network"`, `"filesystem"`, `"shell"` |
| `tools_declared` | string[] | Names of the tools offered to the model |
| `containment_declared` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the strongest class the manifest, workspace config or `--containment` asked for. Always present; `"advisory"` when none of them declared one |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the class this host can enforce, capped by `workdir_exec`. Nothing in a manifest can raise it. See [Containment](containment.md) |
| `userns_grant` | string \| null | Where this host's permission to create an unprivileged user namespace came from: `"apparmor_absent"`, `"restriction_disabled_host_wide"`, `"profile_confining"` or `"withheld"`. Always written; `null` only off Linux, where AppArmor does not exist. Two sessions can reach the same `containment_achieved` through different permissions, so read this alongside it. See [`W-SEC-013`](diagnostics.md#w-sec-013) |
| `workdir_exec` | bool | `capabilities.filesystem.workdir_exec`, always written. `true` means the session workdir kept its `Execute` right, so `capabilities.shell.allow` was advisory inside it — and it is why `containment_achieved` can read `"advisory"` on a Landlock-capable host. See [`W-SEC-011`](diagnostics.md#w-sec-011) |
| `system_prompt_source` | string | `"manifest"` \| `"cli"` \| `"none"` — where the system prompt in effect came from. `"cli"` whenever [`mur run --system-prompt`](cli.md#mur-run) was passed, including when its value was empty and therefore cleared the prompt. Always written, so its absence identifies a trace from a runtime predating the field rather than a session with no prompt |
| `system_prompt_sha256` | string \| null | SHA-256 (lowercase hex) of the prompt as resolved — the manifest's or the override's own text, before the runtime prepends its `[Capsule]` identity block. `null` when no prompt was in effect. Always written, so two sessions can be compared for prompt equality without either trace carrying the prompt itself |
| `system_prompt` | string | The resolved prompt verbatim. Written **only** when the manifest sets `trace.include_tool_output: true`; omitted otherwise, on the same terms as tool output text. Omitted regardless when no prompt was in effect |
| `effective_grants` | object | The complete grant set this session ran under — the same object [`mur run --explain-scope --json`](../how-to/different-ways-to-run-murmur.md#step-5-inspect-the-capsules-reach-before-launching-it) prints for the same manifest on the same host: `declared_containment`, `achieved_containment`, `floor_met`, `shortfall_reason` (present only when `floor_met` is `false`), `enforcement_tier`, `userns_grant`, `filesystem_scope`, `workdir_exec`, `network_allow`, `unix_sockets`, `shell_allow`, `spawn_allow`, `env_allow`, `interpreter_runtime_grants`, `staged_runtime_grants`, `state_stores` (`[]` when no artifact declares [`capabilities.state`](manifest.md#field-capabilities)), `configured_artifacts` (`[]` when no artifact declares [`config:`](manifest.md#artifact-config)), `exports_files` (`null` when the manifest declares no [`exports.files`](manifest.md#field-exports)), `peer_files` (`null` when the manifest declares no [`exports.peer_files`](manifest.md#field-exports-peer-files)) and `peer_fetch_allow` (`[]` when the manifest declares no [`capabilities.peer_fetch`](manifest.md#field-peer-fetch)). Where `capabilities` above names categories, this names the actual destinations, binaries and paths |

**`inference`** — written after each driver response is parsed

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based turn index |
| `task_id` | string \| null | The task this turn belongs to. `null` when no task is in scope |
| `input_tokens` | u64 | The runtime's own tiktoken (`cl100k_base`) estimate of the request, counted before the request was sent. This is the number the compaction threshold and the session totals run on |
| `output_tokens` | u64 | The runtime's own tiktoken estimate of the driver response |
| `decision` | string | `"tool_call"` \| `"end_turn"` \| `"text"` |
| `tool_name` | string \| null | The tool the response asked for; `null` when it asked for none |
| `input_tokens_actual` | u64 | The provider's own count of the request, from the driver's [`usage`](wit-interfaces.md#driver-usage) block |
| `output_tokens_actual` | u64 | The provider's own count of the completion |
| `cached_tokens` | u64 | Request tokens the provider served from its prompt cache |
| `cache_write_tokens` | u64 | Request tokens the provider wrote into its prompt cache |
| `origin` | string | `hook:<hook name>` when a hook produced this completion through [`run-inference`](wit-interfaces.md#murmurruntimeinference). Absent for an ordinary agent-loop turn |
| `model` | string | The model this call was sent to. Written only alongside `origin` |

The four provider-reported fields are written only when the driver reported that member, and are
absent otherwise — never `0`. They sit beside the runtime's estimates rather than replacing them,
so estimator drift is a subtraction on one line. See
[Reported token usage](wit-interfaces.md#driver-usage) for what a driver sends and what the
runtime does with it.

**`tool_call`** — written after each tool invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `task_id` | string \| null | The task this call belongs to. `null` when no task is in scope |
| `tool_name` | string | |
| `tool_call_id` | string \| null | The provider's own id for this call, recorded verbatim and never parsed. It is what pairs this line with the tool-result message the runtime sent back. `null` when the provider named none |
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
| `task_id` | string \| null | The task this call belongs to. `null` when no task is in scope |
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
| `task_id` | string \| null | The task this command belongs to. `null` when no task is in scope |
| `binary` | string | The program that ran — canonicalized absolute path when the invoked name resolved against the host `PATH` (e.g. `/usr/bin/pytest`), else the bare invoked name |
| `command` | string | The argument list alone; for a shell interpreter, the script text passed via `-c`. Read `binary` to know what ran |
| `exit_code` | i32 | Non-zero is data, not an error |
| `stdout_bytes` | u64 | |
| `stderr_bytes` | u64 | |
| `duration_ms` | u64 | |
| `resource_limit` | string | The `capabilities.resources` field this subprocess hit — `cpu_seconds`, `max_file_size_bytes`, `cgroup_memory_bytes` or `cgroup_pids_max`. Written only when the kernel's own evidence names exactly one limit, and omitted from the line otherwise — see [Which limit a subprocess hit](resource-limits.md#which-limit) |

**`compaction`** — written when context compaction fires

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `task_id` | string \| null | The task this compaction belongs to. `null` when no task is in scope |
| `tokens_before` | u64 | Context occupancy before the replacement |
| `tokens_after` | u64 | Context occupancy after it |

Both are the same measurement: occupancy is the tiktoken count of the whole serialized driver
payload — system prompt, tool inventory and the complete `messages` array — because that is what
consumes the provider's context window. `tokens_before` is the same number the turn's
`input_tokens` carries.

**`compaction_declined`** — written when the compaction threshold is crossed and the context is
left as it was

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | The turn that crossed the threshold |
| `task_id` | string \| null | The task this turn belongs to. `null` when no task is in scope |
| `tokens` | u64 | Context occupancy at the moment of the decline — the same measurement `compaction` records as `tokens_before`, and the budget the session went on running over |
| `reason` | string | `"no_hook_replacement"` when no bound hook returned `replace-context`; `"unresolved_tool_call"` when a hook's replacement was discarded because its tool calls and tool results did not pair up |

The session continues over budget on both. Each decline is also written to
`workdir/logs/bootstrap.log`. A trace can hold any number of them, and a `compaction_declined` on
one turn does not stop a later turn from compacting successfully.

**`session_end`** — written once per launch, after the `on-session-end` hooks fire and the task
loop has exited, on every exit path

| Field | Type | Notes |
|---|---|---|
| `total_turns` | u32 | Equals the count of `inference` lines |
| `total_input_tokens` | u64 | |
| `total_output_tokens` | u64 | |
| `total_tool_calls` | u32 | Equals the count of `tool_call` lines |
| `total_shell_calls` | u32 | Equals the count of `shell` lines |
| `duration_ms` | u64 | Wall-clock time from session start |
| `exit_status` | string | `"ok"` \| `"failed"` \| `"max_turns_reached"` — the last task's own terminal outcome |

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

**`resource_list`** — written when the [resource plane](resource-plane.md) answers a `list`, served
or refused

| Field | Type | Notes |
|---|---|---|
| `root` | string | `exports.files.root` verbatim. Empty when the capsule declares no export and the request was refused |
| `entry_count` | u64 | Regular files listed. `0` on any non-`ok` outcome |
| `total_bytes` | u64 | Sum of the listed files' sizes. `0` on any non-`ok` outcome |
| `generation` | u64 | Completed tasks in this process at the moment of the request |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the class this session achieved |
| `outcome` | string | `"ok"`, or the [error code](resource-plane.md#errors) the caller received |
| `reason` | string \| null | `null` on `"ok"`; one sentence otherwise |

**`resource_read`** — written when the resource plane answers a `read`, served or refused

| Field | Type | Notes |
|---|---|---|
| `path` | string | The requested path after percent-decoding, before any validation, so `%2e%2e%2f` and `../` read as one attempt |
| `outcome` | string | `"ok"`, or the [error code](resource-plane.md#errors) the caller received |
| `bytes` | u64 \| null | Bytes served. `null` on any non-`ok` outcome |
| `sha256` | string \| null | SHA-256 (lowercase hex) of the bytes served — the same value as the response's `etag`. `null` on any non-`ok` outcome |
| `generation` | u64 | Completed tasks in this process at the moment of the request |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` |
| `reason` | string \| null | `null` on `"ok"`; one sentence otherwise |

Both events are written at the moment of the request rather than at a task boundary, so a read of a
finished-but-alive capsule is recorded after that session's `session_end`.

**`peer_handle_mint`** — written by the `share-file` tool when a
[peer-file handle](resource-plane.md#peer-plane) is minted or refused

| Field | Type | Notes |
|---|---|---|
| `handle_id` | string \| null | First 16 lowercase hex characters of `sha256(<token>)`. `null` on any non-`ok` outcome — a refused mint produced no token |
| `path` | string | Relative to `exports.peer_files.root`, canonicalised on `"ok"`, and as the agent asked for it on a refusal. Never a host path |
| `audience` | string | `<peer name>@<host:port>`, lowercased. Empty when the peer's agent card could not be read |
| `expires_at_ms` | u64 \| null | Absolute expiry, Unix milliseconds. `null` on any non-`ok` outcome |
| `outcome` | string | `"ok"`, `"peer_unreachable"`, or the [error code](resource-plane.md#redeem) the mint was refused with |
| `reason` | string \| null | `null` on `"ok"`; one sentence otherwise |

**`peer_handle_redeem`** — written by the listener when `GET /resources/peer/<handle>` is answered,
served or refused

| Field | Type | Notes |
|---|---|---|
| `handle_id` | string | As above. Always present: it is derived from the token as presented, whatever the token turns out to be |
| `path` | string \| null | The handle's path relative to `exports.peer_files.root`. `null` until the MAC has verified — a payload that failed it is caller-controlled and is not recorded as fact |
| `generation` | u64 | The runtime's own counter at the moment of the request, never a value taken from the token |
| `audience_asserted` | string \| null | The `x-murmur-audience` header exactly as asserted. `null` when none was sent |
| `bytes` | u64 \| null | Bytes served. `null` on any non-`ok` outcome |
| `sha256` | string \| null | SHA-256 (lowercase hex) of the bytes served — the same value as the response's `etag`. `null` on any non-`ok` outcome |
| `outcome` | string | `"ok"`, or the [error code](resource-plane.md#redeem) the caller received |
| `reason` | string \| null | `null` on `"ok"`; one sentence otherwise |

**`peer_file_fetch`** — written by the `fetch-peer-file` tool on the ingesting side, served or
refused

| Field | Type | Notes |
|---|---|---|
| `peer` | string | The peer address the tool was given |
| `handle_id` | string | As above. Equal to the minting capsule's `handle_id` for the same handle |
| `stored_path` | string \| null | Where the bytes landed, relative to the accessible workdir. `null` on any non-`ok` outcome |
| `bytes` | u64 \| null | Bytes stored. `null` on any non-`ok` outcome |
| `sha256` | string \| null | SHA-256 (lowercase hex) of the bytes stored. `null` on any non-`ok` outcome |
| `outcome` | string | `"ok"`, `"peer_not_allowed"`, `"peer_unreachable"`, `"etag_mismatch"`, `"io_error"`, or the peer's own [error code](resource-plane.md#redeem) |
| `reason` | string \| null | `null` on `"ok"`; one sentence otherwise |

`peer_handle_mint` and `peer_file_fetch` come from the agent loop; `peer_handle_redeem` is written
by the listener, concurrently with any running task. All three are written at the moment of the
event.

**The handle itself never appears in a trace, on either side.** Where a token would otherwise reach
one — most obviously as the recorded `handle` argument of a `fetch-peer-file` `tool_call` — it is
replaced with `<handle:<handle_id>>`.

**Guarantees:**

- `trace.jsonl` exists after any capsule session, regardless of exit cause.
- One `session_start`/`session_end` pair per launch, framing every task. A launch that handles
  three queued tasks writes one pair and three `task_start`/`task_end` pairs inside it.
- Each task writes one `task_start`/`task_end` pair, however many agent-loop attempts an
  `on-task-end` hook reopened it for.
- `session_id` is identical on every line, and `event_id` is distinct on every line.
- Every non-null `parent_id` names an `event_id` written earlier in the same file.
- Count fields in the last `session_end` are cumulative across every task and attempt in the
  session, and equal the sum of the corresponding per-task fields on every `task_end`.

**Non-obvious behaviour:**

- A trace write that fails ends the session with `E-RUN-007` (see [Diagnostics](diagnostics.md)).
  The exceptions are `compaction` and `compaction_declined`: that failure is logged to
  `workdir/logs/bootstrap.log` and the session continues.
- When the launch fails before `session_start` is written (a missing driver artifact, for
  example), `trace.jsonl` is created but empty. No `session_end` is written, because no session
  started.
- A `task_end` carries the attempt's own terminal outcome, so it reads `"failed"` or
  `"max_turns_reached"` on a task the runtime survived and reported on. The launch's own
  `session_end` carries the last task's outcome the same way.

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
| `capsule.inference` | `inference` | `turn`, `input_tokens`, `output_tokens`, `decision`, `tool_name` (when the response asked for one), `input_tokens_actual`, `output_tokens_actual`, `cached_tokens` and `cache_write_tokens` (each when the driver reported it), plus `origin` and `model` for a hook-run completion |
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
