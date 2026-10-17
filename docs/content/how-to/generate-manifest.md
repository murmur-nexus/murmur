# How to generate a capsule manifest with mur new

!!! warning "Beta feature"

    `mur new "<task description>"` is a beta feature: it exists only in a `mur` built with the
    `beta-mur-new` feature, and runs only after `mur beta enable mur-new`. Behavior and flags may
    change in future releases. To scaffold a formation instead, use
    [`mur new --roster`](../reference/cli.md#mur-new-roster), which is in every build.

`mur new` takes a plain-language task description and writes a ready-to-refine `murmur.yaml`
to the current directory. It cold-boots a short-lived generator capsule backed by Claude Haiku,
which queries the artifact index to discover relevant tools and produces a manifest tailored
to the task — no schema knowledge required.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [lifecycle.task_acceptance](../reference/manifest.md#lifecycle-task-acceptance) | How the capsule accepts tasks; always `single` in generated manifests |
| [lifecycle.after_task](../reference/manifest.md#lifecycle-after-task) | What happens after the task completes; always `exit` in generated manifests |
| [inference.model](../reference/manifest.md#field-inference) | The model the generated capsule will use; defaults to Claude Haiku |

---

## Step 1 — install the generator artifacts

`mur new` uses two artifacts to run its internal generator capsule. Both must be present in
`~/.murmur/artifacts/` before the first run.

Install the inference driver for your provider:

=== "Anthropic"

    ```bash
    mur install murmur/driver-anthropic@{{ v.murmur_driver_anthropic }}
    ```

    --8<-- "includes/mur-pull-info.md"

=== "OpenAI"

    ```bash
    mur install murmur/driver-openai@{{ v.murmur_driver_openai }}
    ```

    --8<-- "includes/mur-pull-info.md"

=== "DeepSeek"

    ```bash
    mur install murmur/driver-deepseek@{{ v.murmur_driver_deepseek }}
    ```

    --8<-- "includes/mur-pull-info.md"

Install `murmur/tool-registry-search`:

```bash
mur install murmur/tool-registry-search@{{ v.murmur_tool_registry_search }}
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

The generator queries the artifact index, reasons about the task, and writes `murmur.yaml`
to the current directory.

By default the generator searches the index on the latest default-artifacts release. To point it
at another source's index, pass the index's URL:

```bash
mur new "review this PR for security issues" \
  --index https://github.com/your-org/your-artifacts/releases/latest/download/artifacts-index.json
```

The generator capsule fetches that URL itself, and `mur new` adds its host to the generator's
network allow list. To search an index file on disk, pass its path instead; `mur new` checks it
and copies it into the generator's work folder:

```bash
mur new "review this PR for security issues" --index ./artifacts-index.json
```

The generator cannot see the artifacts installed on this machine, so `--index local` is refused
with `E-NEW-005`. To list what is installed, run:

```bash
mur search "" --index local
```

---

## Step 3 — understand the output

The generator picks one of two shapes based on the task description.

**Single capsule** — for focused, bounded tasks (review, classify, summarise):

=== "Anthropic"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}
      - name: murmur/tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      shell:
        allow:
          - git

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}
      - name: murmur/tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      shell:
        allow:
          - git

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: pr-security-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}
      - name: murmur/tool-git
        version: "{{ v.murmur_tool_git }}"
        runtime: tool

    capabilities:
      shell:
        allow:
          - git

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur/driver-deepseek
    ```

**Orchestrator** — for research, multi-step, pipeline, or report tasks. The generator adds a
`spawn` block so the capsule can launch child capsules:

=== "Anthropic"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}

    capabilities:
      spawn:
        allow:
          - local/climate-data-worker

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
    ```

=== "OpenAI"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}

    capabilities:
      spawn:
        allow:
          - local/climate-data-worker

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
    ```

=== "DeepSeek"

    ```yaml
    name: climate-research-agent
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}

    capabilities:
      spawn:
        allow:
          - local/climate-data-worker

    lifecycle:
      task_acceptance: single
      after_task: exit

    inference:
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur/driver-deepseek
    ```

!!! note "Generated manifests are a starting point"
    Artifact versions in the generated manifest reflect what the generator found in the artifact index at
    run time. `mur install` refuses a version that does not fit this `mur` and names the one
    that does; run `mur list` to compare with what is installed.

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
mur run --manifest murmur.yaml --task task.md
```

---

## Summary

| Feature / setting | How it works |
|---|---|
| `mur new "<description>"` | Boots a generator capsule; writes `murmur.yaml` to CWD |
| No `--index` | Searches the index on the latest default-artifacts release |
| `--index <url>` | Searches that index; its host is added to the generator's network allow list |
| `--index <path>` | Checks the index file on the host, then searches a copy in the generator's work folder |
| `--index local` | Refused with `E-NEW-005`; `mur search <query> --index local` lists the installed artifacts |
| Single capsule | Generated for focused, bounded tasks — no `spawn` block |
| Orchestrator | Generated for research and pipeline tasks — adds a `capabilities.spawn.allow` list |
| Validation | Generated YAML is validated before writing; nothing written if validation fails |
| Inference config | Config file (`inference:` in `~/.murmur/config.yaml`) → env var (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) → interactive first-run wizard |
| Generator artifacts | `murmur/driver-anthropic` (or `murmur/driver-openai`), `murmur/tool-registry-search`, `murmur/tool-editor` and `murmur/skill-create-manifest` must all be installed |
| Artifact discovery | `murmur/tool-registry-search` is a native tool and runs in the generator capsule's sandbox; [What each native tool needs](../reference/default-artifacts.md#native-tool-needs) lists what it can reach |
| `inference.model` | Defaults to `claude-haiku-4-5-20251001`; change it in the generated manifest if needed |

---

## See also

- [`mur search`](../reference/cli.md#mur-search) — search the artifact index from the CLI
- **`murmur/tool-registry-search`** — the in-capsule tool the generator uses for artifact discovery
- **`murmur/skill-create-manifest`** — full schema reference skill for authoring manifests manually
- [`mur build`](../reference/cli.md#mur-build) — build the generated capsule into a `.mur.zip`
- [`mur run`](../reference/cli.md#mur-run) — run the capsule locally
