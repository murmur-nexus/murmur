# How to run a capsule from the CLI or from another program

`mur run` is the only way to start a capsule, but it has two output modes and they serve different callers. Interactively you want a session ID and a status line you can read. From a script, a CI job, or a supervising service you want the capsule's URL on stdout the moment it is reachable, so the caller can start sending it work. This guide covers both, plus the two read-only checks worth running before either.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest.md#lifecycle-task-acceptance) | Whether the capsule runs `task.md` once or stays up to accept messages |
| [network.internal_port](../reference/manifest.md#field-network) | A fixed port for the capsule's HTTP endpoint instead of an OS-assigned one |
| [capabilities.containment](../reference/containment.md#field-containment) | The minimum kernel enforcement the host must provide before the capsule launches |

---

## Step 1 — write the manifest

Create a `murmur.yaml` file:

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

Create a `task.md` file next to it with the work you want done:

```markdown
Summarise what this repository does in three sentences.
```

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — run it from the CLI

```bash
mur run --task task.md
```

```text
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

The first two lines are printed the moment the capsule's HTTP port is bound, before the agent loop starts. The `status:` line is printed after the session ends.

| Line | Meaning |
|---|---|
| `murmur: url` | Where the capsule is reachable while it runs |
| `session:` | The session ID — names the workdir subdirectory and appears in `trace.jsonl` |
| `status:` | `ok`, `failed`, or `trapped` |

`--task` also accepts inline text. If the value is the path to an existing file its contents are copied to `task.md`; anything else is written to `task.md` verbatim:

```bash
mur run --task "Summarise what this repository does in three sentences."
```

Add `-v` when you need the paths and the resolved identity:

```bash
mur run --task task.md -v
```

```text
murmur: url localhost:52222
session: ses_019ed2af76b57c42912b612f07dd4d51
workdir: /path/to/workdir/ses_019ed2af76b57c42912b612f07dd4d51
manifest: my-agent v0.1.0
driver: murmur-driver-anthropic (claude-sonnet-4-6)
skills: 2 installed
status:  ok
```

The `driver:` line names the artifact and model for `transport: http`, or the command and model for `transport: process`. The `skills:` line appears only when the manifest declares at least one `runtime: skill` artifact.

---

## Step 4 — run it from another program

Pass `--json` to get a single JSON line on stdout instead of the human-readable lines. It is written as soon as the HTTP server is listening, so a caller can block on that one line and then start sending work:

```bash
mur run --json --task task.md
```

```json
{"url":"localhost:52222","pid":12345,"session_id":"ses_019ed2af76b57c42912b612f07dd4d51","name":"my-agent","version":"0.1.0","workdir":"/path/to/workdir/ses_019ed2af76b57c42912b612f07dd4d51"}
```

| Field | Type | Description |
|---|---|---|
| `url` | string | `localhost:PORT` — the capsule's HTTP endpoint |
| `pid` | number | The `mur` process ID, for waiting on or killing the run |
| `session_id` | string | Matches the `session_id` field in the session's `trace.jsonl` |
| `name` | string | Capsule name from the manifest |
| `version` | string | Capsule version from the manifest |
| `workdir` | string | Absolute path to the directory the agent can read and write |

`--json` takes precedence over `-v`: when both are set, no human-readable output is produced at all.

If the run fails before the port is bound — a bad manifest path, a missing artifact — stdout stays empty. The error goes to stderr and the exit code is non-zero, so a caller that reads one line from stdout can treat "no line" as a launch failure.

Connect with retry and backoff. A small window exists between the port being bound and the server accepting the first request.

### Send the capsule work

A capsule that runs `task.md` and exits needs nothing further. To keep it up and drive it over HTTP, set [`lifecycle.task_acceptance`](../reference/manifest.md#lifecycle-task-acceptance) to `single` or `queue` and read the port out of the JSON line:

```bash
PORT=$(mur run --json --task task.md | head -n 1 | sed 's/.*localhost:\([0-9]*\).*/\1/')
```

The capsule describes itself at `/.well-known/agent-card.json`:

```bash
curl -s http://localhost:$PORT/.well-known/agent-card.json
```

Send it a message over JSON-RPC:

```bash
curl -s -X POST http://localhost:$PORT \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "message/send",
    "params": {
      "message": {
        "messageId": "msg-001",
        "role": "user",
        "parts": [{"text": "Summarise the README."}]
      }
    }
  }'
```

See [Connect two capsules with A2A messaging](capsules-a2a-messaging.md) for the full request and response shapes.

Two flags matter when something other than a local script is the caller:

| Flag | Use it when |
|---|---|
| `--bind 0.0.0.0` | The caller is on another machine. The printed `url` stays `localhost:PORT` — substitute the host address yourself |
| `--no-env-file` | Running in CI. It skips auto-loading the workspace-root `.env`, so secrets come only from the environment you injected |

For a port that does not change between runs, declare [`network.internal_port`](../reference/manifest.md#field-network) in the manifest. The runtime then binds that exact port and fails with `error[E-RUN-010]` if it is already taken, instead of picking a free one.

---

## Step 5 — inspect the capsule's reach before launching it

`--explain-scope` resolves the capsule's effective grants and prints them, then exits `0`. It contacts no registry, compiles no component and creates no workdir, so it is safe to run against any manifest:

```bash
mur run --explain-scope
```

```text
Containment
  declared:  advisory
  achieved:  scoped
  floor met: yes
  mechanism: landlock+seccomp
  userns grant: profile_confining

Effective grants
  filesystem scope: <none>
  workdir exec:     false
  read_only: <none>
  preopens:
    - murmur-tool-git (tool): the whole accessible workdir — no capabilities.filesystem.scope declared
    - murmur-driver-anthropic (driver): the whole accessible workdir — no capabilities.filesystem.scope declared
    - murmur-hook-telemetry (hook): nothing preopened — no capabilities.filesystem.scope declared
  network allow:
    - https://api.anthropic.com
  unix sockets:     false
  shell allow: <none>
  spawn allow: <none>
  env allow: <none>
  interpreter runtime: <none>
  staged runtime: <none>
```

The report shows *declared* network destinations, not resolved IP addresses — it deliberately skips DNS to stay fast and read-only.

`preopens:` lists one line per tool, driver and hook: which directory that artifact works out of once its own `capabilities.filesystem.scope` is applied. A tool or driver that declares no scope gets the whole accessible workdir; a hook that declares none gets no directory at all. See [The filesystem default](../concepts/access-control.md#filesystem-default) for why the two roles start from opposite baselines and when to narrow one.

`userns grant:` names where this host's permission to create an unprivileged user namespace comes
from, and is `n/a` off Linux. See
[Where the user namespace comes from](../reference/containment.md#userns-grant) for the values.

`mechanism:` is a stable name for the kernel enforcement the host actually provides:

| `mechanism` | Achieved class | What the host provides |
|---|---|---|
| `mountns+pivot_root+landlock+seccomp` | `sealed` | Private mount namespace pivoted onto a composed root, with Landlock and seccomp inside it |
| `landlock+seccomp` | `scoped` | Landlock filesystem mediation plus the seccomp syscall allowlist over the host filesystem |
| `seccomp-only` | `advisory` | Linux without a usable Landlock ABI: seccomp only, filesystem scope by convention |
| `none` | `advisory` | No kernel sandboxing primitive (macOS and every non-Linux target) |

Add `--json` for one machine-readable line instead, which is the same object `trace.jsonl` records as `effective_grants` on every session:

```bash
mur run --explain-scope --json
```

Because it is a diagnostic, it reports even when the capsule could not launch here — which is exactly the case you are inspecting. When the floor is not met it says so and exits `0` anyway:

```text
This is a report only — `mur run` without --explain-scope would refuse to launch here.
```

---

## Step 6 — require a containment floor

A containment class is a floor: "do not launch me unless the host can enforce at least this much". Declare it in the manifest when the capsule should never run unprotected:

```yaml
capabilities:
  containment: scoped
```

Or require it for one invocation:

```bash
mur run --task task.md --containment scoped
```

Three sources can each declare a floor — the manifest, `containment` in `.murmur/config.yaml`, and the flag — and they combine by taking the **strongest**. The flag can raise a floor the manifest set, never lower it.

`mur run` then probes the kernel directly rather than trusting the manifest. If the host falls short, the run is refused before any registry pull, component compile, or workdir creation:

```text
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): scoped requires Landlock filesystem mediation (Linux 5.13+ with a usable Landlock ABI); this host provides no kernel filesystem mediation, so paths outside the workdir are constrained by convention only
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

