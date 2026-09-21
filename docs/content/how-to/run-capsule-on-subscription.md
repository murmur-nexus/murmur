# How to run a capsule on your subscription

A capsule normally reaches a model over HTTPS with an API key you declare in its manifest. Under `transport: process` it instead drives a harness CLI you are already logged into — the `claude` binary on your machine — so the work is billed to that login rather than to a key. Murmur still owns the task, the tools, the trace and the A2A door; the harness owns the model calls and the conversation.

??? definition "What is a process driver?"
    A **process driver** is a WASM artifact declared with `runtime: driver` that knows how to drive one harness CLI: which executable to run, which arguments to pass, how to read its output, and what its exit codes mean. Murmur's runtime holds none of that — it resolves a binary, moves bytes, and records what the driver says happened.

    A process driver is granted nothing: no environment, no files, no network, and no Murmur host interface. `murmur-driver-claude-code` is the driver for the Claude CLI.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [inference.transport](../reference/manifest.md#field-inference) | Whether the capsule calls a provider API or drives a harness subprocess |
| [inference.driver.artifact](../reference/manifest.md#field-inference) | The process driver artifact that knows this harness |
| [inference.command](../reference/manifest.md#field-inference) | The executable to run instead of the one the driver names |
| [inference.max_turns](../reference/manifest.md#field-inference) | How many model steps one task may take |
| [inference.system_prompt_file](../reference/manifest.md#inference-system-prompt) | The instructions the agent runs under |
| [capabilities.env.allow](../reference/manifest.md#field-capabilities) | Every environment variable the harness process may see |
| [lifecycle.conversation](../reference/manifest.md#lifecycle-conversation) | Whether tasks in one context continue the same harness session |

---

## What this transport costs you

Read this before you choose it. Four things are true here that are not true of `transport: http`.

| Cost | What it means |
|---|---|
| The harness process is not contained | Unlike a WASM artifact, the `claude` process runs on the host with the host network and your real home directory. Murmur controls the environment it starts with and the tools it is offered, not what the process can reach once running. |
| The capsule depends on your machine | It runs on your login and on whichever `claude` version is installed. The manifest pins neither, so the same manifest on two machines is not the same capsule. |
| Spend is invisible to Murmur | The harness reaches its provider with its own credentials, so no request passes through Murmur. [inference.max_session_tokens](../reference/manifest.md#inference-max-session-tokens) is a manifest error here, `spend.machine_tokens_per_day` does not cover this capsule, and every token count in the trace is reported as zero. |
| Your own configuration does not load | The driver launches the harness with `--setting-sources ""`, so your `CLAUDE.md`, your settings and your hooks are not read. The agent's instructions come from the manifest alone. |

A capsule that ran on a subscription and reported zero tokens spent nothing that Murmur can tell you about. If you need token accounting or a spend ceiling, use [`transport: http`](../reference/manifest.md#transport-http).

---

## Step 1 — log in to the Claude CLI

The capsule authenticates as you. Install the CLI and log in once:

```bash
claude login
```

```bash
claude --version
```

The version it prints is the version your capsule runs on — the manifest pins neither it nor your login.

The capsule reads that login out of `~/.claude` under the `HOME` it is given, which is why `HOME` is one of the variables the driver requires. An API key cannot reach the harness through the manifest at all: `capabilities.env.allow` refuses every credential-shaped name with [E-CAP-016](../reference/diagnostics.md#e-cap-016), `ANTHROPIC_API_KEY` among them.

A run whose harness session reports an API key anyway prints [`warning[W-SEC-031]`](../reference/diagnostics.md#w-sec-031) and keeps going, because that spend is billed to a key Murmur neither counts nor limits. On a subscription login it does not appear.

## Step 2 — create murmur.yaml

Create a `murmur.yaml` file. `inference.driver.artifact` is required under this transport, and every variable the driver's `describe()` requires must appear in `capabilities.env.allow`:

```yaml
name: my-capsule
version: "0.1.0"

artifacts:
  - name: murmur-driver-claude-code
    version: "{{ v.murmur_driver_claude_code }}"
    runtime: driver
  - name: murmur-tool-editor
    version: "{{ v.murmur_tool_editor }}"
    runtime: tool

capabilities:
  env:
    allow:
      - HOME
      - PATH

inference:
  transport: process
  max_turns: 100
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur-driver-claude-code

lifecycle:
  conversation: threaded
```

Declare no `endpoint`, no `api_key` and no `capabilities.network`: the harness reaches its provider itself. Leave `inference.model` out to run on whatever model your subscription defaults to.

Create an `instructions.md` file next to it with the agent's instructions. That file is the whole of what the agent is told about its job.

## Step 3 — install the artifacts

```bash
mur install
```

```
--8<-- "includes/mur-pull-info.md"
```

The driver is resolved like any other artifact. `mur run` then checks it three times before the harness starts, and refuses the launch on the first failure:

| Check | Refused with |
|---|---|
| The artifact exports the process driver interface | [`E-RUN-029`](../reference/diagnostics.md#e-run-029) |
| It loads, and its `describe()` returns usable variable names | [`E-RUN-032`](../reference/diagnostics.md#e-run-032) |
| Every variable `describe()` requires is declared in `capabilities.env.allow` | [`E-CAP-019`](../reference/diagnostics.md#e-cap-019) |

## Step 4 — run a task

Create a `task.md` file with the work you want done, then run the capsule:

```bash
mur run --task task.md
```

```
mur run --task task.md
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

The harness writes its answer to `out/result.txt` in the session workdir, exactly as `transport: http` does.

## Step 5 — give the agent tools

The harness's own built-in tools are stripped. Murmur stands up a loopback tool server for the run, hands the driver its address and the capsule's tool names, and the driver points the harness at it — so the model is offered the capsule's tools and nothing else, and Murmur executes every call it makes.

Two kinds of entry reach the model this way:

| Declared as | Reaches the model as |
|---|---|
| A `runtime: tool` artifact, such as `murmur-tool-editor` | One tool per artifact, under its own name |
| A binary in [capabilities.shell.allow](../reference/manifest.md#field-capabilities) | One tool per binary, running in the capsule workdir |

`runtime: driver` and `runtime: hook` artifacts are not offered to the model, on this transport as on the other.

Watch what the harness actually called:

```bash
mur trace show
```

```
--8<-- "includes/mur-trace-show-info.md"
```

```
--8<-- "includes/mur-trace-explore.md"
```

The session's `harness_start` record names the driver, the harness, the absolute path of the binary spawned, the version it reported, and the capsule tool names the bridge offered. It records no argument values: a driver is free to put the tool server's bearer token in one.

## Step 6 — continue a conversation

`lifecycle.conversation: threaded` maps each A2A context to one harness session. The first task in a context starts a session; every later task in that context resumes it, and the harness — not Murmur — carries the history. Murmur writes no `conversation.jsonl` here.

The mapping lives in a `harness-session.json` file under `~/.murmur/conversations/`. [`mur run --resume`](../reference/cli.md#mur-run) continues from it. If the harness has lost that conversation, the next task fails with [`E-RUN-036`](../reference/diagnostics.md#e-run-036) and the file is left exactly as it was, rather than answering from nothing.

Because the harness holds the history, `--resume-mode compact` has nothing to compact and refuses with [`E-RUN-037`](../reference/diagnostics.md#e-run-037). `context.max_tokens` and `inference.compaction` parse but do nothing: the harness manages its own context.

## Step 7 — know the two fixed limits

Two timeouts bound a run, and neither is a manifest setting.

| Limit | Value | What it bounds |
|---|---|---|
| Inactivity | 600 seconds | How long a run may go with neither a line of harness output nor a tool call before the harness is killed with [`E-RUN-035`](../reference/diagnostics.md#e-run-035). Any output and any tool call start the window again, so a harness that is working is never interrupted for taking a long time. |
| Interrupt grace | 10 seconds | How long a harness that was asked to stop has to end on its own before it is killed. |

---

## A full manifest

A coding capsule with tool artifacts, a hook and a shell allowlist on this transport:

```yaml
name: my-coding-capsule
version: "1.0.0"

artifacts:
  - name: murmur-driver-claude-code
    runtime: driver
    version: "{{ v.murmur_driver_claude_code }}"
  - name: murmur-tool-editor
    runtime: tool
    version: "{{ v.murmur_tool_editor }}"
  - name: murmur-tool-code-graph
    runtime: tool
    version: "{{ v.murmur_tool_code_graph }}"
  - name: murmur-hook-diff-summary
    runtime: hook
    version: "{{ v.murmur_hook_diff_summary }}"
    capabilities:
      filesystem:
        scope: "."
  - name: murmur-tool-corpus
    runtime: tool
    version: "{{ v.murmur_tool_corpus }}"
    capabilities:
      state: {}
    config:
      config_version: 1
      read_recent: { default: 10, max: 50 }
      search: { default_k: 5, max_k: 25 }
      prefix_map: { session-note: snt }
      types:
        note:
          schema_version: 1
          schema:
            type: object
            required: [text]
            properties:
              text: { type: string }
              tags: { type: array, items: { type: string } }
            additionalProperties: false

capabilities:
  env:
    allow:
      - HOME
      - PATH
  shell:
    allow:
      - bash
      - git
      - cat
      - ls
      - head
      - tail
      - wc
      - grep
      - find
      - sed
      - diff
      - cp
      - mv
      - rm
      - mkdir
      - python3
    interpreter_runtime:
      - binary: python3
        dirs:
          - path: /usr/lib/python3.12
            list_dir: true
          - path: /usr/lib/python3.12/lib-dynload
            list_dir: false

inference:
  transport: process
  max_turns: 100
  system_prompt_file: ./instructions.md
  driver:
    artifact: murmur-driver-claude-code

lifecycle:
  task_acceptance: queue
  queue_depth: 8
  after_task: sleep
  shell_grace_secs: 600
  conversation: threaded
```

It declares no compaction hook and no `context.max_tokens`: both are inert under this transport, and a manifest that named them would promise a mechanism that never runs.

---

## Summary

| Manifest setting | Effect |
|---|---|
| `inference.transport: process` | Drives a harness CLI as a subprocess instead of calling a provider API |
| `inference.driver.artifact` | Required — names the process driver for that harness |
| `inference.command` | Overrides the executable the driver's `describe()` names |
| `capabilities.env.allow` | The whole environment the harness sees; every variable the driver requires must appear, or the launch is refused with `E-CAP-019` |
| `capabilities.shell.allow` | Each binary becomes one tool the model may call through Murmur |
| `lifecycle.conversation: threaded` | Each A2A context resumes one harness session; the harness holds the history |
| `inference.max_session_tokens` | A manifest error — Murmur sees no spend on this transport |
| `context.max_tokens`, `inference.compaction` | Parse but do nothing; the harness manages its own context |
| Inactivity limit, 600 seconds | Fixed; a silent harness is killed with `E-RUN-035` |
| Interrupt grace, 10 seconds | Fixed; a harness asked to stop is killed after it |
| Token counts in the trace | Always zero; the subprocess protocol does not carry them |
