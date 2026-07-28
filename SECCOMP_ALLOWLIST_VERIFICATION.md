# Seccomp allowlist — manual verification procedure

This procedure confirms, on real hardware, the two claims the default-deny syscall allowlist in
`crates/capsule-runtime/src/sandbox.rs` makes:

1. **Security.** A capsule shell subprocess can no longer call `io_uring_setup`, `bpf`,
   `userfaultfd`, `perf_event_open`, `process_vm_readv`, `open_by_handle_at`, `keyctl`, `add_key`
   or `ptrace` — because they are absent from `SECCOMP_SYSCALL_ALLOWLIST`, so the filter's default
   action (`SECCOMP_RET_ERRNO(EPERM)`) refuses them without inspecting a single argument.
2. **Compatibility.** The SWE-bench workload that already ran under Docker's default seccomp
   profile still runs, unmodified, under `mur run` on bare metal.

**It is deliberately not automated, and a green `cargo test`/CI run is not evidence for either
claim.** This repo's CI has never resolved to a Linux enforcement tier, so the filter under test is
not even installed there. The committed unit tests
(`sandbox::tests::allowlist_*`, `sandbox::tests::socket_domain_*`) assert only what two hand-authored
Rust constants contain — they exist so that re-permitting a dangerous syscall cannot happen by
accident during a future reconciliation, not to show that any kernel refuses anything. Until
someone runs the steps below and records the result, the allowlist is *implemented*, not *verified*.

Step 5 (the SWE-bench run) is a compatibility check rather than a security check, and it is the one
most likely to come back negative. Run it before treating the allowlist as safe to rely on broadly.

Related, and run separately: `docs/content/reference/security-warnings.md` §"Manual acceptance
procedure — unmediated AF_UNIX sockets" and §"Manual acceptance procedure — workdir device-node
escape". This document does not repeat those.

## Prerequisites

- A **real, uncontainerized** Linux host. Not Docker, not a rootless container, not WSL. Under
  Docker every syscall in step 2 is already refused by Docker's own profile, so every row passes
  for the wrong reason — a guaranteed false pass. This is exactly the confusion the card exists to
  resolve.
- Either Linux tier works. The syscall allowlist is pure seccomp — it does not involve Landlock —
  so kernel <5.13 (`KernelSeccompOnly`) exercises the same code path as kernel ≥5.13
  (`KernelFull`).
- A checkout of this repository and a working `cargo`.
- `python3` on the host, and `bash` + `python3` reachable on `PATH`. No compiler is needed: the
  probe invokes syscalls by number through `ctypes`.
- `root` (via `sudo`) for reading the kernel ring buffer in step 3.

Record the host facts first — every result below is only meaningful next to them:

```bash
uname -rm
cat /etc/os-release | head -2
python3 -c "import ctypes; print(ctypes.CDLL('libseccomp.so.2').seccomp_version and 'libseccomp present')" 2>/dev/null || true
pkg-config --modversion libseccomp 2>/dev/null || dpkg -s libseccomp2 2>/dev/null | grep ^Version || rpm -q libseccomp
```

`libseccomp` older than 2.4.0 means the filter's audit-logging attribute (`SCMP_FLTATR_CTL_LOG`)
could not be set and step 3 will find nothing — enforcement is unaffected, but say so in the
result. Older than 2.5.4 means `landlock_*` syscall names did not resolve and the filter fell back
to their syscall numbers (444/445/446); step 1 passing on a `KernelFull` host is what proves that
fallback worked.

## Step 0 — confirm this host resolves to a kernel enforcement tier

```bash
cd /path/to/murmur
sudo -E cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::kernel_tier_allows_exec_within_shell_allowlist \
  -- --nocapture
```

If that prints a `skipping ...: resolved to EnvironmentOnly` line, the host is not on a kernel tier
and **the rest of this procedure is meaningless on this machine**. Stop and find a different host.

A pass here is also the first compatibility signal: `bash` executed and exited normally *under the
new default-deny filter*. Then run the rest of the Linux suite, which spawns real `bash`, `ls` and
`cat` and pipes between them:

