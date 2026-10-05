# Runtime-provided tools

A runtime-provided tool is an entry in the agent's tool inventory with no artifact behind it. The
runtime writes its `murmur.yaml` into `workdir/tools/<name>/` while the session is staged, and
answers the call itself.

The model sees no difference. A runtime-provided tool has the same manifest shape, appears in the
same inventory, and is called the same way as a tool artifact. What differs is where it comes from
and what decides whether it may run.

## The seven

Each one appears only when the declaration in the second column is present.

| Tool | Gated on | Does |
|---|---|---|
| One per shell binary | [`capabilities.shell.allow`](manifest.md#shell-allow) | Runs that binary as a subprocess in the accessible workdir |
| `share-file` | [`exports.peer_files`](manifest.md#field-exports-peer-files) | Mints a [peer-file handle](resource-plane.md#peer-plane) for one file under the declared export root |
| `fetch-peer-file` | [`capabilities.peer_fetch`](manifest.md#field-peer-fetch) | Redeems a handle a peer sent and stores the file in this capsule's workdir |
| `delegate-task` | [`capabilities.spawn.allow`](manifest.md#field-capabilities) | Hands one task to one sub-capsule and returns as soon as it is running and holding it; once the turn ends, the task waits and continues with the outcome — see [The delegation tool](roost-api.md#the-delegation-tool) |
| `submit-plan` | [`capabilities.plan.submit`](manifest.md#field-capabilities) | Runs one plan of steps against this session's own tools and returns every step's result — see [Plans](plans.md) |
| `switch-driver` | [`control.agent_settings: [inference.driver]`](manifest.md#field-control) | Selects the [driver choice](manifest.md#inference-alternates) the agent's next inference call is served by — see [`switch-driver`](#switch-driver) |
| `call-member` | A [`reachability`](roster.md#reachability) rule in the formation's `roster.yaml` that lets this member call another | Hands one task to another formation member and returns as soon as that member holds it; the answer arrives later in the same task — see [`call-member`](#call-member) |

Every one of their manifests carries `version: 0.0.0`, `runtime: tool` and
`implementation: native`. Nothing was fetched, so nothing is version-pinned, nothing is
hash-verified, and no entry appears for them in `murmur.lock` or in `mur list`.

## The grant is the tool's existence

`share-file`, `fetch-peer-file`, `delegate-task`, `submit-plan`, `switch-driver` and `call-member`
are answered before the tool allowlist is consulted. The allowlist governs which tool *artifacts*
may run; it has no say over these six.

**The gate is whether the manifest file was written at all.** With the grant absent, staging writes
nothing under `workdir/tools/<name>/`, so:

- The tool is absent from the inventory the model is sent.
- It is absent from `session_start`'s `tools_declared` in [`trace.jsonl`](observability-schemas.md).
- A call naming it anyway is refused with a message naming the declaration that is missing.

So `capabilities.spawn.allow` decides whether `delegate-task` exists, rather than whether a call to
it succeeds. The same holds for the two peer-handoff tools, for `submit-plan`, for
`switch-driver`, for `call-member`, and for their grants.

`capabilities.plan.submit` also decides whether the model is told *when* to plan: the runtime's
plan guidance is part of the system prompt exactly when the tool exists.

Shell binaries are gated the same way — one manifest per name in `capabilities.shell.allow` — and
are additionally checked against that list again at dispatch.

## Reserved names

`share-file`, `fetch-peer-file`, `delegate-task`, `submit-plan`, `switch-driver` and `call-member`
are reserved.
A capsule declaring an artifact under one of them is refused at staging, before any artifact is
pulled, with [`E-CAP-013`](diagnostics.md#e-cap-013). The same refusal covers an in-session
`manage.pull()` of that name.

A name is reserved whether or not the capsule declares the grant that would provide the tool, so
adding or removing a capability never changes whether a manifest is accepted.

Shell binary names are not reserved. They come from `capabilities.shell.allow`, which is the
operator's own list, so there is no fixed set to reserve — and a capsule may declare both an
artifact named `bash` and `bash` in its shell allowlist. Staging writes the artifact's manifest
first and the shell manifest yields to it, so the inventory describes the artifact.

Dispatch resolves such a pair by [precedence](../concepts/tools.md#tool-dispatch): a native
artifact answers ahead of the shell binary, and the shell binary answers ahead of a WASM artifact
of the same name.

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

## `call-member` { #call-member }

`call-member` exists for a member of a formation launched by
[`mur run --roster`](cli.md#mur-run-roster) when `roster.yaml` lets it call at least one other
member. A member the roster lets call nobody, and a session in no formation, has no such tool.

| Aspect | Behaviour |
|---|---|
| Input | `{"member": "<name>", "task": "<text>"}`, both required. The schema's `member` is an `enum` of the members this one may call, in roster order |
| Description | Tells the model that `task` is the whole of what the member is told, so any file content it needs goes in the task text; that the call returns once the member holds the task; that the answer arrives in the conversation after the turn ends, so it should not wait or poll; and that a second call to a member before its answer has arrived is refused |
| Sent | One `message/send` to the member's door, carrying `task` as one text part, the formation token, and the calling task's trust class with origin `peer`. The model never sees the door's address or the token |
| Egress | The member's door is checked against this capsule's own [`capabilities.network.allow`](manifest.md#network-allow-entries), which must list `localhost` — see [Giving a member work](roster.md#member-calls). Without it, every call fails and staging prints [`W-RUN-008`](diagnostics.md#w-run-008) |
| Trace | [`member_call_start`](observability-schemas.md#member-call-start) when the member holds the task, and one [`member_call`](observability-schemas.md#member-call) per call once it is accounted for |

### What a call returns { #call-member-result }

A call ends its tool call in one of two ways:

| Result | When | `data` |
|---|---|---|
| `passed`, summary `Called <member>: started` | The member's door answered with a task id and a state that is not terminal | `{"call_id": "mcl_…", "member": "<name>", "status": "started", "task_id": "<the member's task id>"}` |
| `failed`, summary `Called <member>: failed` | The call was refused, the door could not be reached, the door answered an error status or a JSON-RPC error, or the member answered with a terminal state such as `rejected` | `{"call_id": "mcl_…", "member": "<name>", "status": "failed", "output": "<why>"}` |

`data` reaches the model [fenced](untrusted-fence.md) under `tool:call-member`. A started call adds the runtime's own note on the line after the closing
marker, outside the fence:

```text
<untrusted-content source=tool:call-member>
{"call_id":"mcl_01a1…","member":"worker","status":"started","task_id":"tsk_…"}
</untrusted-content>
[call-member] worker is now working on call mcl_01a1…. Its answer is not in this result and no tool fetches it: the runtime adds it to this conversation after you end your turn. Unless you still have work to hand to a different member, end your turn now by replying without calling a tool. Calling worker again before its answer arrives is refused.
```

Three calls are refused as a tool error, with nothing sent and nothing recorded as a member call:

| Call | Error |
|---|---|
| A `member` the roster does not let this capsule call | Names the members it may call |
| A call made while no task is running | Says the tool is answered only while a task runs |
| A `member` that already holds a call from this task whose answer has not been delivered | One of the two texts below, naming the earlier call |

| The earlier call's answer | Error text |
|---|---|
| Has not arrived | `<member> has not yet answered call <call_id> from this task, so nothing was sent. Its answer reaches you only after you end your turn: end your turn now by replying without calling a tool. Once that answer has arrived you may call <member> again with new work.` |
| Has arrived and waits for the turn to end | `<member> has already answered call <call_id>, so nothing was sent. The answer reaches you as soon as you end your turn: end your turn now by replying without calling a tool.` |

Once its answer has been delivered, a member can be called again with new work. Calls to
different members in one turn never refuse each other.

### How the answer arrives { #call-member-answer }

1. The model ends its turn with a turn still left.
2. If no answer has arrived yet, the task's stream shows a `working` status
   `waiting on call-member: <n> call(s) outstanding`, and the task waits.
3. Once at least one answer has arrived, the task continues with one new user message holding
   every answer that has arrived. The stream shows `continuing with <n> member answer(s)`.
4. The model reads the answers and answers in turn, or calls again.

Each answer in that message is one line naming the call id, the member and how the call ended,
then the member's output fenced under `member:<name>`, whatever the calling task's trust. The
message ends with one line of the runtime's own, outside every fence:

```text
[call-member] call mcl_01a1… to worker ended completed:
<untrusted-content source=member:worker>
WORKER-0123
</untrusted-content>

[call-member] Every call this task made has ended, and the answers are above. Answer the task with them now; call a member again only to give it new work.
```

| Calls still outstanding | Last line |
|---|---|
| None | `[call-member] Every call this task made has ended, and the answers are above. Answer the task with them now; call a member again only to give it new work.` |
| One or more | `[call-member] Still working: <member> (call <call_id>), <member> (call <call_id>). Their answers arrive after you end your turn; do not call them again before then.` |

| Ending | Output |
|---|---|
| `completed` | The member's answer: its task's `response` artifact |
| `failed`, `canceled`, `rejected` | The member's task's status message |
| `timed_out` | The member did not answer within the bound; it was not cancelled and may still be working |
| `unreachable` | The member's door stopped answering |

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

A task cancelled while it waits ends `canceled`. A task with `delegate-task` delegations
outstanding as well waits for both kinds together and continues with whatever has arrived of
either, answers first — see [How the outcome arrives](roost-api.md#how-the-outcome-arrives).

| Bound | Value |
|---|---|
| How long a call is watched | [`lifecycle.delegation_deadline_secs`](manifest.md#lifecycle-delegation-deadline-secs), default 600 seconds, or `MURMUR_DELEGATION_TIMEOUT_SECS` — the bound delegations use |
| How often the member's task is read | Every 500 ms, through the member's `tasks/get` |
| Unreachable | Two reads in a row that get no answer |

No call is ever cancelled at the member: a formation token cannot call `tasks/cancel`. A call
that times out or is abandoned leaves the member's task running until it finishes or the formation
ends.

