# Murmur

**The infrastructure layer for AI agents. Package any agent once and run it anywhere, reproducibly and fully isolated.**

## What is murmur?

Murmur packages your agent as a single capsule, with its inference, tools, skills, and configuration all declared in one manifest. It runs on your own infrastructure, with nothing calling out except what you declared. And because everything is explicit, you get a lot for free: you can read exactly what the agent does, pin it so every run behaves the same, share it so your whole team runs an identical environment, and trust that it can never reach beyond what you granted.

## Why Murmur?

Today's agent harnesses shift under you without warning, and every vendor ships its own opinions you have to work around. Murmur places the opposite bet: one manifest as the whole contract, defined by you for any occasion. It's built from the ground up to be portable, reproducible, and isolated, so it runs the same anywhere you send it.

We strongly believe that if you own the knowledge domain, you should own the harness too. Murmur lets teams generate a custom harness for any use case, with batteries already included:

- Isolated, WASM-sandboxed capsules
- Secure by default, capability-scoped
- Reproducible, pinned artifacts
- Model-agnostic
- Language-agnostic (WASM or native)
- Composable from versioned artifacts
- OCI-native (deploy to a VM or Kubernetes)

## Documentation

**Getting Started** covers project orientation, setup, and your first run.

**Concepts** explains core architecture: capsules, the registry, the runtime, and the agent bootstrap.

**How-to Guides** covers common tasks and practical workflows.

**Reference** contains generated interfaces and schemas.

**Contributing** describes the slice-based development workflow for Murmur.
