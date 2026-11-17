# Tools

A tool artifact is a WebAssembly component. The model also runs programs through the shell
allowlist. The two differ in where they run and what bounds them:

| Kind | Runs as | Bounded by |
|---|---|---|
| **WASM tools** | A WebAssembly component in the runtime process | Its own artifact entry's `capabilities:`, narrowed below the capsule's, on every platform |
| **Shell commands** | A host subprocess, named in `capabilities.shell.allow` | The capsule-wide grants, enforced by a kernel sandbox; see [Subprocess enforcement tiers](../reference/containment.md#subprocess-enforcement-tiers) |

## Files a WASM tool opens { #wasm-tool-files }

A WASM tool opens files only in the folders opened for it. A relative path always opens in the
work folder. An absolute host path opens only where the operator gave the session more of the
host — the whole host under `mur run --unconfined`, or the folders `host_paths` names — at
`murmur-host` followed by that path, and only when a folder the tool holds covers it. A tool lists
its folders and checks a path before opening it: see
[The folders a WASM tool is handed](../reference/wit-interfaces.md#wasm-tool-folders).

## Programs a WASM tool runs { #wasm-tool-programs }

A WASM tool runs a program through the [`murmur:process`](../reference/wit-interfaces.md#process)
interface its `tool` world imports: `run` runs one to completion, and `start` keeps one running
for the rest of the session, which the tool finds again on later calls, writes to, reads from,
connects to over TCP and stops.

A program runs under exactly what a shell command of the capsule runs under, so the capsule
grants it the same way:

| Grant | Where | What it allows |
|---|---|---|
| The program's name | [`capabilities.shell.allow`](../reference/manifest.md) | Running it at all |
| A containment class | [`capabilities.containment`](../reference/containment.md#wasm-tool-programs) | The tier the program runs at: the session's, in its cgroup scope and network namespace |
| Hosts it reaches | `capabilities.network.allow` | Outbound connections through the session's egress proxy, as for a shell command |

```yaml
capabilities:
  shell:
    allow:
      - python3
```

The `capabilities.filesystem.read_only` check and every `on-shell` hook with
`commit_policy: deny` see the program's argv before it starts. A refused call starts nothing
and returns `refused` naming the rule:

```json
{"rule": "capabilities.shell.allow", "reason": "binary 'ls' is not in capabilities.shell.allow"}
```

`capabilities.shell.allow` is one list for the whole capsule: every WASM tool in it can run every
program the list names, as the model's shell calls can. A session holds at most 8 running
processes, and all of them are killed when it ends. Each tool sees only the processes it started.

A session process lives as long as the program `start` ran: when it exits, whatever it left
running in its process group is killed. Start a server in its foreground mode, not one that
forks into the background and exits.

## Code mode { #code-mode }

Code mode gives the model a REPL artifact, named in the manifest's `code_mode` block, in place of
calling each tool itself. The programs the model writes there call the capsule's operations through
[the bus](../reference/wit-interfaces.md#bus), with the same grants, hooks and records as the
model's own calls, and only what a program prints returns to the model.
[Use code mode in a capsule](../how-to/use-code-mode.md) sets one up; the
[manifest block](../reference/manifest.md#code-mode) and the
[code mode records](../reference/observability-schemas.md#code-mode-records) are in the reference.

A WASM tool runs each call in a fresh instance, so nothing one call leaves in memory reaches the
next. The one artifact a manifest names in [`code_mode.repl`](../reference/manifest.md#code-mode)
keeps a single instance instead, for the rest of the session:

- The instance starts with the first call the model makes to the tool. A session whose model never
  calls it starts none.
- It runs one call at a time. A call that arrives while another runs, from the model or from a plan
  step, is refused with a tool error and starts nothing.
- With `code_mode.detach_after_secs` set, a call from the model that runs that long returns a work
  id while it keeps running. The [`background-command`](../reference/runtime-provided-tools.md#background-command)
  tool reports on it, waits for it or stops it. A plan step's call never moves to the background.

The instance ends in any of these ways, and every value it held is lost:

| Ends when | Next call |
|---|---|
| The session ends; a call still running is stopped | — |
| `background-command stop` names the call running in it | Starts a new instance |
| The tool crashes | Starts a new instance |
| A call runs past [`capabilities.limits.deadline_seconds`](../reference/resource-limits.md) | Starts a new instance |
| The tool grows past a `capabilities.limits` memory or table cap | Starts a new instance |
| A call is canceled before it moved to the background, as a task cancel cancels it | Starts a new instance |
| The worker process that holds it is lost, which ends the session too | — |

A call that ended the instance returns its failure followed by the sentence `The code mode instance
ended; every value it held is lost, and the next call to '<tool>' starts a new instance.` The
instance's start, its end with the reason, and each move to the background are
[trace records](../reference/observability-schemas.md#code-mode-records).

The tool's grants, the `on-tool-call` hooks and the `tool_call` record apply to each call exactly
as to any other tool's.

Under `transport: http`, in a capsule with an `on-tool-call` policy hook or a
[`capabilities.filesystem.read_only`](../reference/manifest.md#read-only-paths) list, a program the
tool starts through [`murmur:process`](../reference/wit-interfaces.md#process) after its call moved
to the background is refused: nothing is left to check it against the hook or the list.

### Calling the capsule's tools from a program { #code-mode-bus }

A code mode tool built against the `code-mode` world imports
[`murmur:bus`](../reference/wit-interfaces.md#bus), through which a program running in its
instance lists the capsule's tools and calls them, one at a time or several at once. One call from
the model can then run many tool calls without a model turn for each.

- `list()` names every tool the model could call, `code_mode.tools_visible` aside, except the code
  mode tool itself, `pull-artifact`, `remove-artifact` and `submit-plan`.
- Each call meets the checks the model's own call to that tool meets, runs through the same
  dispatch and the same fence, and writes the same records, marked with the code mode call it ran
  in. It is put to the `on-tool-call` and `on-shell` hooks as the model's call is.
- The bus answers only while the model's own call to the code mode tool runs in the foreground on
  `inference.transport: http`. A plan step's call, a call made through a `transport: process`
  harness, and a call moved to the background get a refusal for every request.

## Tool dispatch

Each tool call is resolved in a fixed precedence order:

1. The shell allowlist
2. A skill artifact (the body of `tools/<name>/SKILL.md` in the [runtime directory](../reference/workdir.md#runtime-directory), returned directly)
3. A WASM-artifact tool
4. Otherwise, an error

Every [runtime-provided tool](../reference/runtime-provided-tools.md) sits ahead of all
four: it is answered by the runtime rather than by an artifact, and its name is reserved.

Non-zero shell exit codes are data, not errors — only spawn/IO failures set `is_error: true`.
An undeclared tool feeds an error back to the model as a `tool_result` and the session
continues. See [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md) for how the
shell subprocess environment is built.
