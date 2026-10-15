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

## Tool dispatch

Each tool call is resolved in a fixed precedence order:

1. A native binary staged for that artifact
2. The shell allowlist
3. A skill artifact (`workdir/tools/<name>/skill.md`, returned directly)
4. A WASM-artifact tool
5. Otherwise, an error

Every [runtime-provided tool](../reference/runtime-provided-tools.md) sits ahead of all
five: it is answered by the runtime rather than by an artifact, and its name is reserved.

Non-zero shell exit codes are data, not errors — only spawn/IO failures set `is_error: true`.
An undeclared tool feeds an error back to the model as a `tool_result` and the session
continues. See [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md) for how the
native/shell subprocess environment is built.
