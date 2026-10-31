# How to package a skill into an artifact

A skill artifact packages a `skill.md` file — a Markdown document encoding domain knowledge, conventions, or structured guidance — and the reference files and scripts it points at into a `.mur.zip` the runtime installs alongside a capsule. A skill folder made for Claude Code, Codex or pi builds as it is. Skills are **callable**: the agent invokes a skill by name (no arguments) and receives its full `skill.md` content as the tool result. No file-read capability is required. This lets you ship guidance independently of any particular capsule and install it into any capsule that benefits from it.

??? definition "What is a skill artifact?"
    A **skill artifact** is a `.mur.zip` whose required payload is a `skill.md` file, optionally with supporting files beside it. Unlike tool artifacts (WASM or native binaries), a skill artifact installs static files. The runtime writes them under `tools/<name>/` in the session's [runtime directory](../reference/workdir.md#runtime-directory), lists the skill in `MURMUR.md`, and exposes it in the tool inventory as a zero-argument callable. When the agent calls the skill, the runtime reads `skill.md` and returns its content as the tool result — no file-read capability, no subprocess, no WASM. Content is loaded just-in-time, so it only consumes context tokens when the agent chooses to call it.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest.md#field-artifacts) | Declares a skill artifact; set to `skill` to install `skill.md` at session start |
