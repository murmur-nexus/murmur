# Session Workdir

Every session runs against four directories: the one the capsule works in, the session's own
directory beneath it, the one holding the records the runtime keeps about the session, and the one
holding the files the runtime writes for its own use.

| Directory | Path | Holds | The capsule's commands |
|---|---|---|---|
| Accessible workdir | The directory passed to `mur run --workdir`, otherwise `<manifest-dir>/workdir/<session-id>` | The capsule's current directory — everything it writes | Read and write |
| Session workdir | `<accessible-workdir>/.murmur/<session-id>` when `--workdir` is passed, otherwise the accessible workdir itself | The agent's results, and the `$HOME` its shell commands run under | Read and write |
| [Records directory](#records-directory) | `~/.murmur/records/<workdir root>/<session-id>` | `trace.jsonl`, `blobs/`, `logs/`, a delegated child's `completion.json` and a scoring hook's `eval.jsonl` | Read only, where the host can enforce it. Kept after the session |
| [Runtime directory](#runtime-directory) | `~/.murmur/sessions/<session-id>/runtime` | `MURMUR.md`, `task.md`, the manifest copy, installed tools and submitted plans | Read only, where the host can enforce it |

Without `--workdir` the first two are one directory.

In a [formation](roster.md#launch), the entry member's accessible workdir is the roster's project
directory, and each peer's is `~/.murmur/formations/<frm_id>/<member>/` — see
[Member directories](roster.md#member-directories).

To find what the *capsule* changed, diff the accessible workdir and subtract what the runtime wrote
there. `mur run --explain-scope` enumerates those paths, as `runtime_writes` under `--json`:

- That enumeration is authoritative; the tables below describe what each path holds.
- `trace.jsonl` carries the same array verbatim at `session_start.effective_grants.runtime_writes`,
  so a run reports the paths to subtract from it.
- Paths are relative to the accessible workdir and carry the literal segment `<session-id>`, which
  `session_start.session_id` supplies.
- Excluding the single prefix `.murmur/` covers every runtime-written path outside the accessible
  workdir's own top level.

Two more directories sit outside the workdirs and outlive every session: a
[durable state store](#state-store), for artifacts that ask for one by name, and the
[conversation record](#the-conversation-record), which every `http` capsule keeps by default.

---

## Task input

The agent reads its task from `task.md` in the [runtime directory](#runtime-directory). It gets
there one of three ways:

| Way in | When |
|---|---|
| `mur run --task <value>` | Before launch. A value naming an existing file is copied; anything else is written as text |
| The operator's file in the accessible workdir | Once, when the session starts and the runtime directory has no `task.md`: `task.md`, else `input.txt` except under `inference.transport: process`. The workdir file is left where it is and never read again |
| An A2A message | On each incoming message |

The runtime also rewrites it when an `on-task-end` hook returns `reopen-task`, as the original task
plus every reopen's feedback so far. When a task continues with
[`call-member`](runtime-provided-tools.md#call-member) answers on an attempt that restarts rather
than continuing its conversation, the rewrite also holds every batch of answers so far.

The capsule reads it at `$MURMUR_RUNTIME_DIR/task.md` in a shell command and at
`murmur-runtime/task.md` in a file tool, and cannot change it where the host can enforce that — see
[What protects it](#what-protects-it). A `task.md` the capsule writes in its
own workdir is its own file.

A task whose text is empty or only whitespace fails without a request to the model, with a message
saying the task is empty. A launch that receives no task at all ends after its
[idle timeout](manifest.md#idle-timeout) without a model call.

Under `lifecycle.task_acceptance: queue` with `lifecycle.after_task: sleep` the runtime deletes
`task.md` after each task, so the next task comes from the queue rather than from a stale file.

---

## Session workdir files

| Path | Notes |
|---|---|
| `out/result.txt` | The agent's final output. Written on every terminal outcome of a task that ran; a failure writes `error: <message>`, a turn stopped at [`inference.max_tokens`](manifest.md#inference-max-tokens) writes the model's partial text followed by a truncation marker naming that field and the cap, and a task that ends `canceled` writes `canceled: the task was canceled before it completed, so it has no result`. A task canceled before it started writes nothing |
| `out/result_<task-id>.txt` | Per-task copy of the final output, so one task does not overwrite another's: a completed task's answer, or the `canceled:` line above for a canceled one. Only under `lifecycle.conversation: threaded` |
| `out/compaction-summaries.jsonl` | The text each committed compaction replaced the context with. See [below](#compaction-summaries) |
| `.capsule-home` | `$HOME` for every shell command and native tool. Created on the first such call. Reached only through the variable |
| `.mur-tmp` | Backs `/tmp` inside a `sealed` capsule's composed root. The capsule sees `/tmp`. Counted against `capabilities.resources.workdir_max_bytes` like everything else in the workdir |

The capsule's commands can write these like any other file in the workdir.

---

## Accessible workdir files

| Path | Notes |
|---|---|
| `task.md`, `input.txt` | The operator's task, read once when the session starts. See [Task input](#task-input) |
| `.murmur/<session-id>/` | The session workdir |
| `.murmur/children/<capsule>-<suffix>/` | One directory per sub-capsule launch, each the child's own accessible workdir. Created mode `0700` |
| `logs/shell-<timestamp>.log` | Full stdout and stderr of one shell command, written when either stream exceeds 16 KB. The tool result carries the path |
| `logs/<work-id>.log` | The same, for a command demoted to the background. The completion carries the path |
| `peer-in/<handle-id>-<name>` | Bytes fetched by redeeming a peer handle. See [Resource plane](resource-plane.md) |
| `out/result.txt` | A script capsule's final output. An agent session writes it to the session workdir instead |
| `checkpoints/` | `summary.md`, `plan.json` and `decisions.json`. `MURMUR.md` directs the agent to write state here to survive compaction; the runtime neither reads nor writes them |

Everything else in this directory belongs to the capsule.

A `sealed` capsule adds nothing to this list: its `/tmp` store is in the session workdir and its
synthetic `/etc` files are beside the runtime directory.

---

## Runtime directory { #runtime-directory }

Every file the runtime writes for its own use, and reads back as input, lives in a per-session
directory outside the workdir:

```
~/.murmur/sessions/                       0700
└── <session-id>/                         0700, removed when the session ends
    ├── .lock                             held by the live session
    ├── runtime/                          $MURMUR_RUNTIME_DIR
    │   ├── MURMUR.md
    │   ├── task.md
    │   ├── murmur.yaml                   the launched manifest, under its own name
    │   ├── plans/plan-<n>.json
    │   └── tools/<name>/murmur.yaml | <name> | skill.md
    ├── sealed-etc/passwd, sealed-etc/group
    └── harness/<run-id>/
```

A launch whose `HOME` is unset, relative, or names a directory in which `~/.murmur` cannot be
created is refused with [`E-IO-004`](diagnostics.md#e-io-004) before anything is created.

| Path | Holds |
|---|---|
| `runtime/MURMUR.md` | The capsule's generated inventory: identity (for a formation member, also its roster name and formation id), both directories, installed tools and skills, shell access. Agent sessions only. Written at staging and rewritten once the capsule's port is bound |
| `runtime/task.md` | The task. See [Task input](#task-input) |
| `runtime/<manifest file>` | A copy of the manifest file the launch read — `--manifest`, or the project's `murmur.yaml` — under its own name. A capsule launched by name with `--capsule` read none and has none |
| `runtime/tools/<name>/murmur.yaml` | A staged or pulled artifact's manifest. `<name>` is the artifact's bare name, the part of its full name after the `/`, which is also its tool name |
| `runtime/tools/<name>/<name>` | A staged or pulled native tool, marked executable |
| `runtime/tools/<name>/skill.md` | A staged skill's text. The runtime returns this as the tool result when the skill is called |
| `runtime/plans/plan-<n>.json` | One submitted plan per file, numbered by this session's own counter, and read back when the plan runs |
| `sealed-etc/` | The `passwd` and `group` a `sealed` capsule's composed root binds at `/etc/passwd` and `/etc/group` |
| `harness/<run-id>/` | The files a `transport: process` driver asked for, present while its harness runs |

### How the capsule reaches it

| Reader | Where |
|---|---|
| A shell command, a plan's `shell` step, a native tool, a `transport: process` harness | `$MURMUR_RUNTIME_DIR`, the absolute path of `runtime/`. Set by the runtime last, so neither `capabilities.env.allow`, a shell baseline variable nor the host can change it |
| A script capsule's own component, a `runtime: tool` artifact with no `capabilities.filesystem.scope` | `murmur-runtime/`, beside the workdir it sees as its current directory, read-only. A script capsule reads its task at `murmur-runtime/task.md` |
| A tool with a `scope`, a driver, a hook | Nothing |
| The model | The [`[Capsule]` block and `MURMUR.md`](../concepts/session-loop.md#murmur-md) name both ways in |

`sealed-etc/` and `harness/` are reached by no command.

### What protects it

| Applied tier | `runtime/` for the capsule's commands |
|---|---|
| `mountns+pivot_root+landlock+seccomp` (`sealed`) | Bound read-only at its own absolute path inside the composed root, and granted read-only by Landlock. A bind that fails ends the session with [`E-RUN-014`](containment.md#field-containment) |
| `landlock+seccomp` (`scoped`) | Granted read-only by Landlock: reading and listing, no writing, no running. A native tool's entrypoint runs through its own grant |
| `seccomp-only` | Not protected: no filesystem mechanism |
| `none` (macOS and every non-Linux host) | Not protected: no kernel containment for capsule commands |

A command that tries to overwrite, append to, remove, rename over or create a file there gets
`Permission denied` under Landlock and `Read-only file system` inside a composed root; a hard link
from it into the workdir gets `Invalid cross-device link`.

What Landlock does not mediate on `landlock+seccomp`:

| Operation | What a command can do |
|---|---|
| `chmod`, `utimes`, `setxattr` | Change a runtime file's mode or times, never its contents. A file made unreadable fails the read that needs it |
| `truncate(2)` at Landlock ABI 1 or 2 (kernels before 6.2) | Empty or zero-extend a runtime file by path, without writing a byte of its own |

Inside a composed root the read-only bind refuses all of them.

### A runtime directory inside the workdir { #runtime-directory-inside-the-workdir }

When `~/.murmur/sessions`, or the session's [records directory](#records-directory), lies inside the
accessible workdir — `HOME` and `--workdir` the same directory, for one — the capsule's commands can
write it:

| Declared containment | Outcome |
|---|---|
| `scoped` or `sealed` | Refused with [`E-CAP-003`](diagnostics.md#e-cap-003), naming the directory and the workdir |
| `advisory` | The launch runs, prints [`W-SEC-033`](diagnostics.md#w-sec-033) and records `achieved_containment: advisory` |

Run with a `--workdir` that does not contain `~/.murmur`.

### Lifetime

| When | What happens |
|---|---|
| Staging, after every refusal that stops a launch | The directory is created and its `.lock` taken |
| The session ends — success, a failed task, a refusal, a `SIGTERM` — or a staged session is never launched | The directory is removed |
| The next launch's staging | Every `~/.murmur/sessions/<id>/` whose `.lock` is free and more than 60 seconds old, and every entry with no `.lock` more than 60 seconds old, is removed. A live session's lock is never free |

### Reporting

`mur run --explain-scope` prints the block after `Containment`:

```
Runtime directory
  path:      /home/dev/.murmur/sessions/<session-id>/runtime
  commands:  read only (landlock)
```

On a host or workdir that cannot protect it, `commands:` reads `can change it — <reason>`, and the
same reason appears under `Not protected here`. The `landlock+seccomp` tier at Landlock ABI 1 or 2
reports this too, naming `truncate(2)`. `--json` carries the object as `runtime_dir`, and
`trace.jsonl`'s `session_start` carries it verbatim as `effective_grants.runtime_dir`. `mur doctor`
prints the same block for the project's declared containment.

---

## Records directory { #records-directory }

Every record the runtime writes about a session lives in a per-session directory outside the
workdir, kept after the session ends:

```
~/.murmur/records/                                0700
└── <workdir root without its leading />/         0700, each level
    └── <session-id>/                             0700, created at staging, kept after the session
        ├── trace.jsonl
        ├── blobs/<sha256>
        ├── logs/bootstrap.log | otel.log | hook-<hook-name>.log
        ├── completion.json
        └── hooks/
```

The **workdir root** names the group a session's directory sits in:

| Launch | Workdir root | Records directory |
|---|---|---|
| `mur run --workdir /home/dev/proj` | `/home/dev/proj` | `~/.murmur/records/home/dev/proj/<session-id>` |
| `mur run` from `/home/dev/proj` | `/home/dev/proj/workdir` | `~/.murmur/records/home/dev/proj/workdir/<session-id>` |
| A [formation](roster.md#member-directories) peer | `~/.murmur/formations/<frm_id>/<member>` | That directory mirrored under `~/.murmur/records` |

The root is made absolute and resolved through its nearest existing ancestor, so a link above it
names the directory it points at. A project moved to another path starts a new group; the old one
stays under the old path.

| Path | Holds |
|---|---|
| `trace.jsonl` | One JSON object per session event. See [Observability schemas](observability-schemas.md#session-trace-tracejsonl) |
| `blobs/<sha256>` | Trace bodies too large to inline, addressed by their own digest. See [Trace blobs](observability-schemas.md#trace-blobs) |
| `logs/bootstrap.log` | Staging and agent-loop diagnostics: the installed tool inventory, once per session and again each time it changes, compaction decisions, non-fatal write failures, and any line the runtime could not write to a [closed standard output or standard error](cli.md#mur-run-closed-stream) |
| `logs/otel.log` | OpenTelemetry exporter diagnostics |
| `logs/hook-<hook-name>.log` | Errors from one hook, one per line. `<hook-name>` is the bare name: `hook-grafana` for `murmur/hook-grafana` |
| `completion.json` | How a delegated child ended: written by the child at the end of its session and by the launcher that started it, read by that launcher |
| `hooks/` | The current directory of every hook declared [`capabilities.filesystem.records: true`](manifest.md#hook-capabilities), read-write. `murmur/hook-eval` writes `hooks/eval.jsonl` and `hooks/logs/hook-hook-eval.log` here. Created only when such a hook is declared |

### How the capsule reaches it

| Reader | Where |
|---|---|
| A shell command, a plan's `shell` step, a native tool, a `transport: process` harness | `$MURMUR_RECORDS_DIR`, the absolute path of the session's records directory. Set by the runtime last, on `$MURMUR_RUNTIME_DIR`'s terms |
| A hook declared `capabilities.filesystem.records: true` | `hooks/`, as its current directory, read-write |
| A WASM tool, a driver, any other hook | Nothing |

### What protects it

The runtime directory's mechanisms, applied to the records directory — see
[What protects it](#what-protects-it). A command that tries to overwrite, append to, truncate,
remove, rename over or create a file there gets `Permission denied` under Landlock and
`Read-only file system` inside a composed root; a hard link from it into the workdir gets
`Invalid cross-device link`. The runtime's own writes go on for the whole session. On a host with no
kernel containment, macOS included, a command can change it.

A records directory inside the workdir is judged as a runtime directory inside it is — see
[A runtime directory inside the workdir](#runtime-directory-inside-the-workdir).

### Lifetime

| When | What happens |
|---|---|
| Staging, after every refusal that stops a launch | The directory is created |
| The session ends | Nothing: the directory is kept |
| A later launch of a capsule declaring [`trace.retain`](manifest.md#trace-retain) | Each session that policy does not keep loses its records directory and its session workdir together |
| An ended formation pruned by its entry member's `trace.retain` | The formation's whole records subtree goes with its formation directory |

### Reading it

`mur trace`, `mur eval`, `mur run --resume`, `mur ps` and `mur stop` read the records directory. A
`--workdir` given to `mur trace` or `mur eval` names the workdir root — the directory a run was
given as `--workdir`, or `<manifest-dir>/workdir` — and the sessions listed are the ones in its
group. `mur run --json`'s [readiness line](cli.md#mur-run-json) and the
[running record](cli.md#running-capsule-records) carry the session's `records_dir`.

### Reporting

`mur run --explain-scope` prints a `Records directory` block after `Runtime directory`:

```
Records directory
  path:      /home/dev/.murmur/records/home/dev/proj/workdir/<session-id>
  commands:  read only (landlock)
```

Its four keys, its per-tier `commands:` reasons and its `Not protected here` statement are the
runtime directory's. `--json` carries the object as `records_dir`, `trace.jsonl`'s `session_start`
carries it as `effective_grants.records_dir`, and `mur doctor` prints the same block.

---

## Links inside the workdir { #links-inside-the-workdir }

The runtime never follows a symbolic link inside the accessible workdir, the session workdir or the
runtime directory. Every path in the tables above, and every file the runtime reads to decide what
to do next, is reached one directory at a time from its directory, and a link anywhere on the way
is refused.
Nothing outside the workdir is written, read or removed through one. The directory passed to
`--workdir` may itself be reached through a link; only what lies beneath it is checked.

| The runtime refuses | When it would |
|---|---|
| A symbolic link at any directory on the way to a path, or at the path itself | Write, append to, read, create, or remove it |
| A FIFO, socket, device node or directory where a regular file belongs | Append to or read it |
| A regular file with a second hard link | Append to it, or rewrite it in place |

Under `--workdir`, keep `task.md`, `input.txt` and `.murmur` in that directory as real files and
directories: a link at `.murmur` stops the launch, and a link at the task file fails the first
task.

What a refusal does depends on when it happens:

| When | What happens |
|---|---|
| Staging: the `.murmur/<session-id>` directory, installing tools and skills, `MURMUR.md` | The launch fails with [`E-RUN-050`](diagnostics.md#e-run-050), naming the path and the link |
| During a task, on any transport | A [`workdir_path_refused`](observability-schemas.md#workdir-path-refused) trace line for each refusal, then `task_failed` with cause `workdir_path_refused`, then `task_end` with `exit_status: failed`. No further request is sent to the model for that task. An A2A task ends `failed` with the refusal as its status message, and `mur run --task` exits with [`E-RUN-050`](diagnostics.md#e-run-050) |
| A tool's own write — a submitted plan, `fetch-peer-file`, `manage.pull()`, `pull-artifact`, `manage.remove()` or `remove-artifact` | The tool call returns an error naming the path, and the task then fails as above |
| Between tasks | The next task to start fails as above before it reads its task or sends a request |

---

## Durable state store { #state-store }

An artifact that declares `capabilities.state` is granted one directory outside both workdirs:

| | Path | Lifetime |
|---|---|---|
| Session workdir | `<accessible-workdir>` or `<accessible-workdir>/.murmur/<session-id>` | One session |
| Accessible workdir | `--workdir`, otherwise `<manifest-dir>/workdir/<session-id>` | One session, unless `--workdir` names a directory you keep |
| State store | `~/.murmur/state/<store>/` | Every session of that capsule, on that machine |

A component sees the store as a directory named `state`, beside the workdir it sees as its
current directory. Its code reaches the store with an ordinary relative path:

```rust
std::fs::write("state/notes.jsonl", contents)?;   // the store
std::fs::write("out/result.txt", summary)?;       // the workdir
```

`~/.murmur`, `~/.murmur/state/` and each store directory under it are created mode `0700`, and the
mode is reasserted on every launch. See [`~/.murmur` modes](config.md#murmur-home-permissions).

### What distinguishes it

**It is keyed by capsule, not by directory.** The store name comes from `capabilities.state.store`,
defaulting to the capsule name — never from the workdir, the session id or the machine's layout. A
launch that gets a fresh `<manifest-dir>/workdir/<session-id>` reads back exactly what the previous
launch wrote.

**It sits outside every workdir, and only the artifact that declared it can reach it.** A subtree
of the workdir would be readable by anything that can reach the workdir — `murmur/tool-editor` and
`shell` included — and `capabilities.filesystem.scope` cannot help, because it is a single path
prefix: protecting one subtree would mean narrowing every other artifact. The capsule's own code
declares no artifact grant and reaches no store.

**Each capsule gets its own store.** Two capsules launched in the same directory get two stores and
cannot see each other's, with or without a declaration on either side. Sharing between capsules
goes over A2A, with a grant on both ends. Declaring the same `store:` name in two capsules is the
one way to point them at one directory, and it has to be written in both manifests.

**WASM tools, drivers and hooks reach the store; native subprocesses do not.** Under the `sealed`
containment class the store is absent from the capsule's composed root, so neither a binary
allowlisted by `capabilities.shell.allow` nor a sub-capsule allowlisted by
`capabilities.spawn.allow` can open it.

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
    - murmur/tool-corpus: shey -> /home/dev/.murmur/state/shey
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

A record whose capsule declares [`context.retain`](manifest.md#context-retain) opens with a
[header line](#record-header). Every other line is one message.

One line is one message, as the runtime holds it:

```json
{"role":"user","content":[{"type":"text","text":"Summarize today's changes."}],"id":"msg_01a04900754b7183b66c11e744612e2d"}
```

`role`, `content` and `id` are always present. `id` is `msg_` plus a uuid-v7, minted once where the
message was created and preserved everywhere after — including across a reload and across a hook
that hands the message back. It is the runtime's own and is stripped before the messages go to a
driver. A `tool` message also carries `tool_call_id` and `is_error`.

Six more keys are the runtime's own envelope, each written when it applies and each stripped
before the messages go to a driver:

| Key | Written on |
|---|---|
| `source_id` | A message a hook produced, when that hook supplied one |
| `canceled` | The assistant message left behind by a turn a person stopped |
| `truncated` | An assistant message the provider cut off at [`inference.max_tokens`](manifest.md#inference-max-tokens) |
| `fence` | A message whose `content` is wrapped in the [untrusted fence](untrusted-fence.md) — the source name, `tool:<artifact name>` or `task:<origin>` |
| `inserted_by` | A message a hook output put into the conversation: `replace-context` for a compaction hook's replacement, `seed-context` for an `on-task-start` hook's seed |
| `produced_by` | Every assistant message the agent loop writes, when the capsule declares [`inference.alternates`](manifest.md#inference-alternates): `{"driver": "<artifact>", "model": "<model>"}`, the [driver choice](../concepts/context.md#switching-drivers) that produced it. A message without it counts as the primary's |

`inserted_by` is set by the runtime when it commits the hook's output, whatever `role` the message has and whatever the hook claimed. A message a compaction hook hands back unchanged keeps the value it already had, so a person's task message stays without one.

The record holds every message that enters the context, in the order it enters: the task's user
message, each committed `seed-context` message, each assistant message, each tool result, and each
message a compaction commits — beside, not instead of, the messages it replaced.

[`context.retain`](manifest.md#retention) is what bounds it, and
[`mur conversation`](cli.md#mur-conversation) is what an operator prunes it with by hand. Without
a `retain:` block the record grows without bound.

### The header line { #record-header }

The first line of a record whose capsule declares `context.retain`, and of any record
[`mur conversation truncate`](cli.md#mur-conversation-truncate) has rewritten:

```json
{"type":"murmur.record","capsule":"shey","created_ms":1756400000000,"truncated":{"dropped":500,"oldest_surviving_id":"msg_01a0490075…","last_dropped_id":"msg_0199f2a1c3…","at_ms":1756400000000}}
```

| Key | Value |
|---|---|
| `type` | Always `murmur.record`. A first line carrying anything else is not a header |
| `capsule` | The capsule that owns this record — `name:` from its manifest, not the record store. What makes pruning safe when two capsules share one `context.record_store` |
| `created_ms` | When the header was written. On an adopted record, that is the adoption, not the conversation's first message |
| `truncated` | Absent until the record has been truncated. `dropped` is cumulative over the record's life; `last_dropped_id` is what [`mur conversation ls --message`](cli.md#mur-conversation-ls-message) classifies a missing id against |

It is a JSON object with no `role`, so it is not a message: every reader of a record skips it, the
`total` a [`murmur:conversation/read`](wit-interfaces.md#murmurconversationread) page reports does
not count it, and a `threaded` reload does not load it.

A record with no header line is unowned: the age sweep skips it however old it is. The next launch
that opens it under `mur run --context` writes the header and applies its policy from that launch.

### What pruning removes { #what-pruning-removes }

| Policy | Removed |
|---|---|
| [`trace.retain`](manifest.md#trace-retain) | A whole session: its [records directory](#records-directory), with its `trace.jsonl` and `blobs/`, and its session workdir |
| [`context.retain.max_messages`](manifest.md#context-retain) | The oldest message lines of one `conversation.jsonl`. The file, the directory and every surviving `id` stay |
| [`context.retain.max_age`](manifest.md#context-retain) | A whole `<context-id>/` directory under the record store |

Pruning runs at launch, in the runtime, immediately after the session's `session_start`. Every
deletion is written to that session's own trace as a
[`retention` event](observability-schemas.md#retention).

### What it means for a run

**It is written as the context is built, not at the end.** A task that fails, and one that spends
`inference.max_turns`, have both already recorded everything they sent.

**`lifecycle.conversation` governs loading, not recording.** A `stateless` capsule appends to its
record like any other and simply starts every task from nothing;
[`threaded`](manifest.md#lifecycle-conversation) starts a task from the whole record for its
context. [`mur run --resume`](cli.md#mur-run) loads the record either way, for that launch only.

**Turning it off creates nothing.** [`context.record: off`](manifest.md#context-record) means no
`~/.murmur/conversations/` directory at all.

**A failure to write never fails a task.** An unresolvable `HOME`, a full disk or an unwritable
directory is reported once to stderr and to `logs/bootstrap.log`, and the task runs on unrecorded.

### Reading it from an artifact

No artifact ever gets a filesystem path into `~/.murmur/conversations/`. The only way in is
[`murmur:conversation/read`](wit-interfaces.md#murmurconversationread), granted per hook with
[`capabilities.conversation.read: true`](manifest.md#hook-capabilities).

| Path | Mode |
|---|---|
| `~/.murmur`, the conversation root and each directory under it | `0700` |
| `conversation.jsonl` | `0600` |
| `harness-session.json` | `0600` |

Each mode is set again on every append and every rewrite.

## The harness session map { #harness-session-map }

On [`inference.transport: process`](manifest.md#transport-process) the harness owns the
conversation. The runtime keeps no message list for that capsule and records one thing per context
— the opaque session id the harness answers to — in the directory the record would have been in:

```
~/.murmur/conversations/<record>/<context-id>/harness-session.json
```

`<record>` and `<context-id>` are the two segments above, so
[`context.record_store`](manifest.md#field-context) points both transports at the same place.

```json
{"type":"murmur.harness-session","session_id":"0199c7d4-1f60-7c31-9a6e-0f2b9c1d4e55","harness":"claude-code","driver":"murmur/driver-claude-code","created_ms":1758326400000,"updated_ms":1758326461000}
```

| Key | Value |
|---|---|
| `type` | Always `murmur.harness-session`. A file carrying anything else is read as no entry |
| `session_id` | What the harness calls this conversation. Opaque: the runtime only ever hands it back |
| `harness` | The harness its process driver named, for a person reading the directory |
| `driver` | The full `<scope>/<name>` of the process driver artifact that drove it, for a person reading the directory |
| `created_ms` | When this context first got a session |
| `updated_ms` | When the entry was last written |

The runtime never compares `harness` or `driver` against the capsule it is launching. Whether a
session still exists is a question only the harness can answer, and it answers it by failing the
turn — see [E-RUN-036](diagnostics.md#e-run-036).

### What decides the launch

| [`lifecycle.conversation`](manifest.md#lifecycle-conversation) | The map holds an id for this context | The harness is launched |
|---|---|---|
| `stateless` | Not read | Starting a new conversation, under a fresh id |
| `threaded`, or any launch under [`mur run --resume`](cli.md#mur-run) | No | Starting a new conversation, under a fresh id |
| `threaded`, or any launch under `mur run --resume` | Yes | Continuing that conversation, under the stored id |

A task whose context id resolved to nothing starts a new conversation and writes no entry.

The id the runtime mints is a bare UUID. A harness that mints its own reports it back, and **that**
is what the context is keyed on from then on. A turn that never names a session is keyed on the id
it was handed.

### What writes an entry { #what-writes-an-entry }

The entry is written once, when the run ends, and only when the run produced observable work: a
piece of text, a thought, a tool call, or a turn that ran to completion. A harness names its
session before the model has produced anything, so a run that got no further — one interrupted in
that window, or one refused for `auth` or `quota` — leaves no entry, and the next task in that
context starts a conversation the harness will answer to.

A cancel is not special. A task stopped after the harness has said something keeps the conversation
it opened, and the next task in that context continues it.

### What removes an entry { #what-removes-an-entry }

One action, asked for on the request that starts the next turn:

| Surface | How it is asked for |
|---|---|
| CLI | [`mur run --forget-session`](cli.md#run-forget-session), with `--context <id>` |
| A2A door | [`forgetSession: true`](agent-card.md#forget-session) in the metadata of a `SendMessage` or `SendStreamingMessage` message that starts a task |

The entry is deleted from the file and from the running capsule's memory, the run's trace records a
[`harness_session_forgotten`](observability-schemas.md#harness-session-forgotten) event naming the
context, the id that was dropped and who asked, and the turn launches as a new conversation under
the same context id. Asked for a context that holds no entry, it drops nothing and records nothing.

Nothing else removes one. A failed resume leaves the entry where it is — see
[E-RUN-036](diagnostics.md#e-run-036) — and [`context.retain`](manifest.md#context-retain) prunes
nothing here.

### What it does not keep

`context.record` is inert on this transport, exactly as
[`context.record: off`](manifest.md#context-record) is everywhere:

| | Under `transport: process` |
|---|---|
| `conversation.jsonl` | Never written |
| [`murmur:conversation/read`](wit-interfaces.md#murmurconversationread) | Reads an empty page, however the hook is granted |
| An `on-task-start` `seed-context` | Rejected, recorded as a [`context_seed`](observability-schemas.md#context-seed) with reason `unsupported_transport` |
| [`context.retain`](manifest.md#context-retain) | Prunes nothing — it is wired to the conversation record, which this transport keeps none of, and a deleted session id is a person's conversation lost with no way back |
| [`mur conversation ls`](cli.md#mur-conversation) and `rm` | Neither lists nor removes these contexts: both work on conversation records, and `rm` answers [`E-CNV-001`](diagnostics.md#e-cnv-001). [`mur run --forget-session`](cli.md#run-forget-session) is what drops a context's session |

The session id is the whole of what murmur keeps for such a capsule. To read or export the
conversation itself, use whatever the harness offers for its own sessions.

`context.record: off` and a host whose `HOME` cannot be resolved both mean no file at all: the
capsule still threads the contexts it serves for as long as it is running, and forgets them when it
stops. A map that cannot be written is reported once to stderr and to `logs/bootstrap.log`, and the
task runs on.

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
registry-resolved artifact to a version and a hash, and records who wrote the pin:

```yaml
lock_version: 3
artifacts:
  - name: ana/some-tool
    resolved_version: "1.2.3"
    sha256:
      any: "<sha256>"
    origin: runtime
    session: ses_0190a1b2c3d4...
  - name: local/some-native-tool
    resolved_version: "0.4.2"
    sha256:
      platforms:
        darwin-aarch64: "<sha256>"
        linux-x86_64: "<sha256>"
    origin: operator
```

`lock_version` must be `3`. Each entry's `name` is the artifact's full name, `<scope>/<name>` —
see [Artifact names](../concepts/registry.md#artifact-names). An entry may carry `owner_id`,
GitHub's numeric id of the scope's owner, written only when known: by `mur install` from a source,
and by a runtime pull whose source or stored payload records one.

An entry carries exactly one of two hash shapes:

| Key | Pins | Written for |
|---|---|---|
| `sha256.any` | One hash for one payload every host resolves | A WASM component or a static skill |
| `sha256.platforms.<platform>` | One hash per platform tag | A native tool, whose payload is a different binary per platform |

A platform tag is one of `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64`, `linux-x86_64` — the
same tags [`mur publish --platform`](cli.md#mur-publish) writes and the store files a payload
under.

A host verifies against the hash pinned for its own platform. A native artifact with no key for
this host is not pinned here yet — run `mur install` on this host to add it, which leaves every
other platform's key intact.

Every path below verifies against the pin before writing anything: a registry-resolved artifact
whose version or hash disagrees with an existing entry is rejected, with nothing written to disk or
to the lock. A platform the entry has no key for is not a disagreement.

| Path | When it writes |
|---|---|
| `mur run` | Creates `murmur.lock` when none exists. Once present, only verifies against it — an existing entry is never refreshed |
| `mur eval` | As `mur run`, once for the whole dataset run |
| `mur install` | Upserts an entry for each artifact it installs successfully, preserving the rest. An artifact installed from a file is pinned as `local/<name>`. Skipped for `-g` (no project directory) and for `--all-platforms`, which installs into the global store |
| `manage.pull()`, [`pull-artifact`](runtime-provided-tools.md#pull-artifact) | The same verify-then-upsert, from a running capsule rather than the CLI. Pins a new entry as `origin: runtime` and sets `owner_id` when the source or the stored payload records one; a pull whose owner id differs from one the lock records for the scope is refused — see [Pin origin](#lock-origin) |
| `manage.remove()`, [`remove-artifact`](runtime-provided-tools.md#remove-artifact) | Deletes one `origin: runtime` entry the manifest does not declare, and rewrites every other entry unchanged — see [Pin origin](#lock-origin) |

An upsert adds this platform's key beside the keys already there. It replaces the whole `sha256`
block only when everything in it is stale: a different `resolved_version`, or a change between the
`any` and `platforms` shapes.

### Pin origin { #lock-origin }

| Key | Values | Written | Notes |
|---|---|---|---|
| `origin` | `operator`, `runtime` | Always | `operator`: written by `mur install`, or by the `mur run` or `mur eval` that created the lock. `runtime`: written by a running capsule through `manage.pull()` or `pull-artifact`. An entry with no `origin` key reads as `operator` |
| `session` | A session id (`ses_…`) | Only for `origin: runtime` | The session whose capsule pulled the artifact. Required and non-blank for `runtime`; refused on `operator` |

Each writer sets the origin like this:

| Writer | New entry | Existing entry |
|---|---|---|
| `mur run` / `mur eval` creating the lock | `origin: operator` | — (they never rewrite an existing lock) |
| `mur install` | `origin: operator` | Becomes `origin: operator` and `session` is dropped; `mur install` [prints one line](cli.md#mur-install) for each runtime pin it adopts |
| `manage.pull()`, `pull-artifact` | `origin: runtime` with the pulling session | Origin left exactly as it was |
| `manage.remove()`, `remove-artifact` | — | An `origin: runtime` entry is deleted, whichever session pulled it. An `origin: operator` entry is never deleted |

A pull never turns an operator pin into a runtime one, and never relabels an earlier pull with its
own session.

[`manage.remove()`](wit-interfaces.md#manage-remove) reads the entry's origin when it runs, so an
entry `mur install` adopted mid-session is no longer removable. It also refuses an entry for an
artifact `murmur.yaml` declares, whatever its origin, because the next `mur run` needs that entry.

`mur run` and `mur eval` stage only the artifacts `murmur.yaml` declares, so an entry a capsule
pulled that the manifest does not declare is never staged. At the next launch, a declared artifact
whose pin has `origin: runtime` is handled by role:

| Declared as | At launch |
|---|---|
| A tool or skill without `gateway:` | Staged, and marked untrusted wherever it reaches the model: its tool-array description opens with the runtime-origin marker, and a skill's guidance arrives fenced. See [Artifact origin](../concepts/access-control.md#artifact-origin) |
| `runtime: hook`, `runtime: driver`, or with `gateway:` | Refused with [`E-RUN-043`](diagnostics.md#e-run-043) |
| A skill named by `inference.system_prompt_artifact` | Refused with [`E-RUN-043`](diagnostics.md#e-run-043) |

Both hold on every launch until `mur install <scope>/<name>@<version>` adopts the pin as `operator`.
`session_start.runtime_artifacts` in [`trace.jsonl`](observability-schemas.md#session-trace-tracejsonl)
lists every runtime pin a launch staged. [`mur doctor`](cli.md#mur-doctor) reports every runtime
pin.

A `mur` that predates `origin` reads a lock that has it, and drops both keys if it rewrites the
file; every entry then reads as `operator`.

A capsule with write access to the project directory — one run with the project as its
`--workdir` — can edit `murmur.lock` like any other file there, `origin` included. List
`murmur.lock` in [`capabilities.filesystem.read_only`](manifest.md#read-only-paths) to have the
runtime refuse the writes to it that it can identify.

A lock of any other `lock_version`, or an entry whose `name` is not a full name, is refused before
any entry is checked, with a message ending "delete murmur.lock and run `mur install` to regenerate
it". Delete `murmur.lock` and run `mur install` to regenerate it.

A missing entry for a manifest artifact, an entry with no hash for this host's platform, an
unsupported `lock_version`, or a malformed `origin` / `session` pair fails the run with
`E-RUN-003`. An install whose registry hash disagrees with the pin fails with `E-REG-005`. See [Diagnostics](diagnostics.md).
