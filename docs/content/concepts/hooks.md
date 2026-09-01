# Hooks

A hook is an artifact that observes a lifecycle point, receives structured input from the
runtime, and optionally returns output that the runtime commits. Hooks are not tools: the
model cannot call them, they are omitted from the tool inventory, and hook errors are
non-fatal — failures are logged to `workdir/logs/hook-<name>.log` and the session continues.

**Binding** — when the hook fires:

| Value | When |
|---|---|
| `on-stage` | During `stage_session`, before the agent loop begins. Once per launch. |
| `on-session-start` | After staging, before the first task's work begins. Once per capsule launch. |
| `on-task-start` | Before that task's first inference turn. Once per task. |
| `on-inference` | After each inference response is received. |
| `on-tool-call` | Before each tool invocation is dispatched, and again after it returns. |
| `on-shell` | Before each shell command is dispatched, and again after it returns. |
| `on-compaction` | When session tokens reach the [compaction threshold](context.md). |
| `on-task-end` | Immediately after that task's agent loop returns. Once per task. |
| `on-session-end` | After the task loop exits (idle timeout, shutdown, or explicit exit). Once per capsule launch. |
| *(omitted)* | All session events (`on-session-start` through `on-session-end`). Does not include `on-stage`. |

**Execution mode** — whether the agent loop waits for the hook. **`blocking`** (the default) stops
the loop until the hook returns; **`async`** enqueues the event and lets the loop continue. If the
loop needs the hook's answer, use `blocking`; otherwise `async` keeps the work off the critical
path — which matters for a hook doing network I/O on every event, since a `blocking` one pays that
round trip inline every time. An async hook is one reused instance per session, so in-memory state
persists across calls; its calls are serialized in dispatch order and finish before the session
ends. See [Async hook execution](../reference/manifest.md#hook-overflow) for overflow rules.

**Commit policy** — what the runtime does with the hook's output.

| `commit_policy` | What the runtime does | Typical use |
|---|---|---|
| `none` | Discards the output | Observability hooks |
| `replace-context` | Replaces the conversation history | Compaction |
| `write-manifests` | Writes tool manifest records to `workdir/tools/<binary>/murmur.yaml`, overwriting any existing file | Shell tool enrichment during staging |
| `reopen-task` | Re-runs the task's agent loop with the hook's feedback instead of finalizing it — see [Task reopening](session-loop.md#task-reopening-commit_policy-reopen-task) | Review and retry hooks |
| `seed-context` | Places the hook's messages at the head of the task's first message list, under the [`context.seed_budget`](../reference/manifest.md#field-context) ceiling — see [Context seeding](session-loop.md#context-seeding-commit_policy-seed-context) | Memory |
| `deny` | Refuses the shell command or tool call the hook was asked about, before it runs — see [Policy hooks](#policy-hooks) | Guardrails |

`binding` is the single source of truth for what a hook commits: each binding honors exactly one
arm, so it admits that one policy plus `none`, and a `commit_policy` the binding cannot honor fails
at capsule-staging time — before the hook component is compiled — with an error naming the binding,
the declared policy, and the policy the binding honors. A hook with no `binding:` receives every
event, so any policy is valid for it — with the single exception of `deny`, which requires an
explicit `binding:` of `on-shell` or `on-tool-call`. See [Hook contract
fields](../reference/manifest.md#hook-contract-fields).

The two fields meet in one rule: a binding that commits an arm must be blocking, because every
committable arm is a decision the agent loop is blocked on — the context it continues from, the
manifests it stages with, the feedback that reopens a task. `async` is available exactly where a
hook commits nothing, so it requires `commit_policy: none`, and `on-stage` may never be `async`.

Of the shipped hooks, `murmur-hook-debug` is the only `async` one, and it is stateless — it appends
one JSON line per event and nothing waits on it. `murmur-hook-grafana` and `murmur-hook-eval`
declare no `execution_mode` and so run `blocking` even though both commit nothing: each buffers
session state in memory across every call and exports it once at `on-session-end`.

```yaml
name: murmur-hook-example
version: 1.0.0
runtime: hook
binding: on-compaction           # when it fires
execution_mode: blocking         # blocking or async
commit_policy: replace-context   # none, replace-context, write-manifests, reopen-task, seed-context, deny
description: "Hook description."
```

A hook's capability grant is read from the capsule's manifest, not from the hook's own — see
[Hook capabilities](access-control.md#hook-capabilities).

## Policy hooks { #policy-hooks }

A hook declaring `commit_policy: deny` on `binding: on-shell` or `binding: on-tool-call` is a
**policy hook**. Every other hook is an observer.

| | Observer hook | Policy hook |
|---|---|---|
| When it is called | After the call, with what the call produced | Before the call, with what the call is about to do |
| `outcome` on the event | Present | Absent |
| What its answer changes | Nothing about the call — it has already run | Whether the call happens at all |
| A failure means | The session continues, the failure is logged | The call is refused |

A policy hook is called at a decision point placed after the manifest's capability check and
immediately before the call is dispatched. It is handed the resolved identity of what is about to
run: the resolved absolute path of the executable, the exact untruncated argument list, the `-c`
script body, and for a tool call the exact input JSON. Returning `deny(reason)` means the call is
not made. The agent is handed a result naming the hook and its reason, and stating that retrying
the same call unchanged will be refused again. A [`call_denied`](../reference/observability-schemas.md#call-denied)
event records the refusal; no `tool_call` or `shell` event is written, because nothing ran.

**A policy hook only narrows.** There is no arm that permits a call. The manifest decided what the
capsule may do, and a policy hook is a second, stricter gate standing in front of that decision —
so a hook can never make a capsule able to do something its manifest did not allow. A shell binary
missing from `capabilities.shell.allow` is still refused with the same message whatever the hook
returns.

**A policy hook that fails refuses.** This is the inversion of the non-fatal default that governs
every other hook, and it is the rule a policy hook exists to provide: a gate that opens when it
breaks is not a gate. The call proceeds only on a clean `none`. A returned `deny`, a crash, a
call that outruns its deadline, a memory-limit kill, a return the runtime cannot read, a `deny`
with an empty reason, and any other `hook-output` arm all refuse the call, with a reason naming
the hook and the defect. Nothing in the manifest, the environment or the CLI changes this.

The inversion applies at the decision point alone. The observation dispatch of the same two
events keeps the non-fatal default: a `deny` returned there is a dispatch fault, logged and traced
and honored by nothing, because the call has already happened.

A policy decides on `argv`, `script` and `input`, never on `command` — `command` is truncated for
display. For `make <target>`, `just <recipe>` and `npm run <script>` the runtime reads the named
recipe out of the capsule's workdir and carries its body on `shell-event.recipe`, so a policy
gating `just build` decides on the text the justfile gives `build`. An absent `recipe` means the
runtime resolved no body, and says nothing about the call.
