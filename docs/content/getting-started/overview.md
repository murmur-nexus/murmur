# Getting Started Overview

Murmur is the infrastructure layer for AI agents. It packages any agent as a single capsule — inference, tools, skills, and configuration all declared in one manifest — and runs it on your own infrastructure, fully isolated, with nothing calling out except what you declared.

## What a capsule declares

A capsule manifest (`murmur.yaml`) is the single source of truth for an agent. It declares:

- Which artifacts (tools, drivers, bootstrap logic) the capsule depends on
- What capabilities are granted (filesystem, network, shell)
- How inference is configured (transport, model, turns)
- What outputs are expected

Nothing runs that isn't declared.

## Why Murmur

Today's agent harnesses shift under you without warning, and every vendor ships its own opinions you have to work around. Murmur places the opposite bet: one manifest as the whole contract, defined by you for any occasion. Batteries included:

- Isolated, WASM-sandboxed capsules
- Secure by default, capability-scoped
- Reproducible, pinned artifacts
- Model-agnostic
- Language-agnostic (WASM or native)
- Composable from versioned artifacts
- OCI-native (deploy to a VM or Kubernetes)

## Design philosophy

Murmur is opinionated about **interfaces and boundaries**, not implementation details.

- The runtime defines standard contracts (manifest shape, capabilities, interfaces)
- Artifacts implement behavior (tooling, provider drivers, bootstrap logic)
- Teams can use defaults first, then swap artifacts without rewriting the runtime

See **Quickstart** for the declare → install → run → inspect loop, and **Language Guides** for language-specific workflows (for example, building tools in Python).
