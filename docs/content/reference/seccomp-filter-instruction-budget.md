# Seccomp filter instruction budget

The child's seccomp filter compiles to **298 BPF instructions**. The kernel refuses a single
filter longer than **4096** and a whole filter chain longer than **32768**. The filter therefore
runs at **13.7×** headroom against the first ceiling and **110×** against the second.

Every number on this page was produced by running code on the host named below, not quoted from
documentation. The tests that produce them live in `crates/capsule-runtime/src/sandbox.rs`, module
`linux_enforce::seccomp_budget`, and run under `cargo test -p capsule-runtime seccomp_`.

## The measurement

| | |
|---|---|
| Host | Linux 7.0.0-28-generic, x86_64, Ubuntu 24.04.4 |
| libseccomp | 2.5.5 (system library), `libseccomp` crate 0.4.0 |
| Instructions, `unix_sockets_allowed = false` | 298 |
| Instructions, `unix_sockets_allowed = true` | 298 |
| Project budget (`SECCOMP_FILTER_INSTRUCTION_BUDGET`) | 512 |

Both settings measure identically because they trade one rule for another rather than adding one:
`AF_UNIX` moves from the denied socket domains to the allowed ones, so the rule count is the same
either way.

The count comes from exporting the filter the spawn path actually builds. `build_seccomp_filter`
returns the context unloaded; `seccomp_export_bpf` writes the compiled program as a sequence of
8-byte `struct sock_filter` records, and the instruction count is that export's length divided by
8. Nothing is hardcoded, and nothing is replicated: the test measures the same function
`install_seccomp_filter` loads.

The number is host-dependent. It is the sum of one rule per denied socket domain, one per allowed
socket domain, one per `SECCOMP_SYSCALL_ALLOWLIST` name **this architecture and this libseccomp
resolve**, and three Landlock syscalls. On x86_64 all 275 allowlist names resolve; on aarch64 the
legacy x86_64 spellings (`open`, `dup2`, `poll`, `stat`, …) do not, so the filter comes out
shorter. Re-measure rather than assume when the architecture changes.

## The two kernel ceilings

Both are measured in forked children, never in the test process — a loaded seccomp filter is
irreversible for the life of the task, so an in-process load would apply to the rest of the test
binary and everything it spawns.

| Ceiling | Value | Errno on refusal | What it limits |
|---|---|---|---|
| `BPF_MAXINSNS` | 4096 | `EINVAL` | One filter's length |
| `MAX_INSNS_PER_PATH` | 32768 | `ENOMEM` | Every filter attached to one task, added up |

**Per-filter.** `seccomp_per_filter_ceiling` builds two filters that straddle 4096 and loads each
in its own forked child. The largest that loaded was **4096 instructions**; the smallest that did
not was **4097**, refused with `EINVAL`.

**Cumulative.** `seccomp_cumulative_ceiling` stacks one filter repeatedly in a single forked child
until the kernel refuses. **227 loads** of a 132-instruction filter succeeded — 29,964 instructions
stacked, or 31,004 once the kernel's 4-instruction per-filter accounting overhead is added — and
load **228** was refused with **`ENOMEM`**.

The kernel adds up the *converted* program rather than the classic one: `seccomp_prepare_filter`
translates the classic BPF that `seccomp_export_bpf` writes into eBPF before the chain is measured,
and the translation comes out slightly longer. That is why 227 loads is a little short of the 240
the exported classic length alone predicts, and why the test brackets the ceiling rather than
pinning it to an exact load count.

Reaching this ceiling with the runtime's own filter would take **109 of them attached to a single
task**. The runtime attaches one.

## What the musl `ENOMEM` was

Evaluating a static-musl build (`.nexus/workspace/roadmap/musl-evaluation-verdict.md`, decided
2026-09-08) produced a failure this page's measurements were taken to bound.

Six integration tests failed under musl, all six being ones where a shell actually executes at the
sealed tier — cancel ×2, deny ×2, lifecycle ×1, untrusted_fence ×1. The twelve deny tests that
passed are the ones where a hook refuses the call, so no shell is spawned at all. The failing call
was `add_rule_conditional(Allow, socket, domain == AF_INET)` returning libseccomp `ENOMEM` inside
the forked child's `pre_exec` window, under musl only, reaching the capsule as `Unable to allocate
enough memory to perform the requested operation`.

The runtime behaved correctly throughout: it failed closed and produced an attributable message.
Nothing shipped is known to be wrong.

## What is ruled out

**Not the per-filter ceiling.** That ceiling refuses with `EINVAL`, not `ENOMEM`, and it is
reached at 4096 instructions against a filter of 298.

**Not the cumulative ceiling.** That one does return `ENOMEM`, and it is the only path in the
kernel's seccomp code that does — but it is reached at 32768 instructions across a whole filter
chain, and the runtime attaches exactly one filter of 298. It would take 109 of them.

**Not any kernel seccomp path at all.** The failure happened at `add_rule_conditional` — while the
filter was still being built in userspace, before `load()` was called. No attach had been attempted
when the error was returned, so no kernel refusal of any kind can explain it. The `ENOMEM` came
from libseccomp's own allocation inside `db_col_rule_add`.

**Not address space.** Peak `mur` `VmSize` during the failing test was 78 MiB against an
`RLIMIT_AS` of 2 GiB.

**Not the rule set.** A standalone probe replicating the construction exactly — same rule order,
deny-domain conditionals then allow-domain conditionals then the allowlist then `load()`, inside a
`fork()`ed child of an eight-thread process, with `unshare(CLONE_NEWUSER|CLONE_NEWNS)`, `capset` to
empty, `PR_SET_NO_NEW_PRIVS`, `close_range` and `RLIMIT_AS` = 2 GiB all applied first — succeeded
200/200 under musl and 200/200 under glibc.

## What remains unverified

The filter is built entirely inside the `pre_exec` window of a `fork()` from a multi-threaded
process. `malloc` is not async-signal-safe: if another thread held the allocator's lock or left its
arena mid-update at the moment of the fork, the child inherits that state with no thread left to
resolve it, and an allocation inside libseccomp can fail for a reason that has nothing to do with
available memory. musl's allocator and glibc's differ in exactly the places that would make this
libc-specific.

**This is a hypothesis. This card did not verify it.** It is the leading candidate because every
other explanation above is ruled out by measurement, not because it was tested.

Two follow-ups would settle it, neither of them done here:

| Follow-up | What it would give |
|---|---|
| `pre_exec`-window instrumentation | Direct evidence of the allocator's state at the moment `add_rule_conditional` fails. This is what would confirm or refute the hypothesis. |
| Building the filter before `fork()` | Removes the exposure regardless of the cause: the context would be constructed in the parent, and the child's post-fork work would be `load()` alone. |

Nothing in the spawn path moved for this card. The filter is still built where it was.

## Growing the allowlist

`SECCOMP_FILTER_INSTRUCTION_BUDGET` is a project ceiling, not a kernel one — 512, chosen against a
measurement of 298 and set an order of magnitude below `BPF_MAXINSNS`. Crossing it fails
`cargo test` with the measured count, the budget and the kernel ceiling in the message. That is a
prompt to re-measure and decide, not evidence that the kernel would refuse.

The three constants live next to the allowlist in `crates/capsule-runtime/src/sandbox.rs`:
`SECCOMP_FILTER_INSTRUCTION_BUDGET`, `BPF_MAXINSNS_CEILING` and `SECCOMP_MAX_INSNS_PER_PATH`.
Raising the budget means updating the numbers on this page in the same change.
