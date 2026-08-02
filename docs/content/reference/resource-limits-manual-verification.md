# Resource limits — manual verification on a real Linux host

!!! danger "This procedure must be run by hand, on a real uncontainerised Linux host. Never on CI."

    Every scenario below depends on a cgroup v2 scope that CI cannot provide: the runners are
    themselves containerised, their cgroup subtree is not delegated, and the enforcement tier they
    resolve to is not the one being verified. A green CI run says nothing about any claim on this
    page — the same gap `crates/capsule-runtime/src/sandbox.rs` already documents for its
    Landlock/seccomp enforcement. The repository's own crate code lives on a `darwin/aarch64`
    machine, which structurally cannot run any of this either.

    **No automated test asserts that a fork bomb was contained, that `memory.max` was enforced, or
    that the host stayed responsive.** Those claims are true only when a person has run the
    commands below and recorded what happened. Until then, treat them as designed-for, not
    verified.

## What is being verified, and what class of problem it is

`capabilities.resources` bounds the operating-system processes the runtime spawns — the
`capabilities.shell.allow` binaries and native-implementation tool artifacts. A capsule that is
fully contained can still wedge or starve its host: unbounded forking, unbounded open files,
unbounded disk writes, unbounded per-process CPU and memory, unbounded workdir growth.

**This is denial of service, not a boundary defeat.** A capsule that exhausts host resources has
not escaped containment — nothing outside its granted scope was read, written, or reached. Report
and document it as such; do not fold it in with confirmed escape findings.

Three mechanisms, verified separately below:

