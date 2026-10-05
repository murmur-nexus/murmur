# CLI Commands

Every `mur` command, its flags, and what each one does.

## Commands

| Command | Purpose |
|---|---|
| `mur build` | Build a `.mur.zip` from a source directory |
| `mur new` | Scaffold a formation, or (beta) generate a `murmur.yaml` from a plain-language task description |
| `mur publish` | Publish a built artifact to local or remote registry |
| `mur install` | Fetch and install artifacts from configured registry sources |
| `mur precompile` | Compile WASM artifact files for this machine, so their first launch skips compiling |
| `mur list` | List installed artifacts in the project or global store |
| `mur doctor` | Check every artifact declared in `murmur.yaml` against the project and global stores |
| `mur run` | Run a capsule with lockfile-aware artifact resolution |
| `mur ps` | List the capsules running on this machine |
| `mur watch` | Stream live events from a running capsule's output to stdout |
| `mur cancel` | Stop one running task on a capsule, leaving the session running |
| `mur stop` | End one running capsule, and report what it left behind |
| `mur control` | Show or change what a running capsule's `control:` block lets a controller change |
| `mur deploy run` | Upload a capsule to an existing VM and return its public URL |
| `mur deploy ls` | List all deployed capsules |
| `mur destroy` | Remove a deployment record from the local tracking list |
| `mur conversation ls` | List the durable conversation records, or place one message id in them |
| `mur conversation rm` | Remove one context's record directory, whole |
| `mur conversation truncate` | Drop the oldest messages from a record, keeping the newest N |
| `mur trace show` | Human-readable summary of a single `trace.jsonl` session |
| `mur trace steps` | Turn-by-turn tree of what one session's agent did |
| `mur trace diff` | Side-by-side metric comparison of two sessions |
| `mur trace report` | Aggregate statistics across a set of sessions |
| `mur eval show` | Human-readable (or JSON) summary of a single `eval.jsonl` session |
| `mur eval diff` | Side-by-side scorer comparison of two eval sessions |
| `mur eval run` | Drive a multi-case dataset and collect `eval.jsonl` per run |
| `mur search` | Search the public artifact index for artifacts matching a keyword |
| `mur topology` | Render capsule sessions as a DAG from Grafana Tempo OTel data |

---

## Session addresses { #session-addresses }

Every command that names a session spells the address the same way.

| Form | Example | Names |
|---|---|---|
| Full ID | `ses_019f01a940ce7761854e768ecbe3d399` | The session with that ID: `ses_` followed by 32 hex characters |
| Suffix | `d399` | The one session whose ID ends with those characters, matched case-insensitively. 4 characters or more. Two or more matches are refused, and the refusal lists them |
| Ordinal | `@1`, `@2` | The most recent session, the second most recent, and so on. Session IDs sort in creation order, so `@N` counts back from the newest |
| Path | `workdir/ses_019f…/trace.jsonl` | The record file at that literal path, taken verbatim. `mur run --resume` also accepts the session directory itself |

What an address is resolved against depends on what the command needs.

