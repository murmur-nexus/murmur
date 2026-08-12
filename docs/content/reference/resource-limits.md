# Resource Limits

`capabilities.limits` bounds the components the runtime runs; `capabilities.resources`
bounds the operating-system processes it spawns. This page covers both blocks, their
defaults, and what each platform enforces.

---

## Execution limits

`capabilities.limits` bounds how long a component (capsule, tool/driver, or hook) may run and
how much memory it may consume — see [Execution limits](../concepts/capsules.md#execution-limits)
for the runtime-level behavior. Every field is optional and independently defaulted; omitting
the whole block is equivalent to omitting every field.

```yaml
capabilities:
  limits:
    memory_bytes: 16777216   # 16 MiB
    table_elements: 10000
    instances: 100
    deadline_seconds: 30
```

- Setting any field to `0` is rejected at manifest-parse time with `E-MAN-003`, before any WASM
  executes:

  ```
  error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.limits.memory_bytes': must be greater than zero
  ```

- A component that exceeds `deadline_seconds` fails with an `E-RUN-001` naming the deadline that
  fired; one that grows past `memory_bytes` or `table_elements` fails with an `E-RUN-001` naming
  the limit and the size it tried to reach. Both are reported distinctly from a plain crash —
  see [CLI error codes](diagnostics.md#index).

## Host resource limits

`capabilities.resources` bounds the **operating-system processes** the runtime spawns for
`capabilities.shell.allow` binaries and for native-implementation tool artifacts. It is a
different subject from `capabilities.limits` above, which bounds a WASM *component* inside the
runtime: a capsule that cannot escape its containment can still wedge its host by forking,
allocating, opening files or writing without bound. That is denial of service, not a containment
escape — nothing outside the capsule's granted scope is read, written, or reached — but the host
is wedged either way.

Every field is optional and independently defaulted; omitting the whole block is equivalent to
omitting every field. **A silent manifest means defaults, never "unlimited"** — the same rule
`capabilities.limits` already follows.

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

Three mechanisms enforce these, in descending order of portability:

- **`setrlimit(2)` ceilings**, applied to every spawned subprocess before `execve` on every
  platform. They are set as **hard** limits (`rlim_cur == rlim_max`), not soft ones: an
  unprivileged process may raise its own soft limit up to the hard one at any time, so a
  soft-only cap is advisory against a hostile capsule. From inside a capsule, `ulimit -Hn`
  reports the configured ceiling and any attempt to raise past it is refused by the kernel. A
  value above the runtime's own inherited hard limit is clamped down to it rather than rejected.
  `RLIMIT_CORE` is additionally pinned to `0` with no manifest surface at all.

    `max_processes` is the one field applied as **headroom rather than an absolute number**, and
    the reason is `RLIMIT_NPROC`'s own semantics: it counts everything already owned by the *uid*,
    not the entries in the capsule's tree — and the unit it counts differs by platform. On Linux
    it is **threads**: `setrlimit(2)` describes the limit as "the maximum number of processes (or,
    more precisely on Linux, threads) that can be created for the real user ID". On macOS, whose
    BSD-derived limit is genuinely per-process, it is **processes**. A workstation account is
    routinely several hundred processes and several *thousand* threads deep before a capsule
    starts, so a literal hard ceiling of 128 would not bound the capsule at 128 — it would make
    the subprocess's very first `fork()` fail with `EAGAIN` before the capsule did anything. The
    runtime therefore measures the uid's live count once at launch, in that platform's own unit
    (threads on Linux, processes on macOS), and sets `RLIMIT_NPROC = baseline + max_processes`.
    The Linux cgroup `pids.max` needs no such adjustment, because it counts only the tasks in the
    capsule's own scope — which is exactly why it, and not this field, is the bound that actually
    stops a fork bomb.
- **A cgroup v2 scope** around the whole subprocess tree (`cgroup_*` fields), **Linux only**.
  This is what rlimits structurally cannot do: `RLIMIT_NPROC` is a per-**uid** ceiling, so a fork
  bomb of distinct, short-lived processes evades it; `pids.max` on a cgroup does not.
- **A periodic workdir-size check** (`workdir_max_bytes`), on every platform. A breach is caught
  within one poll interval — not at the instant it happens — and terminates the session with
  `E-RUN-013`. The interval is an internal constant, not a manifest key.

Setting any field to `0` is rejected at manifest-parse time with `E-MAN-003`, before any WASM
executes:

```
error[E-MAN-003]: murmur.yaml: invalid capability config for 'capabilities.resources.max_processes': must be greater than zero
```

**Platform behavior.** On Linux, a capsule that can spawn *any* native subprocess
(`capabilities.shell.allow`, `capabilities.spawn.allow`, or a native-implementation artifact)
refuses to launch with `E-RUN-012` when the host cannot delegate a cgroup — running that tree
with no aggregate ceiling is worse than not running it. This requires systemd user delegation
(`Delegate=yes` for `memory pids cpu io` on the unit `mur` runs under); see
[Verification](containment.md#verification) for how these bounds are checked by hand. A capsule that
declares no subprocess capability at all is never blocked — there is no process tree to bound.
On macOS (and any non-Linux host) cgroups cannot exist, so the launch always proceeds with
rlimits only and `W-SEC-010` documents the residual gap: no aggregate bound, and — because macOS
has no `RLIMIT_AS` and does not enforce `RLIMIT_DATA` — no per-process memory bound either.

When a subprocess is killed for exceeding a limit and the kernel's own evidence names exactly
one, the `shell` event in `trace.jsonl` carries a `resource_limit` field naming it
(`cpu_seconds`, `max_file_size_bytes`, `cgroup_memory_bytes`, `cgroup_pids_max`). Cases the
kernel does not identify unambiguously — `RLIMIT_AS`/`RLIMIT_DATA` (surfaces as `ENOMEM` inside
the child's allocator), `RLIMIT_NPROC` and `RLIMIT_NOFILE` (fail a `fork()`/`open()` with
`EAGAIN`/`EMFILE` inside the child, killing nothing) — are deliberately left unattributed rather
than guessed at.