| Mechanism | Scope | Platforms | Scenario |
|---|---|---|---|
| `setrlimit(2)` hard ceilings | per process | all Unix | [1](#scenario-1), [4](#scenario-4), [5](#scenario-5) |
| cgroup v2 scope | whole process tree | Linux, delegated | [2](#scenario-2), [3](#scenario-3) |
| periodic workdir-size check | session workdir | all | [6](#scenario-6) |

## Install requirement — systemd user cgroup delegation

Creating a cgroup requires write access to a cgroup directory, and the runtime has none by
default. The usual alternative — start as root, create the cgroup, drop privileges — is not
available: **nothing in this codebase ever runs as root or drops privileges**, so there is no
privileged phase to create a cgroup from. The mechanism that fits is systemd user delegation, the
same one rootless Docker and Podman use for the identical problem.

### Confirm the host is cgroup v2 unified

```bash
stat -fc %T /sys/fs/cgroup
# expected: cgroup2fs
#   (tmpfs => cgroup v1 or hybrid; `mur run` will refuse to launch a subprocess-capable capsule)

grep '^0::' /proc/self/cgroup
# expected: a single line, e.g.
#   0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-...scope
```

If `stat` reports `tmpfs`, boot the host with `systemd.unified_cgroup_hierarchy=1` (and, on older
distros, `cgroup_no_v1=all`) before continuing. Nothing below works on a v1 or hybrid host, and
that is reported as a launch refusal, not silently.

### Delegate the controllers to the user manager

```bash
mkdir -p ~/.config/systemd/user/ # (drop-in target is a *system* unit; see note)
sudo mkdir -p /etc/systemd/system/user@.service.d
sudo tee /etc/systemd/system/user@.service.d/10-murmur-delegate.conf >/dev/null <<'EOF'
[Service]
Delegate=memory pids cpu io
EOF
sudo systemctl daemon-reload
# Log the user session fully out and back in — the delegation applies at user-manager start.
```

Verify the delegation actually landed:

```bash
CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
cat "$CG/cgroup.controllers"
# expected to include: memory pids cpu io

# The decisive check — can this uid create a child cgroup here?
mkdir "$CG/murmur-delegation-probe" && rmdir "$CG/murmur-delegation-probe" && echo "delegation OK"
```

If the `mkdir` fails with `EACCES`, the delegation did not apply; the runtime performs this exact
probe at launch and refuses with `E-RUN-012` naming what is missing.

### Running `mur` under its own scope (optional, recommended)

Running the capsule under a dedicated transient scope keeps its cgroup subtree out of the shell's:

```bash
systemd-run --user --scope --property=Delegate=yes -- mur run murmur.yaml
```

### What the runtime does with the delegation

At launch, before any WASM is instantiated, if the capsule can spawn a native subprocess
(`capabilities.shell.allow`, `capabilities.spawn.allow`, or a native-implementation artifact) the
runtime:

1. reads `/proc/self/cgroup` for the `0::` line and joins it to the `cgroup2` mount from
   `/proc/mounts` to find its delegated base;
2. enables `memory`, `pids`, `cpu` (and `io`, if delegated) in that base's `cgroup.subtree_control`
   — moving *itself* into `<base>/murmur-supervisor` first if the base still holds processes,
   which cgroup v2's "no internal processes" rule requires;
3. creates `<base>/murmur-<session_id>` and writes `memory.max`, `pids.max`, `cpu.max` (fatal on
   failure) and `io.max` (best-effort, logged on failure);
4. opens that scope's `cgroup.procs` write-only, and every subprocess writes its own pid there
   from inside its `pre_exec`, before `execve`.

Inspect a live scope while a capsule runs:

```bash
CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
ls -d "$CG"/murmur-*
cat "$CG"/murmur-*/memory.max "$CG"/murmur-*/pids.max "$CG"/murmur-*/cpu.max
```

## The test capsule

Every scenario below uses this manifest. Save it as `murmur.yaml` in an empty directory. Adjust
`inference` to whatever driver the host has installed — nothing here depends on the model, only on
the capsule reaching its `bash` tool.

```yaml
name: resource-limit-probe
version: 0.1.0

capabilities:
  shell:
    allow: [bash]
  resources:
    # Deliberately tight, so each scenario trips in seconds rather than minutes.
    max_processes: 64
    max_open_files: 16
    max_file_size_bytes: 10485760      # 10 MiB
    cpu_seconds: 5
    memory_bytes: 268435456            # 256 MiB
    cgroup_memory_bytes: 268435456     # 256 MiB
    cgroup_pids_max: 32
    cgroup_cpu_percent: 50
    workdir_max_bytes: 52428800        # 50 MiB

inference:
  transport: process
  model: claude-sonnet-4-5
  max_turns: 4
```

A second manifest, `murmur-defaults.yaml`, identical but with the whole `resources:` block
**deleted** — used by [scenario 0](#scenario-0) to prove defaults apply to a silent manifest.

Before each scenario, in another terminal, keep an eye on the host:

```bash
vmstat 1 & top -b -n1 | head -5
```

The host staying responsive throughout is part of every expected result, and is the whole point.

---

## Scenario 0 — a silent manifest gets defaults, not "unlimited" { #scenario-0 }

```bash
mur run murmur-defaults.yaml --task 'run this shell command and report its exact output: ulimit -Hn; ulimit -Hu; ulimit -Ht'
```

**Expected:** `1024` (`max_open_files`), `3600` (`cpu_seconds`), and for `ulimit -Hu` **the
uid's current process count plus 128** — not a bare `128`. `RLIMIT_NPROC` is per-uid, so
`max_processes` is applied as headroom above the runtime's own baseline; a literal `128` on an
account already running more than that would make the subprocess's first `fork()` fail. Compare
against `ps -u "$(id -un)" | wc -l` on the same host.

All three come from a manifest that declares no `resources:` block at all.

**Failure to watch for:** `unlimited` on any line. That would mean the defaulting path was skipped
and the subprocess is unbounded — the exact failure mode `capabilities.limits` already guards
against and this block must match.

Confirm the cgroup scope exists for this run too:

```bash
CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
cat "$CG"/murmur-*/pids.max     # expected: 256 (default)
cat "$CG"/murmur-*/memory.max   # expected: 4294967296 (default, 4 GiB)
```

---

## Scenario 1 — limits are hard, and a declared value overrides the default { #scenario-1 }

The one property that separates a real bound from an advisory one: `setrlimit(2)` lets an
unprivileged process raise its soft limit up to its hard limit at any time, so a soft-only cap is
undone by one call from inside the capsule.

```bash
mur run murmur.yaml --task 'run this shell command and report its exact output verbatim:
  echo "hard=$(ulimit -Hn) soft=$(ulimit -Sn)"; ulimit -n 4096 2>&1 || echo "raise refused"'
```

**Expected:**

```
hard=16 soft=16
bash: ulimit: open files: cannot modify limit: Operation not permitted
raise refused
```

`hard=16` proves the declared `max_open_files: 16` was applied (not the 1024 default), and
`hard == soft` with a refused raise proves `rlim_max` was set, not only `rlim_cur`. If `hard` were
`unlimited` while `soft` were `16`, every bound on this page would be advisory.

Now confirm the limit actually bites, at the declared value and not the default:

```bash
mur run murmur.yaml --task 'run this shell command and report its exact output:
  bash -c "for i in \$(seq 1 40); do exec {fd}<>/tmp/fd-probe-\$i || { echo \"EMFILE at \$i\"; break; }; done"'
```

**Expected:** the failure appears in the teens (descriptor 17 or thereabouts once bash's own
stdin/stdout/stderr and internal descriptors are counted), **never** in the 1000s.

**Attribution note:** `resource_limit` is *not* set in the trace for this case, deliberately.
`RLIMIT_NOFILE` fails an `open()` inside the child with `EMFILE`; it kills nothing, so there is no
parent-visible signal to attribute. The evidence is the child's own stderr, which is where the
runtime leaves it rather than guessing.

---

## Scenario 2 — fork bomb: stopped by the cgroup, not by `RLIMIT_NPROC` { #scenario-2 }

This is the scenario that justifies cgroups existing in this slice at all. `RLIMIT_NPROC` is a
per-**uid** ceiling; a tree of distinct, rapidly-forking, short-lived processes evades it in
practice even when set correctly. `pids.max` is per-cgroup and does not.

```bash
mur run murmur.yaml --task 'run this shell command: :(){ :|:& };:'
```

Or, if a named function is easier to get past prompt handling:

```bash
mur run murmur.yaml --task 'run this shell command: bomb() { bomb | bomb & }; bomb'
```

**Expected:**

- the host stays responsive throughout — no login delay, no OOM, no need for a reset;
- the shell reports `fork: retry: Resource temporarily unavailable` (`EAGAIN`) — note this is
  the cgroup's `pids.max` refusing the fork, not `RLIMIT_NPROC`, which sits at the uid baseline
  plus `max_processes` and is the looser of the two by construction;
- `mur trace show` names the limit:

```bash
mur trace show --session <session_id> | grep -i resource_limit
# expected: "resource_limit": "cgroup_pids_max"
```

- and the kernel's own counter agrees, read from the live scope:

```bash
CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
cat "$CG"/murmur-*/pids.events
# expected: max <nonzero>
```

**The specific claim being verified:** the trace says `cgroup_pids_max`, not a generic failure and
not `max_processes`. If it says `max_processes`, the attribution is wrong — `RLIMIT_NPROC` does
not kill anything and must never be reported as a kill cause.

**If the host does wedge:** record it. That is the finding, and it means either the scope was not
created (check `E-RUN-012` did not fire and the scope directory exists) or the subprocess did not
join it (check `cgroup.procs` inside the scope while a subprocess runs).

---

## Scenario 3 — memory hog: OOM-killed inside the capsule's own cgroup { #scenario-3 }

The point is *where* the kill happens: inside the capsule's scope, before host-wide memory
pressure forces the system OOM killer to pick an arbitrary victim.

```bash
mur run murmur.yaml --task 'run this shell command:
  python3 -c "b=[]
while True: b.append(bytearray(50*1024*1024))"'
```

If the capsule has no Python, use `bash` alone:

```bash
mur run murmur.yaml --task 'run this shell command: s=""; while :; do s="$s$(head -c 10000000 /dev/zero | tr "\0" "x")"; done'
```

**Expected:**

- the process is killed at ~256 MiB (the declared `cgroup_memory_bytes`), not at host exhaustion;
- nothing *else* on the host is killed — check `dmesg -T | tail -20` and confirm any
  `Memory cgroup out of memory` line names the murmur scope, and that no unrelated process appears;
- the trace names the limit:

```bash
mur trace show --session <session_id> | grep -i resource_limit
# expected: "resource_limit": "cgroup_memory_bytes"

CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
cat "$CG"/murmur-*/memory.events
# expected: oom_kill <nonzero>
```

**Attribution note:** the per-process `memory_bytes` (`RLIMIT_AS`) is *not* attributed, ever. An
`RLIMIT_AS` overrun surfaces as `ENOMEM` inside the child's own allocator — the child may die of
`SIGSEGV`, `SIGABRT`, or handle it and carry on — and none of those identify the cause. Only the
cgroup counter is evidence, which is why only the cgroup case is named.

---

## Scenario 4 — CPU hog: the one unambiguous rlimit signal { #scenario-4 }

```bash
time mur run murmur.yaml --task 'run this shell command: while :; do :; done'
```

**Expected:**

- the subprocess dies after ~5 CPU-seconds (`cpu_seconds: 5`), not after the agent's wall-clock
  deadline;
- the trace names the limit:

```bash
mur trace show --session <session_id> | grep -i resource_limit
# expected: "resource_limit": "cpu_seconds"
```

`SIGXCPU` is the one rlimit-triggered signal that maps to exactly one cause, which is why this
attribution is safe to make. The shell event's `exit_code` is `128 + 24 = 152` (or `128 + 9 = 137`
if the process ignored `SIGXCPU` and the kernel followed with `SIGKILL`).

---

## Scenario 5 — disk filler, per-file: `RLIMIT_FSIZE` { #scenario-5 }

```bash
mur run murmur.yaml --task 'run this shell command: dd if=/dev/zero of=./big bs=1M count=200'
```

**Expected:**

- `dd` dies at 10 MiB (`max_file_size_bytes: 10485760`) with `File size limit exceeded`;
- the trace names the limit:

```bash
mur trace show --session <session_id> | grep -i resource_limit
# expected: "resource_limit": "max_file_size_bytes"
```

`SIGXFSZ` is the second and last unambiguous rlimit signal.

---

## Scenario 6 — disk filler, aggregate: the periodic workdir check { #scenario-6 }

`RLIMIT_FSIZE` bounds one file; nothing in it stops a thousand files just under the ceiling. That
is what `workdir_max_bytes` covers.

```bash
mur run murmur.yaml --task 'run this shell command:
  for i in $(seq 1 200); do dd if=/dev/zero of=./fill-$i bs=1M count=9 2>/dev/null; done'
```

**Expected, and note the timing carefully:**

- the breach is caught **within one poll interval of 10 seconds** of the workdir crossing 50 MiB —
  not instantaneously, and not never. The workdir can legitimately overshoot the ceiling by
  whatever is written inside one interval;
- once latched, the *next* subprocess spawn is refused outright, so the filler stops writing at
  the first spawn boundary after the breach;
- the session terminates with `E-RUN-013` naming both numbers:

```
error[E-RUN-013]: session workdir grew to 57671680 bytes, past the 52428800 byte ceiling
```

- `workdir/<session_id>/logs/bootstrap.log` carries the same wording from the watcher thread.

**Scope note, stated so it is not read as a missed requirement.** This slice ships the *periodic
check*, not a structurally-bounded (tmpfs-backed, size-mounted) workdir. The structural version
needs a mount namespace this runtime cannot create — nothing here runs as root, and the
`sealed-containment-runtime` work that would provide one is a separate, unbuilt roadmap card whose
own text acknowledges this dependency. The one-interval detection lag is the honest cost of the
mechanism that *is* available, and it is stated rather than hidden.

---

## Scenario 7 — fail-closed launch refusal { #scenario-7 }

Verify the Linux refusal is real, and that it is correctly scoped.

**7a — subprocess-capable capsule, no delegation → refuse.** Temporarily remove the drop-in from
[the install section](#install-requirement-systemd-user-cgroup-delegation) (or run under a unit
without `Delegate=`), then:

```bash
mur run murmur.yaml --task 'echo hello'
```

**Expected:** the launch fails **before any WASM is instantiated** and before any subprocess is
spawned:

```
error[E-RUN-012]: this capsule can spawn native subprocesses but no cgroup v2 scope could be
created to bound them: cannot create a child cgroup under /sys/fs/cgroup/... ; the unit `mur`
runs under needs `Delegate=yes` for memory, pids, cpu and io
```

Confirm no shell subprocess ran at all — `workdir/<session_id>/trace.jsonl` must contain no
`shell` event.

**7b — WASM-only capsule, no delegation → launch proceeds.** Delete the whole `capabilities:`
block (or leave only `network:`), keeping delegation removed:

```bash
mur run murmur-wasm-only.yaml --task 'echo hello'
```

**Expected:** the capsule runs normally. There is no process tree to bound, so requiring host
configuration here would be a disproportionate regression for WASM-only capsules. If this refuses
to launch, the condition is scoped too broadly.

Restore the delegation drop-in afterwards.

---

## Scenario 8 — native-implementation tool artifacts are bounded too { #scenario-8 }

Before this slice, `dispatch_native_tool` spawned with no `pre_exec` hook of any kind, so a native
artifact ran completely unbounded while `capabilities.shell.allow` subprocesses did not.

With a native tool artifact installed (e.g. `murmur-tool-git`), run any task that invokes it and,
while it runs, confirm it is inside the scope:

```bash
CG=/sys/fs/cgroup$(grep '^0::' /proc/self/cgroup | cut -d: -f3)
cat "$CG"/murmur-*/cgroup.procs        # the native tool's pid must appear here
grep -E '^Max (open files|processes)' /proc/<pid>/limits
# expected: the configured ceilings in BOTH the soft and hard columns
```

**Known and deliberate gap:** this path still installs no seccomp filter and no Landlock scope.
That asymmetry is pre-existing and separately tracked; this slice closed only the rlimit/cgroup
half of it. Do not record the missing seccomp/Landlock here as a regression.

---

## Scenario 9 — macOS: rlimits apply, cgroups do not, and it says so { #scenario-9 }

Run on the macOS development machine, not Linux:

```bash
mur run murmur.yaml --task 'run this shell command: ulimit -Hn'
```

**Expected:**

- the launch **succeeds** — a missing cgroup is never a launch refusal off Linux, because it is a
  property of the platform rather than a host misconfiguration;
- `ulimit -Hn` reports `16`, so the per-process rlimits are genuinely applied;
- stderr and `logs/bootstrap.log` both carry `W-SEC-010`, stating that no aggregate bound exists
  and that `RLIMIT_NPROC` alone does not stop a fork bomb of distinct short-lived processes.

**Do not run scenario 2 on macOS expecting containment.** It will not be contained, and that is
the documented, permanent gap `W-SEC-010` exists to state — see
[Security Warnings](security-warnings.md#w-sec-010).

---

## Recording the result

When this procedure has actually been run, record — here, in this file — the host (distro, kernel
version, systemd version), the date, and per scenario: what happened, the exact `resource_limit`
string the trace carried, and whether the host stayed responsive. Until that edit lands, every
claim on this page is a design intent, not a verified result, and the "not yet verified" framing
in [Security Warnings](security-warnings.md) stands as written.

Scenarios 1, 2, 3, 5 and 6 are also driven automatically by the maintainer conformance procedure
described in [Security Warnings](security-warnings.md#w-sec-005), where resource exhaustion is
reported as **its own category** — denial of service — and is never merged into the escape
findings. That is not a substitute for running them here: it grades each scenario from a single
automated signal, while the scenarios above also ask a person to watch the host stay responsive and
to read `dmesg`, `pids.events` and `memory.events` directly.

One finding from that work is worth carrying here, because it changes what a manifest should
declare: on the Linux Full tier the rlimits are applied *before* the seccomp filter is installed,
and installing the filter needs file descriptors of its own. A `max_open_files` in the low tens —
including the `16` declared by [the test capsule](#the-test-capsule) above — makes **every**
subprocess spawn fail outright with a libseccomp setup error rather than merely capping descriptors.
Use a ceiling with headroom (64 was the lowest value observed to both spawn and still bite) when the
intent is to bound a real workload rather than to demonstrate the limit.
