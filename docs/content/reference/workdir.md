# Session Workdir

Every session runs against two directories: the one the capsule can see, and the one the runtime
keeps its bookkeeping in.

| Directory | Path | Holds |
|---|---|---|
| Accessible workdir | The directory passed to `mur run --workdir`, otherwise `<manifest-dir>/workdir/<session-id>` | The capsule's current directory — its task, everything it writes, and the `$HOME` its shell commands run under |
| Session workdir | `<accessible-workdir>/.murmur/<session-id>` when `--workdir` is passed, otherwise the accessible workdir itself | Runtime bookkeeping: staged artifacts, results, traces and logs |

Without `--workdir` the two are one directory and everything below lands in the same place.

---

## Task input

The agent reads its task from the accessible workdir at the start of every task, taking the first
of:

1. `task.md`
2. `input.txt`
3. Neither, in which case the agent starts with an empty task.

`task.md` has three writers:

| Writer | When |
|---|---|
| `mur run --task <value>` | Before launch. A value naming an existing file is copied; anything else is written as text |
| The runtime | On each incoming A2A message, and again when an `on-task-end` hook returns `reopen-task` — rewritten as the original task plus every reopen's feedback so far |
| The capsule | Through its own file tools, like any other file in the accessible workdir |

Under `lifecycle.task_acceptance: queue` the runtime deletes `task.md` after each task, so the next
task comes from the queue rather than from a stale file.

---

## Session workdir files

| Path | Notes |
|---|---|
| `MURMUR.md` | The capsule's generated inventory: identity, directory layout, installed tools and skills, shell access. Agent sessions only. Written at staging and rewritten once the capsule's port is bound |
| `trace.jsonl` | One JSON object per session event. See [Observability schemas](observability-schemas.md) |
| `eval.jsonl` | Scorer output for the session. `mur eval` reads it after each case. See [Observability schemas](observability-schemas.md) |
| `out/result.txt` | The agent's final output. Written on every terminal outcome; a failure writes `error: <message>` |
| `out/result_<task-id>.txt` | Per-task copy of the final output, so one task does not overwrite another's. Only under `lifecycle.conversation: threaded` |
| `out/compaction-summaries.jsonl` | The text each committed compaction replaced the context with. See [below](#compaction-summaries) |
| `contexts/<context-id>/history.json` | Full message history for one A2A `contextId`, reloaded when a later task arrives on the same context. Only under `lifecycle.conversation: threaded`, and only after a task succeeds |
| `logs/bootstrap.log` | Staging and agent-loop diagnostics: the installed tool inventory, compaction decisions, and non-fatal write failures |
| `logs/otel.log` | OpenTelemetry exporter diagnostics |
| `logs/hook-<hook-name>.log` | Errors from one hook, one per line |
| `tools/<name>/murmur.yaml` | A staged artifact's manifest |
| `tools/<name>/<name>` | A staged native binary, marked executable |
| `tools/<name>/skill.md` | A staged skill's text. The runtime returns this as the tool result when the skill is called |

## Accessible workdir files

| Path | Notes |
|---|---|
| `task.md`, `input.txt` | The task, as above |
| `.capsule-home` | `$HOME` for every shell command. Created on the first shell invocation |
| `logs/shell-<timestamp>.log` | Full stdout and stderr of one shell command, written when either stream exceeds 16 KB. The tool result carries the path |
| `checkpoints/` | `summary.md`, `plan.json` and `decisions.json`. `MURMUR.md` directs the agent to write state here to survive compaction; the runtime neither reads nor writes them |

Everything else in this directory belongs to the capsule.

---

## out/compaction-summaries.jsonl { #compaction-summaries }

Written only when the manifest sets `inference.compaction.dump_summaries: true` (default `false`;
see [`inference.compaction.dump_summaries`](manifest.md#field-inference)). One line per committed
compaction, appended in the order compactions occur:

```json
{"turn":17,"tokens_before":81501,"tokens_after":334,"summary":"1. THE BUG: ..."}
```

| Field | Type | Description |
|---|---|---|
| `turn` | integer | The turn the compaction fired on |
| `tokens_before` | integer | Session token count immediately before compaction |
| `tokens_after` | integer | Session token count immediately after the replacement context committed |
| `summary` | string | The text of the committed replacement context, tool messages excluded |

A line is written only after a compaction hook returns `replace-context` **and** that replacement
survives the tool-call-pairing check and commits. A rejected replacement, or a session where no
hook returns `replace-context`, appends nothing — so the file appears on the first successful
compaction, and a run that never compacts leaves none behind.

A write failure is logged to `logs/bootstrap.log` and does not fail the session; the compaction has
already committed by then.

This log is the only place the summary text is kept. The `compaction` event in `trace.jsonl` records
the turn and the two token counts, not the text.

---

## Lockfile (`murmur.lock`) { #lockfile-murmurlock }

`murmur.lock` sits in the project directory beside `murmur.yaml`, not in the workdir. It pins every
registry-resolved artifact to a version and a hash:

```yaml
lock_version: 1
artifacts:
  - name: some-tool
    resolved_version: "1.2.3"
    sha256:
      wasm: "<sha256>"
```

`lock_version` must be `1`. Every path below verifies against the pin before writing anything: a
registry-resolved artifact whose version or hash disagrees with an existing entry is rejected, with
nothing written to disk or to the lock.

| Path | When it writes |
|---|---|
| `mur run` | Creates `murmur.lock` when none exists. Once present, only verifies against it — an existing entry is never refreshed |
| `mur eval` | As `mur run`, once for the whole dataset run |
| `mur install` | Upserts an entry for each artifact it installs successfully, preserving the rest. Skipped for `-g` (no project directory), for local-file installs, and for `--all-platforms` |
| `manage.pull()` | The same verify-then-upsert, from a running capsule rather than the CLI |

A missing entry for a manifest artifact, or an unsupported `lock_version`, fails the run with
`E-RUN-003`. An install whose registry hash disagrees with the pin fails with `E-REG-005`. See
[Diagnostics](diagnostics.md).
