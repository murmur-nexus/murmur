# murmur

![Version](https://img.shields.io/github/v/release/murmur-nexus/murmur?style=for-the-badge&color=0000FF)
![GitHub License](https://img.shields.io/github/license/murmur-nexus/murmur?style=for-the-badge&color=00F0D0)


**The infrastructure layer for AI agents. Package any agent once and run it anywhere, reproducibly and fully isolated.**

![](https://murmur-static.s3.eu-north-1.amazonaws.com/assets/mur-header-github-v2.png)

[Getting Started](#getting-started) · [How it works](#how-it-works) · [Why Murmur?](#why-murmur) · [Documentation](https://docs.murmur.nexus) · [Website](https://murmur.rs)

Murmur packages your agent as a single capsule, with its inference, tools, skills, and configuration all declared in one manifest. It runs on your own infrastructure, with nothing calling out except what you declared. And because everything is explicit, you get a lot for free: you can read exactly what the agent does, pin it so every run behaves the same, share it so your whole team runs an identical environment, and trust that it can never reach beyond what you granted.

<br><p align="center">·</p>

## Getting started

### Install

MacOS (Apple Silicon / Intel) and Linux x86_64
```bash
curl -fsSL https://install.murmur.rs | sh
```

Or with Cargo
```bash
cargo install murmur-cli
```

## How it works

![](https://murmur-static.s3.eu-north-1.amazonaws.com/assets/mur-how-it-works-github.png)

### Create a manifest

Declare a capsule in `murmur.yaml` — its driver, the host it may reach, and how inference runs.

`murmur.yaml`
```yaml
name: my-capsule
version: "1.0.0"

artifacts:
  - name: murmur-driver-anthropic
    version: "1.0.0"
    runtime: driver

capabilities:
  network:
    allow:
      - https://api.anthropic.com

inference:
  transport: http
  endpoint: https://api.anthropic.com
  model: claude-haiku-4-5
  api_key: ${ANTHROPIC_API_KEY}
  driver:
    artifact: murmur-driver-anthropic
```

### Run the capsule

In the same directory, fetch the declared driver and run the capsule with a task:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
mur install
mur run --task "When I say Ping you say?"
```

Inspect the final output at `workdir/<session_id>/out/result.txt`, for example:

```bash
> cat workdir/*/out/result.txt
Pong! 🏓
```

Learn how to [use a subscription instead](https://docs.murmur.nexus/getting-started/quickstart/#want-to-use-a-subscription), or explore every option of the `murmur.yaml` manifest in our [Manifest Schema documentation](https://docs.murmur.nexus/reference/manifest-schema/).

---

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

[Read more >](https://murmur.nexus/blog/the-agent-capsule-runtime)

## Contributing

Murmur grows through improving the runtime, building new artifacts and putting capsules to the test by teams that depend on it.

- Sign off your commits (`git commit -s`) — a quick [DCO 1.1](https://developercertificate.org/) certification. A bot will verify.
- Changes touching sandbox, capability, or network-execution code get extra scrutiny before merge.
- Read [CONTRIBUTING.md](./CONTRIBUTING.md) and join the community on [Discord](https://discord.gg/Y45yJv5rrC).

<br>
<p align="center">
  <img src="https://murmur-static.s3.eu-north-1.amazonaws.com/assets/murmur-logo-ascii-animated2.gif" alt="" width="100%">
</p>

## License

Murmur is open source under the `Apache License 2.0`. 