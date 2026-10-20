# Runtime-provided tools

A runtime-provided tool is an entry in the agent's tool inventory with no artifact behind it. The
runtime writes its `murmur.yaml` into `tools/<name>/` in the session's
[runtime directory](workdir.md#runtime-directory) while the session is staged, and answers the call
itself.

The model sees no difference. A runtime-provided tool has the same manifest shape, appears in the
same inventory, and is called the same way as a tool artifact. What differs is where it comes from
and what decides whether it may run.

## The eleven

Each one appears only when the declaration in the second column is present.

| Tool | Gated on | Does |
|---|---|---|
| One per shell binary | [`capabilities.shell.allow`](manifest.md#shell-allow) | Runs that binary as a subprocess in the accessible workdir |
| `share-file` | [`exports.peer_files`](manifest.md#field-exports-peer-files) | Mints a [peer-file handle](resource-plane.md#peer-plane) for one file under the declared export root |
| `fetch-peer-file` | [`capabilities.peer_fetch`](manifest.md#field-peer-fetch) | Redeems a handle a peer sent and stores the file in this capsule's workdir |
| `delegate-task` | [`capabilities.spawn.allow`](manifest.md#field-capabilities) | Hands one task to one sub-capsule, named by the bare part of its `spawn.allow` entry, and returns as soon as it is running and holding it; once the turn ends, the task waits and continues with the outcome — see [The delegation tool](roost-api.md#the-delegation-tool) |
| `submit-plan` | [`capabilities.plan.submit`](manifest.md#field-capabilities) | Runs one plan of steps against this session's own tools and returns every step's result — see [Plans](plans.md) |
| `switch-driver` | [`control.agent_settings: [inference.driver]`](manifest.md#field-control) | Selects the [driver choice](manifest.md#inference-alternates) the agent's next inference call is served by — see [`switch-driver`](#switch-driver) |
| `call-member` | A [`reachability`](roster.md#reachability) rule in the formation's `roster.yaml` that lets this member call another | Hands one task to another formation member and returns at once, started or busy; the answer arrives later in the same task — see [`call-member`](#call-member) |
| `end-without-answer` | The same rule as `call-member` | Ends a task another formation member sent without an answer, and the runtime tells that member none came — see [`end-without-answer`](#end-without-answer) |
| `pull-artifact` | [`capabilities.install`](manifest.md#field-install), under `transport: http` | Installs a skill or tool into the running session, usable from the next turn, or lists what the capsule may pull — see [`pull-artifact`](#pull-artifact) |
| `remove-artifact` | The same grant as `pull-artifact` | Takes out an artifact the session pulled — see [`remove-artifact`](#remove-artifact) |
| `session-status` | An [`inference:`](manifest.md#field-inference) block: every agent capsule, on both transports | Reports the time the session has run and has left, its turns, context, tokens and spend — see [`session-status`](#session-status) |

Every one of their manifests carries `version: 0.0.0`, `runtime: tool` and
`implementation: native`. Nothing was fetched, so nothing is version-pinned, nothing is
hash-verified, and no entry appears for them in `murmur.lock` or in `mur list`.

## The grant is the tool's existence

`share-file`, `fetch-peer-file`, `delegate-task`, `submit-plan`, `switch-driver`, `call-member`,
`end-without-answer`, `pull-artifact`, `remove-artifact` and `session-status` are answered before
the tool allowlist is consulted. The allowlist governs which tool *artifacts* may run; it has no say
over these ten.

`session-status` needs no grant: staging writes its manifest for every agent capsule and for no
script capsule.

**The gate is whether the manifest file was written at all.** With the grant absent, staging writes
nothing under `tools/<name>/`, so:

- The tool is absent from the inventory the model is sent.
- It is absent from `session_start`'s `tools_declared` in [`trace.jsonl`](observability-schemas.md).
- A call naming it anyway is refused with a message naming the declaration that is missing.

So `capabilities.spawn.allow` decides whether `delegate-task` exists, rather than whether a call to
it succeeds. The same holds for the two peer-handoff tools, for `submit-plan`, for
`switch-driver`, for `call-member` and `end-without-answer`, for `pull-artifact` and
`remove-artifact`, and for their grants.

`capabilities.plan.submit` also decides whether the model is told *when* to plan: the runtime's
plan guidance is part of the system prompt exactly when the tool exists.

Shell binaries are gated the same way — one manifest per name in `capabilities.shell.allow` — and
are additionally checked against that list again at dispatch.

## Reserved names

`share-file`, `fetch-peer-file`, `delegate-task`, `submit-plan`, `switch-driver`, `call-member`,
`end-without-answer`, `pull-artifact`, `remove-artifact` and `session-status` are reserved.
The check is made on an artifact's tool name, the part of its full name after the `/`, so a
capsule declaring `murmur/delegate-task` or `ana/pull-artifact` is refused at staging, before any
artifact is pulled, with [`E-CAP-013`](diagnostics.md#e-cap-013). The same refusal covers an
in-session pull of such a name, by `manage.pull()` or by `pull-artifact`.

A name is reserved whether or not the capsule declares the grant that would provide the tool, so
adding or removing a capability never changes whether a manifest is accepted.

Shell binary names are not reserved. They come from `capabilities.shell.allow`, which is the
operator's own list, so there is no fixed set to reserve — and a capsule may declare both an
artifact whose bare name is `bash`, such as `local/bash`, and `bash` in its shell allowlist. Staging writes the artifact's manifest
first and the shell manifest yields to it, so the inventory describes the artifact.

Dispatch resolves such a pair by [precedence](../concepts/tools.md#tool-dispatch): a native
artifact answers ahead of the shell binary, and the shell binary answers ahead of a WASM artifact
of the same name.

## `session-status` { #session-status }

`session-status` takes no input (`{"type":"object","properties":{}}`) and returns one JSON object,
measured at the moment of the call:

```json
{
  "time":    {"elapsed_secs": 1834, "task_elapsed_secs": 1790,
              "deadline": {"at": "2026-10-20T14:00:00Z", "left_secs": 5366}},
  "turns":   {"used": 42, "cap": "none"},
  "context": {"used_tokens": 61234, "window_tokens": 200000, "measured_by": "estimate"},
  "tokens":  {"input": 812345, "output": 23456, "cached": 600000, "cache_write": 10000,
              "unreported_calls": 0},
  "spend":   {"tokens": 835801, "ceiling": "none", "machine": "none"}
}
```

A limit that is not set reads `"none"`. A count the runtime does not have reads `"unknown"`.
Seconds are whole, rounded down.

| Field | Value | `"none"` / `"unknown"` |
|---|---|---|
| `time.elapsed_secs` | Seconds since the session started | — |
| `time.task_elapsed_secs` | Seconds since the running task's `task_start` | `"unknown"` outside a task |
| `time.deadline.at` | When the runtime ends the session, RFC 3339 UTC: [`mur run --deadline`](cli.md#mur-run-deadline) seconds after `mur run` started | `time.deadline` is `"none"` with no `--deadline` |
| `time.deadline.left_secs` | Seconds until `at`; `0` once it has passed | — |
| `turns.used` | Inference calls the running task has made, across its reopens and continuations, a hook's `run-inference` included: the count [`inference.max_turns`](manifest.md#field-inference) is checked against | — |
| `turns.cap` | `inference.max_turns` | `"none"` with no cap |
| `context.used_tokens` | The conversation's size as the compaction check last counted it | `context` is `"unknown"` before the first turn, and always under `transport: process` |
| `context.window_tokens` | The context window that check compared it with, the serving driver choice's | `"none"` when the choice has none |
| `context.measured_by` | `"estimate"`: the runtime's own count | — |
| `tokens.input`, `tokens.output`, `tokens.cached`, `tokens.cache_write` | Sums of what the provider reported over the session's `inference` records so far | `"unknown"` for a member no record carried |
| `tokens.unreported_calls` | `inference` records that carried no count | — |
| `spend.tokens` | Tokens the spend ceilings count for this session | — |
| `spend.ceiling` | [`inference.max_session_tokens`](manifest.md#field-inference) | `"none"` with no ceiling |
| `spend.machine` | `{"tokens_today": …, "ceiling": …}`: this machine's day total against [`spend.machine_tokens_per_day`](config.md#spend) | `"none"` when the machine ceiling does not cover the session |

No money amount is kept on either transport: spend is counted in tokens.

The two transports differ in where the numbers come from:

| Field | `transport: http` | `transport: process` |
|---|---|---|
| `turns.used` | Every inference call | Turns the harness has closed |
| `context` | An object | `"unknown"`: the harness keeps its own context |
| `tokens` | The provider's `usage` on each call | The harness's own counts for each closed turn |

Each call writes one [`session_status`](observability-schemas.md#session-status) record carrying
the object returned.

### The deadline notice { #deadline-notice }

With [`mur run --deadline`](cli.md#mur-run-deadline) set, the agent is told once that the session is about
to end. The first tool result the runtime hands the model once no more than
[`lifecycle.deadline_notice_secs`](manifest.md#field-lifecycle) are left carries one line after
its fence:

```text
Session deadline: the runtime ends this session at 2026-10-20T14:00:00Z, in 297 seconds.
```

| Aspect | Behaviour |
|---|---|
| Margin | `lifecycle.deadline_notice_secs`; default five minutes, or a tenth of the deadline when that is shorter, and at least one second. `0` sends none |
| Carried by | The first tool result dispatched inside the margin, on either transport — under `transport: process`, the bridged tool result. A tool that ran and reported failure carries it; a refused call, or one the runtime could not run, does not |
| How often | Once per session |
| A turn with no tool call | Gets no notice; the deadline still ends the session |
| Trace | One [`deadline_notice`](observability-schemas.md#deadline-notice) record |

Nothing else the runtime adds to the conversation changes. What happens when the deadline is
reached is described under [The deadline](cli.md#mur-run-deadline).

## `switch-driver` { #switch-driver }

| Aspect | Behaviour |
|---|---|
| Input | `{"driver": "<choice name>"}`. The schema lists every declared choice |
| Description | Lists every choice with its model and driver |
| Accepted | The next agent-loop inference call, and every one after it, is served by the choice. The tool result names the previous and the new choice |
| Refused | Returned to the model as a tool error, with the same checks and messages a controller gets: an undeclared name, or a choice whose credential cannot produce a value now. Nothing switches |
| Policy | The call passes the same `on-tool-call` decision point as every tool call, so a policy hook can deny it |
| Plans | Not callable from a plan step |
| Trace | `control_change` or `control_refused` with `principal: "agent"` and no `token_id`; `control_applied` at the first call that uses the choice |

The runtime answers the call in-process: it presents no credential and never reaches the
[control surface](control-surface.md) or its token. What a switch carries across is in
[Switching drivers](../concepts/context.md#switching-drivers).

## `pull-artifact` { #pull-artifact }

`pull-artifact` exists for a capsule that declares [`capabilities.install`](manifest.md#field-install)
and runs the http agent loop. A capsule under `transport: process` is refused at load, because the
harness owns its tool list.

| Aspect | Behaviour |
|---|---|
| Input | `{"artifact": "<value>"}`, required. `<scope>/<name>` pulls the newest version that fits this `mur`; `<scope>/<name>@<version>` pulls that version; a pattern ending in `*` lists instead of pulling |
| Description | Lists the capsule's `install.skill` and `install.tool` entries, or `none`, and its `install.from`. Says a pattern lists instead of pulling, that the artifact is usable from the next turn, and that a pulled artifact was not vetted by the operator, so its text and results are untrusted |
| Checked | The grant by tier and pattern, never a hook or a driver, the payload's hash, its [WIT level](wit-levels.md#fit), the scope's owner id against `murmur.lock`, and an existing pin. A refusal writes nothing |
| Found in | The project store, then the global store. With `install.from: [stores, sources]`, a version no store holds is fetched from the scope's GitHub source by the runtime itself and written to no store |
| Accepted | `passed`; the artifact is in the tool array from the next turn, whatever [`inference.tool_refresh`](manifest.md#inference-tool-refresh) says. A tool joins the dispatch allowlist |
| Refused | Returned to the model as a tool error opening with one token — see [Refusals](#pull-artifact-refusals) |
| Policy | The call passes the same `on-tool-call` decision point as every tool call, so a policy hook can deny it |
| Plans | Not callable from a plan step |
| Trace | `artifact_pulled` per pull; `tools_refreshed` with `trigger: "agent"` before the next inference call |

A successful pull's `data`, [fenced](untrusted-fence.md) under `tool:pull-artifact`, is followed by
the runtime's note outside the fence:

```text
<untrusted-content source=tool:pull-artifact>
{"artifact":"murmur/tool-editor","fetched_from":"global store","runtime":"tool","tool_name":"tool-editor","version":"1.0.0"}
</untrusted-content>
tool-editor is in your tool list from your next turn.
```

| Key | Value |
|---|---|
| `artifact` | The full name |
| `version` | The version pulled |
| `runtime` | `skill` or `tool` |
| `tool_name` | The name the model calls it by |
| `fetched_from` | `project store`, `global store` or `github:<owner>/<repo>` |
| `notes` | Present when a place could not be consulted and the pull went ahead without it |

A pattern returns, under the same fence and with no note:

| Key | Value |
|---|---|
| `pattern` | The pattern asked for |
| `matches` | At most 50, sorted by name: `artifact`, `runtime`, `description` (when one is recorded), `versions` (newest first), `newest_fitting` (when one fits), `held_in` |
| `omitted` | How many more matched |
| `notes` | Why a place could not be read |

A match is an artifact the pattern names that an entry of its own tier's list also names, held in
an allowed place. A pattern that reaches no granted name is refused `not-granted:`.

### Refusals { #pull-artifact-refusals }

| Token | When |
|---|---|
| `not-granted:` | No `capabilities.install`; not a full name; outside the grant; a hook, a driver, or a role that disagrees with its payload; a tool name another artifact of the session holds |
| `not-found:` | No allowed place holds the name or version |
| `does-not-fit:` | The payload does not fit the WIT level this `mur` serves, carrying [`E-WIT-LEVEL-001`](diagnostics.md#e-wit-level-001) and naming the newest version that fits |
| `not-fetched:` | The source could not be read, failed a check, or answers for an owner `murmur.lock` does not record for the scope ([`E-REG-010`](diagnostics.md#e-reg-010)) |

A pin of the same name at another version or digest is refused with
`murmur.lock conflict for '<name>': …`.

## `remove-artifact` { #remove-artifact }

| Aspect | Behaviour |
|---|---|
| Input | `{"artifact": "<value>"}`, required: the full name or the tool name |
| Accepted | `passed`, `data` `{"artifact": "<full name>", "removed": true}`, then the note `<tool name> is gone from your tool list from your next turn; a call to it is refused.` The lock entry, `tools/<tool name>/` and the allowlist entry are gone |
| Not installed | `passed`, `data` `{"artifact": "<name>", "removed": false}`, no note |
| Refused | A tool error opening `not-removable:` for a hook, a driver, an artifact `murmur.yaml` declares, or a pin with `origin: operator`; `not-granted:` with no `capabilities.install` |
| In-flight calls | The agent loop runs a turn's calls one at a time, in order: a call listed before the removal completes, one listed after it is refused with `tool '<name>' was removed from this session; it can no longer be called` |
| Policy | The same `on-tool-call` decision point as every tool call |
| Plans | Not callable from a plan step |
| Trace | No event of its own; `tools_refreshed` with `trigger: "agent"` and the name in `removed` before the next inference call |

## `call-member` { #call-member }

`call-member` exists for a member of a formation launched by
[`mur run --roster`](cli.md#mur-run-roster) when `roster.yaml` lets it call at least one other
member. A member the roster lets call nobody, and a session in no formation, has no such tool.

| Aspect | Behaviour |
|---|---|
| Input | `{"member": "<name>", "task": "<text>"}`, both required. The schema's `member` is an `enum` of the members this one may call, in roster order |
| Description | Tells the model that `task` is the whole of what the member is told, so any file content it needs goes in the task text; that the call returns at once with a call id, started when the member holds the task or busy when it has no room, in which case the runtime keeps offering the task until the member takes it or the call's deadline passes; that the answer arrives in the conversation after the turn ends, so it should not wait or poll; and that a second call to a member before its answer has arrived is refused |
| Sent | One `SendMessage` to the member's door, carrying `task` as one text part, the formation token, and the calling task's trust class with origin `peer`. The model never sees the door's address or the token |
| Egress | The member's door is checked against this capsule's own [`capabilities.network.allow`](manifest.md#network-allow-entries), which must list `localhost` — see [Giving a member work](roster.md#member-calls). Without it, every call fails and staging prints [`W-RUN-008`](diagnostics.md#w-run-008) |
| Trace | One [`member_call_busy`](observability-schemas.md#member-call-busy) per offer the member turns away busy, [`member_call_start`](observability-schemas.md#member-call-start) when the member holds the task, and one [`member_call`](observability-schemas.md#member-call) per call once it is accounted for |

### What a call returns { #call-member-result }

A call ends its tool call in one of four ways:

| Result | When | `data` |
|---|---|---|
| `passed`, summary `Called <member>: started` | The member's door answered with a task id and a state that is not terminal | `{"call_id": "mcl_…", "member": "<name>", "status": "started", "task_id": "<the member's task id>"}` |
| `passed`, summary `Called <member>: busy` | The member's door answered [`TASK_STATE_REJECTED`](agent-card.md#task-states) with the status message `task rejected: capsule is busy`. The runtime keeps offering the task — see [A busy member](#call-member-busy) | `{"call_id": "mcl_…", "member": "<name>", "status": "busy", "output": "<member> is busy with other work and did not take the task"}` |
| `failed`, summary `Called <member>: rejected` | The member's door answered `TASK_STATE_REJECTED` for any other reason, such as its session closing, which `call-member` reports as `rejected` | `{"call_id": "mcl_…", "member": "<name>", "status": "rejected", "output": "<member> did not take the task: <reason>"}` |
| `failed`, summary `Called <member>: failed` | The call was refused, the door could not be reached, the door answered an error status or a JSON-RPC error, or the member answered with another terminal state | `{"call_id": "mcl_…", "member": "<name>", "status": "failed", "output": "<why>"}` |

`data` reaches the model [fenced](untrusted-fence.md) under `tool:call-member`. Each result
adds the runtime's own note on the line after the closing marker, outside the fence:

```text
<untrusted-content source=tool:call-member>
{"call_id":"mcl_01a1…","member":"worker","status":"started","task_id":"tsk_…"}
</untrusted-content>
[call-member] worker is now working on call mcl_01a1…. Its answer is not in this result and no tool fetches it: the runtime adds it to this conversation after you end your turn. Unless you still have work to hand to a different member, end your turn now by replying without calling a tool. Calling worker again before its answer arrives is refused.
```

| Result | Note |
|---|---|
| `started` | `[call-member] <member> is now working on call <call_id>. Its answer is not in this result and no tool fetches it: the runtime adds it to this conversation after you end your turn. Unless you still have work to hand to a different member, end your turn now by replying without calling a tool. Calling <member> again before its answer arrives is refused.` |
| `busy` | `[call-member] <member> is busy with other work and has not taken call <call_id> yet. The runtime keeps offering it the task for up to <N>s and adds <member>'s answer, or word that it stayed busy, to this conversation after you end your turn. Unless you still have work to hand to a different member, end your turn now by replying without calling a tool. Calling <member> again before then is refused.` `<N>` is the call's deadline in seconds |
| `rejected`, `failed` | `[call-member] Call <call_id> to <member> ended <status>, with no answer from <member>. Do not present an answer of your own as <member>'s.` For a task another formation member sent, followed by ` If you have no answer to give without <member>, call end-without-answer with the reason: the runtime then tells <caller> plainly that you gave none.` |

A door's `TASK_STATE_REJECTED` answer reads as one sentence:

| The door's status message | `output` |
|---|---|
| `task rejected: capsule is busy` | `<member> is busy with other work and did not take the task` |
| Any other | `<member> did not take the task: <message>`, with a leading `task rejected: ` dropped |
| None | `<member> did not take the task` |

### A busy member { #call-member-busy }

A busy member's call is outstanding from the moment the tool call returns, and the runtime, not
the model, offers the member the task again:

1. Each offer is a new `SendMessage` carrying the same task text.
2. The second offer comes 1 second after the first refusal. Each later wait doubles, up to 8
   seconds, plus up to 250 ms of jitter.
3. Each offer the member turns away busy writes its own
   [`member_call_busy`](observability-schemas.md#member-call-busy).

The call then ends in one of these ways:

| What happens | The call |
|---|---|
| The member takes the task | Writes [`member_call_start`](observability-schemas.md#member-call-start) and runs as a started call does, against the same deadline |
| The member answers `TASK_STATE_REJECTED` for another reason, or another terminal state | Ends `rejected` or `failed` respectively, with the sentence [What a call returns](#call-member-result) gives |
| Two offers in a row get no answer | Ends `unreachable` |
| [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs) passes while the member is still busy | Ends `rejected`, never held: `<member> stayed busy with other work for the whole <N>s this call may wait and never took the task; it was offered the task <k> times. Nothing was done on it.` |
| The calling task ends between offers | Ends `abandoned`, never held: `the calling task ended before <member> took the task; <member> was busy and was never handed it` |
| The calling task ends while an offer is on its way, and the member takes the task | Ends `abandoned` with the member's task id: `the calling task ended just after <member> took the task; a cancel was sent to <member>`. The runtime sends that task `CancelTask` |
| The calling task ends while an offer is on its way, and the member turns it away busy | Writes that offer's `member_call_busy`, then ends as when the calling task ends between offers |
| The calling task ends while an offer is on its way, and the member does not answer within 6 seconds | Ends `abandoned`: `the calling task ended while an offer to <member> was in flight; <member> may hold the task` |

A calling task that ends while an offer is on its way waits up to 6 seconds for the member's answer
to that offer before it ends. No offer is sent after the calling task ends.

Only a refusal with exactly the message `task rejected: capsule is busy` is offered again; any
other refusal ends the call in the same turn.

Four calls are refused as a tool error, with nothing sent and nothing recorded as a member call:

| Call | Error |
|---|---|
| A `member` the roster does not let this capsule call | Names the members it may call |
| A call made while no task is running | Says the tool is answered only while a task runs |
| A call made after [`end-without-answer`](#end-without-answer) was accepted in this task | `'call-member' makes no call from a task that is ending without an answer.` |
| A `member` that already holds a call from this task whose answer has not been delivered | One of the two texts below, naming the earlier call |

| The earlier call's answer | Error text |
|---|---|
| Has not arrived | `<member> has not yet answered call <call_id> from this task, so nothing was sent. Its answer reaches you only after you end your turn: end your turn now by replying without calling a tool. Once that answer has arrived you may call <member> again with new work.` |
| Has arrived and waits for the turn to end | `<member> has already answered call <call_id>, so nothing was sent. The answer reaches you as soon as you end your turn: end your turn now by replying without calling a tool.` |

Once its answer has been delivered, a member can be called again with new work. Calls to
different members in one turn never refuse each other.

### How the answer arrives { #call-member-answer }

1. The model ends its turn with a turn still left.
2. The task waits until every call it made has ended. While any call is outstanding, the task's
   stream shows a `TASK_STATE_WORKING` status `waiting on call-member: <n> call(s) outstanding`: once when
   the wait starts, and again with the new count each time an answer arrives while others are
   still out. When every answer arrived during the turn, the task does not wait and shows no
   such status.
3. Once every call has ended, the task continues with one new user message holding every answer.
   The stream shows `continuing with <n> member answer(s)`, and the trace records one
   [`task_continued`](observability-schemas.md#task-continued).
4. The model reads the answers and answers in turn, or calls again.

One round of calls costs one continuation, however far apart the answers arrive. The wait is
bounded by each call's own deadline, [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs):
a member that never answers holds the round until its call ends `timed_out`.

Each answer in that message is one header line naming the call id, the member and how the call
ended, then the member's output fenced under `member:<name>`, whatever the calling task's trust.
The message ends with the runtime's own lines, outside every fence:

```text
[call-member] call mcl_01a1… to worker ended completed:
<untrusted-content source=member:worker>
WORKER-0123
</untrusted-content>

[call-member] call mcl_01a2… to critic ended timed_out, with no answer from critic:
<untrusted-content source=member:critic>
critic did not answer within 600s. …
</untrusted-content>

[call-member] No answer came from: critic (call mcl_01a2…, timed_out). What you asked of them has not been done by them: do not present an answer of your own as theirs.

[call-member] Every call this task made has ended. Answer the task with the answers you have and say plainly which part has no answer, or call a member again if another attempt could succeed.
```

| Ending | Header |
|---|---|
| `completed` | `[call-member] call <call_id> to <member> ended completed:` |
| Any other | `[call-member] call <call_id> to <member> ended <status>, with no answer from <member>:` |

A call ends `no_answer` when the member ended its task with
[`end-without-answer`](#end-without-answer): its task is `TASK_STATE_FAILED` and its `GetTask` result
carries [`metadata.murmur.noAnswer: true`](agent-card.md#no-answer-metadata). A member's
`GetTask` result may also name, in `metadata.murmur.noAnswerBelow`, the members further down
that gave it no answer. The runtime reads those names only for a `completed` or `no_answer` call,
keeps at most 8, and keeps only a roster member name with a status other than `completed`.

A member has no answer when its latest call in this task did not end `completed`, whether that
call ended in its own tool call or in a continuation. Each such member is named, with that call's
id and status, in one line after every fence:

```text
[call-member] No answer came from: <member> (call <call_id>, <status>), <member> (call <call_id>, <status>). What you asked of them has not been done by them: do not present an answer of your own as theirs.
```

A member that reported members further down with no answer is named in the further-down form:

```text
<member> (call <call_id>, <status>; further down, no answer came from <name> (<status>), <name> (<status>))
```

A member whose latest call ended `completed` while it reported members further down with no
answer has answered with a gap. Its answer is fenced as usual, and it gets a line of its own after
the no-answer line:

```text
[call-member] <member> (call <call_id>) answered without an answer from <name> (<status>), <name> (<status>): any part of its answer that stands in for theirs is <member>'s own, not theirs.
```

A later call to the same member that completes with nothing missing below it removes it from
both lines. The message's last line:

| Members with no answer, or with a gap | Last line |
|---|---|
| None | `[call-member] Every call this task made has ended, and the answers are above. Answer the task with them now; call a member again only to give it new work.` |
| One or more | `[call-member] Every call this task made has ended. Answer the task with the answers you have and say plainly which part has no answer, or call a member again if another attempt could succeed.` |

For a task another formation member sent, the second line continues with
` If you have no answer to give, call end-without-answer with the reason instead: the runtime then tells <caller> plainly that you gave none.`
The entry member's task has no formation caller, so its lines never name the tool.

| Ending | Output |
|---|---|
| `completed` | The member's answer: its task's `response` artifact |
| `no_answer` | The member's reason, its task's status message, or `<member> ended its task without an answer` |
| `failed`, `canceled`, `rejected` | The member's task's status message, or for a member that stayed busy, the runtime's sentence from [A busy member](#call-member-busy) |
| `timed_out` | The member did not answer within [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs); it was not cancelled and may still be working |
| `unreachable` | The member's door stopped answering |

Under [`transport: process`](manifest.md#transport-process), the harness calls `call-member`
through the runtime's loopback tool server, and the message holding the answers reaches the harness
as the prompt of a resume of its same session.

Output is cut at 64 KiB. Continuing with answers is not a reopen: it spends no
[`lifecycle.max_task_reopens`](manifest.md#field-lifecycle), writes no `task_reopened`, and runs
before the task's `on-task-end` hooks. Its turns count against
[`inference.max_turns`](manifest.md#field-inference).

The task waits for answers only when the model ends a turn with a turn still left. Every other ending accounts for the task's calls at once, and
the task ends as its attempt ended:

| The attempt | Calls still outstanding | Answers that arrived |
|---|---|---|
| The model ends a turn with a turn left | Waited for, as above | Delivered in the continuation |
| The model ends its last allowed turn | Not waited for: each is recorded `abandoned` | Recorded `delivered: false` |
| Fails, runs out of turns, is cancelled, or is stopped by `SIGTERM` or its formation ending | Each is recorded `abandoned` | Recorded `delivered: false` |

A task cancelled while it waits ends `TASK_STATE_CANCELED`. A task with `delegate-task` delegations
outstanding as well waits for both kinds together and continues once neither has anything
outstanding, with every answer and every delegation outcome in one message, answers first — see
[How the outcome arrives](roost-api.md#how-the-outcome-arrives).

| Bound | Value |
|---|---|
| How long a call may take, [offering a busy member the task](#call-member-busy) and waiting for its answer together | [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), default 600 seconds, or `MURMUR_DELEGATION_TIMEOUT_SECS` — the bound delegations use |
| How often the member's task is read | Every 500 ms, through the member's `GetTask` |
| Unreachable | Two reads, or two offers, in a row that get no answer |

The runtime cancels a member's task only when the member took it just after the calling task
ended, as the [busy-member table](#call-member-busy) shows. A call that times out, or is abandoned
while the member works on the task, leaves the member's task running until it finishes or the
formation ends.

## `end-without-answer` { #end-without-answer }

`end-without-answer` exists exactly where [`call-member`](#call-member) does. It ends the running
task without an answer, when the member has none to give — usually because a member it called
gave it none. The runtime then tells the member that sent the task, in its own words, that no
answer came.

| Aspect | Behaviour |
|---|---|
| Input | `{"reason": "<text>"}`, required. The reason is the task's status message |
| Description | Tells the model to use it instead of replying when it has no answer to give, usually because a member it called gave it none; that `reason` is passed to the member that sent the task; that the runtime tells that member plainly no answer came and names each member called that gave none; and that it is refused while a call is still out and for a task no formation member sent |
| Accepted | `passed`, summary `Ending task without an answer`, `data` `{"status": "ending", "caller": "<caller>"}`, then the note `[end-without-answer] This task ends without an answer once this turn's tool calls finish, and <caller> is told you gave none.` The other tool calls of the same turn still run |
| Policy | The call passes the same `on-tool-call` decision point as every tool call |
| Plans | Not callable from a plan step |

It is refused as a tool error, checked in this order:

| Case | Error text |
|---|---|
| The session is no formation member with a callee | `'end-without-answer' is answered only for a formation member that roster.yaml lets call another; this session is not one` |
| No task is running | `'end-without-answer' is answered only while a task runs; this session is running none` |
| No formation member sent the task, as for the entry member's task | `'end-without-answer' ends only a task another formation member sent; no formation member sent this one. Reply in text, saying plainly which part has no answer.` |
| A call this task made is being sent, is outstanding, or has an answer not yet delivered | `Call <call_id> to <member> is still out; its answer arrives after you end your turn. Call end-without-answer only when no call is left to wait for.` |
| `reason` is missing or blank | `'end-without-answer' needs a reason: say in a sentence why you have no answer.` |
| The task has already accepted one | `This task is already ending without an answer.` |

### How the task ends { #end-without-answer-ending }

Once the turn's tool calls finish, the attempt ends with no further inference call. Under
[`transport: process`](manifest.md#transport-process), an attempt whose harness kept going after
the call and finished ends the same way.

| Surface | What it shows |
|---|---|
| A2A state | `TASK_STATE_FAILED`, with the reason as its status message |
| `GetTask` | [`metadata.murmur.noAnswer: true`](agent-card.md#no-answer-metadata), and `metadata.murmur.noAnswerBelow` naming each member further down this task had no answer from |
| `out/result.txt` | `no answer: <reason>` |
| [`task_end.exit_status`](observability-schemas.md#task-end) and the `on-task-end` exit status | `no_answer` |
| `task_failed` | None is written |
| The caller's call | Ends `no_answer` — see [How the answer arrives](#call-member-answer) |
| The launch | A `no_answer` task never decides `session_end.exit_status` or the launch's exit code |

An `on-task-end` hook may reopen the task; the reopened attempt may answer, or end without an
answer again. A caller that is not a formation member reads an ordinary `TASK_STATE_FAILED` task with a
status message.

A task that answers in text while a member it called gave no answer completes as usual. Its
`GetTask` result carries `metadata.murmur.noAnswerBelow` naming those members, and its caller
reads its answer with a gap line.
