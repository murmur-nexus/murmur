# How to package a skill into an artifact

A skill artifact packages a skill folder — a `SKILL.md` encoding domain knowledge, conventions or structured guidance, and the reference files and scripts it points at — into a `.mur.zip` the runtime installs alongside a capsule. Murmur skills follow the [Agent Skills standard](https://agentskills.io/specification), so a folder `npx skills` installs, or one made for Claude Code, Codex or pi, is the folder `mur build` packs. Skills are **callable**: the agent invokes a skill by name (no arguments) and receives the body of its `SKILL.md` as the tool result. No file-read capability is required. This lets you ship guidance independently of any particular capsule and install it into any capsule that benefits from it.

??? definition "What is a skill artifact?"
    A **skill artifact** is a `.mur.zip` whose required payload is a `SKILL.md` file, optionally with supporting files beside it. Unlike a tool artifact, which is a WASM component, a skill artifact installs static files. The runtime writes them under `tools/<name>/` in the session's [runtime directory](../reference/workdir.md#runtime-directory), lists the skill in `MURMUR.md`, and exposes it in the tool inventory as a zero-argument callable. When the agent calls the skill, the runtime returns the body of `SKILL.md` as the tool result — no file-read capability, no subprocess, no WASM. Content is loaded just-in-time, so it only consumes context tokens when the agent chooses to call it.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest.md#field-artifacts) | Declares a skill artifact; set to `skill` to install the skill folder at session start |
| [artifacts[].source](../reference/manifest.md#local-source-skills) | A local skill folder or `SKILL.md` the runtime installs directly, with no build or publish; on by default for `runtime: skill` |
| [inference.system_prompt_artifact](../reference/manifest.md#inference-system-prompt-artifact) | A skill bound as the system prompt; its `SKILL.md` body is read once at launch instead of on demand |

There are two ways to get a skill into a capsule:

- **Package and publish** (Steps 1–6 below) — the distribution path. `mur build <folder>` produces a `.mur.zip`, `mur publish` puts it in the global store, and the capsule declares it by `name` + `version`. Use this to share a skill across machines or pin a version.
- **Local source** ([Fast iteration](#fast-iteration-source-a-skill-from-a-local-path)) — the authoring path. The capsule points `source:` at a skill folder or its `SKILL.md` on disk; the runtime reads it directly on every run. Edit the file, relaunch, the change is live. No build or publish step.

!!! warning "Skills built by earlier `mur` releases are refused"
    A skill artifact built by an earlier `mur` release holds a lowercase `skill.md` and no `SKILL.md`. `mur install`, `mur publish` and a launch refuse it with [`E-REG-017`](../reference/diagnostics.md#e-reg-017). Rebuild it with `mur build` from a folder holding `SKILL.md` with a [front block](#step-1-write-the-skill-folder), and install the new version. No setting makes `mur` accept the old artifact.

---

## Fast iteration: source a skill from a local path

While you are still writing a skill, packaging and publishing on every edit is friction. Set `source:` on the artifact instead and the runtime reads the skill folder straight from disk:

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/
    runtime: skill
```

`source:` may point at the skill folder, or at its `SKILL.md` directly:

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/SKILL.md
    runtime: skill
```

| `source:` names | What installs under `tools/<name>/` |
|---|---|
| A folder | Its `SKILL.md` and every supporting file, chosen by the same rules as [`mur build`](#step-2-build-the-artifact) |
| A file | That file alone, as `SKILL.md`; the file must be named `SKILL.md` |

The runtime checks `SKILL.md` at launch by the same [front block rules](#step-1-write-the-skill-folder) as `mur build`, except that it does not need `metadata.version`. For a `source:` naming a file, the front block's `name` must equal the name of the folder holding the file.

Key points:

- **No `version:` needed.** A local folder has no registry version — the runtime labels it `local`. If you do set `version:`, it is ignored with a warning.
- **The entry's `name:` is the tool name** the agent calls. It is not compared with the front block's `name`.
- **Relative paths resolve against the manifest's directory**, not your shell's working directory. Absolute paths work too.
- **Everything downstream is the same as for a published skill.** The skill installs to `tools/<name>/` in the session's [runtime directory](../reference/workdir.md#runtime-directory) and appears in the tool inventory and `MURMUR.md` exactly as a registry skill would be — the agent calls it by name the same way (see [Step 4](#step-4-declare-the-skill-in-a-capsule-manifest)).

Run `mur install` to fetch the driver and any other registry artifacts in your manifest:

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

Then run the capsule — there is nothing to build or publish first:

```bash
mur run
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
^C
```

If the path is wrong, or `SKILL.md` breaks a front block rule, the run fails before any workdir is created and names what it found:

```text
error[E-IO-001]: skill source path not found: /path/to/capsule/skills/code-review-conventions
```

```text
error[E-BLD-009]: /path/to/capsule/skills/code-review-conventions/SKILL.md: front block name 'review-conventions' differs from the folder name 'code-review-conventions'
  hint: set name: code-review-conventions, or rename the folder to review-conventions
```

When you are ready to distribute the skill, switch to the packaged path below — build, publish, and replace `source:` with `version:`.

---

## Step 1 — write the skill folder

Create a folder named for the skill, holding a `SKILL.md` with the guidance the agent reads and any files that guidance points at:

```text
code-review-conventions/
├── SKILL.md
└── references/
    └── severity-levels.md
```

`SKILL.md` opens with a front block naming, describing and versioning the skill, then the body the agent receives:

```markdown
---
name: code-review-conventions
description: Project PR review conventions. Use when reviewing a pull request.
metadata:
  version: "1.0.0"
---
# Code Review Conventions

Guidance for reviewing pull requests in this project.

## What to check

- All public functions have doc comments
- No `unwrap()` calls outside of tests

## How to structure feedback

Group findings by severity — see references/severity-levels.md.
Report each finding as: file · line · what · why.
```

| Front block key | Rule | Used for |
|---|---|---|
| `name` | 1–64 lowercase letters, digits and `-`; no `-` at the start or end, no `--`; equal to the folder's name | The artifact name |
| `description` | Non-empty, at most 1024 characters | The skill's summary in the tool inventory, `MURMUR.md` and `mur list` |
| `metadata.version` | A semver string, such as `"1.0.0"`; quote it | The artifact version. Needed by `mur build` only |

Every other key (`license`, `compatibility`, `allowed-tools`, other `metadata` entries, Claude Code's `when_to_use`) is not read and stays in `SKILL.md` unchanged. The front block is YAML: quote a value that holds `: `, such as `description: "Fill forms: use when …"`, or the build refuses the block as invalid YAML.

The file is named `SKILL.md`, exactly. A skill folder holds no `murmur.yaml`: the front block is where the skill's name, description and version live. A `skill.md` or `Skill.md`, or a `murmur.yaml` beside `SKILL.md`, is refused with [`E-BLD-009`](../reference/diagnostics.md#e-bld-009).

---

## Step 2 — build the artifact

Run `mur build` on the folder:

```bash
mur build code-review-conventions/
```

```text
Built artifact: /home/you/work/code-review-conventions-1.0.0.mur.zip (WIT level 1)
```

`mur build` packs a skill when the folder's root holds `SKILL.md`. The artifact is written to the current directory as `<name>-<version>.mur.zip`. Pass `--output` to choose another place: a directory receives `<name>-<version>.mur.zip`, any other path is the output file.

A front block that breaks a rule fails the build with `E-BLD-009`, naming the file and the rule:

```text
error[E-BLD-009]: /home/you/work/code-review-conventions/SKILL.md: front block metadata.version '1.0' is not a semver version: …
  hint: write a full semver version, e.g. version: "1.2.0"
```

What the build does with each file in the folder:

| File | Result |
|---|---|
| Root `SKILL.md` | Packed byte for byte |
| Every other regular file, at any depth | Packed at its path relative to the folder, e.g. `references/severity-levels.md` |
| A name starting with `.` at any depth (`.git/`, `.DS_Store`) | Skipped |
| A file ending `.mur.zip`, and the output file | Skipped |
| A root `skill.md`, `Skill.md` or any other casing of `SKILL.md` | Refused, `E-BLD-009` |
| A root `murmur.yaml` | Refused, `E-BLD-009` |
| A symlink anywhere | Refused, `E-BLD-002` |
| Root `murmur-build.json` | Refused, `E-BLD-002` — `mur build` writes it |

A refused build writes no artifact. The finished artifact holds the folder's files, a generated `murmur.yaml` and `murmur-build.json`:

```text
murmur.yaml
SKILL.md
references/severity-levels.md
murmur-build.json
```

The generated `murmur.yaml` is taken from the front block:

```yaml
name: code-review-conventions
version: '1.0.0'
runtime: skill
description: Project PR review conventions. Use when reviewing a pull request.
```

!!! warning "Agents with Bash or file-read tools will bypass the skill runtime"
    If your capsule exposes a Bash tool or a file-reader, the agent will almost always prefer a direct tool call to read the `SKILL.md` path rather than calling the skill through the runtime. The skill's tool-inventory entry is effectively invisible to it. If you need the agent to invoke the skill via the runtime (so the guidance is loaded on-demand and appears as a tool call in the trace), package and publish it first — then declare it by name and version in the manifest rather than via `source:`. See [Step 3](#step-3-publish-into-the-global-store) onwards.

---

## Step 3 — publish into the global store

```bash
mur publish code-review-conventions-1.0.0.mur.zip
```

`mur publish` writes the artifact into the global store as `local/code-review-conventions`, at `~/.murmur/artifacts/local/code-review-conventions/1.0.0/`, where every project on this machine can install it from. A file names no publisher, so what it publishes is in the `local` scope.

Then install:

```bash
mur install
```

---

## Step 4 — declare the skill in a capsule manifest

Create a `murmur.yaml` for the capsule that needs the skill. Add the skill artifact under `artifacts:` with `runtime: skill`. No special system prompt instruction is required — the agent sees the skill in its tool inventory and calls it by name when relevant:

=== "Anthropic"

    ```yaml
    name: pr-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
        gateway:
          endpoint: https://api.anthropic.com
          api_key: ${ANTHROPIC_API_KEY}

      - name: local/code-review-conventions
        version: "1.0.0"
        runtime: skill

    inference:
      transport: http
      model: {{ v.model_anthropic }}
      driver:
        artifact: murmur/driver-anthropic
      system_prompt: |
        You are a code review assistant.
    ```

=== "OpenAI"

    ```yaml
    name: pr-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
        gateway:
          endpoint: https://api.openai.com/v1
          api_key: ${OPENAI_API_KEY}

      - name: local/code-review-conventions
        version: "1.0.0"
        runtime: skill

    inference:
      transport: http
      model: {{ v.model_openai }}
      driver:
        artifact: murmur/driver-openai
      system_prompt: |
        You are a code review assistant.
    ```

=== "DeepSeek"

    ```yaml
    name: pr-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur/driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
        gateway:
          endpoint: https://api.deepseek.com
          api_key: ${DEEPSEEK_API_KEY}

      - name: local/code-review-conventions
        version: "1.0.0"
        runtime: skill

    inference:
      transport: http
      model: {{ v.model_deepseek }}
      driver:
        artifact: murmur/driver-deepseek
      system_prompt: |
        You are a code review assistant.
    ```

The skill appears in the agent's tool inventory as a zero-argument callable, under the part of its name after the `/`. When the agent calls `code-review-conventions`, the runtime reads `tools/code-review-conventions/SKILL.md` and returns its body, the text after the front block, as the tool result. No file-read capability is required.

If you want the skill's content pre-loaded as the system prompt instead of called on-demand, use [`inference.system_prompt_artifact`](capsule-system-prompt.md#step-4-load-the-prompt-from-a-skill-artifact) instead.

---

## Step 5 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 6 — run and verify

Create a `task.md` with the review request:

```markdown
Review the diff in `/path/to/my-repo` against the project conventions.
Report findings grouped by severity: blocking, non-blocking, nit.
```

Run the capsule:

```bash
mur run
```

```
murmur: url localhost:52222
session: ses_019ed2af53da75c2aefee84ee10c34af
^C
```

At startup, the runtime installs the skill in the session's runtime directory, which exists while
the session runs:

```
~/.murmur/sessions/<session_id>/runtime/
└── tools/
    └── code-review-conventions/
        ├── murmur.yaml
        ├── SKILL.md
        └── references/
            └── severity-levels.md
```

Every packed file except the artifact's `murmur-build.json` is installed at its packed path: `SKILL.md` byte for byte, and `murmur.yaml`, the skill's manifest, whose `description` the tool inventory and `MURMUR.md` show. The files are read-only to the capsule and not executable — a script in the skill runs through an interpreter the capsule's `capabilities.shell.allow` lists.

`MURMUR.md`, which the agent reads in its runtime directory, lists all installed skills and tells the agent they are callable:

```markdown
## Installed Skills

Skills are callable: invoke a skill by name (no input) to receive its full guidance
as the tool result. The content is returned just-in-time — it is not pre-injected
into context. No file-read capability is required.

- **code-review-conventions** — Project PR review conventions. Use when reviewing a pull request. *(call by name to load guidance; /home/you/.murmur/sessions/ses_…/runtime/tools/code-review-conventions/SKILL.md)*
```

The runtime returns the body of `SKILL.md` as the tool result: the text after the front block's closing `---` line, with leading blank lines removed. The installed file is not changed. When the skill installed supporting files, a block naming their directory and listing them follows it, so a capsule with file access can read them:

```text
<SKILL.md body>

---
Supporting files for this skill (read-only) are in /home/you/.murmur/sessions/ses_…/runtime/tools/code-review-conventions:
- references/severity-levels.md
```

The list is sorted and names at most 50 files, then `- … and N more`. A skill with only `SKILL.md` returns its body alone.

---

## Finding skill calls in the trace

After a session completes, run:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

The summary includes a dedicated `── Skill calls ──` section:

```
── Session ──────────────────────────────────────
session:    ses_019ed2af53da75c2aefee84ee10c34af
capsule:    pr-reviewer v0.1.0
model:      deepseek-v4-flash
status:     ok
duration:   4.1s

── Turns ────────────────────────────────────────
count:      2  (max: no cap)

── Tokens ───────────────────────────────────────
input:      8,214  (avg 4,107/turn)
output:     612  (avg 306/turn)
total:      8,826

── Tool calls ───────────────────────────────────
count:      0

── Skill calls ──────────────────────────────────
count:      1  (1 ok, 0 error)  success 100.0%
latency:    avg 0ms
  turn 0  code-review-conventions 0ms ✓

── Compaction ───────────────────────────────────
fired:      no

── Tasks ────────────────────────────────────────
task 1  tsk_019ed2af  turns: 2  in: 8,214  out: 612  ok  4.1s
```

The underlying event in `trace.jsonl` looks like this:

```json
{"event_type":"skill_call","session_id":"ses_019ed2af53da75c2aefee84ee10c34af","timestamp":1782429237756,"turn":0,"skill_name":"code-review-conventions","output_bytes":6543,"duration_ms":0,"status":"ok"}
```

---

## How skills differ from tools

| | Tool artifact | Skill artifact |
|---|---|---|
| Payload | WASM component | `SKILL.md` Markdown file, plus any supporting files |
| Model interaction | Agent calls it as a function with input | Agent calls it by name (no input); runtime returns the `SKILL.md` body and lists the supporting files |
| Runs code | Yes | No — static file read by the runtime |
| Requires file-read capability | No | No |
| Injected into system prompt | No | No — unless declared as `system_prompt_artifact` |
| Appears in tool inventory (`%list()`) | Yes | Yes — shown as callable, zero-argument |
| Appears in `MURMUR.md` | Yes (`## Installed Tools`) | Yes (`## Installed Skills`) |
| Consumes context tokens at start | No | No — only when called |

Skills do not run any code. They are static files: the guidance is returned on-demand when the agent calls the skill by name, and the supporting files are read like any other file.

---

## Summary

| Step | What you produce |
|---|---|
| Skill folder: `SKILL.md` with a front block, plus supporting files | The skill payload |
| `artifacts[].source: ./path/to/skill-folder` | Authoring mode — runtime reads the skill from disk; no build or publish |
| `mur build <folder>` | `.mur.zip` named, versioned and described from the front block, holding every file in the folder; output in CWD |
| `mur publish <zip>` | Skill available to any local capsule |
| `artifacts[].runtime: skill` in capsule manifest | Runtime installs the skill under `tools/<name>/`; skill appears in tool inventory as callable |
| `inference.system_prompt_artifact: <name>` | `SKILL.md` body read at launch and used as system prompt; not separately callable |