| [artifacts[].source](../reference/manifest.md#local-source-skills) | Points at a local `skill.md` or skill folder so the runtime installs it directly — no build, no publish. Skills get this for free (`local_source` defaults to `true` for `runtime: skill`); other roles must declare `local_source: true` to opt in. |
| [inference.system_prompt_artifact](../reference/manifest.md#inference-system-prompt) | Bind a skill as the system prompt; reads its `skill.md` once at launch instead of on-demand |

There are two ways to get a skill into a capsule:

- **Package and publish** (Steps 1–6 below) — the distribution path. `mur build <folder>` produces a `.mur.zip`, `mur publish` puts it in the global store, and the capsule declares it by `name` + `version`. Use this to share a skill across machines or pin a version.
- **Local source** ([Fast iteration](#fast-iteration-source-a-skill-from-a-local-path)) — the authoring path. The capsule points `source:` at a `skill.md` or skill folder on disk; the runtime reads it directly on every run. Edit the file, relaunch, the change is live. No build or publish step.

---

## Fast iteration: source a skill from a local path

While you are still writing a skill, packaging and publishing on every edit is friction. Set `source:` on the artifact instead and the runtime reads `skill.md` straight from disk:

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/skill.md
    runtime: skill
```

`source:` may point at the `skill.md` file directly, or at a skill folder containing it (the runtime finds `skill.md` case-insensitively):

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/
    runtime: skill
```

| `source:` names | What installs under `tools/<name>/` |
|---|---|
| A file | That file, as `skill.md` |
| A folder | The folder's `skill.md` and every supporting file, chosen by the same rules as [`mur build` on a skill folder](#from-a-skill-folder-no-manifest); the folder's own `murmur.yaml` is never installed |

Key points:

- **No `version:` needed.** A local file has no registry version — the runtime labels it `local`. If you do set `version:`, it is ignored with a warning.
- **Relative paths resolve against the manifest's directory**, not your shell's working directory. Absolute paths work too.
- **`source:` is skill-only.** Putting it on a `tool`, `driver`, or `hook` artifact is a manifest error (`E-MAN-003`) caught before launch.
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

If the path is wrong, the run fails before any workdir is created, naming what it could not find:

```text
error[E-IO-001]: skill source path not found: /path/to/capsule/skills/code-review-conventions/skill.md
```

When you are ready to distribute the skill, switch to the packaged path below — build, publish, and replace `source:` with `version:`.

---

## Step 1 — write the skill folder

A skill folder holds a `skill.md` with the guidance the agent reads, and any files that guidance points at. The front block at the top of `skill.md` names and describes the skill, the same block Claude Code, Codex and pi read:

```text
code-review-conventions/
├── SKILL.md
└── references/
    └── severity-levels.md
```

```markdown
---
name: code-review-conventions
description: Project PR review conventions. Use when reviewing a pull request.
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

| Front block key | Used for |
|---|---|
| `name` | The artifact name |
| `description` | `description:` in the artifact's manifest: the skill's summary in the tool inventory, `MURMUR.md` and `mur list` |

Every other key (`allowed-tools`, `license`, `metadata`, …) is ignored. The front block is optional; without one the artifact is named from the folder.

The filename is case-insensitive — `SKILL.md`, `Skill.md`, and `skill.md` are all accepted. The artifact and the runtime always store it as lowercase `skill.md`.

---

## Step 2 — build the artifact

### From a skill folder (no manifest)

Run `mur build` on the folder:

```bash
mur build code-review-conventions/
```

```text
Built artifact: /home/you/work/code-review-conventions-0.1.0.mur.zip (WIT level 1)
```

`mur build` takes this path when the folder holds a `skill.md` (any casing) and no `murmur.yaml`. A folder with neither still fails with `error[E-IO-001]: murmur.yaml not found at …`.

The artifact is written to the current directory. Pass `--output` to choose another place: a directory receives `<name>-<version>.mur.zip`, any other path is the output file.

The manifest is generated from the front block and the flags:

| Field | Taken from, first that is set |
|---|---|
| `name` | `--skill <name>`, the front block's `name`, the folder name (lowercased, characters other than letters, digits and hyphens replaced by `_`) |
| `description` | `--summary "<text>"`, the front block's `description` |
| `version` | `--version <ver>`, `0.1.0` |

```bash
mur build --skill pr-conventions --version 1.0.0 --summary "Project PR review conventions" code-review-conventions/
```

```yaml
name: pr-conventions
version: '1.0.0'
runtime: skill
description: Project PR review conventions
```

A name that is not a valid artifact name (lowercase letters, digits and inner hyphens) fails with `E-BLD-001`; pass `--skill <name>` to build the skill under a valid one.

What the build does with each file in the folder:

| File | Result |
|---|---|
| Root `SKILL.md` / `skill.md` | Packed as `skill.md` |
| Every other regular file, at any depth | Packed at its path relative to the folder, e.g. `references/severity-levels.md` |
| A name starting with `.` at any depth (`.git/`, `.DS_Store`) | Skipped |
| A file ending `.mur.zip`, and the output file | Skipped |
| A symlink anywhere | Refused, `E-BLD-002` |
| Both `SKILL.md` and `skill.md` at the root | Refused, `E-BLD-002` |
| Root `murmur-build.json` | Refused, `E-BLD-002` — `mur build` writes it |

A refused build writes no artifact. The finished artifact holds:

```text
murmur.yaml
skill.md
references/severity-levels.md
murmur-build.json
```

`mur build --skill <folder>` builds the same way. With `--skill`, a `murmur.yaml` in the folder is packed as the manifest instead of a generated one, and the front block is not read; `--summary` sets its `description` in the packed copy. The manifest must declare `runtime: skill`; any other runtime value is an error (`E-MAN-003`). `--skill` also accepts a `.zip` holding `SKILL.md`, from which only root entries are packed.

!!! warning "Agents with Bash or file-read tools will bypass the skill runtime"
    If your capsule exposes a Bash tool or a file-reader, the agent will almost always prefer a direct tool call to read the `skill.md` path rather than calling the skill through the runtime. The skill's tool-inventory entry is effectively invisible to it. If you need the agent to invoke the skill via the runtime (so the guidance is loaded on-demand and appears as a tool call in the trace), package and publish it first — then declare it by name and version in the manifest rather than via `source:`. See [Step 3](#step-3-publish-into-the-global-store) onwards.

### From a project folder with an explicit manifest

If you need control over the manifest, create a `murmur.yaml` alongside `skill.md`:

```yaml
name: code-review-conventions
version: "1.0.0"
runtime: skill
requires_files:
  - skill.md
  - references/severity-levels.md
```

Then build from the project directory:

```bash
mur build ./my-skill
```

```text
Built artifact: ./my-skill/code-review-conventions-1.0.0.mur.zip (WIT level 1)
```

With a `murmur.yaml`, the artifact holds the manifest and the files `requires_files:` lists — `skill.md` alone when it is omitted. Supporting files ride along only when listed. If a listed file is missing:

```text
error[E-IO-003]: missing required file 'skill.md' in /path/to/my-skill
```

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

The skill appears in the agent's tool inventory as a zero-argument callable, under the part of its name after the `/`. When the agent calls `code-review-conventions`, the runtime reads `tools/code-review-conventions/skill.md` and returns the content as the tool result. No file-read capability is required.

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
        ├── skill.md
        └── references/
            └── severity-levels.md
```

Every packed file except the artifact's `murmur-build.json` is installed at its packed path; `murmur.yaml` is the skill's manifest. The files are read-only to the capsule and not executable — a script in the skill runs through an interpreter the capsule's `capabilities.shell.allow` lists.

`MURMUR.md`, which the agent reads in its runtime directory, lists all installed skills and tells the agent they are callable:

```markdown
## Installed Skills

Skills are callable: invoke a skill by name (no input) to receive its full guidance
as the tool result. The content is returned just-in-time — it is not pre-injected
into context. No file-read capability is required.

- **code-review-conventions** — Project PR review conventions *(call by name to load guidance; /home/you/.murmur/sessions/ses_…/runtime/tools/code-review-conventions/skill.md)*
```

The runtime returns the full `skill.md` content as the tool result. When the skill installed supporting files, a block naming their directory and listing them follows it, so a capsule with file access can read them:

```text
<skill.md text>

---
Supporting files for this skill (read-only) are in /home/you/.murmur/sessions/ses_…/runtime/tools/code-review-conventions:
- references/severity-levels.md
```

The list is sorted and names at most 50 files, then `- … and N more`. A skill with only `skill.md` returns its `skill.md` text alone.

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
| Payload | WASM component or native binary | `skill.md` Markdown file, plus any supporting files |
| Model interaction | Agent calls it as a function with input | Agent calls it by name (no input); runtime returns `skill.md` content and lists the supporting files |
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
| `mur build <folder>` (no `murmur.yaml`) | `.mur.zip` named and described from the front block, holding every file in the folder; output in CWD |
| `mur build --skill <name> --version <ver> --summary "<text>" <folder>` | The same, with explicit name, version and description |
| `mur build .` (with `murmur.yaml`) | `.mur.zip` with hand-authored manifest and its `requires_files:`; output in source dir |
| `mur publish <zip>` | Skill available to any local capsule |
| `artifacts[].runtime: skill` in capsule manifest | Runtime installs the skill under `tools/<name>/`; skill appears in tool inventory as callable |
| `inference.system_prompt_artifact: <name>` | Skill content read at launch and used as system prompt; not separately callable |
