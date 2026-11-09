# Guest Workers

A `mur run` session is one supervisor process, `mur` itself, and up to three guest workers, one
for each kind of WASM artifact. A guest worker is the same `mur` binary, started again by `mur` as
`mur __guest-worker <kind>`. It runs the code of every artifact of its kind. No WASM code runs in
`mur`.

## Which process runs what

| Process | Runs |
|---|---|
| `driver` worker | Every `runtime: driver` artifact: the inference driver, every driver choice, a driver a capsule reaches by name as a tool, a hook's `run-inference` calls and the `transport: process` driver. Also a script capsule's own component |
| `tools` worker | Every `runtime: tool` WASM artifact, staged at launch or pulled while the session runs |
| `hooks` worker | Every `runtime: hook` artifact: blocking, async and `on-stage` |
| `mur` | No WASM code. Everything listed under [What never enters a worker](#what-never-enters-a-worker) |

An artifact goes to a worker by its `runtime:` kind. The role of one call never decides it.
No tool shares the driver's process, and no hook shares a process with the tool it checks.

`mur doctor` checks a harness login by calling its process driver in a driver worker too.

## Started only when needed

A worker starts only when the capsule has an artifact of its kind:

| Capsule | Workers |
|---|---|
| A driver and WASM tools | `driver` and `tools` |
| A driver, WASM tools and a hook | `driver`, `tools` and `hooks` |
| A driver and only runtime-provided tools, such as `bash` or `delegate-task` | `driver` |
| A driver and `capabilities.install.tool`, before it pulls a tool | `driver`; `tools` starts at the first pulled tool's first call |
| A script capsule with no tools | `driver` |

`mur` starts the workers as staging begins, so their start overlaps artifact resolution. Each
worker runs in its own process group with `/` as its working directory. It moves with `mur` into
the session's cgroup scope. It dies with `mur`.

## What never enters a worker

A worker is started with an empty environment and nothing on its command line but its kind. It
refuses to start if its environment is not empty. It is not dumpable, so no other process can
read its memory or its `/proc` environment.

`mur` keeps everything WASM code must never hold:

| Kept in `mur` | Why |
|---|---|
| Every credential gateway and its key | A driver's or tool's request reaches the provider only through `mur`, which attaches the key |
| Formation and door tokens | `mur` addresses a formation call and attaches the token |
| The trace and the records directory | The run's record is written by `mur` alone |
| The session's processes, the cgroup scope and the namespaces | `murmur:process` and the shell allowlist are decided in `mur` |

Every call a WASM artifact makes past its own files, clocks and randomness goes to `mur` over the
worker's private connection:

- `wasi:http` requests
- `murmur:process`
- stream events and input requests
- a hook's `run-inference`, `task-io` and `conversation` reads
- a script capsule's tool registry, artifact manager and message send

`mur` answers a call only while the artifact call it belongs to is running on that worker, and
applies every policy check itself: the network allowlist, the credential gateway, the shell
allowlist and each import's grant.

## A lost worker

A worker that dies, or stops answering well past its call's `capabilities.limits.deadline_seconds`,
fails the task in flight. The run ends with `E-RUN-065`, which names the worker, its pid, the call
and how it ended. The trace still carries `task_failed`, `task_end` and `session_end`. A script
capsule's run ends with `session_end` and the exit status `failed`. A lost worker is not
restarted: a lost hooks worker takes every hook's session state with it.

A worker that cannot be started refuses the launch with `E-RUN-066`.

See [E-RUN-065](../reference/diagnostics.md#e-run-065) and
[E-RUN-066](../reference/diagnostics.md#e-run-066).

## Not confined by the containment class

A worker runs as your user, with the same reach `mur` has. The containment class does not confine
it. The [Routes block](../reference/containment.md#routes) of `mur run --explain-scope` says what
holds each route.
