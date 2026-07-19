# How to generate a capsule manifest with mur new

!!! warning "Beta feature"

    `mur new` is a beta feature and is hidden by default. Enable it first with
    `mur beta enable mur-new`. Behavior and flags may change in future releases.

`mur new` takes a plain-language task description and writes a ready-to-refine `murmur.yaml`
to the current directory. It cold-boots a short-lived generator capsule backed by Claude Haiku,
which queries the artifact registry to discover relevant tools and produces a manifest tailored
to the task — no schema knowledge required.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest-schema.md#lifecycle-task-acceptance) | How the capsule accepts tasks; always `single` in generated manifests |
| [lifecycle.after_task](../reference/manifest-schema.md#lifecycle-after-task) | What happens after the task completes; always `exit` in generated manifests |
| [inference.model](../reference/manifest-schema.md#field-inference) | The model the generated capsule will use; defaults to Claude Haiku |

---

## Step 1 — install the generator artifacts

`mur new` uses two artifacts to run its internal generator capsule. Both must be present in
`~/.murmur/artifacts/` before the first run.

Install the inference driver for your provider:

=== "Anthropic"

    ```bash
    mur install murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}
    ```

    --8<-- "includes/mur-pull-info.md"

=== "OpenAI"

    ```bash
    mur install murmur-driver-openai@{{ v.murmur_driver_openai }}
    ```

    --8<-- "includes/mur-pull-info.md"

=== "DeepSeek"

    ```bash
    mur install murmur-driver-deepseek@{{ v.murmur_driver_deepseek }}
    ```

    --8<-- "includes/mur-pull-info.md"

Install `murmur-tool-registry-search`:

```bash
mur install murmur-tool-registry-search@{{ v.murmur_tool_registry_search }}
```

--8<-- "includes/mur-pull-info.md"

`mur new` checks for both artifacts before starting any capsule and exits with a clear error and
an `mur install` hint if either is missing.

Configure your inference provider — `mur new` detects it automatically using the following
priority order:

1. **Config file** (`~/.murmur/config.yaml`, `inference:` section) — recommended for repeated use:

    ```yaml
    inference:
      provider: anthropic          # or "openai"
      model: claude-haiku-4-5-20251001
      api_key: sk-ant-...
      endpoint: ""                 # leave empty to use the provider default
    ```

2. **Environment variable** — `ANTHROPIC_API_KEY` or `OPENAI_API_KEY`. When detected, `mur new`
   prints a one-line hint suggesting you add `inference:` to `~/.murmur/config.yaml` instead.

3. **First-run wizard** — if neither the config file nor an env var is present, `mur new` launches
   an interactive wizard that prompts for provider, model, and API key, then offers to save the
   result to `~/.murmur/config.yaml` for future runs.

   The wizard requires an interactive terminal. In non-interactive contexts (CI, piped stdin), it
   exits with error `E-CFG-001` and a message describing how to configure inference.

**Registry token** — by default `mur new` queries the remote artifact index hosted on GitHub.
Set `MURMUR_REGISTRY_TOKEN` to a GitHub personal access token with repository read scope so
the request is authenticated:

```bash
export MURMUR_REGISTRY_TOKEN=ghp_...
```

The token is injected automatically when the registry URL hostname is `raw.githubusercontent.com`
or `api.github.com`. For public repos it is optional — unauthenticated requests are served
without a token. For private repos it is required.

If you point `mur new` at a custom registry URL on a private GitHub repo (see Step 2), the
same `MURMUR_REGISTRY_TOKEN` value is reused. Registries on other hostnames are never given the
token automatically.

---

## Step 2 — run mur new

Run `mur new` with a plain-language description of what the capsule should do:

```bash
mur new "review this PR for security issues"
```

Output:

```text
mur new: generating manifest...
murmur.yaml written to /path/to/current-dir/murmur.yaml
```

The generator queries the artifact registry, reasons about the task, and writes `murmur.yaml`
to the current directory.

To restrict artifact discovery to versions already installed in `~/.murmur/artifacts/` — which
guarantees the generated manifest references versions you can run immediately — pass
`--registry local`:

```bash
mur new "review this PR for security issues" --registry local
```

To point the generator at a custom artifact index — for example, a private GitHub-hosted
`artifacts-index.json` — pass its raw URL:

```bash
mur new "review this PR for security issues" \
  --registry https://raw.githubusercontent.com/your-org/your-artifacts/main/artifacts-index.json
```

When the URL hostname is `raw.githubusercontent.com` or `api.github.com`, `mur new`
automatically attaches `MURMUR_REGISTRY_TOKEN` as a bearer token. Set the variable before
running if the repo is private (see Step 1).

---

## Step 3 — understand the output

The generator picks one of two shapes based on the task description.

**Single capsule** — for focused, bounded tasks (review, classify, summarise):

=== "Anthropic"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
      - name: murmur-tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.anthropic.com

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
      - name: murmur-tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.openai.com

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
      - name: murmur-tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.deepseek.com

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

**Orchestrator** — for research, multi-step, pipeline, or report tasks. The generator adds a
`spawn` block so the capsule can launch child capsules:

=== "Anthropic"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.anthropic.com
      spawn:
        scoped: true

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.openai.com
      spawn:
        scoped: true

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

    capabilities:
      network:
        allow:
          - https://api.deepseek.com
      spawn:
        scoped: true

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek
    ```

!!! note "Generated manifests are a starting point"
    Artifact versions in the generated manifest reflect what the generator found in the registry at
    run time. Re-run with `--registry local` to ensure versions match what is installed, or run
    `mur list` to compare.

---

## Step 4 — refine the manifest

Open `murmur.yaml` and check these fields before running:

**Model** — the generator always uses Claude Haiku. Change `inference.model` if the task needs
a more capable model:

=== "Anthropic"

    ```yaml
    inference:
      model: {{ v.model_anthropic }}
    ```

=== "OpenAI"

    ```yaml
    inference:
      model: {{ v.model_openai }}
    ```

=== "DeepSeek"

    ```yaml
    inference:
      model: {{ v.model_deepseek }}
    ```

**System prompt** — the generator does not add `inference.system_prompt`. Add one to give the
capsule role-specific context:

```yaml
inference:
  system_prompt: |
    You are a security-focused code reviewer. Focus on OWASP Top 10 vulnerabilities,
    insecure defaults, and missing input validation.
```

**Capabilities** — add or remove `capabilities.network.allow` entries to match what the capsule
actually needs to access. Remove entries for hosts the capsule will never contact.

---

## Step 5 — install manifest dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 6 — run the capsule

Create a `task.md` file with the task description for the capsule, then run it:

```bash
mur run --manifest murmur.yaml --input task.md
```

---

## Summary

| Feature / setting | How it works |
|---|---|
| `mur new "<description>"` | Boots a generator capsule; writes `murmur.yaml` to CWD |
| `--registry local` | Restricts artifact discovery to `~/.murmur/artifacts/`; ensures generated versions are installed |
| `--registry <url>` | Queries a custom artifact index; GitHub URLs receive the bearer token automatically |
| `MURMUR_REGISTRY_TOKEN` | GitHub PAT injected as `Authorization: Bearer` on `raw.githubusercontent.com` and `api.github.com` requests; required for private repos, optional for public |
| Single capsule | Generated for focused, bounded tasks — no `spawn` block |
| Orchestrator | Generated for research and pipeline tasks — adds `spawn.scoped: true` |
| Validation | Generated YAML is validated before writing; nothing written if validation fails |
| Inference config | Config file (`inference:` in `~/.murmur/config.yaml`) → env var (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) → interactive first-run wizard |
| Generator artifacts | `murmur-driver-anthropic` (or `murmur-driver-openai`) and `murmur-tool-registry-search` must both be installed |
| `inference.model` | Defaults to `claude-haiku-4-5-20251001`; change it in the generated manifest if needed |

---

## See also

- [`mur search`](../reference/cli.md#mur-search) — search the artifact registry interactively from the CLI
- **`murmur-tool-registry-search`** — the in-capsule tool the generator uses for artifact discovery
- **`murmur-skill-create-manifest`** — full schema reference skill for authoring manifests manually
- [`mur build`](../reference/cli.md#mur-build) — build the generated capsule into a `.mur.zip`
- [`mur run`](../reference/cli.md#mur-run) — run the capsule locally
