# CLI Commands

This page documents the **currently implemented** `mur` CLI surface in this repository.

## Implemented commands

| Command | Purpose |
|---|---|
| `mur build` | Build a `.mur.zip` from a source directory |
| `mur new` | Generate a `murmur.yaml` from a plain-language task description |
| `mur publish` | Publish a built artifact to local or remote registry |
| `mur install` | Fetch and install artifacts from configured registry sources |
| `mur list` | List installed artifacts in the project or global store |
| `mur doctor` | Check every artifact declared in `murmur.yaml` against the project and global stores |
| `mur run` | Run a capsule with lockfile-aware artifact resolution |
| `mur watch` | Stream live events from a running capsule's output to stdout |
| `mur deploy` | Upload a capsule to an existing VM and return its public URL |
| `mur destroy` | Remove a deployment record from the local tracking list |
| `mur ps` | List all deployed capsules |
| `mur trace show` | Human-readable summary of a single `trace.jsonl` session |
| `mur trace diff` | Side-by-side metric comparison of two trace files |
| `mur trace report` | Aggregate statistics across all `.jsonl` files in a directory |
| `mur eval show` | Human-readable (or JSON) summary of a single `eval.jsonl` session |
| `mur eval diff` | Side-by-side scorer comparison of two eval files |
| `mur eval run` | Drive a multi-case dataset and collect `eval.jsonl` per run |
| `mur search` | Search the public artifact index for artifacts matching a keyword |
| `mur topology` | Render capsule sessions as a DAG from Grafana Tempo OTel data |

> `init` is described in design docs but is not implemented in this codebase yet.

---

