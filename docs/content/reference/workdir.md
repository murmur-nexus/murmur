# Session Workdir

Every session runs against two directories: the one the capsule can see, and the one the runtime
keeps its bookkeeping in.

| Directory | Path | Holds |
|---|---|---|
| Accessible workdir | The directory passed to `mur run --workdir`, otherwise `<manifest-dir>/workdir/<session-id>` | The capsule's current directory — its task, everything it writes, and the `$HOME` its shell commands run under |
| Session workdir | `<accessible-workdir>/.murmur/<session-id>` when `--workdir` is passed, otherwise the accessible workdir itself | Runtime bookkeeping: staged artifacts, results, traces and logs |

Without `--workdir` the two are one directory and everything below lands in the same place.

Two more directories sit outside both and outlive every session: a
[durable state store](#state-store), for artifacts that ask for one by name, and the
[conversation record](#the-conversation-record), which every `http` capsule keeps by default.

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

## Durable state store { #state-store }

An artifact that declares `capabilities.state` is granted one directory outside both workdirs:

| | Path | Lifetime |
|---|---|---|
| Session workdir | `<accessible-workdir>` or `<accessible-workdir>/.murmur/<session-id>` | One session |
| Accessible workdir | `--workdir`, otherwise `<manifest-dir>/workdir/<session-id>` | One session, unless `--workdir` names a directory you keep |
| State store | `~/.murmur/state/<store>/` | Every session of that capsule, on that machine |

The store is mounted into the guest as a second WASI preopen named `state`, alongside the workdir
mounted as `.`. Guest code reaches it with an ordinary relative path:

```rust
std::fs::write("state/notes.jsonl", contents)?;   // the store
std::fs::write("out/result.txt", summary)?;       // the workdir
```

Both `~/.murmur/state/` and each store directory under it are created mode `0700`, and the mode is
reasserted on every launch.

### What distinguishes it

**It is keyed by capsule, not by directory.** The store name comes from `capabilities.state.store`,
defaulting to the capsule name — never from the workdir, the session id or the machine's layout. A
launch that gets a fresh `<manifest-dir>/workdir/<session-id>` reads back exactly what the previous
launch wrote.

**It sits outside every workdir, and only the artifact that declared it can reach it.** A subtree
of the workdir would be readable by anything holding the workdir preopen — `murmur-tool-editor` and
`shell` included — and `capabilities.filesystem.scope` cannot help, because it is a single path
prefix: protecting one subtree would mean narrowing every other artifact. The capsule's own code
declares no artifact grant and reaches no store.

**Each capsule gets its own store.** Two capsules launched in the same directory get two stores and
cannot see each other's, with or without a declaration on either side. Sharing between capsules
goes over A2A, with a grant on both ends. Declaring the same `store:` name in two capsules is the
one way to point them at one directory, and it has to be written in both manifests.

**WASM tools, drivers and hooks reach the store; native subprocesses do not.** Under the `sealed`
containment class the store is absent from the capsule's composed root, so an allowlisted binary
spawned through `capabilities.shell.allow` or `capabilities.spawn.allow` cannot open it.

### What belongs in the workdir instead

Per-project notes. The accessible workdir already *is* the project: notes about the repository the
capsule is working in belong beside that work, where they move, get committed and get deleted with
it. A store keyed by capsule would carry them from one project to the next.

The store is for what transcends workdirs — a corpus, a learned index, an append-only memory log
that means the same thing whichever directory the capsule was launched from.

### Reporting

`mur run --explain-scope` lists every declared store under `Effective grants`:

```
  state stores:
    - murmur-tool-corpus: shey -> /home/dev/.murmur/state/shey
```

`--json` emits the same list as `state_stores`, and `trace.jsonl`'s `session_start` carries it
verbatim as `effective_grants.state_stores`. Declaring a store changes no other field of the
report: it is a directory grant, not a containment property, so `declared_containment`,
`achieved_containment`, `floor_met` and `enforcement_tier` are unmoved by it.

`--explain-scope` resolves and prints host paths without creating any of them. Only a real launch
creates a store.

See [Tool and driver capabilities](manifest.md#tool-capabilities),
[Hook capabilities](manifest.md#hook-capabilities) and
[State store name](manifest.md#state-store-name).

---

## The conversation record { #the-conversation-record }

Every task on `inference.transport: http` appends the messages it puts in front of the model to one
durable file:

```
~/.murmur/conversations/<record>/<context-id>/conversation.jsonl
```

| Segment | Value |
|---|---|
| `<record>` | [`context.record_store`](manifest.md#field-context), defaulting to the capsule name |
| `<context-id>` | The task's context id: the `contextId` an A2A client sent, the value of [`mur run --context`](cli.md#mur-run), the id [`mur run --resume`](cli.md#mur-run) looked up from a previous session, or a fresh `ctx_…` per task |

Two runs given the same context id continue one conversation, whether they arrive over A2A or from
two `mur run --context <id>` launches with no session directory in common. `mur run --resume
<session>` reaches the same record without you having to know the id: it reads the context off that
session's trace.

One line is one message, as the runtime holds it:

```json
{"role":"user","content":[{"type":"text","text":"Summarize today's changes."}],"id":"msg_01a04900754b7183b66c11e744612e2d"}
```

`role`, `content` and `id` are always present. `id` is `msg_` plus a uuid-v7, minted once where the
message was created and preserved everywhere after — including across a reload and across a hook
that hands the message back. A `tool` message also carries `tool_call_id` and `is_error`; a message
a hook produced carries `source_id` when that hook supplied one. Neither `id` nor `source_id` ever
reaches a driver.

The record holds every message that enters the context, in the order it enters: the task's user
message, each committed `seed-context` message, each assistant message, each tool result, and each
message a compaction commits — beside, not instead of, the messages it replaced. Nothing trims the
record, and there is no retention or pruning mechanism.

### What it means for a run

**It is written as the context is built, not at the end.** A task that fails, and one that spends
`inference.max_turns`, have both already recorded everything they sent.

**`lifecycle.conversation` governs loading, not recording.** A `stateless` capsule appends to its
record like any other and simply starts every task from nothing;
[`threaded`](manifest.md#lifecycle-conversation) starts a task from the whole record for its
context. [`mur run --resume`](cli.md#mur-run) loads the record either way, for that launch only.

**Turning it off creates nothing.** [`context.record: off`](manifest.md#context-record) means no
`~/.murmur/conversations/` directory at all. So does `inference.transport: process`, whose CLI owns
its own conversation.

**A failure to write never fails a task.** An unresolvable `HOME`, a full disk or an unwritable
directory is reported once to stderr and to `logs/bootstrap.log`, and the task runs on unrecorded.

### Reading it from an artifact

No artifact ever gets a filesystem path into `~/.murmur/conversations/`. The only way in is
[`murmur:conversation/read`](wit-interfaces.md#murmurconversationread), granted per hook with
[`capabilities.conversation.read: true`](manifest.md#hook-capabilities). The conversation root and
each directory under it are created mode `0700`.

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
