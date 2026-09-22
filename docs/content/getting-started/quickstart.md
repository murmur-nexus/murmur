# Quickstart

This walkthrough takes you from an empty directory to a running, inspectable agent [capsule](../concepts/capsules.md) using nothing but a manifest. You declare what the capsule depends on and is allowed to do, install those dependencies, hand it a task, run it, and read back exactly what it did. Everything the capsule can reach is declared up front — nothing else is permitted.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest.md#field-artifacts) | Whether a declared artifact is a driver, tool, hook, or skill |
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
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}

    inference:
      transport: http
      model: {{ v.model_anthropic }}
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
        gateway:
          endpoint: https://api.openai.com
          api_key: ${OPENAI_API_KEY}

    inference:
      transport: http
      model: {{ v.model_openai }}
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
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}

    inference:
      transport: http
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur-driver-deepseek
    ```

The driver entry's `gateway.api_key` field reads from the environment at run time. Export your provider key in the shell you will run from:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

This manifest is the entire contract. The capsule reaches its inference provider through the runtime, which holds the API key, and nothing else — no shell, no filesystem, no network destinations, because none are declared.

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

A capsule can drive inference through a harness CLI you are already logged into, spending that login instead of an API key. Set `inference.transport: process`, declare a [process driver](../reference/manifest.md#process-driver) artifact — the component that knows how to drive that one CLI — and declare the variables it needs in `capabilities.env.allow`. There is no `capabilities.network` and no `api_key`: the harness reaches its provider with its own credentials.

This is a secondary path; [`transport: http`](#step-1-declare-the-capsule-in-murmuryaml) remains the primary, fuller-featured way to run a capsule. Before choosing it, read [Run a capsule on your subscription](../how-to/run-capsule-on-subscription.md), which states what this transport costs you.

### Create a manifest

Create a `murmur.yaml` in an empty directory. `murmur-driver-claude-code` drives the Claude CLI:

```yaml
name: my-capsule
version: "1.0.0"
artifacts:
  - name: murmur-driver-claude-code
    version: "{{ v.murmur_driver_claude_code }}"
    runtime: driver
capabilities:
  env:
    allow: [HOME, PATH]   # every variable the driver's describe() requires
inference:
  transport: process
  driver:
    artifact: murmur-driver-claude-code
  max_turns: 10
```

Install the driver before the first run:

```bash
mur install
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

Tool artifacts work under `transport: process` too — declare them exactly as you would for `transport: http`, and `mur trace show` records each tool call. Murmur stands up a loopback tool server, the driver points the harness at it, and the harness's own built-in tools are stripped, so the model is offered the capsule's tools and nothing else. `max_turns` counts one turn per model step here just as it does on `transport: http` — roughly one per tool call plus a final turn — so budget it the same way (a too-low limit fails with [`error[E-RUN-033]`](../reference/diagnostics.md#e-run-033) naming the kind `max-turns`).

!!! note "Observability differs from `transport: http`"

    Because the harness owns the model calls, the per-turn token counts `mur trace show` prints are the harness's own report of what it spent, relayed by the driver, rather than requests Murmur measured. A driver that reports no counts leaves them out of the trace entirely. Turns, tool calls, declared tools, exit status, system prompts and lifecycle hooks work the same on both transports.

Learn more about the `murmur.yaml` manifest in the [Manifest Schema reference](../reference/manifest.md).
