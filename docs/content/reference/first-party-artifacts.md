# First-party Artifacts

Murmur publishes a set of artifacts you can declare in `artifacts:` without building
anything yourself. This page documents them and the environment the runtime hands hook
and driver components.

---

## Inference drivers

An inference driver is the component that turns the runtime's provider-agnostic request into one
provider's wire format. Declare one with `runtime: driver` and name it in
[`inference.driver.artifact`](manifest.md#field-inference).

| Artifact | Provider |
|---|---|
| `murmur-driver-anthropic` | Anthropic Messages API |
| `murmur-driver-openai` | OpenAI |
| `murmur-driver-deepseek` | DeepSeek |

Every driver receives the same [`MURMUR_INFERENCE_*` environment](#inference-config-and-the-driver)
and forwards [`inference.max_tokens`](manifest.md#inference-max-tokens) verbatim to its provider.

## `murmur-hook-debug`

Appends one JSON object per lifecycle event to `hook-debug.jsonl`, which makes it the quickest way
to see what the runtime actually dispatched during a session. It is the only shipped hook that runs
`async`, and it is stateless — nothing in the agent loop waits on it.

```yaml
artifacts:
  - name: murmur-hook-debug
    version: "{{ v.murmur_hook_debug }}"
    runtime: hook
    capabilities:
      filesystem:
        scope: .
```

The `filesystem` grant is what lets it write: a hook with no `capabilities:` block gets no
directory at all. See [Hook capabilities](manifest.md#hook-capabilities).

## `murmur-tool-registry-search`

Searches the public artifact index — the same `artifacts-index.json` that
[`mur search`](cli.md#mur-search) reads — and returns matches as a tool result, so a capsule can
discover artifacts mid-session. [`mur new`](cli.md#mur-new) declares it in the short-lived
generator capsule it cold-boots, which is why it has to be installed before `mur new` runs.

## `murmur-hook-grafana`

The hook artifact targeting Grafana Tempo:

```yaml
artifacts:
  - name: murmur-hook-grafana
    version: "{{ v.murmur_hook_grafana }}"
    runtime: hook

observability:
  otel_endpoint: "http://localhost:4318"
```

When `MURMUR_OTEL_ENDPOINT` is absent or empty, `murmur-hook-grafana` logs a warning to `logs/hook-murmur-hook-grafana.log` and becomes a no-op for that session — it does not crash.

## `murmur-hook-eval`

The hook that scores sessions and writes `eval.jsonl`:

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

| Scorer type | Passes when | Extra fields |
|---|---|---|
| `exit_ok` | `exit_status == "ok"` | — |
| `max_turns` | `total_turns <= max` | `max: <integer>` |
| `max_tokens` | `total_input_tokens + total_output_tokens <= max` | `max: <integer>` |
| `tool_sequence` | `expected` is a subsequence of observed tool calls | `expected: [tool1, tool2, ...]` |
| `llm_judge` | recognized but not implemented — logs a warning, emits no score | — |

When `MURMUR_EVAL_CONFIG` is absent (i.e. no `observability.eval` in the manifest), the hook logs a warning to `logs/hook-murmur-hook-eval.log` and becomes a no-op. The session proceeds normally.

See [Structured evaluation (`eval.jsonl`)](../reference/observability-schemas.md#structured-evaluation-evaljsonl) for the output file format and [mur eval](../reference/cli.md#mur-eval) for how to read and compare eval files.

---

## Hook WASI environment variables

The runtime injects the following env vars into every hook component's WASI context:

| Env var | Injected when | Value |
|---|---|---|
| `MURMUR_OTEL_ENDPOINT` | `observability.otel_endpoint` is set | The configured OTLP endpoint URL |
| `MURMUR_FORMATION_ID` | `MURMUR_FORMATION_ID` is set on the host | Forwarded from host env |
| `MURMUR_EVAL_CONFIG` | `observability.eval.scorers` is non-empty | JSON-serialized `EvalConfig`; consumed by `murmur-hook-eval` |
| `MURMUR_CASE_ID` | `mur eval run` is driving a dataset case | The `case_id` field from the dataset line |
| `MURMUR_DATASET_ID` | `observability.eval.dataset_id` is set, or `mur eval run` is active | The configured or overridden dataset ID |

Custom hooks can read any of these variables from their WASI environment at `on_session_start`.

## Inference config and the driver

When `inference` is configured with `transport: http`, the runtime passes the following values to the WASM driver component via WASI environment variables:

| Env var | Source field | `transport: process` |
|---|---|---|
| `MURMUR_INFERENCE_TRANSPORT` | `inference.transport` | `"process"` |
| `MURMUR_INFERENCE_ENDPOINT` | `inference.endpoint` | `""` (empty) |
| `MURMUR_INFERENCE_MODEL` | `inference.model` | set |
| `MURMUR_INFERENCE_DRIVER` | `inference.driver.artifact` | `""` (empty) |
| `MURMUR_INFERENCE_DRIVER_CONFIG` | `inference.driver.config` as JSON | not set |
| `MURMUR_INFERENCE_API_KEY` | `inference.api_key` (only when set) | not set |

For `transport: process`, no WASM driver component is loaded. The env vars above are still injected into any hook components, but `MURMUR_INFERENCE_DRIVER` and `MURMUR_INFERENCE_ENDPOINT` will be empty strings.
