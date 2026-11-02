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
