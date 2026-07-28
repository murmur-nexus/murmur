# Audit — TOCTOU on the seccomp-notify supervisor's `exec` and `connect` arguments

!!! danger "Verdict: both architecture questions are **yes**. The supervisor is structurally subject to a TOCTOU on every pointer argument it inspects."

    The subprocess enforcement layer in `crates/capsule-runtime/src/sandbox.rs` decides
    `execve`/`execveat`/`connect`/`sendto` by reading a pointed-to argument out of the notifying
    task's memory and then answering with `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which hands the
    kernel the job of dereferencing that same pointer *again*, after the decision has been made.
    Nothing in between re-validates the bytes. A hostile **multithreaded** subprocess can therefore
    have one decision computed and a different one enforced.

    This is a property of the architecture, not a bug in its implementation, and it is what
    `seccomp_unotify(2)` warns about in its own words. **This audit records the verdict and stops
    there** — it changes no enforcement code. See
    [What this audit deliberately does not do](#what-this-audit-deliberately-does-not-do).

Line numbers below are as of the commit that added this document. Function and type names are the
stable citation; if a line number has drifted, search for the name.

## Scope

In scope: the userspace supervisor loop, and the four syscalls it mediates through
`SECCOMP_RET_USER_NOTIF`.

| syscall | argument inspected | read by |
|---|---|---|
| `execve` | `filename` (`args[0]`) | `read_cstr_from_child` |
| `execveat` | `pathname` (`args[1]`) | `read_cstr_from_child` |
| `connect` | `addr` (`args[1]`) | `read_sockaddr_ip_from_child` |
| `sendto` | `dest_addr` (`args[4]`) | `read_sockaddr_ip_from_child` |

Out of scope, and **not** subject to this class of race — see
[Mechanisms outside this class](#mechanisms-outside-this-class) for why:

- the `socket(2)` domain denials (`denied_socket_domains`),
- the Landlock filesystem scope (`apply_landlock_scope`),
- the filter's default-deny action and the syscall allowlist.

## Question 1 — does the allow path use `SECCOMP_USER_NOTIF_FLAG_CONTINUE`?

**Yes. Unconditionally, for every allowed notification, with no alternative path.**

`linux_enforce::supervisor_loop` (`crates/capsule-runtime/src/sandbox.rs:2537`) answers a
`Decision::Allow` at `sandbox.rs:2568-2571`:

```rust
let resp = match decision {
    Decision::Allow => {
        libseccomp::ScmpNotifResp::new_continue(req.id, libseccomp::ScmpNotifRespFlags::empty())
    }
    Decision::Deny => libseccomp::ScmpNotifResp::new_error(
        req.id,
        -libc::EACCES,
        libseccomp::ScmpNotifRespFlags::empty(),
    ),
};
```

The `ScmpNotifRespFlags::empty()` argument is not a way out. `libseccomp` is pinned at
`libseccomp = "0.4"` in `crates/capsule-runtime/Cargo.toml:40` and resolves to **0.4.0**
(`Cargo.lock:1368-1369`). That version's `ScmpNotifResp::new_continue`
(`~/.cargo/registry/src/index.crates.io-*/libseccomp-0.4.0/src/notify.rs:351-358`) reads:

```rust
pub fn new_continue(id: u64, flags: ScmpNotifRespFlags) -> Self {
    Self {
        id,
        val: 0,
        error: 0,
        flags: ScmpNotifRespFlags::CONTINUE.bitor(flags).bits(),
    }
}
```

`CONTINUE` (the crate's name for `SECCOMP_USER_NOTIF_FLAG_CONTINUE`) is OR-ed in regardless of what
the caller passes. `empty()` therefore yields exactly `CONTINUE`, and there is no reachable code in
`sandbox.rs` where an allowed `execve`/`execveat`/`connect`/`sendto` notification is answered any
other way — `supervisor_loop` has exactly two response arms and `new_continue` is the only one on
the allow side.

## Question 2 — does it read `filename`/`sockaddr` out of the child's memory?

**Yes, via `/proc/<pid>/mem`.**

Both readers live in `mod linux_enforce`:

- `read_cstr_from_child(pid: u32, addr: u64)` — `sandbox.rs:2682`. Opens
  `std::fs::File::open(format!("/proc/{pid}/mem"))` at `sandbox.rs:2683` and reads at the notified
  task's own argument address with `FileExt::read_at` at `sandbox.rs:2687` (4096 bytes, falling
  back to 256 on an unmapped-page failure).
- `read_sockaddr_ip_from_child(pid: u32, addr: u64)` — `sandbox.rs:2705`. Same open at
  `sandbox.rs:2706`, one 28-byte `read_at` at `sandbox.rs:2709` (`sizeof(sockaddr_in6)`), parsed by
  the OS-agnostic `parse_sockaddr_ip`.

`classify_and_decide` (`sandbox.rs:2588`) is where each syscall's argument address is handed to
them: `sandbox.rs:2601` (`execve`, `args[0]`), `sandbox.rs:2619` (`execveat`, `args[1]`),
`sandbox.rs:2643` (`connect`, `args[1]`), `sandbox.rs:2656` (`sendto`, `args[4]`).

Neither reader uses `process_vm_readv(2)` directly. That distinction does not matter here:
`seccomp_unotify(2)` groups `/proc/<pid>/mem` and `process_vm_readv(2)` together as the two
spellings of the same primitive, and warns about both identically. Reading through one rather than
the other does not change the race by one instruction.

## What follows from the two answers

Put together, one notification runs this sequence:

1. `classify_and_decide` reads N bytes out of the child at the argument address (`sandbox.rs:2601`
   / `2619` / `2643` / `2656`).
2. It computes a decision from *those bytes* — `decide_exec_allowed`'s canonicalization, or
   `network_ip_allowed`'s allowlist membership.
3. `notify_id_valid` runs (`sandbox.rs:2564`).
4. The supervisor answers `CONTINUE` (`sandbox.rs:2570`).
5. **The kernel resumes the syscall and dereferences the argument pointer itself**, from scratch.

Step 5 is the problem. The kernel has no memory of what the supervisor read in step 1; it re-reads
the pathname buffer, re-resolves the pathname through the filesystem, or re-reads the `sockaddr`.
Any writable-by-the-child input to that re-resolution can be changed between steps 1 and 5 by a
second thread — or by any other process sharing the address space or the filesystem path.

### `notify_id_valid` does not close this, and the code no longer claims it does

`SECCOMP_IOCTL_NOTIF_ID_VALID` — what `libseccomp::notify_id_valid` wraps — answers exactly one
question: *is this notification still live?* It exists so a supervisor does not act on a
notification whose task died, and so a supervisor that reads the target's memory is not fooled by
pid reuse. It says nothing about whether the bytes already read are still the bytes the kernel will
act on.

The comment at `sandbox.rs:2546-2563` used to label this sequence a "TOCTOU-safe pattern". That
label was wrong and this audit corrected it in place; the comment now states that the check is a
liveness check and that `CONTINUE` leaves the argument race open.

### The race is a class, not two special cases

`decide_exec_allowed`'s doc comment (`sandbox.rs:606-620`) already described the exec half
correctly — the kernel re-resolves the pathname after the check, so a symlink retargeted in that
window wins, and closing it properly needs `SECCOMP_IOCTL_NOTIF_ADDFD`-style fd substitution rather
than continue semantics. What it did not say is that exec and symlinks do not bound the problem.
This audit widened it, because the network half is the same race:

| | exec half | network half |
|---|---|---|
| what the supervisor reads | pathname string, then resolves it through the filesystem | `sockaddr` bytes |
| what the attacker changes | the symlink the pathname resolves through | the `sockaddr` buffer itself |
| when | between the supervisor's `canonicalize` and the kernel's re-resolution | between the supervisor's `read_at` and the kernel's re-read |
| result | a non-allowlisted binary executes under an "allow" decision | a non-allowlisted destination is connected under an "allow" decision |

The exec half additionally has a *second*, easier variant: the attacker need not touch memory at
all, because the pathname is resolved through the filesystem twice and a `rename(2)` over a symlink
is atomic and cheap. That is the variant `exec_race` probes.

## Mechanisms outside this class

Do not read this audit as a verdict on the whole sandbox. Two enforcement mechanisms sit alongside
the notify path and are structurally immune to it:

- **`socket(2)` domain denial** (`denied_socket_domains`, and the rules built from it at
  `sandbox.rs:2108-2120`). This is a classic register-value seccomp-BPF rule: `SECCOMP_RET_ERRNO`,
  evaluated in-kernel at syscall time, no notification raised, no userspace round trip, no memory
  of another task read. `socket()`'s `domain` is a plain integer already sitting in a register, so
  there is no pointer to re-dereference and no window to race. `AF_UNIX`/`AF_NETLINK`/`AF_PACKET`
  denials are unaffected by anything in this document.
- **Landlock filesystem scope** (`apply_landlock_scope`, `sandbox.rs:2250`). Enforced entirely
  kernel-side, against file descriptors the parent opened *before* `fork()` (`open_landlock_fds`,
  `sandbox.rs:2311`).
  An already-resolved fd cannot be retargeted by renaming a path. Also not a
  userspace-read-then-continue pattern.

This asymmetry is the actionable finding: the two mechanisms that decide from a value the kernel
already holds are sound, and the one that decides from a value it must fetch from the child is not.

## Why this architecture forces `CAP_SYS_PTRACE` in containers { #cap-sys-ptrace }

Supporting evidence for the reading above, not a separate claim to verify.

`crates/capsule-runtime/src/security.rs:23`'s `harden_process_dumpable()` marks the murmur host
process non-dumpable via `prctl(PR_SET_DUMPABLE, 0)`. That is about protecting *this* process's
`/proc/<pid>/environ` from its own descendants, and is independent of what follows.

Separately, the supervisor reading its spawned child's `/proc/<pid>/mem` is a cross-process read,
so it must pass the kernel's `ptrace_may_access` check. Inside a container's default capability set
that typically requires `CAP_SYS_PTRACE`. A prior containment investigation found exactly this:
granting a container `CAP_SYS_PTRACE` fixes shell-subprocess failures that
`--security-opt seccomp=unconfined` alone does not.

That requirement is not a Docker quirk. It is a direct structural consequence of an enforcement
design built on *reading another process's memory* — the same design property that produces the
TOCTOU. An architecture that decided from kernel-held values, or substituted a resolved fd, would
need neither the read nor the capability.

## The race probes

The static verdict above is settled by reading code. The probes exist for a different question:
**how wide is the window in practice, on real hardware?** That is not answerable by reading, and
this slice does not answer it.

### Where they live, and why they are invisible to every automated run

```
crates/capsule-runtime/racecheck/
├── .gitignore          # /target/ — this package builds into its own, outside the repo's
├── Cargo.toml          # package `seccomp-toctou-racecheck`; own `[workspace]` table
└── src/
    ├── lib.rs          # `racecheck` — arg parsing, the non-Linux stub message
    ├── linux.rs        # Linux-only scaffolding, mirroring `mod linux_enforce` function for function
    ├── connect_race.rs # [[bin]] connect_race
    └── exec_race.rs    # [[bin]] exec_race
```

`seccomp-toctou-racecheck` is **not** a member of the root workspace. `/Cargo.toml`'s `members`
list names exactly four crates and this is not one of them, so
`cargo check --workspace`, `cargo build --workspace` and `cargo test --workspace` — and therefore
CI's `cargo check --workspace --all-targets` — never see it. No `exclude` entry is involved; the
empty `[workspace]` table in the package's own `Cargo.toml` is what makes a non-member package
under a workspace directory legal.

That exclusion is deliberate and load-bearing. A race probe for a kernel-enforcement property must
never be able to "pass" in CI by skipping on a runner that lacks the enforcement tier — no
Landlock, no `libseccomp`, or not Linux at all. A green run there would look like evidence while
proving nothing. So the probes are not workspace members, not `#[test]` functions, and not under
any workspace crate's `tests/` directory: they stay excluded even if this repo's CI later grows a
`cargo test` step.

### Fidelity: what the probes reproduce

Both binaries reproduce the audited pattern rather than a simplification of it. `src/linux.rs`
mirrors `mod linux_enforce` function for function — `install_notify_filter` ↔
`install_seccomp_filter`, `supervise` ↔ `supervisor_loop`, and byte-identical copies of
`read_cstr_from_child`, `read_sockaddr_ip_from_child`, `send_fd_over_socket` and
`receive_fd_over_socket`. In particular `supervise` keeps the exact ordering under audit: receive →
read the argument from `/proc/<pid>/mem` → `notify_id_valid` → `new_continue`.

Two deliberate divergences, both documented at their definitions:

- The probe filter's **default action is `Allow`**, not `Errno(EPERM)`. The probe is not a sandbox;
  a default-deny would only stop the probe process from running. The race lives entirely in the
  `Notify` rules, which are constructed identically.
- Each probe installs a `Notify` rule for **one** syscall, not all four.

### Building and running

The probes build on any host. On non-Linux they compile to a stub that prints one line and exits 0;
only on Linux is the race logic compiled at all.

```bash
cd crates/capsule-runtime/racecheck
cargo build --release
```

```bash
./target/release/connect_race --iterations 100000
./target/release/exec_race    --iterations 20000
```

`--iterations` defaults to 100000 for `connect_race` and 20000 for `exec_race`; the lower default
reflects that every exec attempt costs a `fork` plus a real `execve`, roughly three orders of
magnitude more than a loopback `connect`.

On a macOS dev machine both binaries print, and exit 0:

```
connect_race: this probe only runs its race on Linux; see docs/content/reference/seccomp-notify-toctou-audit.md
```

### What each probe does

**`connect_race`** — races the `sockaddr` buffer.

The parent binds two `TcpListener`s on the *same port*: `127.0.0.1` (the "allowed" destination,
standing in for a capsule's single `network.allow` entry) and `127.0.0.2` (the "disallowed" one).
Linux routes all of `127.0.0.0/8` to `lo`, so two loopback IPs need no host configuration — and
using two IPs rather than two ports means the decision is keyed on exactly the field
`read_sockaddr_ip_from_child` + `network_ip_allowed` key on in production.

The child installs a `connect` notify filter, hands back the notify fd, then runs two threads.
Thread A connects in a loop through a shared 16-byte `sockaddr_in`; thread B spins, flipping that
buffer's **last address octet** between `1` and `2`. One byte is all it takes, and a single-byte
flip leaves no torn intermediate state to argue about: the buffer is always a valid `127.0.0.1` or
a valid `127.0.0.2`, each with a live listener behind it. The buffer is `[AtomicU8; 16]` rather than
a raw pointer written non-atomically — same bytes to the kernel, but defined behaviour under Rust's
memory model, so the result cannot be dismissed as UB.

*A win* is a successful `connect` whose `getpeername(2)` reports `127.0.0.2`. It cannot be a false
positive: had the supervisor read `127.0.0.2` it would have answered `EACCES` and the `connect`
would have failed outright, so every `127.0.0.2` peer is a connection the supervisor approved while
looking at `127.0.0.1`.

**`exec_race`** — races the pathname's filesystem resolution.

A scratch directory holds a symlink and a marker path. The symlink normally points at `true`
(the "allowlisted binary", which ignores its arguments); the disallowed retarget points at `touch`,
which — given the *same* `argv` — creates the marker. That asymmetry means one fixed `argv` is
inert for the allowed binary and self-evidencing for the disallowed one, so no compiled helper is
needed.

The child installs an `execve` notify filter and runs two threads. Thread A repeatedly `fork`s and
`execve`s the symlink path (`execve` replaces the caller, so each attempt needs its own process);
thread B spins, retargeting the symlink between the two binaries with `symlink` + `rename`. The
rename is atomic, so the link always resolves to one of the two and never transiently vanishes — an
attempt can never fail with `ENOENT` and be miscounted as a denial.

*A win* is the marker file existing after an attempt. A supervisor that had read the disallowed
target would have answered `EACCES` and nothing would have run, so the marker cannot be produced any
other way.

### Reading the output

Both binaries print progress lines and end with the same summary shape:

```
RACE_WON: <n>/<total>
```

`n` counts attempts where the disallowed outcome occurred despite the supervisor's decision having
been computed against the allowed one.

**`RACE_WON: 0/N` is a valid, reportable outcome and is not a bug in the probe.** It means the
window did not open on that kernel and that hardware in N attempts. Neither binary panics on a race
not won.

Read the corroborating lines before recording anything:

- `<probe>: supervisor: allowed=… denied=… stale-id=…` — if both `allowed` and `denied` are 0, the
  supervisor never saw a notification and **the run proves nothing**. `exec_race` prints an explicit
  warning in that case; check `/proc/<pid>/mem` readability first (see Prerequisites).
- `connect_race: detail: … other-errors=…` — usually ephemeral-port or backlog pressure. If it
  dominates, lower `--iterations` or set `sysctl -w net.ipv4.tcp_tw_reuse=1`.

---

## Manual acceptance procedure — seccomp-notify argument TOCTOU { #manual-acceptance-toctou }

This procedure measures how exploitable the confirmed race is on real hardware. **It is
deliberately not automated**, for the reason given above: a probe for a kernel-enforcement property
that can skip on a runner without that enforcement would turn a green CI run into false evidence.

Note what is and is not pending. The two architecture questions are **settled** — they are answered
from code above and need no hardware. What is pending is only the empirical win rate.

### Prerequisites

- A **real, uncontainerized** Linux host. Not Docker, not a rootless container, not WSL. Under a
  container's default capability set the supervisor's `/proc/<pid>/mem` read fails
  `ptrace_may_access`, every decision falls to the fail-closed `Err(_) => Decision::Deny` arm, and
  you get `RACE_WON: 0/N` for the wrong reason — a false pass. See
  [Why this architecture forces `CAP_SYS_PTRACE` in containers](#cap-sys-ptrace).
- Multiple physical cores. The racing thread and the syscall thread must run genuinely
  concurrently; on a single core the window is scheduler-bound and the result says more about the
  scheduler than about the kernel.
- A checkout of this repository and a working `cargo`, plus `libseccomp` development headers
  (`libseccomp-dev` on Debian/Ubuntu, `libseccomp-devel` on Fedora).
- `/bin/true` and `/bin/touch` (or their `/usr/bin` equivalents) present, for `exec_race`.
- No `sudo` required. The probe supervises its own child, so a default `ptrace_scope` of 1 permits
  the read.

Confirm the notify path is live before trusting any number:

```bash
cd crates/capsule-runtime/racecheck
cargo build --release
./target/release/exec_race --iterations 100
```

A `supervisor: allowed=… denied=…` line with both values 0 means the supervisor never saw a
notification, and **the rest of this procedure is meaningless on this machine**. Stop and find a
different host.

### Scenario 1 — `connect`/`sendto` `sockaddr` rewrite

```bash
cd crates/capsule-runtime/racecheck
./target/release/connect_race --iterations 100000
```

Record the `RACE_WON:` line verbatim, plus the `detail:`, `supervisor:` and `parent accepts` lines.
Run it at least three times: a race measurement of one sample is not a measurement.

### Scenario 2 — `execve` pathname retarget

```bash
./target/release/exec_race --iterations 20000
```

Record the `RACE_WON:` line verbatim plus the `detail:` and `supervisor:` lines. Again, at least
three runs.

Raise `--iterations` once the throughput on that host is known. If scenario 2 wins at a materially
different rate than scenario 1, say so — the two race different kernel machinery (path resolution
vs. a memory re-read) and there is no reason to expect the same number.

### Scenario 3 — the fail-closed check

Confirm the probe reports a loss honestly, so a `0/N` in scenarios 1–2 can be trusted. Run
`connect_race` on a host or container where `/proc/<pid>/mem` is *not* readable by the supervisor
and confirm the output shows `denied-EACCES` dominating with `RACE_WON: 0/N` — that is the
fail-closed path, not a race loss, and the two must be distinguishable in the recorded output.

### Recording the result

**PENDING — not yet run. No result has been recorded, and none should be inferred.**

No real Linux hardware was available when this audit was written, so the empirical win rate is
unknown. A green `cargo check`/`cargo build` on a macOS dev machine is **not** evidence about this
race and must not be reported as if it were.

When someone runs the procedure, replace this subsection with the verbatim `RACE_WON:` lines from
every run, the host's `uname -r`, core count, and libseccomp version — then update the callout at
the top of this page with the measured rate. Until that edit lands, this page's verdict stands as
written: the race is
**confirmed by construction and unmeasured in practice**.

---

## What this audit deliberately does not do

Having confirmed the TOCTOU, the correct action is to record the verdict and stop. This document
and its probes change **no** enforcement behaviour:

- No change to `supervisor_loop`, `classify_and_decide`, `decide_exec_allowed`,
  `install_seccomp_filter`, or any other executable line in `sandbox.rs`. The only edits that slice
  made to that file are the two doc comments named above.
- No fd substitution, no `SECCOMP_IOCTL_NOTIF_ADDFD`, no mitigation of any kind.
- No new `CapabilityPolicy` field, manifest schema field, WIT interface, CLI flag, or `W-SEC-*`
  warning code.

The reason is that there is no incremental patch to make. Closing this race means replacing the
mechanism, not tightening it:

- **exec** — Landlock's `Execute` right on the resolved allowlist paths, which the kernel enforces
  against the inode it actually executes. Scoped to the future `retire-exec-supervisor` work.
- **network** — a network namespace plus an egress proxy, so destination policy is enforced on the
  packet path rather than on a pointer read. Scoped to the future
  `network-namespace-egress-proxy` work.

Both are full replacements and both are separately gated. Until one lands, treat the subprocess
`shell.allow` and `network.allow` grants as defences against *non-racing* misuse — a renamed or
copied binary, a plainly non-allowlisted destination — and not as boundaries that hold against a
subprocess deliberately racing them.
