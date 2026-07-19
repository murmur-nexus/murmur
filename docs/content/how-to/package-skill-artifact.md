# How to package a skill into an artifact

A skill artifact packages a `skill.md` file — a Markdown document encoding domain knowledge, conventions, or structured guidance — into a `.mur.zip` the runtime installs alongside a capsule. Skills are **callable**: the agent invokes a skill by name (no arguments) and receives its full `skill.md` content as the tool result. No file-read capability is required. This lets you ship guidance independently of any particular capsule and install it into any capsule that benefits from it.

??? definition "What is a skill artifact?"
    A **skill artifact** is a `.mur.zip` whose required payload is a single `skill.md` file. Unlike tool artifacts (WASM or native binaries), a skill artifact installs a static guidance file. The runtime writes it to `workdir/tools/<name>/skill.md`, lists it in `MURMUR.md`, and exposes it in the tool inventory as a zero-argument callable. When the agent calls the skill, the runtime reads `skill.md` and returns its content as the tool result — no file-read capability, no subprocess, no WASM. Content is loaded just-in-time, so it only consumes context tokens when the agent chooses to call it.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [artifacts[].runtime](../reference/manifest-schema.md#field-artifacts) | Declares a skill artifact; set to `skill` to install `skill.md` at session start |
| [artifacts[].source](../reference/manifest-schema.md#local-source-skills) | Points at a local `skill.md` so the runtime installs it directly — no build, no publish. Skills get this for free (`local_source` defaults to `true` for `runtime: skill`); other roles must declare `local_source: true` to opt in. |
| [inference.system_prompt_artifact](../reference/manifest-schema.md#inference-system-prompt) | Bind a skill as the system prompt; reads its `skill.md` once at launch instead of on-demand |

There are two ways to get a skill into a capsule:

- **Package and publish** (Steps 1–6 below) — the distribution path. `mur build --skill` produces a `.mur.zip`, `mur publish` puts it in the registry, and the capsule declares it by `name` + `version`. Use this to share a skill across machines or pin a version.
- **Local source** ([Fast iteration](#fast-iteration-source-a-skill-from-a-local-path)) — the authoring path. The capsule points `source:` at a `skill.md` on disk; the runtime reads it directly on every run. Edit the file, relaunch, the change is live. No build or publish step.

---

## Fast iteration: source a skill from a local path

While you are still writing a skill, packaging and publishing on every edit is friction. Set `source:` on the artifact instead and the runtime reads `skill.md` straight from disk:

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/skill.md
    runtime: skill
```

`source:` may point at the `skill.md` file directly, or at a directory containing it (the runtime finds `skill.md` case-insensitively):

```yaml
artifacts:
  - name: code-review-conventions
    source: ./skills/code-review-conventions/
    runtime: skill
```

Key points:

- **No `version:` needed.** A local file has no registry version — the runtime labels it `local`. If you do set `version:`, it is ignored with a warning.
- **Relative paths resolve against the manifest's directory**, not your shell's working directory. Absolute paths work too.
- **`source:` is skill-only.** Putting it on a `tool`, `driver`, or `hook` artifact is a manifest error (`E-MAN-003`) caught before launch.
- **Everything downstream is unchanged.** The skill installs to `workdir/tools/<name>/skill.md` and appears in the tool inventory and `MURMUR.md` exactly as a registry skill would be — the agent calls it by name the same way (see [Step 4](#step-4-declare-the-skill-in-a-capsule-manifest)).

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

## Step 1 — write skill.md

Create a `skill.md` file containing the guidance you want the agent to be able to read. Any Markdown structure works — use headings and lists to make the content easy to scan:

```markdown
# Code Review Conventions

Guidance for reviewing pull requests in this project.

## What to check

- All public functions have doc comments
- Error types are described in the doc comment, not just the return type
- No `unwrap()` calls outside of tests
- No raw SQL strings — use the query builder

## How to structure feedback

Group findings by severity: blocking, non-blocking, nit.
Report each finding as: file · line · what · why.
```

The filename is case-insensitive at build time — `SKILL.md`, `Skill.md`, and `skill.md` are all accepted. The runtime always installs it as lowercase `skill.md`.

---

## Step 2 — build the artifact

### From an existing skill.md (no manifest required)

If you have a `skill.md` and want to wrap it immediately, use `mur build --skill`. No `murmur.yaml` is needed — one is generated for you:

```bash
mur build --skill my-skill/
```

```text
Built artifact: ./my-skill-1.0.0.mur.zip
```

The output zip is always written to the current working directory. The artifact name is inferred from the folder name: lowercased, with non-alphanumeric characters (except hyphens) replaced by underscores. The version defaults to `1.0.0`; pass `--version` to override:

To set an explicit name and version:

```bash
mur build --skill code-review-conventions --version 1.0.0 my-skill/
```

```text
Built artifact: ./code-review-conventions-1.0.0.mur.zip
```

To add a one-line description (shown in the tool inventory and `mur list`):

```bash
mur build --skill code-review-conventions --summary "Project PR review conventions" my-skill/
```

The `--summary` text is written as `description:` in the generated manifest. You can update it later by re-running `mur build --skill` with a new `--summary` value — the existing manifest is updated in place.

The generated `murmur.yaml` inside the artifact contains three fields (four when `--summary` is given):

```yaml
name: my-skill
version: '1.0.0'
runtime: skill
description: 'Project PR review conventions'   # only present when --summary is passed
```

If the input folder already contains a `murmur.yaml`, it is used and the `description` field is set or overwritten when `--summary` is provided. The manifest must declare `runtime: skill`; any other runtime value is an error (`E-MAN-003`).

!!! warning "Agents with Bash or file-read tools will bypass the skill runtime"
    If your capsule exposes a Bash tool or a file-reader, the agent will almost always prefer a direct tool call to read the `skill.md` path rather than calling the skill through the runtime. The skill's tool-inventory entry is effectively invisible to it. If you need the agent to invoke the skill via the runtime (so the guidance is loaded on-demand and appears as a tool call in the trace), package and publish it first — then declare it by name and version in the manifest rather than via `source:`. See [Step 3](#step-3-publish-to-the-local-registry) onwards.

### From a project folder with an explicit manifest

If you need control over the manifest — to pin a version or add a description — create a `murmur.yaml` alongside `skill.md`:

```yaml
name: code-review-conventions
version: "1.0.0"
runtime: skill
```

Then build from the project directory:

```bash
mur build ./my-skill
```

```text
Built artifact: ./my-skill/code-review-conventions-1.0.0.mur.zip
```

`mur build` validates that `skill.md` is present. If it is missing:

```text
error[E-IO-003]: missing required file 'skill.md' in /path/to/my-skill
```

---

## Step 3 — publish to the local registry

```bash
mur publish code-review-conventions-1.0.0.mur.zip
```

??? info "Choosing a registry target"
    Without `--registry`, the target is determined by your workspace config (`registry.default` / `registry.remote_url` in `murmur.yaml`). You can override it explicitly:

    - **`--registry local`** — writes to `~/.murmur/artifacts/`. No remote registry or API key needed. Use this when you are working locally or have no remote configured.

        ```bash
        mur publish --registry local code-review-conventions-1.0.0.mur.zip
        ```

    - **`--registry <url>`** — publishes to a remote registry at that URL. Requires `NEXUS_API_KEY` to be set.

        ```bash
        mur publish --registry https://registry.example.com code-review-conventions-1.0.0.mur.zip
        ```

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
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver

      - name: code-review-conventions
        version: "1.0.0"
        runtime: skill

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
        You are a code review assistant.
    ```

=== "OpenAI"

    ```yaml
    name: pr-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver

      - name: code-review-conventions
        version: "1.0.0"
        runtime: skill

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
        You are a code review assistant.
    ```

=== "DeepSeek"

    ```yaml
    name: pr-reviewer
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver

      - name: code-review-conventions
        version: "1.0.0"
        runtime: skill

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
        You are a code review assistant.
    ```

The skill appears in the agent's tool inventory as a zero-argument callable. When the agent calls `code-review-conventions`, the runtime reads `tools/code-review-conventions/skill.md` and returns the content as the tool result. No file-read capability is required.

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

At startup, the runtime installs the skill to:

```
workdir/
└── tools/
    └── code-review-conventions/
        └── skill.md
```

`MURMUR.md` at the workdir root lists all installed skills and tells the agent they are callable:

```markdown
## Installed Skills

Skills are callable: invoke a skill by name (no input) to receive its full guidance
as the tool result. The content is returned just-in-time — it is not pre-injected
into context. No file-read capability is required.

- **code-review-conventions** — Project PR review conventions *(call by name to load guidance)*
```

The runtime returns the full `skill.md` content as the tool result.

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
count:      2  (max: 10)

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
| Payload | WASM component or native binary | `skill.md` Markdown file |
| Model interaction | Agent calls it as a function with input | Agent calls it by name (no input); runtime returns `skill.md` content |
| Runs code | Yes | No — static file read by the runtime |
| Requires file-read capability | No | No |
| Injected into system prompt | No | No — unless declared as `system_prompt_artifact` |
| Appears in tool inventory (`%list()`) | Yes | Yes — shown as callable, zero-argument |
| Appears in `MURMUR.md` | Yes (`## Installed Tools`) | Yes (`## Installed Skills`) |
| Consumes context tokens at start | No | No — only when called |

Skills do not run any code. They are static guidance files returned on-demand when the agent calls them by name.

---

## Summary

| Step | What you produce |
|---|---|
| `skill.md` with domain guidance | The skill payload |
| `artifacts[].source: ./path/to/skill.md` | Authoring mode — runtime reads `skill.md` from disk; no build or publish |
| `mur build --skill <path>` | `.mur.zip` with generated manifest; output in CWD |
| `mur build --skill <name> --version <ver> <path>` | `.mur.zip` with explicit name and version |
| `mur build --skill --summary "<text>" <path>` | `.mur.zip` with description written to manifest |
| `mur build .` (with `murmur.yaml`) | `.mur.zip` with hand-authored manifest; output in source dir |
| `mur publish <zip>` | Skill available to any local capsule |
| `artifacts[].runtime: skill` in capsule manifest | Runtime installs `skill.md`; skill appears in tool inventory as callable |
| `inference.system_prompt_artifact: <name>` | Skill content read at launch and used as system prompt; not separately callable |