```bash
sudo -E cargo test -p capsule-runtime --lib --no-fail-fast \
  sandbox::linux_integration_tests -- --nocapture
```

Any failure here that did not fail before this slice means the allowlist is too narrow. Go to
step 6.

## Step 1 — scratch harness

The probe has to run *inside* a capsule shell subprocess tree — that is the only place the filter
is installed. Running it from a host shell proves nothing. `shell::execute_shell` is crate-private,
so the probe runs from a scratch test appended to `crates/capsule-runtime/src/sandbox.rs`. **Do not
commit it.** Append this to the end of `mod linux_integration_tests` (just before that module's
closing brace):

```rust
    #[test]
    fn scratch_seccomp_allowlist_acceptance() {
        let tier = detect_enforcement_tier();
        eprintln!("TIER: {tier:?}");

        let workdir = tempfile::tempdir().unwrap();
        let policy = CapabilityPolicy {
            shell_allow: vec!["bash".to_string(), "python3".to_string()],
            ..CapabilityPolicy::default()
        };
        let exec_allow_paths = resolve_exec_allowlist(&policy.shell_allow);
        let enforcement = ShellEnforcement {
            tier,
            network_allow_ips: Vec::new(),
            unix_sockets_allowed: policy.unix_sockets_allowed,
            landlock_grants: LandlockGrant::non_listable_files(resolve_landlock_grants(
                &exec_allow_paths,
            )),
            exec_allow_paths,
        };

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

Run it with:

```bash
cd /path/to/murmur
sudo -E SCRATCH_SCRIPT='<the script for this step>' \
  cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_seccomp_allowlist_acceptance -- --nocapture --exact
