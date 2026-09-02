# Tools

For tool artifacts, Murmur supports both:

- **WASM tools** (preferred, stronger isolation)
- **Native tools** (compatibility path, process-level boundary)

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

`share-file`, `fetch-peer-file` and `delegate-task` sit ahead of all five: they are
[runtime-provided tools](../reference/runtime-provided-tools.md), answered by the runtime rather
than by an artifact, and their names are reserved.

Non-zero shell exit codes are data, not errors — only spawn/IO failures set `is_error: true`.
An undeclared tool feeds an error back to the model as a `tool_result` and the session
continues. See [Lock down a capsule's capabilities](../how-to/lock-down-capsule.md) for how the
native/shell subprocess environment is built.
