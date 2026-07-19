# Capsules

A **Capsule** is Murmur's primary execution abstraction.

A capsule launches from a manifest, installs its declared artifacts, executes inside a constrained environment, writes outputs, and exits.

## Isolation model

Murmur uses the WASM Component Model as the primary execution boundary:

- WASM components run in a sandboxed runtime
- WASI capabilities explicitly control filesystem and network access
- Manifest capability grants are part of the security policy

For tool artifacts, Murmur supports both:

- **WASM tools** (preferred, stronger isolation)
- **Native tools** (compatibility path, process-level boundary)

## Lifecycle

Typical lifecycle:

1. Launch capsule
2. Read manifest
3. Resolve and install artifacts
4. Execute workload
5. Write outputs/logs
6. Exit

Capsules are designed to be fast-starting and horizontally scalable.

## Why WASM

WASM enables consistent execution contracts and explicit capability boundaries, while still allowing portable packaging and composable interfaces between runtime components.
