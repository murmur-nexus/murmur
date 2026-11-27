# How to use code mode in a capsule

In code mode the model writes a short program instead of calling tools one by one. The program runs
in a REPL artifact whose state lasts for the session. It calls the capsule's tools through
[`murmur:bus`](../reference/wit-interfaces.md#bus), several at once if it likes, and prints only
what the model needs. One model turn can then do the work of many, and the full tool results stay
in the program instead of the model's context. This guide gives a capsule code mode with the
`tool-repl-js` artifact, a JavaScript REPL, and reads what one session did in its trace.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts](../reference/manifest.md#field-artifacts) | Which REPL artifact the capsule installs |
| [code_mode.repl](../reference/manifest.md#code-mode) | Which artifact keeps one instance for the session and is linked to the bus |
| [code_mode.detach_after_secs](../reference/manifest.md#code-mode) | How long a call from the model runs before it moves to the background |
| [code_mode.tools_visible](../reference/manifest.md#code-mode) | Whether the model is offered the capsule's other tools beside the REPL |
| [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | Which host programs the capsule offers as tools and a program may run |
| [capabilities.session_status.read](../reference/manifest.md#capabilities-session-status) | Whether the capsule has the runtime-provided `session-status` tool |

---

## Step 1 — write the manifest

Create a `murmur.yaml` file:

```yaml
name: code-mode-demo
version: "0.1.0"

artifacts:
  - name: murmur/driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver
    gateway:
      endpoint: https://api.anthropic.com
      api_key: ${ANTHROPIC_API_KEY}
  - name: murmur/tool-repl-js
    version: "{{ v.murmur_tool_repl_js }}"
    runtime: tool

capabilities:
  shell:
    allow:
      - echo
      - uname
  session_status:
    read: true

inference:
  transport: http
  model: {{ v.model_anthropic }}
  driver:
    artifact: murmur/driver-anthropic

code_mode:
  repl: murmur/tool-repl-js
  detach_after_secs: 10
  tools_visible: false
```

What each `code_mode` key does here:

| Key | Value | Effect |
|---|---|---|
| `repl` | `murmur/tool-repl-js` | The model's first call to `tool-repl-js` starts one instance of it, which runs every later call until the session ends. Its programs reach the capsule's tools through the bus |
| `detach_after_secs` | `10` | A call from the model still running after 10 seconds moves to the background, and the `background-command` tool is offered |
| `tools_visible` | `false` | The model is offered `tool-repl-js` and `background-command` only. `echo`, `uname` and `session-status` stay callable from a program |

The bus answers only on `inference.transport: http`. A capsule that lists `tool-repl-js` without a
`code_mode` block naming it is refused at launch with
[`E-CAP-022`](../reference/diagnostics.md#e-cap-022).

## Step 2 — install the artifacts

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

## Step 3 — run a task

Create a `task.md` file:

```markdown
Greet the operator with echo, report this session's status, and name the host's kernel.
Use tool-repl-js for all of it.
```

Run it:

```bash
mur run --task task.md
```

```text
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

## Step 4 — what a program can call

The model's input to `tool-repl-js` is `{"code": "<JavaScript source>"}`. Every call runs in the
same JavaScript context, so a variable one program defines is there for the next. These globals
reach outside it:

| Global | Does | Returns |
|---|---|---|
| `tools()` | Lists every operation a program may call: the capsule's tools, `tools_visible` aside, without `tool-repl-js`, `pull-artifact`, `remove-artifact` and `submit-plan` | `[{name, description, inputSchema}]` |
| `call(name, input)` | Calls one operation | `{status, summary, data, dataPath, truncated, metadata, attachments}`; throws when the call is refused or fails |
| `callAll([[name, input], ...])` | Calls several operations at once | One entry per pair, in order: the result, or the error `call` would throw |
| `run(program, args, options)` | Runs a program named in `capabilities.shell.allow` through [`murmur:process`](../reference/wit-interfaces.md#process) | `{exitCode, timedOut, stdout, stdoutTruncated, stderr, stderrTruncated}` |
| `print(...values)` | Appends the values to the call's text | `undefined` |

Each bus call passes the same grants and `on-tool-call` hooks as the model's own call to that tool,
and writes the same `tool_call` record. The model gets back only what the program prints, followed
by `=> ` and the value of its last expression when that is not `undefined`.

This program calls `echo` and `session-status` in one batch and prints a summary of both:

```js
const [said, status] = callAll([
  ["echo", { command: "hello from the bus" }],
  ["session-status", {}],
]);
print(said.status, status.status, said.data.includes("hello from the bus"));
```

The model receives one line:

```text
passed passed true
```

The artifact's
[README](https://github.com/murmur-nexus/default-artifacts/tree/main/tools/tool-repl-js) holds
each global's errors, how values print, and its one config key, `output_limit_bytes`.

## Step 5 — when a call moves to the background

A call from the model that runs longer than `detach_after_secs` returns a work id, such as
`wrk_019ed2b0c41e7d3a9f5b6c7d8e9f0a1b`, and keeps running.
[`background-command`](../reference/runtime-provided-tools.md#background-command) reports on it,
waits for it or stops it by that id; a `wait` returns what the program printed once it finishes.

| While the call is in the background | Result |
|---|---|
| The program calls the bus | Refused: the bus answers only while the model's call is in the foreground |
| The model calls `tool-repl-js` again | Refused with a tool error: the instance runs one call at a time |
| `background-command stop` names it | The instance ends and every value it held is lost; the next call starts a new one |
| The session ends | The call is stopped |

A plan step's call to `tool-repl-js` never moves to the background, and its program's bus calls
are refused.

## Step 6 — read the trace

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

Under `── Tool calls ──`, each bus call is a `↳` row under the `tool-repl-js` call it ran in. These
are the rows for the batch program from Step 4, in the order its calls finished:

```text
  turn 2  tool-repl-js 45ms ✓  {"code":"const [said, status] = callAll([\n  [\"echo\", { command: \"hello from the bus\" }],\n  [\"session-status\", {}…
      ↳ session-status 0ms ✓  {}
      ↳ echo 37ms ✓  {"command":"hello from the bus"}
```

The `count:` line ends with `code mode: 2`, the number of bus calls in the session. The trace also
records the instance's start and end and each move to the background, as
`code_mode_instance_start`, `code_mode_instance_end` and `code_mode_detach`, which
`mur trace steps` lists; a program's `run` writes `process_start` and `process_exit`. Their fields
are in [Code mode records](../reference/observability-schemas.md#code-mode-records).

## Summary

| Feature / setting | How it works |
|---|---|
| `code_mode.repl` | Names the REPL artifact; one instance runs every call from the model's first until the session ends |
| `tools_visible: false` | The model sees `tool-repl-js` and `background-command`; a program still calls every other tool |
| The bus | `tools()`, `call` and `callAll` reach the capsule's tools with the model's grants, hooks and records |
| `run` | Runs a `capabilities.shell.allow` program and returns its exit code and output |
| What the model reads | Only what the program prints, and its last expression's value |
| `code_mode.detach_after_secs` | A longer call returns a work id for `background-command`; its bus calls are refused from then on |
| The trace | Each bus call is a `↳` row under its `tool-repl-js` call in `mur trace show` |
