# Observability Schemas

Every session writes a structured record of what it did, and streams what it is doing to any
client watching it. This page documents the file formats, the OpenTelemetry span tree they map
onto, and the task stream's `artifact` frame.

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
| `task_end`, `task_reopened`, `task_continued`, `task_canceled`, `task_failed`, `context_seed` | The task node, or the session node for a `task_failed` written outside any task |
| `inference` (agent loop's own) | The task node, or the session node between tasks. Its `event_id` is the turn node — a turn has no line of its own |
| `inference` (a hook's, carrying `origin`), `tool_call`, `skill_call`, `shell`, `shell_detached`, `shell_detach_unrecorded`, `compaction`, `compaction_declined` | The turn node, falling back to the task node and then the session node |
| `call_denied`, `protected_path_denied`, `tool_input_refused`, `spend_ceiling_reached` | The turn node, falling back to the task node and then the session node |
| `harness_start`, `harness_warning`, `harness_session`, `harness_session_forgotten`, `harness_retry`, `harness_note`, `harness_failed`, `harness_interrupt`, `harness_exit` | The task node |
| `session_end`, `a2a_task_received`, `a2a_send`, `artifact_pulled`, `hook_dispatch_error`, `retention`, `task_rejected`, `formation_ended`, `spawner_ended` | The session node |
| `inference_credential`, `gateway_credential` | The session node — written as the keyed request is sent, outside any turn |
| `control_change`, `control_refused` | The session node — written as the control surface answers, outside any turn |
| `control_applied`, `tools_refreshed` | The task node, or the session node between tasks. Written just before the turn's own `inference` line |
| `shell_completed`, `shell_abandoned` | The session node — by the time either lands, the turn that started the command is over |
| `shell_lost` | The `session_start` node of the session named in `session_id`, which is the session that started the command and not the one that wrote the line |
| `resource_list`, `resource_read`, `peer_handle_mint`, `peer_handle_redeem`, `peer_file_fetch`, `delegation_start`, `delegation`, `member_call_busy`, `member_call_start`, `member_call` | The session node |
| `plan_start` | The session node. Its `event_id` is the plan node |
| `plan_step_start`, `plan_step` | The plan node |
| `plan_end` | The plan node, or the session node for a plan file that never parsed and so has none |

A trace with no `session_start` line — a script capsule flushing buffered `artifact_pulled` and
`a2a_send` records into a file that has no session frame — writes `parent_id: null` on every line,
rather than naming a parent that has no line behind it.

**`session_start`** — written once per launch, before the `on-session-start` hooks fire and
before the first task begins

| Field | Type | Notes |
|---|---|---|
| `capsule_name` | string | Manifest `name` |
| `capsule_version` | string | Manifest `version` |
| `model` | string | `inference.model` |
| `max_turns` | u32 | `inference.max_turns` — the turn ceiling each task of this launch runs under |
| `max_session_tokens` | u64 \| null | [`inference.max_session_tokens`](manifest.md#inference-max-session-tokens). Always present; `null` when no session ceiling applies |
| `machine_tokens_per_day` | u64 \| null | [`spend.machine_tokens_per_day`](config.md#spend) when this session is counted against it. Always present; `null` when no machine ceiling was in effect, and under `transport: process` |
| `capabilities` | string[] | The capability categories the manifest granted anything under: `"network"`, `"filesystem"`, `"shell"` |
| `tools_declared` | string[] | Names of the tools offered to the model |
| `tool_refresh` | string \| null | [`inference.tool_refresh`](manifest.md#inference-tool-refresh): `"compaction"` \| `"immediate"` under `transport: http`, `null` under `transport: process`. Always written |
| `runtime_artifacts` | array of object | Every staged artifact whose [`murmur.lock` pin](workdir.md#lock-origin) a running capsule fetched, in staging order. One object per artifact: `name`, `version`, `origin` (always `"runtime"`), `session` (the session whose `manage.pull()` wrote the pin) and `trust` (always `"untrusted"`). Each reached the model marked — see [Artifact origin](../concepts/access-control.md#artifact-origin). Always written; `[]` when every staged artifact is operator-pinned |
| `containment_declared` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the strongest class the manifest, workspace config or `--containment` asked for. Always present; `"advisory"` when none of them declared one |
| `containment_achieved` | string | `"advisory"` \| `"scoped"` \| `"sealed"` — the class this host can enforce, capped by `workdir_exec`. Nothing in a manifest can raise it. See [Containment](containment.md) |
| `userns_grant` | string \| null | Where this host's permission to create an unprivileged user namespace came from: `"apparmor_absent"`, `"restriction_disabled_host_wide"`, `"profile_confining"` or `"withheld"`. Always written; `null` only off Linux, where AppArmor does not exist. Two sessions can reach the same `containment_achieved` through different permissions, so read this alongside it. See [`W-SEC-013`](diagnostics.md#w-sec-013) |
| `workdir_exec` | bool | `capabilities.filesystem.workdir_exec`, always written. `true` means the session workdir kept its `Execute` right, so `capabilities.shell.allow` was advisory inside it — and it is why `containment_achieved` can read `"advisory"` on a Landlock-capable host. See [`W-SEC-011`](diagnostics.md#w-sec-011) |
| `resumed_from` | string \| null | The session [`mur run --resume`](cli.md#mur-run) continued, verbatim as the address resolved it. `null` on an ordinary launch. Always written, so its absence identifies a trace from a runtime predating the field |
| `context_id` | string \| null | The context id every task of this launch runs under: the `mur run --context` value, or the id `--resume` resolved to. `null` when each task mints its own — `task_start.context_id` carries the id a task actually ran under either way. Always written, on the same terms as `resumed_from` |
| `control` | object | What may be changed on this session: `settings` (the names `control.settings` lists), `secrets` (the names `control.secrets` lists) and `agent_settings` (the names `control.agent_settings` lists; absent when it lists none). Written only when the manifest declares [`control:`](manifest.md#field-control); absent otherwise. Never a value — see [Control surface](control-surface.md) |
| `inference_choices` | array of object | Every [driver choice](manifest.md#inference-alternates), `primary` first, one object each: `name`, `driver`, `model`, `credential_source` (as on `gateways`, or `"none"` for a credential found nowhere at launch) and `available` (whether that credential resolved at launch; an injected one is `true` here and checked again at each switch). Written only when `inference.alternates` is declared; absent otherwise |
| `spawned_by` | string | `ses_…` — the session that spawned this one, for a capsule another capsule launched with [`delegate-task`](runtime-provided-tools.md) or with a plan's [`capsule` step](plans.md). Written only then; the field is absent from every other line rather than written as `null`, so a capsule nobody delegated produces a byte-identical record |
| `delegation_id` | string | `dlg_…` — the delegation that created this session, character-identical to the id on the spawning session's own `delegation_start`. Present exactly when `spawned_by` is |
| `formation_id` | string | `frm_…` — the [formation](cli.md#mur-run-formation) this session is a member of. Written only for a member; absent rather than `null` otherwise, so a session in no formation produces a byte-identical record. `session_start` is always the trace's first line, so this is too. See [Reading a formation](#formation) |
| `formation_member` | string | This session's roster name. Written only for a session `mur run --roster` launched as a member, which is handed its credentials on a [formation channel](roster.md#formation-channel); absent otherwise, a member's delegated child included |
| `formation_callees` | list of strings | The members this one may call, in roster order: `[]` for a member that may call nobody. Present exactly when `formation_member` is. See [How reachability is enforced](roster.md#enforcement) |
| `system_prompt_source` | string | `"manifest"` \| `"cli"` \| `"none"` — where the system prompt in effect came from. `"cli"` whenever [`mur run --system-prompt`](cli.md#mur-run) was passed, including when its value was empty and therefore cleared the prompt. Always written, so its absence identifies a trace from a runtime predating the field rather than a session with no prompt |
| `credential_source` | string | `"config"` \| `"environment"` \| `"manifest"` \| `"keyless"` \| `"none"` — where the inference key this session attaches came from: [`credentials.<NAME>`](config.md#credentials) in the global config, the launching environment, a literal `gateway.api_key` on the driver's entry, a driver entry with [`keyless: true`](manifest.md#gateway-keyless), or no inference gateway at all. Never the key, a hash of it, its length or any part of it |
| `gateways` | array of object | Every [credential gateway](manifest.md#artifact-gateway) the session holds, the configured driver's first and the rest by artifact name. One object per gateway: `artifact` (the entry's name), `host` (the host of `gateway.endpoint`, with its port when one was written), `credential_source` (`"config"` \| `"environment"` \| `"manifest"` \| `"keyless"` as `credential_source` above, or `"injected"` for a name in [`control.secrets`](manifest.md#field-control), supplied by a controller and held in memory only) and `metered` (`true` only for the gateways of the configured `transport: http` driver and of each `inference.alternates` driver, whose calls count toward the spend ceilings). Always written; `[]` when no entry declares `gateway:`. Never a key |
| `system_prompt_sha256` | string \| null | SHA-256 (lowercase hex) of the prompt as resolved — the manifest's or the override's own text, before the runtime prepends its `[Capsule]` identity block. `null` when no prompt was in effect. Always written, so two sessions can be compared for prompt equality without either trace carrying the prompt itself. Under [`trace.capture: content`](manifest.md#field-trace) those bytes are also stored as `blobs/<system_prompt_sha256>`. Deliberately a different value from `inference.system_sha`, which covers the augmented prompt that went on the wire |
| `effective_grants` | object | The complete grant set this session ran under — the same object [`mur run --explain-scope --json`](../how-to/different-ways-to-run-murmur.md#step-5-inspect-the-capsules-reach-before-launching-it) prints for the same manifest on the same host: `declared_containment`, `achieved_containment`, `floor_met`, `shortfall_reason` (present only when `floor_met` is `false`), `enforcement_tier`, `userns_grant`, `filesystem_scope`, `workdir_exec`, `read_only_paths` (the subtrees [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) protects; `[]` when the manifest declares none), `read_only_advisory_for` (the entries of `shell_allow` that protection is only advisory against; `[]` when it is enforced for every call the runtime can read as a write), `network_allow`, `unix_sockets`, `shell_allow`, `spawn_allow`, `env_allow`, `install_skill` and `install_tool` (the entries of [`capabilities.install`](manifest.md#field-install); `[]` when the manifest declares none), `interpreter_runtime_grants`, `staged_runtime_grants`, `preopens` (one entry per `runtime: tool`, `runtime: driver` and `runtime: hook` entry — `artifact`, `role`, the declared `scope` or `null`, and a `surface` of `whole-workdir`, `scoped-subtree` or `nothing`; `[]` when the capsule declares only skills), `state_stores` (`[]` when no artifact declares [`capabilities.state`](manifest.md#field-capabilities)), `configured_artifacts` (`[]` when no artifact declares [`config:`](manifest.md#artifact-config)), `exports_files` (`null` when the manifest declares no [`exports.files`](manifest.md#field-exports)), `peer_files` (`null` when the manifest declares no [`exports.peer_files`](manifest.md#field-exports-peer-files)), `peer_tasks` (`true` only when the manifest declares [`exports.peer_tasks.accept: true`](manifest.md#field-exports-peer-tasks); `false` otherwise), `peer_fetch_allow` (`[]` when the manifest declares no [`capabilities.peer_fetch`](manifest.md#field-peer-fetch)), `runtime_writes`, `filesystem_boundary` (always present; a `restriction` of `advisory`, `enforced` or `absent` naming the filesystem mechanism this session installs rather than the class this host can back, and `not_protected`, the statements `mur run --explain-scope` prints under `Not protected here` — `[]` at `absent`, and two statements otherwise, one about the filesystem and one about the `HOME` rewrite; see [Testing containment honestly](containment.md#testing-containment)) and `io_max` (always present; `declared_bytes_per_sec`, a `status` of `enforced`, `unavailable`, `not-required` or `not-probed`, and a `reason` absent only when the status is `enforced` — see [Whether the I/O ceiling applied](resource-limits.md#io-max-report)). Where `capabilities` above names categories, this names the actual destinations, binaries, capsule names and paths |
| `effective_grants.runtime_writes` | array of object | Every path the runtime itself writes inside the accessible workdir, so a consumer for whom that workdir is the deliverable can subtract them and be left with what the capsule changed. One object per path, with `path`, `kind` (`"file"` \| `"directory"`), `scope` (`"accessible"` \| `"session"`) and `condition` (`"always"`, `"workdir-provided"`, `"agent-session"`, `"script-session"`, `"shell"`, `"peer-fetch"`, `"spawn"`, `"delegated"` or `"sealed"`). Paths are relative to the accessible workdir and carry the literal segment `<session-id>`, which `session_id` on this same line supplies. The `"sealed"` rows are present exactly when this session composes a sealed root, which needs both a host that reaches the sealed tier and a containment class that asked for it — so a sealed-capable host running an `advisory` capsule reports the sealed `enforcement_tier` with no `"sealed"` rows. Every other condition names when the path appears rather than deciding whether the row is listed. See [Session workdir](workdir.md) |

**`inference`**{ #inference } — written after each driver call, including one that failed

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based turn index, counted across every attempt of the task: a task an `on-task-end` hook reopened, or that continued with the outcomes of handed-off work, carries on from its last turn. Starts again at 0 for each task. A hook's `run-inference` record carries the number of the turn it ran inside |
| `task_id` | string \| null | The task this turn belongs to. `null` when no task is in scope |
| `input_tokens` | u64 | What this turn's input cost, and the number the task and session totals accumulate. Under `transport: http` the runtime's own tiktoken (`cl100k_base`) estimate of the request, counted before it was sent, and the number the compaction threshold runs on; under `transport: process` the count the harness reported and its driver relayed. Absent when nothing counted the turn: a process driver reporting no usage, or a [failed call](#inference-failed) that is not counted — never the same fact as `0` |
| `output_tokens` | u64 | What this turn's output cost, on the same terms as `input_tokens` |
| `decision` | string | `"tool_call"` \| `"end_turn"` \| `"text"` \| `"error"` — what the loop does next. A turn the provider cut off at the output cap reads `"text"`; `stop_reason` beside it is the field that says it was cut off. `"error"` is a [failed call](#inference-failed) |
| `stop_reason` | string | The provider's own stop reason, verbatim as the loop dispatched on it — `"max_tokens"` for a turn stopped at [`inference.max_tokens`](manifest.md#inference-max-tokens). Written on every agent-loop turn, and as `""` when the driver reported none. `"error"` on every failed call, on both transports and for a hook's `run-inference`. Absent on any other record no driver response was parsed for: a hook's successful `run-inference` and a `process` turn that ended |
| `tool_name` | string \| null | The tool the response asked for; `null` when it asked for none |
| `input_tokens_actual` | u64 | The provider's own count of the request, from the driver's [`usage`](wit-interfaces.md#driver-usage) block. `transport: http` only: on `process` the harness's own count is already `input_tokens`, and writing it twice would invent a second measurement |
| `output_tokens_actual` | u64 | The provider's own count of the completion, on the same terms |
| `cached_tokens` | u64 | Request tokens the provider served from its prompt cache. Beside `input_tokens`, never inside it |
| `cache_write_tokens` | u64 | Request tokens the provider wrote into its prompt cache. Beside `input_tokens`, never inside it |
| `thinking_tokens` | u64 | The part of the output reported as reasoning — a subset of `output_tokens`, never an addition to it |
| `origin` | string | `hook:<hook name>` when a hook produced this completion through [`run-inference`](wit-interfaces.md#murmurruntimeinference). Absent for an ordinary agent-loop turn |
| `driver_choice` | string | The [driver choice](manifest.md#inference-alternates) that served an agent-loop turn, by name. Written only when `inference.alternates` is declared |
| `model` | string | The model this call was sent to. Written alongside `origin`, and alongside `driver_choice` |
| `message_ids` | array of string | Ids of the messages this request embedded, in the order they sat in it. Under an active [driver continuation](wit-interfaces.md#stateful-driver-continuation) only the tail the driver has not seen is sent, and this names exactly that tail. Absent when the list is empty: a hook's own completion and the `process` transport both send a message list the runtime never minted |
| `system_sha` | string | SHA-256 (lowercase hex) of this request's `system` string — the resolved prompt with the `[Capsule]` identity block already prepended |
| `tools_sha` | string | SHA-256 (lowercase hex) of this request's serialized `tools` array |
| `response_sha` | string | SHA-256 (lowercase hex) of the raw driver response body, as the runtime read it before parsing |
| `message_shas` | array of string | SHA-256 (lowercase hex) of each message this request embedded, in send order — one entry per `message_ids` entry, over the same messages once the runtime's own identity keys are stripped |
| `error_code` | string | Why the call failed — one of the [`error_code` values](#inference-error-code). Present exactly when `decision` is `"error"` |
| `error` | string | The failure in words, present exactly when `error_code` is. Every credential value the session holds is replaced with `[redacted]`, and the text is capped at 2,000 bytes. For `credential_rejected` it is the runtime's own `E-RUN-027` message, never the provider's reply |
| `provider_status` | u16 | The HTTP status the runtime's credential gateway received for the failed call. Absent when the request reached no provider, on the `process` transport, and for a driver with no `gateway:` |

The five provider-reported fields are written only when the driver reported that member, and are
absent otherwise — never `0`. See [Reported token usage](wit-interfaces.md#driver-usage) for what a
driver sends and what the runtime does with it.

Which of them appear depends on the transport, because the two transports make a different number
of measurements:

| Transport | `input_tokens` / `output_tokens` | `*_actual` | `cached_tokens`, `cache_write_tokens`, `thinking_tokens` |
|---|---|---|---|
| `http` | The runtime's own estimate, always present | The provider's own counts, beside the estimate so drift is a subtraction on one line | Present when the driver reported them |
| `process` | The harness's own reported counts, absent when its driver reports none | Always absent: there is one count on this transport and it is recorded once | Present when the driver reported them |

#### Failed calls { #inference-failed }

A model call that fails writes one `inference` record with `decision` and `stop_reason` `"error"`,
`error_code`, `error` and, when the gateway saw one, `provider_status`. It never carries
`system_sha`, `tools_sha`, `response_sha` or `message_shas`, and stores no blob, at any
`trace.capture`. Its provider counts appear only when the driver's error payload carried a `usage`
block. Whether it counts as a turn depends on where it failed:

| Failed call | `input_tokens` / `output_tokens` | Counted in `task_end.turns`, `session_end.total_turns` and the token totals |
|---|---|---|
| Agent loop, `transport: http` | Absent | No |
| `transport: process`, with a turn open when the harness failed | The harness's own counts, as for any turn | Yes |
| `transport: process`, with no turn open | Absent | No |
| A hook's `run-inference` | `input_tokens` only: the runtime's count of the request it sent | Yes, as every hook record is |

A call that returned an answer the loop could not act on — `stop_reason: "tool_call"` with no tool
call, or an unsupported stop reason — succeeded, and is recorded with the provider's own
`stop_reason` before the [`task_failed`](#task-failed) that ends the attempt.

##### `error_code` { #inference-error-code }

| Value | Written when |
|---|---|
| `credential_rejected` | The provider answered `401` and the key, re-read and resent, was refused too |
| `driver_error` | The driver reported the call failed: a provider HTTP error, the driver's own error, a request the runtime refused before sending it, or a response whose `stop_reason` is `"error"` |
| `driver_failed` | The driver component could not be run, or stopped partway: it failed to load, hit `capabilities.limits.deadline_seconds`, or exceeded its memory limit |
| `malformed_response` | The driver reported success with no response body, or a body that is not JSON |
| `harness_auth` | A `process` harness reported a `turn-failed` of kind `auth` |
| `harness_quota` | A `process` harness reported a `turn-failed` of kind `quota` |
| `harness_error` | A `process` harness reported a `turn-failed` of kind `harness-error` |
| `harness_other` | A `process` harness reported a `turn-failed` of kind `other` |
| `harness_inactive` | A `process` harness produced no output for the inactivity limit and was stopped ([`E-RUN-035`](diagnostics.md#e-run-035)) |

A spend ceiling that refuses a call before it is sent, a canceled call, and a `process` harness
failure of kind `max-turns` or `canceled` write no failed record.

### `inference_credential` { #inference-credential }

Written when the configured `transport: http` driver's credential is rotated, rejected or cannot
be read, at the moment it happens.

| Field | Type | Notes |
|---|---|---|
| `source` | string | `"config"` \| `"environment"` \| `"manifest"` — as `session_start.credential_source` |
| `credential` | string | The credential name the driver entry's `gateway.api_key: ${NAME}` referenced. Absent for a manifest literal |
| `change` | string | `"rotated"` — a re-read found a different key; `"rejected"` — the provider answered `401` and the rejection stood; `"unreadable"` — the config file or its entry could not supply a key, and the last key read stays in use |
| `trigger` | string | `"file_changed"` — the config file held a different key when a request read it; `"rejection"` — the re-read after a `401`. Only with `change: "rotated"` |
| `status` | u16 | The provider's status, `401`. Only with `change: "rejected"` |
| `retried` | bool | `true` when the rejected request was the one resend made with a re-read key. Only with `change: "rejected"` |
| `reason` | string | `"missing"`, `"unreadable"`, `"unparseable"`, `"no_entry"` or `"empty_entry"`. Only with `change: "unreadable"`, which is written once per state of the file and reason |

No field carries the key, a hash of it, its length or any part of it. See
[Rotating a key](config.md#credentials-rotation).

### `gateway_credential` { #gateway-credential }

Written when the credential of any other artifact's [gateway](manifest.md#artifact-gateway) is
rotated, rejected or cannot be read, at the moment it happens. Its fields are those of
[`inference_credential`](#inference-credential), read against that artifact's `gateway.api_key`
and upstream, plus:

| Field | Type | Notes |
|---|---|---|
| `artifact` | string | The artifact entry whose `gateway.api_key` this is |

A `change: "rejected"` here fails nothing: the `401` went back to the artifact as its response.

For a name in [`control.secrets`](manifest.md#field-control), `source` is `"injected"`, and
`change: "unreadable"` carries `reason: "not_injected"`: a keyed request found no value a controller
had injected, and was refused without being sent. It is written once per period with no value —
before the first injection, and again after a `mur control forget`. A replaced value writes no
`rotated` line; the [`control_change`](#control-events) that set it is the record.

### Control events { #control-events }

Written by the [control surface](control-surface.md) and the agent loop. No field carries a secret
value, a hash of it, its length or any part of it, or the control token.

**`control_change`** — a change was accepted, from a controller or from the agent's
[`switch-driver`](runtime-provided-tools.md#switch-driver) call

| Field | Type | Notes |
|---|---|---|
| `principal` | string | `"controller"` \| `"agent"` |
| `token_id` | string | The first 16 hex characters of the SHA-256 of the token the request carried. Absent when `principal` is `"agent"` |
| `kind` | string | `"setting"` \| `"secret"` |
| `name` | string | The setting or secret, as the manifest spells it |
| `action` | string | `"set"` \| `"forget"` (a secret's `DELETE`) |
| `previous` | number \| string | The setting's value before the change: a number for `inference.max_tokens`, a choice name for `inference.driver`. Settings only |
| `value` | number \| string | The setting's new value, on the same terms. Settings only |
| `applies_from` | string | `"next_inference_call"`. Settings only |
| `replaced` | bool | Whether the secret replaced a value already held. Secret `set` only |

**`control_refused`** — the control surface refused a request, or the runtime refused the agent's
`switch-driver` call

| Field | Type | Notes |
|---|---|---|
| `principal` | string | `"controller"` for every request to the surface, `"agent"` for a `switch-driver` call |
| `status` | u16 | The HTTP status answered, or the one a controller would have been answered for the agent's refusal |
| `reason` | string | `"unauthenticated"`, `"unknown_path"`, `"undeclared_setting"`, `"undeclared_secret"`, `"method_not_allowed"`, `"not_loopback"`, `"body_too_large"`, `"missing_body"`, `"unsupported_media_type"`, `"invalid_value"`, `"undeclared_driver"`, `"credential_unresolvable"`, `"in_use"` or `"truncated_body"` — see [Refusals](control-surface.md#refusals) |
| `kind` | string | `"setting"` \| `"secret"`, when the path named one. Absent on a `401` |
| `name` | string | The setting or secret the path named, declared or not. Absent on a `401` |
| `token_id` | string | As on `control_change`. Absent on a `401` and on the agent's refusal |

A capsule with no `control:` block writes nothing: every request under `/control` is `404`.

**`control_applied`** — the first agent-loop inference call to use a setting value a controller or the agent set

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based, the same number the turn's own `inference` line carries |
| `task_id` | string \| null | The task the turn belongs to |
| `name` | string | The setting |
| `value` | number \| string | The value this call used: a choice name for `inference.driver` |
| `change_id` | string | The `event_id` of the `control_change` that set `value` |

Written only when the value differs from the one the previous call used. Compaction calls and a
hook's `run-inference` never write it.

### `tools_refreshed` { #tools-refreshed }

Written when the tool list an agent-loop inference call sends differs from the one the previous
call sent, because an artifact was installed during the task. It comes just before that call's
`inference` line, whose `tools_sha` is the new list's. See
[`inference.tool_refresh`](manifest.md#inference-tool-refresh).

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based, the same number the call's own `inference` line carries |
| `task_id` | string \| null | The task the turn belongs to |
| `trigger` | string | `"immediate"` — `inference.tool_refresh: immediate` released the install; `"compaction"` — a compaction replaced the context since the previous call, under either value |
| `added` | string[] | Names offered now and not before, sorted |
| `removed` | string[] | Names offered before and not now, sorted |
| `tools` | string[] | Every name the list now offers, in the order it is sent, which is sorted |

An install that leaves the list byte-identical, such as a skill that is the
[`inference.system_prompt_artifact`](manifest.md#inference-system-prompt-artifact), writes nothing.
Never written under `transport: process`.

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
| `output` | string | The tool output text, with peer handle tokens redacted. Carries the [untrusted fence](untrusted-fence.md) the model received it inside. Written only under [`trace.capture: content`](manifest.md#field-trace) |
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
| `output_bytes` | u64 | Byte length of the returned `skill.md` text. An operator-pinned skill's result carries no [fence](untrusted-fence.md); a runtime-pinned one's does |
| `duration_ms` | u64 | |
| `status` | string | `"ok"` \| `"error"` |
| `origin` | string | `"operator"` \| `"runtime"` — the skill's [`murmur.lock` origin](workdir.md#lock-origin). A skill with no lock entry, such as a `source:` skill, is `"operator"`. Always written |
| `trust` | string | `"trusted"` \| `"untrusted"` — derived from `origin`, in the spelling `task_start.trust` uses. `"untrusted"` means the guidance reached the model fenced under `skill:<skill_name>`. Always written |

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

**`shell_abandoned`** — written once per demoted command the session ended without carrying its
result back, whether it was still running when the session ended or finished while it was shutting
down

| Field | Type | Notes |
|---|---|---|
| `work_id` | string | The `shell_detached` line's `work_id` |
| `binary` | string | As on `shell` |
| `command` | string | As on `shell` |
| `running_ms` | u64 | How long the command had been running when the session gave up on it. For one that finished during teardown, its full duration from spawn to exit |
| `exit_code` | i32 | As on `shell_completed`. Written only for a command that finished during teardown; absent for one still running |
| `output_path` | string | As on `shell_completed`. Present exactly when `exit_code` is |
| `output_bytes` | u64 | As on `shell_completed`. Present exactly when `output_path` is |

The last three are omitted rather than written as `null`, so a command still running produces a
line carrying only the first four fields — `null` would read as a known-absent exit code rather
than an unknown one. Their absence means no exit code exists and no `logs/<work_id>.log` was
written or ever will be: that file is written from the command's own runtime thread after the
command exits, and that thread ends with the session.

No task carries the result either way. The session does not wait for the command and does not kill
it, and one grouped report naming every discarded command is written to stderr and to
`logs/bootstrap.log` under the [capsule workdir](workdir.md) — see
[`lifecycle.shell_grace_secs`](manifest.md#lifecycle-shell-grace-secs). A capsule whose `lifecycle`
block cannot receive a completion at all is warned before the run with
[`W-SEC-022`](diagnostics.md#w-sec-022).

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
| `exit_status` | string | `"ok"` \| `"failed"` \| `"max_turns_reached"` \| `"spend_ceiling_reached"` \| `"canceled"` \| `"formation_ended"` — the launch's outcome, which is the status [`mur run`](cli.md#mur-run-status) prints. The first [task that decides the launch](cli.md#mur-run-status) and ended anything but `"ok"` sets it, and no later run replaces it. `"formation_ended"` is a formation member's whose [formation ended](roster.md#launch-stop) while it was running a task, which the wind-down canceled, when no earlier run set anything else |

**`a2a_task_received`** — written when an incoming message reserves the task slot

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Runtime-generated UUID |
| `context_id` | string | Echoed or generated `contextId` |
| `message_id` | string | `messageId` from the incoming A2A Message |
| `traceparent_from_caller` | string \| null | W3C `traceparent` header from the incoming request |
| `caller_member` | string | The roster name of the formation member that called, when the door let the task in on a [formation token](roster.md#formation-token). Absent for every other task |

**`a2a_send`** — written when a capsule component calls `murmur:message/send`

| Field | Type | Notes |
|---|---|---|
| `peer_url` | string | Target capsule URL |
| `message_id` | string | `message-id` from the outgoing Message |
| `task_id` | string | Task ID returned by the peer |
| `context_id` | string | Context ID returned by the peer |
| `traceparent` | string \| null | W3C `traceparent` injected on the outgoing request |
| `trust` | string | `"trusted"` \| `"untrusted"` — the class the sending runtime stamped on `x-murmur-task-trust`, which is the class the sending capsule's own task ran under. The receiving capsule records the same value as `task_start.trust` |

**`artifact_pulled`** — written for each successful [`manage.pull()`](wit-interfaces.md#murmurartifact-managermanage). A refused or failed pull writes nothing

| Field | Type | Notes |
|---|---|---|
| `name` | string | The artifact pulled |
| `version` | string | The version pulled |
| `runtime` | string | `"skill"` \| `"tool"` |
| `origin` | string | `"runtime"` \| `"operator"` — the lock entry's origin after the pull. `"operator"` when the operator had already pinned the same version, which a pull leaves the operator's |
| `session` | string \| null | The session the lock entry names as the puller — this session, unless an earlier pull of the same pin recorded another. `null` when `origin` is `"operator"`. Always written |
| `trust` | string | `"trusted"` \| `"untrusted"`, derived from `origin` |

A script capsule writes these lines, before its buffered `a2a_send` lines, after its `run` returns.

**`task_start`** — written at the start of each task, before the agent loop runs

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | UUID for this task (runtime-generated for A2A; synthesized for `task.md` path) |
| `context_id` | string | Context UUID for this task |
| `source` | string | Which door the task came through: `"a2a"` for a task from a peer, `"task_md"` for the task.md path, `"detached_shell"` for a completion the runtime enqueued for itself when a [demoted shell command](manifest.md#lifecycle-shell-grace-secs) finished, `"detached_lost"` for the report a resume enqueues about demoted commands the session it resumes never accounted for |
| `origin` | string | `"user"` \| `"peer"` \| `"schedule"` \| `"event"` \| `"completion"` \| `"system"` — why the capsule woke. `"task_md"` tasks are `"user"`; an A2A task is whatever the peer door derived from the request headers. See [Task origin and trust class](../concepts/access-control.md#task-origin-and-trust-class) |
| `trust` | string | `"trusted"` \| `"untrusted"` — derived from `origin` and, for `"peer"` and `"completion"`, from the sending capsule's own class. Never taken from a value a capsule component supplied |
| `lane` | string | `"user"` \| `"peer"` \| `"bg"` — the queue lane the task waited in, derived from `origin`. See [Queue lanes](../concepts/session-loop.md#queue-lanes) for the mapping |
| `message_parts_bytes` | u64 | Byte length of the task message text |

Resets all per-task counters. Follows `a2a_task_received` for A2A tasks; is the first event for
`task.md` tasks. A `"detached_shell"` task follows the `shell_completed` line that enqueued it and
has no `a2a_task_received` line, having never crossed the peer door. A `"detached_lost"` task
names every lost work id in one message, and joins to the `shell_lost` lines in the resumed-from
session's trace through `reconciled_task_id`. A delegated sub-capsule's outcome starts no task: it
is delivered into the task that made the delegation, which writes the terminal `delegation` line
before its own `task_end`.

**`task_end`** — written after the agent loop returns and any hook-requested reopens are resolved,
for every task, on every exit path

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Matches the corresponding `task_start` |
| `exit_status` | string | `"ok"` if the last attempt succeeded; `"failed"` if it did not; `"max_turns_reached"` if it spent the `inference.max_turns` budget without finishing; `"spend_ceiling_reached"` if a [spend ceiling](manifest.md#inference-max-session-tokens) refused its next driver call — an agent turn, or a compaction hook's `run-inference` call before the hook returned an error; `"reopen_budget_exhausted"` if an `on-task-end` hook still wanted to reopen the task after `lifecycle.max_task_reopens` (or the `inference.max_turns` ceiling) was reached; `"canceled"` if a person stopped the task with [`tasks/cancel`](../how-to/capsules-a2a-messaging.md#cancelling-a-running-task) |
| `duration_ms` | u64 | Wall-clock time from `task_start` to `task_end`, across every attempt |
| `turns` | u32 | Cumulative inference turns for this task across every attempt (reset at `task_start`) |
| `input_tokens` | u64 | Input tokens for this task only |
| `output_tokens` | u64 | Output tokens for this task only |
| `tool_calls` | u32 | Tool calls for this task only |
| `shell_calls` | u32 | Shell calls for this task only |
| `reopen_count` | u32 | Times an `on-task-end` hook reopened this task before it ended. `0` for a task that ran once (the common case). A reader that finds no `reopen_count` field should default it to `0` |

**`task_canceled`**{ #task-canceled } — written where the agent loop stopped because a person
called [`tasks/cancel`](../how-to/capsules-a2a-messaging.md#cancelling-a-running-task)

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task that was stopped |
| `turn` | u32 | The turn that was in flight, 0-based. Absent for a task cancelled before it ran |
| `phase` | string | `"queued"` \| `"turn"` \| `"inference"` \| `"input"` \| `"delegation"` \| `"member_call"` \| `"harness"` \| `"plan"` — which wait the cancel interrupted. `"delegation"` is a wait on a `delegate-task` delegation: its child coming up, or a finished attempt waiting for its outcome. `"member_call"` is a wait on a [`call-member`](runtime-provided-tools.md#call-member) call: reaching the member's door, or waiting for its answer. `"harness"` is a [`transport: process`](manifest.md#transport-process) run, where the harness itself is interrupted. `"plan"` is a `submit-plan` call running its steps, which stops the plan and ends any `capsule` step's sub-capsule — see [When the task is cancelled](plans.md#when-the-task-is-cancelled) |
| `detached_work_ids` | array of string | Demoted shell commands still running when the loop stopped |
| `delegation_ids` | array of string | Delegations in flight when the loop stopped, named before the task ended them |

Appears at most once per task, before that task's terminal `task_end`. A task cancelled at
`"queued"` never started, so it has no `task_start` and no `task_end` — this is its only record.
A command named in `detached_work_ids` keeps the lifecycle it already had. A delegation named in
`delegation_ids` is then ended by the cancelled task, which closes it with a `delegation` line
(`outcome: terminated`, `reason: the delegating task was cancelled`) before its `task_end`. The
arrays are a snapshot taken where the loop stopped, so they may differ from the `residue` artifact
the `tasks/cancel` response carried, which was taken when that response was sent.

**`task_rejected`**{ #task-rejected } — written once per task the session refused because it
stopped taking work while the task was still queued, as described under
[Tasks queued when the session ends](manifest.md#queued-tasks-at-session-end)

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The refused task |
| `context_id` | string | The task's context id |
| `source` | string | Where the task came from: `"a2a"` \| `"detached_shell"` \| `"detached_lost"`, as on `task_start` |
| `cause` | string | Why the session stopped taking work, from the table below |
| `reason` | string | The `status.message` of the task's final `rejected` [status frame](streaming-protocol.md#one-final-status) |

| `cause` | Written when |
|---|---|
| `session_ended` | The session ended on its own: its task finished under `after_task: exit` or `task_acceptance: single`, its launch task failed, or the [idle timeout](manifest.md#idle-timeout) fired |
| `session_stopped` | [`mur stop`](cli.md#mur-stop) or `SIGTERM` ended the session |

Written when the session stops taking work, before `session_end` and whatever
[`trace.capture`](manifest.md#field-trace) is. A refused task never started, so it has no
`task_start` and no `task_end`, and never a `task_failed` — this is its only record. A task
cancelled while still queued keeps its [`task_canceled`](#task-canceled) line and has no
`task_rejected`.

**`formation_ended`**{ #formation-ended } — written once by a formation member whose
[lifeline](roster.md#launch-stop) closed: its formation ended, and the session is winding down
because of it

| Field | Type | Notes |
|---|---|---|
| `formation_id` | string | The formation that ended, as `session_start.formation_id` names it |

Written at the moment the member sees its lifeline close, before every `task_canceled`, `task_end`,
`task_rejected` and `session_end` its wind-down writes. A member already ending because of
`SIGTERM` or [`mur stop`](cli.md#mur-stop) when its lifeline closes writes none, and neither does a
session in no formation.

```json
{"event_type":"formation_ended","event_id":"evt_0192a5b3c4d97c1e9a3b5d0c4e8f2a61","parent_id":"evt_0192a5b3c4a17b2c8d4e6f0a1b3c5d7e","session_id":"ses_0192a5b3c4a07e6f8a9b0c1d2e3f4a5b","timestamp":1767225600123,"formation_id":"frm_0192a5b3c4d57e6f8a9b0c1d2e3f4a5b"}
```

**`spawner_ended`**{ #spawner-ended } — written once by a delegated child whose
[spawner lifeline](roost-api.md#spawner-lifeline) closed: the process that delegated to it ended,
and the session is winding down because of it

| Field | Type | Notes |
|---|---|---|
| `spawned_by` | string | The `ses_` id of the session that spawned this one, as `session_start.spawned_by` names it. Absent, not null, when the session was launched with no `MURMUR_SPAWNER` |
| `delegation_id` | string | The `dlg_` id of the delegation this session was launched for, as `session_start.delegation_id` names it. Absent under the same condition |

Written at the moment the child sees its lifeline close, before every `task_canceled`, `task_end`,
`task_rejected`, `shell_abandoned` and `session_end` its wind-down writes. A child already ending
because of `SIGTERM` or [`mur stop`](cli.md#mur-stop) writes none.

```json
{"event_type":"spawner_ended","event_id":"evt_01a1058d3a9070319d2783f65aa594f3","parent_id":"evt_01a1058d39ef70d18c55d007dae98732","session_id":"ses_01a1058d39de73f18822af4c5494398f","timestamp":1791094504080,"spawned_by":"ses_01a1058d39217722b5e52c896926dbf4","delegation_id":"dlg_01a1058d39a07992a594e3ff0888c45a"}
```

**`member_call_busy`**{ #member-call-busy } — written once per offer of a
[`call-member`](runtime-provided-tools.md#call-member) call's task that the member's door turned
away busy: it answered `rejected` with the status message `task rejected: capsule is busy`. The
first is written as the tool call returns, each later one as the runtime offers the task again —
see [A busy member](runtime-provided-tools.md#call-member-busy)

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The calling task |
| `call_id` | string | `mcl_` and 32 hex digits, as the tool result names it |
| `member` | string | The called member's roster name |
| `offer` | u32 | Which offer of the call this was, from 1 |
| `waited_ms` | u64 | From the tool call to this refusal |
| `message` | string | The door's status message: always `task rejected: capsule is busy` |

A call's lines are written in order: its `member_call_busy` lines, then `member_call_start` if the
member took the task, then its `member_call`.

```json
{"event_type":"member_call_busy","event_id":"evt_01a106502a33702a9a3b5d0c4e8f2a61","parent_id":"evt_01a10650283b7a82bd5a5b81e6aa7554","session_id":"ses_01a10650282f7610a69952ba1dee5781","timestamp":1791107280431,"task_id":"tsk_01a10650283e7881b7a192a438359df9","call_id":"mcl_01a106502a3f7d4189618000400406c5","member":"reviewer","offer":2,"waited_ms":1187,"message":"task rejected: capsule is busy"}
```

**`member_call_start`**{ #member-call-start } — written once per
[`call-member`](runtime-provided-tools.md#call-member) call whose member holds the task: as the
tool call returns, or, for a member that was busy, when it takes the task on a later offer

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The calling task |
| `call_id` | string | `mcl_` and 32 hex digits, as the tool result names it |
| `member` | string | The called member's roster name |
| `member_task_id` | string | The id of the task the member's door holds: the `task_id` on the member's own `a2a_task_received` |

```json
{"event_type":"member_call_start","event_id":"evt_01a106502a41776088ec506b49b10a1c","parent_id":"evt_01a10650283b7a82bd5a5b81e6aa7554","session_id":"ses_01a10650282f7610a69952ba1dee5781","timestamp":1791107279425,"task_id":"tsk_01a10650283e7881b7a192a438359df9","call_id":"mcl_01a106502a3f7d4189618000400406c5","member":"worker","member_task_id":"tsk_01a106502a41745381369d93d349d695"}
```

**`member_call`**{ #member-call } — written once per `call-member` call, when it is accounted for:
delivered to the calling task, or left behind when that task ended

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The calling task |
| `call_id` | string | As on `member_call_start` |
| `member` | string | The called member's roster name |
| `member_task_id` | string | As on `member_call_start`. Absent for a call the member never held |
| `status` | string | `completed`, `failed`, `canceled`, `rejected`, `timed_out`, `unreachable` or `abandoned` — see below |
| `duration_ms` | u64 | From the tool call to this outcome |
| `output` | string | The member's answer for `completed`, its task's status message for `failed`, `canceled` and `rejected`, or why the call ended otherwise. At most 64 KiB and a cut marker |
| `truncated` | bool | Whether `output` was cut |
| `delivered` | bool | Whether the calling task received `output`: as the tool result for a call that ended within its tool call, as a continuation for any other. `false` for every `abandoned` call, and for an answer that arrived when the task did not wait for it — see [How the answer arrives](runtime-provided-tools.md#call-member-answer) |

| `status` | Meaning |
|---|---|
| `completed`, `failed`, `canceled`, `rejected` | The member's task ended in that state |
| `rejected`, with no `member_task_id` | The member never held the task: it stayed busy for the whole [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), or its door refused the task because its session was closing |
| `failed`, with no `member_task_id` | Every other call that never started: the caller's `capabilities.network.allow` does not reach the member's door, the door's address never arrived, the door answered an error, or it could not be reached |
| `timed_out` | The member had not answered within [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs); its task was not cancelled |
| `unreachable` | The member's door stopped answering |
| `abandoned` | The calling task ended before the answer arrived. There is no `member_task_id` when the task ended while a busy member was still being offered the task, or was cancelled while the member's door was being reached. Always `delivered: false` |

No member call line carries the member's door address or a token.

```json
{"event_type":"member_call","event_id":"evt_01a106502c377c72bcc0f222268e783d","parent_id":"evt_01a10650283b7a82bd5a5b81e6aa7554","session_id":"ses_01a10650282f7610a69952ba1dee5781","timestamp":1791107279927,"task_id":"tsk_01a10650283e7881b7a192a438359df9","call_id":"mcl_01a106502a3f7d4189618000400406c5","member":"worker","member_task_id":"tsk_01a106502a41745381369d93d349d695","status":"completed","duration_ms":504,"output":"WORKER-0123456789abcdef0123456789abcdef","truncated":false,"delivered":true}
```

**`task_failed`**{ #task-failed } — written once per task attempt that failed, before that task's
terminal `task_end`

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task in progress. Absent for the run a launch makes from `input.txt` when no task arrived, which has no `task_start` |
| `turn` | u32 | The turn that failed, 0-based. Absent when the failure is the task's rather than one turn's: `reopen_budget_exhausted` and `runtime_error` |
| `cause` | string | Why the attempt failed, from the table below |
| `reason` | string | What went wrong, in words: the driver's or provider's error text, the refusal, the runtime error. A `driver_error` reason has every credential value the session holds replaced with `[redacted]`, as `out/result.txt` does. At most 2,000 bytes; the whole text is in `out/result.txt`. For a launch that ends `failed`, this is the reason [`E-RUN-040`](diagnostics.md#e-run-040) prints |

| `cause` | Written when |
|---|---|
| `driver_error` | The driver returned an error, or a response whose `stop_reason` is `"error"`: a body it could not parse, a body cut off in transit, an HTTP error, provider error text, a provider request that timed out |
| `credential_rejected` | As `driver_error`, while the provider keeps rejecting the inference credential. The reason is the [`E-RUN-027`](diagnostics.md#e-run-027) message |
| `malformed_response` | The response the driver handed back asked for a tool call and carried none, or its `stop_reason` is missing or one the runtime does not handle |
| `compaction_hook` | A hook bound to `on-compaction` returned an error |
| `input_timeout` | A `request-input` wait passed [`lifecycle.input_timeout_secs`](manifest.md#lifecycle-input-timeout-secs) with no answer. The attempt makes no further inference call |
| `reopen_budget_exhausted` | An `on-task-end` hook still asked to reopen the task after `lifecycle.max_task_reopens` or `inference.max_turns` was spent. The reason names the limit |
| `runtime_error` | The attempt ended in any other error, for example a [`transport: process`](manifest.md#transport-process) failure (`E-RUN-033`–`E-RUN-036`). The reason is the error's text |

Written whatever [`trace.capture`](manifest.md#field-trace) is. A task that ended `ok`,
`max_turns_reached`, `spend_ceiling_reached` or `canceled` has no `task_failed` line: its
`task_end` status, a [`spend_ceiling_reached`](#spend-ceiling-reached) line or a
[`task_canceled`](#task-canceled) line already says what happened.

**`task_reopened`** — written once per reopen, between two agent-loop attempts of the same task,
when a blocking `on-task-end` hook (`commit_policy: reopen-task`) returns `reopen-task(reason)` and
the reopen is granted

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task being reopened |
| `hook_name` | string | Manifest name of the hook that requested the reopen |
| `reason` | string | Feedback text the hook returned; the next attempt receives it as one new user message |
| `reopen_number` | u32 | 1-based ordinal of this reopen within the task (first reopen = `1`) |
| `attempt_context` | string | `continued` — the next attempt continues the task's conversation with the feedback appended; `restarted` — the previous attempt left nothing to continue, so the next one starts from the rewritten `task.md` |
| `turns_remaining` | u32 | Turns the next attempt is handed: `inference.max_turns` less every turn the task's attempts have spent |

Appears zero or more times per task, always before the task's terminal `task_end`. See [Task
reopening](../concepts/session-loop.md#task-reopening-commit_policy-reopen-task) for the full
mechanism.

**`task_continued`**{ #task-continued } — written once per continuation with handed-off work's
outcomes: the task's attempt ended with a turn left, the task waited until no
[`call-member`](runtime-provided-tools.md#call-member-answer) call and no
[`delegate-task`](roost-api.md#how-the-outcome-arrives) delegation was outstanding, and the next
attempt receives every answer and outcome in one message

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | The task that continues |
| `continuation_number` | u32 | 1-based ordinal of this continuation within the task, counted across reopens |
| `member_calls` | string[] | The `mcl_` id of every call whose outcome this continuation delivers, in arrival order. `[]` when it delivers none |
| `delegations` | string[] | The `dlg_` id of every delegation whose outcome this continuation delivers — arrived, or ended by the backstop — in id order. `[]` when it delivers none |
| `waited_ms` | u64 | How long the task waited, from its attempt's end until nothing was outstanding |
| `turns_remaining` | u32 | Turns the continued attempt is handed: `inference.max_turns` less every turn the task's attempts have spent |

```json
{"event_type":"task_continued","event_id":"evt_0193…","parent_id":"evt_0192…","session_id":"ses_0192…","timestamp":1791273600000,"task_id":"tsk_0192…","continuation_number":1,"member_calls":["mcl_01a1…","mcl_01a2…","mcl_01a3…"],"delegations":[],"waited_ms":12210,"turns_remaining":8}
```

Written after the round's `member_call` and `delegation` lines and before the continued attempt's
first `inference`. Not written when the task is cancelled while it waits, when the attempt ended any
other way, or when no turn is left. `mur trace show` counts these lines under **Turns** as
`continued:  <N>  (<A> member answer(s), <D> delegation outcome(s))`.

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

**`tool_input_refused`**{ #tool-input-refused } — written when a tool call's input lacks a name
the tool's [`input_schema`](manifest.md#input-schema) lists in `required`, and the call is refused
before the tool runs

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | The turn the refused call was requested in |
| `tool_name` | string | The tool the model called |
| `tool_call_id` | string \| null | The provider's id for the call. `null` under `transport: process`, whose bridged calls carry none |
| `missing` | string[] | Every required name the input lacked, in the order the schema declares them |
| `reason` | string | The text the model was handed, so the trace and the agent agree on why |

Written whatever [`trace.capture`](manifest.md#field-trace) is. A `tool_call` record with
`status: "error"` accompanies it on both transports — the call is reported as a failed tool call —
and under the default `trace.capture: meta` that record carries no `output`, so this is the line
that holds the reason. No `call_denied` or `protected_path_denied` sits beside it: this check runs
first and the policy hook is not asked. A refusal is not a session failure and the turn continues.

A plan `tool` step refused for the same reason writes no `tool_input_refused`; its
[`plan_step`](#plan-events) record carries the same text in `error`.

**`spend_ceiling_reached`**{ #spend-ceiling-reached } — written when a spend ceiling refuses a
driver call before it is sent

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | The turn the refused call belonged to |
| `task_id` | string \| null | The task in scope, `null` between tasks |
| `limit` | string | `"session"` — [`inference.max_session_tokens`](manifest.md#inference-max-session-tokens) \| `"machine"` — [`spend.machine_tokens_per_day`](config.md#spend) |
| `ceiling` | u64 | The ceiling's value |
| `used` | u64 | `"session"`: this session's settled tokens plus its calls in flight. `"machine"`: the day's ledger total plus this session's calls in flight |
| `requested` | u64 | The refused call's `input_tokens` plus the most output it could request, and `0` for a ceiling reached rather than crossed by a call — which is every refusal under `transport: process`, where the spend is already made by the time the runtime learns of it |
| `origin` | string | `"hook:<hook name>"` for a hook's `run-inference` call. Absent for an agent turn |

Under `transport: http` no `inference` line accompanies it: nothing was sent. Under
`transport: process` it follows the `inference` line of the turn whose spend reached the ceiling,
which is the turn its `turn` names. A `"session"` refusal latches, so every later driver call in
the session writes one of these too; a `"machine"` refusal is checked again on every call.

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

**Harness records**{ #harness-records } — written only under
[`transport: process`](manifest.md#transport-process), where a harness CLI runs the turn and a
[process driver](manifest.md#process-driver) reads its output. No argument value, environment
value or file content is recorded on any of them — only names and counts.

**`harness_start`** — written once per harness run, after the driver plans it and before the
process exists

| Field | Type | Notes |
|---|---|---|
| `driver` | string | Artifact name of the process driver |
| `driver_version` | string | Its version |
| `harness` | string | The harness the driver names in its `describe()` |
| `binary` | string | Absolute path of the executable spawned |
| `binary_source` | string | `"inference.command"` or `"the process driver's describe()"` — which named the binary |
| `harness_version` | string \| null | The line the binary printed for its version arguments. `null` when it could not be read |
| `version_tested` | bool | Whether that line names a version the driver lists as tested. See [`W-RUN-002`](diagnostics.md#w-run-002) |
| `harness_session_id` | string | The session id handed to the driver. Distinct from `session_id`, the murmur session |
| `session_mode` | string | `"new"` or `"resume"` — whether this run started the harness on a fresh session or continued the one its context already had |
| `args_count` | u32 | How many arguments the driver asked for. Never the arguments |
| `env_names` | array of string | Names of the environment variables the harness was given, sorted. Never their values |
| `files` | array of string | Names of the files the driver asked to be written into the run's private directory. Never their contents |
| `bridge_tools` | array of string | The capsule's tool names offered over the loopback tool server. `[]` when the capsule declares no tools |
| `stdin_bytes` | u64 | Bytes written to the harness's stdin |
| `keep_stdin_open` | bool | Whether stdin stayed open after those bytes |

**`harness_session`** — written when the harness reports the session it opened

| Field | Type | Notes |
|---|---|---|
| `harness_session_id` | string | The session id the harness itself reports, which need not be the one it was given |
| `auth` | string | How the harness is authenticating. Anything other than `"subscription"` also raises [`W-SEC-031`](diagnostics.md#w-sec-031) |
| `model` | string \| null | The model the harness chose. `null` when it reported none |

**`harness_session_forgotten`**{ #harness-session-forgotten } — written when a person asked this
capsule to drop the harness session a context names, and there was one to drop. See
[what removes an entry](workdir.md#what-removes-an-entry)

| Field | Type | Notes |
|---|---|---|
| `context_id` | string | The context whose entry was dropped |
| `harness_session_id` | string | The id that was dropped, which the harness is no longer asked to continue |
| `requested_by` | string | `"cli"` for `mur run --forget-session`, `"a2a"` for the door header |

**`harness_warning`** — written beside each warning the run printed to stderr

| Field | Type | Notes |
|---|---|---|
| `code` | string | [`W-RUN-002`](diagnostics.md#w-run-002) or [`W-SEC-031`](diagnostics.md#w-sec-031) |
| `message` | string | The same text the stderr line carried |

**`harness_failed`** — written when the turn failed

| Field | Type | Notes |
|---|---|---|
| `kind` | string | `"auth"`, `"quota"`, `"max-turns"`, `"canceled"`, `"harness-error"` or `"other"` |
| `message` | string | What failed, from the harness or from the runtime |
| `source` | string | `"harness"` when the harness reported the failure, `"runtime"` when a turn opened past `inference.max_turns` |

The run ends with [`E-RUN-033`](diagnostics.md#e-run-033) naming the same kind.

**`harness_retry`** — written when the harness reports that it is retrying

| Field | Type | Notes |
|---|---|---|
| `attempt` | u32 | Which attempt the harness is on |
| `reason` | string | Why it is retrying |

**`harness_note`** — written for:

- anything else the harness said;
- a tool result that matched no call;
- each tool call the run ended without a result for;
- an interrupt that could not be delivered;
- a `classify-exit` call the driver never answered;
- a tool call whose connection the harness closed before the call returned;
- a tool call refused at the [concurrent tool call limit](../how-to/run-capsule-on-subscription.md#step-7-know-the-three-fixed-limits).

| Field | Type | Notes |
|---|---|---|
| `text` | string | The note |

**`harness_interrupt`** — written when a person cancels a task whose harness is running, before
the harness is given any grace

| Field | Type | Notes |
|---|---|---|
| `method` | string | `"stdin-message"`, `"signal-int"` or `"unsupported"` — the interrupt the [process driver](manifest.md#process-driver)'s `describe()` declares |
| `delivered` | bool | Whether a graceful interrupt reached the harness. `false` for `"unsupported"`, for a `"stdin-message"` driver whose own launch plan left nothing to write to or nothing to write, and for a `SIGINT` the kernel refused — each of which is also a `harness_note` |
| `grace_ms` | u64 | How long the harness has to end on its own before it is killed: 10000, or `0` whenever `delivered` is `false` |

The attempt ends `canceled` from here on, whatever the harness says next. Its `harness_exit`
carries `cause: "canceled"`, and `killed` says whether the runtime had to reap it.

**`harness_exit`** — written once per spawned harness, after it is gone

| Field | Type | Notes |
|---|---|---|
| `code` | i32 \| null | Exit code. `null` when a signal ended it |
| `signal` | i32 \| null | Signal that ended it. `null` when it exited normally |
| `cause` | string | `"terminal"` (the harness finished the turn), `"eof"` (its output ended first), `"inactivity"` ([`E-RUN-035`](diagnostics.md#e-run-035)), `"max_turns"`, `"driver_error"` or `"canceled"` (a person stopped the task) |
| `killed` | bool | Whether the runtime had to kill it rather than wait for it |
| `duration_ms` | u64 | How long the process lived |

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

**`delegation_start`** — written once per launched child, as soon as that child's process is up
and has reported its session id

| Field | Type | Notes |
|---|---|---|
| `delegation_id` | string | `dlg_…`, the id the delegation is named by. Always present: a delegation with no id was never launched and writes no line here |
| `capsule` | string | The sub-capsule that was named |
| `version` | string | The version that was named |
| `child_session_id` | string | `ses_…`, the session the child's runtime minted for itself |
| `child_workdir` | string | The child's directory, relative to this capsule's accessible workdir. Join the two, then `.murmur/<child_session_id>/trace.jsonl`, to reach the child's own trace |

Written when the child starts, so a child that then hangs, crashes or is ended is attributable
from the parent's side whatever happens next. A delegation the daemon refused writes none of these
— nothing was launched — and is recorded only by the `delegation` line below.

**`delegation`** — written once per delegation, when it ends

| Field | Type | Notes |
|---|---|---|
| `capsule` | string | The sub-capsule that was named |
| `version` | string | The version that was named |
| `delegation_id` | string \| null | `dlg_…`, the id the delegation is named by. `null` whenever no child was launched: a delegation the daemon refused, or one that was never started, was never made |
| `child_session_id` | string \| null | `ses_…`, the child's own session, so its trace is findable. `null` when no child ran |
| `duration_ms` | u64 | How long the child ran, on an outcome; how long the call took, on one that never started |
| `outcome` | string | How the delegation ended, in one of two vocabularies — see [Which outcome vocabulary applies](#delegation-outcome) |
| `reason` | string \| null | `null` on `"ok"`, `"error"` and `"completed"`; otherwise one sentence — the sub-capsule's `detail`, the sentence the model was given, or why the delegating task ended the sub-capsule (see [How the outcome arrives](roost-api.md#how-the-outcome-arrives)) |

### Which outcome vocabulary applies { #delegation-outcome }

`outcome` is drawn from the sub-capsule's own vocabulary whenever the delegating task, rather than
the call, closes the delegation:

| Delegation | Closed by | Vocabulary |
|---|---|---|
| A [`delegate-task`](runtime-provided-tools.md) call whose child started | The task | Sub-capsule |
| A plan [`capsule` step](plans.md) whose task was cancelled while its child ran | The task | Sub-capsule |
| Any other plan `capsule` step | The step | Delegating call |
| A `delegate-task` call whose child never started | The call | Delegating call |

The two vocabularies name different subjects: one names what a child that ran did, the other names
how far the delegating call got. A delegation that was `refused` never existed; one that `crashed`
did. A single merged list would lose that, so the two are kept apart.

The sub-capsule's vocabulary, read out of the child's own
[`completion.json`](roost-api.md#the-completion-path):

| Value | Means |
|---|---|
| `"ok"` | The child's session finished, and reported so itself |
| `"error"` | The child's session ran and failed, and reported so itself |
| `"crashed"` | The child's process ended without recording a completion |
| `"terminated"` | The child was ended: by the delegating task as it ended — including a cancel of the task running a plan, which ends a `capsule` step's child — at [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), or by the task's backstop 30 seconds after it |
| `"unknown"` | A completion arrived, and the parent found no readable `completion.json` behind it. The parent's own word, and reachable on no other path |

The delegating call's vocabulary, read out of the call's result:

| Value | Surface | Means |
|---|---|---|
| `"completed"` | `capsule` step only | The child answered the caller that was waiting for it |
| `"timed_out"` | `capsule` step only | The child had not answered within [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs) and was stopped |
| `"failed"` | either | The spawn was approved and no answer came back — a child that could not be launched or handed its task, or one whose own task failed |
| `"refused"` | either | `mur-roost` refused the spawn, so no child was launched |

**`ok` and `completed` are the only two values that say the sub-capsule did the work** — `ok` from
a `delegate-task` delegation, `completed` from a plan `capsule` step. A `delegate-task` delegation
reaches `ok`, never `completed`.

**`started` is never an `outcome`.** It is the
[`delegate-task` result's `status`](roost-api.md#the-delegation-tool), naming a delegation still in
flight, and a delegation in flight has written no `delegation` line at all: its terminal line is
written when its outcome is delivered or the task ends it.

**Two surfaces launch children**: the [`delegate-task`](runtime-provided-tools.md) tool an agent
calls, and a plan's [`capsule` step](plans.md). Both write both lines under the session node, and
neither line carries a field naming the surface — where the `outcome` value does not settle it, a
reader can tell that a delegation happened but not which surface made it.

They differ only in when the terminal line lands, because `delegate-task` returns as soon as the
child is up while a `capsule` step waits for the answer:

| | `delegate-task` | `capsule` step |
|---|---|---|
| Child started | `delegation_start` in the turn that called the tool; `delegation` later in the same task, when the outcome is delivered or the task ends the child | both lines within the step; when the task is cancelled while the child runs, `delegation` is written by the task after `task_canceled` |
| Child never started | `delegation` only, in the same turn, with no `delegation_id` | `delegation` only, within the step, with no `delegation_id` |
| Repeat launches | one pair per call | one pair per attempt, so a step with `retries` writes several |

A `capsule` step's `plan_step` records are not a second copy of this: the plan-step pair records
the scheduler's unit of work — its dependencies, its attempts, its status after `on_error` — and
the delegation pair records one child launch.

The `delegation` line carries neither the task text nor the child's answer — both are the agent's
own conversation, which the `tool_call` line for the same call already records under the session's
[`trace.capture`](manifest.md#field-trace) setting.

### Reading a formation { #formation }

A formation member's id is recorded in two places:

| Where | Field | Lifetime |
|---|---|---|
| The first line of the member's `trace.jsonl` | `session_start.formation_id` | As long as the trace is kept |
| The member's [running-capsule record](cli.md#running-capsule-records) | `formation_id` | While the session runs |

[`mur trace show <formation-id>`](cli.md#mur-trace-show-formation) reconstructs a formation from
the session roots it searches: it reads the first line of each
session's trace, and for each member reads on to its `session_end`. A member with no `session_end`
— killed, or still running — is listed as `no session_end`. A session whose first line is not a
`session_start` is no member. The command does not follow `delegation_start` lines into other
roots; it counts a member's children found elsewhere, and the member's own `mur trace show` names
each child's trace. [`mur ps`](cli.md#mur-ps-formations) reads the same lines to account for members it does not
list.

A member that wound down because its formation ended carries one
[`formation_ended`](#formation-ended) line, ahead of its `session_end`.

A session resumed with `mur run --resume` is a member only when its own launch names a formation;
its `resumed_from` names the member it continues.

### Reading a delegation tree { #delegation-lineage }

The relationship between a parent and a child is recorded once, from both ends, and joined by the
`dlg_` id:

| From | Read | To reach |
|---|---|---|
| A parent's trace | `delegation_start.child_workdir` and `child_session_id` | `<accessible workdir>/<child_workdir>/.murmur/<child_session_id>/trace.jsonl` |
| A child's trace | `session_start.spawned_by` | The `ses_` id of the session that spawned it |
| A child's trace | `spawner_ended.spawned_by` and `delegation_id` | The same lineage, on a child that wound down because that session's process ended |
| A parent's trace | `delegation_start.delegation_id` | The terminal `delegation` line with the same `delegation_id`, written by the task that made the delegation before its `task_end` |

[`mur trace show`](cli.md#mur-trace-show) renders both ends within the one file it is given: a child's
header names the session that spawned it and the delegation that created it, and a parent grows a
Delegations section listing each delegation, the child session it launched, how it ended and why.
No command walks a delegation tree across files.

**A resumed child's lineage is one hop back.** `spawned_by` is written at spawn and never
rewritten, so resuming a *parent* keeps the child reachable: the resumed session's `resumed_from`
names the session the child's `spawned_by` names. Resuming a *child* is the other direction and the
window is open — that resume is an operator launch with no `MURMUR_SPAWNER` in its environment, so
the new session writes no `spawned_by` at all, and its `resumed_from` names the child session that
was spawned. The lineage is in the session it continues, one `resumed_from` hop back.

**The handle itself never appears in a trace, on either side.** Where a token would otherwise reach
one — most obviously as the recorded `handle` argument of a `fetch-peer-file` `tool_call` — it is
replaced with `<handle:<handle_id>>`.

**`plan_start`**{ #plan-events } — written once by the plan scheduler, as soon as the plan file
parses

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
| `outcome` | string | `"completed"`, `"failed"` or `"canceled"`. `"canceled"` is a run whose task was cancelled — see [When the task is cancelled](plans.md#when-the-task-is-cancelled) |
| `failed_step` | string | The step that ended the run. `"plan"` when the run failed before any step could — a file that would not parse or validate, a cgroup scope the host refused. Absent on `"completed"` and `"canceled"` |
| `steps_total` | usize | How many steps the plan declared |
| `steps_succeeded` | usize | |
| `steps_failed` | usize | |
| `steps_skipped` | usize | |
| `duration_ms` | u64 | Wall-clock time for the whole run, the plan file read included |
| `reason` | string | Why the run ended when the reason was not a step's own failure: `the task running this plan was cancelled` on `"canceled"`. Absent otherwise |

The three counts cover the steps that settled, and sum to less than `steps_total` on a run that
failed early. A `"canceled"` run settles every step, so its counts sum to `steps_total`.

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
- A `task_end` carries the attempt's own terminal outcome, so it reads `"failed"`,
  `"max_turns_reached"` or `"spend_ceiling_reached"` on a task the runtime survived and reported
  on. The launch's own `session_end` carries the first of those outcomes among the tasks that
  decide the launch, and a later task that completed does not replace it.
- Every failed attempt writes one `task_failed` line before its task's `task_end`.

---

## Task stream `artifact` frame { #task-stream-artifact-frame }

The `artifact` frame on a capsule's event stream, and every other frame, is listed in
[Streaming Protocol](streaming-protocol.md#event-artifact).

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
| `capsule.inference` | `inference` | `turn`, `input_tokens` and `output_tokens` (each when the turn was counted at all), `decision`, `stop_reason` (the provider's own reason, on every agent-loop turn), `tool_name` (when the response asked for one), `input_tokens_actual`, `output_tokens_actual`, `cached_tokens`, `cache_write_tokens` and `thinking_tokens` (each when the driver reported it), plus `origin` and `model` for a hook-run completion |
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
- A session that belongs to a [formation](cli.md#mur-run-formation) hands its formation id to every
  hook as `MURMUR_FORMATION_ID`, and `murmur-hook-grafana` adds it to the root span as
  `murmur.formation_id`. A session in no formation hands none.
