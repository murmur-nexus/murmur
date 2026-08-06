# Verification — staged runtime bind mount

!!! success "Status: **RUN — 2026-08-06, single host.** Part A run during efc0d7db's review; Part B run on one host during 309a3184's review."

    **Part A** (the bind-mount-vs-copy cost measurement) needs nothing this slice did not ship and
    was run against a substitute tree (see [Recording the result](#recording-the-result) for why,
    and which tree was used) — the reference SWE-bench conda tree was not available on the host
    that ran the review.

    **Part B** is no longer blocked: `309a3184` wired the grant into
    `sealed::plan_composed_root` as a *required* bind, and the mechanism was exercised end to end
    on a real host through a live capsule's shell tool — the tree is present, read-only, and
    outside-the-root paths are still `ENOENT`. A **missing** `source_path` refuses the session with
    `error[E-RUN-014]` naming the path, with no shell command executed.

    **The cross-host half of Part B was not run** and remains the one acceptance criterion on this
    page with no observed result: only one Linux host was available. The single-host run is
    evidence for the *mechanism*; it is not evidence for the "same pin, same interpreter, two
    hosts" identity claim, which is what steps B1–B2 exist to check. See
    [Recording the result](#recording-the-result) — the Host 2 column records this verbatim rather
    than being quietly dropped.

    A green `cargo build` / `cargo test` / `cargo clippy` is **not** evidence about the mount and
    must not be reported as if it were.

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
| The composed root actually carrying the staged tree | **shipped** (`309a3184`) |
| A missing `source_path` refusing the session with `E-RUN-014` | **shipped** (`309a3184`) |
| `staged_runtime::bind_mount_staged_runtimes`, proven in its own mount namespace | **shipped, and deliberately not the production call site** |

`309a3184` added a fourth parameter to `sealed::plan_composed_root`
(`staged_runtime_read_only: &[PathBuf]`) and a `PlanBuilder::require_bind` method that schedules a
**required** `RootOp::Bind { read_only: true }` for every declared `source_path` — before the fixed
runtime tree, `/etc`, `/dev`, `/proc`, `/tmp` and the workdir, and therefore before `pivot_root`.
No existence check happens anywhere in Murmur: the real `mount(2)` is the source of truth, and its
`ENOENT` aborts the construction through the pre-existing required-step path
(`RuntimeError::SealedRootConstructionFailed` → `E-RUN-014`, session-fatal).

`bind_mount_staged_runtimes` is still not the call site, and that is deliberate rather than
leftover: it allocates, and the composed root executes inside the forked child's `pre_exec` window,
which must not. It remains the independently-proven statement of the same two-call read-only bind
contract the planned step executes.

**One thing the bind alone does not buy.** `sealed` keeps Landlock installed *inside* the composed
root as defence in depth, and Landlock denies any path with no matching rule. A staged tree that is
bind-mounted but ungranted is present, read-only, and unreadable (`EACCES`) — which is
indistinguishable from the capability not working. `309a3184` therefore also emits one listable
`LandlockGrant` per staged directory (`sandbox::resolve_staged_runtime_landlock_grants`). This was
found by the hand-run below, not by a test: the first end-to-end attempt returned
`cat: ...: Permission denied` on a tree that was demonstrably mounted (the write probe already
reported `Read-only file system`).

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

!!! success "Unblocked as of `309a3184` — run on one host, see [Recording the result](#recording-the-result)"

    The mechanism half of this procedure (B3, B4, B5, plus the missing-source refusal in B6 below)
    was run on a single real host and passed. The identity half (B1, B2 compared *across* two
    hosts) still has no observed result, because only one host was available.

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

### Step B6 — a `source_path` absent from the host refuses the session

The inverse of everything above, and the reason the bind is planned `required: true`. Point the
same declaration at a path this host does not have and run again:

```yaml
    staged_runtime:
      - binary: python3
        source_path: /opt/definitely-not-here
        pin: whatever
```

`mur run --explain-scope` still reports the grant cleanly — existence is deliberately not checked at
parse time, so a manifest stays parseable on a machine that will never run it. The refusal comes at
the first shell tool call, when the composed root is constructed:

```text
error[E-RUN-014]: the sealed containment class was achievable at launch but its composed root could not be built for this subprocess: sealed-root: bind (ro) /opt/definitely-not-here -> /tmp/opt/definitely-not-here failed: No such file or directory (os error 2)
```

Confirm from `trace.jsonl` that `session_end` carries `"exit_status":"failed"` and
`"total_shell_calls":0` — the session ended rather than the capsule retrying, and **no shell command
ran inside a root missing what it declared**. That is the whole property.

!!! note "`transport: process` does not end the session here, by design"

    Under `inference.transport: process` the tool call is served by `agent::claude_bridge`, which
    deliberately does not act on the session-fatal flag — it is a tool server for an external CLI
    and owns no murmur session (see the comment at its dispatch site). The error text still reaches
    the caller in full, but the process keeps going. Use `transport: http` for this step if you
    want to observe the session actually terminating with `E-RUN-014`. This is pre-existing
    behaviour, unrelated to staged runtimes.

## Recording the result

Fill in on the run. Leave the "not run" markers in place until then — an unfilled table is a
correct record of an unperformed check.

### Part A — cost

| Field | Value |
|---|---|
| Date | 2026-08-06 |
| Host / kernel | Linux 7.0.0-28-generic, x86_64 |
| Tree path | `/usr/lib/python3` — **substitute**: no SWE-bench conda tree was available on the review host; this is a real, in-use CPython stdlib tree, not a synthetic fixture |
| Tree size (total) | 83 MB |
| File count | 5,448 |
| Backing filesystem | btrfs |
| Cache state for the copy | warm (no root available to drop caches in the review environment; a cold copy would only widen the gap below) |
| **Bind mount `real`** | 0.004s |
| **Recursive copy `real`** | 0.313s |
| Ratio | ~78x |
| Both `OK` probes observed in A1? | yes (`read: OK`, `write refused: OK`) |

Smaller than the roadmap's 285 MB / 9,194-file reference tree, so the absolute copy time will be
higher there — but the bind mount is O(1) in tree size, so the ratio only grows in staged_runtime's
favor on the reference tree. Re-run against the actual reference tree before treating this as the
final recorded number for the roadmap's acceptance criterion.

### Part B — end to end

The tree used was **not** an interpreter, and deliberately so: the property under test on one host
is "is this directory bind-mounted read-only into the composed root at its own absolute path", which
a directory holding one readable file answers exactly as well as a conda env, without needing a
285 MB fixture. Proving "the same *interpreter*" is the cross-host claim, and that is the column
that went unfilled.

| Field | Host 1 | Host 2 |
|---|---|---|
| Date | 2026-08-06 | **not run — no second host.** This environment is a single sandboxed development machine and no second Linux host was provisioned or reachable. Not waived as unnecessary: the cross-host identity comparison (B1/B2) is genuinely unverified, and the Host 1 column is compensating evidence for the *mechanism* only. |
| Distro / kernel | Linux 7.0.0-28-generic, x86_64, AppArmor `restrict_unprivileged_userns=0` | not run |
| `achieved` class | `sealed` (`mur run --explain-scope`: `declared: sealed / achieved: sealed / floor met: yes / mechanism: mountns+pivot_root+landlock+seccomp`) | not run |
| Staged tree used | `/space/mur-staged-check`, containing `marker.txt` (31 bytes). Substitute for the reference conda env — see the note above. | not run |
| `pin` declared | `hand-verification-309a3184/marker-tree-2026-08-06` | not run |
| `python3 -VV` output | **not applicable** on Host 1 — a marker tree was staged, not an interpreter. This row is the cross-host identity check and needs two hosts to mean anything. | not run |
| `sys.executable` / `sys.prefix` | not applicable (as above) | not run |
| B2 substitute — tree readable at its declared path? | **yes.** `cat /space/mur-staged-check/marker.txt` → `staged-runtime-marker-309a3184`. `ls -la` of the same path listed `marker.txt` from inside the session. | not run |
| B3 write refused? | **yes.** `touch /space/mur-staged-check/probe` → `touch: cannot touch '/space/mur-staged-check/probe': Read-only file system` (`EROFS`) | not run |
| B4 `ENOENT` (not `EACCES`)? | **yes.** `stat /etc/shadow` → `stat: cannot statx '/etc/shadow': No such file or directory` | not run |
| `W-SEC-009` absent? | **yes.** No `interpreter_runtime` declared and no `W-SEC-009` in stderr or `logs/bootstrap.log`. `W-SEC-005` (the standard sealed-tier notice) did fire, as expected. | not run |
| `trace.jsonl` `session_start` | `"containment_achieved":"sealed"` | not run |
| B6 missing `source_path` refuses? | **yes.** With `source_path: /space/mur-staged-ABSENT`: `error[E-RUN-014]: ... sealed-root: bind (ro) /space/mur-staged-ABSENT -> /tmp/space/mur-staged-ABSENT failed: No such file or directory (os error 2)`; `session_end` carried `"exit_status":"failed"` and `"total_shell_calls":0`. | not run |

## Residuals recorded here rather than buried

* **Only one host, so only half of Part B.** The mechanism is confirmed on a real kernel; the
  "same pin, same interpreter, two hosts" claim is not. Anyone with a second Linux host should run
  B1/B2 against a real pinned tree and fill the Host 2 column — until then, treat the cross-host
  guarantee as asserted rather than observed.

* **The helper is proven and is still not the call site.** `bind_mount_staged_runtimes` has a real
  mount-namespace test that passes on a real Linux host. That test proves the *operation*; the
  production mount goes through `sealed::plan_composed_root`'s required-bind path instead, because
  the helper allocates and `pre_exec` must not. Cite the hand-run above as evidence for Part B, not
  that test.

* **A substitute tree, not an interpreter.** Host 1 staged a directory holding one file. That is
  sufficient for "is it mounted, read-only, at the right path" and insufficient for anything about
  interpreter identity. Do not read the Host 1 column as a Python result.
* **`pin` is unverified by construction.** Nothing compares `pin` against the tree at
  `source_path` — not a hash, not a version probe, not an existence check. Two hosts could declare
  the same pin over genuinely different trees and Murmur would not notice. Step B2 exists precisely
  because the pin is a human claim; it is the check, not the guarantee.
* **`source_path` existence is not validated at parse time, and is not pre-checked at launch
  either.** A manifest naming a tree absent from the launch host parses cleanly, by design:
  manifests must stay parseable on machines that will never run them (`mur build` on a laptop for a
  Linux fleet). The failure surfaces when the mount is attempted, at `mount(2)`, in the child — and
  that placement is deliberate. A parent-side existence check would return `Err(String)` from
  `build_sealed_root`, which converts to the ordinary retryable `ShellExecError::Failed`; letting
  the required bind fail for real is what routes it to the session-fatal variant. Verified in B6.
* **Part A measures the kernel, not Murmur.** Its numbers stay valid across refactors of this
  codebase and should not be re-run for a Murmur-side change unless the mount flags themselves
  change.