```

When you are done with the whole procedure: `git checkout crates/capsule-runtime/src/sandbox.rs`.

## Step 2 — reproduce the evidence table

The probe calls each syscall by number with junk arguments. That is the point: a syscall the filter
refuses returns `EPERM` **before the kernel ever looks at the arguments**, so junk arguments produce
`EPERM` and nothing else. Any other errno (`EFAULT`, `EINVAL`, `E2BIG`, or a success) means the call
reached the kernel's implementation — i.e. the filter let it through.

Syscall numbers differ per architecture. Use the block for the host's `uname -m`.

**x86_64:**

```bash
sudo -E SCRATCH_SCRIPT='python3 -c "
import ctypes, errno
libc = ctypes.CDLL(None, use_errno=True)
probes = [(\"io_uring_setup\",425),(\"io_uring_enter\",426),(\"io_uring_register\",427),
          (\"bpf\",321),(\"userfaultfd\",323),(\"perf_event_open\",298),
          (\"process_vm_readv\",310),(\"process_vm_writev\",311),(\"open_by_handle_at\",304),
          (\"keyctl\",250),(\"add_key\",248),(\"request_key\",249),(\"ptrace\",101)]
for name, nr in probes:
    ctypes.set_errno(0)
    rc = libc.syscall(nr, 0, 0, 0, 0, 0, 0)
    e = ctypes.get_errno()
    print(\"%-20s rc=%-4d %s\" % (name, rc, errno.errorcode.get(e, e)))
"' cargo test -p capsule-runtime --lib \
  sandbox::linux_integration_tests::scratch_seccomp_allowlist_acceptance -- --nocapture --exact
```

**aarch64** — identical, with this `probes` list substituted:

```python
probes = [("io_uring_setup",425),("io_uring_enter",426),("io_uring_register",427),
          ("bpf",280),("userfaultfd",282),("perf_event_open",241),
          ("process_vm_readv",270),("process_vm_writev",271),("open_by_handle_at",265),
          ("keyctl",219),("add_key",217),("request_key",218),("ptrace",117)]
```

Confirm those numbers on the host itself rather than trusting this file, if `audit` is installed:

```bash
for s in io_uring_setup bpf userfaultfd perf_event_open process_vm_readv open_by_handle_at keyctl add_key ptrace; do
  printf '%-20s %s\n' "$s" "$(ausyscall "$(uname -m)" "$s" 2>/dev/null)"
done
```

### Expected result

Every row must read `rc=-1 EPERM`:

| syscall | before this slice (bare metal) | required now |
|---|---|---|
| `io_uring_setup` / `_enter` / `_register` | `EFAULT` — permitted | `rc=-1 EPERM` |
| `bpf` | `E2BIG` — permitted | `rc=-1 EPERM` |
| `userfaultfd` | `rc=3` — succeeded | `rc=-1 EPERM` |
| `perf_event_open` | `EFAULT` — permitted | `rc=-1 EPERM` |
| `process_vm_readv` / `_writev` | `EINVAL` — permitted | `rc=-1 EPERM` |
| `open_by_handle_at` | `EFAULT` — permitted | `rc=-1 EPERM` |
| `keyctl` / `add_key` / `request_key` | `EINVAL` — permitted | `rc=-1 EPERM` |
| `ptrace` | permitted (wedged a probe in `ptrace_stop` for 20 min) | `rc=-1 EPERM` |

Any row that is not `EPERM` is a **failure**: that syscall is still reachable from a capsule. Note
which one and stop — do not proceed to step 5 as if the security property held.

`ENOSYS` on a row is *not* a pass. It means this kernel does not implement that syscall at all, so
the row proves nothing about the filter. Note it as "not testable on this kernel".

A useful control, to prove the probe itself works and is not just failing for its own reasons — run
the same script *outside* the harness, as a plain host command:

```bash
python3 -c "
import ctypes, errno
libc = ctypes.CDLL(None, use_errno=True)
ctypes.set_errno(0)
rc = libc.syscall(425, 0, 0, 0)          # io_uring_setup on x86_64 and aarch64 alike
print(rc, errno.errorcode.get(ctypes.get_errno()))
"
```

Unsandboxed this should print something *other* than `EPERM` (typically `-1 EFAULT`). If the host
itself already returns `EPERM` — a hardening LSM, a distro-wide seccomp policy, `io_uring_disabled`
sysctl — then step 2's result is not attributable to murmur and this host cannot verify the claim.

## Step 3 — read the denial out of the kernel audit trail

The filter sets `SCMP_FLTATR_CTL_LOG`, so each denial is recorded kernel-side. The denied process
still sees only `EPERM` (`SECCOMP_RET_ERRNO` carries no other channel), so this log is the entire
diagnosability story for "a workload died on an unexpected denial".

Immediately after step 2, on the same host:

```bash
sudo dmesg | tail -20
sudo journalctl -k --since "-2min" | tail -20
sudo ausearch -m SECCOMP -ts recent          # only if auditd is running
```

A legible entry looks like this (the `dmesg`/`journalctl` form; field order varies by kernel):

```
audit: type=1326 audit(1721990400.123:456): auid=1000 uid=0 gid=0 ses=2 pid=31337
  comm="python3" exe="/usr/bin/python3" sig=0 arch=c000003e syscall=425 compat=0
  ip=0x7f... code=0x50001
```

What to read off it:

- `syscall=425` — the syscall number. Resolve it with `ausyscall <arch> 425` or
  `ausyscall --dump | grep -w 425`. `arch=c000003e` is x86_64; `arch=c00000b7` is aarch64.
- `pid=31337`, `comm="python3"` — which process in the shell subprocess tree hit it.
- `code=0x50001` — `SECCOMP_RET_ERRNO` (`0x00050000`) with errno `1` = `EPERM`. That low word being
  `1` is what distinguishes a default-action denial from the `EACCES` (13) that the `socket()`
  domain rules and the notify supervisor return.

**Pass condition:** at least one entry naming one of step 2's syscall numbers, with a `pid`/`comm`
from the probe. If step 2 showed `EPERM` for every row but no entry appears here, enforcement is
working and *logging* is not — go to step 4 before concluding anything.

## Step 4 — check `actions_logged`

Setting the filter attribute is necessary but not sufficient. The kernel only logs an action whose
type also appears in this host-level sysctl, which murmur does not and cannot set for you:

```bash
cat /proc/sys/kernel/seccomp/actions_logged
```

Expected output contains `errno`, e.g.:

```
kill_process kill_thread trap errno user_notif trace log
```

**If `errno` is absent:** the denials in step 2 still happened — the syscalls were still refused
with `EPERM`, and the security property in step 2 is unaffected. What is lost is only step 3: those
denials are invisible in the audit trail, so an operator debugging a workload that dies on an
unexpected denial gets a bare `EPERM` with no syscall name. Fix it on the host (not in murmur) and
re-run steps 2–3:

```bash
# Non-persistent; add every action currently listed plus errno.
echo "kill_process kill_thread trap errno user_notif trace log" | sudo tee /proc/sys/kernel/seccomp/actions_logged
# Persistent:
echo 'kernel.seccomp.actions_logged = kill_process kill_thread trap errno user_notif trace log' | sudo tee /etc/sysctl.d/99-seccomp-log.conf
```

Also confirm the kernel's audit subsystem is actually emitting: `auditctl -s` (if `auditd` is
installed) should not report `enabled 0`. Without `auditd`, records still reach the kernel ring
buffer, which is what `dmesg`/`journalctl -k` read.

Record the value of `actions_logged` in the result either way — a "no log entries" finding is only
interpretable next to it.

## Step 5 — run the SWE-bench suite unmodified

This is the authoritative compatibility check, and the reason the allowlist is modelled on the
OCI/Docker default profile rather than hand-minimised: this exact workload already ran to
completion under that profile, via Docker. Running it here answers "does the same policy still hold
when murmur, not Docker, is the thing enforcing it?"

On the same bare-metal host, with no container in the picture:

```bash
cd /path/to/murmur
cargo build --release -p murmur-cli

# Run the full suite exactly as it is run under Docker — same capsule, same manifest, same
# arguments, no reductions, no per-task allowlist edits.
./target/release/mur run <the SWE-bench capsule> [the same arguments used for the Docker run]
```

Requirements for a pass:

- The suite's pass/fail counts match the Docker baseline. A *lower* pass count is a failure of this
  slice even if every individual failure looks unrelated — a missing syscall surfaces as an
  arbitrary tool crash, not as a message naming seccomp.
- No task fails with an unexplained `EPERM` / "Operation not permitted".

While it runs, watch for denials in real time — this is far faster than post-mortem triage:

```bash
sudo journalctl -kf | grep -i 'audit.*syscall='
```

Every syscall number that appears there is a candidate for the allowlist. Resolve each with
`ausyscall <arch> <nr>` and take it to step 6.

## Step 6 — a workload died on an unexpected denial: what to change

1. Get the syscall name from the audit entry (`ausyscall <arch> <nr>`). If there is no audit entry,
   fix step 4 first — guessing from an `EPERM` is not worth the time.
2. Check it against `SECCOMP_MUST_STAY_DENIED` in `crates/capsule-runtime/src/sandbox.rs`. If it is
   listed there, **do not add it**. That list is the security property this slice delivers; a
   workload needing one of those syscalls is a design conversation (and a card), not a one-line
   edit. A `#[test]` fails if a name appears in both lists, so the edit cannot be made quietly.
3. Otherwise add it to `SECCOMP_SYSCALL_ALLOWLIST` in the same file, in the group it belongs to,
   with a one-line comment saying which workload needed it. Names are resolved per-architecture at
   filter-build time and silently skipped when unknown, so adding a name that does not exist on
   some architecture is safe.
4. Re-run steps 0, 2 and 5.

## Recording the result

Append the outcome to the card / build summary for slice `ebcc518e`, with: `uname -rm`, the distro,
the libseccomp version, the tier from step 0, the full step-2 table as printed, one verbatim audit
entry from step 3, the `actions_logged` value from step 4, and the SWE-bench pass/fail counts from
step 5 next to the Docker baseline they are being compared against. Until that record exists, the
tier warnings (`W-SEC-002` / `W-SEC-005`) stay accurate as written: this mechanism has not been
verified by the team on real hardware.
