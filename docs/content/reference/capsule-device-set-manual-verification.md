# Verification — the fixed capsule device set

!!! warning "Status: **NOT RUN.** Implemented and unit-tested; never executed on a real Landlock-capable Linux host."

    This page, not the published [security warnings](diagnostics.md) reference, is where the
    run status of this mechanism lives. Record the outcome of each scenario below, verbatim, in
    [Recording the result](#recording-the-result) — including the date and the host it ran on.

This procedure confirms the [fixed capsule device set](containment.md#capsule-device-set) described under
[W-SEC-005](diagnostics.md#w-sec-005) — that a capsule can read *and write* `/dev/null`, can read but not write
`/dev/zero` and `/dev/urandom`, and cannot open any other device at all — on real hardware.
**It is deliberately not automated.** There is deliberately *no* committed test that asserts "a
capsule can write `/dev/null`" or "a capsule cannot open `/dev/random`", and a green `cargo
test`/CI run is not evidence for either: this repo's CI has never resolved to a tier where
`apply_landlock_scope` is even called, and the dev machine is macOS, where the code does not
compile. The committed unit tests (`sandbox::tests::capsule_device_grants_*`) assert only what a
Rust constant holds, not what a kernel does. Until someone runs the steps below and records the
result, the mechanism is *implemented*, not *verified*.

Scenario 1 is the correctness fix (a read-only `/dev/null` breaks ordinary Python and shell code).
Scenario 4 is the security claim. Scenario 6 is a compatibility check and is the one most likely to
come back negative — run it before treating the three-device set as broadly safe.

### Prerequisites

- A **real, uncontainerized** Linux host with kernel ≥5.13. Not Docker, not a rootless container,
  not WSL. Unlike the [AF_UNIX procedure](af-unix-sockets-manual-verification.md), **only the Full tier is
  meaningful here**: this is a pure Landlock mechanism, so on `KernelSeccompOnly` every scenario
  below "passes" for the wrong reason (no Landlock domain exists, so nothing is denied) and gives a
  false pass on scenario 4.
- A checkout of this repository, a working `cargo`, `python3`, and `strace`.
- `root` is **not** required and is better avoided — the device grants are uid-independent, and a
  root run muddies scenario 4 (a non-root refusal could be ordinary DAC rather than Landlock).
  Where a scenario needs root, it says so.

Confirm the tier the host resolves to before anything else:

```bash
cd /path/to/murmur
cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_exec_within_shell_allowlist \
  -- --nocapture
```

A `SKIP — PROVES NOTHING` line in that output means the host is not on a kernel tier and **the rest
of this procedure is meaningless on this machine**. Stop and find a different host.

### Scenario 0 — re-establish the premise on the bare host (no murmur involved)

The whole reason `/dev/null` is writable is the open flags real code uses. Confirm them on this
host, outside the sandbox, so the rest of the procedure is anchored to observed behavior rather
than to this page:

```bash
# Python's subprocess.DEVNULL — expect O_RDWR|O_CLOEXEC. A read-only Landlock grant fails this.
strace -f -e trace=openat -o /tmp/devnull-python.strace \
  python3 -c 'import subprocess; subprocess.run(["true"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)'
grep '/dev/null' /tmp/devnull-python.strace

# A shell 2>/dev/null redirect — expect O_WRONLY|O_CREAT. A read-only grant fails this too.
strace -f -e trace=openat -o /tmp/devnull-bash.strace bash -c 'ls /nonexistent 2>/dev/null'
grep '/dev/null' /tmp/devnull-bash.strace
```

- **Expected:** `O_RDWR|O_CLOEXEC` in the first, `O_WRONLY|O_CREAT` in the second.
- **If either differs on this host/libc/Python version:** record the actual flags here before
  going further. They are the justification for the `WriteFile` bit, and if they change, the
  justification has to be rewritten rather than assumed.
- Note on the `O_CREAT`: it costs nothing extra. Landlock only checks `MakeReg` when a file is
  actually created, and `/dev/null` already exists — so the redirect needs `WriteFile` and nothing
  more. If scenario 1's redirect fails with `EACCES` while the direct write succeeds, that premise
  is wrong on this kernel and is worth recording.

### Scratch harness

Scenarios 1–5 drive `shell::execute_shell` directly, which is crate-private, so they run from a
scratch test appended to `crates/capsule-runtime/src/sandbox.rs`. **Do not commit it.** Append this
to the end of `mod linux_integration_tests` (just before that module's closing brace) — it reuses
that module's existing `kernel_full_enforcement` helper, so it builds the same enforcement
`ShellEnforcement::resolve` would:

```rust
    #[test]
    fn scratch_device_set_acceptance() {
        let tier = detect_enforcement_tier();
        eprintln!("TIER: {tier:?}");
        if tier != EnforcementTier::KernelFull {
            eprintln!("SKIP — PROVES NOTHING: device grants only exist on KernelFull");
            return;
        }

        let workdir = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "python3".to_string()],
            ..CapabilityPolicy::default()
        };
        let enforcement = kernel_full_enforcement(tier, &policy);

        let script = std::env::var("SCRATCH_SCRIPT").expect("set SCRATCH_SCRIPT");
        let result = crate::shell::execute_shell(
            "bash",
            &["-c", &script],
            &[],
            workdir.path(),
            &policy,
            &enforcement,
        )
        .expect("execute_shell must return Ok, not Err");
        eprintln!("EXIT: {}", result.exit_code);
        eprintln!("OUT: {}", result.stdout);
        eprintln!("ERR: {}", result.stderr);
    }
```

Run one scenario at a time with:

```bash
SCRATCH_SCRIPT='<the script for this scenario>' \
  cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_device_set_acceptance -- --nocapture --exact
```

When you are done: `git checkout crates/capsule-runtime/src/sandbox.rs`.

### Scenario 1 — `/dev/null` is readable and writable

This is the correctness fix. All four sub-checks must pass; each one is a distinct open shape.

```bash
SCRATCH_SCRIPT='
# 1a: the shell redirect — O_WRONLY|O_CREAT. If bash cannot open /dev/null for the redirect it
#     never runs the command at all and the `if` takes the else branch.
if true 2>/dev/null; then echo 1a_REDIRECT_OK; else echo 1a_REDIRECT_FAILED; fi
# 1b: an explicit write — O_WRONLY
if echo hello > /dev/null; then echo 1b_WRITE_OK; else echo 1b_WRITE_FAILED; fi
# 1c: a read — O_RDONLY, must return EOF immediately
if head -c 1 /dev/null >/dev/null 2>&1; then echo 1c_READ_OK; else echo 1c_READ_FAILED; fi
# 1d: subprocess.DEVNULL — O_RDWR|O_CLOEXEC, the shape a read-only grant breaks
python3 -c "
import subprocess
try:
    subprocess.run([\"true\"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print(\"1d_DEVNULL_OK\")
except OSError as e:
    print(\"1d_DEVNULL_FAILED\", e.errno, e.strerror)
"
'
```

- **Expected (fixed):** `1a_REDIRECT_OK`, `1b_WRITE_OK`, `1c_READ_OK`, `1d_DEVNULL_OK`.
- **Regression (unfixed / read-only grant):** `1a_REDIRECT_FAILED`, `1b_WRITE_FAILED` and/or
  `1d_DEVNULL_FAILED 13 Permission denied`. Errno 13 is `EACCES` — the Landlock denial. This is the
  exact failure the `WriteFile` bit exists to prevent, and it is why `/dev/null` cannot be granted
  read-only.
- **If `1d` fails but `1b` passes:** the grant is `WriteFile`-only rather than
  `ReadFile | WriteFile`. `subprocess.DEVNULL` needs both.

Confirm the write really is a discard and not, say, a file shadowed inside the workdir:

```bash
SCRATCH_SCRIPT='echo some-bytes > /dev/null; echo "SIZE=$(stat -c %s /dev/null)"; stat -c "TYPE=%F" /dev/null'
```

- **Expected:** `SIZE=0` and `TYPE=character special file`. Anything else means you are not writing
  to the real device node.

### Scenario 2 — `/dev/zero` and `/dev/urandom` are readable

```bash
SCRATCH_SCRIPT='
head -c 16 /dev/zero | od -An -tx1 | tr -d " \n"; echo " <- 2a_ZERO"
head -c 16 /dev/urandom | wc -c; echo "^ 2b_URANDOM_BYTES"
python3 -c "
import ssl, os
print(\"2c_OPENSSL_RAND\", len(os.urandom(32)))
with open(\"/dev/urandom\", \"rb\") as f:
    print(\"2d_EXPLICIT_OPEN_OK\", len(f.read(32)))
"
'
```

- **Expected:** sixteen `00` bytes for `2a`, `16` for `2b`, `2c_OPENSSL_RAND 32`, and
  `2d_EXPLICIT_OPEN_OK 32`. `2d` is the one that matters — it is the explicit `open()` path
  OpenSSL and older glibc take, which `getrandom(2)` would not exercise.
- **Regression:** any `Permission denied`. The read-only grants are not being added.

### Scenario 3 — `/dev/zero` and `/dev/urandom` are **not** writable

The read-only grants must be exactly that. This is what keeps `/dev/null` the *sole* writable path
outside the workdir (on `sealed`, `/tmp` is writable too — but it *is* the workdir, bound there from
`<workdir>/.mur-tmp`; see [the note above](containment.md#capsule-device-set)).

```bash
SCRATCH_SCRIPT='
for dev in /dev/zero /dev/urandom; do
  if echo x > "$dev" 2>/dev/null; then echo "WRITABLE $dev  <- REGRESSION"; else echo "REFUSED $dev"; fi
done
python3 -c "
for dev in (\"/dev/zero\", \"/dev/urandom\"):
    try:
        open(dev, \"wb\").write(b\"x\"); print(\"WRITABLE\", dev, \"<- REGRESSION\")
    except OSError as e:
        print(\"REFUSED\", dev, e.errno, e.strerror)
"
'
```

- **Expected:** `REFUSED` for both, errno 13 (`EACCES`).
- **Note:** on many hosts `/dev/zero` and `/dev/urandom` are mode `0666`, so a *non*-sandboxed
  write to them succeeds. Run the same two python lines on the bare host first to confirm that —
  otherwise a `REFUSED` here could be ordinary DAC rather than Landlock, and the scenario proves
  nothing.
- **Regression:** `WRITABLE` for either — the `writable` bit is being set for more than
  `/dev/null`.

### Scenario 4 — every other device is refused

This is the security claim. It needs no new code to hold — `handle_access` declares the full ABI v1
right-set, so an unlisted path is denied — but "needs no code" is not "verified".

```bash
SCRATCH_SCRIPT='
python3 -c "
import os
for dev in (\"/dev/random\", \"/dev/full\", \"/dev/tty\", \"/dev/console\", \"/dev/mem\",
            \"/dev/kmsg\", \"/dev/ptmx\", \"/dev/sda\", \"/dev/nvme0n1\", \"/dev/loop0\"):
    if not os.path.exists(dev):
        print(\"ABSENT\", dev); continue
    try:
        os.close(os.open(dev, os.O_RDONLY))
        print(\"OPENED\", dev, \"<- REGRESSION\")
    except OSError as e:
        print(\"REFUSED\", dev, e.errno, e.strerror)
try:
    print(\"LISTED /dev:\", len(os.listdir(\"/dev\")), \"entries <- REGRESSION\")
except OSError as e:
    print(\"REFUSED listdir /dev\", e.errno, e.strerror)
"
'
```

- **Expected:** `REFUSED` (errno 13, `EACCES`) or `ABSENT` for every entry, and
  `REFUSED listdir /dev` — no rule names `/dev` itself, so the directory is not enumerable and
  `ReadDir` was never granted on any device.
- **Substitute the right block device** for this host: run `lsblk -dno NAME` on the bare host and
  put the real device names into the list. `ABSENT` for every block device is a **weak** result —
  find one that actually exists, or the most important half of this scenario went untested.
- **`/dev/mem` and raw block devices need root to be meaningful.** A non-root refusal there is
  ordinary DAC, not Landlock. Re-run *just this scenario* under `sudo -E` and confirm the refusal
  is still errno 13: as root without Landlock those opens would succeed, so a root `REFUSED` is
  the result that carries weight.
- **Regression:** any `OPENED`, or a successful `/dev` listing. Either means the grant is wider
  than three inodes — check whether a rule was added on `/dev` rather than on the individual
  device nodes.

### Scenario 5 — a missing device degrades, it does not crash the launch

The parent skips a device it cannot open (shrink-not-fail in `open_landlock_fds`), the same as an
unresolvable `shell.allow` grant. A host with a broken or minimal `/dev` must therefore lose that
one device, not lose the capsule. Reproduce it in a private mount namespace, where a tmpfs over
`/dev` makes `/dev/urandom` genuinely absent — so the parent's `open()` fails with `ENOENT` for any
uid, rather than for a permission reason that root would bypass:

```bash
sudo unshare --mount bash -c '
  mount -t tmpfs tmpfs /dev
  mknod /dev/null    c 1 3 && chmod 666 /dev/null
  mknod /dev/zero    c 1 5 && chmod 666 /dev/zero
  # /dev/urandom deliberately NOT created — this is the missing-device case.
  cd /path/to/murmur
  SCRATCH_SCRIPT="echo probe > /dev/null && echo NULL_STILL_OK; head -c 4 /dev/urandom | wc -c" \
    cargo test -p capsule-runtime --lib \
    sandbox::linux_integration_tests::scratch_device_set_acceptance -- --nocapture --exact
'
```

- **Expected:** the test still prints `TIER:`/`EXIT:`/`OUT:`/`ERR:` — the launch **succeeds** — with
  `NULL_STILL_OK` in the output and the `/dev/urandom` read failing. Losing one device narrows the
  scope and leaves the other two intact; it must never abort the capsule.
- **Regression:** `execute_shell` returns `Err`, or the test panics on its `.expect`. That would
  make a broken `/dev` entry a launch-killing failure, which is the opposite of the intended
  shrink-not-fail behavior.
- **If the namespace trick is impractical on this host** (some `cargo`/toolchain setups object to a
  synthetic `/dev`), record this scenario as **not run** rather than as passed. Do not substitute
  `chmod 000 /dev/urandom`: the harness runs as root there, and root's `CAP_DAC_OVERRIDE` makes the
  parent `open()` succeed anyway, so it tests nothing.

### Scenario 6 — full SWE-bench workload compatibility (the one that may fail)

Three devices is a deliberate floor, not a survey of what real tooling opens. Whether it is
*sufficient* for the workloads this project runs is an empirical question. **Run the full SWE-bench
workload, not a sample**, on a host resolving to `KernelFull`:

```bash
cd /path/to/murmur
# Baseline first — with the device set in place, since there is no "off" switch to compare against
# (this is not a manifest flag). To get a true baseline, temporarily comment out the device loop in
# `apply_landlock_scope` and rebuild; that is the pre-card behavior.
<your-swe-bench-invocation> 2>&1 | tee /tmp/swebench-devices-none.log

# Then the shipped behavior — restore the loop, rebuild, run again.
<your-swe-bench-invocation> 2>&1 | tee /tmp/swebench-devices-three.log

# Compare resolved/failed counts and diff the failure sets.
diff <(grep -E '^(PASS|FAIL)' /tmp/swebench-devices-none.log | sort) \
     <(grep -E '^(PASS|FAIL)' /tmp/swebench-devices-three.log | sort)
```

Then check for the specific suspects — a missing device usually fails quietly, as a confusing
downstream error rather than a named denial:

```bash
grep -iE '/dev/(null|zero|urandom|random|tty|full|pts)|Permission denied.*dev|No such device|not a tty|Inappropriate ioctl' \
  /tmp/swebench-devices-three.log | head -40
```

- **Expected:** the three-device run is **strictly better than or equal to** the baseline — every
  task that passed before still passes, and some that failed on a `/dev/null` denial now pass. That
  asymmetry is the point: this card only ever adds grants, so it cannot make a workload worse
  unless something else regressed.
- **If a task fails only in the three-device run:** that is not a widening problem, it is a bug —
  investigate before shipping.
- **If a task fails in *both* runs on a device that is not in the set** (`/dev/tty` for an
  interactive-terminal check, `/dev/pts/*` for a pty, `/dev/fd/*` for process substitution): record
  the task ID, the device, and the exact failing call **here**, then decide whether to add a fourth
  entry to `CAPSULE_DEVICE_GRANTS`. Add it *with* the recorded failure. Do **not** widen to `/dev`
  wholesale, and do not add a device on suspicion alone.

### Recording the result

Record what actually happened **in this file**, under a dated `### Run of YYYY-MM-DD` heading —
including scenario 0's observed `openat` flags verbatim, scenario 4's result under root against a
real block device, the scenario-6 compatibility outcome, and the host, kernel version and resolved
tier the run was made on. Then update the status box at the top of this page. If scenario 6
justified a fourth device, the recorded failure goes in scenario 6 above, next to the widening it
caused.

Run status does not go on the published [security warnings](diagnostics.md) page. That page
states which three devices every capsule gets and that every other device is refused, and links here
for anyone who wants to check it by hand; it carries no verification status, no dated run, and no
scenario. A fourth device is the one outcome that *does* change the published page, because it
changes what the mechanism enforces — see
[the fixed capsule device set](containment.md#capsule-device-set).
