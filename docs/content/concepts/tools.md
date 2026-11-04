# Tools

For tool artifacts, Murmur supports both:

| Kind | Runs as | Bounded by |
|---|---|---|
| **WASM tools** (preferred) | A WebAssembly component in the runtime process | Its own artifact entry's `capabilities:`, narrowed below the capsule's, on every platform |
| **Native tools** | A host subprocess | The capsule-wide grants, enforced by the same kernel sandbox as a shell command; see [Native tools](../reference/containment.md#native-tools) |

Tools reach the runtime through one of two invocation paths behind a single agent-facing
contract: a **WASM path** (tool exposed through typed WIT interfaces) and a **native path**
(tool launched as a subprocess and normalized into the same result envelope). The goal is
consistent agent behavior regardless of tool implementation language.

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
| A containment class | [`capabilities.containment`](../reference/containment.md#wasm-tool-programs) | Nothing extra: the program runs at the session's tier, in its cgroup scope and network namespace |
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

A session holds at most 8 running processes, and all of them are killed when it ends. Each tool
sees only the processes it started.

## Tool dispatch

Each tool call is resolved in a fixed precedence order:

1. A native binary staged for that artifact
2. The shell allowlist
3. A skill artifact (`tools/<name>/skill.md` in the [runtime directory](../reference/workdir.md#runtime-directory), returned directly)
4. A WASM-artifact tool
5. Otherwise, an error

Every [runtime-provided tool](../reference/runtime-provided-tools.md) sits ahead of all
five: it is answered by the runtime rather than by an artifact, and its name is reserved.

Non-zero shell exit codes are data, not errors — only spawn/IO failures set `is_error: true`.
An undeclared tool feeds an error back to the model as a `tool_result` and the session
continues. See [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md) for how the
native/shell subprocess environment is built.