| Command | Candidate set | `@1` means |
|---|---|---|
| [`mur run --resume`](#mur-run), [`mur trace show`](#mur-trace-show), [`mur trace steps`](#mur-trace-steps), [`mur trace diff`](#mur-trace-diff), [`mur trace report`](#mur-trace-report), [`mur eval show`](#mur-eval-show), [`mur eval diff`](#mur-eval-diff) | The `ses_*` session directories in one workdir, whether or not the session is still running | The most recent recorded session |
| [`mur watch`](#mur-watch), [`mur cancel`](#mur-cancel), [`mur stop`](#mur-stop) | The [running-capsule records](#running-capsule-records) for this whole machine | The most recent running session |

The recorded set is the `ses_*` directories in `./workdir` for the `mur trace` and `mur eval`
commands, and for `mur run` either `<manifest-dir>/workdir` or `.murmur` inside the directory
`--workdir` names. See [Session workdir](workdir.md).

`mur watch`, `mur cancel` and `mur stop` have to reach a process, so they take the three forms that
name a session and refuse the path form: a path names a directory on disk, which says nothing
about whether a process is running. An address naming a session that has stopped reports
[`E-RUN-022`](diagnostics.md#e-run-022) rather than resolving to a different capsule.

Omitting the address selects a default:

| Command | Bare form means |
|---|---|
| `mur run --resume` | `@1` |
| `mur trace show` | `@1` |
| `mur trace steps` | `@1` |
| `mur trace diff` | `@2 @1` |
| `mur trace report` | every session in the workdir |
| `mur eval show` | `@1` |
| `mur eval diff` | `@2 @1` |
| `mur watch` | `@1` |

`mur trace diff` and `mur eval diff` take their arguments in *before, after* order, so the bare
`@2 @1` puts the older run in the Run A column and the delta column reads forwards in time. Both
take two addresses or none; one address is refused.

An address matching no session, or several, is refused with
[`E-TRC-002`](diagnostics.md) under `mur run` and `mur trace`,
[`E-EVAL-002`](diagnostics.md) under `mur eval`, and
[`E-RUN-022`](diagnostics.md#e-run-022) under `mur watch`, `mur cancel` and `mur stop`.

---

## Running-capsule records { #running-capsule-records }

A capsule that opens an [A2A door](../how-to/capsules-a2a-messaging.md) writes one record of where that door is, and
removes it when the session ends.

| Property | Value |
|---|---|
| Location | `~/.murmur/running/<session_id>.json` |
| Directory mode | `0700` |
| File mode | `0600` |
| Written by | The runtime, once the capsule is serving its door and just before `mur run` prints its URL |
| Removed by | The runtime, when the session ends |

A session whose manifest declares `control:` also holds its control token beside its record, at
`~/.murmur/running/<session_id>.control`, mode `0600`, removed with the record — see
[Control surface](control-surface.md#token).

Each record carries the session id, the capsule address, the process id and its start time, the
capsule name and version, the session workdir, whether the session outlives its launcher, and the
time it started. It carries nothing from the environment: no API key, no granted variable.

A session that is a [member of a formation](#mur-run-formation) also records its formation id, as
`formation_id`. A session in no formation writes no such key. The id groups records; it grants
nothing. A record whose `formation_id` is not a formation id is unreadable, and is removed on the
next read like any other unreadable record.

A session whose manifest declares [`network.authentication`](manifest.md#field-network-authentication)
also records its operator token, as `door_token`. [`mur ps`](#mur-ps), [`mur stop`](#mur-stop),
[`mur watch`](#mur-watch) and [`mur cancel`](#mur-cancel) present it to the door when they name the
session by address. `mur ps` never prints it.

Taken together the records are a map of every reachable capsule on the machine, readable by
anything running as the same user. The `0700` directory and `0600` files are what keep that map
owner-only, and both modes are reapplied on every write.

### A record is a hint { #running-record-is-a-hint }

Nothing can guarantee a record is removed — a capsule killed outright writes no farewell — so every
read verifies it in three layers, and a session is reported as running only when all three hold.

| Layer | Question | Alone it proves |
|---|---|---|
| 1 | Is a process holding that process id? | Little: process ids are handed out again |
| 2 | Did that process start when the record says it did? | That the process id was not reused by something unrelated |
| 3 | Does the capsule's agent card answer, naming that session? | That the capsule is the one being addressed and can still respond |

Layer 3 reads the public card, or, for a record holding a `door_token`, the
[extended card](agent-card.md#extended-card) with that token.

Reading the records removes a record only on evidence that its process is gone — that is the only
sweep there is, and it is enough because a record is never treated as truth.

| Reading | The record | Signalled by `mur stop` |
|---|---|---|
| Layer 1: no process holds the process id | Removed | No |
| Layer 2: the process started at another time | Removed — the process id was reused | No |
| Layer 2: the process's start time could not be read | Kept, reported as unreachable | No |
| Layer 3: the agent card did not answer for the session | Kept, reported as unreachable | Yes |

A kept record names a process that is alive, and the command reports
[`E-RUN-023`](diagnostics.md#e-run-023) instead of throwing the address away. A capsule started by
`mur` v0.4.0 or earlier fails layer 3 until it ends: this version cannot read a session from
[its agent card](agent-card.md#no-supported-interfaces).

When `~/.murmur/running/` itself cannot be read, every command that reads the records fails with
[`E-RUN-028`](diagnostics.md#e-run-028) and removes nothing.

Ordinals count over records that pass layers 1 and 2, so a capsule that has stopped never shifts
the numbering of the ones still running.

### Whether a capsule outlives its launcher { #outlives-launcher }

The `outlives_launcher` field is read from whether the launching process has a controlling
terminal, which `/dev/tty` answers regardless of where the streams were redirected.

| Launch | Controlling terminal | `outlives_launcher` |
|---|---|---|
| Started in a terminal window | Yes | `false` — the capsule ends with that window |
| Started with no terminal attached — `nohup`, a service manager, a detached session | No | `true` |

---

## `mur new` { #mur-new }

`mur new` has two forms:

| Form | Writes | Available |
|---|---|---|
| [`mur new --roster <NAME>`](#mur-new-roster) | `./<NAME>/`: a formation of two capsules, from fixed templates | In every build |
| [`mur new "<task description>"`](#mur-new-task) | `./murmur.yaml`, generated by a model | Beta: only in a `mur` built with the `beta-mur-new` feature, and only after `mur beta enable mur-new` |

`--roster` cannot be combined with `<task description>` or `--registry`; together they are a usage
error, exit status 2.

### `mur new --roster` { #mur-new-roster }

Write a new directory holding a formation that [roster admission](roster.md#admission) accepts
and [`mur run --roster`](#mur-run-roster) launches without editing. The files are fixed templates
filled in with the name and the `inference:` block of `~/.murmur/config.yaml`: no model is called,
no key is read, and no registry or network is contacted.

```bash
mur new --roster <NAME>
```

| Argument / Flag | Required | Description |
|---|---|---|
| `--roster <NAME>` | yes | The formation's name: the directory written, and the prefix of each member's capsule name. `<NAME>`, `<NAME>-lead` and `<NAME>-worker` must all be artifact names: lowercase letters, digits and `-`, not starting or ending with `-`, at most 100 characters. Anything else is `E-NEW-002` |

#### Layout { #mur-new-roster-layout }

```text
<NAME>/
  roster.yaml          the formation: lead is the entry member, and lead may call worker
  lead/murmur.yaml     the entry member, capsule <NAME>-lead
  worker/murmur.yaml   the member lead may call, capsule <NAME>-worker
```

| Convention | Value |
|---|---|
| Formation directory | `./<NAME>/`, the directory `mur run --roster <NAME>` takes |
| Roster | `<NAME>/roster.yaml`, at the directory's root |
| Member sources | `<NAME>/<member>/murmur.yaml`: one subdirectory per member, named after the member |
| Capsule names | `<NAME>-<member>`, so two scaffolded formations never share a capsule name |
| Versions | `0.1.0` for every capsule |
| Install scope | The global store, `mur install -g`. The directory has no top-level `murmur.yaml`, so `mur install` without `-g` refuses there |

#### Generated files { #mur-new-roster-files }

Every key carries a YAML comment saying what it does, and every top-level key's comment links its
section of this reference.

`roster.yaml`:

| Key | Value |
|---|---|
| [`roster_version`](roster.md#fields) | `1` |
| [`members[0]`](roster.md#members) | `name: lead`, `capsule: <NAME>-lead`, `version: 0.1.0` |
| [`members[0].entry`](roster.md#entry-member) | `true` |
| [`members[1]`](roster.md#members) | `name: worker`, `capsule: <NAME>-worker`, `version: 0.1.0` |
| [`reachability`](roster.md#reachability) | One rule: `from: lead`, `to: [worker]`. Its comment explains that `all` would grant nothing here: `all` pairs only members that both serve peers, and `lead` does not |

`lead/murmur.yaml` and `worker/murmur.yaml`:

| Key | `lead` | `worker` |
|---|---|---|
| [`name`](manifest.md#field-identity) | `<NAME>-lead` | `<NAME>-worker` |
| [`version`](manifest.md#field-identity) | `0.1.0` | `0.1.0` |
| [`runtime`](manifest.md#artifact-manifest) | `capsule` | `capsule` |
| [`execution`](manifest.md#artifact-manifest) | `static`: `mur build` packs only the `murmur.yaml` | `static` |
| [`artifacts[0].name`, `.version`](manifest.md#field-artifacts) | The provider's driver, from [Provider](#mur-new-roster-provider) | The same |
| [`artifacts[0].runtime`](manifest.md#field-artifacts) | `driver` | `driver` |
| [`artifacts[0].gateway.endpoint`](manifest.md#artifact-gateway) | The provider's endpoint | The same |
| [`artifacts[0].gateway.api_key`](manifest.md#gateway-api-key) | The provider's key reference, never a key | The same |
| [`inference.transport`](manifest.md#field-inference) | `http` | `http` |
| [`inference.model`](manifest.md#field-inference) | The provider's model | The same |
| [`inference.driver.artifact`](manifest.md#field-inference) | The provider's driver | The same |
| [`inference.system_prompt`](manifest.md#field-inference) | Names the member and the formation, and the hand-off: give the task to `worker` with [`call-member`](runtime-provided-tools.md#call-member), end the turn while it works, and answer with the result when its answer arrives | One sentence naming the member and the formation |
| [`lifecycle.task_acceptance`](manifest.md#field-lifecycle) | `single`: the formation's one task | `queue` |
| [`lifecycle.after_task`](manifest.md#field-lifecycle) | `exit`, which stops the formation | `sleep`: it waits at its door while the formation runs |
| [`capabilities.network.allow`](manifest.md#field-capabilities) | `[localhost]`, the entry through which `call-member` reaches `worker`'s door — see [Giving a member work](roster.md#member-calls) | Absent: `worker` declares no `capabilities` |
| [`exports.peer_tasks.accept`](manifest.md#field-exports-peer-tasks) | Absent: `lead` serves no peer | `true`, because the rule calls `worker` |
| [`network.authentication.scheme`](manifest.md#field-network-authentication) | `bearer`, because a roster with a rule needs every member's door authenticated | `bearer` |

Neither member declares `capabilities.spawn.allow`, which `network.authentication` refuses.
Without `lead`'s `capabilities.network.allow`, every `call-member` call fails and the launch prints
[`W-RUN-008`](diagnostics.md#w-run-008).

#### Provider { #mur-new-roster-provider }

The provider comes from `inference.provider`, `inference.endpoint` and `inference.model` in
`~/.murmur/config.yaml`. Only the global file is read, and `mur new --roster` never writes it.
`inference.api_key` is ignored. An empty or absent field takes the provider's default.

| `inference.provider` | Driver | Default endpoint | Default model | `gateway.api_key` |
|---|---|---|---|---|
| `anthropic`, empty, or no `inference:` block | `murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}` | `https://api.anthropic.com` | `claude-haiku-4-5-20251001` | `${ANTHROPIC_API_KEY}` |
| `openai` | `murmur-driver-openai@{{ v.murmur_driver_openai }}` | `https://api.openai.com` | `gpt-4o-mini` | `${OPENAI_API_KEY}` |
| Any other value | Refused with `E-NEW-004` | — | — | — |

A configured `inference.endpoint` must pass [`gateway.endpoint` validation](manifest.md#gateway-endpoint-validation),
or the command refuses with `E-MAN-003` naming `inference.endpoint`.

#### Output { #mur-new-roster-output }

On success, stdout lists the files written, then the steps that install and launch the
formation. For `mur new --roster crew` with no `inference:` block:

```text
Scaffolded formation 'crew' in ./crew
  crew/roster.yaml          lead is the entry member; lead may call worker
  crew/lead/murmur.yaml     capsule crew-lead@0.1.0
  crew/worker/murmur.yaml   capsule crew-worker@0.1.0, serves peers

Next:
  mur install -g murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}
  mur build crew/lead && mur install -g crew/lead/crew-lead-0.1.0.mur.zip
  mur build crew/worker && mur install -g crew/worker/crew-worker-0.1.0.mur.zip
  export ANTHROPIC_API_KEY=...
  mur run --roster crew --task "<your task>"
```

| Step | Does |
|---|---|
| `mur install -g <driver>@<version>` | Installs the driver both members declare |
| `mur build <NAME>/<member> && mur install -g …` | Packs each member and installs it into the global store, where admission finds it |
| `export <KEY>=...` | Supplies the key `gateway.api_key` references. `mur config set -g credentials.<KEY> <key>` stores it instead — see [`gateway.api_key` resolution](manifest.md#gateway-api-key) |
| `mur run --roster <NAME> --task "…"` | Admits and launches the formation, with `lead` as the entry member |

None of the steps changes a generated file.

#### Writing { #mur-new-roster-writing }

1. `<NAME>` and both capsule names are checked. A refusal writes nothing.
2. `./<NAME>` is refused if anything exists at that path: a directory, a file or a symlink.
3. Every file is rendered and parsed: `roster.yaml` as a roster, each `murmur.yaml` as a capsule
   manifest. A refusal writes nothing.
4. The files are written into a new `./.<NAME>.tmp-<id>`, which is then renamed to `./<NAME>`. A
   failure at any step removes the temporary directory.

**Error codes:**

| Code | Meaning |
|---|---|
| `E-NEW-002` | `<NAME>`, `<NAME>-lead` or `<NAME>-worker` is not an artifact name. The message quotes the name and the reason |
| `E-NEW-003` | `./<NAME>` already exists |
| `E-NEW-004` | `inference.provider` names a provider other than `anthropic` or `openai` |
| `E-MAN-003` | `inference.endpoint` is not a usable `gateway.endpoint` |
| `E-IO-003` | `~/.murmur/config.yaml` could not be read or parsed, or a file could not be written |

### `mur new "<task description>"` (beta) { #mur-new-task }

!!! warning "Beta feature"

    This form exists only in a `mur` built with the `beta-mur-new` feature, and runs only after
    `mur beta enable mur-new`. Without the flag, `mur new "<task description>"` exits 1 and names
    the command that enables it.

Generate a ready-to-refine `murmur.yaml` in the current directory from a plain-language task description. `mur new` cold-boots a short-lived generator capsule backed by Claude, which searches the artifact registry and produces a manifest tailored to the task.

```bash
mur new "<task description>" [--registry <URL|local>]
```

| Argument / Flag | Required | Description |
|---|---|---|
| `<task description>` | yes | Plain-language description of what the capsule should do |
| `--registry` | no | Registry to search for artifacts — `"local"` scans `~/.murmur/artifacts/`; a URL fetches that index; omit for the public index |

**Prerequisites:**

- Inference provider configured — detected in this order:
    1. `inference:` section in `~/.murmur/config.yaml` (recommended)
    2. `ANTHROPIC_API_KEY` env var (uses `claude-haiku-4-5-20251001` by default)
    3. `OPENAI_API_KEY` env var (uses `gpt-4o-mini` by default)
    4. Interactive first-run wizard (requires a TTY; saves result to `~/.murmur/config.yaml`)

    `mur new` reads and writes the global file only — it does not consult or write the
    project-level `<cwd>/.murmur/config.yaml` file described in
    [Configuration files](config.md#configuration-files).
- The generator's own artifacts must be installed:
    - the driver for your chosen provider — `murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}` or `murmur-driver-openai@{{ v.murmur_driver_openai }}`
    - `murmur-tool-registry-search@{{ v.murmur_tool_registry_search }}`
    - `murmur-tool-editor@{{ v.murmur_tool_editor }}`
    - `murmur-skill-create-manifest@{{ v.murmur_skill_create_manifest }}`

Install missing artifacts with `mur install <name>@<version>`. `mur new` exits with a clear
error and install hint naming the first artifact it cannot resolve.

**Output:**

- `murmur.yaml` written to the current working directory
- Nothing is written if the generator fails or produces invalid YAML

**Examples:**

```bash
# Generate a manifest for a PR security review capsule
mur new "review this PR for security issues"

# Use locally installed artifacts (ensures generated versions are available)
mur new "summarise a document" --registry local

# Research/report task — generator adds a spawn capability
mur new "research climate change trends and produce a report"
```

**Single capsule vs orchestrator:**

The generator automatically infers whether the task is:

- **Single capsule** — focused, bounded task (e.g. "review this PR", "summarise a document"): produces a minimal manifest without `spawn`.
- **Orchestrator** — research, multi-step, pipeline, or report tasks: adds a `capabilities.spawn` block so the capsule can spawn child capsules.

**Generator behavior:**

The generator reads its manifest guide, calls `murmur-tool-registry-search` to find artifacts for the task, and writes the manifest to `out/murmur.yaml` in its own session workdir. The CLI reads that file, validates the YAML, then writes it to `murmur.yaml` in the current directory through a temporary file and a rename, so an interrupted run leaves no partial manifest. A generator that writes no `out/murmur.yaml` fails with `E-NEW-001`, quoting whatever the agent did produce.

If the generated YAML fails validation, nothing is written to CWD and the error is printed to stderr.

**The generated manifest is a starting point.** Review it and refine versions, capabilities, and inference settings before running.

**After generation:**

```bash
mur build .           # package the capsule
mur run --manifest murmur.yaml  # run it locally
```

**Error codes:**

| Code | Meaning |
|---|---|
| `E-CFG-001` | No inference provider configured and wizard cannot run in non-interactive mode |
| `E-RUN-008` | A required generator artifact is not installed |
| `E-MAN-002` | Generated YAML failed structural validation |
| `E-NEW-001` | The generator produced no `out/murmur.yaml` |
| `E-IO-003` | `out/murmur.yaml` could not be read, or `murmur.yaml` could not be written to the current directory |

See the `mur new` how-to guide for a full walkthrough.

---

## `mur build`

Build a `.mur.zip` artifact. Two modes: standard build from a source directory, or skill packaging from an external skill folder or zip.

### Standard build

```bash
mur build [source] [--output <path-or-dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `source` | `.` | Source directory containing `murmur.yaml` |
| `--output` | `<source>/<name>-<version>.mur.zip` | Output path or directory |

```bash
mur build .
# Built artifact: ./my-capsule-0.1.0.mur.zip
```

- Reads `murmur.yaml` (requires `name` and `version` fields)
- Scans `murmur.yaml` for literal secret patterns and emits warnings
- Packages **`murmur.yaml` plus exactly the files listed in `requires_files:`** — the rest of the
  source directory (`src/`, `Cargo.toml`, `README.md`, editor files, build output) is not packaged.
  `mur build` never compiles anything, so a `.wasm` payload must already exist on disk and be
  declared. A native or static artifact that declares no `requires_files:` builds to a
  manifest-only archive; a **wasm** artifact does not — with no root `*.wasm` to pack it fails
  with [`E-BLD-003`](diagnostics.md#e-bld-003).
- Validates the manifest `name:`, the `requires_files:` paths and the resulting payload shape,
  and warns about redundant or misplaced declarations — see [Build Lints](diagnostics.md)
- Output written **inside the source directory** unless `--output` is specified

### Skill packaging (`--skill`)

Package an externally sourced skill — a folder or `.zip` containing `SKILL.md` — into a `.mur.zip` artifact without authoring a `murmur.yaml` first.

```bash
mur build --skill [<name>] <path> [--version <version>]
```

| Argument / Flag | Description |
|---|---|
| `--skill` | Enable skill-packaging mode |
| `<name>` | Optional explicit artifact name. Omit to infer from the folder or filename. |
| `<path>` | Path to a folder or `.zip` containing `SKILL.md` (case-insensitive). Defaults to `.` |
| `--version` | Artifact version for the generated manifest. Default: `0.1.0` |

**Name inference** — when `<name>` is not provided, the artifact name is derived from the last path component:

1. Strip trailing `/` so `foo/` resolves to `foo`
2. Strip `.zip` extension (case-insensitive)
3. Lowercase
4. Replace any non-ASCII-alphanumeric, non-hyphen character with `_`
5. Collapse consecutive underscores to one
6. Strip leading/trailing `_` and `-`

Examples: `my-coding-skill/` → `my-coding-skill`; `My Skill.zip` → `my_skill`

**`<name>` vs `<path>` disambiguation** — if the value immediately following `--skill` contains `/`, `\`, or starts with `.`, it is treated as the input path (name inferred); otherwise it is the explicit artifact name and `<path>` is the next positional argument.

**`murmur.yaml` handling:**

- **Absent** — a three-field manifest is generated: `name`, `version`, `runtime: skill`
- **Present** — used unchanged; the `runtime` field must be `skill` or the build fails with `E-MAN-003`

**Output location** — always written to CWD (not the source directory). Written atomically via a temp file + rename, so a failed write never leaves a partial zip.

```bash
# Infer name from folder
cd /tmp && mur build --skill my-skill/
# Built artifact: /tmp/my-skill-0.1.0.mur.zip

# Explicit name and version
mur build --skill wrapped-skill --version 1.2.0 my-skill/
# Built artifact: ./wrapped-skill-1.2.0.mur.zip

# Zip input
mur build --skill external-skill.zip
# Built artifact: ./external-skill-0.1.0.mur.zip
```

**Error cases:**

| Code | Meaning |
|---|---|
| `E-IO-001` | `SKILL.md` not found in the input folder or zip (case-insensitive search found no match) |
| `E-MAN-002` | `murmur.yaml` present but YAML is malformed |
| `E-MAN-003` | `murmur.yaml` present but `runtime` is not `skill` |

See also: [Package a skill into an artifact](../how-to/package-skill-artifact.md), [mur publish](#mur-publish), [mur install](#mur-install)

---

## `mur publish`

Publish an existing artifact.

```bash
mur publish [artifact_path] [--registry <url>] [--platform <os-arch>]
```

- If `artifact_path` is omitted, CLI infers `<name>-<version>.mur.zip` from local `murmur.yaml`
- `--registry` forces remote mode for this command
- `--platform` overrides the platform tag (format: `os-arch`, e.g. `darwin-aarch64`). When omitted for a native artifact (`implementation: native` in the zip's `murmur.yaml`), the platform is **auto-detected** from the current build host. WASM artifacts publish without a platform tag regardless.

Example — WASM artifact (no platform tag):

```bash
mur publish my-tool-0.1.0.mur.zip
```

```text
Published my-tool@0.1.0
```

Example — native artifact (auto-detected platform):

```bash
mur publish my-native-tool-0.1.0.mur.zip
```

```text
Platform: darwin-aarch64 (auto-detected)
Published my-native-tool@0.1.0
```

Reserved versions rejected:

- `latest`
- `stable`
- `edge`

---

## `mur install`

Fetch and install artifacts from configured registry sources. `mur install` is the canonical way to seed a project's dependencies before `mur run` — equivalent to `npm install` or `cargo fetch` in those ecosystems.

```bash
# Install all artifacts declared in the project manifest (reads murmur.yaml in or above CWD)
mur install

# Install a specific artifact by name@version from the configured registry
mur install <name@version>

# Install from a GitHub source directly
mur install github:<owner>/<repo>@<tag>

# Install into the global store (~/.murmur/artifacts/) instead of the project store
mur install -g <ref>

# Download all platform variants into the global store (CI / cross-platform seeding)
mur install --all-platforms <name@version>
```

| Form | Behavior |
|---|---|
| `mur install` (no args) | Reads `murmur.yaml` in or above CWD; fetches all declared artifacts in parallel into the project-local store (`.murmur/artifacts/` next to the manifest) |
| `mur install <name@version>` | Fetches a specific artifact into the project-local store |
| `mur install github:<owner>/<repo>@<tag>` | Fetches directly from a GitHub release into the project-local store |
| `mur install -g <ref>` | Fetches into the global store (`~/.murmur/artifacts/`) |
| `mur install --all-platforms <name@version>` | Downloads all platform variants into the global store, filing each under its own platform tag; useful for CI and cross-platform build seeding |
| `mur install --registry <url\|local> <ref>` | Resolves `name@version` against that registry for this invocation — a URL forces remote mode, `local` forces the local store. See [Registry selection rules](config.md#registry-selection-rules) |
| `mur install --no-precompile` | Installs without compiling. Without the flag, `mur install` compiles each WASM tool, driver and hook it installs into [`~/.murmur/compiled/`](config.md#murmur-home-permissions), for this machine and this `mur` build, so the first `mur run` loads it instead of compiling it. With no arguments that includes each manifest artifact already in the store, and one whose compiled form is already there is not compiled again. A compile that fails does not fail the install or change its output; that artifact compiles on its first launch |

`mur install` (no args) is the standard pre-run step. It reads `murmur.yaml`, resolves every artifact listed in it, and if an artifact is not found in the local registry it falls back to the configured source chain automatically.

Example — seed a project before running:

```bash
mur install
mur run
```

Example — install a specific artifact:

```bash
mur install murmur-tool-git@{{ v.murmur_tool_git }}
```

Behavior:

1. Resolve the artifact from the registry — the local store by default, a remote Nexus with `--registry`
2. On a registry hit, verify the bytes against the SHA-256 the registry reports; on a miss, fall through to the configured source chain and download from there
3. Store into the project-local store (or global store with `-g`)
4. Pin the name, resolved version and SHA-256 in `murmur.lock` as [`origin: operator`](workdir.md#lock-origin) — project installs only, since `-g` has no project to pin. An entry a capsule pinned through `manage.pull()` is adopted as operator-declared, with one line after the lock is written:

    ```text
    Adopted some-tool@1.2.3 as operator-declared (it was pulled at runtime by session ses_0190a1b2c3d4...)
    ```

5. Compile each WASM tool, driver and hook for this machine into `~/.murmur/compiled/`, unless `--no-precompile` is given (see the table above)

---

## `mur precompile`

Compile `.mur.zip` files for this machine into [`~/.murmur/compiled/`](config.md#murmur-home-permissions), so the first `mur run` that stages them loads the compiled form instead of compiling. It reads only the files it is given: it resolves nothing, installs nothing and writes no `murmur.lock`. [`mur deploy run`](#mur-deploy-run) runs it on the target.

```bash
mur precompile [--workdir <dir>] [--json] <ZIP>...
```

| Flag | Default | Description |
|---|---|---|
| `<ZIP>...` | — | One or more `.mur.zip` files (required) |
| `--workdir` | — | The `--workdir` of the `mur run` that will launch these artifacts. When `~/.murmur` is inside it, nothing is stored, as that launch would neither read nor write `compiled/` |
| `--json` | off | Print the report as one JSON object on stdout |

Each WASM tool, driver and hook is compiled under this machine's `mur` build and [`MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES`](../concepts/registry.md#artifact-integrity) value; a later launch loads the form only under the same two. Files are compiled in parallel and reported in argument order, one outcome each:

| Outcome (text) | Outcome (JSON) | Meaning |
|---|---|---|
| `compiled` | `compiled` | Compiled, and stored unless `compiled/` cannot be written or `--workdir` holds it back |
| `already stored` | `already_stored` | A usable compiled form was already in `compiled/`; nothing was written |
| `not wasm` | `not_wasm` | A native tool, a skill or a capsule; nothing to compile |
| `failed` | `failed` | The file could not be read, is not a `.mur.zip`, has no `murmur.yaml` that parses, or its WASM does not compile. Its first launch compiles it, or fails, as it would have |

**Text output** — one line per file: the outcome, then `name@version`, or the path when the file has no readable manifest.

```text
compiled        murmur-tool-echo@1.0.0
not wasm        murmur-tool-git@{{ v.murmur_tool_git }}
failed          ./broken.mur.zip
```

**JSON output** (`--json`) — exactly one line:

```json
{"mur_version":"1.0.0","decompression_ceiling":524288000,"artifacts":[{"path":"./echo.mur.zip","name":"murmur-tool-echo","version":"1.0.0","outcome":"compiled"},{"path":"./broken.mur.zip","name":null,"version":null,"outcome":"failed"}]}
```

| Field | Type | Description |
|---|---|---|
| `mur_version` | string | Version of the `mur` that compiled |
| `decompression_ceiling` | integer | The `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES` value the forms were compiled under, in bytes |
| `artifacts[].path` | string | The path as given |
| `artifacts[].name`, `artifacts[].version` | string or `null` | From the file's `murmur.yaml`; `null` when there is none that parses |
| `artifacts[].outcome` | string | One of the JSON outcomes above |

**Exit status:** `0` when no file is `failed`; `1`, after the full report, when any is.

A `mur` release older than this command exits with `error: unrecognized subcommand 'precompile'`. A deploy whose target runs such a release prints [`W-DEPLOY-001`](diagnostics.md#w-deploy-001) and continues; set `mur_version` in the manifest to a release that has `mur precompile`, or pass `--mur-binary`.

---

## `mur list`

List installed artifacts. Scope follows where you run the command.

```bash
# Inside a project directory — show the project store
mur list

# Show the global store (~/.murmur/artifacts/)
mur list -g

# Show both stores with a SCOPE column (project / global)
mur list --all

# Show only artifacts declaring a WIT interface under murmur:hook
mur list -g --contract murmur:hook
```

| Flag | Shows |
|---|---|
| _(none)_ | Project store (`.murmur/artifacts/` next to `murmur.yaml`) when inside a project directory |
| `-g` | Global store (`~/.murmur/artifacts/`) |
| `--all` | Both stores; output leads with a `SCOPE` column (`project` or `global`) |
| `--contract <PREFIX>` | Only artifacts whose recorded [WIT contracts](installing-artifacts.md#local-artifact-cache) include an interface name starting with `PREFIX`; output includes a `CONTRACTS` column naming the matches. Combines with `-g` and `--all` |

`--contract` matches the imports and the exports alike: a package version bump renames the
interface for an artifact that imports it as much as for one that exports it.

Example output (`mur list`):

```text
NAME                     VERSION  RUNTIME  PLATFORMS
murmur-driver-anthropic  {{ v.murmur_driver_anthropic }}   driver   —
murmur-tool-git          {{ v.murmur_tool_git }}    tool     darwin-aarch64
```

Example output (`mur list --all`):

```text
SCOPE    NAME                     VERSION  RUNTIME  PLATFORMS
project  murmur-driver-anthropic  {{ v.murmur_driver_anthropic }}   driver   —
global   murmur-tool-git          {{ v.murmur_tool_git }}    tool     darwin-aarch64
```

Example output (`mur list -g --contract murmur:tool`):

```text
NAME                     VERSION  RUNTIME  PLATFORMS       CONTRACTS
murmur-driver-anthropic  {{ v.murmur_driver_anthropic }}   driver   —               murmur:tool-registry/invoke@0.1.0
murmur-tool-git          {{ v.murmur_tool_git }}    tool     darwin-aarch64  murmur:tool/run@0.1.0
```

---

## `mur doctor`

Check that every artifact declared in the current project's `murmur.yaml` is available to a session — the same project-store-then-global-store, current-platform resolution `mur run` performs before staging.

```bash
mur doctor [--bind <ADDR>]
```

| Flag | Default | Description |
|---|---|---|
| `--bind` | `127.0.0.1` | The address `mur run --bind` would bind the capsule's door on. With an address off loopback and no [`network.authentication`](manifest.md#field-network-authentication), prints [`W-SEC-032`](diagnostics.md#w-sec-032) |

It walks up from the current directory to find `murmur.yaml` (same walk `mur install` uses), loads it, and prints one checklist line per declared artifact:

| Line | Meaning |
|---|---|
| `✓ name@version   <platform>` | A native tool whose `bin/<name>` binary was read and identified as this host's platform |
| `✓ name@version   platform-independent` | The payload runs the same on every platform: a skill, a WASM tool, a driver, a hook |
| `✓ name@version   platform unverified` | A native tool whose `bin/<name>` payload is in a format the platform check does not recognise, such as a shell script |
| `✓ name@version   local source` | Declared with a `source:` path; resolved from the filesystem at stage time, never checked against a registry or a lockfile |
| `✗ name@version   <platform>   — missing` | Resolved from neither store |
| `✗ name@version   <platform>   — native binary is built for <binary-platform>, this host is <platform>` | The artifact holds a host executable this machine cannot run; `mur run` refuses it at staging with [`E-RUN-021`](diagnostics.md#e-run-021) |
| `✗ name@version   — built against <interface>, this mur serves <package>@<version>` | The artifact names an interface version this `mur` does not serve, and `mur run` refuses it at launch. The line ends `this mur serves no version of <package>` when this `mur` has no such package, and `(+N more)` follows when the artifact names several. See [Interface versions](#doctor-interface-versions) |

Every green line means the artifact resolved from the project store (`.murmur/artifacts/`) or the global store (`~/.murmur/artifacts/`), and agrees with `murmur.lock` if one is present (see below). The host platform appears on a green line only for an artifact whose binary was identified and matched.

There is no hardcoded artifact list: the checklist is derived entirely from `murmur.yaml`'s `artifacts:` block. Editing a version pin or adding/removing an artifact changes what `mur doctor` checks, with no code change.

Ahead of the checklist it prints these blocks. None of them affects the exit code.

| Block | What it reports |
|---|---|
| `AppArmor / user namespaces` | The running binary's resolved path and the profile confining it. AppArmor attaches profiles by executable path, so a `mur` run from a build output or any other unusual location is told that no profile attaches to it, and which paths the shipped profile does attach to. See [Where the user namespace comes from](containment.md#userns-grant) |
| `Filesystem preopens` | One line per `runtime: tool`, `runtime: driver` and `runtime: hook` entry, naming the directory that artifact works out of — see [The filesystem default](../concepts/access-control.md#filesystem-default). An entry whose `capabilities.filesystem.scope` `mur run` would refuse prints `<unresolved>`, with an [`E-CAP-002`](diagnostics.md#e-cap-002) warning on stderr naming it |
| `Read-only paths` | The subtrees [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) protects, and whether that protection is enforced for every call the runtime can read as a write or advisory against a named interpreter |
| `Install grant` | The `install skill` and `install tool` entries of [`capabilities.install`](manifest.md#field-install) |
| `Interface versions` | Installed artifacts built against an interface version this `mur` does not serve, printed only when there is something to report. See [Interface versions](#doctor-interface-versions) |

`Read-only paths` and `Install grant` print the same lines, in the same words, as [`mur run --explain-scope`](#mur-run).

For a capsule declaring [`capabilities.spawn.allow`](manifest.md#field-capabilities), `mur doctor` also reports whether `mur-roost` — the daemon that capsule registers with at launch — is installed and answering, naming [`E-RUN-019`](diagnostics.md#e-run-019) as the error a `mur run` would meet. It tells apart three states: the binary is not on `PATH`, `MURMUR_ROOST_URL` is not set, and nothing answers at the URL that variable names. A reachable daemon prints nothing, and a capsule declaring no `spawn.allow` gets no such line. Like the blocks above, this warning goes to stderr and never changes the exit code.

For the same capsule, `mur doctor` also prints an `Environment requirements` block: what the capsule's spawn closure — that capsule plus the transitive closure of its `capabilities.spawn.allow` — needs from the environment before its first token is spent. It resolves each named capsule to an exact version: `murmur.lock` pins it if the lockfile holds an entry for the name, otherwise the project store (`.murmur/artifacts/`) decides alone if it holds the name at all, otherwise the global store (`~/.murmur/artifacts/`). No version is guessed — a name that no single source settles is listed as one the walk could not inspect. Nothing is launched and no daemon is contacted.

The block also reports every variable the project manifest itself references as `${VAR}` and neither this shell nor the workspace `.env` sets. [`gateway.api_key`](manifest.md#gateway-api-key) on an artifact entry is the manifest field that takes such a reference. A capsule declaring no `spawn.allow` has nothing to walk, so it gets a block only when it has such a reference to report.

`mur doctor` always prints one `door:` line saying who may call the capsule's A2A door:

| Line | Manifest |
|---|---|
| `door: public (no network.authentication)` | No `network.authentication` |
| `door: bearer — operator, <name> (<scopes>), …` | `network.authentication`, naming each declared credential and its scopes |

When `--bind` is off loopback and the door is public, it also prints the [`W-SEC-032`](diagnostics.md#w-sec-032)
line, byte for byte the line `mur run --bind` prints for the same manifest. Loopback is an IPv4
address in `127.0.0.0/8`, `::1` or `localhost`.

`mur doctor` parses the project manifest without resolving what it references, so a reference this shell cannot satisfy is a line in this block rather than a refusal ahead of it. `mur run` needs the value, and refuses the same manifest with [`E-MAN-003`](diagnostics.md#index).

Four findings, of which two change the exit code:

| Finding | Line | Exit code |
|---|---|---|
| A name that neither this shell nor the workspace `.env` sets: an entry in the closure's `capabilities.env.allow`, or a variable the project manifest references | `✗ NAME   unset   — <capsule>` | non-zero, [`E-CAP-014`](diagnostics.md#e-cap-014) on stderr |
| A capsule declaring a `capabilities.env.allow` entry the capsule that spawns it does not hold — the spawn `mur-roost` refuses | `declarations mur-roost will refuse:` | non-zero, [`E-CAP-015`](diagnostics.md#e-cap-015) on stderr |
| A capsule the walk could not read: not installed, more than one version installed with nothing pinning which, or an unreadable archive | `could not inspect N of M capsules in the spawn closure:` | `0`, [`W-REG-002`](diagnostics.md#w-reg-002) on stderr |
| A `spawn.allow` edge pointing back at a capsule already on the walk | `spawn.allow cycle: a@1 → b@2` | `0` |

This block is stricter than the manifest blocks above, which report a refusal of the root capsule as a warning: a failure in the spawn closure lands at depth, after the parent has already spent tokens reaching the point of delegating.

Only names are printed. No variable's value is read into the report or written anywhere, and set/unset is decided by presence alone, so a name set to the empty string counts as set. The workspace `.env` counts because `mur run` loads it; a `.env` that cannot be parsed is reported by file and line, adds a `Fix:` entry, and leaves the variable list computed from this shell's environment alone.

A variable line names every capsule that needs the name. A name a capsule declares in `capabilities.env.allow` is attributed to that capsule alone; a name reached through any other manifest field carries that field in brackets, as `solo@0.0.1 (artifacts.murmur-driver-anthropic.gateway.api_key)`.

**Output — a spawn closure with one unset variable:**

```text
Environment requirements
  capsules: root-capsule@0.0.1, worker@0.1.0, deep-worker@0.2.0
  variables:
    ✓  ANTHROPIC_API_KEY   set     — root-capsule@0.0.1, worker@0.1.0
    ✗  WORKER_TOKEN        unset   — worker@0.1.0
```

**Output — a capsule that delegates to nobody, with one unset reference:**

```text
Environment requirements
  capsules: solo@0.0.1
  variables:
    ✗  SOLO_PROVIDER_KEY   unset   — solo@0.0.1 (artifacts.murmur-driver-anthropic.gateway.api_key)
```

**Output — happy path:**

```text
Filesystem preopens
  - murmur-driver-anthropic (driver): the whole accessible workdir — no capabilities.filesystem.scope declared
  - murmur-tool-git (tool): one subtree of the accessible workdir — capabilities.filesystem.scope: repo

Read-only paths
  read_only:
    - tests
    - bench/fixtures
  read_only enforcement: enforced for every tool call and every shell command the dispatch check can read

Checking /path/to/murmur.yaml for darwin-aarch64...
  ✓  murmur-driver-anthropic@{{ v.murmur_driver_anthropic }}   platform-independent
  ✓  murmur-tool-git@{{ v.murmur_tool_git }}            darwin-aarch64

All checks passed.
```

**Output — one or more artifacts missing:**

```text
Checking /path/to/murmur.yaml for darwin-aarch64...
  ✗  murmur-tool-git@{{ v.murmur_tool_git }}   darwin-aarch64   — missing

0 checks passed, 1 error found.

Fix: mur install murmur-tool-git@{{ v.murmur_tool_git }}
```

**Output — a native binary built for another platform:**

```text
Checking /path/to/murmur.yaml for linux-x86_64...
  ✗  murmur-tool-git@{{ v.murmur_tool_git }}   linux-x86_64   — native binary is built for darwin-aarch64, this host is linux-x86_64

0 checks passed, 1 error found.

Fix: murmur-tool-git: native binary is built for darwin-aarch64 — reinstall murmur-tool-git@{{ v.murmur_tool_git }} on this host
```

### Lock integrity

Each registry-resolved artifact is also checked against `murmur.lock` when one is present — see
[Lock integrity](../concepts/registry.md#lock-integrity). A disagreement produces one of three
failure lines:

- `✗ name@version   <platform>   — murmur.lock missing artifact entry for 'name'` — the lock exists but has no entry for this artifact
- `✗ name@version   <platform>   — murmur.lock version mismatch for 'name': manifest requested X, lock pinned Y` — the lock pins a different version than `murmur.yaml` declares
- `✗ name@version   <platform>   — artifact integrity check failed for name@version` (with `expected sha256 (murmur.lock):` / `actual sha256 (on disk):` detail lines) — the installed bytes don't hash to the lock's recorded sha256
- `✗ name@version   <platform>   — murmur.lock has no sha256 for 'name' on <platform>: it pins <platforms>` — the lock was written on another platform and has never been installed against on this one

A pin a capsule wrote through `manage.pull()` ([`origin: runtime`](workdir.md#lock-origin)) adds
one of three lines:

| Line | Meaning | Effect |
|---|---|---|
| `⚠ name@version   — pulled at runtime by session <session>` | A declared tool or skill whose pin a capsule pulled. `mur run` stages it, [marked untrusted](../concepts/access-control.md#artifact-origin) | Warning: `Fix: mur install name@version`. Exit code unchanged |
| `✗ name@version   — murmur.lock pins 'name' from a runtime pull by session <session>; murmur.yaml declares it with <declared>` | `<declared>` is `runtime: hook`, `runtime: driver`, `gateway:` or `inference.system_prompt_artifact`. `mur run` refuses it with [`E-RUN-043`](diagnostics.md#e-run-043) | Failure: `Fix: mur install name@version`. Exit `1` |
| `· name@version   pulled at runtime by session <session> — not declared in murmur.yaml, so mur run does not stage it` | A pulled entry `murmur.yaml` does not declare, printed after the checklist | None |

**Output — runtime-pulled pins:**

```text
Checking /path/to/murmur.yaml for linux-x86_64...
  ⚠  demo-skill@0.1.0   — pulled at runtime by session ses_0190a1b2c3d4
  ✗  demo-tool@0.1.0    — murmur.lock pins 'demo-tool' from a runtime pull by session ses_0190a1b2c3d4; murmur.yaml declares it with gateway:
  ·  other-tool@2.0.0   pulled at runtime by session ses_0190a1b2c3d4 — not declared in murmur.yaml, so mur run does not stage it

1 check passed, 1 error found, 1 warning.

Fix: mur install demo-tool@0.1.0
Fix: mur install demo-skill@0.1.0
```

**Output — lock hash mismatch:**

```text
Checking /path/to/murmur.yaml for darwin-aarch64...
  ✗  demo-skill@0.1.0   darwin-aarch64   — artifact integrity check failed for demo-skill@0.1.0
        expected sha256 (murmur.lock): deadbeef
        actual sha256 (on disk):       0e29c7e8c291a2800a266a01c28300e24f2a640d4a21a085e4fd9aee01adfaef

0 checks passed, 1 error found.

Fix: demo-skill: artifact on disk does not match murmur.lock — re-publish or delete the lock
```

### Murmur home { #doctor-murmur-home }

Once the manifest loads, `mur doctor` prints a `Murmur home` block: the mode of `~/.murmur` and of
each entry in it, whatever the project declares. The known entries are always listed, present or not, followed by any other name in
the directory.

```text
Murmur home (/home/alice/.murmur)
  .: 0755  the murmur home, expected owner-only
  config.yaml: 0644  provider credentials, expected owner-only
  deploy_keys: 0700  SSH private keys, expected owner-only
    wider than 0600: deploy_keys/dep_x/id_ed25519 is 0644
  deploy_staging: absent  deployment staging copies, expected owner-only
  deployments.json: 0600  deployment records, expected owner-only
  spend: 0700  the machine spend ledger, expected owner-only
  conversations: 0700  conversation records, expected owner-only
  running: absent  running-capsule records, expected owner-only
  state: 0700  capsule state stores, expected owner-only
  artifacts: 0755  installed artifacts
  compiled: 0700  compiled WASM cache, safe to delete, expected owner-only
  bin: absent  cached mur binaries
  formations: 0700  formation members' working directories, expected owner-only
  nexus-config.json: 0644  something not recognised by this build
```

| Line | Meaning |
|---|---|
| `<name>: <mode>` | The entry's permission bits. `.` is `~/.murmur` itself |
| `<name>: absent` | Nothing exists at that name |
| `<name>: unreadable (<error>)` | The entry exists and its metadata could not be read |
| `expected owner-only` | The entry is held at the mode in [`~/.murmur` modes](config.md#murmur-home-permissions), and so is everything beneath it. Under `formations`, only the `<frm_id>/` and `<member>/` levels are held: what a member's sessions write inside its owner-only member directory is not reported |
| `wider than <mode>: <path> is <mode>` | A directory beneath an owner-only entry wider than `0700`, or a file wider than `0600`. At most 20 are listed per entry, then `and N more wider than expected` |

Each owner-only entry wider than expected, and each path listed beneath one, also prints
[`W-SEC-028`](diagnostics.md#w-sec-028) on stderr. The block changes no mode and does not affect the
exit code. When `HOME` cannot be resolved,
the block is one `not reported` line.

### Roster { #doctor-roster }

When a [`roster.yaml`](roster.md) sits beside the project's `murmur.yaml`, `mur doctor` admits it
and prints a `Roster` block ahead of the checklist. It resolves every member from the project
store and then the global store, and checks each one against `murmur.lock`, as described in
[Admission order](roster.md#admission). Nothing is launched and no file is written. Without a
`roster.yaml` there is no block.

| Line | Meaning |
|---|---|
| `file: <path>` | The roster admitted |
| `<name>   <capsule>@<version>   [entry]   <serves peers\|refuses peers>   <authenticated door\|public door>` | One line per member, in roster order. `entry` marks the entry member. The last two columns read the member's [`exports.peer_tasks.accept`](manifest.md#field-exports-peer-tasks) and [`network.authentication`](manifest.md#field-network-authentication) |
| `reachability: none` | No member may call another |
| `reachability: a → b, …` | The edges the rules declare, ordered by the members' order in the roster |
| `reachability (all): a → b, …` | The edges `reachability: all` expands to |
| `✗  <message>` | The roster is refused. The member lines are not printed |

A refused roster is an error: its code goes to stderr with a hint, the hint is added as a `Fix:`
entry, and the exit code is `1`. The codes are [`E-ROS-001`](diagnostics.md#e-ros-001) to
[`E-ROS-008`](diagnostics.md#e-ros-008), and [`E-REG-005`](diagnostics.md#index) for a member
`murmur.lock` pins at another version or hash.

**Output — an admitted roster:**

```text
Roster
  file: /path/to/roster.yaml
  planner    planner@0.3.0    entry   serves peers    authenticated door
  coder      coder@1.2.0              serves peers    authenticated door
  reviewer   reviewer@0.9.0           serves peers    authenticated door
  reachability: planner → coder, planner → reviewer, reviewer → coder
```

**Output — a refused roster:**

```text
Roster
  file: /path/to/roster.yaml
  ✗  roster.yaml: member 'coder' declares no `network.authentication`, and this roster declares peer traffic, so every member's door must require authentication

Checking /path/to/murmur.yaml for linux-x86_64...

0 checks passed, 1 error found.

Fix: declare `network.authentication` in the murmur.yaml of the capsule 'coder' runs and rebuild it, or remove the peer traffic from roster.yaml. A capsule that declares network.authentication cannot also declare capabilities.spawn.allow (E-MAN-003)
```

### Interface versions { #doctor-interface-versions }

Just before the checklist, `mur doctor` checks every artifact installed in the project store
(`.murmur/artifacts/`) and the global store (`~/.murmur/artifacts/`), declared or not. It names
each one that speaks a Murmur interface at a version this `mur` does not serve. These are the
interfaces in the `murmur:` namespace, in both directions: those the artifact provides and those
it expects the host to provide. This `mur` serves exactly one version of each `murmur:` package
and accepts no other, so `mur run` refuses such an artifact at launch.

| Compared | Not compared |
|---|---|
| Every `murmur:` interface name, against the one version this `mur` serves of its package. A name with no version is a mismatch | `wasi:` interfaces, which load against any compatible `0.2.x` version this `mur` provides. Interfaces in any other namespace |

The interface versions come from the store's record of the artifact, which is written from the
artifact's own bytes every time it is installed or published. When the record carries no
interface list, it is read from the installed payload. The check never writes to either store.

**Output — a stale artifact this project does not declare:**

```text
Interface versions
  ⚠  stale-hook@0.3.0   global    exports murmur:hook/lifecycle@0.8.0 — this mur serves murmur:hook@0.9.0
  ⚠  stale-hook@0.3.0   global    imports murmur:runtime/inference@0.3.0 — this mur serves murmur:runtime@0.4.0
  mur run refuses these at launch (warning[W-REG-003], https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-reg-003)

Checking /path/to/murmur.yaml for linux-x86_64...

0 checks passed, 0 errors found, 1 warning.

Fix: mur install -g stale-hook@<a release built against murmur:hook@0.9.0, murmur:runtime@0.4.0>
```

| Line | Meaning |
|---|---|
| `⚠ name@version   <project\|global>   <exports\|imports> <interface> — this mur serves <package>@<version>` | The artifact in that store speaks `<interface>`; this `mur` serves `<package>` only at `<version>` |
| `… — this mur serves no version of <package>` | This `mur` has no package of that name at all |
| `… <interface> (unversioned) — …` | The interface name carries no version |
| `not checked (<project\|global> store): <error>` | The store's index could not be read, so nothing in it was checked. The exit code is unchanged |

Which tier a stale artifact lands in depends on whether the next `mur run` of this project
launches it:

| Artifact | Finding | Exit code |
|---|---|---|
| Not declared in `murmur.yaml` at that version | Warning: [`W-REG-003`](diagnostics.md#w-reg-003) and `Fix: mur install [-g] name@<a release built …>` | Unchanged |
| Declared in `murmur.yaml` at that version | `✗` checklist line and `Fix: name: pin a release built against … in murmur.yaml, then run mur install` | `1` |

The global store holds artifacts for every project on the machine, so an artifact nothing here
launches is a warning. A declared one is an error because the next `mur run` refuses it. A
declared artifact that already fails the [lock checks](#lock-integrity) shows only that failure,
and an artifact declared with `source:` is not checked.

The fix is always a release built against the versions this `mur` serves. Reinstalling the same
version fetches the same build. The block prints nothing when every installed artifact speaks only
served versions.

### Warnings

A finding that is worth reporting but is not a failure prints on its own checklist line, marked
`⚠`, and adds a `Fix:` line in the same block as the errors. The summary line counts warnings
separately and the exit code ignores them, so a store that resolves everything it is asked for
still exits `0`. [`W-REG-001`](diagnostics.md#w-reg-001) is the one warning the checklist reports.
A stale artifact this project does not declare adds its `Fix:` line in the same way, from the
[Interface versions](#doctor-interface-versions) block.

**Output — a native artifact with no recorded platform:**

```text
Checking /path/to/murmur.yaml for linux-x86_64...
  ⚠  murmur-tool-git@{{ v.murmur_tool_git }}   linux-x86_64   — native artifact with no recorded platform (warning[W-REG-001])

1 check passed, 0 errors found, 1 warning.

Fix: mur install murmur-tool-git@{{ v.murmur_tool_git }}
```

**Exit codes:**

- `0` — every declared artifact resolved (or is local-source), agrees with `murmur.lock` if one is present, carries no binary built for another platform, and speaks only interface versions this `mur` serves; and the `Environment requirements` block found no unset variable and no declaration `mur-roost` will refuse. Warnings do not change this
- `1` — one or more declared artifacts missing, disagree with `murmur.lock`, have a runtime-pulled pin `mur run` would refuse with `E-RUN-043`, hold a native binary this host cannot run, or speak an interface version this `mur` does not serve (checklist printed to stdout first); or the `Environment requirements` block found an unset variable or a predicted refusal; or a setup failure (no checklist printed; error goes to stderr)

**Error codes:**

| Code | Meaning |
|---|---|
| `E-IO-001` | No `murmur.yaml` found in the current directory or any parent |
| `E-MAN-001` / `E-MAN-002` / `E-MAN-003` | Manifest failed to load — missing field, YAML syntax error, or invalid field, respectively |
| `E-RUN-003` | `murmur.lock` exists but failed to parse or validate — including a `lock_version` other than 2, which is refused rather than migrated |
| `E-RUN-021` | A declared native tool's binary is built for another platform — reported on the checklist line; `mur run` refuses the same artifact at staging |
| `E-RUN-043` | A declared artifact's pin was written by `manage.pull()` and `murmur.yaml` declares it as a hook, a driver, or with `gateway:` — reported on the checklist line; `mur run` refuses the same artifact at staging |
| `E-CAP-014` | A variable the spawn closure's `capabilities.env.allow` declares is unset in this environment |
| `E-CAP-015` | A capsule in the spawn closure declares a `capabilities.env.allow` entry the capsule that spawns it does not hold |
| `W-REG-002` | A capsule in the spawn closure could not be inspected, so what it declares is missing from the report — a warning; the exit code is unchanged |
| `W-REG-003` | An installed artifact this project does not declare speaks an interface version this `mur` does not serve — a warning; the exit code is unchanged |

A setup failure (no project found, the manifest fails to load, or the lockfile fails to parse) is reported on stderr before any checklist is printed — `mur doctor` never reports "all checks passed" against zero artifacts because the manifest or lockfile couldn't be read.

---

## `mur run`

Run capsule component and resolve declared artifacts.

```bash
mur run [--manifest <path>] [--task <path-or-text>] [--json]
mur run --roster [<path>] [--task <path-or-text>] [--json]
```

| Flag | Default | Description |
|---|---|---|
| `--manifest` | `./murmur.yaml` | Path to the capsule manifest |
| `--roster` | `./roster.yaml` when the flag is given with no value | Launch the formation a [roster](roster.md) declares, for one task. Takes the project directory or the `roster.yaml` inside it; any other file name refuses with [`E-ROS-001`](diagnostics.md#e-ros-001). Passes `--task`, `--json`, `--verbose`, `--no-env-file` and `--containment` through to its members, and cannot be combined with any other flag. See [Launching a formation](#mur-run-roster) |
| `--capsule` | — | Run an installed registry artifact by name instead of a project directory. Requires `--capsule-version`, and cannot be combined with an explicitly given `--manifest`. The capsule is resolved from the project store and then the global store, and staged from the artifact bytes in memory: no `murmur.yaml` is read from disk, and no `murmur.lock` is read or written. This is the form a parent capsule's runtime launches a delegated child on |
| `--capsule-version` | — | Version of the `--capsule` artifact. Required with `--capsule` |
| `--spawn-grant-stdin` | off | Read one line from standard input as this launch's spawn approval, and present it when the session registers with `mur-roost`. Set by a parent capsule's runtime when it launches a delegated child. Standard input rather than an argument or an environment variable, both of which any process running as the same user can read out of `/proc` |
| `--task` | — | Written to the capsule workdir as `task.md` before launch. An existing file path is copied; any other value is written verbatim as UTF-8 text |
| `--context` | a fresh `ctx_…` per task | Context id this run's task runs under. Two runs given the same id continue one [conversation record](workdir.md#the-conversation-record), whichever session directory each got. One path segment: no `/`, no `.` or `..`, not absolute, not starting with a dot — anything else refuses the launch with [`E-CAP-011`](diagnostics.md#e-cap-011) |
| `--resume` | `@1` when the flag is given with no value | Session whose conversation this run continues, as a [session address](#session-addresses). Resolves that session's context id and runs under it, so it is `--context` with the id looked up for you. Loads the [conversation record](workdir.md#the-conversation-record) — or, under [`transport: process`](manifest.md#transport-process), hands the harness the session id that context's [harness session map](workdir.md#harness-session-map) holds — even when the capsule declares `lifecycle.conversation: stateless`. A context with neither refuses the launch with [`E-RUN-017`](diagnostics.md#e-run-017). Passing it together with `--context` refuses the launch with [`E-RUN-015`](diagnostics.md#e-run-015). Reads the named session's `trace.jsonl` only as far as its first task, so a session whose process was killed — leaving the file ending mid-record — still resumes, while [`mur trace show`](#mur-trace-show) over that same file reports [`E-TRC-001`](diagnostics.md) |
| `--resume-mode` | `full` | How `--resume` puts the loaded conversation in front of the model. `full` loads the record verbatim; `compact` runs the capsule's `on-compaction` hook over it first and continues from the summary, which is the answer when the conversation would not fit the context window at all. `full` is often the cheaper of the two: a verbatim reload can hit the provider's prompt cache, while compaction changes the prefix from the first altered token, guarantees a cache miss, and costs an extra inference call to produce the summary. `compact` with no hook bound to `on-compaction` refuses the launch with [`E-RUN-018`](diagnostics.md#e-run-018), and `compact` under [`transport: process`](manifest.md#transport-process) — where the harness holds the history and murmur has none to summarize — with [`E-RUN-037`](diagnostics.md#e-run-037) |
| <span id="run-forget-session">`--forget-session`</span> | off | Drop the harness session `--context` names, then run this launch's first task as a new conversation under the same context id. The answer to [`E-RUN-036`](diagnostics.md#e-run-036), where the harness no longer holds the conversation a context names and every later task in it fails the same way. The entry is deleted from the [harness session map](workdir.md#harness-session-map) and the run's trace records a `harness_session_forgotten` event naming the context, the id that was dropped and `requested_by: "cli"`; asked for a context with no entry, it drops nothing and records nothing. Applies to the launch's first task and to no later one. Requires `--context`, and cannot be combined with `--resume`, which asks for the opposite. Only [`transport: process`](manifest.md#transport-process) has a harness session, so every other transport refuses the launch with [`E-RUN-039`](diagnostics.md#e-run-039) |
| `--workdir` | `<manifest-dir>/workdir/<session-id>` | Directory mounted as the capsule's accessible workspace. When passed, session artifacts are created inside it under `.murmur/<session-id>`. See [Session workdir](workdir.md) |
| `--bind` | `127.0.0.1` | Address the capsule's HTTP server binds. Use `0.0.0.0` to accept connections from other machines. An address off loopback on a capsule declaring no [`network.authentication`](manifest.md#field-network-authentication) prints [`W-SEC-032`](diagnostics.md#w-sec-032) on stderr |
| `--json` | off | Emit launch info as a single JSON line instead of human-readable output. Takes precedence over `--verbose` |
| `--verbose`, `-v` | off | Add `workdir:`, `manifest:`, `driver:` and `skills:` to the startup lines, and `formation:` for a [formation member](#mur-run-formation) |
| `--lifecycle-task-acceptance` | — | Override `lifecycle.task_acceptance` (`none`\|`single`\|`queue`) |
| `--lifecycle-after-task` | — | Override `lifecycle.after_task` (`exit`\|`sleep`). The resolved value is the one [`W-SEC-024`](diagnostics.md#w-sec-024) reports, so `--explain-scope` previews the warning this override produces; a value outside the two is refused there as on a real run |
| `--no-env-file` | off | Skip auto-loading the workspace-root `.env` file for this invocation. Recommended default for CI/CD pipelines |
| `--containment` | — | Require at least this containment class (`advisory`\|`scoped`\|`sealed`). Combines with the manifest's `capabilities.containment` and the workspace `containment` config by taking the strongest of the three — this flag can only raise the effective floor, never lower one another source already set. See [Containment class](containment.md#field-containment) |
| `--explain-scope` | off | Print the effective grant set and the declared/achieved containment classes, then exit `0` without staging or launching anything — no registry pull, no component compile, no workdir. Reports even when the declared floor is not met. The `Resource plane` block, and `io_max` under `--json`, report whether the declared [`cgroup_io_bytes_per_sec`](resource-limits.md#io-max-report) ceiling applies on this host. Also lists every path the runtime writes into the workdir, as `runtime_writes` under `--json` — see [Session workdir](workdir.md). Unless the session composes a sealed root, a `Not protected here` block prints under `Containment`, naming what its filesystem mechanism leaves unrestricted and why a `~`-based probe is not evidence of containment — see [Testing containment honestly](containment.md#testing-containment); `--json` carries the same statements as `filesystem_boundary` |
| `--system-prompt` | — | Replace the manifest's system prompt for this invocation only. Overrides `inference.system_prompt`, `inference.system_prompt_file` and `inference.system_prompt_artifact` alike, whichever the manifest used — and applies just as well when it declared none. The value is trimmed; an empty or whitespace-only value clears the prompt rather than setting one. `murmur.yaml` is not modified. Requires an agent capsule: on a manifest with no `inference:` block the run fails with `error[E-IO-003]` before anything is staged. Inert under `--explain-scope`. See [Override the prompt for a single run](../how-to/capsule-system-prompt.md#step-8-override-the-prompt-for-a-single-run) |

For the output modes, the read-only pre-flight checks and driving a capsule over HTTP, see
[Run a capsule from the CLI or from another program](../how-to/different-ways-to-run-murmur.md).

- Auto-loads `.env` from nearest workspace containing `murmur.yaml`, unless `--no-env-file` is passed
- Creates/uses `murmur.lock` in manifest directory, except under `--capsule`, which has no project directory to hold one

**Door tokens.** A capsule declaring [`network.authentication`](manifest.md#field-network-authentication)
prints the tokens its runtime minted, on stdout only, once the door is up:

| Mode | Output |
|---|---|
| Human | One `murmur: token <name> <token>` line per token, right after `murmur: url`: `operator` first, then the declared credentials by name |
| `--json` | The readiness line gains `"tokens": {"operator": "<token>", "<name>": "<token>", …}`. A capsule declaring no `network.authentication` prints the line without the key |

```text
murmur: url localhost:41873
murmur: token operator mdt1.eyJjcmVkZW50aWFsIjoib3BlcmF0b3IiLC4uLn0.Zk9x…
murmur: token watcher mdt1.eyJjcmVkZW50aWFsIjoid2F0Y2hlciIsLi4ufQ.q2Lr…
session: ses_019f01a940ce7761854e768ecbe3d399
```

A token is valid until the session ends. A line carrying a token that standard output refuses is
dropped, never written to `logs/bootstrap.log`. What each token reaches is in
[Agent Card: Security](agent-card.md#tokens).

<span id="mur-run-formation"></span>**Formation membership.** A session launched with
[`MURMUR_FORMATION_ID`](roost-api.md#environment-variables) set to a formation id is a member of
that formation, and every child it delegates to joins the same one. A formation id is `frm_`
followed by 32 lowercase hex digits; [`mur run --roster`](#mur-run-roster) mints one and hands it
to every member it starts. A session launched without it belongs to no formation and prints neither
of these:

| Mode | Output |
|---|---|
| Human | `--verbose` adds `formation: <formation-id>` after the other startup lines |
| `--json` | The readiness line gains `"formation_id": "<formation-id>"` |

```json
{"formation_id":"frm_019f01a93ff27c1e9a3b5d0c4e8f2a61","name":"researcher","pid":48213,"session_id":"ses_019f01a940ce7761854e768ecbe3d399","url":"localhost:41873","version":"0.1.0","workdir":"/home/me/project/workdir/ses_019f01a940ce7761854e768ecbe3d399"}
```

The variable is read before the workspace `.env` is loaded, so a `.env` line naming it has no
effect. A value that is not a formation id refuses the launch with
[`E-RUN-044`](diagnostics.md#e-run-044). `mur run --resume` joins a formation only when its own
environment names one.

[`MURMUR_FORMATION_LIFELINE`](roost-api.md#environment-variables) is set beside it by
`mur run --roster` and is not for operators. A member launched by hand without it prints
[`W-RUN-007`](diagnostics.md#w-run-007) — see
[A member started by hand](roster.md#launch-stop).

**Registration.** A capsule whose manifest declares `capabilities.spawn.allow`, and any capsule
launched with `--spawn-grant-stdin`, registers with the daemon named by `MURMUR_ROOST_URL` at
launch and is retired from it when the session ends. A registration that cannot be completed
refuses the launch with [`E-RUN-019`](diagnostics.md#e-run-019). Every other capsule opens no
connection at all and needs no daemon running. See
[the mur-roost HTTP API](roost-api.md#post-register).

**Artifact pre-check:** Before staging, `mur run` verifies that all artifacts declared in the manifest are installed locally. If any are missing it exits immediately with `error[E-RUN-008]` and a `mur install` hint. Run `mur install` first to fetch missing artifacts.

Current runtime constraints:

- `mur run` accepts all four artifact runtimes: `tool`, `driver`, `hook`, and `skill`
- `tool` artifacts are exposed as model-callable tools; `driver`, `hook`, and `skill` artifacts are staged for runtime use but are hidden from the model's tool inventory
- `skill` artifacts install `skill.md` to `tools/<name>/skill.md` in the workdir; the agent reads them voluntarily via filesystem access
- Capsule component discovery:
  - prefers `capsule.wasm`
  - otherwise requires exactly one root `*.wasm`
  - under `--capsule`, the root component of the artifact archive, with no project directory searched
- Agent capsules require `inference.driver.artifact` in `murmur.yaml` on both transports: an [http driver](manifest.md#transport-http) under `transport: http`, a [process driver](manifest.md#process-driver) under `transport: process`. A missing driver exits with `error[E-RUN-005]`; a `transport: process` harness binary that cannot be resolved exits with `error[E-RUN-006]`

### Status and exit code { #mur-run-status }

When the session ends, `mur run` prints one `status:` line and exits:

| `status:` | Exit code | The launch |
|---|---:|---|
| `ok` | 0 | Ran, and every task that decides its outcome completed. A turn cut off at `inference.max_tokens` still completes its task: it is marked, and warned about with [`W-RUN-001`](diagnostics.md#w-run-001) |
| `failed` | 1 | Ran a task that failed — a failed driver call, a response it could not act on, a compaction hook error, a `request-input` wait past `lifecycle.input_timeout_secs` — and prints [`E-RUN-040`](diagnostics.md#e-run-040) with the reason. Also the status of a launch that could not run at all, with that error's own code |
| `max_turns_reached` | 1 | Ran a task that used every turn `inference.max_turns` allows, and prints `E-RUN-040` |
| `spend_ceiling_reached` | 1 | Ran a task a [spend ceiling](manifest.md#inference-max-session-tokens) stopped, and prints `E-RUN-040` |
| `canceled` | 1 | Ran a task that was canceled — by `tasks/cancel`, `session/stop`, [`mur stop`](#mur-stop) or `SIGTERM` — and prints `E-RUN-040` |
| `trapped` | 1 | Ran a script capsule that stopped with an error |

The tasks that decide the outcome:

- the task `--task` or `task.md` gives the launch, under every lifecycle;
- an A2A task, when the capsule ends after it (`lifecycle.after_task: exit`, or `task_acceptance: single`), or while the session is closing out.

A peer's task on a capsule that sleeps between tasks reports through `tasks/get` and its stream,
and leaves the launch's status alone. The first deciding task that did not complete sets the
status, and no later run replaces it. The trace's `session_end.exit_status` carries the same value.

`--json` prints no `status:` line; the exit code and error are the same.

### A closed stream { #mur-run-closed-stream }

Under `--json`, standard output carries one line: the readiness line, written the moment the
capsule is addressable. A supervisor that launches `mur run --json`, reads that line and closes
the pipe leaves the session running to its own end, with the exit status it would have had.
Closing standard error has the same effect.

A line the runtime could not hand to a closed stream is kept where the session keeps its other
diagnostics.

| The line was headed for | Where it lands instead |
|---|---|
| Either stream, once the session directory exists | [`logs/bootstrap.log`](workdir.md#session-workdir-files) in the session directory |
| Either stream, before the session directory exists | Nowhere: a warning raised while the manifest and its grants are still being read has no file to fall back to |

The fallback writes to that file alone, so `--json` standard output holds the readiness line and
nothing else.

### `SIGTERM` { #mur-run-sigterm }

An agent capsule started with `mur run` that receives `SIGTERM` — from [`mur stop`](#mur-stop),
`kill`, or a service manager — ends its session the way a clean exit does:

1. Every live task is cancelled, as [`session/stop`](../how-to/capsules-a2a-messaging.md#ending-the-session) cancels them, and no new task is started.
2. Each cancelled task's `task_end` is written, after its `on-task-end` hooks.
3. The session teardown runs: `on-session-end`, waiting for asynchronous hooks to finish, `shell_abandoned` records for detached commands, `session_end`, and removal of the [running-capsule record](#running-capsule-records).

| Bound | Effect |
|---|---|
| A second `SIGTERM` | The process exits at once, with status 143 |
| 20 seconds after the first `SIGTERM` | The process exits with status 143, wherever the teardown is |

A session that was running a task when the first `SIGTERM` arrived, and whose teardown finishes,
ends `status:  canceled` and exits `1`; one that was waiting for a task ends `status:  ok`. See
[Status and exit code](#mur-run-status).

A teardown cut short by either bound, or by `SIGKILL`, leaves the rest undone. A script capsule,
and every session that `mur eval run` or `mur new` runs, has no `SIGTERM` handling: the process
ends at once.

### Launching a formation (`--roster`) { #mur-run-roster }

`mur run --roster` launches every member of the formation a project's
[`roster.yaml`](roster.md) declares, runs the entry member's one task, and then stops every member.
Each member is a `mur run --capsule <capsule> --capsule-version <version>` process of its own; the
launcher runs no session itself. The order and the rules are in
[Launching a formation](roster.md#launch).

| Flag | Passed to |
|---|---|
| `--task` | The entry member |
| `--json` | The entry member. Peers always run with `--json` |
| `--verbose` | The entry member |
| `--no-env-file` | Every member. The launcher itself never loads `.env` |
| `--containment` | Every member |

`--roster` cannot be combined with `--manifest`, `--capsule`, `--capsule-version`,
`--spawn-grant-stdin`, `--system-prompt`, `--context`, `--resume`, `--resume-mode`,
`--forget-session`, `--lifecycle-task-acceptance`, `--lifecycle-after-task`, `--workdir`, `--bind`
or `--explain-scope`.

Standard output under `--json` carries exactly two lines:

1. The **formation line**, printed once every peer's door answers and before the entry member
   starts.
2. The entry member's own [readiness line](#mur-run-formation), unchanged, from the entry member's
   output.

```json
{"entry":"lead","formation_id":"frm_01a106501a1d78328710ed56d19a10a6","peers":[{"name":"worker","pid":1979177,"session_id":"ses_01a1065027fe7a5295b53597e7671a53","url":"http://localhost:36061","workdir":"/home/me/.murmur/formations/frm_01a106501a1d78328710ed56d19a10a6/worker"}]}
```

| Formation line key | Value |
|---|---|
| `entry` | The entry member's roster name |
| `formation_id` | The id every member was launched with, `frm_` followed by 32 lowercase hex digits |
| `peers` | One object per non-entry member, in roster order: `name`, `pid`, `session_id`, `url` as `http://host:port`, and `workdir`, the peer's own [directory](roster.md#member-directories) `~/.murmur/formations/<frm_id>/<member>` |

The formation line carries no door token. Without `--json`, the launcher writes the same
information to stderr as a `formation:` block, and the entry member writes its usual startup lines
to stdout.

Every line a peer writes to stderr, and every stdout line after its readiness line, goes to the
launcher's stderr behind a `[<member>] ` prefix:

```text
[coder] [capsule-runtime] formation lifeline closed — the formation has ended; cancelling live tasks and ending the session
```

The entry member writes to the launcher's own stdout and stderr, unprefixed, and stays in the
launcher's process group, so `^C` at a terminal reaches it as it reaches a hand-run `mur run`.

| The launcher | Exits with |
|---|---|
| The entry member's process ended | The entry member's exit code, or 128 plus the signal that ended it |
| A refusal before the entry member started — admission ([`E-ROS-*`](diagnostics.md#e-ros-001), `E-REG-005`), [`E-RUN-045`](diagnostics.md#e-run-045), [`E-RUN-046`](diagnostics.md#e-run-046) | 1 |
| The launcher received `SIGINT`, `SIGTERM` or `SIGHUP` | 130, 143 or 129 |

By the time the launcher exits, every member it started has been stopped and reaped. A launcher
killed with `SIGKILL`, or by the OOM killer, stops none itself; each member then winds down on its
own lifeline. See [How a formation ends](roster.md#launch-stop).

---

## `mur ps`

List the capsules running on this machine, with the address each one answers on.

```bash
mur ps
```

`mur ps` takes no arguments. It is host-scoped, exactly like `docker ps`: a capsule deployed onto
another machine writes its [record](#running-capsule-records) on *that* machine, so it is that
machine's `mur ps` that lists it.

| Column | Width | Carries |
|---|---|---|
| `SESSION` | 36 | The full session id, never abbreviated, so it can be copied into the next command |
| `CAPSULE` | 24 | `name@version` from the manifest the session was launched from |
| `STATUS` | 12 | `running` or `unreachable` |
| `DETACHED` | 8 | `yes` when the capsule outlives the window that launched it, `no` when it dies with it |
| `UPTIME` | 9 | `HH:MM:SS` since the session started, prefixed `Nd ` past a day |
| `FORMATION` | 36 | The full formation id of a [member](#mur-run-formation), `-` for a session in no formation. Present only when [a formation is listed](#mur-ps-formations) |
| `URL` | — | The `host:port` the capsule's A2A door is bound to |

```text
SESSION                               CAPSULE                   STATUS        DETACHED  UPTIME     URL
ses_019f01a940ce7761854e768ecbe3d399  my-worker@0.1.0           running       yes       00:14:02   localhost:41235
ses_019f0193c7d871a5b2e30ff41a7c0ce2  my-agent@0.2.0            unreachable   no        01:03:55   localhost:41102
```

A machine running nothing prints one line and exits 0:

```text
no running capsules
```

An absent `~/.murmur/running/` and an empty one are the same fact about the machine, and read the
same way. A `~/.murmur/running/` that cannot be read — a file where the directory belongs, or a
directory this user may not list — is neither: `mur ps` prints nothing on stdout and fails with
[`E-RUN-028`](diagnostics.md#e-run-028).

Every record `mur ps` removes because its process is gone is named on stderr, one line each. A
file in the directory that is not a readable record is removed without a line.

```text
pruned: ses_019f0193c7d871a5b2e30ff41a7c0ce2 — no process holds pid 48213
```

A pruned record of a formation member names its formation at the end of the line:

```text
pruned: ses_019f0193c7d871a5b2e30ff41a7c0ce2 — no process holds pid 48213 (formation frm_019f01a93ff27c1e9a3b5d0c4e8f2a61)
```

Exit codes:

- `0` — the records were read, whether or not any row was printed
- `1` — `~/.murmur/running/` could not be read ([`E-RUN-028`](diagnostics.md#e-run-028))

### What each row was verified against { #mur-ps-verification }

Every record is put through all three layers described under
[A record is a hint](#running-record-is-a-hint) before its row is printed, and what the layers say
decides both the `STATUS` column and whether the record survives the read.

| Layers | `STATUS` | The record | Reason on the `pruned:` line |
|---|---|---|---|
| All three pass | `running` | Kept | — |
| The process is the one that wrote the record; the door did not answer | `unreachable` | Kept | — |
| A process holds the process id and its start time could not be read | `unreachable` | Kept | — |
| No process holds the process id | No row | Unlinked | `no process holds pid N` |
| The process holding the process id started at another time | No row | Unlinked | `pid N is held by a process that started at another time` |

Neither a quiet door nor a start time that could not be read is evidence that the process is gone,
and unlinking the record would throw away the only handle anyone has on something still running. A
capsule's door is served apart from its turns, so a capsule busy with a turn still reads `running`.
A capsule whose process is alive reads `unreachable` when:

- the process is suspended, for example by `SIGSTOP` or a debugger
- the host is too loaded to schedule the capsule within the probe's deadline
- the capsule's runtime has every thread it serves requests on occupied

Rows are sorted by session id descending — the same order [`@N` counts in](#session-addresses) —
so the first row is what `@1` names. When a formation is listed, its members are kept together, so
past the first row count `@N` by session id, not by row.

### Formations { #mur-ps-formations }

When any record `mur ps` lists or prunes carries a formation id, the listing changes in three ways.
When none does, the output is the plain listing above.

1. The `FORMATION` column appears between `UPTIME` and `URL`.
2. Rows are grouped. A formation's listed members form one group, and a session in no formation
   is a group of one. Groups are ordered by their newest session, newest first, and rows within a
   group by session id descending. The first row is still the newest session.
3. After the table — or after `no running capsules` — come a blank line and one summary line per
   formation: listed formations in row order, then formations known only from records pruned in
   this read.

```text
SESSION                               CAPSULE                   STATUS        DETACHED  UPTIME     FORMATION                             URL
ses_019f01a9b1d27c3e8f0a4b5c6d7e8f90  researcher@0.1.0          running       yes       00:02:11   frm_019f01a93ff27c1e9a3b5d0c4e8f2a61  localhost:41873
ses_019f01a940ce7761854e768ecbe3d399  writer@0.1.0              unreachable   yes       00:02:14   frm_019f01a93ff27c1e9a3b5d0c4e8f2a61  localhost:41235
ses_019f01a95a0b7e21a3c4d5e6f7a8b9c0  my-agent@0.2.0            running       no        00:02:13   -                                     localhost:41102

formation frm_019f01a93ff27c1e9a3b5d0c4e8f2a61: 2 listed (1 running, 1 unreachable), 1 pruned now; not listed: 1 ended
```

A summary line counts what it found and never reports a formation as complete. It reads:

```text
formation <id>: <L> listed (<statuses>)[, <P> pruned now][; <not-listed>][; <U> session root(s) could not be read]
```

| Part | Says |
|---|---|
| `<L> listed (<statuses>)` | How many of the formation's members have a row, as `<n> running` and `<n> unreachable`, each only when non-zero. With no row, `0 listed` and no parentheses |
| `, <P> pruned now` | Member records this read removed. Only when non-zero |
| `; not listed: <n> ended, <n> with no record` | Members found in the [session roots](#mur-ps-session-roots) that had no record in this read. `ended` traces hold a `session_end`; `with no record` traces do not — a member killed outright, or one whose record was never written. Each count appears only when non-zero |
| `; no other member found in <k> session root(s)` | Replaces `not listed` when the session roots hold no member beyond the ones with a record |
| `; <U> session root(s) could not be read` | Session roots that exist and could not be listed. Only when non-zero |

<span id="mur-ps-session-roots"></span>The session roots searched are:

- each directory holding a listed or pruned member's session directory;
- each peer's session root under `~/.murmur/formations/<formation-id>/`, whether or not a record
  of that peer remains — see [Member directories](roster.md#member-directories).

A member that ran under another root and has no record is not counted.
[`mur trace show <formation-id>`](#mur-trace-show-formation) lists the members by name.

---

## `mur stop`

End one running capsule, and report what it left behind.

```bash
mur stop <SESSION> [--timeout <SECONDS>]
```

| Argument | Default | Description |
|---|---|---|
| `SESSION` | — | A [session address](#session-addresses) naming a running capsule |
| `--timeout` | `10` | Seconds to wait after `SIGTERM` before escalating to `SIGKILL`. `0` escalates immediately, with no grace period |

Three steps, in this order:

| Step | What it does |
|---|---|
| 1. [`session/stop`](../how-to/capsules-a2a-messaging.md#ending-the-session) through the A2A door | Cancels every task the session still holds and reads what it leaves running, then waits up to 5 seconds for the trace to record how each cancelled task ended |
| 2. `SIGTERM` to the recorded process | Ends the capsule. An agent capsule records its remaining endings and runs its teardown first — see [`SIGTERM`](#mur-run-sigterm) |
| 3. `SIGKILL` after `--timeout` seconds | Only if the process is still there |

A cancelled task's ending is its `task_end` record, or, for a task cancelled before it started,
its `task_canceled` record with `phase: "queued"`.

The door step is the only moment anything can ask the capsule what it leaves running. A detached
shell command keeps its own lifecycle and a delegated sub-capsule is still going, and the record of
both dies with the process, so the question is asked while the capsule is still answering.

```text
stopped: ses_019f01a940ce7761854e768ecbe3d399
capsule: my-worker@0.1.0
signal:  SIGTERM
canceled: tsk_019ed5211c827f63a8fe4be623277c55
running: wrk_9f2a1c  detached shell  sleep 30
running: dlg_7b31de  delegation  worker@0.1.0
```

The `running:` lines are the ones [`mur cancel`](#mur-cancel) prints for the same items. Nothing on
them was stopped.

### The three answers about what it left behind { #mur-stop-residue }

| Line | Means |
|---|---|
| `running: …`, one per item | The capsule answered and named these |
| `residue: nothing else was left running` | The capsule answered and had nothing to name |
| `residue: unknown — the capsule could not be asked: …` | The door did not answer, and the reason says why |

A capsule that answered with nothing and a capsule that could not be asked are different facts
about the machine, so they are different lines. Both exit 0: the session was ended either way, and
only the accounting is incomplete.

One more line appears only when an ending could not be recorded — the capsule was still busy when
the wait and `--timeout` ran out, and `SIGKILL` ended it. It prints after the `canceled:` lines,
one per task:

| Line | Means |
|---|---|
| `unended: <task_id>  the trace does not record how this task ended` | The task was cancelled, but `trace.jsonl` has no ending for it |

A stop whose capsule recorded every ending prints no `unended:` line.

### There is no `--url` { #mur-stop-no-url }

Two of the three steps signal a local process id, so a URL-addressed stop could only ever perform
the first one. `mur stop --url` is refused by the argument parser rather than silently doing a
third of the job. A capsule on another machine records on *that* machine, so it is that machine's
`mur stop` that ends it.

Exit codes:

- `0` — the session was ended, whether or not its door answered
- `1` — the address named no running session ([`E-RUN-022`](diagnostics.md#e-run-022)), the
  session could not be ended and is still running ([`E-RUN-024`](diagnostics.md#e-run-024)), or
  the running-capsule records could not be read ([`E-RUN-028`](diagnostics.md#e-run-028))

`mur stop` refuses to signal a process id it cannot confirm, and no signal of any kind is sent.

| The recorded process | Refusal | The record |
|---|---|---|
| Started at another time than the host reports — the number was inherited | `E-RUN-022` | Unlinked |
| Start time could not be read | `E-RUN-024` | Kept |

---

## `mur watch`

Stream live SSE events from a running capsule's output to stdout. The command opens a
`stream/watch` connection to the capsule and prints each event in a human-readable
format until the capsule closes or Ctrl+C is pressed.

```bash
mur watch [SESSION]
mur watch --url <host:port>
```

| Argument | Default | Description |
|---|---|---|
| `SESSION` | `@1` | A [session address](#session-addresses) naming a running capsule |
| `--url` | — | A capsule's address, reached without resolving anything. Conflicts with `SESSION` |

On a capsule declaring [`network.authentication`](manifest.md#field-network-authentication), a
session address presents the operator token from the [running-capsule record](#running-capsule-records),
and `--url` presents `MURMUR_DOOR_TOKEN` when it is set. A door that answers `401` or `403` fails
the command with `E-IO-003`, naming the status and `MURMUR_DOOR_TOKEN`.

The session is resolved against the [running-capsule record](#running-capsule-records) and verified
before the connection is opened.

**Ctrl-C ends the watch, not the capsule.** The capsule keeps running and keeps answering; only
this connection to it closes. On connect, `mur watch` prints one line to stderr naming the session
it is watching and saying so, which keeps stdout to SSE events alone for piping. To end the capsule
itself, use [`mur stop`](#mur-stop).

Output format:

```text
[working]  inference turn 1
[artifact] tool: bash [ok, exit 0, 12ms] | <untrusted-content source=tool:bash>
  $ echo hello
  Exit code: 0
  Stdout:
  hello

  Stderr:

  </untrusted-content>
[working]  inference turn 2
[completed]
```

The bracket after a tool name is the call's outcome, read from the
[`artifact` frame](streaming-protocol.md#event-artifact):

| Segment | Shown when |
|---|---|
| `ok` or `error` | Always — `error` when `is_error` is true |
| `exit <n>` | The call ran a subprocess to completion |
| `<n>ms` | The frame reports a duration |
| `truncated` | The tool marked its result truncated |

A capsule whose runtime reports no outcome prints no bracket.

### Heartbeat

`mur watch` reads the capsule's [heartbeat](streaming-protocol.md#heartbeat) and discards it; no
heartbeat appears in its output, and `mur watch` keeps waiting on a quiet connection for as long as
it stays open. A pause in the output means the capsule wrote no frame: a turn waiting on an
inference call, a tool call or a shell command writes none until the call returns. What a pause in
the heartbeat itself means is under [Heartbeat](streaming-protocol.md#heartbeat) on the protocol
page.

`mur watch` does not reconnect. When the connection ends without a
[`capsule-closed`](streaming-protocol.md#event-capsule-closed) frame, it names the last
[event id](streaming-protocol.md#event-ids) it received and exits. A capsule that exits, including
one ended with [`mur stop`](#mur-stop), closes the connection without that frame:

```text
error[E-IO-003]: connection to ses_019ed2af53da75c2aefee84ee10c34af lost after event id 4 — the capsule may still be running; run mur watch again to reattach
```

Exit codes:

- `0` — the capsule closed the stream with a `capsule-closed` frame
- `1` — the session address named nothing running (`E-RUN-022`), the capsule did not answer
  (`E-RUN-023`), the connection failed, or the connection was lost before the capsule closed the
  stream (`E-IO-003`)

---

## `mur cancel`

Stop one running task on a capsule. The capsule's session, its conversation and its queue are
untouched: queued tasks proceed, and the capsule keeps answering.

```bash
mur cancel <SESSION> <TASK_ID>
mur cancel --url <host:port> <TASK_ID>
```

| Argument | Default | Description |
|---|---|---|
| `SESSION` | — | A [session address](#session-addresses) naming a running capsule |
| `TASK_ID` | — | The `tsk_` id `message/send` returned, or the one `tasks/get` reports |
| `--url` | — | A capsule's address, reached without resolving anything. Takes the place of `SESSION` |

| Environment variable | Read by | Holds |
|---|---|---|
| `MURMUR_DOOR_TOKEN` | `mur cancel --url`, `mur watch --url` | A door token `mur run` printed, presented as `Authorization: Bearer` to a capsule declaring [`network.authentication`](manifest.md#field-network-authentication). A session address reads the operator token from the [running-capsule record](#running-capsule-records) instead |

A door that answers `401` or `403` fails the command with `E-IO-003`, naming the status and
`MURMUR_DOOR_TOKEN`.

The in-flight inference call is dropped rather than waited out, and the task reaches the terminal
state `canceled`. Nothing else is stopped: a detached shell command keeps its own lifecycle and a
delegated sub-capsule keeps running. Both are named in the output instead, one line each.

```text
task:    tsk_0199c4e2f1b7712a9d3e4f5061728394
state:   canceled
running: wrk_9f2a1c  detached shell  sleep 30
running: dlg_7b31de  delegation  worker@0.1.0
```

Cancelling a task that has already reached `completed`, `failed`, `rejected` or `canceled` reports
that state and changes nothing.

Exit codes:

- `0` — the capsule holds this task; the line printed says what state it is in
- `1` — the capsule does not hold this task id, the session address named nothing running
  (`E-RUN-022`), or the capsule did not answer (`E-RUN-023`)

---

## `mur control`

Show or change what a running capsule's [`control:`](manifest.md#field-control) block lets a
controller change, over its [control surface](control-surface.md).

```bash
mur control show [SESSION] [--json]
mur control set <SETTING> <VALUE> [SESSION]
mur control secret <NAME> [SESSION]
mur control forget <NAME> [SESSION]
```

| Subcommand | Does |
|---|---|
| `show` | Lists the declared settings with their current values, and the declared secrets with whether each is set |
| `set` | Changes a declared setting from the capsule's next inference call |
| `secret` | Supplies a declared secret's value, read from standard input |
| `forget` | Drops a declared secret's value, so its gateway's keyed requests are refused again |

| Argument | Default | Description |
|---|---|---|
| `SESSION` | `@1` | A [session address](#session-addresses) naming a running capsule |
| `SETTING` | — | A setting `control.settings` declares, e.g. `inference.max_tokens` |
| `VALUE` | — | The new value. Sent as JSON when it parses as JSON, as a string otherwise; the capsule decides what the setting takes |
| `NAME` | — | A secret `control.secrets` declares |
| `--json` | off | `show` only: print the control surface's JSON as it answered |

Each subcommand resolves `SESSION` through its running record, reads the control token beside it,
and makes one request to the capsule. There is no `--url`: the token is found only through the
record.

`mur control secret` reads the value from standard input and strips exactly one trailing `\n` or
`\r\n`. On a terminal it prompts `value for NAME: ` on stderr and turns echo off for the read,
restoring it on return, `SIGINT`, `SIGTERM` or `SIGHUP`. The value is sent only when the capsule is
reached over loopback; the control surface refuses it otherwise.

Output:

```text
$ mur control show
session:  ses_0199c4e2f1b7712a9d3e4f5061728394
settings:
  inference.max_tokens   4096
secrets:
  CARD_TOKEN             not set

$ mur control set inference.max_tokens 2048
setting:  inference.max_tokens
previous: 4096
value:    2048
applies:  next inference call

$ printf '%s\n' "$CARD_TOKEN_VALUE" | mur control secret CARD_TOKEN
secret: CARD_TOKEN
set:    yes (new)

$ mur control forget CARD_TOKEN
secret: CARD_TOKEN
set:    no
```

`set:` reads `yes (replaced)` when a value was already held. Neither the token nor a secret value
appears in any output or error.

Exit codes:

- `0` — the control surface accepted the request
- `1` — the session has no control surface or the surface refused the request (`E-RUN-042`, which
  states the HTTP status and the surface's reason), the session address named nothing running
  (`E-RUN-022`), or the capsule did not answer (`E-RUN-023`)

---

## `mur deploy run`

Upload the `mur` binary and capsule files to an existing VM via SSH, start the capsule, and print the public A2A endpoint. The VM must already exist and be reachable via SSH — `mur deploy run` never provisions or terminates VMs on your behalf.

```bash
mur deploy run --host <ip> [--ssh-user <user>] [--ssh-key <path>]
               [--manifest <path>] [--workdir <path>] [--mur-binary <path>]
               [--env KEY=VALUE] [--env-file <path>] [--deploy-platform <platform>]
               [--no-precompile]
```

| Flag | Default | Description |
|---|---|---|
| `--host` | — | IP address or hostname of the target VM (required) |
| `--ssh-user` | `root` | SSH username on the VM |
| `--ssh-key` | — | Path to SSH private key; uses SSH agent if omitted |
| `--manifest` | `./murmur.yaml` | Path to the capsule manifest to deploy |
| `--workdir` | — | Local directory to upload as the capsule's working directory |
| `--mur-binary` | — | Path to a `mur` binary for `--deploy-platform` to upload. When omitted, the release named by `mur_version` in the manifest (or the running `mur` version) is downloaded from GitHub and cached at `~/.murmur/bin/mur-{version}-{platform}` |
| `--env` | — | Environment variable in `KEY=VALUE` format; repeat for multiple vars |
| `--env-file` | — | Path to a `.env` file of `KEY=VALUE` lines, `#` comments ignored. Takes precedence over the `.env` beside the manifest, which is loaded when neither `--env` nor `--env-file` is given |
| `--deploy-platform` | `linux-x86_64` | Platform the uploaded artifacts and `mur` binary are resolved for |
| `--no-precompile` | off | Skip compiling the uploaded WASM artifacts on the target; they compile on the capsule's first launch instead |

**Output — a summary box on stderr.** `mur deploy run` emits no JSON; progress and the final box
both go to stderr. Standard output carries the capsule's door tokens alone, when it has any.

```
  ┌────────────────────────────────┐
  │  ∞  my-agent                   │
  │                                │
  │  url   http://1.2.3.4:9000     │
  │  dep   dep_01954a3b            │
  │  time  42s                     │
  └────────────────────────────────┘
```

| Row | Description |
|---|---|
| `url` | Public A2A endpoint — `http://<VM_PUBLIC_IP>:<PORT>`. Use for `message/send`, `tasks/get`, and `/.well-known/agent-card.json`. |
| `dep` | The deployment ID, abbreviated to its `dep_` prefix and first 8 hex characters. The full `dep_` + UUID v7 is stored in `~/.murmur/deployments.json` and listed by [`mur deploy ls`](#mur-deploy-ls); `mur destroy` accepts any unambiguous prefix. |
| `time` | Elapsed wall-clock seconds |

To script against a deployment, read `~/.murmur/deployments.json` or parse `mur deploy ls` — the box is
for humans and its layout is not a stable interface.

**Deployment flow:**

1. Validate `--manifest`, `--workdir`, `--mur-binary` and every `--env` entry (no network calls)
2. Resolve every declared artifact for `--deploy-platform`, and the `mur` binary
3. Wait up to 30s for SSH to become available on the VM
4. Upload the `mur` binary via `scp` to `/usr/local/bin/mur`
5. Upload the manifest, the files it references and the optional workdir to `/root/mur-<id>/`
6. Upload every artifact into the VM's global store, `/root/.murmur/artifacts/<name>/<version>/`
7. Write the environment variables to `/root/mur-<id>/.env`, mode `600`, when there are any
8. Run [`mur precompile --json --workdir /root/mur-<id>`](#mur-precompile) on the VM over every uploaded artifact, with the `.env` exported; skipped under `--no-precompile` or when the capsule declares no artifacts
9. Run `mur run --manifest <path> --workdir /root/mur-<id> --json` on the VM with the same `.env` exported; wait up to 120s for the JSON line
10. Parse `localhost:PORT` from the JSON output; open the port and construct the public URL
11. Say who may call the published door:

    | Manifest | Output |
    |---|---|
    | No [`network.authentication`](manifest.md#field-network-authentication) | [`W-SEC-032`](diagnostics.md#w-sec-032) on stderr, naming the public URL |
    | `network.authentication` | One `token <name> <token>` line per token on stdout, `operator` first, then the declared credentials by name |

12. Persist to `~/.murmur/deployments.json`; print the summary box

Artifacts are pre-staged in step 6, so the remote `mur run` finds them installed and starts without fetching anything. Step 8 compiles them with the VM's own `mur`, so the capsule's first launch loads compiled forms instead of compiling; see [Precompile on the target](#deploy-precompile).

| Measured: release `mur`, a driver, a WASM tool and a native tool, 8 logical CPUs | Median of 5 deploys |
|---|---|
| Time step 8 adds | 0.52 s |
| First launch to first inference request, with step 8 | 0.20 s |
| First launch to first inference request, `--no-precompile` | 0.63 s |
| Later launches, either way | 0.20 s |

The compile moves out of the capsule's start rather than adding to it, so a deploy's total time barely changes.

The flow depends on `mur run --json` — see [`mur run`](#mur-run) for the `--json` output shape.

### Precompile on the target { #deploy-precompile }

Steps 8 and 9 start from the same `.env`, so the compile and the capsule share one `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES` value, the VM's: set it with `--env`, or it is the default. This machine's value does not reach the VM. When the two differ, deploy prints [`W-DEPLOY-002`](diagnostics.md#w-deploy-002); the capsule still starts warm.

Step 8 never fails a deploy:

| What happened on the VM | Deploy prints | The capsule |
|---|---|---|
| Every artifact compiled, was already stored, or is not WASM | nothing | Loads every compiled form on its first launch |
| Some artifacts `failed` | [`W-DEPLOY-001`](diagnostics.md#w-deploy-001), naming each | Compiles those on its first launch |
| No report: the VM's `mur` predates `mur precompile`, or the SSH command failed | [`W-DEPLOY-001`](diagnostics.md#w-deploy-001), quoting the first error line | Compiles every artifact on its first launch |

**Example:**

```bash
mur deploy run \
  --host 1.2.3.4 \
  --manifest ./my-agent/murmur.yaml \
  --mur-binary ./target/x86_64-unknown-linux-musl/release/mur \
  --env ANTHROPIC_API_KEY=sk-ant-...
# summary box on stderr: url http://1.2.3.4:9000 / dep dep_01954a3b / time 42s
```

**Error codes:**

| Code | Meaning |
|---|---|
| `E-IO-001` | `--manifest`, `--workdir`, or `--mur-binary` path not found |
| `E-DEPLOY-001` | No `--host` given, or an `--env` value is not `KEY=VALUE` |
| `E-DEPLOY-003` | SSH connection or remote command failed |
| `E-DEPLOY-004` | Capsule did not emit usable startup JSON within 120s |
| `E-DEPLOY-006` | The pinned `mur` release could not be fetched from GitHub |

**Warnings** (the deploy continues and exits `0`):

| Code | Meaning |
|---|---|
| [`W-DEPLOY-001`](diagnostics.md#w-deploy-001) | The VM compiled none, or not all, of the uploaded artifacts |
| [`W-DEPLOY-002`](diagnostics.md#w-deploy-002) | The VM's `MURMUR_MAX_ARTIFACT_DECOMPRESSED_BYTES` differs from this machine's |

---

## `mur deploy ls`

List all deployed capsules tracked in `~/.murmur/deployments.json`.

```bash
mur deploy ls
```

Output columns:

| Column | Description |
|---|---|
| `DEPLOYMENT_ID` | Id assigned at deploy time (`dep_` + UUID v7) |
| `PROVIDER` | Always `manual` — VMs are created by the user, not by `mur deploy run` |
| `REGION` | Empty for every record `mur deploy run` writes; the VM is one you created, and its region is never queried |
| `STATUS` | Always `running` for present entries (`mur destroy` removes the entry) |
| `URL` | Public A2A endpoint (`http://IP:PORT`) |

Prints `no deployments` when `~/.murmur/deployments.json` is absent or empty.

**Example:**

```text
DEPLOYMENT_ID                           PROVIDER      REGION        STATUS      URL
----------------------------------------------------------------------------------------------------
dep_01954a3b5c7d8e9f0a1b2c3d4e5f6a7b    manual                      running     http://1.2.3.4:9000
```

---

## `mur destroy`

Remove a deployment entry from `~/.murmur/deployments.json`. Does not stop or delete the VM — shut down the VM from your cloud provider's dashboard separately.

```bash
mur destroy <deployment_id>
```

- `deployment_id` — the id returned by `mur deploy run` (also listed by `mur deploy ls`); a unique prefix is enough
- Exits non-zero with a clear error if the id is not found in `~/.murmur/deployments.json`

**Example:**

```bash
mur destroy dep_01954a3b
# destroyed dep_01954a3b5c7d8e9f0a1b2c3d4e5f6a7b (1.2.3.4)
```

---

## `deployments.json`

Location: `~/.murmur/deployments.json`

A JSON array that tracks all active deployments. Written on `mur deploy run`; entries removed on `mur destroy`. Schema per entry:

```json
{
  "deployment_id":  "dep_01954a3b...",
  "provider":       "manual",
  "provider_vm_id": "",
  "provider_key_id": "",
  "region":         "",
  "ip":             "1.2.3.4",
  "url":            "http://1.2.3.4:9000",
  "manifest_path":  "/Users/you/my-agent/murmur.yaml",
  "started_at":     "2026-06-03T12:00:00+00:00",
  "status":         "running"
}
```

| Field | Description |
|---|---|
| `deployment_id` | `dep_` + UUID v7 — the deployment's identity across all commands |
| `provider` | Always `"manual"` — VMs are created by the user outside of `mur` |
| `provider_vm_id` | Always empty — reserved for future provider integrations |
| `provider_key_id` | Always empty — reserved for future provider integrations |
| `region` | Always empty — reserved for future provider integrations |
| `ip` | Public IPv4 of the VM (the value passed to `--host`) |
| `url` | `http://IP:PORT` — the public A2A endpoint |
| `manifest_path` | Absolute local path to the manifest used at deploy time |
| `started_at` | RFC 3339 timestamp of when the deployment was created |
| `status` | Always `"running"` — entries are removed on destroy, not updated |

---

## `mur conversation`

Inspect and prune the [durable conversation records](workdir.md#the-conversation-record) under
`~/.murmur/conversations/`. These commands read and write that store directly; they do not stage
or launch a capsule, and they need no manifest.

A context id is unique inside one record store and nowhere else. When one appears under more than
one store, `rm` and `truncate` refuse with [`E-CNV-002`](diagnostics.md#e-cnv-002) rather than
guess, and `--record <NAME>` says which store to act on.

### `mur conversation ls`

```bash
mur conversation ls [--record <NAME>] [--message <MSG-ID>] [--json]
```

| Flag | Default | Description |
|---|---|---|
| `--record` | every store | Limit to one directory under `~/.murmur/conversations/` |
| `--message` | — | Report where one `msg_` id stands instead of listing records |
| `--json` | off | Print the same values as JSON |

Without `--message`, one row per context:

```
RECORD                   CONTEXT                      MESSAGES       SIZE  LAST TOUCHED         TRUNCATED
shey                     ctx_0199f2a1                       48   12.4 KiB  2026-08-29 09:14:02  500 dropped
```

| Column | Contents |
|---|---|
| `RECORD` | The record store: the directory under `~/.murmur/conversations/` |
| `CONTEXT` | The context id: the directory under the record store |
| `MESSAGES` | Message lines. The [header line](workdir.md#record-header) is not a message and is not counted |
| `SIZE` | Bytes of `conversation.jsonl` |
| `LAST TOUCHED` | Last write to `conversation.jsonl`, in UTC |
| `TRUNCATED` | Messages this record has dropped over its life, or `-` |

`--json` prints an array whose objects carry `record`, `context_id`, `path`, `messages`, `bytes`,
`last_touched_ms`, `capsule` (`null` for a record no capsule owns) and `truncated` (`null`, or an
object with `dropped`, `oldest_surviving_id`, `last_dropped_id` and `at_ms`).

#### `mur conversation ls --message` { #mur-conversation-ls-message }

Answers one of three things about a `msg_` id, which is what an artifact that stored a
`source_id` and now finds nothing needs to know:

| Answer | When | Reported |
|---|---|---|
| `present` | The id is a line in a record | The record, the context, and its position |
| `truncated` | The id is not a line, the record's header carries a truncation marker, and the id's own uuid-v7 timestamp is at or before the `last_dropped_id`'s | The record, the context, how many were dropped, and the oldest surviving id |
| `unknown` | Anything else | Nothing further |

### `mur conversation rm` { #mur-conversation-rm }

```bash
mur conversation rm <CONTEXT-ID> [--record <NAME>]
```

Removes that context directory whole and reports the path and the message count it held. This is
how to reclaim a record whose capsule no longer runs: the
[age sweep](manifest.md#retention-never) skips a record whose header line names no capsule.

### `mur conversation truncate` { #mur-conversation-truncate }

```bash
mur conversation truncate <CONTEXT-ID> --keep <N> [--record <NAME>]
```

Drops everything before the newest `N` messages and reports what went. `N` must be at least 1;
`--keep 0` is refused with [`E-CNV-003`](diagnostics.md#e-cnv-003), because truncating a record to
nothing is `mur conversation rm`.

The rewrite is atomic: the kept tail plus a [header line](workdir.md#record-header) recording the
drop is staged beside the record and renamed over it, so an interrupted truncation leaves the
original whole. Every surviving message keeps the exact `id` it carried.

---

## `mur trace`

Read and analyze `trace.jsonl` files produced by `mur run`. These commands are read-only — they do not modify any file and do not require a running registry or runtime.

See [Session trace (`trace.jsonl`)](observability-schemas.md#session-trace-tracejsonl) for the file format.

### `mur trace show`

Print a human-readable summary of a single session, the recorded body behind one of its
content hashes, or the members of a formation.

```bash
mur trace show [<session>] [--workdir <dir>] [--body <selector> --turn <n>]
mur trace show <formation-id> [--workdir <dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<session>` | `@1`, the most recent session in the workdir | A [session address](#session-addresses), or a formation id (`frm_…`), which prints only the [Formation section](#mur-trace-show-formation) |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories. With a formation id, the first root searched — see [Listing a formation](#mur-trace-show-formation) |
| `--body` | — | Print the body behind one hash and nothing else. Selectors below |
| `--turn` | — | The turn whose hashes `--body system`, `tools`, `response` and `message:<i>` name. Required with those four, invalid without `--body` |

Output sections, in the order they are printed:

| Section | Printed | Contents |
|---|---|---|
| Session | always | `session_id`, capsule name+version, model, exit status, duration, granted capability categories, declared tools, `containment: <declared> → <achieved>`, `workdir exec`, `userns`, and the system prompt's source and hash. For a capsule another capsule launched, a `Spawned by <session> (delegation <id>)` line follows `session`. For a [formation member](#mur-run-formation), a `formation:  <formation-id>` line follows those. For a session that staged a [runtime pin](workdir.md#lock-origin), a `runtime pins: <name>@<version> (pulled by <session>), …` line follows `tools` |
| Hook failures | one or more `hook_dispatch_error` records | One `✗ <hook> <lifecycle event> <arm>` row per fault |
| Retention | one or more [`retention`](observability-schemas.md#retention) records | One `<store>  <reason>  removed <n>` row per pair, followed by the names of what went |
| Context | one or more `context_seed` records | Per seeding hook: outcome, tokens committed, tokens proposed, the budget, the rejection reason, and the ids of the messages seeded |
| Turns | always | Turn count and configured max |
| Tokens | always | Input tokens, output tokens, total, per-turn averages, and a `provider:` line summing each reported count — `in`, `out`, `cached`, `cache write`, `thinking` — over the turns that reported it. A count no turn reported is left off the line. Under [`transport: process`](manifest.md#transport-process) the harness's input and output counts are the totals above, so only the cache and thinking counts appear here |
| Wire | one or more turns carrying content hashes, or one or more [`tools_refreshed`](observability-schemas.md#tools-refreshed) records | Per turn: the abbreviated `system`, `tools` and `response` hashes and how many messages the request carried. Then one `refreshed:  turn <n>  <trigger>  +<added>  -<removed>` row per turn whose tool list changed, and the `--body` command that prints one of the hashes |
| Tool calls | always | Count, ok/error breakdown, success rate, average latency, plus a per-turn breakdown of every call |
| Redundant calls | always | Calls that re-read a resource nothing had changed since. Agent turns and plan steps are scored against one shared history, so either can be named as the call or as the earlier read it duplicates |
| Skill calls | always | Count, ok/error breakdown, success rate, average latency, plus a per-turn breakdown of every call. The call of a skill whose `skill_call.trust` is `untrusted` ends in ` runtime/untrusted`: `turn 1  pulled-style 2ms ✓ runtime/untrusted` |
| Pulled at runtime | one or more [`artifact_pulled`](observability-schemas.md#session-trace-tracejsonl) records | One `<name>@<version>  <runtime>  <origin>/<trust>` row per pull, in file order |
| Shell calls | always | Count, exit code distribution, average latency |
| Compaction | always | Whether it fired, with turn number and before/after token counts, followed by one `declined:` row per turn that crossed the compaction threshold and was left uncompacted, naming its turn, the context occupancy and the reason |
| Cancelled | one or more [`task_canceled`](observability-schemas.md#task-canceled) records | One `task_canceled  at <phase>` row per cancel, naming what was still running |
| Rejected | one or more [`task_rejected`](observability-schemas.md#task-rejected) records | One `task_rejected  <task id>  <cause>  source <source>` row per refused task, in file order |
| Failed | one or more [`task_failed`](observability-schemas.md#task-failed) records | One `task_failed  <cause>  <task id>  at turn <n>` row per failed attempt, followed by its `reason:` |
| Reopens | one or more `task_reopened` records | Per reopen: its ordinal, the hook that asked, whether the next attempt `continued` the task's conversation or `restarted` it, the turns it had left, and the hook's feedback — `reopen 1  by gatekeeper  continued, 7 turns left  “…”` |
| Resource plane | one or more `resource_list`/`resource_read` records | Counts by outcome |
| Peer files | one or more `peer_handle_mint`/`peer_handle_redeem`/`peer_file_fetch` records | Counts by outcome |
| Delegations | one or more [`delegation_start`](observability-schemas.md#delegation-lineage)/`delegation` records | One row per delegation: its `dlg_` id, `capsule@version`, the child session, and the outcome — `in flight` for a delegation this trace never saw end. The reason follows on any outcome that recorded one, and the path to the child's own trace follows on any delegation that launched one |
| Plan | one or more [`plan_start`](observability-schemas.md#plan-events)/`plan_step`/`plan_end` records | Per plan run: its id, outcome, duration and step totals by status, the step that ended it, and one row per step in the order the plan declared them — kind, status, duration, attempt count when it retried more than once, what it waited on, and its error. A step the run never reached reads `not run` |
| A2A | one or more `a2a_task_received`/`a2a_send` records | Tasks received, messages sent, and the peer URLs they went to |
| Tasks | more than one task in the session | Per-task breakdown |
| Member calls | one or more [`member_call_start`](observability-schemas.md#member-call-start)/[`member_call`](observability-schemas.md#member-call) records | One row per [`call-member`](runtime-provided-tools.md#call-member) call: its `mcl_` id, the member, the member's task id — `(not started)` for a call the member never held — and how it ended with its duration, `outstanding` for a call this trace never saw end, `not delivered` for an answer the task never received |
| Formation | the session is a [formation member](#mur-run-formation) | The [Formation section](#mur-trace-show-formation) for the session root this session is in and the formation's peer directories |

#### Listing a formation { #mur-trace-show-formation }

`mur trace show <formation-id>` lists every session whose trace names that formation, by session
id, with how each ended. It searches these session roots, in order:

| `--workdir` | Roots searched |
|---|---|
| Not given | `./workdir`, then `./.murmur`, then each peer's session root under `~/.murmur/formations/<formation-id>/` |
| Given | That directory, then each peer's session root under `~/.murmur/formations/<formation-id>/` |

The entry member records under the roster's directory, so run the command from that directory, or
pass `--workdir <roster directory>/.murmur`. Each peer's session root is
`~/.murmur/formations/<formation-id>/<member>/.murmur` — see
[Member directories](roster.md#member-directories). A member's own `mur trace show` prints the same
section, searching the session root it is in and then each peer's session root.

Run from the roster's directory `/home/me/project`:

```text
── Formation ────────────────────────────────────
formation:  frm_019f01a93ff27c1e9a3b5d0c4e8f2a61
searched:   /home/me/project/workdir
searched:   /home/me/project/.murmur
searched:   /home/me/.murmur/formations/frm_019f01a93ff27c1e9a3b5d0c4e8f2a61/writer/.murmur
ses_019f01a940ce7761854e768ecbe3d399  researcher@0.1.0          ok
ses_019f01a95a0b7e21a3c4d5e6f7a8b9c0  writer@0.1.0              no session_end
ses_019f01a9b1d27c3e8f0a4b5c6d7e8f90  checker@0.1.0             ok  spawned by ses_019f01a940ce7761854e768ecbe3d399
delegated children not under these roots: 1 — `mur trace show <member>` names each child trace
```

| Row | Carries |
|---|---|
| `formation:` | The formation id |
| `searched:` | One line per session root searched, in the order searched, whether or not it exists. A root after the first that could not be listed ends `could not be read: <why>`, and fails nothing |
| One per member | Session id, `name@version`, and the `session_end` exit status — `no session_end` for a member that was killed or is still running. `spawned by <session>` follows for a member another member delegated to |
| `delegated children not under these roots` | Members' delegated children whose traces are in another session root. Only when there are any. The member's own `mur trace show` prints each `child trace:` path in its Delegations section |

A member that ran from another project, or with another `--workdir`, is listed by running the
command against that root.

| Situation | Exit |
|---|---|
| `<formation-id>` is not `frm_` followed by 32 lowercase hex digits | [`E-TRC-002`](diagnostics.md) `'<arg>' is not a formation id` |
| No session under any searched root names the formation | [`E-TRC-002`](diagnostics.md) `no session under <root>, <root>… belongs to formation <formation-id>` |
| `--body` or `--turn` with a formation id | [`E-TRC-001`](diagnostics.md) |

A member killed before it wrote `session_end` cannot be summarized on its own. Its
`mur trace show` fails with [`E-TRC-001`](diagnostics.md), and the message names its
formation and the command that lists it:

```text
error[E-TRC-001]: /home/me/.murmur/formations/frm_019f01a93ff27c1e9a3b5d0c4e8f2a61/writer/.murmur/ses_019f01a95a0b7e21a3c4d5e6f7a8b9c0/trace.jsonl: no session_end event found; this session is a member of formation frm_019f01a93ff27c1e9a3b5d0c4e8f2a61 — `mur trace show frm_019f01a93ff27c1e9a3b5d0c4e8f2a61` lists the formation
```

#### Printing one recorded body { #mur-trace-show-body }

`--body` prints the bytes behind one hash to stdout — no headers, no added trailing newline — so
the output pipes into `sha256sum` and matches the blob's own name. The bodies live in
[`<session>/blobs/`](observability-schemas.md#trace-blobs) and are stored only under
[`trace.capture: content`](manifest.md#field-trace).

| Selector | Resolves to |
|---|---|
| `system` | the named turn's `system_sha` |
| `tools` | the named turn's `tools_sha` |
| `response` | the named turn's `response_sha` |
| `message:<i>` | entry `i` (0-based) of the named turn's `message_shas` |
| `<sha256>` | that hash — a full 64-character lowercase hex string, or a prefix of 8 or more characters naming exactly one hash anywhere in the trace, `session_start.system_prompt_sha256` included. Needs no `--turn` |

```bash
mur trace show --body system --turn 1 | sha256sum
```

Every `--body` failure exits non-zero with [`E-TRC-001`](diagnostics.md):

| Situation | Message |
|---|---|
| A named selector with no `--turn` | `--turn is required with --body <selector>; this trace has turns 1, 2, 3` |
| `--turn` names no `inference` record | `turn 7 has no inference record in this trace` |
| The turn recorded no hashes | `turn 3 recorded no content hashes — the session ran under trace.capture: none` |
| `message:<i>` past the end of the list | `turn 2 recorded 4 messages; there is no message 7` |
| The hash is recorded and the body is not | `turn 1 system prompt <sha>: recorded under capture: meta; no body was stored` |
| A hash nothing in the trace names | `no hash in this trace matches <arg>` |
| A prefix matching several hashes | The refusal lists every hash it matched |
| `--turn` without `--body` | `--turn has no meaning without --body` |

The **Tasks** section appears only for sessions that ran more than one task.

Below the **Tool calls** summary line, each turn that made at least one tool call gets its own row: tool name, duration, a `✓`/`✗` status icon, and — when the call carried an `input` — its compact-JSON input, truncated to 120 characters with a trailing `…` if longer. A call with no recorded input shows no input segment at all.

Example (single-task session — no Tasks section):

```text
── Session ──────────────────────────────────────
session:    ses_aaaaaaaaaaaa4aaa8aaa000000000001
capsule:    my-agent v0.1.0
model:      claude-3-5-sonnet
status:     ok
duration:   500ms
capabilities: shell
tools:      bash
containment: sealed → scoped
workdir exec: no
userns:     profile_confining
prompt:     manifest  cf07194ee232…

── Turns ────────────────────────────────────────
count:      2  (max: 10)

── Tokens ───────────────────────────────────────
input:      2,200  (avg 1100/turn)
output:     350  (avg 175/turn)
total:      2,550
provider:   in 2,090, out 320, cached 1,840, cache write 210

── Wire ─────────────────────────────────────────
turn 1  system bbc5e661e106…  tools f9d35d43770d…  response afb8c1747105…  3 messages
turn 2  system bbc5e661e106…  tools f9d35d43770d…  response 4ed87cafe960…  5 messages
bodies:     mur trace show --body system --turn 1

── Tool calls ───────────────────────────────────
count:      1  (1 ok, 0 error)  success 100.0%
latency:    avg 100ms
  turn 1  bash 100ms ✓  {"command":"cargo test --workspace"}
  turn 2  end_turn

── Redundant calls ──────────────────────────────
count:      0

── Skill calls ──────────────────────────────────
count:      0

── Shell calls ──────────────────────────────────
count:      1
exit codes: 1 ok
latency:    avg 50ms

── Compaction ───────────────────────────────────
fired:      no
```

Example (multi-task session — Tasks section added):

```text
── Session ──────────────────────────────────────
...

── Compaction ───────────────────────────────────
fired:      no
── Tasks ───────────────────────────────────────
task 1  08ecee82  turns: 1  in: 39  out: 20  ok  178ms
task 2  d014bbd7  turns: 1  in: 39  out: 20  ok  2ms
```

Each task row shows the first 8 characters of the `task_id`, per-task turns, input tokens, output tokens, exit status, and duration.

A `── Denied calls ──` section is printed when a [policy hook](../concepts/hooks.md#policy-hooks)
refused a shell command or tool call, one line per refusal:

```text
── Denied calls ─────────────────────────────────
turn 0  on-shell  /usr/bin/bash  by branch-policy  “protected branch”
```

### `mur trace steps`

Print what the agent did, turn by turn.

```bash
mur trace steps [<session>] [--verbose] [--workdir <dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<session>` | `@1`, the most recent session in the workdir | A [session address](#session-addresses) |
| `--verbose` | off | Append a truncated summary of each tool call's input |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories |

A trace whose lines carry [`event_id`](observability-schemas.md#session-trace-tracejsonl) renders as
the session → task → turn tree that its `parent_id` chain describes: each turn's tool, shell and
skill calls under their turn, each turn under its task, and each task under the session. A
turn-level line whose `parent_id` names no line in the file is attributed by its `task_id`.

```text
Session ses_019f01a940ce7761854e768ecbe3d399  (1 task, 2 turns)

task tsk_11112222…  ctx_11112222…  (a2a)
  context_seed memory-hook  trimmed  1,204 tokens
  turn 1  tool_call  bash
    tool_call  bash  120ms  ✓
    shell      /usr/bin/bash  exit 0  50ms
  turn 2  end_turn
```

A call to a skill whose [`murmur.lock` pin](workdir.md#lock-origin) a running capsule pulled ends
in its origin and trust class, and a successful `manage.pull()` renders as an `artifact_pulled`
row:

```text
    skill_call pulled-style  2ms  ✓ (runtime/untrusted)
artifact_pulled pulled-style@0.1.0 (runtime/untrusted)
```

A call a policy hook refused renders as a `call_denied` row under its turn:

```text
  turn 0  tool_call  bash
    call_denied on-shell  /usr/bin/bash  denied by branch-policy
```

What sits beside that row depends on the transport:

| Transport | Rows for a refused call |
|---|---|
| [`transport: http`](manifest.md#transport-http) | The `call_denied` row alone. Nothing ran, so there is no `tool_call` or `shell` row |
| [`transport: process`](manifest.md#transport-process) | The `call_denied` row, and a `tool_call` row marked `✗`. The tool still did not run; the harness reports the refusal it was handed as its own failed call, and that report is recorded |

A call refused because its input lacked a field the tool's
[`input_schema`](manifest.md#input-schema) requires renders as a `tool_input_refused` row naming
every missing field, followed by a `tool_call` row marked `✗` on both transports:

```text
  turn 0  tool_call  murmur-tool-editor
    tool_input_refused murmur-tool-editor  missing operation
    tool_call  murmur-tool-editor  0ms  ✗
```

| Transport | Where the `tool_call` row comes from |
|---|---|
| [`transport: http`](manifest.md#transport-http) | The runtime records the refused call as a failed tool call |
| [`transport: process`](manifest.md#transport-process) | The harness reports the refusal it was handed as its own failed call |

No `call_denied` row sits beside it: the policy hook is not asked about a call missing a field.

A failed attempt renders as a `task_failed` row under its task, naming the
[cause](observability-schemas.md#task-failed) and the first 120 characters of the reason:

```text
task tsk_11112222…  ctx_11112222…  (task_md, user/trusted, lane user)
  task_failed driver_error  {"error":"driver: failed to parse Anthropic response JSON: EOF while parsing a string at line 1 column 38","stop_reason"
```

A task the session refused when it stopped taking work renders as a `task_rejected` row under the
session, naming the [cause](observability-schemas.md#task-rejected) and the task. It has no task
row of its own, because it never started:

```text
Session ses_019f01a940ce7761854e768ecbe3d399  (1 task, 1 turn)

task tsk_11112222…  ctx_11112222…  (task_md, user/trusted, lane user)
  turn 0  end_turn
task_rejected session_ended  tsk_33334444…
```

A [plan run](observability-schemas.md#plan-events) renders as its own subtree: a `plan_start` row
naming the plan and its step count, then a `plan_step_start` row as each step is handed to a
worker and a `plan_step` row when it settles, then `plan_end` with the outcome. A step that was
never dispatched has only the `plan_step` row.

```text
plan_start release  3 steps
  plan_step_start build  tool
  plan_step  build  tool  success  300ms
  plan_step_start ship  capsule  after build
  plan_step  ship  capsule  failed  900ms
  plan_end   failed  failed at ship
```

A trace whose lines carry no `event_id` renders one row per turn: turn number, decision, tool
name, duration.

```text
Session ses_aaaaaaaaaaaa4aaa8aaa000000000001  (2 turns)

  1  tool_call    bash        100ms
  2  end_turn     —           —
```

### `mur trace diff`

Compare two sessions side by side, with a delta and directional indicator per metric.

```bash
mur trace diff [<before> <after>] [--workdir <dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<before>` | `@2` | The run in the Run A column, as a [session address](#session-addresses) |
| `<after>` | `@1` | The run in the Run B column, as a [session address](#session-addresses) |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories |

Both addresses or neither: one argument is refused with
[`E-TRC-002`](diagnostics.md).

Example (A = 2 turns, ok; B = 5 turns, max_turns_reached):

```text
Metric                 Run A            Run B            Delta
────────────────────── ──────────────── ──────────────── ──────────────────────────
turns                  2                5                +3 (A better)
duration               500ms            1.7s             +1.2s (A better)
input tokens           2,200            9,700            +7500 (A better)
output tokens          350              1,090            +740 (A better)
input/turn (avg)       1100             1940             +840.0 (A better)
output/turn (avg)      175              218              +43.0 (A better)
tool calls             1                5                +4 (A better)
tool success rate      100.0%           80.0%            -20.0 (A better)
avg tool latency       100ms            188ms            +88ms (A better)
shell calls            1                5                +4 (A better)
avg shell latency      50ms             36ms             -14ms (B better)
compaction             none             turn 3           —
exit status            ok               max_turns_reached —
```

- Numeric metrics that are lower-is-better (turns, tokens, latency) flag the lower run as `(X better)`.
- `tool success rate` is higher-is-better.
- Non-numeric or non-comparable fields (`compaction`, `exit status`) show `—` in the Delta column.

#### Prefix divergence { #mur-trace-diff-divergence }

Below the table, a **Prefix divergence** section reports where the two runs' requests stopped
agreeing — the answer to why a provider-side prompt cache missed. It reads the
[content hashes](observability-schemas.md#wire-hashes) each run's `inference` lines recorded, so
both runs need [`trace.capture`](manifest.md#field-trace) `meta` or `content`.

```text
── Prefix divergence ────────────────────────────
system prompt: differs    A d27e9be1c0de…  B aaaaaaaaaaaa…
tool schemas:  identical  143f541e445d…
turn 1:  diverges at message 1  A 4d3fd85ffaa2…  B ffffffffffff…
turn 2:  identical  (2 messages)
```

| Line | Reports |
|---|---|
| `system prompt:` | The `system_sha` each run's first agent-loop turn recorded. A run that changes its system prompt mid-session gets a `note:` line naming the turn |
| `tool schemas:` | The `tools_sha` each run's first agent-loop turn recorded |
| `turn <n>:` | The two runs' `message_shas` for that turn, compared element-wise: the index of the first entry that differs, `identical (<n> messages)` when every entry agrees, or `only in run A` when the other run has no such turn. When one array is a prefix of the other, the divergence index is the shorter array's length and the line reports both lengths |

Divergence has no polarity, so no `(A better)`/`(B better)` marker appears in this section. When a
run recorded no hashes at all, one line names it and says it ran under `trace.capture: none`, and
nothing is compared.

### `mur trace report`

Aggregate statistics across a set of sessions. Useful for repeated-run experiments.

```bash
mur trace report [<session>...] [--last <n>] [--since <duration>] [--workdir <dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<session>...` | every session in the workdir | One or more [session addresses](#session-addresses). Cannot be combined with `--last` or `--since` |
| `--last` | — | Limit to the `n` most recently created sessions. Must be at least 1 |
| `--since` | — | Limit to sessions created within a duration, written `<n>m`, `<n>h` or `<n>d` |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories |

Output: a short block per session, then mean, population stddev, min, and max for each numeric metric, followed by exit status distribution. If any sessions contain more than one task, a **Per-task averages** section is appended showing per-task metrics across all multi-task sessions.

The aggregate section, for 3 sessions with no multi-task session:

```text
Sessions: 3  (./workdir)

Metric                 Mean           StdDev         Min            Max
────────────────────── ────────────── ────────────── ────────────── ──────────────
turns                  2.7            1.7            1.0            5.0
duration (ms)          800            648            200            1,700
input tokens           4,133          3,996          500            9,700
output tokens          513            420            100            1,090
tool calls             2.0            2.2            0.0            5.0
tool success (%)       90.0           10.0           80.0           100.0
shell calls            2.0            2.2            0.0            5.0
redundant calls        0.0            0.0            0.0            0.0

Exit status:
  max_turns_reached        1  (33.3%)
  ok                       2  (66.7%)
```

Example (the set includes multi-task sessions):

```text
Sessions: 2  (./workdir)

Metric                 Mean           StdDev         Min            Max
...

── Per-task averages (multi-task sessions only) ──────────────
Metric                 Mean           StdDev         Min            Max
────────────────────── ────────────── ────────────── ────────────── ──────────────
task turns             2.0            1.0            1.0            3.0
task input tokens      1,000          500            500            1,500
task output tokens     200            100            100            300
task duration (ms)     400            200            200            600
Tasks: 6
```

Notes:

- Sessions whose `trace.jsonl` holds no events are skipped, and a `note: skipped <n> incomplete session(s)` line on stderr says how many.
- Sessions with no tool calls are excluded from the `tool success (%)` row rather than counted as 0%.
- A single session produces stddev = 0.
- The Per-task averages section appears when at least one session ran more than one task. Traces carrying no task events are excluded from per-task aggregation.
- Exits non-zero if the workdir does not exist or holds no sessions.

---

## `mur eval`

Read and analyze `eval.jsonl` files produced by `murmur-hook-eval`, or drive a capsule against a dataset. These commands are read-only except for `mur eval run`, which launches real capsule sessions. They do not require a running registry (unless the capsule needs to pull artifacts).

See [Structured evaluation (`eval.jsonl`)](observability-schemas.md#structured-evaluation-evaljsonl) for the file format.

### `mur eval show`

Print a human-readable summary of a single session's scored events, or emit a JSON object for programmatic use.

```bash
mur eval show [<session>] [--workdir <dir>] [--json]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<session>` | `@1`, the most recent session in the workdir | A [session address](#session-addresses), resolved to that session's `eval.jsonl` |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories |
| `--json` | off | Emit a single pretty-printed JSON object instead of human-readable text |

Human output sections:

| Section | Contents |
|---|---|
| Scorers | Per-scorer pass count, total count, and pass rate (%) |
| Overall | `pass`, `fail`, or `no_scores` |
| Score summary | Per-scorer float score from the `dataset_run` summary record |
| Worst events | Up to 5 failing event_score records, sorted by scorer then turn |

With `--json`, emits a single pretty-printed JSON object:

```json
{
  "overall": "pass",
  "scorers": {
    "turn_limit": { "pass": 1, "fail": 0, "total": 1, "pass_rate": 1.0 }
  },
  "dataset_run": { "overall": "pass", "scores": { "turn_limit": 1.0 }, ... }
}
```

Exit codes: `0` on success (including empty files and no-scorer sessions), `1` on I/O error or parse error.

### `mur eval diff`

Compare two eval sessions side by side with a delta column.

```bash
mur eval diff [<a> <b>] [--workdir <dir>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<a>` | `@2` | The run in the Run A column, as a [session address](#session-addresses) |
| `<b>` | `@1` | The run in the Run B column, as a [session address](#session-addresses) |
| `--workdir` | `./workdir` | Directory holding the `ses_*` session directories |

Both addresses or neither: one argument is refused with
[`E-EVAL-002`](diagnostics.md).

Example output:

```text
Scorer                   Run A          Run B          Delta
──────────────────────── ────────────── ────────────── ──────────────────────────
success_check            0.0%           100.0%         +100.0pp (B better)
token_budget             100.0%         100.0%         =
turn_limit               100.0%         100.0%         =

overall                  fail           pass
```

- Delta is expressed in percentage points (`pp`).
- Scorers present in only one file are shown as `(A only)` or `(B only)`.
- An equal pass rate shows `=`.

### `mur eval run`

Run a capsule once per case in a dataset, collect `eval.jsonl` from each run, and print a per-case summary.

```bash
mur eval run <capsule-dir> --dataset <dataset.jsonl>
```

**Dataset format** — one JSON object per line:

```json
{ "case_id": "case_001", "task_path": "/path/to/task.md" }
{ "case_id": "case_002", "task_path": "/path/to/task2.md", "expected": "optional" }
```

| Field | Required | Description |
|---|---|---|
| `case_id` | yes | Identifier passed as `MURMUR_CASE_ID` to hooks; appears in `dataset_run` records |
| `task_path` | yes | File to copy into the capsule's `workdir/task.md` before session launch |
| `expected` | no | Scorer-defined; ignored by current deterministic scorers; reserved for future `llm_judge` |

**What happens per case:**

1. Stages the capsule session with `case_id` and `dataset_id` injected into the hook environment.
2. Copies `task_path` to `workdir/task.md`. If the file does not exist, a warning is printed and the session runs without it.
3. Launches the session.
4. Reads `workdir/eval.jsonl` from the resulting session workdir.
5. Prints a result line: `result: pass|fail|no_scores  session: <id>`.

After all cases, prints a summary table:

```text
── Summary ──────────────────────────────────────
pass: 2/2

  case_001                 pass  success_check=1.00 turn_limit=1.00  (/path/to/workdir/...)
  case_002                 pass  success_check=1.00 turn_limit=1.00  (/path/to/workdir/...)
```

**Non-obvious behaviour:**

- `mur eval run` reads `murmur.yaml` from `<capsule-dir>/murmur.yaml`. The capsule must declare `murmur-hook-eval` in its `artifacts:` block — the CLI does not inject the hook automatically.
- The lockfile (`murmur.lock`) is read from `<capsule-dir>/murmur.lock`. If absent, one is created on the first case run and reused for subsequent cases.
- A case that fails to stage (e.g. missing artifact) is recorded as `stage_failed` and does not count toward `pass`.
- `MURMUR_DATASET_ID` is taken from `observability.eval.dataset_id` in the manifest, not from the dataset file.

---

## `mur topology`

Query a Grafana Tempo instance for capsule session traces and render them as an interactive DAG in the default browser.

```bash
mur topology --otel-endpoint <URL> [--window <DURATION>] [--output <PATH>] [--port <PORT>]
```

| Flag | Default | Description |
|---|---|---|
| `--otel-endpoint` | required (or `MURMUR_OTEL_ENDPOINT` env) | Grafana Tempo HTTP query API endpoint (e.g. `http://localhost:3200`) — this is the **query port**, not the OTLP ingest port |
| `--window` | `1h` | Time window to query: `30m`, `1h`, `6h`, `24h`, `7d` |
| `--output` | — | Write HTML to this file path instead of opening a browser |
| `--port` | — | Serve the HTML on a local port and open browser at `http://127.0.0.1:<port>` |

**What the page shows:**

- Each **node** is one `capsule.session` span — capsule name, version, exit status, total duration
- Node **color** reflects exit status: green = ok, red = failed, yellow = running, orange = error/unknown
- **Edges** are directed parent → child, derived from W3C TraceContext parent span references across traces
- Edge **weight** encodes call volume (multiple A2A sends from the same parent to the same child)
- **Node tooltip** includes per-span timing: inference ms, tool call ms, shell ms

The graph requires capsules to have `observability.otel_endpoint` configured in their manifests. See [Work with capsule trace spans in Grafana](../how-to/grafana-tempo-spans.md) for the full setup guide.

**Examples:**

```bash
# open in browser from last hour
mur topology --otel-endpoint http://localhost:3200

# write HTML to file (no browser opened)
mur topology --otel-endpoint http://localhost:3200 --output /tmp/topology.html

# query last 6 hours, serve on port 8080
mur topology --otel-endpoint http://localhost:3200 --window 6h --port 8080

# read endpoint from environment
MURMUR_OTEL_ENDPOINT=http://localhost:3200 mur topology
```

**Exit codes:**

- `0` — Tempo reachable; HTML written (even when no traces found — empty graph with message)
- `1` — Tempo unreachable (`E-TOP-001`), HTTP query failed (`E-TOP-002`), parse error (`E-TOP-003`), or I/O error (`E-IO-003`)

When Tempo is reachable but no `capsule.session` spans exist in the time window, the command exits `0` and the HTML shows "No capsule sessions found in the selected time window."

The generated HTML is self-contained: all graph data is embedded as `window.TOPOLOGY_DATA` JSON; [vis.js Network](https://visjs.github.io/vis-network/docs/network/) is loaded from CDN. No server required to view the file.

---

## `mur search`

Search the public artifact index for artifacts matching a keyword.

```bash
mur search <query> [--registry <URL|local>] [--limit <n>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `<query>` | required | Case-insensitive keyword matched against artifact name, description, and tags |
| `--registry` | public index URL | `local` scans `~/.murmur/artifacts/`; an absolute file path reads a local index file; any URL fetches that index |
| `--limit` | `10` | Maximum number of results to show |

**Default behaviour (no `--registry`):** fetches the public artifact index from the configured URL (default: the Murmur default-artifacts repository). Override the URL with `registry.index_url` in the effective (global + project-level, merged) config — see [Artifact index and custom registry URL](config.md#artifact-index-and-custom-registry-url) and [Configuration files](config.md#configuration-files).

**Output format:**

```text
NAME                     VERSION  RUNTIME  DESCRIPTION
murmur-tool-git          {{ v.murmur_tool_git }}    tool     Structured git interface for Murmur capsules.
murmur-driver-anthropic  {{ v.murmur_driver_anthropic }}   driver   Anthropic Messages API inference driver for Murmur agent capsules.
```

When no artifacts match, prints `No results found.` and exits `0` (not an error).

**Examples:**

```bash
# Search the public index for git-related artifacts
mur search "git"

# Search only locally installed artifacts
mur search "editor" --registry local

# Use a private or custom index
mur search "git" --registry https://my-org.example.com/artifacts-index.json

# Cap results at 3
mur search "murmur" --limit 3
```

**Error cases:**

- Network unreachable or DNS failure → exits `1`; error message names the URL
- Non-2xx HTTP response → exits `1`; error includes the HTTP status
- Malformed JSON or missing `schema_version` → exits `1`; error describes the parse failure
- Unsupported `schema_version` → exits `1`; error names the version found and the URL

---

## `mur beta`

Manage opt-in beta features. Beta features are capabilities that are compiled into the binary
but hidden behind a runtime flag until explicitly enabled.

```bash
mur beta list
mur beta enable  <feature>
mur beta disable <feature>
```

### `mur beta list`

Reads the effective (global + project-level, merged) config — see
[Configuration files](config.md#configuration-files) — so a `beta.enabled` flag set in either
`~/.murmur/config.yaml` or `<cwd>/.murmur/config.yaml` shows as enabled. Lists all beta features
compiled into this build and their current enabled status. On a
standard release build with no beta features compiled in, prints:

```text
This build has no beta features.
```

When beta features are present:

```text
Beta features compiled into this build:

  blueprint            disabled  Blueprint file support in taskflow stage slots
  dag-topology         enabled   DAG-based multi-stage topology (Fleet v1.1 preview)

Use `mur beta enable <name>` or `mur beta disable <name>` to opt in or out.
```

### `mur beta enable <feature>`

Adds `feature` to the `enabled` list in `~/.murmur/config.yaml` (global — there is no `-g`/project
flag on this command). If `feature` is not compiled
into this build, a warning is printed and the flag is saved anyway (useful for pre-enabling
before upgrading to a build that includes the feature).

```bash
mur beta enable blueprint
# Warning: 'blueprint' is not compiled into this build. The flag will be saved
# but has no effect until a build that includes it is installed.
# Beta feature 'blueprint' enabled.
```

Idempotent: calling `enable` on an already-enabled feature prints "already enabled" and makes
no change to the config.

### `mur beta disable <feature>`

Removes `feature` from the enabled list. Idempotent: if the feature is not currently enabled,
prints "already disabled" and exits `0`.

```bash
mur beta disable blueprint
# Beta feature 'blueprint' disabled.

mur beta disable blueprint
# Beta feature 'blueprint' is already disabled.
```

**Persistence:** enabled flags are written to `~/.murmur/config.yaml` under the `beta:` section.
See [Configuration files](config.md#configuration-files).

---

## `mur config`

Read and write individual keys in the CLI config files described in
[Configuration files](config.md#configuration-files).

```bash
mur config set <key> <value> [-g|--global]
```

### `mur config set <key> <value>`

Writes `<key>` to the project-level file at `<cwd>/.murmur/config.yaml` by default. Pass
`-g`/`--global` to write `~/.murmur/config.yaml` instead.

These dotted keys are settable:

| Key | Maps to |
|---|---|
| `registry.default` | `registry.default` |
| `registry.index_url` | `registry.index_url` |
| `inference.provider` | `inference.provider` |
| `inference.model` | `inference.model` |
| `inference.api_key` | `inference.api_key`. Also prints a note that `mur run` reads provider keys from `credentials.<NAME>` |
| `inference.endpoint` | `inference.endpoint` |
| `credentials.<NAME>` | One entry of [`credentials:`](config.md#credentials). `-g` only; `NAME` matches `[A-Z_][A-Z0-9_]*`. Running capsules use a replaced entry on their next inference request |

`registry.sources` and `beta.enabled` are list-typed and **not** settable with `config set` —
edit `registry.sources` by hand in the YAML file, and use
[`mur beta enable`/`mur beta disable`](#mur-beta-enable-feature) for `beta.enabled`.

Setting a key never clobbers other keys already present in the target file:

```bash
mur config set registry.default official
# Set registry.default in ./.murmur/config.yaml

mur config set inference.model claude-haiku-4-5-20251001 -g
# Set inference.model in ~/.murmur/config.yaml
```

Any other dotted key — a known config field or not — is rejected with `E-CFG-002` and writes
nothing:

```bash
mur config set nonsense.field value
# error[E-CFG-002]: unsupported config key 'nonsense.field'
#   hint: supported keys: registry.default, registry.index_url, inference.provider, inference.model, inference.api_key, inference.endpoint, credentials.<NAME>
```

`credentials.<NAME>` without `-g`, or with a `NAME` outside the grammar, is refused with the same
code:

```bash
mur config set credentials.ANTHROPIC_API_KEY sk-ant-...
# error[E-CFG-002]: 'credentials.ANTHROPIC_API_KEY' can only be set in the global config
#   hint: credentials are read from the global config (~/.murmur/config.yaml) only; run `mur config set -g credentials.ANTHROPIC_API_KEY <key>`
```

!!! warning "`inference.api_key` is always global"
    `inference.api_key` is the one key that ignores the project-wins merge rule below — the
    *effective* config always takes it from the global file, never the project file. Running
    `mur config set inference.api_key <value>` **without** `-g` still writes the value to the
    project file, but it prints a warning first and the value has no effect on what `mur`
    actually uses:

    ```text
    warning: writing a literal inference.api_key to ./.murmur/config.yaml has no effect; inference.api_key is always read from the global config (~/.murmur/config.yaml) — this project-level value will be ignored when resolving effective config
    ```

    No warning is printed for a `${VAR}`-shaped value — see
    [`inference.api_key` is always global](config.md#inferenceapi_key-is-always-global).
