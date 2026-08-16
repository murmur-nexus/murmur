# Default Artifacts

Murmur publishes the artifacts below. Declare one in `artifacts:` and install it with
[`mur install`](installing-artifacts.md) — none of them has to be built locally.
[`mur search`](cli.md#mur-search) reads the same index from the terminal.

This page also documents the environment the runtime hands hook and driver components.

---

## Inference drivers

An inference driver turns the runtime's provider-agnostic request into one provider's wire format.
Declare one with `runtime: driver` and name it in
[`inference.driver.artifact`](manifest.md#field-inference).

| Artifact | Provider |
|---|---|
| `murmur-driver-anthropic` | Anthropic Messages API |
| `murmur-driver-openai` | OpenAI Chat Completions, and the Responses API for `gpt-5` and later models |
| `murmur-driver-deepseek` | DeepSeek |

Every driver reads the same [inference environment](#driver-environment) and passes
[`inference.max_tokens`](manifest.md#inference-max-tokens) through unchanged as the provider's
output cap.

---

## Hooks

A hook's binding, execution mode and commit policy are fixed by the artifact, not by the entry that
declares it — see [Hook artifacts](manifest.md#hook-artifacts). A hook with no `capabilities:`
block on its manifest entry gets no directory and no network at all; the ones that write a file
need a [`filesystem` grant](manifest.md#hook-capabilities).

| Artifact | Fires on | What it does |
|---|---|---|
| `murmur-hook-compact` | `on-compaction` | Summarizes conversation history when the session token threshold is reached, and replaces the context with the summary |
| `murmur-hook-debug` | Every event | Appends one JSON object per lifecycle event to `hook-debug.jsonl` |
| `murmur-hook-diff-summary` | Every event | Snapshots files before each editor tool call and emits a unified-diff summary at the end of the turn |
| `murmur-hook-eval` | Every event | Scores the session against the configured scorers and writes `eval.jsonl` |
| `murmur-hook-grafana` | Every event | Emits OpenTelemetry spans for each lifecycle event to a Grafana Tempo OTLP/HTTP endpoint |
| `murmur-hook-memory-jsonl` | Every event | Appends each turn to a JSONL file, reloads prior turns at task start, and writes a close-out marker at task end |
| `murmur-hook-regression-verifier` | Every event | Watches a task's test runs and reopens the task when a change broke previously-passing tests |
| `murmur-hook-shell-desc` | `on-stage` | Returns enriched tool manifests for common shell binaries at staging time |

### `murmur-hook-debug`

The quickest way to see what the runtime dispatched during a session. It is the only shipped hook
that runs `execution_mode: async`, so the agent loop never waits on it.

```yaml
artifacts:
  - name: murmur-hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook
    capabilities:
      filesystem:
        scope: .
```

`scope: .` mounts the accessible workdir as the hook's current directory, which is where
`hook-debug.jsonl` lands.

### `murmur-hook-grafana`

```yaml
artifacts:
  - name: murmur-hook-grafana
    version: "{{ v.murmur_hook_grafana }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

Without `observability.otel_endpoint` the hook logs a warning to
`workdir/logs/hook-murmur-hook-grafana.log` and exports nothing for that session. The session runs
as normal.

### `murmur-hook-eval`

```yaml
artifacts:
  - name: murmur-hook-eval
    version: "{{ v.murmur_hook_eval }}"
    runtime: hook

observability:
  eval:
    dataset_id: my-dataset
    scorers:
      - type: exit_ok
        name: success_check
      - type: max_turns
        name: turn_limit
        max: 5
```

Without an `observability.eval` block the hook logs a warning to
`workdir/logs/hook-murmur-hook-eval.log` and scores nothing. The session runs as normal.

See [Scorer types](observability-schemas.md#structured-evaluation-evaljsonl) for what each scorer
measures and the shape of the file it writes, [`observability.eval`](manifest.md#field-observability)
for the fields each scorer takes, and [`mur eval`](cli.md#mur-eval) for reading and comparing eval
files.

---

## Tools

| Artifact | What it does |
|---|---|
| `murmur-tool-code-coverage` | Spectrum-based fault localization (Ochiai / Tarantula) over per-test LCOV reports the agent has already captured. Reads a repository indexed by `murmur-tool-code-graph` |
| `murmur-tool-code-graph` | Indexes a Rust or Python repository into a symbol/edge graph and answers structured queries over it. Symbols are addressed by a stable identity rather than by `file:line` |
| `murmur-tool-create` | Scaffolds a new tool artifact directory: `murmur.yaml`, a stub implementation and a README |
| `murmur-tool-editor` | Reads, writes, patches and searches files without a shell grant |
| `murmur-tool-git` | Structured git operations returning typed JSON, in place of `git` in `capabilities.shell.allow` |
| `murmur-tool-registry-search` | Searches the artifact index by keyword |
| `murmur-tool-request-input` | Pauses the agent loop and requests external input over A2A; the answer comes back as the tool result |
| `murmur-tool-test-report` | Parses a test-runner output file the agent has already captured into a structured list of failures |

### `murmur-tool-registry-search`

Returns ranked matches with name, version, runtime, description and publication date, so a capsule
can discover artifacts mid-session. It takes three inputs:

| Input | Required | Notes |
|---|---:|---|
| `query` | yes | Matched case-insensitively against artifact name, description and tags |
| `registry` | no | Omit for the public index; `local` for the local artifact store; a URL or absolute path for a custom registry |
| `limit` | no | Maximum results to return. Default: 10 |

The public index is the same `artifacts-index.json` that [`mur search`](cli.md#mur-search) reads.
[`mur new`](cli.md#mur-new) declares this tool in the generator capsule it cold-boots, and fails
with `E-RUN-008` if it is not installed first.

---

## Skills

| Artifact | What it does |
|---|---|
| `murmur-skill-create-manifest` | How to write a valid `murmur.yaml`. `mur new` loads it into its generator capsule |
| `murmur-skill-investigation-checkpoint` | The `checkpoints/decisions.json` convention for recording and reusing investigative verdicts across a session |

---

## Hook environment { #hook-environment }

The runtime injects these into every hook component. They are the whole environment a hook sees —
nothing from the host is inherited.

| Env var | Injected when | Value |
|---|---|---|
| `MURMUR_OTEL_ENDPOINT` | `observability.otel_endpoint` is set | The configured OTLP endpoint URL |
| `MURMUR_EVAL_CONFIG` | `observability.eval` is set | The eval block as JSON: `dataset_id` and the parsed `scorers` list |
| `MURMUR_DATASET_ID` | `mur eval run` is driving a dataset and `observability.eval.dataset_id` is set | `observability.eval.dataset_id` |
| `MURMUR_CASE_ID` | `mur eval run` is driving a dataset | The `case_id` of the case being run |
| `MURMUR_FORMATION_ID` | `MURMUR_FORMATION_ID` is set in the host environment | Forwarded unchanged |

## Driver and tool environment { #driver-environment }

When the manifest configures `inference:`, the runtime injects these into the driver component and
into every tool artifact.

| Env var | Value | Under `transport: process` |
|---|---|---|
| `MURMUR_INFERENCE_TRANSPORT` | `inference.transport` | `process` |
| `MURMUR_INFERENCE_MODEL` | `inference.model` | The configured model, empty when `inference.model` is omitted |
| `MURMUR_INFERENCE_ENDPOINT` | `inference.endpoint` | Empty |
| `MURMUR_INFERENCE_DRIVER` | `inference.driver.artifact` | Empty |
| `MURMUR_INFERENCE_DRIVER_CONFIG` | `inference.driver.config` as JSON. Not set when the field is absent | Not set |
| `MURMUR_INFERENCE_API_KEY` | `inference.api_key`. Not set when the field is absent | Not set |
| `MURMUR_CAPSULE_NAME` | `name` from the manifest | Same |
| `MURMUR_CAPSULE_VERSION` | `version` from the manifest | Same |
| `MURMUR_SESSION_ID` | The session ID | Same |
| `MURMUR_CAPSULE_URL` | `localhost:<port>`, the address the capsule's HTTP server bound | Same |

`transport: process` loads no driver component: `inference.endpoint`, `inference.driver` and
`inference.api_key` are rejected in the manifest, and the agent loop spawns `inference.command`
instead. Tool artifacts still receive the whole table.
