# Verification — workdir device-node escape

!!! warning "Status: **NOT RUN.** Implemented and unit-tested; never executed on a real Landlock-capable Linux host."

    This page, not the published [security warnings](security-warnings.md) reference, is where the
    run status of this mechanism lives. Record the outcome of each scenario below, verbatim, in
    [Recording the result](#recording-the-result) — including the date and the host it ran on.

This procedure confirms the two mechanisms described under [W-SEC-005](security-warnings.md#w-sec-005) — the narrowed
Landlock workdir grant and the child capability drop — on real hardware. **It is deliberately not
automated.** There is no committed test that asserts "`mknod` is refused", and a green
`cargo test`/CI run is not evidence that any of this works: this repo's CI has never resolved to a
Linux enforcement tier where the code path is even executed. Until someone runs the steps below and
records the result, the mechanisms are *implemented*, not *verified*.

### The `ptrace` side of the same child

A third property rides along on the same `pre_exec` window and is checked here rather than
anywhere else, because it is observed in the same `/proc/self/status` read scenario 5 already does.

The forked shell child used to be left `ptrace`-able by same-UID processes: the runtime explicitly
restored `PR_SET_DUMPABLE` on each shell subprocess, because the retired seccomp-notify exec
supervisor had to read `/proc/<child>/mem` to recover the pathname of every `execve`, and the
kernel's `ptrace_may_access` check refuses that read on a non-dumpable target — even for the
target's own parent. With that supervisor gone (exec is a Landlock right now, decided in-kernel),
nothing reads the child's memory, the restore is gone, and the child inherits the runtime process's
non-dumpable state. Both processes stay non-dumpable for their whole lives, which is what closes
the `/proc/<pid>/environ` side channel from a shell child back onto its own parent's raw
environment.

Confirm it alongside scenario 5, from inside the sandboxed child:

```bash
SCRATCH_SCRIPT='grep -E "^(Cap[A-Za-z]+|NoNewPrivs):" /proc/self/status; \
  awk "/^(TracerPid|Uid):/" /proc/self/status; \
  cat /proc/$PPID/environ >/dev/null 2>&1 && echo PARENT_ENVIRON_READABLE || echo PARENT_ENVIRON_REFUSED'
```

- **Expected:** `PARENT_ENVIRON_REFUSED`. A `PARENT_ENVIRON_READABLE` means the runtime process is
  dumpable, or the child re-enabled its own flag — either way the side channel is open.
- **Regression:** any code path that calls `PR_SET_DUMPABLE` back on in the child. There is no
  longer a reason for one to exist.

### Prerequisites

- A **real, uncontainerized** Linux host. Not Docker, not a rootless container, not WSL. Containers
  routinely drop `CAP_MKNOD` from the container's own bounding set and mask `/dev`, so the "before"
  half of scenario 1 will not reproduce and the "after" half will pass for the wrong reason.
- Kernel ≥5.13 for the Landlock half (`KernelFull`). The capability half (scenarios 4 and 5) also
  runs on `KernelSeccompOnly`.
- A checkout of this repository and a working `cargo`.
- `root`. Scenarios 1–3 and 5 must be run as root, because root is the deployment shape that was
  actually exposed (see scenario 4 for the non-root case).

Confirm the tier the host resolves to before anything else:

```bash
cd /path/to/murmur
sudo -E cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_exec_within_shell_allowlist \
  -- --nocapture
```

A `SKIP — PROVES NOTHING` line in that output means the host is not on a kernel tier and **the rest
of this procedure is meaningless on this machine**. Stop and find a different host.

### Scratch harness

Scenarios 1–3 and 5 drive `shell::execute_shell` directly, which is crate-private, so they run from
a scratch test appended to `crates/capsule-runtime/src/sandbox.rs`. **Do not commit it.** Append
this to the end of `mod linux_integration_tests` (just before that module's closing brace):

```rust
    #[test]
    fn scratch_manual_acceptance() {
        let tier = detect_enforcement_tier();
        eprintln!("TIER: {tier:?}");

        let workdir = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string()],
            ..CapabilityPolicy::default()
        };
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            exec_allow_paths,
        };

        // Replace SCRIPT with the script from each scenario below, one at a time.
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
sudo -E SCRATCH_SCRIPT='<the script for this scenario>' \
  cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_manual_acceptance -- --nocapture --exact
```

When you are done: `git checkout crates/capsule-runtime/src/sandbox.rs`.

### Scenario 1 — `mknod` of a block device inside the workdir is refused

This is the escape itself. `8:0` is the conventional major/minor for `/dev/sda`; use whatever
`lsblk -o NAME,MAJ:MIN` reports for a real disk on this host if `8:0` is not present.

```bash
SCRATCH_SCRIPT='mknod ./pwn b 8 0 && echo MKNOD_OK || echo MKNOD_REFUSED; \
  dd if=./pwn bs=512 count=1 status=none | head -c 32 | xxd | head -1 || true'
```

- **Expected (fixed):** `MKNOD_REFUSED`, and no readable device node. The `mknod` fails with
  `EACCES` (Landlock, on `KernelFull`) or `EPERM` (no `CAP_MKNOD`, on either tier).
- **Regression (unfixed):** `MKNOD_OK` followed by an `xxd` dump of the host disk's first sector —
  i.e. raw host-filesystem bytes from inside a sandboxed workdir.

To see the "before" behavior for comparison, `git stash` this card's change to
`WORKDIR_ACCESS_RIGHTS` **and** the `drop_all_capabilities()` call in `child_install_enforcement`;
both must be reverted, because either one alone blocks the escape.

Repeat with a character device to cover `MakeChar` as well:

```bash
SCRATCH_SCRIPT='mknod ./pwnc c 1 3 && echo MKNOD_OK || echo MKNOD_REFUSED'
```

### Scenario 2 — FIFO creation inside the workdir still works

`MakeFifo` is deliberately still granted. If this scenario fails, the fix broke real build tooling
and must not ship as-is.

```bash
SCRATCH_SCRIPT='mkfifo ./p && echo FIFO_OK || echo FIFO_BROKEN; \
  ( echo hello > ./p & ) ; head -1 ./p'
```

- **Expected:** `FIFO_OK`, then `hello`. Regular-file and directory creation should also still work:

```bash
SCRATCH_SCRIPT='mkdir ./d && echo x > ./d/f && cat ./d/f && ln -s ./d/f ./l && \
  readlink ./l && echo BASIC_FS_OK'
```

### Scenario 3 — unix-socket creation inside the workdir (the known open question)

`MakeSock` is withheld, which means this scenario is **expected to fail** as shipped. The point of
running it is to find out whether that failure matters for a real workload before the caveat is
discovered in production. Record the result either way.

> **Note:** as of the `socket(2)` domain rule described under [W-SEC-005](security-warnings.md#w-sec-005), this scenario
> now fails *earlier* than it used to and for a different reason. With the default
> `capabilities.network.unix_sockets: false`, the `socket(socket.AF_UNIX, ...)` constructor itself
> raises `PermissionError` (`EACCES`) from seccomp, so `bind()` is never reached and the scenario
> says nothing about `MakeSock` either way. To test `MakeSock` specifically, set
> `unix_sockets_allowed: true` on the `ShellEnforcement` literal in the scratch harness first —
> otherwise you will attribute a seccomp denial to Landlock.

```bash
SCRATCH_SCRIPT='python3 -c "
import socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.bind(\"./sock\")
print(\"SOCK_BIND_OK\")
" 2>&1 | tail -2'
```

(`python3` must also be in `shell_allow` for this one — add `"python3".to_string()` to the scratch
harness's `shell_allow` vector, or use any other tool that binds an `AF_UNIX` path.)

- **Expected as shipped:** a `PermissionError`/`OSError` from `bind`, not `SOCK_BIND_OK`.
- **Decision to record:** if any tool the team actually runs under `capabilities.shell.allow` needs
  to bind a unix socket in its working tree, add `MakeSock` back to `WORKDIR_ACCESS_RIGHTS` in
  `crates/capsule-runtime/src/sandbox.rs`, update the [W-SEC-005](security-warnings.md#w-sec-005) section, and re-run
  scenario 1 to confirm the device-node refusal is unaffected (it is a separate bit — it will be).
  `MakeChar`/`MakeBlock` are not up for reconsideration either way.

### Scenario 4 — `CAP_MKNOD` is the sole gate for the device half (non-root)

This scenario needs no murmur code at all; it establishes the baseline claim in
[W-SEC-005](security-warnings.md#w-sec-005) that a non-root capsule was never exposed to this escape.

```bash
# As an ordinary, non-root user, with no ambient capabilities:
id
grep CapAmb /proc/self/status          # expect CapAmb: 0000000000000000
mknod /tmp/nonroot-test b 8 0 ; echo "exit=$?"
```

- **Expected:** `mknod: /tmp/nonroot-test: Operation not permitted`, `exit=1` — `mknod(2)` for
  `S_IFBLK`/`S_IFCHR` always requires `CAP_MKNOD` in the effective set, with or without Landlock and
  with or without this fix. Confirming this is what makes "the severity is about root-operated
  `mur run`" a documented finding rather than an assumption.

### Scenario 5 — the exec'd shell's capability sets are empty

Confirms mechanism 2 directly, on either Linux tier. Run as root — the whole point is that a root
parent does not hand its capabilities to the child.

```bash
SCRATCH_SCRIPT='grep -E "^Cap(Inh|Prm|Eff|Bnd|Amb):" /proc/self/status; \
  grep -E "^NoNewPrivs:" /proc/self/status'
```

- **Expected:**

```text
CapInh: 0000000000000000
CapPrm: 0000000000000000
CapEff: 0000000000000000
CapBnd: 0000000000000000
CapAmb: 0000000000000000
NoNewPrivs: 1
```

  `CapBnd` is zero only up to the kernel's own highest defined capability
  (`cat /proc/sys/kernel/cap_last_cap`); bits above that were never set. If `capsh` is installed,
  `capsh --print` inside the same script is an equivalent, more readable check.
- **Regression:** any non-zero `CapEff`/`CapPrm`/`CapBnd` under a root-run parent, or
  `NoNewPrivs: 0`, means `drop_all_capabilities` did not run or partially failed.

For contrast, the *unfixed* behavior for a root-run parent is `CapEff`/`CapPrm`/`CapBnd` all
`000001ffffffffff` (or whatever this kernel's full set is).

### Recording the result

Record what actually happened **in this file**, under a dated `### Run of YYYY-MM-DD` heading —
including the scenario-3 decision on `MakeSock`, and the host, kernel version and resolved tier the
run was made on. Then update the status box at the top of this page.

Run status does not go on the published [security warnings](security-warnings.md) page. That page
states what each mechanism enforces and links here for anyone who wants to check it by hand; it
carries no verification status, no dated run, and no scenario. If a scenario's outcome changes what
a mechanism *does* — a `MakeSock` decision that widens the workdir grant, say — then the published
description of the mechanism changes too, and so does
[W-SEC-005](security-warnings.md#w-sec-005). Nothing else on that page moves.
