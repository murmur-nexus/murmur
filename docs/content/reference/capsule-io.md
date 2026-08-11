# Capsule I/O Schema

Capsules communicate with their runtime environment through structured files in the session workdir. This page documents the typed I/O envelope used by agent capsules.

---

## Overview

All structured capsule I/O uses a common JSON envelope:

```json
{
  "schema": "murmur.message.v1",
  "type": "<message-type>",
  "job_id": "<uuid-or-null>",
  "payload": { ... }
}
```

| Field | Type | Description |
|---|---|---|
| `schema` | string | Always `"murmur.message.v1"` |
| `type` | string | Message type identifier |
| `job_id` | string \| null | Job UUID assigned by mur-roost; `null` for direct `mur run` |
| `payload` | object | Type-specific payload (see below) |

---

## input.json — Task input

**Location:** `<session-workdir>/input.json`

**Type string:** `murmur.code_task.request.v1`

Written by `mur run` when `--input` or `--instructions` is passed. Also written by mur-roost when spawning a worker with a structured task.

```json
{
  "schema": "murmur.message.v1",
  "type": "murmur.code_task.request.v1",
  "job_id": null,
  "payload": {
    "objective": "Build a CLI tool in Python",
    "instructions": "Write only implementation files, no tests",
    "context": null,
    "output_format": null
  }
}
```

### Payload fields

| Field | Type | Description |
|---|---|---|
| `objective` | string | The task goal (maps from `--input`) |
| `instructions` | string \| null | Role or constraint guidance (maps from `--instructions`) |
| `context` | string \| null | Background context; currently only set programmatically |
| `output_format` | string \| null | Desired output format; currently unused (reserved) |

### Fallback chain

The agent runtime reads the task in this order:

1. `input.json` — parsed as `murmur.code_task.request.v1`; formats task as:
   ```
   Objective: <objective>

   Instructions: <instructions>   (omitted when null)

   Context: <context>             (omitted when null)
   ```
2. `task.md` — raw text fallback
3. `input.txt` — raw text fallback
4. Empty string — agent receives no task

If `input.json` exists but cannot be parsed, the runtime falls through to `task.md`.

---

## out/result.json — Task output

**Location:** `<session-workdir>/out/result.json`

**Type string:** `murmur.code_task.result.v1`

Written by the runtime at the end of a successful agent run (`stop_reason: end_turn` or `max_tokens`). **Not written on error paths** — those write only `out/result.txt`.

```json
{
  "schema": "murmur.message.v1",
  "type": "murmur.code_task.result.v1",
  "job_id": "job_3f8a1b2c...",
  "payload": {
    "status": null,
    "summary": null,
    "files": null,
    "output": "The CLI tool is complete. See output/ for generated files."
  }
}
```

### Payload fields

| Field | Type | Description |
|---|---|---|
| `status` | string \| null | `null` until the evaluator runs (planned); do not interpret yet |
| `summary` | string \| null | Short summary of what the agent did; currently always `null` |
| `files` | array \| null | List of output file paths; currently always `null` |
| `output` | string | Final agent output — same content as `out/result.txt` |

### When result.json is NOT written

| Stop reason | result.json written? |
|---|---|
| `end_turn` | ✅ yes |
| `max_tokens` | ✅ yes |
| `error` (driver-side failure) | ❌ no — only `result.txt` |
| Driver invocation failure | ❌ no — only `result.txt` |
| Unsupported stop reason | ❌ no — only `result.txt` |
| Missing tool-call blocks | ❌ no — only `result.txt` |
| Turn limit exhausted | ❌ no — only `result.txt` |

---

## meta/job_id.txt — Worker identity

**Location:** `<session-workdir>/meta/job_id.txt`

**Written by:** mur-roost at spawn time, before staging the worker session.

Contains the job UUID string assigned to this worker by mur-roost.

```
job_3f8a1b2c4d5e6f789012abcdef012345
```

The same UUID is available as:

- `meta/job_id.txt` — readable by the capsule as a workdir file
- `MURMUR_JOB_ID` environment variable — available to shell tools
- `result.json["job_id"]` — included in the structured output envelope

---

## MURMUR_JOB_ID environment variable

Injected into the worker's shell environment by mur-roost at spawn time.

```bash
# Available inside shell tool invocations
echo $MURMUR_JOB_ID
# → job_3f8a1b2c4d5e6f789012abcdef012345
```

Set alongside `MURMUR_ROOST_URL` in the capsule's `shell_baseline_env`. Only present in workers spawned by mur-roost — not set for top-level `mur run`.

---

## out/compaction-summaries.jsonl — Compaction summary log { #compaction-summaries }

**Location:** `<session-workdir>/out/compaction-summaries.jsonl`

Written only when the manifest sets `inference.compaction.dump_summaries: true` (default
`false`; see [`inference.compaction.dump_summaries`](manifest-schema.md#field-inference)). Not
part of the `murmur.message.v1` envelope — this is a flat JSONL eval log, one line per
**committed** compaction, appended in the order compactions occur:

```json
{"turn":17,"tokens_before":81501,"tokens_after":334,"summary":"1. THE BUG: ..."}
```

| Field | Type | Description |
|---|---|---|
| `turn` | integer | The turn the compaction fired on |
| `tokens_before` | integer | Session token count immediately before compaction |
| `tokens_after` | integer | Session token count immediately after the replacement context committed |
| `summary` | string | The compaction hook's replacement text, verbatim — the exact string that replaced the context, not a re-inferred or re-encoded copy |

**When a line is written.** Only after a compaction hook returns `replace-context` *and* that
replacement passes the existing tool-call-pairing safety net and actually commits. A compaction
attempt the safety net rejects, or a session where no hook returns `replace-context` at all,
appends nothing. This means the file (and the `out/` entry for it) is created lazily on the
first successful compaction — a run with `dump_summaries: true` that never compacts leaves no
file behind.

**Failure handling.** A write failure (for example a permission error) is logged to
`logs/bootstrap.log` and does not fail the session — the compaction itself has already
committed by the time the write is attempted.

This log exists to capture the summary text itself, which `trace.jsonl`'s `compaction` event
does not record (it carries only the two token counts).
