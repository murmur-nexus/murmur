# How to score your agent's behavior with eval

Murmur's eval system lets you run your capsule against a dataset of test cases and automatically score each session. Scores are written to `eval.jsonl` alongside `trace.jsonl` in each session workdir and can be compared across runs with `mur eval diff`. This guide shows how to wire up `murmur-hook-eval`, write a dataset, run it, and interpret the results.

The relevant manifest options are:

| Option | Controls |
|---|---|
| [observability.eval.scorers](../reference/manifest-schema.md#field-observability) | List of scorer configurations; at least one required to activate the hook |
| [observability.eval.dataset_id](../reference/manifest-schema.md#field-observability) | Label applied to every `dataset_run` record in `eval.jsonl` |

---

## Step 1 — declare murmur-hook-eval in the manifest

`murmur-hook-eval` is a hook artifact — it runs at fixed lifecycle points and writes scores. Create a `murmur.yaml` file with `runtime: hook` and configure scorers under `observability.eval`:

=== "Anthropic"

    ```yaml
    name: my-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-anthropic
        version: "{{ v.murmur_driver_anthropic }}"
        runtime: driver
      - name: murmur-hook-eval
        version: "{{ v.murmur_hook_eval }}"
        runtime: hook
        capabilities:
          filesystem:
            scope: .          # the hook writes the session's eval.jsonl scores here; hooks get no filesystem by default
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.anthropic.com
      filesystem:
        allow:
          - .

    inference:
      transport: http
      endpoint: https://api.anthropic.com
      model: {{ v.model_anthropic }}
      api_key: ${ANTHROPIC_API_KEY}
      driver:
        artifact: murmur-driver-anthropic

    observability:
      eval:
        dataset_id: my-dataset
        scorers:
          - type: exit_ok
            name: success_check
          - type: max_turns
            name: turn_limit
            max: 5
          - type: max_tokens
            name: response_size
            max: 3000
          - type: tool_sequence
            name: tool_order
            expected: [murmur-tool-editor]
    ```

=== "OpenAI"

    ```yaml
    name: my-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-openai
        version: "{{ v.murmur_driver_openai }}"
        runtime: driver
      - name: murmur-hook-eval
        version: "{{ v.murmur_hook_eval }}"
        runtime: hook
        capabilities:
          filesystem:
            scope: .          # the hook writes the session's eval.jsonl scores here; hooks get no filesystem by default
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.openai.com
      filesystem:
        allow:
          - .

    inference:
      transport: http
      endpoint: https://api.openai.com
      model: {{ v.model_openai }}
      api_key: ${OPENAI_API_KEY}
      driver:
        artifact: murmur-driver-openai

    observability:
      eval:
        dataset_id: my-dataset
        scorers:
          - type: exit_ok
            name: success_check
          - type: max_turns
            name: turn_limit
            max: 5
          - type: max_tokens
            name: response_size
            max: 3000
          - type: tool_sequence
            name: tool_order
            expected: [murmur-tool-editor]
    ```

=== "DeepSeek"

    ```yaml
    name: my-agent
    version: "0.1.0"

    artifacts:
      - name: murmur-driver-deepseek
        version: "{{ v.murmur_driver_deepseek }}"
        runtime: driver
      - name: murmur-hook-eval
        version: "{{ v.murmur_hook_eval }}"
        runtime: hook
        capabilities:
          filesystem:
            scope: .          # the hook writes the session's eval.jsonl scores here; hooks get no filesystem by default
      - name: murmur-tool-editor
        version: "{{ v.murmur_tool_editor }}"
        runtime: tool

    capabilities:
      network:
        allow:
          - https://api.deepseek.com
      filesystem:
        allow:
          - .

    inference:
      transport: http
      endpoint: https://api.deepseek.com
      model: {{ v.model_deepseek }}
      api_key: ${DEEPSEEK_API_KEY}
      driver:
        artifact: murmur-driver-deepseek

    observability:
      eval:
        dataset_id: my-dataset
        scorers:
          - type: exit_ok
            name: success_check
          - type: max_turns
            name: turn_limit
            max: 5
          - type: max_tokens
            name: response_size
            max: 3000
          - type: tool_sequence
            name: tool_order
            expected: [murmur-tool-editor]
    ```

**What each scorer does:**

| Scorer type | Passes when |
|---|---|
| `exit_ok` | The session exited with `status: ok` (not `failed` or `max_turns_reached`) |
| `max_turns` | The session used at most `max` inference turns |
| `max_tokens` | Total input + output tokens did not exceed `max` |
| `tool_sequence` | The `expected` list is a subsequence of the observed tool calls (same order, gaps allowed) |

The `name` field for each scorer is the key that appears in `eval.jsonl` score records.

---

## Step 2 — install dependencies

```bash
mur install
```

--8<-- "includes/mur-pull-info.md"

---

## Step 3 — write a dataset

A dataset is a JSONL file — one JSON object per line, one line per test case. The conventional location is `eval.jsonl` in the project directory — the same folder as `murmur.yaml`, where `mur eval run` looks by default.

Create one task file in the project root:

`task.md`:
```text
Read MURMUR.md, then draft a one-sentence summary of the Murmur project. Reply with only the final sentence.
```

The task starts with a tool call — the agent reads `MURMUR.md` (the capsule environment reference injected into every workdir) before answering. This is important: `tool_sequence` applies to every case in the dataset, so every case must call the tool. Grounding the task in a file read makes that natural.

The same task runs for all four cases — what differs is the `case_id`, which names the scorer each case is primarily meant to verify.

Then create `eval.jsonl`:

```json
{ "case_id": "exit-check",  "task_path": "task.md" }
{ "case_id": "turns-check", "task_path": "task.md" }
{ "case_id": "size-check",  "task_path": "task.md" }
{ "case_id": "tool-check",  "task_path": "task.md" }
```

Each `task_path` points to a file that is copied into the capsule workdir as `task.md` before the session starts. `exit-check` verifies `success_check` (clean exit); `turns-check` verifies `turn_limit` (read + answer takes two inference turns, well within five); `size-check` verifies `response_size` (MURMUR.md content adds roughly 1200 tokens to the second turn, bringing the total to ~2450, within the 3000-token budget); `tool-check` verifies `tool_order` (the editor was called before the answer).

The `case_id` is injected into the hook environment as `MURMUR_CASE_ID` and appears in every `dataset_run` record — it's how you identify each case in the output.

---

## Step 4 — run the dataset

Run from the directory containing `murmur.yaml`:

```bash
mur eval run
```

Both arguments default: capsule to the current directory and dataset to `eval.jsonl`. To override either:

```bash
mur eval run ./other-capsule                                # explicit capsule
mur eval run --dataset ./other.jsonl                        # explicit dataset
mur eval run ./other-capsule --dataset ./other.jsonl        # both explicit
```

`mur eval run` runs one capsule session per case, sequentially. For each case it:

1. Stages the session — allocates a per-session workdir (`workdir/ses_xxx/`) and loads artifacts
2. Copies `task_path` into the session workdir as `task.md`
3. Launches the session
4. Reads `eval.jsonl` from the session workdir (written by the hook at session end)
5. Prints a result line

Output as cases complete:

```text
Running 4 case(s) …
  case: exit-check
    result: pass  session: ses_6801f81dd28b4a9daf434e8324c4793e
  case: turns-check
    result: pass  session: ses_7902a92ee39c5baebf545f9435d5804f
  case: size-check
    result: pass  session: ses_9b14c03df47e6a1c84523f7896d5920f
  case: tool-check
    result: pass  session: ses_ab25d04ef58f7643bce9fbd67da681c3
```

After all cases:

```text
── Summary ──────────────────────────────────────
pass: 4/4

  exit-check    pass  response_size=1.00 success_check=1.00 tool_order=1.00 turn_limit=1.00  (/path/to/workdir/ses_6801...)
  turns-check   pass  response_size=1.00 success_check=1.00 tool_order=1.00 turn_limit=1.00  (/path/to/workdir/ses_7902...)
  size-check    pass  response_size=1.00 success_check=1.00 tool_order=1.00 turn_limit=1.00  (/path/to/workdir/ses_9b14...)
  tool-check    pass  response_size=1.00 success_check=1.00 tool_order=1.00 turn_limit=1.00  (/path/to/workdir/ses_ab25...)
```

All four cases pass every scorer. Reading MURMUR.md takes one inference turn (the tool call) and answering takes a second, so `turn_limit` sees two turns. The tool result injects MURMUR.md's content into the second turn's context, which pushes total tokens to around 2450 — that's why `max_tokens` is set to 3000 rather than something tighter.

---

## Step 5 — read the eval output

Each case produces its own `eval.jsonl` in the session workdir. Inspect the most recent session, or any session by suffix:

```bash
mur eval show               # most recent session
mur eval show 6801          # session whose ID ends in "6801"
```

Output:

```text
── Eval: workdir/ses_6801.../eval.jsonl ─────────────────────────────────────

── Scorers ──────────────────────────────────────
  response_size            1/1 pass  (100.0%)
  success_check            1/1 pass  (100.0%)
  tool_order               1/1 pass  (100.0%)
  turn_limit               1/1 pass  (100.0%)

── Overall ──────────────────────────────────────
  result:  pass
  case:    exit-check
  dataset: my-dataset

── Score summary ────────────────────────────────
  response_size            1.0000
  success_check            1.0000
  tool_order               1.0000
  turn_limit               1.0000

── Worst events ─────────────────────────────────
  (no failing events)
```

The **Worst events** section lists the individual scoring events that failed — useful for diagnosing which specific turn or tool call tripped a scorer. Here it's empty because all four scorers passed.

For programmatic use:

```bash
mur eval show --json
```

```json
{
  "overall": "pass",
  "scorers": {
    "response_size": { "pass": 1, "fail": 0, "total": 1, "pass_rate": 1.0 },
    "success_check": { "pass": 1, "fail": 0, "total": 1, "pass_rate": 1.0 },
    "turn_limit": { "pass": 1, "fail": 0, "total": 1, "pass_rate": 1.0 }
  },
  "dataset_run": { "overall": "pass", "scores": { "response_size": 1.0, "success_check": 1.0, "turn_limit": 1.0 } }
}
```

---

## Step 6 — compare two eval runs

After re-running the dataset (for example, after changing the system prompt or swapping the model), compare the new results against a baseline to check for regressions:

```bash
mur eval diff 21f7 93a8
```

Output when both runs pass all scorers:

```text
Scorer                   Run A          Run B          Delta
──────────────────────── ────────────── ────────────── ──────────────────────────
response_size            100.0%         100.0%         =
success_check            100.0%         100.0%         =
tool_order               100.0%         100.0%         =
turn_limit               100.0%         100.0%         =

overall                  pass           pass
```

If a scorer regressed — for example `response_size` dropped from 100% to 0% after a prompt change — the delta would show `-100.0pp (A better)` and `overall` would change to `fail`. That is the signal to investigate.

---

## Step 7 — interpret a failing scorer

Each failing scorer tells you something specific:

| Scorer fails | What it usually means |
|---|---|
| `exit_ok` | The session errored out or hit the turn cap — check the trace for the exit status |
| `max_turns` | The agent needed more turns than the budget allows — consider raising `max` or tightening the system prompt |
| `max_tokens` | The session used more tokens than the budget — the agent is producing verbose output; try a more constrained prompt or a lower-verbosity model |
| `tool_sequence` | The agent called tools in the wrong order or skipped a required tool — check the inference events in the trace |

When a scorer fails unexpectedly, open the companion trace for the same session:

```bash
mur trace show
```

--8<-- "includes/mur-trace-show-info.md"

--8<-- "includes/mur-trace-explore.md"

---

## Summary

| Step | What you do |
|---|---|
| Declare `murmur-hook-eval` with `runtime: hook` | Activates the eval hook for this capsule |
| Grant it `capabilities.filesystem.scope` on its own entry | Lets it write `eval.jsonl`; hooks have no filesystem access by default (see [Hook capabilities](../reference/manifest-schema.md#hook-capabilities)) |
| Declare `murmur-tool-editor` with `runtime: tool` | Gives the agent file-read capability; required for `tool_sequence` cases |
| Configure `observability.eval.scorers` | Defines the scoring criteria — all scorers run for every case |
| Write `eval.jsonl` | One case per line, each with a `case_id` and `task_path` |
| `mur eval run` | Runs all cases against `eval.jsonl` and prints a pass/fail summary |
| `mur eval show <eval.jsonl>` | Shows which scorers passed, which failed, and why |
| `mur eval diff <a> <b>` | Compares two runs to check for regressions |
