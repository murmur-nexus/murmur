# Capsule I/O Schema

Capsules communicate with their runtime environment through structured files in the session workdir. This page documents the typed I/O envelope.

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

## Checkpoint files { #checkpoint-files }

The three checkpoint files under `workdir/checkpoints/` (`summary.md`, `plan.json`,
`decisions.json`) are HMAC-SHA256-signed by the runtime whenever it has visibility into a
legitimate write, and verified before the agent gets control on every session start —
including a resume against a pre-existing workdir via `mur run --workdir <dir>`.

**When signing happens:**

- immediately after a blocking compaction hook returns a replacement context
- at every session-end lifecycle event, regardless of whether compaction ever fired

For each checkpoint file that exists at that point, the runtime writes a sidecar
`checkpoints/<name>.sig` containing a hex-encoded HMAC-SHA256 tag over the file's current bytes.

**When verification happens:** at session start, before the first inference call. Each
checkpoint file's `.sig` is recomputed and compared; a file with a missing, undecodable, or
mismatched signature is renamed to `checkpoints/<name>.rejected` (its stale `.sig` removed) so
the trusted path is empty, and a warning naming the file is written to `logs/bootstrap.log`. A
pre-existing `.rejected` file from an earlier rejection is overwritten, not preserved. Files
that verify successfully are left untouched.

**Signing key.** A 32-byte key is generated once per capsule workdir (CSPRNG) and persisted at
`$HOME/.murmur/checkpoint-keys/<sha256 of the canonicalized accessible workdir path>.key` with
owner-only (`0600`) permissions — a location outside any directory the WASI sandbox pre-opens,
so no WASM tool, hook, or the agent itself can read or forge it via shell or `wasi:filesystem`.

**Fail-open on infrastructure failure.** If the signing key itself cannot be derived (for
example `HOME` is unset, or the key directory is unwritable), signing and verification are
skipped for that session — a warning is logged to `logs/bootstrap.log`, but the session still
launches and no existing checkpoint file is renamed or deleted as a side effect. A key-derivation
failure is never treated as a signature mismatch.

---

## Script capsules

Script capsules are not affected by this schema. The `out/result.json` writer is part of the native agent loop only. Script capsules continue to write their own output to `out/result.txt` (or other paths) as before.
