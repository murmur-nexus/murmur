# Quickstart

This walkthrough takes you from an empty directory to a running, inspectable agent [capsule](../concepts/capsules.md) using nothing but a manifest. You declare what the capsule depends on and is allowed to do, install those dependencies, hand it a task, run it, and read back exactly what it did. Everything the capsule can reach is declared up front — nothing else is permitted.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest.md#field-artifacts) | Whether a declared artifact is a driver, tool, hook, or skill |
| [capabilities.network.allow](../reference/manifest.md#field-capabilities) | Which hosts the capsule may reach |
| [inference.driver.artifact](../reference/manifest.md#field-inference) | Which driver artifact performs the model calls |
| [inference.model](../reference/manifest.md#field-inference) | Which model the driver calls |

---

## Step 1 — declare the capsule in `murmur.yaml`

Create a `murmur.yaml` file. A minimal agent capsule declares its identity, one driver artifact, the API host it is allowed to reach, and how inference is configured:

=== "Anthropic"

    ```yaml
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

    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: my-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.openai.com

    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: my-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.deepseek.com

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

The `api_key` field reads from the environment at run time. Export your provider key in the shell you will run from:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

This manifest is the entire contract. The capsule can call the one host listed under `capabilities.network.allow` and nothing else — no shell, no filesystem, no other network destinations, because none are declared.

---

## Step 2 — install the declared artifacts

`mur install` reads `murmur.yaml`, resolves every artifact declared in it, and fetches them in parallel into the project-local store. Run it once before the first run:

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

`mur run` verifies that every declared artifact is installed before it starts. Skipping this step makes the next one exit immediately with `error[E-RUN-008]` and an install hint.

---

## Step 3 — write the task

Create a `task.md` file describing what the agent should do. With this minimal manifest the capsule has inference only — no tools — so keep the first task to something the model can answer directly:

```text
Explain what makes an infrastructure deployment reproducible, in three bullet points.
```

---

## Step 4 — run the capsule

Pass the task file with `--task`. `mur run` stages the declared artifacts, copies `task.md` into the capsule's working directory, starts the capsule's local HTTP server, and drives the agent loop to completion:

```text
mur run --task task.md
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

The `session:` value identifies this run. Everything the run produces lands under `workdir/<session_id>/`: the task input (`task.md`), the model's final text (`out/result.txt`), the structured trace (`trace.jsonl`), and runtime logs (`logs/`). The session workdir is the single place to look — read the result directly with:

```bash
cat workdir/*/out/result.txt
```

---

## Step 5 — inspect the run

Every session writes a structured `trace.jsonl` under its workdir. `mur trace show` reads the most recent one and prints a human-readable summary — turns, token usage, tool calls, and exit status:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

The trace is written by the runtime, not the capsule, so it exists after every session and cannot be suppressed or falsified by the agent. It is the authoritative record of what the run actually did.

---

## Summary

| Step | Command | What it does |
|---|---|---|
| Declare | edit `murmur.yaml` | Pins the driver artifact, the allowed host, and the model — the capsule's whole contract |
| Install | `mur install` | Fetches every artifact declared in `murmur.yaml` into the project-local store |
| Run | `mur run --task task.md` | Stages the artifacts, feeds in `task.md`, and drives the agent loop to completion |
| Inspect | `mur trace show` | Prints the runtime-written trace for the most recent session |

Pin every artifact to an exact version and the manifest becomes an execution contract you can audit, roll forward, and roll back. From here, grant more capabilities: [lock down its capabilities](../how-to/lock-down-capsule.md), [shape its behavior with a system prompt](../how-to/capsule-system-prompt.md), or [connect two capsules](../how-to/capsules-a2a-messaging.md).

---

## Want to use a subscription?

Instead of a driver artifact and an API key, a capsule can drive inference through a provider CLI you are already logged into — the Claude CLI (Anthropic) or the Codex CLI (OpenAI). Set `inference.transport: process` and point `command` at the CLI — no driver artifact, no `capabilities.network`, and no API key.

This is a secondary path for spending a subscription rather than an API key; [`transport: http`](#step-1-declare-the-capsule-in-murmuryaml) remains the primary, fuller-featured way to run a capsule.

### Create a manifest

Create a `murmur.yaml` in an empty directory, using the CLI for the subscription you are logged into:

=== "Anthropic"

    ```yaml
    name: my-capsule
    version: "1.0.0"
    inference:
      transport: process
      command: claude
      model: claude-opus-4-8   # optional — omit to use your subscription's default
      max_turns: 10
    ```

=== "OpenAI"

    ```yaml
    name: my-capsule
    version: "1.0.0"
    inference:
      transport: process
      command: codex
      model: gpt-5.5   # optional — omit to use your subscription's default
      max_turns: 10
    ```

### Run the capsule

In the same directory, run the capsule with a task passed inline:

```bash
mur run --task "When I say Ping you say?"
```

Inspect the capsule's output at `workdir/<session_id>/out/result.txt`, for example:

```bash
> cat workdir/*/out/result.txt
Pong! 🏓
```

### Calling tools

Tool artifacts work under `transport: process` too — declare them exactly as you would for `transport: http`, and `mur trace show` records each tool call. `max_turns` counts one turn per model step here just as it does on `transport: http` — roughly one per tool call plus a final turn — so budget it the same way (a too-low limit fails with `error[E-RUN-007]: max_turns exceeded`).

!!! note "Observability differs from `transport: http`"

    Because the CLI owns the model calls, `mur trace show` records turns, tool calls, declared tools, and exit status — but **not** per-turn token usage. When you need full token accounting, use `transport: http`. System prompts and lifecycle hooks apply on both paths.

Learn more about the `murmur.yaml` manifest in the [Manifest Schema reference](../reference/manifest.md).