A manifest that declares nothing is never gated by this check — the effective floor is `advisory`, which every host satisfies. See [Containment](../reference/containment.md) for what each class enforces and [`E-CAP-003`](../reference/diagnostics.md#e-cap-003) for the other refusal reasons.

---

## Summary

| Feature / setting | How it works |
|---|---|
| `mur run --task task.md` | Human-readable: URL and session ID at port bind, `status:` when the session ends |
| `--task <text>` | An argument that is not an existing file path is written to `task.md` verbatim |
| `mur run -v` | Adds `workdir:`, `manifest:`, `driver:`, and `skills:` to the startup lines |
| `mur run --json` | One JSON line at port bind carrying `url`, `pid`, `session_id`, `name`, `version`, `workdir` |
| `--json` with `-v` | `--json` wins; no human-readable output is produced |
| Launch failure with `--json` | Empty stdout, error on stderr, non-zero exit |
| `--bind 0.0.0.0` | Accepts connections from other machines; the printed `url` still reads `localhost:PORT` |
| `--no-env-file` | Skips the workspace-root `.env`; the recommended default in CI |
| `network.internal_port` | Binds one fixed port; `error[E-RUN-010]` when it is already taken |
| `mur run --explain-scope` | Prints declared and achieved containment plus every effective grant, then exits `0` without staging anything |
| `--explain-scope --json` | The same report as one line, identical to `effective_grants` in `trace.jsonl` |
| `capabilities.containment` / `--containment` | Strongest of manifest, workspace config, and flag; a host that falls short refuses with `E-CAP-003` |
