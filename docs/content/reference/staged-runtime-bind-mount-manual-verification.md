# Verification — staged runtime bind mount

!!! warning "Status: **NOT RUN.** Part A is runnable today; Part B is blocked."

    **Part A** (the bind-mount-vs-copy cost measurement) needs nothing this slice did not ship and
    can be run the day it merges. It has **not** been run — [Recording the
    result](#recording-the-result) has empty slots waiting for two numbers.

    **Part B** (the end-to-end, two-host check) is **blocked** until the staged-runtime grant is
    actually wired into the composed-root construction. The schema, the validation, the
    `--explain-scope` reporting and the `E-CAP-004` refusal all ship and work; the bind mount into
    a live capsule root does not happen yet. See [What is and is not wired
    today](#what-is-and-is-not-wired-today) before running anything in Part B and concluding
    something from it.

## What this verifies

A capsule declaring `capabilities.shell.staged_runtime` gets its pinned interpreter tree
bind-mounted read-only into its composed root, at the same absolute path it has on the host — so
the capsule runs a runtime that is *inside* its root, rather than reaching *outside* the root
through an `interpreter_runtime` Landlock widening.

Two independent claims, verified separately because they become checkable at different times:

* **Part A — cost.** Staging by bind mount is effectively free compared to copying the tree. This
  is what makes the mechanism viable per-session for a 285 MB conda environment, and it is a
  property of the kernel, not of this codebase — so it is measurable today.
* **Part B — behaviour.** The same declared `pin` on two different hosts yields the same
  interpreter version inside the capsule, with no `interpreter_runtime` grant declared anywhere.

## What this deliberately is not

**This procedure is not automated, and must not be wired into CI in any form** — no `#[test]`, no
`#[ignore]` marker intended for an automated runner, no workflow step. The same reasoning as the
[sealed containment procedure](sealed-containment-manual-verification.md#what-this-deliberately-is-not)
applies: CI runs in containers that cannot create the namespaces involved, so an automated
assertion there would pass vacuously or skip, turning a green run into false evidence about a
property it never touched.

**A green `cargo test` is not evidence for anything on this page.** The automated tests this slice
added cover exactly two things, and neither is the property above:

1. **Manifest parsing and validation**, on any OS — the accepted shape, and the four rejections
   (binary not in `shell.allow`; binary also carrying an `interpreter_runtime` grant; a relative
   `source_path`; a missing or empty `pin`). Plus the `CapabilityPolicy` / `ScopeReport` lowering
   and the `E-CAP-004` floor check.
2. **The standalone bind-mount helper's own correctness**, on a real Linux host —
   `capsule_runtime::staged_runtime::bind_mount_staged_runtimes` really bind-mounts a throwaway
   directory tree read-only inside a private mount namespace the test creates, and confirms reads
   through the target succeed while writes fail with `EROFS`. It runs against a temp directory,
   not a capsule root, and never calls `pivot_root`.

Neither says anything about a real capsule staging a real interpreter, because — see below — that
path does not exist yet.

## What is and is not wired today

Read this before Part B.

| Piece | State |
|---|---|
| `capabilities.shell.staged_runtime` schema + parse validation | **shipped** |
| `CapabilityPolicy.shell_staged_runtime` / `ScopeReport.staged_runtime_grants` | **shipped** |
| `mur run --explain-scope` reporting the grant on any host and any floor | **shipped** |
| `E-CAP-004` refusal when the declared floor is below `sealed` | **shipped** |
| `mur doctor` warning ahead of a run | **shipped** |
| `staged_runtime::bind_mount_staged_runtimes`, proven in its own mount namespace | **shipped, and called from nowhere** |
| The composed root actually carrying the staged tree | **NOT wired** |

The composed-root mechanism itself (`capsule-runtime/src/sealed.rs`) does exist and is live — a
`sealed` capsule really does run inside a private mount namespace pivoted onto a composed root. What
is missing is only the step that feeds a declared `staged_runtime` grant into that root's plan. See
the build summary for `efc0d7db` for the exact seam
(`sealed::plan_composed_root`'s `extra_read_only` parameter) and why the wiring was left separate.

**Consequence for Part B:** a `sealed` capsule declaring `staged_runtime` today launches
successfully and simply does not have the tree mounted. Running Part B now would show a missing
interpreter, which is the expected outcome of an unwired mechanism — not a bug to chase, and not a
result to record on this page.

## Part A — bind mount vs. copy cost

Runnable today. Nothing here involves Murmur, a capsule, or a composed root: it measures the kernel
operation the mechanism is built on.

### The reference tree

The roadmap's reference case is an SWE-bench testbed conda environment:

* **285 MB** total
* **151 MB** of it `lib/python3.9`
* **9,194 files**

Any real interpreter tree of comparable size and file count is an acceptable substitute; record
which one was actually used, since the file count matters more than the byte count for the copy
side.

### Host prerequisites

* Linux with unprivileged user namespaces (`/proc/sys/kernel/unprivileged_userns_clone` = `1`, or
  run as root). Check with:

  ```bash
  unshare --user --mount true && echo "namespaces OK"
  ```

* A real interpreter tree at a known path. Set it once:

  ```bash
  export RUNTIME_TREE=/opt/testbed/conda/envs/django__django
  ```

### Step A0 — record what is being measured

```bash
du -sh   "$RUNTIME_TREE"
du -sh   "$RUNTIME_TREE/lib/python3.9"
find     "$RUNTIME_TREE" -type f | wc -l
findmnt -no FSTYPE --target "$RUNTIME_TREE"
```

Record all four. The filesystem type matters: a copy onto `overlayfs` or a network mount behaves
very differently from one onto `ext4`, and a reader comparing two runs needs to know which was
measured.

### Step A1 — time the bind mount

The mount happens inside a throwaway namespace, so nothing leaks into the host mount table and no
cleanup is needed — the namespace is destroyed when the shell exits.

```bash
mkdir -p /tmp/stage-bench-root

unshare --user --map-root-user --mount bash -c '
  mkdir -p "/tmp/stage-bench-root$RUNTIME_TREE"
  time {
    mount --bind "$RUNTIME_TREE" "/tmp/stage-bench-root$RUNTIME_TREE"
    mount -o remount,bind,ro,nosuid "/tmp/stage-bench-root$RUNTIME_TREE"
  }
  # Prove it actually mounted read-only rather than timing a no-op.
  ls "/tmp/stage-bench-root$RUNTIME_TREE/bin/python3" >/dev/null && echo "read: OK"
  touch "/tmp/stage-bench-root$RUNTIME_TREE/probe" 2>&1 | grep -q "Read-only" \
    && echo "write refused: OK"
'
```

Record the `real` time. **Also record the two `OK` lines** — a bind mount that silently failed
would time as ~0.000s and look like a spectacular result.

This is the same pair of calls `bind_mount_staged_runtimes` issues, in the same order, and for the
same reason: a single `mount -o bind,ro` does *not* produce a read-only bind; the flag only takes
effect on the follow-up `remount,bind`.

### Step A2 — time the recursive copy

```bash
rm -rf /tmp/stage-bench-copy && mkdir -p /tmp/stage-bench-copy
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null   # cold cache; omit if no sudo
time cp -a "$RUNTIME_TREE" /tmp/stage-bench-copy/
du -sh /tmp/stage-bench-copy
rm -rf /tmp/stage-bench-copy
```

Record the `real` time and whether the cache was dropped. A warm-cache copy of a tree that was just
`du`'d is not the number this comparison is about — the honest case is a cold copy, since a real
session stages a tree nothing has touched.

### Step A3 — record the ratio

Both numbers go in [Recording the result](#recording-the-result), with the ratio. The expectation
is that the bind mount is constant-time and cache-independent (single-digit milliseconds regardless
of tree size) while the copy scales with file count into the tens of seconds. **If the measured
ratio is not dramatic, that is the finding** — record it rather than re-running until it looks
right, because it would mean the mechanism's central premise does not hold on this filesystem.

## Part B — end-to-end, two hosts

!!! danger "Blocked — do not run for a result yet"

    Requires the staged-runtime grant to be wired into the composed-root plan. See [What is and is
    not wired today](#what-is-and-is-not-wired-today). Written now so the wiring slice inherits a
    ready procedure rather than an empty page.

### Prerequisites

* **Two different real Linux hosts**, both reaching `sealed`. Confirm on each *before* starting:

  ```bash
  mur run --explain-scope | head -5     # expect: achieved: sealed / floor met: yes
  ```

  Different distributions or kernel versions make the check stronger, not weaker — the point is
  that the capsule's interpreter does not vary with the host's.

* The **same** pinned runtime tree present at the **same absolute path** on both, with a recorded
  provenance (image digest, tarball checksum, or conda lockfile). "Both hosts have a Python 3.9" is
  not the precondition; "both hosts have *this* tree" is.

### The capsule

```yaml
name: staged-runtime-check
version: 0.1.0
capabilities:
  containment: sealed
  shell:
    allow:
      - python3
    staged_runtime:
      - binary: python3
        source_path: /opt/testbed/conda/envs/django__django
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01
```

Note what is **absent**: no `interpreter_runtime` block anywhere. That absence is half the claim.

### Step B1 — the declaration is visible on both hosts

On each host:

```bash
mur run --explain-scope
```

Expect, identically on both:

```text
  interpreter runtime: <none>
  staged runtime:
    - python3: /opt/testbed/conda/envs/django__django (pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01)
```

### Step B2 — the interpreter is identical inside the capsule

Run the capsule on each host and, through a real `bash` tool call in a live session, execute:

```bash
python3 -VV
python3 -c 'import sys; print(sys.executable); print(sys.prefix)'
python3 -c 'import sys; print(len(sys.path))'
```

Expect `python3 -VV` — which includes the build date and compiler, not just the version number — to
match **byte for byte** across the two hosts, and `sys.executable` / `sys.prefix` to be the
declared `source_path`, not a host location.

### Step B3 — the tree is read-only inside the capsule

Through the same session:

```bash
touch /opt/testbed/conda/envs/django__django/probe
```

Expect a read-only filesystem error. A success here means the tree was staged writable, which would
let one session mutate the interpreter every later session uses.

### Step B4 — the host outside the root is still absent

The staged tree must not have punched a hole in the composed root:

```bash
stat /etc/shadow
ls /home
```

Expect `No such file or directory` — not `Permission denied` — exactly as in the
[sealed procedure](sealed-containment-manual-verification.md#3a-the-fixed-outside-the-root-target-list).

### Step B5 — no `interpreter_runtime` was needed

Confirm from the session's `trace.jsonl` that `session_start` records
`"containment_achieved":"sealed"`, and re-confirm the manifest declares no `interpreter_runtime`
and that no `W-SEC-009` warning was emitted on either host. `W-SEC-009` firing would mean an
`interpreter_runtime` grant was still in play and the run does not demonstrate what it claims.

## Recording the result

Fill in on the run. Leave the "not run" markers in place until then — an unfilled table is a
correct record of an unperformed check.

### Part A — cost

| Field | Value |
|---|---|
| Date | _not run_ |
| Host / kernel | _not run_ |
| Tree path | _not run_ |
| Tree size (total / `lib/pythonX.Y`) | _not run_ |
| File count | _not run_ |
| Backing filesystem | _not run_ |
| Cache state for the copy (cold/warm) | _not run_ |
| **Bind mount `real`** | _not run_ |
| **Recursive copy `real`** | _not run_ |
| Ratio | _not run_ |
| Both `OK` probes observed in A1? | _not run_ |

### Part B — end to end

| Field | Host 1 | Host 2 |
|---|---|---|
| Date | _blocked_ | _blocked_ |
| Distro / kernel | _blocked_ | _blocked_ |
| `achieved` class | _blocked_ | _blocked_ |
| `python3 -VV` output | _blocked_ | _blocked_ |
| `sys.executable` / `sys.prefix` | _blocked_ | _blocked_ |
| B3 write refused? | _blocked_ | _blocked_ |
| B4 `ENOENT` (not `EACCES`)? | _blocked_ | _blocked_ |
| `W-SEC-009` absent? | _blocked_ | _blocked_ |

## Residuals recorded here rather than buried

* **The helper is proven, the wiring is not.** `bind_mount_staged_runtimes` has a real
  mount-namespace test that passes on a real Linux host. That test proves the *operation* is
  correct; it proves nothing about a capsule, because nothing calls the function yet. Do not cite
  it as evidence for Part B.
* **`pin` is unverified by construction.** Nothing compares `pin` against the tree at
  `source_path` — not a hash, not a version probe, not an existence check. Two hosts could declare
  the same pin over genuinely different trees and Murmur would not notice. Step B2 exists precisely
  because the pin is a human claim; it is the check, not the guarantee.
* **`source_path` existence is not validated at parse time.** A manifest naming a tree absent from
  the launch host parses cleanly, by design: manifests must stay parseable on machines that will
  never run them (`mur build` on a laptop for a Linux fleet). The failure surfaces when the mount is
  attempted — which, today, is never.
* **Part A measures the kernel, not Murmur.** Its numbers stay valid across refactors of this
  codebase and should not be re-run for a Murmur-side change unless the mount flags themselves
  change.
