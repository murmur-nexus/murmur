# Guest Workers

A `mur run` session is one supervisor process, `mur` itself, and a guest worker for each kind of
WASM guest that runs outside it. A guest worker is the same `mur` binary, started again by `mur`
as `mur __guest-worker <kind>`. It holds the Wasmtime engine and every store of its kind, and it
runs those guests' code.

## Which process runs what

| Process | Runs |
|---|---|
| `driver` worker | Every `runtime: driver` artifact: the inference driver, every driver choice, a driver a capsule reaches by name as a tool, and a hook's `run-inference` calls |
| `tools` worker | Every `runtime: tool` WASM artifact, staged at launch or pulled while the session runs |
| `mur` | Hooks, the `transport: process` driver and a script capsule's own component, plus everything listed under [What never enters a worker](#what-never-enters-a-worker) |

A guest goes to a worker by its artifact's `runtime:` kind. The role of one call never decides it.
No tool shares the driver's process.

## Started only when needed

A worker starts only when the capsule has a guest of its kind:

| Capsule | Workers |
|---|---|
| A driver and WASM tools | `driver` and `tools` |
| A driver and only runtime-provided tools, such as `bash` or `delegate-task` | `driver` |
| A driver and `capabilities.install.tool`, before it pulls a tool | `driver`; `tools` starts at the first pulled tool's first call |

`mur` starts the workers as staging begins, so their start overlaps artifact resolution. Each
worker runs in its own process group with `/` as its working directory. It moves with `mur` into
the session's cgroup scope. It dies with `mur`.

## What never enters a worker

A worker is started with an empty environment and nothing on its command line but its kind. It
refuses to start if its environment is not empty. It is not dumpable, so no other process can
read its memory or its `/proc` environment.

`mur` keeps everything a guest must never hold:

| Kept in `mur` | Why |
|---|---|
| Every credential gateway and its key | A guest's request reaches the provider only through `mur`, which attaches the key |
| Formation and door tokens | `mur` addresses a formation call and attaches the token |
| The trace and the records directory | The run's record is written by `mur` alone |
| The session's processes, the cgroup scope and the namespaces | `murmur:process` and the shell allowlist are decided in `mur` |

Every host call that reaches past the guest's own files, clocks and randomness goes to `mur` over
the worker's private connection: `wasi:http` requests, `murmur:process`, stream events and input
requests. `mur` answers it only while the call it belongs to is running on that worker. It makes
the same policy decision it would make for a guest running in `mur`.

## A lost worker

A worker that dies, or stops answering past its call's deadline and a five second grace, fails
the task in flight. The run ends with `E-RUN-065`, which names the worker, its pid, the call and
how it ended. The trace still carries `task_failed`, `task_end` and `session_end`, and
`completion.json` is written. A lost worker is not restarted.

A worker that cannot be started refuses the launch with `E-RUN-066`.

See [E-RUN-065](../reference/diagnostics.md#e-run-065) and
[E-RUN-066](../reference/diagnostics.md#e-run-066).

## Not yet under the class

A worker runs as your user, with the same reach `mur` has. The containment class does not confine
it yet. The [Routes block](../reference/containment.md#routes) of `mur run --explain-scope` says
what holds each route today.
