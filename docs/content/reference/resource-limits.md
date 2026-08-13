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
`capabilities.shell.allow` and `capabilities.spawn.allow` binaries, and native-implementation tool
artifacts. A capsule that cannot escape its containment can still wedge the host it runs on by
forking, allocating, opening files or writing without bound; this block is what stops that.

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
| `setrlimit(2)` ceilings, applied to each spawned process before it execs | `max_processes`, `max_open_files`, `max_file_size_bytes`, `cpu_seconds`, `memory_bytes` | Every platform | Set as hard limits, so a process cannot raise them from inside. A declared value above the ceiling `mur` itself inherited is clamped down to that ceiling rather than rejected. Core dumps are disabled outright, with no manifest field |
| A cgroup v2 scope around the whole subprocess tree | `cgroup_memory_bytes`, `cgroup_pids_max`, `cgroup_cpu_percent`, `cgroup_io_bytes_per_sec` | Linux only | The only bound that applies to the tree in aggregate. `RLIMIT_NPROC` is a per-user ceiling, so a fork bomb of distinct, short-lived processes evades it; a cgroup's `pids.max` does not |
| A periodic workdir-size check | `workdir_max_bytes` | Every platform | The workdir is walked every 10 seconds, and the cadence has no manifest field, so a breach is caught within one interval rather than at the moment it happens. It ends the session with `E-RUN-013` and blocks any further subprocess |

### `max_processes` is headroom, not a ceiling { #max-processes-headroom }

`RLIMIT_NPROC` counts everything the user account already owns rather than the processes in the
capsule's tree, and the unit it counts differs by platform: threads on Linux, processes on macOS.
The runtime measures the account's live count in that unit once at launch and sets the limit to
that baseline plus `max_processes`, so the field means how much a capsule's tree may add to what
the host is already using. `cgroup_pids_max` needs no such adjustment — it counts only the tasks in
the capsule's own scope, which is why it, and not `max_processes`, is the bound that stops a fork
bomb.

### Platform behavior { #platform-behavior }

**Linux.** A capsule that can spawn any native subprocess — through `capabilities.shell.allow`,
`capabilities.spawn.allow`, or a native-implementation artifact — refuses to launch with
`E-RUN-012` when the host cannot delegate a cgroup, rather than running that tree with no aggregate
ceiling. Delegation comes from systemd: `Delegate=yes` for `memory pids cpu io` on the unit `mur`
runs under. A capsule that declares no subprocess capability is never blocked. See
[Verification](containment.md#verification) for how these bounds are checked by hand.

**macOS and other non-Linux hosts.** No cgroup can exist, so the launch proceeds with rlimits alone
and [`W-SEC-010`](diagnostics.md#w-sec-010) names the residual gap: no aggregate bound across the
tree, and no per-process memory bound either, because macOS has no `RLIMIT_AS` and its kernel does
not enforce `RLIMIT_DATA`.

### Which limit a subprocess hit { #which-limit }

When a subprocess dies or fails on a resource ceiling and the kernel's own evidence names exactly
one limit, the `shell` event in `trace.jsonl` carries a `resource_limit` field:

| `resource_limit` | Evidence |
|---|---|
| `cpu_seconds` | The process was killed by `SIGXCPU` |
| `max_file_size_bytes` | The process was killed by `SIGXFSZ` |
| `cgroup_memory_bytes` | The scope's `memory.events` `oom_kill` counter moved |
| `cgroup_pids_max` | The scope's `pids.events` `max` counter moved |

Every other case is left unnamed rather than guessed at: `memory_bytes` surfaces as an allocation
failure inside the process, `max_processes` as a `fork()` failing with `EAGAIN`, and
`max_open_files` as an `open()` failing with `EMFILE` — none of which kills anything the runtime can
attribute. An absent `resource_limit` means the limit could not be identified, not that no limit
was involved.
