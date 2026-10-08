# Resource Limits

`capabilities.limits` bounds the components the runtime runs; `capabilities.resources` bounds the
operating-system processes it spawns. Field types, defaults and validation for both blocks are in
the [manifest reference](manifest.md#field-capabilities); this page covers what enforces them, what
happens when one is crossed, and what each platform can enforce.

Both blocks are optional field by field: an omitted field takes its default, and an omitted block
is the same as omitting every field in it. A silent manifest means defaults, never "unlimited". A
field declared as `0` is rejected when the manifest is parsed, before any component runs:

```
error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.limits.memory_bytes': must be greater than zero
```

---

## Execution limits { #execution-limits }

`capabilities.limits` bounds every component call — a capsule `run`, a tool or driver `run`, and
each hook lifecycle call — in wall-clock time and in the memory, table space and instances it may
take. See [Execution limits](../concepts/capsules.md#execution-limits) for what a deadline does and
does not bound.

```yaml
capabilities:
  limits:
    memory_bytes: 16777216   # 16 MiB
    table_elements: 10000
    instances: 100
    deadline_seconds: 30
```

| Field crossed | What happens |
|---|---|
| `deadline_seconds` | The call is interrupted and fails with `E-RUN-001` naming the deadline that fired |
| `memory_bytes`, `table_elements` | The growth is refused and the call fails with `E-RUN-001` naming the limit and the size it tried to reach |

Both are reported distinctly from a plain crash — see [CLI error codes](diagnostics.md#index).

## Host resource limits { #host-resource-limits }

`capabilities.resources` bounds the operating-system processes the runtime spawns:
`capabilities.shell.allow` binaries, `capabilities.spawn.allow` sub-capsules, and
native-implementation tool artifacts. A capsule that cannot escape its containment can still
wedge the host it runs on by forking, allocating, opening files or writing without bound; this
block is what stops that.

```yaml
capabilities:
  shell:
    allow: [bash]
  resources:
    max_processes: 32
    max_open_files: 64
    cpu_seconds: 60
    cgroup_pids_max: 64
    workdir_max_bytes: 1073741824   # 1 GiB
```

Three mechanisms enforce the block, in descending order of portability:

| Mechanism | Fields | Platforms | Notes |
|---|---|---|---|
| `setrlimit(2)` ceilings, applied to each spawned process before it execs | `max_processes`, `max_open_files`, `max_file_size_bytes`, `cpu_seconds`, `memory_bytes` | Every platform | Set as hard limits, so a process cannot raise them from inside. A declared value above the ceiling `mur` itself inherited is clamped down to that ceiling rather than rejected. Core dumps are disabled outright, with no manifest field. `max_processes` applies only to a process with no cgroup scope; see [`max_processes` by platform](#max-processes-headroom) |
| A cgroup v2 scope around the whole subprocess tree | `cgroup_memory_bytes`, `cgroup_pids_max`, `cgroup_cpu_percent`, `cgroup_io_bytes_per_sec` | Linux only | The only bound that applies to the tree in aggregate. `cgroup_memory_bytes` is the scope's `memory.max`, and its `memory.swap.max` is `0`, so the ceiling bounds the tree's whole footprint rather than only its resident memory. On a kernel built without swap support, or booted with swap accounting off, a scope has no `memory.swap.max`: the launch proceeds with swap unbounded, and [`mur doctor`](cli.md#doctor-memory-bound) reports it. `cgroup_pids_max` is the process bound for a subprocess in a scope: it counts only the capsule's own tree, where `RLIMIT_NPROC` counts every thread the user account owns on the host |
| A periodic workdir-size check | `workdir_max_bytes` | Every platform | The workdir is walked every 10 seconds, and the cadence has no manifest field, so a breach is caught within one interval rather than at the moment it happens. It ends the session with `E-RUN-013` and blocks any further subprocess |

### `max_processes` by platform { #max-processes-headroom }

`RLIMIT_NPROC` counts everything the user account owns on the host rather than the processes in the
capsule's tree, so `max_processes` is headroom above that count, and only where no cgroup scope
bounds the tree:

| Subprocess | Process bound | Effect of `max_processes` |
|---|---|---|
| Linux, in a cgroup scope (every capsule that can spawn a subprocess) | `cgroup_pids_max`, counted over the capsule's own tree | None. The subprocess keeps the `RLIMIT_NPROC` that `mur` inherited |
| Linux, with no cgroup scope | `RLIMIT_NPROC` | The account's thread count at that spawn, plus `max_processes` |
| macOS | `RLIMIT_NPROC` | The account's process count at that spawn, plus `max_processes` |

The count is taken at each spawn, so threads and processes the rest of the host starts during a
session do not use up the capsule's headroom.

### Platform behavior { #platform-behavior }

**Linux.** A capsule that can spawn any native subprocess — through `capabilities.shell.allow`,
`capabilities.spawn.allow`, or a native-implementation artifact — refuses to launch with
`E-RUN-012` when the host cannot delegate a cgroup, rather than running that tree with no aggregate
ceiling. On launch, `mur` asks the systemd user session for a fresh delegated scope of its own —
the same mechanism rootless Podman and Docker use — so it works from an ordinary shell without any
setup. When no systemd user session is reachable, it falls back to whatever cgroup it already
inherited, which must itself carry `Delegate=yes` for `memory pids cpu io` (for example, by running
under a unit configured that way, or via `systemd-run --user --scope --property=Delegate=yes`). A
capsule that declares no subprocess capability is never blocked. See
[Verification](containment.md#verification) for how these bounds are checked by hand.

**macOS and other non-Linux hosts.** No cgroup can exist, so the launch proceeds with rlimits alone
and [`W-SEC-010`](diagnostics.md#w-sec-010) names the residual gap: no aggregate bound across the
tree, and no per-process memory bound either, because macOS has no `RLIMIT_AS` and its kernel does
not enforce `RLIMIT_DATA`.

### Whether the I/O ceiling applied { #io-max-report }

`cgroup_io_bytes_per_sec` is the one cgroup limit whose failure does not refuse a launch.
`memory.max`, `pids.max` and `cpu.max`, and `memory.swap.max` wherever the kernel has it, are
settable on any cgroup v2 host once the controllers are delegated, so a failure there means the
bound genuinely does not exist and the session must not start. `io.max` names a block device by
`MAJ:MIN`, and a filesystem with no block device behind it — tmpfs, overlayfs, FUSE and network
mounts — has none to name. A capsule that saturates disk
bandwidth is slow; one that exhausts memory or pids is fatal.

Every session reports what became of the ceiling, in an `io_max` object carried by
`mur run --explain-scope --json` and by `session_start.effective_grants` in `trace.jsonl`:

| Field | Type | Meaning |
|---|---|---|
| `declared_bytes_per_sec` | integer | The effective `capabilities.resources.cgroup_io_bytes_per_sec`, after the default is applied. Present whatever the status |
| `status` | string | One of the four below |
| `reason` | string | Why, for every status but `enforced`. Absent when there is nothing to say |

| `status` | Means |
|---|---|
| `enforced` | The `io.max` write succeeded against the device backing the workdir. The ceiling is on the scope |
| `unavailable` | A scope exists and the write did not succeed. `memory.max`, `pids.max` and `cpu.max` are still enforced on it; I/O bandwidth is not bounded. [`W-SEC-021`](diagnostics.md#w-sec-021) reports it |
| `not-required` | No scope was asked for: the capsule can reach no native subprocess, or this is not Linux |
| `not-probed` | The write was never attempted, so nothing is claimed either way |

`mur run --explain-scope` reports the status a launch would reach: it creates a throwaway cgroup,
performs the same write, and removes the directory again.

The device that write names is resolved in three steps, because neither of the two numbers closest
to hand is one the block layer accepts:

| Step | Read from | Why |
|---|---|---|
| The filesystem behind the workdir | `st_dev` of the nearest existing ancestor of the path | Under `--explain-scope` the workdir does not exist yet, and a launch's workdir sits on the same filesystem as the project directory |
| The device it was mounted from | `/proc/self/mountinfo` | btrfs, overlayfs, tmpfs and every FUSE mount are given an anonymous device number, which names no device |
| The whole disk carrying that device | `/sys/dev/block/MAJ:MIN` | An `io.max` entry binds to a request queue and only a whole disk carries one, so the kernel refuses a partition such as `/dev/nvme0n1p7` |

A filesystem that survives all three has a ceiling written against it; one that reaches the second
step with a source such as `tmpfs` has no device, and that is what `unavailable` reports.

To see both statuses on one host, run the same capsule from a project on a tmpfs and from one on a
block-device-backed filesystem:

```console
$ mkdir -p /dev/shm/io-demo && cp murmur.yaml /dev/shm/io-demo/
$ mur run --manifest /dev/shm/io-demo/murmur.yaml --explain-scope --json | jq .io_max.status
"unavailable"
$ mur run --manifest ./murmur.yaml --explain-scope --json | jq .io_max.status
"enforced"
```

### Which limit a subprocess hit { #which-limit }

When a subprocess dies or fails on a resource ceiling and the kernel's own evidence names exactly
one limit, the `shell` event in `trace.jsonl` carries a `resource_limit` field:

| `resource_limit` | Evidence |
|---|---|
| `cpu_seconds` | The process was killed by `SIGXCPU` |
| `max_file_size_bytes` | The process was killed by `SIGXFSZ` |
| `cgroup_memory_bytes` | The scope's `memory.events` `oom_kill` counter moved, and no other shell command or native tool ran in the scope while this one did |
| `cgroup_pids_max` | The scope's `pids.events` `max` counter moved, and no other shell command or native tool ran in the scope while this one did |

Every other case is left unnamed rather than guessed at: `memory_bytes` surfaces as an allocation
failure inside the process, `max_processes`, where it applies, as a `fork()` failing with
`EAGAIN`, and `max_open_files` as an `open()` failing with `EMFILE` — none of which kills anything
the runtime can attribute. An absent `resource_limit` means the limit could not be identified, not that no limit
was involved.

The model reads the same attribution as a `resource_limit:` line that names the field and what to
do about it:

| How the command ended | Where the line appears |
|---|---|
| Finished within the shell call | After the fenced tool output, on its own line |
| Moved to the background | In the completion message for that command |

For `cgroup_pids_max` the line reads:

```text
resource_limit: cgroup_pids_max — the capsule reached its process limit while this command ran, so starting a new process failed (EAGAIN). Retry with fewer parallel processes, or ask the operator to raise capabilities.resources.cgroup_pids_max.
```

The tool result's status is the same with or without the line.
