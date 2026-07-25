# How to give your agent shell access (and lock it down)

By default a Murmur agent capsule cannot run any shell commands. This guide shows how to expose specific binaries to the model as callable tools. It also covers how to control exactly which host environment variables the agent subprocess can see.

??? definition "What are binaries?"
    A **binary** is a standalone executable program on the filesystem that your shell can invoke by name when it appears on the system `PATH`. They differ from shell built-ins (like `cd`) that only exist inside the shell process itself and cannot be called as subprocesses.

    Common examples by category:

    - **Version Control & Repo Management:** `git`, `gh` (GitHub CLI)
    - **Environment & Scripting:** `python3`, `node`
    - **File & Directory Navigation:** `ls`, `pwd`, `find`, `grep`
    - **File Manipulation:** `cat`, `echo`, `rm`, `mv`, `cp`, `chmod`, `chown`
    - **Network Operations:** `curl`, `wget`
    - **System Information:** `whoami`, `uname`, `hostname`

The relevant manifest options are:

| Option | Controls |
|---|---|
| [capabilities.shell.allow](../reference/manifest-schema.md#field-capabilities) | Which binaries the agent may invoke as shell tools |
| [capabilities.shell.strip_env](../reference/manifest-schema.md#field-capabilities) | Glob patterns for host env vars to remove from the subprocess environment |
| [capabilities.shell.baseline_env](../reference/manifest-schema.md#field-capabilities) | Additional host env vars to expose beyond the default baseline |

---

## Step 1 — add shell capabilities to the manifest

Create a `murmur.yaml` file. Add a `capabilities.shell` block listing the binaries you want the agent to use:

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
      shell:
        allow:
          - bash

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
      shell:
        allow:
          - bash

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

Each entry in `allow` is a **bare binary name** — `bash`, `jq`, `python3`. Paths are not accepted. The binary must exist on the host's `PATH`.

---

!!! info "How shell tools appear to the agent"
    At session start the runtime writes a synthetic tool manifest for each listed binary under `workdir/tools/<binary>/murmur.yaml`. The agent discovers these alongside any WASM tool artifacts and can call them by name. A typical call looks like:

    ```
    tool: bash
    input: { "command": "wc -l ./data/report.csv" }
    ```

    A non-zero exit code appears in the result text. Only a spawn failure (binary not found, permission error) surfaces as an error to the model.

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — run and confirm shell tools are available

```bash
mur run
```

Check `workdir/<session_id>/MURMUR.md`. The **Running Shell Commands** section lists the binaries the agent has access to:

```text
## Running Shell Commands

Declared shell binaries: bash
Call them via the shell-execution interface or directly from bash if `bash` is in the allowlist.
```

!!! warning "`bash` + network access is the highest-risk capability combination"
    Pairing `shell.allow: [bash]` with a non-empty `capabilities.network.allow` (or any
    fetch-capable tool) gives a capsule both shell authority and exposure to untrusted external
    content — the combination the runtime's [manifest-schema threat-model
    section](../reference/manifest-schema.md#threat-model) documents as maximum-risk, along with
    the recommended data/action phase-separation pattern for capsules that ingest untrusted
    content.

To verify that the agent actually calls the shell, run with the `--task` flag, passing the task as a string:

```bash
mur run --task 'echo hello from bash'
```

After the session completes, inspect `workdir/<session_id>/trace.jsonl` to confirm the call happened. Each shell invocation produces a `shell` event:

```json
{"event_type":"shell","turn":0,"command":"echo hello from bash","exit_code":0,"stdout_bytes":21,"stderr_bytes":0,"duration_ms":12}
```

---

## Step 4 — manage the subprocess environment

The subprocess does not inherit your full host environment. It starts with a minimal baseline: `PATH`, `HOME`, `USER`, `LANG`, `LC_ALL`, `TMPDIR`, `TEMP`, `TMP`, `CARGO_HOME`, `RUSTUP_HOME`, and `TERM`. Known credential-shaped variables are always stripped before the subprocess spawns, regardless of any other configuration — this includes exact names (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `HUGGING_FACE_HUB_TOKEN`, `NEXUS_API_KEY`, `GITHUB_TOKEN`, `GH_TOKEN`, `KUBECONFIG`, `NPM_TOKEN`, `PYPI_TOKEN`, `CARGO_REGISTRY_TOKEN`) and glob patterns (`AWS_*`, `DOCKER_*`, `*_API_KEY`).

!!! info "HOME is always synthetic"
    `HOME` (and, on Windows, `USERPROFILE`) is never the real host home directory. The runtime always replaces it with a session-scoped directory (`<workdir>/.capsule-home`, created on demand) before the subprocess spawns. Neither `baseline_env` nor any environment override the agent supplies can restore the real host value — the synthetic path always wins.

!!! info "Native tool artifacts get the same treatment"
    Everything on this page — the synthetic `HOME`, the credential-pattern strip list, `strip_env`, and `baseline_env` — also applies to `runtime: tool` artifacts declared with `implementation: native` (see [Python Tool Quickstart (Native)](../language-guides/python-native.md)), even if the capsule declares no `capabilities.shell.allow` entries at all. Both subprocess spawn paths build their environment through the same internal function, so there's no separate configuration surface for native tools.

Use `strip_env` to remove baseline variables the agent doesn't need:

```yaml
capabilities:
  shell:
    allow:
      - bash
    strip_env:
      - CARGO_HOME
      - RUSTUP_HOME
```

Each entry is an exact variable name or a glob with the `*` at the start, the end, or both:

```yaml
strip_env:
  - AWS_*        # matches AWS_DEFAULT_REGION, AWS_PROFILE, and any other AWS_-prefixed var
  - MY_CORP_*    # matches any var starting with MY_CORP_
  - "*_TOKEN"    # matches any var ending in _TOKEN
  - "*SECRET*"   # matches any var with SECRET anywhere in the name
```

---

## Step 5 — add host variables beyond the default baseline

The default baseline only includes variables the runtime considers universally safe. Any host variable outside that fixed set — a database URL, an API endpoint, an application config path — is excluded unless you explicitly add it.

??? info "Default baseline variables"
    | Variable | Purpose |
    |---|---|
    | `PATH` | Locates executables on the host |
    | `HOME` | Session-scoped synthetic directory, not the real host home (see above) |
    | `USER` | Current username |
    | `LANG` | System locale |
    | `LC_ALL` | Locale override for all categories |
    | `TMPDIR` | Preferred temporary directory (macOS/BSD) |
    | `TEMP` | Preferred temporary directory (Windows) |
    | `TMP` | Fallback temporary directory |
    | `CARGO_HOME` | Rust package cache directory |
    | `RUSTUP_HOME` | Rust toolchain installation directory |
    | `TERM` | Terminal type identifier |

Use `baseline_env` to bring additional host variables into the subprocess environment:

```yaml
capabilities:
  shell:
    allow:
      - bash
    baseline_env:
      - DATABASE_URL
      - APP_CONFIG_PATH
```

The runtime builds the final subprocess environment in order: it starts from the default baseline, appends any variables listed in `baseline_env`, then applies `strip_env` removals (chained with the built-in credential patterns), and finally sets `HOME`/`USERPROFILE` to the synthetic session directory. This means `strip_env` always wins over `baseline_env` for everything except `HOME`/`USERPROFILE`, which are fixed last and can't be reintroduced by either setting — a variable that appears in both lists is not passed to the subprocess:

```yaml
capabilities:
  shell:
    allow:
      - bash
    baseline_env:
      - DATABASE_URL
    strip_env:
      - CARGO_HOME
      - RUSTUP_HOME
```

This example adds `DATABASE_URL` from the host and removes `CARGO_HOME` and `RUSTUP_HOME` from the default baseline.

---

## Step 6 — expose additional tools

You can list multiple binaries. Each gets its own entry in the tool inventory:

```yaml
capabilities:
  shell:
    allow:
      - bash
      - jq
      - python3
```

The agent can now call `bash`, `jq`, and `python3` as first-class tools.

---

## Summary

| Manifest setting | Effect |
|---|---|
| `shell.allow: [bash]` | Exposes `bash` as a model-visible tool with a generated description and `command` input schema |
| `shell.allow: [bash, jq, python3]` | Exposes all three binaries; each appears as a separate tool in the inventory |
| `shell.strip_env: [CARGO_HOME]` | Removes a specific variable from the default baseline |
| `shell.strip_env: [AWS_*]` | Removes all vars whose name starts with `AWS_` |
| `shell.strip_env: ["*_TOKEN"]` | Removes all vars whose name ends with `_TOKEN` |
| `shell.baseline_env: [DATABASE_URL]` | Exposes an additional host variable not in the default baseline |
| `HOME` / `USERPROFILE` | Always the synthetic session directory; cannot be overridden by `baseline_env` or a tool-supplied env value |
| Non-zero exit code | Passed back to the model as data; the session continues |
