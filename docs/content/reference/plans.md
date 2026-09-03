# Plans

A plan is one JSON object describing a set of steps and the order they may run in. A capsule
granted [`capabilities.plan.submit`](manifest.md#field-capabilities) gains the
[runtime-provided tool](runtime-provided-tools.md) `submit-plan`, which takes one plan, runs every
step, and returns one report.

The call returns when every step has settled. `shell` and `capsule` steps that do not depend on
each other run at the same time; `tool` steps are dispatched one at a time even when the plan
places no dependency between them.

## Shape

```json
{
  "id": "refresh-fixtures",
  "steps": [
    {"id": "list", "tool": "repo-files", "input": {"glob": "tests/**"}},
    {"id": "check", "shell": "bash -c 'cargo test --quiet'", "depends_on": ["list"]},
    {"id": "note", "tool": "notes", "if": "$check.status == 'failed'", "input": {"body": "$check.output"}}
  ]
}
```

| Field | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | yes | Names the plan in the report and in [`trace.jsonl`](observability-schemas.md). It never names a file: the runtime writes the plan to `plans/plan-<n>.json` in the [session workdir](workdir.md), numbered per session |
| `steps` | list<step> | yes | Runs in dependency order. An empty list is a plan that completes having done nothing |

### Step

| Field | Type | Required | Notes |
|---|---|---:|---|
| `id` | string | yes | Unique within the plan. Other steps name it in `depends_on` and in `$<id>.output` / `$<id>.status` references |
| `tool` | string | see notes | A tool in this capsule's inventory. Exactly one of `tool`, `shell` or `capsule` per step |
| `shell` | string | see notes | A command line whose first word is a binary in [`capabilities.shell.allow`](manifest.md#shell-allow). Split into words by the runtime, not by a shell — quote a pipeline as an argument (`bash -c '…'`) |
| `capsule` | string | see notes | A capsule name in [`capabilities.spawn.allow`](manifest.md#field-capabilities), run as a sub-capsule through `mur-roost` — see [Roost API](roost-api.md). The version is `0.1.0` |
| `input` | object \| string | no | Handed to the step. A tool step receives it as its JSON input; a capsule step's input is the task text and must be a string or `{"objective": "<text>"}`; a shell step ignores it |
| `depends_on` | list<string> | no | Step ids that must settle before this one is dispatched. Omitted, the step is ready immediately |
| `if` | string | no | Condition deciding whether the step runs — see [Conditions](#conditions). A false condition settles the step as `skipped` without dispatching it |
| `on_error` | `fail` \| `skip` \| `continue` | no | Default: `fail`. What a failed step does to the rest of the plan — see [Failure](#failure) |
| `retries` | integer | no | Default: `0`. Extra dispatches after a failure. `retries: 2` dispatches the step up to three times |

## References

A step reads an earlier step's result with `$<step id>.output` or `$<step id>.status`.

| Reference | Resolves to |
|---|---|
| `$build.output` | The step's output text |
| `$build.status` | `success`, `failed` or `skipped` |

A reference is substituted wherever it is the whole of a string inside `input`, at any depth, and
inside `if`. Substitution happens when the step is dispatched, so the value is the upstream step's
real result:

```json
{"id": "report", "tool": "notes", "depends_on": ["build"], "input": {"log": "$build.output"}}
```

A reference to a step that has not settled fails the referring step. Declaring the dependency is
what orders them.

## Conditions

`if` is an expression over references and quoted literals.

| Operator | Meaning |
|---|---|
| `==` | Equal |
| `!=` | Not equal |
| `>` | Greater than, comparing text |
| `<` | Less than, comparing text |
| `&&` | Both |
| `\|\|` | Either |

Literals are single- or double-quoted (`'failed'`, `"ok"`). An operator inside quotes is part of
the literal.

```json
{"id": "notify", "tool": "notes", "if": "$test.status == 'failed' || $lint.status == 'failed'"}
```

## Failure

A step fails when its tool returns a failing result, its command exits non-zero, its sub-capsule
does not complete, or a reference it names cannot be resolved. `on_error` decides what that does to
the plan:

| `on_error` | The step settles as | The rest of the plan |
|---|---|---|
| `fail` | `failed` | Stops. Steps that had not been dispatched are absent from the report |
| `skip` | `skipped` | Continues |
| `continue` | `failed` | Continues |

`retries` is applied first: a step with retries left is dispatched again, and `on_error` applies
only once the last attempt has failed.

## What is refused before anything runs

The plan is validated as a whole before the first step is dispatched, so a plan with any of these
runs nothing at all:

- Two steps with the same `id`.
- A step declaring none of `tool`, `shell`, `capsule`, or more than one of them.
- A `tool` step naming a tool that is not in the capsule's inventory.
- A `tool` step naming `submit-plan`. A plan cannot submit a plan.
- A `shell` step whose binary is absent from `capabilities.shell.allow`.
- A `capsule` step whose `input` is neither a string nor `{"objective": "<text>"}`.
- A `depends_on` entry, or a `$<id>` reference, naming a step the plan does not declare.
- A reference field other than `output` or `status`.
- An `on_error` value other than `fail`, `skip` or `continue`.
- A dependency cycle, which is reported when the scheduler runs out of ready steps.

Which capsules a `capsule` step may spawn is refereed by `mur-roost` against the session's
registered grant, not checked here.

## The report

`submit-plan` returns one JSON object.

| Field | Type | Notes |
|---|---|---|
| `plan_id` | string | The plan's own `id` |
| `completed` | bool | `true` when every step settled without stopping the plan |
| `failed_step` | string \| null | The step that stopped the plan, `null` when none did |
| `steps` | list | One entry per settled step, in the order they settled |
| `steps[].step_id` | string | |
| `steps[].status` | `success` \| `failed` \| `skipped` | |
| `steps[].output` | string \| null | The step's output text, `null` when it produced none |
| `steps[].error` | string \| null | Why the step failed, `null` otherwise |

A plan that did not complete comes back as a failed tool result, so the model is told the outcome
rather than having to read the body for it.

## What a plan may reach

Every step runs inside the session that submitted the plan and under that session's own grants: a
tool step goes through the same dispatch an agent-loop tool call does, a shell step runs the same
allowlisted binaries in the same accessible workdir, and a capsule step presents the same
registration. A plan reaches exactly what the model could already call one turn at a time.

Every `tool` and `shell` step also passes the same two refusals a direct call passes, before it is
dispatched:

| Refusal | Applies to |
|---|---|
| [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) | A step the runtime identifies as writing a declared read-only path |
| A [hook](manifest.md#hook-contract-fields) bound to `on-tool-call` or `on-shell` with `commit_policy: deny` | Every `tool` step and every `shell` step respectively |

A refused step settles as `failed`, carrying the refusal as its `error`, and nothing it would have
run happens. The refusal is recorded as the `protected_path_denied` or `call_denied` line a direct
call's refusal is recorded as. Steps that had already run stay run: a plan is refused a step at a
time, not as a whole.

A `capsule` step is not put to either check. It runs nothing in this session, and the sub-capsule
it delegates to applies its own manifest to whatever it does.

A plan whose steps can start a subprocess needs a delegated cgroup v2 scope on Linux, on the same
terms a launch does: without one the launch is refused with `E-RUN-012` before the plan is ever
submitted. See [Platform behavior](resource-limits.md#platform-behavior).

## In the trace

A plan run writes `plan_start`, one `plan_step_start` and one `plan_step` per dispatched step, and
`plan_end` to [`trace.jsonl`](observability-schemas.md), under the session that ran it. `mur trace
steps` renders them as rows and `mur trace show` gives them their own section.