## `mur new`

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
    [Configuration files](#configuration-files) below.
- The driver artifact for your chosen provider must be installed:
    - Anthropic: `murmur-driver-anthropic@0.3.32`
    - OpenAI: `murmur-driver-openai@0.3.33`
- `murmur-tool-registry-search@0.4.13` must be installed

Install missing artifacts with `mur install <name>@<version>`. `mur new` exits with a clear
error and install hint if either artifact is missing.

**Output:**

- `murmur.yaml` written to the current working directory
- Nothing is written if the generator fails or produces invalid YAML

**Examples:**

```bash
# Generate a manifest for a PR security review capsule
mur new "review this PR for security issues"

# Use locally installed artifacts (ensures generated versions are available)
mur new "summarise a document" --registry local

# Research/report task — generator adds spawn.scoped: true
mur new "research climate change trends and produce a report"
```

**Single capsule vs orchestrator:**

The generator automatically infers whether the task is:

- **Single capsule** — focused, bounded task (e.g. "review this PR", "summarise a document"): produces a minimal manifest without `spawn`.
- **Orchestrator** — research, multi-step, pipeline, or report tasks: adds `capabilities.spawn.scoped: true` so the capsule can spawn child capsules.

**Generator behavior:**

The generator runs `murmur-tool-registry-search` to discover relevant artifacts, then outputs a complete `murmur.yaml` as its final text response. The CLI reads `out/result.txt`, strips any markdown fences, validates the YAML via `RuntimeManifest::from_yaml_str`, and copies the result to `./murmur.yaml`.

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
| `E-IO-003` | Generator produced no output, or `murmur.yaml` could not be written |

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
- Scans `murmur.yaml` and optional `murmur.yaml` for literal secret patterns and emits warnings
- Packages **`murmur.yaml` plus exactly the files listed in `requires_files:`** — the rest of the
  source directory (`src/`, `Cargo.toml`, `README.md`, editor files, build output) is not packaged.
  `mur build` never compiles anything, so a `.wasm` payload must already exist on disk and be
  declared. A native or static artifact that declares no `requires_files:` builds to a
  manifest-only archive; a **wasm** artifact does not — with no root `*.wasm` to pack it fails
  with [`E-BLD-003`](build-lints.md#e-bld-003).
- Validates the manifest `name:`, the `requires_files:` paths and the resulting payload shape,
  and warns about redundant or misplaced declarations — see [Build Lints](build-lints.md)
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
| `mur install --all-platforms <name@version>` | Downloads all platform variants into the global store; useful for CI and cross-platform build seeding |

`mur install` (no args) is the standard pre-run step. It reads `murmur.yaml`, resolves every artifact listed in it, and if an artifact is not found in the local registry it falls back to the configured source chain automatically.

Example — seed a project before running:

```bash
mur install
mur run
```

Example — install a specific artifact:

```bash
mur install murmur-tool-git@0.3.31
```

Behavior:

1. Resolve artifact from configured registry source (falls back through source chain if not found locally)
2. Verify SHA256 integrity
3. Store into the project-local store (or global store with `-g`)

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
```

| Flag | Shows |
|---|---|
| _(none)_ | Project store (`.murmur/artifacts/` next to `murmur.yaml`) when inside a project directory |
| `-g` | Global store (`~/.murmur/artifacts/`) |
| `--all` | Both stores; output includes a `SCOPE` column (`project` or `global`) |

Example output (`mur list`):

```text
NAME                     VERSION  RUNTIME  PLATFORM
murmur-driver-anthropic  0.3.32   driver   wasm
murmur-tool-git          0.3.31   tool     darwin-aarch64
```

Example output (`mur list --all`):

```text
NAME                     VERSION  RUNTIME  PLATFORM        SCOPE
murmur-driver-anthropic  0.3.32   driver   wasm            project
murmur-tool-git          0.3.31   tool     darwin-aarch64  global
```

---

## `mur doctor`

Check that every artifact declared in the current project's `murmur.yaml` is available to a session — the same project-store-then-global-store, current-platform resolution `mur run` performs before staging.

```bash
mur doctor
```

`mur doctor` takes no flags or arguments. It walks up from the current directory to find `murmur.yaml` (same walk `mur install` uses), loads it, and prints one checklist line per declared artifact:

- `✓ name@version   <platform>` — resolved from the project store (`.murmur/artifacts/`) or the global store (`~/.murmur/artifacts/`) for the current platform, and agrees with `murmur.lock` if one is present (see below)
- `✓ name@version   local source` — declared with a `source:` path; resolved from the filesystem at stage time, never checked against a registry or a lockfile
- `✗ name@version   <platform>   — missing` — resolved from neither store

There is no hardcoded artifact list: the checklist is derived entirely from `murmur.yaml`'s `artifacts:` block. Editing a version pin or adding/removing an artifact changes what `mur doctor` checks, with no code change.

**Output — happy path:**

```text
Checking /path/to/murmur.yaml for darwin-aarch64...
  ✓  murmur-driver-anthropic@0.3.32   darwin-aarch64
  ✓  murmur-tool-git@0.3.31           darwin-aarch64

All checks passed.
```

**Output — one or more artifacts missing:**

```text
Checking /path/to/murmur.yaml for darwin-aarch64...
  ✗  murmur-tool-git@0.3.31   darwin-aarch64   — missing

0 checks passed, 1 error found.

Fix: mur install murmur-tool-git@0.3.31
```

### Lock integrity

When `murmur.lock` is present in the project, `mur doctor` checks every registry-resolved (non-local-source) artifact against it the same way `mur run` does before staging a session: the lock must have an entry for the artifact, that entry's `resolved_version` must match the version `murmur.yaml` declares, and its recorded sha256 must match the sha256 of the bytes actually installed. Any disagreement turns that artifact's line into a `✗`, counts toward the failure tally, and gets a `Fix:` line — so `mur doctor` never reports "All checks passed." on a project `mur run` would reject. When no `murmur.lock` exists, `mur doctor` falls back to presence-only checking and prints nothing lock-related, exactly as before this check existed.

- `✗ name@version   <platform>   — murmur.lock missing artifact entry for 'name'` — the lock exists but has no entry for this artifact
- `✗ name@version   <platform>   — murmur.lock version mismatch for 'name': manifest requested X, lock pinned Y` — the lock pins a different version than `murmur.yaml` declares
- `✗ name@version   <platform>   — artifact integrity check failed for name@version` (with `expected sha256 (murmur.lock):` / `actual sha256 (on disk):` detail lines) — the installed bytes don't hash to the lock's recorded sha256

**Output — lock hash mismatch:**

```text
Checking /path/to/murmur.yaml for darwin-aarch64...
  ✗  demo-skill@0.1.0   darwin-aarch64   — artifact integrity check failed for demo-skill@0.1.0
        expected sha256 (murmur.lock): deadbeef
        actual sha256 (on disk):       0e29c7e8c291a2800a266a01c28300e24f2a640d4a21a085e4fd9aee01adfaef

0 checks passed, 1 error found.

Fix: demo-skill: artifact on disk does not match murmur.lock — re-publish or delete the lock
```

A `murmur.lock` that exists but fails to parse or fails validation is a hard failure — `mur doctor` reports it and exits before printing any checklist line, the same way a malformed `murmur.yaml` already does.

**Exit codes:**

- `0` — every declared artifact resolved (or is local-source), and agrees with `murmur.lock` if one is present
- `1` — one or more declared artifacts missing, or disagree with `murmur.lock` (checklist printed to stdout first), or a setup failure (no checklist printed; error goes to stderr)

**Error codes:**

| Code | Meaning |
|---|---|
| `E-IO-001` | No `murmur.yaml` found in the current directory or any parent |
| `E-MAN-001` / `E-MAN-002` / `E-MAN-003` | Manifest failed to load — missing field, YAML syntax error, or invalid field, respectively |
| `E-RUN-003` | `murmur.lock` exists but failed to parse or validate |

A setup failure (no project found, the manifest fails to load, or the lockfile fails to parse) is reported on stderr before any checklist is printed — `mur doctor` never reports "all checks passed" against zero artifacts because the manifest or lockfile couldn't be read.

---

## `mur run`

Run capsule component and resolve declared artifacts.

```bash
mur run [--manifest <path>] [--input <path>] [--json]
```

| Flag | Default | Description |
|---|---|---|
| `--manifest` | `./murmur.yaml` | Path to the capsule manifest |
| `--task` | — | File to copy into the capsule workdir as `task.md` before launch |
| `--json` | off | Emit launch info as a single JSON line instead of human-readable output |
| `--no-env-file` | off | Skip auto-loading the workspace-root `.env` file for this invocation. Recommended default for CI/CD pipelines |

- Auto-loads `.env` from nearest workspace containing `murmur.yaml`, unless `--no-env-file` is passed
- Creates/uses `murmur.lock` in manifest directory

**Artifact pre-check:** Before staging, `mur run` verifies that all artifacts declared in the manifest are installed locally. If any are missing it exits immediately with `error[E-RUN-008]` and a `mur install` hint. Run `mur install` first to fetch missing artifacts.

Current runtime constraints:

- `mur run` accepts all four artifact runtimes: `tool`, `driver`, `hook`, and `skill`
- `tool` artifacts are exposed as model-callable tools; `driver`, `hook`, and `skill` artifacts are staged for runtime use but are hidden from the model's tool inventory
- `skill` artifacts install `skill.md` to `tools/<name>/skill.md` in the workdir; the agent reads them voluntarily via filesystem access
- Capsule component discovery:
  - prefers `capsule.wasm`
  - otherwise requires exactly one root `*.wasm`
- Agent capsules require either `transport: http` (with `inference.driver.artifact`) or `transport: process` (with `inference.command`) in `murmur.yaml`; missing driver config exits with `error[E-RUN-005]` or `error[E-RUN-006]` respectively

### Human-readable output (default)

Without `--json`, `mur run` prints three lines to stdout after the session completes:

```text
murmur: url localhost:<port>
session: <id>
workdir: <absolute-path>
status:  ok
```

The `murmur: url` line is emitted immediately after the capsule's HTTP server port is bound (before the agent loop starts). The `session:`, `workdir:`, and `status:` lines are printed after the session completes.

### Programmatic output (`--json`)

Pass `--json` to emit a single JSON line to stdout immediately after the capsule's HTTP server port is bound, before the agent loop starts. This enables any external process to discover the capsule URL without parsing human-readable stdout.

```bash
mur run --json --manifest ./murmur.yaml
```

Output — one JSON line written to stdout after port binding:

```json
{"url":"localhost:PORT","pid":12345,"session_id":"ses_..."}
```

Field semantics:

| Field | Type | Description |
|---|---|---|
| `url` | string | `localhost:PORT` — the capsule's A2A HTTP endpoint; use this for `message/send`, `tasks/get`, `message/stream`, and `/.well-known/agent-card.json` |
| `pid` | number | The `mur` process ID (`u32`); use this to manage process lifecycle (wait, kill) |
| `session_id` | string | The session identifier assigned to this run; correlates with the `session_id` field in `workdir/<session_id>/trace.jsonl`'s `session_start` event |

**Trace correlation:** The `session_id` in the JSON output is the same identifier written to `workdir/<session_id>/trace.jsonl` as the `session_start.session_id` field. Use it to join the programmatic launch record with the session's trace for log correlation and post-run analysis.

**Startup latency:** The JSON line is written after the TCP port is bound and the HTTP server is listening, but callers should connect with retry/backoff since a small window exists between port binding and the accept loop processing the first request.

**Error behavior:** If `mur run --json` fails before port binding (e.g. bad manifest path, missing artifact), stdout is empty — no JSON is emitted and no human-readable output appears on stdout. The error message goes to stderr. Exit code is non-zero.

---

## `mur watch`

Stream live SSE events from a running capsule's output to stdout. The command opens a
`stream/watch` connection to the capsule and prints each event in a human-readable
format until the capsule closes or Ctrl+C is pressed.

```bash
mur watch <capsule_url>
```

- `capsule_url` — the `localhost:<port>` URL printed by `mur run` (with or without `http://`)

Output format:

```text
[working]  inference turn 1
[artifact] tool: bash | "Exit code: 0\nStdout:\nhello\n"
[working]  inference turn 2
[completed]
```

Exit codes:

- `0` — terminal state event received (`completed` or `failed`)
- `1` — connection error or non-200 response from the capsule

---

## `mur deploy`

Upload the `mur` binary and capsule files to an existing VM via SSH, start the capsule, and print the public A2A endpoint. The VM must already exist and be reachable via SSH — `mur deploy` never provisions or terminates VMs on your behalf.

```bash
mur deploy --host <ip> [--ssh-user <user>] [--ssh-key <path>]
           [--manifest <path>] [--workdir <path>] [--mur-binary <path>]
           [--env KEY=VALUE] ...
```

| Flag | Default | Description |
|---|---|---|
| `--host` | — | IP address or hostname of the target VM (required) |
| `--ssh-user` | `root` | SSH username on the VM |
| `--ssh-key` | — | Path to SSH private key; uses SSH agent if omitted |
| `--manifest` | `./murmur.yaml` | Path to the capsule manifest to deploy |
| `--workdir` | — | Local directory to upload as the capsule's working directory |
| `--mur-binary` | current executable | Path to a Linux x86_64 `mur` binary to upload. Defaults to `std::env::current_exe()`. Always specify this flag when deploying from macOS. |
| `--env` | — | Environment variable in `KEY=VALUE` format; repeat for multiple vars |

**Output — exactly one JSON line to stdout:**

```json
{"url":"https://IP:PORT","ip":"IP","job_id":"UUID"}
```

| Field | Description |
|---|---|
| `url` | Public A2A endpoint — `https://<VM_PUBLIC_IP>:<PORT>`. Use for `message/send`, `tasks/get`, and `/.well-known/agent-card.json`. |
| `ip` | The VM's public IPv4 address |
| `job_id` | UUID v4 — use with `mur destroy` and `mur ps` |

**Deployment flow:**

1. Validate `--manifest`, `--workdir`, and `--mur-binary` paths (no network calls)
2. Wait up to 30s for SSH to become available on the VM
3. Upload `mur` binary via `scp` to `/usr/local/bin/mur`
4. Upload manifest and optional workdir via `scp`
5. Run `mur run --manifest <path> --json` on the VM; wait up to 30s for the JSON line
6. Parse `localhost:PORT` from the JSON output; construct the public URL
7. Persist to `~/.murmur/deployments.json`; print the JSON line

Artifacts are pre-staged in step 4 (uploaded to `/root/.murmur/artifacts/`), so the remote `mur run` finds them installed and starts without fetching anything.

The flow depends on `mur run --json` — see [`mur run`](#mur-run) for the `--json` output shape.

**Example:**

```bash
mur deploy \
  --host 1.2.3.4 \
  --manifest ./my-agent/murmur.yaml \
  --mur-binary ./target/x86_64-unknown-linux-musl/release/mur \
  --env ANTHROPIC_API_KEY=sk-ant-...
# {"url":"https://1.2.3.4:9000","ip":"1.2.3.4","job_id":"a1b2c3d4-..."}
```

**Error codes:**

| Code | Meaning |
|---|---|
| `E-IO-001` | `--manifest`, `--workdir`, or `--mur-binary` path not found |
| `E-DEPLOY-003` | SSH connection or remote command failed |
| `E-DEPLOY-004` | Capsule did not emit startup JSON within 30s |

---

## `mur destroy`

Remove a deployment entry from `~/.murmur/deployments.json`. Does not stop or delete the VM — shut down the VM from your cloud provider's dashboard separately.

```bash
mur destroy <job_id>
```

- `job_id` — the UUID returned by `mur deploy` (also listed by `mur ps`)
- Exits non-zero with a clear error if `job_id` is not found in `~/.murmur/deployments.json`

**Example:**

```bash
mur destroy a1b2c3d4-e5f6-7890-abcd-ef1234567890
# destroyed a1b2c3d4-... (1.2.3.4)
```

---

## `mur ps`

List all deployed capsules tracked in `~/.murmur/deployments.json`.

```bash
mur ps
```

Output columns:

| Column | Description |
|---|---|
| `JOB_ID` | UUID assigned at deploy time |
| `PROVIDER` | Always `manual` — VMs are created by the user, not by `mur deploy` |
| `IP` | Public IPv4 address of the VM |
| `STATUS` | Always `running` for present entries (`mur destroy` removes the entry) |
| `URL` | Public A2A endpoint (`https://IP:PORT`) |

Prints `no deployments` when `~/.murmur/deployments.json` is absent or empty.

**Example:**

```text
JOB_ID                                  PROVIDER    IP            STATUS      URL
----------------------------------------------------------------------------------------------------
a1b2c3d4-e5f6-7890-abcd-ef1234567890    manual      1.2.3.4       running     https://1.2.3.4:9000
```

---

## `deployments.json`

Location: `~/.murmur/deployments.json`

A JSON array that tracks all active deployments. Written on `mur deploy`; entries removed on `mur destroy`. Schema per entry:

```json
{
  "job_id":         "a1b2c3d4-...",
  "provider":       "manual",
  "provider_vm_id": "",
  "provider_key_id": "",
  "region":         "",
  "ip":             "1.2.3.4",
  "url":            "https://1.2.3.4:9000",
  "manifest_path":  "/Users/you/my-agent/murmur.yaml",
  "started_at":     "2026-06-03T12:00:00+00:00",
  "status":         "running"
}
```

| Field | Description |
|---|---|
| `job_id` | UUID v4 — the deployment's identity across all commands |
| `provider` | Always `"manual"` — VMs are created by the user outside of `mur` |
| `provider_vm_id` | Always empty — reserved for future provider integrations |
| `provider_key_id` | Always empty — reserved for future provider integrations |
| `region` | Always empty — reserved for future provider integrations |
| `ip` | Public IPv4 of the VM (the value passed to `--host`) |
| `url` | `https://IP:PORT` — the public A2A endpoint |
| `manifest_path` | Absolute local path to the manifest used at deploy time |
| `started_at` | RFC 3339 timestamp of when the deployment was created |
| `status` | Always `"running"` — entries are removed on destroy, not updated |

---

## `mur trace`

Read and analyze `trace.jsonl` files produced by `mur run`. These commands are read-only — they do not modify any file and do not require a running registry or runtime.

See [Session trace (`trace.jsonl`)](#session-trace-tracejsonl) below for the file format.

### `mur trace show`

Print a human-readable summary of a single session.

```bash
mur trace show <trace.jsonl>
```

Output sections:

| Section | Contents |
|---|---|
| Session | `session_id`, capsule name+version, model, exit status, duration |
| Turns | Turn count and configured max |
| Tokens | Input tokens, output tokens, total, and per-turn averages |
| Tool calls | Count, ok/error breakdown, success rate, average latency, plus a per-turn breakdown of every call |
| Shell calls | Count, exit code distribution, average latency |
| Compaction | Whether it fired; turn number and before/after token counts |
| Tasks | Per-task breakdown (only shown when the session contains more than one task) |

The **Tasks** section appears only for multi-task persistent capsule sessions. Single-task and legacy traces (no task events) are unaffected — their output is unchanged.

Below the **Tool calls** summary line, each turn that made at least one tool call gets its own row: tool name, duration, a `✓`/`✗` status icon, and — when the call carried an `input` — its compact-JSON input, truncated to 120 characters with a trailing `…` if longer. Calls without a recorded input (older traces, or tools invoked with no arguments) show no input segment at all.

Example (single-task session — no Tasks section):

```text
── Session ──────────────────────────────────────
session:    ses_aaaaaaaaaaaa4aaa8aaa000000000001
capsule:    my-agent v0.1.0
model:      claude-3-5-sonnet
status:     ok
duration:   500ms

── Turns ────────────────────────────────────────
count:      2  (max: 10)

── Tokens ───────────────────────────────────────
input:      2,200  (avg 1100/turn)
output:     350  (avg 175/turn)
total:      2,550

── Tool calls ───────────────────────────────────
count:      1  (1 ok, 0 error)  success 100.0%
latency:    avg 100ms
  turn 0  bash 100ms ✓  {"command":"cargo test --workspace"}

── Shell calls ──────────────────────────────────
count:      1
exit codes: 1×0
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

### `mur trace diff`

Compare two trace files side by side, with a delta and directional indicator per metric.

```bash
mur trace diff <trace-a.jsonl> <trace-b.jsonl>
```

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

### `mur trace report`

Aggregate statistics across all `.jsonl` files in a directory. Useful for repeated-run experiments.

```bash
mur trace report <directory>
```

Output: mean, population stddev, min, and max for each numeric metric, followed by exit status distribution. If any sessions contain more than one task, a **Per-task averages** section is appended showing per-task metrics across all multi-task sessions.

Example (3 files, no multi-task sessions):

```text
Files: 3  (/path/to/traces)

Metric                 Mean           StdDev         Min            Max
────────────────────── ────────────── ────────────── ────────────── ──────────────
turns                  2.7            1.7            1.0            5.0
duration (ms)          800            648            200            1,700
input tokens           4,133          3,996          500            9,700
output tokens          513            420            100            1,090
tool calls             2.0            2.2            0.0            5.0
tool success (%)       90.0           10.0           80.0           100.0
shell calls            2.0            2.2            0.0            5.0

Exit status:
  max_turns_reached        1  (33.3%)
  ok                       2  (66.7%)
```

Example (directory includes multi-task session files):

```text
Files: 2  (/path/to/traces)

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

- Only `.jsonl` files are read; other files in the directory are ignored.
- Files with no tool calls are excluded from the `tool success (%)` row (not counted as 0%).
- A single file produces stddev = 0.
- The Per-task averages section only appears when at least one multi-task session (more than one task) is present. Old-format traces (no task events) are excluded from per-task aggregation.
- Exits non-zero if the directory does not exist or contains no `.jsonl` files.

### Session trace (`trace.jsonl`) schema { #session-trace-tracejsonl }

Every agent session produces a structured trace at `workdir/<session_id>/trace.jsonl`. The
file is written by the runtime — not by the capsule — and cannot be suppressed or falsified
by the capsule. It exists even when no hook artifacts are declared.

**Format:** one JSON object per line (JSONL), UTF-8, line-terminated. Every line carries
`event_type` (discriminator), `session_id` (runtime-generated UUID v7, identical on every line
in a session), and `timestamp` (Unix milliseconds).

**Event types** — six standard events plus two A2A events and two task events:

**`session_start`** — written before the first inference call

| Field | Type |
|---|---|
| `capsule_name` | string |
| `capsule_version` | string |
| `model` | string |
| `max_turns` | u32 |
| `capabilities` | string[] |
| `tools_declared` | string[] |

**`inference`** — written after each driver response is parsed

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | Zero-based turn index |
| `input_tokens` | u64 | |
| `output_tokens` | u64 | |
| `decision` | string | `"tool_call"` \| `"end_turn"` \| `"text"` |
| `tool_name` | string \| null | Present only when `decision` is `"tool_call"` |

**`tool_call`** — written after each tool invocation returns

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `tool_name` | string | |
| `input_bytes` | u64 | Byte length of the serialized tool input |
| `output_bytes` | u64 | Byte length of the tool output text |
| `duration_ms` | u64 | |
| `status` | string | `"ok"` \| `"error"` |

**`shell`** — written after each shell command returns (follows its `tool_call` line)

| Field | Type | Notes |
|---|---|---|
| `turn` | u32 | |
| `command` | string | First 200 characters |
| `exit_code` | i32 | Non-zero is data, not an error |
| `stdout_bytes` | u64 | |
| `stderr_bytes` | u64 | |
| `duration_ms` | u64 | |

**`compaction`** — written when context compaction fires

| Field | Type |
|---|---|
| `turn` | u32 |
| `tokens_before` | u64 |
| `tokens_after` | u64 |

**`session_end`** — always the last line; written on every exit path

| Field | Type | Notes |
|---|---|---|
| `total_turns` | u32 | Equals the count of `inference` lines |
| `total_input_tokens` | u64 | |
| `total_output_tokens` | u64 | |
| `total_tool_calls` | u32 | Equals the count of `tool_call` lines |
| `total_shell_calls` | u32 | Equals the count of `shell` lines |
| `duration_ms` | u64 | Wall-clock time from session start |
| `exit_status` | string | `"ok"` \| `"failed"` \| `"max_turns_reached"` |

**`a2a_task_received`** — written when an incoming message reserves the task slot

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Runtime-generated UUID |
| `context_id` | string | Echoed or generated `contextId` |
| `message_id` | string | `messageId` from the incoming A2A Message |
| `traceparent_from_caller` | string \| null | W3C `traceparent` header from the incoming request |

**`a2a_send`** — written when a script capsule calls `murmur:message/send`

| Field | Type | Notes |
|---|---|---|
| `peer_url` | string | Target capsule URL |
| `message_id` | string | `message-id` from the outgoing Message |
| `task_id` | string | Task ID returned by the peer |
| `context_id` | string | Context ID returned by the peer |
| `traceparent` | string \| null | W3C `traceparent` injected on the outgoing request |

**`task_start`** — written at the start of each task, before `run_agent_loop`

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | UUID for this task (runtime-generated for A2A; synthesized for `task.md` path) |
| `context_id` | string | Context UUID for this task |
| `source` | string | `"a2a"` for A2A tasks; `"task_md"` for the task.md path |
| `message_parts_bytes` | u64 | Byte length of the task message text |

Resets all per-task counters. Follows `a2a_task_received` for A2A tasks; is the first event for `task.md` tasks.

**`task_end`** — written after `run_agent_loop` returns, for every task

| Field | Type | Notes |
|---|---|---|
| `task_id` | string | Matches the corresponding `task_start` |
| `exit_status` | string | `"ok"` if the agent loop returned `Ok(())`; `"failed"` if it returned `Err(_)` |
| `duration_ms` | u64 | Wall-clock time from `task_start` to `task_end` |
| `turns` | u32 | Inference turns for this task only (reset at `task_start`) |
| `input_tokens` | u64 | Input tokens for this task only |
| `output_tokens` | u64 | Output tokens for this task only |
| `tool_calls` | u32 | Tool calls for this task only |
| `shell_calls` | u32 | Shell calls for this task only |

Written unconditionally after `run_agent_loop`, even on error exit (exit_status will be `"failed"`). Always follows the corresponding `session_end`.

**Guarantees:**

- `trace.jsonl` exists after any capsule session, regardless of exit cause.
- For single-task (ephemeral) sessions: `session_start` is the first event and `session_end` is the last.
- For multi-task (persistent) sessions: `task_start` precedes `session_start` for each task; `task_end` follows `session_end` for each task. Each task produces exactly one `task_start`/`task_end` pair.
- `session_id` is identical on every line and matches `StagedSession.session_id`.
- Count fields in the last `session_end` equal the cumulative per-event totals across all tasks in the session.
- For multi-task sessions, the last `session_end` total fields equal the sum of all `task_end` per-task fields.
- Field names are snake_case translations of the hook WIT kebab-case field names (e.g. `input-tokens` → `input_tokens`).

**Non-obvious behaviour:**

- The trace is written by direct file append; it does **not** route through the `murmur:hook/lifecycle` WIT interface. A capsule that declares no hook artifacts still produces `trace.jsonl`.
- Compaction trace write errors are non-fatal (logged to `bootstrap.log`) because `try_compact_messages` itself is non-fatal. All other trace write errors surface as `RuntimeError::AgentLoopFailed`.
- If `run_agent_loop` returns `Err` before `session_start` is written (e.g. driver artifact missing), `trace.jsonl` is created but empty. `session_end` is not written because no session started.

---

## `mur eval`

Read and analyze `eval.jsonl` files produced by `murmur-hook-eval`, or drive a capsule against a dataset. These commands are read-only except for `mur eval run`, which launches real capsule sessions. They do not require a running registry (unless the capsule needs to pull artifacts).

See [Structured evaluation (`eval.jsonl`)](#structured-evaluation-evaljsonl) below for the file format.

### `mur eval show`

Print a human-readable summary of a single session's scored events, or emit a JSON object for programmatic use.

```bash
mur eval show <eval.jsonl> [--json]
```

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

Compare two `eval.jsonl` files side by side with a delta column.

```bash
mur eval diff <a.jsonl> <b.jsonl>
```

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

### Structured evaluation (`eval.jsonl`) schema { #structured-evaluation-evaljsonl }

When `murmur-hook-eval` is declared in the capsule manifest with at least one scorer configured, the hook writes `workdir/<session_id>/eval.jsonl` at `session_end`. The file is **not** written by the runtime itself — it is written by the hook component. `trace.jsonl` is always written by the runtime; `eval.jsonl` is only written when `murmur-hook-eval` is active and has at least one scorer. The two files are siblings in the same session workdir and share the same session scope.

**Format:** one JSON object per line (JSONL). Two record types, distinguished by `record_type`.

**Per-event score** (`record_type = "event_score"`) — one line per scorer:

| Field | Type | Notes |
|---|---|---|
| `record_type` | `"event_score"` | discriminator |
| `ts` | u64 | Unix milliseconds |
| `turn` | u32 | Turn count at the time of scoring |
| `event_type` | string | Lifecycle event that triggered the score (e.g. `"session_end"`) |
| `scorer` | string | Scorer name from manifest |
| `result` | `"pass"` \| `"fail"` | Binary outcome |
| `score` | f64 | `1.0` = pass, `0.0` = fail |
| `reason` | string | Human-readable explanation (e.g. `"turns=3 max=5"`) |

**Dataset run summary** (`record_type = "dataset_run"`) — one line per session, always last:

| Field | Type | Notes |
|---|---|---|
| `record_type` | `"dataset_run"` | discriminator |
| `ts` | u64 | Unix milliseconds |
| `dataset_id` | string \| null | From `observability.eval.dataset_id` |
| `case_id` | string \| null | From `MURMUR_CASE_ID` (set by `mur eval run`) |
| `overall` | `"pass"` \| `"fail"` \| `"no_scores"` | `fail` if any scorer fails; `no_scores` if no scores were emitted |
| `scores` | object | Map of scorer name → float score |

Example:

```jsonl
{"record_type":"event_score","ts":1778161473790,"turn":2,"event_type":"session_end","scorer":"turn_limit","result":"pass","score":1.0,"reason":"turns=2 max=5"}
{"record_type":"dataset_run","ts":1778161473790,"dataset_id":"my-ds","case_id":"case_001","overall":"pass","scores":{"turn_limit":1.0,"success_check":1.0}}
```

**Scorer types:**

| Type | Passes when |
|---|---|
| `exit_ok` | `exit_status == "ok"` |
| `max_turns` | `total_turns <= max` |
| `max_tokens` | `total_input_tokens + total_output_tokens <= max` |
| `tool_sequence` | `expected` list is a subsequence of observed tool calls |
| `llm_judge` | *deferred* — recognized but logs a warning and emits no score |

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

**Default behaviour (no `--registry`):** fetches the public artifact index from the configured URL (default: the Murmur default-artifacts repository). Override the URL with `registry.index_url` in the effective (global + project-level, merged) config — see [Artifact index and custom registry URL](#artifact-index-and-custom-registry-url) and [Configuration files](#configuration-files).

**Output format:**

```text
NAME                     VERSION  RUNTIME  DESCRIPTION
murmur-tool-git          0.3.31   tool     Structured git interface for Murmur capsules.
murmur-driver-anthropic  0.3.32   driver   Anthropic Messages API inference driver for Murmur agent capsules.
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
[Configuration files](#configuration-files) — so a `beta.enabled` flag set in either
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
See [Configuration files](#configuration-files) below.

---

## `mur config`

Read and write individual keys in the CLI config files described in
[Configuration files](#configuration-files) below.

```bash
mur config set <key> <value> [-g|--global]
```

### `mur config set <key> <value>`

Writes `<key>` to the project-level file at `<cwd>/.murmur/config.yaml` by default. Pass
`-g`/`--global` to write `~/.murmur/config.yaml` instead.

Exactly six dotted keys are settable:

| Key | Maps to |
|---|---|
| `registry.default` | `registry.default` |
| `registry.index_url` | `registry.index_url` |
| `inference.provider` | `inference.provider` |
| `inference.model` | `inference.model` |
| `inference.api_key` | `inference.api_key` |
| `inference.endpoint` | `inference.endpoint` |

`registry.sources` and `beta.enabled` are list-typed and **not** settable with `config set` —
edit `registry.sources` by hand in the YAML file, and use
[`mur beta enable`/`mur beta disable`](#mur-beta-enable-feature) for `beta.enabled`.

Setting a key never clobbers other keys already present in the target file:

```bash
mur config set registry.default local
# Set registry.default in ./.murmur/config.yaml

mur config set inference.model claude-haiku-4-5-20251001 -g
# Set inference.model in ~/.murmur/config.yaml
```

Any other dotted key — known `MurConfig` field or not — is rejected with `E-CFG-002` and writes
nothing:

```bash
mur config set nonsense.field value
# error[E-CFG-002]: unsupported config key 'nonsense.field'
#   hint: supported keys: registry.default, registry.index_url, inference.provider, inference.model, inference.api_key, inference.endpoint
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
    [`inference.api_key` is always global](#inferenceapi_key-is-always-global) below.

---

## Configuration files

Murmur CLI configuration lives in up to two YAML files, resolved into one *effective* config:

| File | Scope | Discovery |
|---|---|---|
| `~/.murmur/config.yaml` | Global (per-user) | Fixed path |
| `<cwd>/.murmur/config.yaml` | Project (per-workspace) | `<cwd>` only — unlike project-root manifest discovery elsewhere in the CLI, this does **not** walk up to parent directories |

Both files are optional; a missing file is treated as empty. Write them with
[`mur config set`](#mur-config) (project by default, `-g` for global) or edit the YAML by hand.

### Merge rules

Where both files set a value, the effective config is built per field:

| Field | Rule |
|---|---|
| `registry.default` | Project wins if non-empty, else global, else the built-in default (`official`) |
| `registry.index_url` | Same non-empty-wins rule |
| `inference.provider` / `inference.model` / `inference.endpoint` | Same non-empty-wins rule |
| `inference.api_key` | **Always** the global value — see below |
| `registry.sources` | Union by `name`: a project entry sharing a global entry's name replaces it in place (position preserved); a project entry with a new name is appended; global-only entries are never dropped |
| `beta.enabled` | Union by value: global flags first, then any project-only flags appended, in the project file's order |

If the project file does not exist, the effective config is exactly the global config (with
built-in defaults applied where global is also empty).

### `inference:` section

Configures the inference provider used by `mur new`. When present and complete in the global
file, it takes priority over the `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` env vars and the
interactive wizard. (`mur new` reads `~/.murmur/config.yaml` directly, not the merged effective
config — see the note under [`mur new`](#mur-new).)

```yaml
inference:
  provider: anthropic              # "anthropic" or "openai"
  model: claude-haiku-4-5-20251001
  api_key: sk-ant-...
  endpoint: ""                     # optional; leave empty for the provider default
```

| Key | Required | Description |
|---|---|---|
| `provider` | yes | `"anthropic"` or `"openai"` |
| `model` | yes | Model name passed to the inference driver |
| `api_key` | yes | API key for the provider |
| `endpoint` | no | Override the base URL (e.g. for a proxy); empty = provider default |

**Provider defaults:**

| Provider | Default endpoint | Default model (wizard) |
|---|---|---|
| `anthropic` | `https://api.anthropic.com` | `claude-haiku-4-5-20251001` |
| `openai` | `https://api.openai.com` | `gpt-4o-mini` |

**Env var override behavior:** `inference:` in the global file is checked first; if absent or
incomplete, the env vars `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are checked (in that order); if
neither is set, the interactive wizard fires. To disable the env var fallback, set `inference:`
in the global config file.

#### `inference.api_key` is always global

`inference.api_key` is the one field that does not follow "project wins": the effective config
reads it from `~/.murmur/config.yaml` only, regardless of what the project file contains — even
if the project value is a `${VAR}` reference. If the global file has no `inference:` block at
all, the effective `api_key` is `""`.

A **literal** (non-`${VAR}`) `inference.api_key` in the project file triggers a warning — both
when the effective config is loaded and when `mur config set` writes it (see
[`mur config set`](#mur-config-set-key-value) above):

```text
warning: <cwd>/.murmur/config.yaml sets inference.api_key to a literal value, but inference.api_key is always read from the global config (~/.murmur/config.yaml); this project-level value will be ignored
```

A `${VAR}`-shaped value (e.g. `${MY_ORG_KEY}`) prints no warning on either path — it's simply
never consulted, same as any other project-level `inference.api_key`.

### `registry:` section

See [Artifact index and custom registry URL](#artifact-index-and-custom-registry-url) below for
`registry.index_url`, and [Multiple sources and fallthrough](../concepts/registry.md#multiple-sources-and-fallthrough) for
`registry.sources`.

### `beta:` section

Stores the list of beta features opted into via `mur beta enable`/`mur beta disable`. Those
commands always read and write the global file only (no `-g`/project flag). `beta.enabled` is
not a settable `mur config set` key (see [`mur config set`](#mur-config-set-key-value)) — a
project-level `beta.enabled` entry can only be added by hand-editing
`<cwd>/.murmur/config.yaml`.

```yaml
beta:
  enabled:
    - blueprint
    - dag-topology
```

| Key | Type | Description |
|---|---|---|
| `enabled` | array of strings | Feature names opted into |

An absent `beta:` section is equivalent to `enabled: []`. Features listed here but not compiled
into the current binary are silently ignored — they take effect once a binary that includes them
is installed.

### Where the effective (merged) config is used

The merged global+project config is read by:

- Beta feature gating (which subcommands `mur --help` shows, and `mur beta list`'s enabled column)
- `mur install`'s registry source-chain resolution
- `mur search`'s `registry.index_url` resolution

`mur new` and `mur deploy` — both beta-gated — still read `~/.murmur/config.yaml` only; this is a
deliberate scope limit, not an oversight.

---

## Artifact index and custom registry URL

`mur search` fetches a static JSON catalog called `artifacts-index.json` hosted at a canonical public URL. The default URL points to the Murmur default-artifacts repository. To use a different URL (for example a private org catalog), add this to `~/.murmur/config.yaml` — or `<cwd>/.murmur/config.yaml` to scope it to one project — or set it with `mur config set registry.index_url <url>`:

```yaml
registry:
  index_url: https://my-org.example.com/artifacts-index.json
```

The `registry.index_url` key overrides the default for all `mur search` invocations that do not pass an explicit `--registry` flag. See [Configuration files](#configuration-files) for how the project and global values merge.

**`artifacts-index.json` schema:**

```json
{
  "schema_version": "1",
  "updated_at": "2026-06-07T00:00:00Z",
  "artifacts": [
    {
      "name": "murmur-tool-git",
      "version": "0.3.31",
      "runtime": "tool",
      "description": "Structured git interface for Murmur capsules.",
      "tags": ["tool", "git"],
      "platforms": ["darwin-aarch64", "linux-aarch64", "linux-x86_64"]
    }
  ]
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `schema_version` | string | yes | Always `"1"` — future incompatible changes bump this |
| `updated_at` | string | yes | ISO 8601 UTC timestamp of last regeneration |
| `artifacts` | array | yes | One entry per published artifact |
| `name` | string | yes | Artifact name (matches `name:` in murmur.yaml) |
| `version` | string | yes | SemVer string |
| `runtime` | string | yes | `driver`, `hook`, `tool`, or `skill` |
| `description` | string | yes | Short description from murmur.yaml |
| `tags` | array[string] | no | Keyword tags; empty array if absent |
| `platforms` | array[string] | no | e.g. `darwin-aarch64`; empty for skill artifacts |

**Regenerating the index:** the index is regenerated by `scripts/apply-versions.sh` in the default-artifacts repository whenever artifact versions are bumped. Run that script from the default-artifacts repo root to update `artifacts-index.json` in place.

---

## Registry selection rules

Resolution order in CLI:

1. `--registry <url>` flag
2. `murmur.yaml` `registry.remote_url`
3. `murmur.yaml` `registry.default` (`local` or `remote`)
4. fallback local mode

Remote mode requires:

- `NEXUS_API_KEY` environment variable

---

## Error codes

| Code | Meaning |
|---|---|
| `E-CFG-002` | `mur config set` given an unsupported dotted key |
| `E-MAN-001` | Missing required manifest field |
| `E-MAN-002` | YAML syntax error in manifest |
| `E-MAN-003` | Field type mismatch in manifest |
| `E-BLD-001` | Manifest `name:` is not a valid artifact identifier ([build lints](build-lints.md#e-bld-001)) |
| `E-BLD-002` | `requires_files:` entry is unsafe (absolute, `..`, symlink) or collides inside the archive ([build lints](build-lints.md#e-bld-002)) |
| `E-BLD-003` | Packed entry set is not a launchable wasm payload ([build lints](build-lints.md#e-bld-003)) |
| `E-RUN-001` | Capsule trap, compile failure, execution deadline exceeded (`capabilities.limits.deadline_seconds`), or resource limit exceeded (`capabilities.limits.memory_bytes`/`table_elements`) |
| `E-RUN-002` | Missing WASI import (linker error) |
| `E-RUN-003` | Lock version mismatch or missing lock entry |
| `E-RUN-004` | Capsule WASM not found at expected path |
| `E-RUN-005` | Inference driver not configured in manifest |
| `E-RUN-006` | Inference driver artifact not installed |
| `E-RUN-007` | Agent loop failed at runtime |
| `E-RUN-008` | Required artifact not installed locally |
| `E-IO-001` | File or directory not found |
| `E-IO-003` | General I/O error (read/write failure) |
| `E-DEPLOY-003` | SSH connection or remote command failed |
| `E-DEPLOY-004` | Capsule did not emit startup JSON within 30s |
| `E-TRC-001` | Trace file parse error (malformed JSON, missing required `session_start`/`session_end`, empty file); unknown event types are silently skipped |
| `E-EVAL-001` | Eval file parse error (malformed JSON, unknown `record_type`, missing required field); message includes `:line:` number |
| `E-TOP-001` | Tempo endpoint unreachable, or invalid `--window` format |
| `E-TOP-002` | Tempo HTTP query failed (search or trace fetch) |
| `E-TOP-003` | Tempo response JSON parse failure |
