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
| `task_end`, `task_reopened`, `context_seed` | The task node |
| `inference` (agent loop's own) | The task node, or the session node between tasks. Its `event_id` is the turn node — a turn has no line of its own |
| `inference` (a hook's, carrying `origin`), `tool_call`, `skill_call`, `shell`, `shell_detached`, `shell_detach_unrecorded`, `compaction`, `compaction_declined` | The turn node, falling back to the task node and then the session node |
| `call_denied`, `protected_path_denied` | The turn node, falling back to the task node and then the session node |
| `session_end`, `a2a_task_received`, `a2a_send`, `hook_dispatch_error`, `retention` | The session node |
| `shell_completed`, `shell_abandoned` | The session node — by the time either lands, the turn that started the command is over |
| `shell_lost` | The `session_start` node of the session named in `session_id`, which is the session that started the command and not the one that wrote the line |
| `resource_list`, `resource_read`, `peer_handle_mint`, `peer_handle_redeem`, `peer_file_fetch`, `delegation_start`, `delegation` | The session node |
| `plan_start` | The session node. Its `event_id` is the plan node |
| `plan_step_start`, `plan_step` | The plan node |
| `plan_end` | The plan node, or the session node for a plan file that never parsed and so has none |

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
| `resumed_from` | string \| null | The session [`mur run --resume`](cli.md#mur-run) continued, verbatim as the address resolved it. `null` on an ordinary launch. Always written, so its absence identifies a trace from a runtime predating the field |
| `context_id` | string \| null | The context id every task of this launch runs under: the `mur run --context` value, or the id `--resume` resolved to. `null` when each task mints its own — `task_start.context_id` carries the id a task actually ran under either way. Always written, on the same terms as `resumed_from` |
| `spawned_by` | string | `ses_…` — the session that spawned this one, for a capsule another capsule launched with [`delegate-task`](runtime-provided-tools.md). Written only then; the field is absent from every other line rather than written as `null`, so a capsule nobody delegated produces a byte-identical record |
| `delegation_id` | string | `dlg_…` — the delegation that created this session, character-identical to the id on the spawning session's own `delegation_start`. Present exactly when `spawned_by` is |
| `system_prompt_source` | string | `"manifest"` \| `"cli"` \| `"none"` — where the system prompt in effect came from. `"cli"` whenever [`mur run --system-prompt`](cli.md#mur-run) was passed, including when its value was empty and therefore cleared the prompt. Always written, so its absence identifies a trace from a runtime predating the field rather than a session with no prompt |
| `system_prompt_sha256` | string \| null | SHA-256 (lowercase hex) of the prompt as resolved — the manifest's or the override's own text, before the runtime prepends its `[Capsule]` identity block. `null` when no prompt was in effect. Always written, so two sessions can be compared for prompt equality without either trace carrying the prompt itself. Under [`trace.capture: content`](manifest.md#field-trace) those bytes are also stored as `blobs/<system_prompt_sha256>`. Deliberately a different value from `inference.system_sha`, which covers the augmented prompt that went on the wire |
| `effective_grants` | object | The complete grant set this session ran under — the same object [`mur run --explain-scope --json`](../how-to/different-ways-to-run-murmur.md#step-5-inspect-the-capsules-reach-before-launching-it) prints for the same manifest on the same host: `declared_containment`, `achieved_containment`, `floor_met`, `shortfall_reason` (present only when `floor_met` is `false`), `enforcement_tier`, `userns_grant`, `filesystem_scope`, `workdir_exec`, `read_only_paths` (the subtrees [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) protects; `[]` when the manifest declares none), `read_only_advisory_for` (the entries of `shell_allow` that protection is only advisory against; `[]` when it is enforced for every call the runtime can read as a write), `network_allow`, `unix_sockets`, `shell_allow`, `spawn_allow`, `env_allow`, `interpreter_runtime_grants`, `staged_runtime_grants`, `preopens` (one entry per `runtime: tool`, `runtime: driver` and `runtime: hook` entry — `artifact`, `role`, the declared `scope` or `null`, and a `surface` of `whole-workdir`, `scoped-subtree` or `nothing`; `[]` when the capsule declares only skills), `state_stores` (`[]` when no artifact declares [`capabilities.state`](manifest.md#field-capabilities)), `configured_artifacts` (`[]` when no artifact declares [`config:`](manifest.md#artifact-config)), `exports_files` (`null` when the manifest declares no [`exports.files`](manifest.md#field-exports)), `peer_files` (`null` when the manifest declares no [`exports.peer_files`](manifest.md#field-exports-peer-files)) and `peer_fetch_allow` (`[]` when the manifest declares no [`capabilities.peer_fetch`](manifest.md#field-peer-fetch)). Where `capabilities` above names categories, this names the actual destinations, binaries, capsule names and paths |

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
| `message_ids` | array of string | Ids of the messages this request embedded, in the order they sat in it. Under an active [driver continuation](wit-interfaces.md#stateful-driver-continuation) only the tail the driver has not seen is sent, and this names exactly that tail. Absent when the list is empty: a hook's own completion and the `process` transport both send a message list the runtime never minted |
| `system_sha` | string | SHA-256 (lowercase hex) of this request's `system` string — the resolved prompt with the `[Capsule]` identity block already prepended |
| `tools_sha` | string | SHA-256 (lowercase hex) of this request's serialized `tools` array |
| `response_sha` | string | SHA-256 (lowercase hex) of the raw driver response body, as the runtime read it before parsing |
| `message_shas` | array of string | SHA-256 (lowercase hex) of each message this request embedded, in send order — one entry per `message_ids` entry, over the same messages once the runtime's own identity keys are stripped |

The four provider-reported fields are written only when the driver reported that member, and are
absent otherwise — never `0`. They sit beside the runtime's estimates rather than replacing them,
so estimator drift is a subtraction on one line. See
[Reported token usage](wit-interfaces.md#driver-usage) for what a driver sends and what the
runtime does with it.

### What the wire hashes cover { #wire-hashes }

`system_sha`, `tools_sha`, `response_sha` and `message_shas` are the bytes Murmur **sent**, not
what the model **saw**: provider-side prompt injection, tokenizer differences and safety layers
all happen past the wire and are invisible to the runtime.

They are taken from the same request the driver was handed, so a `message_shas` entry hashes a
message exactly as it was serialized into that request — after the runtime's own `id` and
`source_id` bookkeeping keys are stripped, which is why no blob ever contains one.

All four are written under [`trace.capture`](manifest.md#field-trace) `meta` and `content`, and
none under `none`. They are absent on a record the runtime did not build the request for: a hook's
own completion through [`run-inference`](wit-interfaces.md#murmurruntimeinference), and the
`process` transport, both of which send a request the runtime never held.

`message_shas` does not duplicate `message_ids`. An id names an entity and is freshly minted every
run, so comparing two runs' id arrays only reports that every id differs; a hash names content, and
repeats exactly when content repeats. Comparing two runs' `message_shas` pairwise gives the
divergence index — the first position at which the two prompts stopped agreeing.

### Content blobs (`blobs/`) { #trace-blobs }

Under [`trace.capture: content`](manifest.md#field-trace) the body behind every hash above is also
written to `<session_id>/blobs/<sha256>`, beside `trace.jsonl`. A reader resolves a hash to its
body by joining the two: `cat <session_id>/blobs/<the sha the line names>`.

| Property | Value |
|---|---|
| Path | `workdir/<session_id>/blobs/<sha256>` |
| Filename | The lowercase-hex SHA-256 of that file's own contents — no prefix, no extension |
| Directory mode | `0o700`, owner only |
| Created | On the first blob written, and only under `capture: content` |
| Write policy | Write-once. A path that already exists is never rewritten, so a system prompt unchanged across a session costs one file |
| Lifetime | Session-scoped. Readable exactly as long as the session directory is; nothing prunes it |

`system_prompt_sha256` from `session_start` resolves the same way, to the resolved prompt before
the `[Capsule]` block was prepended.

**Blob bodies are the payload verbatim, unredacted** — including any peer handle token, which
`tool_call` redacts out of its own `input` and `output`. Setting `capture: content` opts in to
storing the wire payload as sent; the default, `meta`, stores no bodies at all.

**`tool_call`** — written after each tool invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `task_id` | string \| null | The task this call belongs to. `null` when no task is in scope |
| `tool_name` | string | |
| `tool_call_id` | string \| null | The provider's own id for this call, recorded verbatim and never parsed. It is what pairs this line with the tool-result message the runtime sent back. `null` when the provider named none |
| `input` | object | The tool input, as the model supplied it |
| `input_bytes` | u64 | Byte length of the serialized tool input |
| `output` | string | The tool output text, with peer handle tokens redacted. Carries the [untrusted fence](../concepts/access-control.md#threat-model) the model received it inside. Written only under [`trace.capture: content`](manifest.md#field-trace) |
| `output_bytes` | u64 | Byte length of the tool output text, fence markers included |
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
| `output_bytes` | u64 | Byte length of the returned `skill.md` text. A skill result carries no fence |
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

**`shell_detached`** — written when a command outruns
[`lifecycle.shell_grace_secs`](manifest.md#lifecycle-shell-grace-secs) and moves to the
background, in place of that command's `shell` line

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `task_id` | string \| null | The task this command belongs to. `null` when no task is in scope |
| `work_id` | string | `wrk_` followed by a UUID v7 in undashed lowercase hex. The same id appears on this command's `shell_completed` or `shell_abandoned` line |
| `binary` | string | As on `shell` |
| `command` | string | As on `shell` |
| `grace_ms` | u64 | The grace period this command outran, in milliseconds |

A demoted command raises `total_shell_calls` and its task's `shell_calls` here, and its
`shell_completed` line does not, so each shell command is counted exactly once whichever way it
ran.

**`shell_completed`** — written when a demoted command finishes and the runtime enqueues its
result as a task

| Field | Type | Notes |
|---|---|---|
| `work_id` | string | The `shell_detached` line's `work_id` |
| `binary` | string | As on `shell` |
| `command` | string | As on `shell` |
| `exit_code` | i32 | `128 + signal` for a signal kill |
| `duration_ms` | u64 | From spawn to exit, foreground portion included |
| `output_path` | string | Where the command's full stdout and stderr were written, relative to the [capsule workdir](workdir.md): always `logs/<work_id>.log` |
| `output_bytes` | u64 | Size of that file. `0` when it could not be written |
| `resource_limit` | string | As on `shell`, and omitted on the same terms |
| `status` | string | `"ok"` \| `"error"`. `"error"` for a non-zero exit, a signal kill, an attributed `resource_limit`, or a wait that itself failed |
| `completion_task_id` | string | The `task_id` of the `completion`-origin task this result was enqueued as, so a reader can join a command to the task that reported it |

**`shell_abandoned`** — written once per demoted command still running when the session ends

| Field | Type | Notes |
|---|---|---|
| `work_id` | string | The `shell_detached` line's `work_id` |
| `binary` | string | As on `shell` |
| `command` | string | As on `shell` |
| `running_ms` | u64 | How long the command had been running when the session gave up on it |

The command's result is lost. The session does not wait for it, and the same line is announced on
the process's stderr.

**`shell_lost`** — written once per demoted command a later `mur run --resume` found with no
`shell_completed` and no `shell_abandoned`, and appended to the `trace.jsonl` of the session that
started it rather than to the resuming session's own

| Field | Type | Notes |
|---|---|---|
| `session_id` | string | The session that started the command, so the line matches the file it is written into |
| `parent_id` | string | That session's `session_start` node. Absent when that record could not be read back |
| `work_id` | string | The `shell_detached` line's `work_id` |
| `binary` | string | As on `shell` |
| `command` | string | As on `shell` |
| `detached_at_ms` | u64 | The `shell_detached` line's own timestamp |
| `reconciled_by_session` | string | The session that found the command unaccounted for and reported it |
| `reconciled_task_id` | string | The `task_id` of the `completion`-origin task that reported it, whose `task_start` carries `source: "detached_lost"` |

An unmatched `shell_detached` means the session was killed outright: the teardown sweep that
writes `shell_abandoned` runs on every clean exit. This line carries no `exit_code`, no `status`,
no `duration_ms`, no `output_path` and no `output_bytes`, because a command whose runtime was
killed produced none of them — including no `logs/<work_id>.log`, which is written from inside the
runtime after the command exits. Its presence is also what keeps a second resume of the same
session from reporting the same work id again.

**`shell_detach_unrecorded`** — written when a command was moved to the background and its own
`shell_detached` line could not be written

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `task_id` | string \| null | The task the command belongs to. `null` when no task is in scope |
| `work_id` | string | The work id of the command that was moved to the background |
| `binary` | string | As on `shell` |
| `reason` | string | The write error, as the operating system reported it |

The demotion stands: the command keeps running and the turn keeps its handle. This record is
attempted into the file whose write just failed, so it is usually absent and the failure reaches
stderr instead. Either way the command has no `shell_detached` line, so a later resume finds
nothing to report about it.

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

**`context_seed`**{ #context-seed } — written once per task whose `on-task-start` hook returned
`seed-context`, recording what the runtime did with it

| Field | Type | Notes |
|---|---|---|
| `task_id` | string \| null | The task the seed was proposed for. `null` when no task is in scope |
| `hook_name` | string | Manifest name of the hook that returned the `seed-context` |
| `tokens` | u64 | Tokens actually committed to the head of the context. `0` on a rejection |
| `proposed_tokens` | u64 | Tokens the hook returned, before any trim or summarization |
| `budget_tokens` | u64 | The ceiling in force: `context.max_tokens` × `context.seed_budget`, rounded down. `0` when the capsule declares no `context.max_tokens` |
| `outcome` | string | What the runtime did — see below |
| `reason` | string | Why nothing was committed. Present on `"rejected"` only; absent otherwise |
| `message_ids` | list of string | The `msg_`-prefixed id of every committed message, in the order they were placed. Empty on a rejection. The same ids appear on the `inference` line of each request that carried these messages, and on their lines in the [conversation record](workdir.md#the-conversation-record); none of them ever reaches the driver |

| `outcome` | Meaning |
|---|---|
| `"seeded"` | The whole proposal fit the budget and was committed as-is |
| `"trimmed"` | The proposal was over budget; its oldest messages were dropped from the front until the rest fit |
| `"compacted"` | The overflowing front was summarized by the compaction hook, and that summary became the seed's first message. No `compaction` line is written — nothing about the session's own context was compacted |
| `"rejected"` | Nothing was committed |

| `reason` | Meaning |
|---|---|
| `"message_over_budget"` | One message alone was wider than the whole budget, so no trim could fit it |
| `"overflow_over_limit"` | The proposal overflowed the budget by more than three times the budget |
| `"no_budget"` | The capsule declares no `context.max_tokens`, so there is no ceiling to enforce |
| `"unsupported_transport"` | The session runs `inference.transport: process`, which owns its own context |

A rejection never fails the task: the seed is dropped, the task runs without it, and a
`hook_dispatch_error` with `arm: "seed-rejected"` is written alongside naming the same hook. Every
outcome is also written to `workdir/logs/bootstrap.log`. A capsule with no seeding hook, or one
whose bound hook returned `none`, writes no `context_seed` line at all.

**`session_end`** — written once per launch, after the `on-session-end` hooks fire and the task
loop has exited, on every exit path

| Field | Type | Notes |
|---|---|---|
| `total_turns` | u32 | Equals the count of `inference` lines |
| `total_input_tokens` | u64 | |
| `total_output_tokens` | u64 | |
| `total_tool_calls` | u32 | Equals the count of `tool_call` lines |
| `total_shell_calls` | u32 | Equals the count of `shell` plus `shell_detached` lines |
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
| `trust` | string | `"trusted"` \| `"untrusted"` — the class the sending runtime stamped on `x-murmur-task-trust`, which is the class the sending capsule's own task ran under. The receiving capsule records the same value as `task_start.trust` |

**`task_start`** — written at the start of each task, before the agent loop runs

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | UUID for this task (runtime-generated for A2A; synthesized for `task.md` path) |
| `context_id` | string | Context UUID for this task |
| `source` | string | Which door the task came through: `"a2a"` for a task from a peer, `"task_md"` for the task.md path, `"detached_shell"` for a completion the runtime enqueued for itself when a [demoted shell command](manifest.md#lifecycle-shell-grace-secs) finished, `"detached_lost"` for the report a resume enqueues about demoted commands the session it resumes never accounted for, `"delegation_deadline"` for the report the runtime enqueues when a delegation reaches [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), `"delegation_late"` for the outcome of a released sub-capsule that ended after that deadline was reported |
| `origin` | string | `"user"` \| `"peer"` \| `"schedule"` \| `"event"` \| `"completion"` \| `"system"` — why the capsule woke. `"task_md"` tasks are `"user"`; an A2A task is whatever the peer door derived from the request headers. See [Task origin and trust class](../concepts/access-control.md#task-origin-and-trust-class) |
| `trust` | string | `"trusted"` \| `"untrusted"` — derived from `origin` and, for `"peer"` and `"completion"`, from the sending capsule's own class. Never taken from a value a capsule component supplied |
| `lane` | string | `"user"` \| `"peer"` \| `"bg"` — the queue lane the task waited in, derived from `origin`. See [Queue lanes](../concepts/session-loop.md#queue-lanes) for the mapping |
| `delegation_id` | string | `dlg_…` — the delegation this task reports on, for a `"completion"`-origin task from a sub-capsule this session launched or from its own deadline. Written only then; the field is absent from every other line rather than written as `null`. It is the value that joins a completion to the delegation that produced it, and it is the id the child's own `completion.json` carries. See [The completion path](roost-api.md#the-completion-path) |
| `message_parts_bytes` | u64 | Byte length of the task message text |

Resets all per-task counters. Follows `a2a_task_received` for A2A tasks; is the first event for
`task.md` tasks. A `"detached_shell"` task follows the `shell_completed` line that enqueued it and
has no `a2a_task_received` line, having never crossed the peer door. A `"detached_lost"` task
names every lost work id in one message, and joins to the `shell_lost` lines in the resumed-from
session's trace through `reconciled_task_id`. A `"delegation_deadline"` task follows the
`delegation` line with `outcome: "timed_out"` that enqueued it; a `"delegation_late"` task follows
its own `delegation_late` line.

**`task_end`** — written after the agent loop returns and any hook-requested reopens are resolved,
for every task, on every exit path

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Matches the corresponding `task_start` |
| `exit_status` | string | `"ok"` if the last attempt succeeded; `"failed"` if it did not; `"max_turns_reached"` if it spent the `inference.max_turns` budget without finishing; `"reopen_budget_exhausted"` if an `on-task-end` hook still wanted to reopen the task after `lifecycle.max_task_reopens` (or the `inference.max_turns` ceiling) was reached |
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

**`call_denied`**{ #call-denied } — written when a [policy hook](../concepts/hooks.md#policy-hooks)
refuses a shell command or tool call before it runs

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | The turn the refused call was requested in |
| `event` | string | `"on-shell"` \| `"on-tool-call"` — the gated lifecycle function whose decision point refused |
| `hook_name` | string | Manifest name of the policy hook that refused |
| `target` | string | What was refused: the resolved executable path for a shell call, the tool name otherwise |
| `reason` | string | The hook's own reason, or the runtime's when the hook returned none it could use — a crash, a deadline, an unsupported arm, an empty reason |

No `tool_call` or `shell` event accompanies it: the call did not run, so there is nothing to
record about a run. A refusal is not a session failure and the turn continues. An unsupported arm
returned at the decision point produces a `hook_dispatch_error` alongside this line.

**`protected_path_denied`**{ #protected-path-denied } — written when the capsule manifest's
[`capabilities.filesystem.read_only`](manifest.md#read-only-paths) refuses a shell command or tool
call before it runs

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | The turn the refused call was requested in |
| `call` | string | `"shell"` \| `"tool"` — which dispatch path was refused |
| `target` | string | What was refused: the resolved executable path for a shell call, the tool name otherwise |
| `path` | string | The resolved workdir-relative path. Always the resolved form, never the string the model typed, so two spellings of one file produce one comparable record |
| `rule` | string | The `read_only` entry that covers `path`, exactly as the manifest declared it |
| `signal` | string | What identified the call as a write: the redirection operator, the write-target argument position of a named binary, the tool-input key pairing, or the location the tool's own `input_schema` declared a destination (`edits[].path`) |
| `reason` | string | The same sentence the model was given, so the trace and the agent agree on why |

No `tool_call` or `shell` event accompanies it: the call did not run. A refusal is not a session
failure and the turn continues. The manifest is asked before any [policy
hook](../concepts/hooks.md#policy-hooks), so a call refused here produces no `call_denied` line
beside it. `mur trace show` reports the count as `protected-path refusals`.

Distinct from `call_denied` above, which is a *hook's* refusal and names the hook.

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

**`retention`**{ #retention } — written when a [`retain:` policy](manifest.md#retention) deleted
something, once per (`store`, `reason`) pair that removed anything

| Field | Type | Notes |
|---|---|---|
| `store` | string | `"sessions"` for the session directories under the workdir, `"records"` for the conversation records under `~/.murmur/conversations/` |
| `reason` | string | `"max_sessions"`, `"max_age"` or `"max_messages"` — the key that condemned what went |
| `removed` | u32 | Units removed: session directories, context directories, or, for `"max_messages"`, the one record that was rewritten. Never `0` |
| `targets` | array of string | What went: `ses_` directory names for `"sessions"`, context ids for `"records"` |
| `messages_dropped` | u64 | Messages dropped from the front of the record. Written for `"max_messages"` only, and absent otherwise |

Written immediately after `session_start`, in the trace of the session that performed the
deletion. A launch that removed nothing writes no line.

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

**`delegation_start`** — written by the `delegate-task` tool once per launched child, as soon as
that child's process is up and has reported its session id

| Field | Type | Notes |
|---|---|---|
| `delegation_id` | string | `dlg_…`, the id the delegation is named by. Always present: a delegation with no id was never launched and writes no line here |
| `capsule` | string | The sub-capsule the agent named |
| `version` | string | The version the agent named |
| `child_session_id` | string | `ses_…`, the session the child's runtime minted for itself |
| `child_workdir` | string | The child's directory, relative to this capsule's accessible workdir. Join the two, then `.murmur/<child_session_id>/trace.jsonl`, to reach the child's own trace |

Written while the delegation is still running, not when it ends, so a child that hangs, crashes or
is timed out is attributable from the parent's side whatever happens next. A delegation the daemon
refused writes none of these — nothing was launched — and is recorded only by the `delegation` line
below.

**`delegation`** — written by the `delegate-task` tool when a delegation ends, whatever ended it

| Field | Type | Notes |
|---|---|---|
| `capsule` | string | The sub-capsule the agent named |
| `version` | string | The version the agent named |
| `delegation_id` | string \| null | `dlg_…`, the id the delegation is named by. `null` whenever no child was launched: a delegation the daemon refused, or one whose approved child's process never started, was never made |
| `child_session_id` | string \| null | `ses_…`, the child's own session, so its trace is findable. `null` when no child ran |
| `duration_ms` | u64 | From the first request to the daemon until the delegation ended |
| `outcome` | string | `"completed"`, `"failed"`, `"timed_out"` or `"refused"` |
| `reason` | string \| null | `null` on `"completed"`; otherwise the same sentence the model was given |

One line per call. It carries neither the task text nor the child's answer — both are the agent's
own conversation, which the `tool_call` line for the same call already records under the session's
`trace.capture` setting.

`"timed_out"` says the parent stopped waiting, not that the child was stopped: the child is
released and keeps running, and the `delegation_late` line below records what it eventually did.

**`delegation_late`** — written by the task loop when a released child ends after its
[deadline](manifest.md#lifecycle-delegation-deadline-secs) was already reported

| Field | Type | Notes |
|---|---|---|
| `delegation_id` | string \| null | `dlg_…`, the same id the `delegation_start` and `delegation` lines for this delegation carry. `null` for a delegation the launcher could not name |
| `capsule` | string | The sub-capsule the agent named |
| `version` | string | The version the agent named |
| `child_session_id` | string | `ses_…`, the child's own session, so its trace is findable |
| `status` | string | `"ok"`, `"error"`, `"crashed"` or `"terminated"` — how the released child ended. A different vocabulary from `delegation.outcome`, because it describes an ending rather than a wait |
| `duration_ms` | u64 | How long the child ran, from launch to ending |
| `after_deadline_ms` | u64 | How long after the deadline fired the child ended |
| `result_path` | string \| null | The child's result file, relative to this capsule's accessible workdir. `null` when the child wrote none |
| `completion_task_id` | string | The `task_id` of the `completion`-origin task that carried this outcome to the agent |

At most one per delegation, and only for a delegation whose `delegation` line reads `"timed_out"`.
It carries no output: the child's answer stays in the file `result_path` names.

### Reading a formation { #delegation-lineage }

The relationship between a parent and a child is recorded once, from both ends, and joined by the
`dlg_` id:

| From | Read | To reach |
|---|---|---|
| A parent's trace | `delegation_start.child_workdir` and `child_session_id` | `<accessible workdir>/<child_workdir>/.murmur/<child_session_id>/trace.jsonl` |
| A child's trace | `session_start.spawned_by` | The `ses_` id of the session that spawned it |
| A parent's trace | `delegation.delegation_id` | The `task_start` with `source: "delegation_deadline"` and the same `delegation_id`, for a delegation that reached its deadline |
| A parent's trace | `delegation_late.completion_task_id` | The `task_start` that carried the released child's outcome to the agent |

[`mur trace show`](cli.md#mur-trace-show) renders both ends within the one file it is given: a child's
header names the session that spawned it and the delegation that created it, and a parent grows a
Delegations section listing each delegation, the child session it launched, how it ended and why.
No command walks a formation across files.

**A resumed child's lineage is one hop back.** `spawned_by` is written at spawn and never
rewritten, so resuming a *parent* keeps the child reachable: the resumed session's `resumed_from`
names the session the child's `spawned_by` names. Resuming a *child* is the other direction and the
window is open — that resume is an operator launch with no `MURMUR_SPAWNER` in its environment, so
the new session writes no `spawned_by` at all, and its `resumed_from` names the child session that
was spawned. The lineage is in the session it continues, one `resumed_from` hop back.

**The handle itself never appears in a trace, on either side.** Where a token would otherwise reach
one — most obviously as the recorded `handle` argument of a `fetch-peer-file` `tool_call` — it is
replaced with `<handle:<handle_id>>`.

**`plan_start`** — written once by [`mur run`](cli.md#mur-run)'s plan scheduler, as soon as the
plan file parses

| Field | Type | Notes |
|---|---|---|
| `plan_id` | string | The plan's authored `id` |
| `step_count` | usize | How many steps the plan declares |
| `steps` | array of object | The DAG as authored, one entry per step in file order: `step_id`, `kind` (`"tool"`, `"shell"` or `"capsule"`; `"unknown"` for a step declaring none or several, which the scheduler refuses), `depends_on`, and `has_condition` — whether the step carries an `if` and so may settle without ever being dispatched |

Written before the plan is validated, so a plan the scheduler refuses still records the shape it
was refused for. The structure is recorded once, up front, which is what keeps a run legible for a
step that never ran.

**`plan_step_start`** — written once per step the scheduler dispatches, as it hands the step to a
worker

| Field | Type | Notes |
|---|---|---|
| `plan_id` | string | The run this step belongs to |
| `step_id` | string | The step's authored id |
| `kind` | string | `"tool"`, `"shell"` or `"capsule"` |
| `depends_on` | string[] | The steps this one waited on. `[]` when it waited on none |

A step that settled without being dispatched — an `if` that evaluated false, a dependency that
never resolved, a plan the validator refused — writes none of these, only its terminal
`plan_step`. Joins to that line on `(plan_id, step_id)`.

**`plan_step`** — written once per settled step, after the step's `on_error` policy has been
applied

| Field | Type | Notes |
|---|---|---|
| `plan_id` | string | The run this step belongs to |
| `step_id` | string | The step's authored id |
| `kind` | string | `"tool"`, `"shell"` or `"capsule"`; `"unknown"` for a step whose dispatch thread died and named nothing the plan declared |
| `status` | string | `"success"`, `"failed"` or `"skipped"` — the status the run's own report carries for this step. A step that failed under `on_error: skip` reads `"skipped"` here, because that is what the report settled it as |
| `attempts` | u32 | How many times the step was dispatched, `retries` included. `0` for a step that settled without dispatch |
| `duration_ms` | u64 | Wall-clock time across every attempt. `0` for a step that settled without dispatch |
| `error` | string | The step's own error text. Absent when there is none, including on a step demoted to `"skipped"` by a policy that carried no text |
| `input` | object | The interpolated step input, with peer handle tokens redacted. Written for a tool step only |
| `state_effect` | string | `"read"` \| `"mutate"`, as the tool declared it. Absent when the tool declared none. Feeds the same redundant-call analysis `tool_call.state_effect` does, against the same resource history — a plan step that re-reads what an agent turn already read is flagged, and the other way round |
| `resource_id` | string | The resource this step addressed, as the tool declared it. An opaque, tool-defined string. Absent when the tool declared none. Read on the same terms as `tool_call.resource_id`, falling back to a path sniffed out of `input` |

Only a step that succeeded takes part in the redundancy analysis: a step that failed or was
skipped observed nothing.

**`plan_end`** — written once as the run returns, whatever ended it

| Field | Type | Notes |
|---|---|---|
| `plan_id` | string | The plan's authored `id`. The empty string for a plan file that never parsed |
| `outcome` | string | `"completed"` or `"failed"` |
| `failed_step` | string | The step that ended the run. `"plan"` when the run failed before any step could — a file that would not parse or validate, a cgroup scope the host refused. Absent on `"completed"` |
| `steps_total` | usize | How many steps the plan declared |
| `steps_succeeded` | usize | |
| `steps_failed` | usize | |
| `steps_skipped` | usize | |
| `duration_ms` | u64 | Wall-clock time for the whole run, the plan file read included |
| `reason` | string | Why the run ended when the reason was not a step's own failure. Absent otherwise |

The three counts cover the steps that settled, and sum to less than `steps_total` on a run that
stopped early.

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
  The exceptions are `compaction`, `compaction_declined` and `context_seed`: that failure is
  logged to `workdir/logs/bootstrap.log` and the session continues.
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
| `capsule.session` | One per task | `exit_status` |
| `capsule.inference` | `inference` | `turn`, `input_tokens`, `output_tokens`, `decision`, `tool_name` (when the response asked for one), `input_tokens_actual`, `output_tokens_actual`, `cached_tokens` and `cache_write_tokens` (each when the driver reported it), plus `origin` and `model` for a hook-run completion |
| `capsule.tool_call` | `tool_call` | `tool_name`, `input_bytes`, `output_bytes`, `duration_ms`, `status` |
| `capsule.shell` | `shell` | `command` (first 200 characters), `exit_code`, `duration_ms` |
| `capsule.compaction` | `compaction` | `tokens_before`, `tokens_after` |

Every span carries two resource attributes: `service.name` (the capsule name) and
`service.version` (the manifest `version`). The `skill_call`, task and A2A events have no span of
their own — they appear in `trace.jsonl` alone.

A `capsule.session` span covers one task, under its own trace id. A launch that handles three
queued tasks therefore posts three of them, where `trace.jsonl` holds a single
`session_start`/`session_end` pair around three `task_start`/`task_end` pairs. Correlate the two
by task, not by session.

**Non-obvious behaviour:**

- Each span is POSTed as its event happens, over a connection the agent loop waits on, so a slow
  endpoint slows the session down.
- `trace.jsonl` is written whether or not `observability.otel_endpoint` is set, and whether or not
  the endpoint is reachable.
- The `MURMUR_FORMATION_ID` host environment variable, when set, is forwarded into every hook's
  WASI environment and added as `murmur.formation_id` to the root span by `murmur-hook-grafana`.
