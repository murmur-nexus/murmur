# How to shape agent behavior with a system prompt

A system prompt tells the model who it is and how it should behave — before any task arrives. In Murmur, the system prompt is injected as the `system` parameter on **every** inference call, not just the first turn, so the agent's persona and constraints are reinforced throughout a multi-turn session. This guide covers the two ways to set a system prompt and when to choose each.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [inference.system_prompt](../reference/manifest.md#inference-system-prompt) | Inline text injected as the system prompt on every turn |
| [inference.system_prompt_file](../reference/manifest.md#inference-system-prompt) | Path to a file whose content is used as the system prompt |
| [inference.system_prompt_artifact](../reference/manifest.md#inference-system-prompt) | Name of a skill artifact whose `skill.md` is read at launch and used as the system prompt |

---

## Step 1 — add an inline system prompt

Create a `murmur.yaml` file with `inference.system_prompt` set to the text you want the model to receive:

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
      system_prompt: |
        You are a concise data analyst. Always respond with:
        1. A one-sentence summary of what you found.
        2. A bullet list of key numbers.
        Never include preamble or closing remarks.
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
      system_prompt: |
        You are a concise data analyst. Always respond with:
        1. A one-sentence summary of what you found.
        2. A bullet list of key numbers.
        Never include preamble or closing remarks.
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
      system_prompt: |
        You are a concise data analyst. Always respond with:
        1. A one-sentence summary of what you found.
        2. A bullet list of key numbers.
        Never include preamble or closing remarks.
    ```

The `|` block scalar preserves newlines. The runtime passes this text verbatim as the `system` field on every POST to the inference driver — including turn 2, turn 3, and beyond.

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — load the prompt from a file

For longer prompts, or when you want to version the prompt independently from the manifest, use `system_prompt_file`:

```yaml
inference:
  ...
  system_prompt_file: conventions.md
```

The path is **relative to the directory containing `murmur.yaml`**, not to the session workdir. A typical layout:

```text
my-capsule/
  murmur.yaml
  conventions.md        ← system prompt lives here
  murmur.lock
```

The runtime reads the file once at launch, before any inference call. If the file is missing or unreadable, `mur run` exits with `error[E-RUN-009]` before the session starts — you will never silently run a session with the wrong prompt.

`conventions.md`:

```markdown
You are a strict code reviewer operating in a CI pipeline.

Rules:
- Only flag issues that violate the project's style guide.
- Output one finding per line in the format: `FILE:LINE — REASON`.
- If there are no issues, output exactly: `LGTM`.
- Do not explain, justify, or suggest alternatives.
```

---

## Step 4 — load the prompt from a skill artifact

If your system prompt is already packaged as a [skill artifact](package-skill-artifact.md), you can bind it directly instead of duplicating the content in a separate file. Set `inference.system_prompt_artifact` to the name of the skill:

```yaml
artifacts:
  - name: code-review-conventions
    version: "1.0.0"
    runtime: skill

inference:
  ...
  system_prompt_artifact: code-review-conventions
```

At launch, the runtime reads `workdir/tools/code-review-conventions/skill.md` and uses that content as the system prompt for every inference turn. The skill is **not** added to the callable tool inventory — it is already in context and would double-inject if called.

`MURMUR.md` notes the binding so the agent is aware:

```markdown
## Installed Skills

- **code-review-conventions** — Project PR review conventions *(bound as system prompt — already in context, not separately callable)*
```

Validation happens at parse time:

- The named artifact must be declared in `artifacts:` — if not, `mur run` exits with a clear error before starting.
- It must declare `prompt_payload: true` — `runtime: skill` defaults to this, so skills work with no extra declaration; a `tool`, `driver`, or `hook` artifact must set `prompt_payload: true` explicitly to be eligible, and one that doesn't is a manifest error. See [`inference.system_prompt_artifact`](../reference/manifest.md#inference-system-prompt-artifact).
- The skill's `skill.md` must be readable at launch — a missing file exits with `error[E-RUN-009]`.

---

## Step 5 — choose between inline, file, and artifact

| Use `system_prompt` (inline) | Use `system_prompt_file` | Use `system_prompt_artifact` |
|---|---|---|
| Short prompts (< 10 lines) | Long or structured prompts | Persona already packaged as a skill |
| Single-file manifests | Prompts version-controlled separately | Capsules that want to distribute both the skill and the persona together |
| Prototyping and quick experiments | Personas shared across multiple manifests | Reuse the same skill content both as a persona and (in other capsules) as a callable |

All three options are mutually exclusive — setting more than one in the same manifest is a parse error (`error[E-MAN-003]`).

---

## Step 6 — verify the system prompt is applied

Create a `task.md` file with a question designed to reveal the persona:

```
Who are you and what are your rules?
```

Run the capsule:

```bash
mur run
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
```

After the session completes, read the agent's output:

```bash
cat workdir/<session_id>/out/result.txt
```

Replace `<session_id>` with the value printed by `mur run`. For the data analyst persona above, you would expect exactly one summary sentence and a bullet list — no preamble, no closing remarks.

The session trace records which prompt was in effect. Every `session_start` line in `workdir/<session_id>/trace.jsonl` carries `system_prompt_source` (`"manifest"`, `"cli"` or `"none"`) and `system_prompt_sha256`, the hash of the prompt as resolved:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

!!! note "The prompt text is recorded only when you ask for it"
    `system_prompt_source` and `system_prompt_sha256` are always written, and the hash alone tells you whether two sessions ran with the same prompt. To read the prompt itself, set `trace.capture: content` in the manifest and `cat <session_id>/blobs/<system_prompt_sha256>`. See [Session trace schema](../reference/observability-schemas.md#trace-blobs).

If the agent is ignoring your constraints, add a more explicit rule to the system prompt and re-run.

---

## Step 7 — use system_prompt_file for a shared persona

When multiple capsules need the same base persona, factor it into a shared file:

```text
personas/
  analyst.md
  reviewer.md
capsule-a/
  murmur.yaml          → system_prompt_file: ../personas/analyst.md
capsule-b/
  murmur.yaml          → system_prompt_file: ../personas/analyst.md
```

Both manifests reference the same file. Updating the persona in one place updates all capsules that share it. The path is resolved relative to each manifest's directory, so a relative `..` path works as long as the directory layout is consistent.

---

## Step 8 — override the prompt for a single run

To try a different persona without editing `murmur.yaml`, pass [`mur run --system-prompt`](../reference/cli.md#mur-run):

```bash
mur run --task task.md --system-prompt "You are a terse code reviewer. Reply with numbered findings only."
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
status:  ok
```

The flag replaces whichever of the three manifest fields the capsule declared — inline, file, or artifact — and applies just as well to a manifest that declared none. It never writes to `murmur.yaml`: the next run without the flag is back to the manifest's own prompt.

Two consequences worth knowing:

- The value is trimmed. An empty or whitespace-only value clears the prompt entirely, so the run sends no `system` parameter beyond the runtime's own identity block — useful for checking how much of the agent's behaviour the persona is responsible for.
- Overriding a `system_prompt_artifact` releases that skill back into the callable tool inventory, since it is no longer already in context. `MURMUR.md` lists it as callable for that run.

The trace records the override: `session_start.system_prompt_source` reads `"cli"`, and `system_prompt_sha256` is the hash of the trimmed value you passed.

On a capsule with no `inference:` block there is no prompt to override, and the run fails with `error[E-IO-003]` before anything is staged.

---

## Summary

| Setting | Behaviour |
|---|---|
| `inference.system_prompt: \|` | Inline text; injected verbatim on every inference turn |
| `inference.system_prompt_file: path.md` | File content; read once at launch; path is relative to manifest directory |
| `inference.system_prompt_artifact: name` | Skill artifact's `skill.md` read at launch and used as system prompt; skill not separately callable |
| More than one field set simultaneously | Parse error `error[E-MAN-003]` |
| File or skill.md missing or unreadable | Launch error `error[E-RUN-009]` — session never starts |
| No field set | No `system` parameter is sent; model receives no system prompt |
| `mur run --system-prompt "text"` | Replaces whichever field the manifest set, for that run only; value is trimmed; empty value clears the prompt; `murmur.yaml` untouched |
| `mur run --system-prompt` on a manifest with no `inference:` | CLI error `error[E-IO-003]` — nothing is staged |
| `session_start.system_prompt_source` in `trace.jsonl` | `"manifest"` \| `"cli"` \| `"none"`, alongside `system_prompt_sha256`; the prompt text itself at `blobs/<system_prompt_sha256>` only under `trace.capture: content` |
