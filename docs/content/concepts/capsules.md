# Capsules

A **Capsule** is Murmur's primary execution abstraction.

A capsule launches from a manifest, installs its declared artifacts, executes inside a constrained environment, writes outputs, and exits. The manifest is the complete configuration.

## Agent capsules

An **agent capsule** runs the LLM inference loop built into the runtime. It is defined
entirely by its `murmur.yaml`: an `inference:` block in the manifest is what puts the capsule
in agent mode.

```yaml
# minimal agent manifest
name: my-agent
version: "0.1.0"

artifacts:
  - name: murmur-driver-anthropic
    version: "{{ v.murmur_driver_anthropic }}"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com
  shell:
    allow:
      - bash

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: {{ v.model_anthropic }}
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

`mur build` packages this as a manifest-only `.mur.zip`.

## Isolation model

Murmur uses the WASM Component Model as the primary execution boundary:

- WASM components run in a sandboxed runtime
- WASI capabilities explicitly control filesystem and network access
- Manifest capability grants are part of the security policy

## Why WASM

WASM enables consistent execution contracts and explicit capability boundaries, while still allowing portable packaging and composable interfaces between runtime components.

## Lifecycle

Typical lifecycle:

1. Launch capsule
2. Read manifest
3. Resolve and install artifacts
4. Execute workload
5. Write outputs/logs
6. Exit

Capsules are designed to be fast-starting and horizontally scalable.

Every capsule goes through the same runtime, which turns its manifest into a running process:

- Parses capsule and artifact metadata
- Resolves dependencies from local/remote registry sources
- Configures capability grants (filesystem, network, shell) from the manifest (see
  [Access control](access-control.md))
- Links and invokes runtime interfaces for tools and artifact management
- Captures outputs and logs in a predictable workspace layout

## Execution limits { #execution-limits }

Every component call the runtime makes — a capsule `run`, a tool or driver `run`, and each
hook lifecycle call — is bounded by two independent limits:

| Limit | Bounds | What happens at the limit |
|---|---|---|
| **Deadline** | Wall-clock time for one component call | A call that never returns is stopped with an error instead of hanging the session |
| **Resource limiter** | Memory growth, table growth, and instance count | A call that tries to grow past its cap is stopped with an error instead of consuming host memory without bound |

Both are set per-manifest under `capabilities.limits` (see [Manifest
Schema](../reference/manifest.md#field-capabilities)). Omitting them applies generous
built-in defaults — a manifest that says nothing gets the defaults, not unlimited resources.
The default deadline for a hook lifecycle call is 30 seconds, against 600 for a capsule, tool
or driver call, so one wedged hook cannot stall a session for the capsule-wide budget on every
event; a declared `deadline_seconds` replaces both defaults and applies to every component call,
hooks included. Hitting either limit is reported distinctly from an ordinary crash. On the hook
path it is handled like any other hook error: logged, and the runtime moves on to the next hook.

**Caveat:** the deadline only counts down while the component's own code is running. Time it
spends waiting on the runtime (for example, a driver awaiting a streaming provider response)
counts against the same budget but cannot be cut short until control returns to the
component. The deadline bounds runaway compute, not slow I/O.

## Runtime identity

Every running agent capsule gets a capsule name and version (from `murmur.yaml`), a
runtime-generated session id, and a capsule URL (an OS-assigned local listener port). The same
identity is exposed in [`MURMUR.md`](session-loop.md#murmurmd), in the WASI env for the driver
and WASM tools, and in the shell tool subprocess env — so the agent, its tools, and its hooks
all see one consistent identity for the session.
