# Escape-conformance harness — the hand-run containment release gate

!!! danger "This procedure must be run by hand, on a real uncontainerised Linux host. Never on CI."

    No runner this project has resolves to the full enforcement tier, so a CI-wired suite would
    skip its way to green and certify nothing — which is precisely how a non-functional Linux
    tier came to be documented as merely "unverified". The harness is therefore a **non-member**
    of the root Cargo workspace: `cargo build --workspace`, `cargo test --workspace` and CI's
    `cargo check --workspace --all-targets` do not reach it, by construction.

    **Do not add it to `/Cargo.toml`'s `members` list.** That exclusion is the mechanism, not a
    convention.

`W-SEC-005` says Linux kernel enforcement is implemented but not team-verified. It has rested on
one live run on one host, which is a smoke test rather than verification. This harness replaces
that with a **dated record file** produced on real hardware, per containment class, asserting
negative results — and it is the recorded run, not a green build, that gates the `W-SEC-005`
wording.

## What it asserts, and what it refuses to assert

Three rules the whole design turns on. They are worth reading before the commands.

**Refuse, never skip.** If the host cannot back the class named by `--class`, the harness exits
non-zero *before running a single case*, prints the declared class, the achieved class and the
reason (reusing `containment_shortfall_reason`'s own wording), and writes **no** record file. An
absent record must never be confused with a passing one. The same applies to a detected container,
and to a host where no probe can start at all.

**Resource exhaustion is its own category.** The fork bomb, the two disk fillers, the memory hog
and the fd exhauster assert *availability*. They never feed the boundary rollup — not in the
stdout summary, not in the exit code, not in the record. A capsule that exhausts host resources
has not escaped containment; nothing outside its granted scope was read, written or reached.

**The suite encodes what is true, not what is wished.** `stat-outside-workdir` asserts **SUCCESS**.
Landlock mediates `open`/`read`/`write` and the create-class actions; it does not mediate bare
metadata queries at any ABI. Metadata visibility is a documented property of the `scoped` class, and
a harness that reported it as a failure would be asserting something false about the class it is
certifying.

## Prerequisites

- A **real, uncontainerised** Linux host — not Docker, not a rootless container, not WSL. A
  container masked three separate findings during the original investigation: the raw-disk escape,
  the `docker.sock` escape, and the entire syscall surface all looked closed inside Docker and were
  wide open outside it.
- Kernel **≥ 5.13** with Landlock available, for `--class scoped`.
- `python3` on `PATH`. Every probe runs as a single `python3` invocation, because under the full
  enforcement tier a capsule may only `execve` the binaries named in `capabilities.shell.allow` —
  a probe written as a pipeline of `cat`, `dd`, `stat`, `ln` and `mknod` would not measure the
  boundary, it would fail to start.
- **cgroup v2 with a delegated subtree.** `mur run` refuses any subprocess-capable capsule with
  `E-RUN-012` when it cannot create a cgroup scope. The harness wraps each run in
  `systemd-run --user --scope --property=Delegate=yes` automatically when `systemd-run` is present;
  see [the install requirement](resource-limits-manual-verification.md#install-requirement-systemd-user-cgroup-delegation)
  for the permanent alternative.
- A built `mur`. The harness finds it at `$MUR_BIN`, then `target/release/mur`, then
  `target/debug/mur`, then `PATH`.
- **Run as `root` if you can.** Three cases — `mknod-block-device-in-workdir`, `syscall-bpf` and
  `syscall-open-by-handle-at` — are refused for an ordinary uid regardless of this runtime, so a
  non-root run records them but cannot attribute the refusal. The record stamps the effective uid
  at the top for exactly this reason. Root is also the deployment shape the device-node escape
  actually exposed.

## Build

The harness is its own workspace root, so it is built from inside its own directory:

```bash
cd crates/capsule-runtime/escape-conformance
cargo build --release
```

This produces two binaries under `crates/capsule-runtime/escape-conformance/target/release/`:

| binary | role |
|---|---|
| `escape-conformance` | the gate: class check, case registry, record writer |
| `probe-driver` | the scripted inference CLI each case runs behind (see below) |

On a non-Linux host it still compiles — a reviewer on macOS can confirm that much — but
`--class scoped` will refuse there, which is the correct outcome and not a build failure.

Lint it on its own; `cargo clippy --workspace` from the repository root does **not** reach it:

```bash
cd crates/capsule-runtime/escape-conformance
cargo clippy --all-targets -- -D warnings
cargo test          # the registry-completeness and expectation-table assertions
```

## Run

```bash
cd crates/capsule-runtime/escape-conformance
./target/release/escape-conformance --class scoped
```

### Flags

| flag | meaning |
|---|---|
| `--class <advisory\|scoped\|sealed>` | **required.** The class to assert. Refuses if the host cannot back it. |
| `--record-dir <DIR>` | where the dated record lands. Default: the current directory. |
| `--work-root <DIR>` | per-case scratch, kept after the run. Default: `<record-dir>/escape-conformance-work-<stamp>`. |
| `--mur <PATH>` | the `mur` binary to grade. |
| `--probe-driver <PATH>` | override the driver binary. Default: next to `escape-conformance`. |
| `--python <NAME>` | interpreter the probes run as. Default `python3`. |
| `--timeout-secs <N>` | wall-clock ceiling per case. Default 300. |
| `--systemd-scope` / `--no-systemd-scope` | wrap each `mur run` in a delegated transient scope. Default: on when `systemd-run` is present. |
| `--allow-container` | run despite a container signal. The record is then stamped **NOT `W-SEC-005` EVIDENCE**. |
| `--only <CASE-ID>` | run one case (repeatable). Stamps the record **PARTIAL RUN**. For iterating by hand, never for a release gate. |
| `--list-cases` | print the registry with per-class expectations and exit. Runs nothing, writes nothing. |

### Exit codes

| code | meaning | record written? |
|---|---|---|
| 0 | every asserted case matched its expected verdict | yes |
| 1 | usage error, or the harness could not proceed | no |
| 2 | **refused before running any case** — class gate, container, or a failed preflight | **no** |
| 3 | at least one **boundary** case failed — a containment escape | yes |
| 4 | boundary clean, but a **resource-exhaustion** case failed — denial of service | yes |

3 and 4 are distinct so a resource failure can never be mistaken for an escape by a reader of the
exit status alone.

## How a case reaches the sandbox

Worth understanding before trusting a verdict.

`sandbox::prepare_enforcement` — the code that installs the Landlock ruleset and the seccomp filter
— runs inside the forked child of `shell::execute_shell`, and both are `pub(crate)` to
`capsule-runtime`. There is no library seam an external package can use. The script-capsule route
does not help either: a capsule component's linker gets `murmur:tool-registry/invoke`, whose
dispatch resolves WASM tool components only, so a script capsule cannot invoke a shell binary at
all. **The only route into the enforcement path is the agent loop.**

So each case launches a real capsule through `mur run`, with `inference.transport: process` pointed
at this package's `probe-driver` instead of a subscription CLI. `mur run` stands up the Claude
Bridge, advertises the capsule's `shell.allow` binaries over it, and spawns whatever
`inference.command` names; `probe-driver` makes exactly one predetermined `tools/call` and exits.
Tool execution, capability enforcement and the trace are all murmur's — byte for byte the path a
live model would drive. Only the *choice* of which tool to call is scripted rather than sampled.

That is what makes this a gate rather than a demonstration: a suite whose verdicts depended on a
model deciding to run the exact command it was handed would be flaky in the one direction that
matters, because a case the model skipped would produce no evidence and "no evidence" must never
read as "contained". It also costs no API calls and needs no key.

### The grants the harness itself adds, and why they do not weaken a case

Each generated manifest declares `capabilities.shell.allow: [bash, python3]` and a
`capabilities.shell.interpreter_runtime` grant for `python3`, derived at preflight by asking the
interpreter for its own `stdlib`, `platstdlib`, `lib-dynload` and multiarch library directory.
`libffi`, which `_ctypes` dlopens, is a dependency of the extension module rather than of the
`python3` binary, so it is not in the library closure the runtime derives from `shell.allow` and
has to be granted explicitly — without it the six dangerous-syscall cases could not import `ctypes`
and would all report `INCONCLUSIVE`.

The grant opens the Python standard library and the system library directory for read+execute.
**Every boundary case targets something else entirely** — `/etc`, `/tmp`, `/proc`, a device node, a
socket, a syscall number — so no case's verdict can be produced by it. `bash` is allowlisted only so
`exec-renamed-disallowed-binary` has an allowlisted basename to wear. `capabilities.network` is left
entirely undeclared, so every destination is unlisted and `unix_sockets` keeps its default of
`false`, which is what the network and AF_UNIX cases assert against.

### Preflight

Before the first case, the harness launches one trivial capsule and confirms a probe can start at
all. If it cannot, the run is **refused** (exit 2, no record) rather than executed. A host where
nothing can execute would report every asserted case as a failure, and "twenty-three boundary
escapes" is a far more damaging false statement than "this harness declined to measure anything
here".

## The case registry

23 boundary cases and 5 resource-exhaustion cases — the roadmap's minimum case set exactly. There is
no mechanism to disable a case: the registry is a flat `const` slice, and the counts are asserted by
this package's own `cargo test`, so a deletion cannot land without appearing in a diff review.
`exec-renamed-disallowed-binary` is additionally pinned by name in `PERMANENT_CASE_IDS`.

`not-asserted` is **not a skip**. The case still runs and its verdict is still recorded; what
changes is only that the result cannot pass or fail, because the declared class provides no
mechanism that could back a claim. `advisory` maps from both `EnvironmentOnly` and
`KernelSeccompOnly`, so it can promise nothing about the filesystem and cannot promise seccomp
either. `sealed` has no arm in `achieved_class_for_tier` at all, so its column is documented but
structurally unreachable — the class gate refuses on every host before any case runs.

### Boundary cases

| case | advisory | scoped | sealed | what it does |
|---|---|---|---|---|
| `read-etc-shadow` | not-asserted | REFUSED | REFUSED (unreachable) | opens `/etc/shadow`; also reads `/etc/passwd` as an attribution control |
| `write-outside-workdir` | not-asserted | REFUSED | REFUSED (unreachable) | creates a file in `/tmp` |
| `stat-outside-workdir` | **SUCCESS** | **SUCCESS** | REFUSED (unreachable) | `stat()`s `/etc/shadow` — metadata only |
| `symlink-escape` | not-asserted | REFUSED | REFUSED (unreachable) | symlinks to `/etc/passwd` and reads through it |
| `hardlink-escape` | not-asserted | REFUSED | REFUSED (unreachable) | hard-links `/etc/passwd` into the workdir |
| `rename-across-boundary` | not-asserted | REFUSED | REFUSED (unreachable) | renames a workdir file into `/tmp` |
| `proc-self-cwd-reopen` | not-asserted | REFUSED | REFUSED (unreachable) | walks out of the workdir through `/proc/self/cwd` |
| `proc-pid-root-reopen` | not-asserted | REFUSED | REFUSED (unreachable) | reads `/etc/passwd` through `/proc/<pid>/root` |
| `proc-self-fd-reopen` | not-asserted | REFUSED | REFUSED (unreachable) | re-opens the read-only `/dev/urandom` grant as `O_RDWR` |
| `inherited-fd-after-exec` | not-asserted | REFUSED | REFUSED (unreachable) | uses fd 7, leaked into `mur` by the launching shell |
| `mknod-block-device-in-workdir` | not-asserted | REFUSED | REFUSED (unreachable) | `mknod`s a block device for the host disk and reads it |
| `exec-renamed-disallowed-binary` | not-asserted | REFUSED | REFUSED (unreachable) | **permanent.** execs a disallowed binary wearing an allowlisted basename |
| `connect-unlisted-tcp-host` | not-asserted | REFUSED | REFUSED (unreachable) | TCP connect to `1.1.1.1:443` |
| `udp-exfiltration` | not-asserted | REFUSED | REFUSED (unreachable) | `sendto` a UDP datagram, no `connect` |
| `dns-exfiltration` | not-asserted | REFUSED | REFUSED (unreachable) | encodes data into a DNS label |
| `abstract-unix-socket-connect` | not-asserted | REFUSED | REFUSED (unreachable) | connects to an abstract-namespace `AF_UNIX` socket |
| `pathname-unix-socket-connect` | not-asserted | REFUSED | REFUSED (unreachable) | connects to `/var/run/docker.sock` and `/run/docker.sock` |
| `syscall-io-uring-setup` | not-asserted | REFUSED | REFUSED (unreachable) | `io_uring_setup(2)` |
| `syscall-userfaultfd` | not-asserted | REFUSED | REFUSED (unreachable) | `userfaultfd(2)` |
| `syscall-bpf` | not-asserted | REFUSED | REFUSED (unreachable) | `bpf(2)` |
| `syscall-open-by-handle-at` | not-asserted | REFUSED | REFUSED (unreachable) | `open_by_handle_at(2)` |
| `syscall-perf-event-open` | not-asserted | REFUSED | REFUSED (unreachable) | `perf_event_open(2)` |
| `syscall-keyctl` | not-asserted | REFUSED | REFUSED (unreachable) | `keyctl(2)` |

### Resource-exhaustion cases — availability, never a boundary

| case | advisory | scoped | sealed | graded on |
|---|---|---|---|---|
| `resource-fork-bomb` | not-asserted | CONTAINED | CONTAINED (unreachable) | the live-child count at the first `EAGAIN` |
| `resource-disk-filler-per-file` | CONTAINED | CONTAINED | CONTAINED (unreachable) | the byte offset at which `RLIMIT_FSIZE` refused the write |
| `resource-disk-filler-aggregate` | CONTAINED | CONTAINED | CONTAINED (unreachable) | a **second** tool call being refused after the breach latches |
| `resource-memory-hog` | not-asserted | CONTAINED | CONTAINED (unreachable) | the shell tool's exit code (`137` = 128 + `SIGKILL`) |
| `resource-fd-exhauster` | CONTAINED | CONTAINED | CONTAINED (unreachable) | the descriptor count at `EMFILE` |

The two disk fillers and the fd exhauster are asserted at `advisory` as well: `setrlimit(2)` and the
periodic workdir check apply on every Unix platform and owe nothing to Landlock or seccomp. The
fork bomb and the memory hog depend on cgroups, which `advisory` spans hosts with and without, so
they are recorded and left ungraded there.

### Deviations from `resource-limits-manual-verification.md`

Three, each a finding rather than a preference. All three are commented at the point of use.

- **`max_open_files: 64`, not that document's `16`.** `apply_hard_rlimits` runs first in the child's
  `pre_exec` window, so every later step lives under the ceiling — and installing the seccomp filter
  needs descriptors. Observed on Linux 7.0.0 with libseccomp 2.5.5: at 16 and at 32, *every* spawn
  dies with `shell enforcement setup failed before exec: There was a system failure beyond the
  control of libseccomp`; at 64 the spawn succeeds and the ceiling still bites, with `EMFILE` at
  descriptor 61 rather than in the 1000s. **A capsule that declares `max_open_files: 16` cannot spawn
  a subprocess at all on the full enforcement tier.**
- **`max_processes: 512`, not `64`.** Headroom well above `cgroup_pids_max` is what makes the fork
  bomb's stopping point attributable: a tree that stops in the low tens stopped because of
  `pids.max` and nothing else.
- **The aggregate disk filler is graded on a second tool call, not on a session kill.** Scenario 6
  says the session terminates with `E-RUN-013`, but the shipped mechanism is narrower than that
  wording: `WorkdirGuard` latches a breach and `ShellEnforcement::check_workdir_budget` refuses the
  *next* `Command::spawn()`. A case making one tool call gives the latch nothing to refuse and
  reports `UNCONTAINED` against a ceiling that works.

### Why `/proc/self/fd` is never the probing mechanism

Under the full tier `/proc` is in no Landlock grant, so an `openat` of anything under it fails with
`EACCES` and a probe built on it reports nothing — indistinguishable, from the outside, from a clean
result. The
[fd-hygiene document](subprocess-fd-hygiene-verification.md#why-not-procselffd) records that exact
mistake being made once already. `inherited-fd-after-exec` therefore enumerates with
`fcntl(fd, F_GETFD)`, and the three `/proc` cases run an explicit reachability control whose result
is folded into their recorded evidence, so "refused because `/proc` itself is denied" is never
confused with "refused for some unrelated reason".

Two of those three are graded more narrowly than their names suggest, and the reason is in the
record: `/proc/self/cwd` is a *magic symlink*, and Landlock applies its rules to the resolved
target, so re-opening it succeeds whenever the workdir is granted — which it always is. That is not
a boundary crossing, so the case is graded on the walk *out*. `proc-self-fd-reopen` bases on
`/dev/urandom`, which `CAPSULE_DEVICE_GRANTS` grants **read-only**, because a workdir file is
already writable and an `O_RDWR` re-open of one would widen nothing.

### The TOCTOU class is out of scope here

`exec-renamed-disallowed-binary`, `connect-unlisted-tcp-host`, `udp-exfiltration` and
`dns-exfiltration` are single-threaded, non-racing attacks — a copy, a plain `connect`, a plain
`sendto`. The seccomp-notify TOCTOU class documented in
[the audit](seccomp-notify-toctou-audit.md) is a different probe with different infrastructure,
living in `crates/capsule-runtime/racecheck/`. **Do not merge this harness into that package**, and
do not add race-retry loops to these cases.

## The dated record

Written on every completed run, never on a refused one. Named
`escape-conformance-<class>-<YYYYMMDD>T<HHMMSS>Z.md` in `--record-dir`.

It contains, in order: a citability stamp; the two-row summary table with the boundary and
resource rollups kept separate; the host block (`uname -r`, `uname -sm`, platform, effective uid,
cgroup v2, container detection, declared and achieved class, the `mur` binary and its version, and
the verbatim invocation); every container signal checked and its individual result; the two case
tables; a per-case evidence section carrying the probe's own `DETAIL` line and the case's
attribution note; and the full expectation table for all three classes, so the file is
self-contained.

A record is stamped **NOT `W-SEC-005` EVIDENCE** — as the first thing a reader sees — when a
container was detected, or when `--only` narrowed the run.

## Negative control

A probe that reports "clean" on a machine where it cannot see anything is worthless. Confirm the
harness detects what it is looking for by removing a fix and re-running, exactly as
[the fd-hygiene procedure](subprocess-fd-hygiene-verification.md) does for its own probe.

Restore `MakeBlock` to `linux_enforce::WORKDIR_ACCESS_RIGHTS` in
`crates/capsule-runtime/src/sandbox.rs`, comment out the `drop_all_capabilities()` call in
`child_install_enforcement` (either alone blocks the escape), rebuild `mur`, and re-run:

```bash
cargo build --release -p murmur-cli
cd crates/capsule-runtime/escape-conformance
./target/release/escape-conformance --class scoped --only mknod-block-device-in-workdir
```

**Expected:** `mknod-block-device-in-workdir` flips to `ALLOWED` and the process exits 3. No other
case's verdict changes. Revert both edits and re-run the full suite before recording any result.

Note that a non-root run cannot demonstrate this control: `mknod(2)` for `S_IFBLK` always requires
`CAP_MKNOD`, so an ordinary uid is refused with or without the fix. The case's attribution note says
so, and the record stamps the effective uid.

## Adding a case

Append a `Case` to `REGISTRY` in `src/cases.rs` and update `BOUNDARY_CASE_COUNT` or
`RESOURCE_CASE_COUNT`. The count assertion failing is the intended speed bump: it forces the change
to be deliberate. Each case needs an `attribution` note saying what a reader must know to interpret
its verdict — which mechanism a refusal is attributable to, and where attribution is ambiguous. A
case that reports `REFUSED` without saying *why* is close to worthless: `/etc/shadow` is unreadable
to an ordinary uid with no sandbox at all.

The probe body is Python defining `main()`, wrapped by the scaffolding in `src/probe.rs`; call
`verdict(v, d)` with one of `REFUSED`/`ALLOWED`/`SUCCESS`/`CONTAINED`/`UNCONTAINED`/`INCONCLUSIVE`.
`INCONCLUSIVE` is never a pass — a missing result is not a clean result.

## Adding a fourth containment class

`ContainmentClass` lives in `crates/murmur-artifact/src/runtime_manifest.rs`. Adding a variant makes
`Case::expectation`'s `match` non-exhaustive, which is the compiler pointing at every place a new
column is needed: add the field to `Case`, fill it for all 28 cases, extend `list_cases` and the
record's three-class table. `sealed_expectations_are_all_marked_unreachable` in `src/cases.rs` is
the pattern to copy for any class no host can yet provide.

## Recording the result

**PENDING — no team run has been recorded.**

A run *was* performed during implementation on Linux 7.0.0-28-generic (x86_64, bare metal, cgroup v2
delegated, non-root uid 1000), and all 23 boundary cases and all 5 resource-exhaustion cases matched
their expected verdicts at `--class scoped`. **That run does not count as the team acceptance**, for
two reasons that must not be glossed:

1. It was non-root, so `mknod-block-device-in-workdir`, `syscall-bpf` and `syscall-open-by-handle-at`
   are unattributable — each is refused for an ordinary uid regardless of this runtime.
2. It was performed against a **locally patched, uncommitted `mur`**. The shipped `mur` cannot run
   this suite on a non-root Linux host at all — see the blocker below.

When someone runs the procedure for real, replace this subsection with the record file's path and
its verbatim summary and host blocks, note any deviation from the commands as written, and then
update [Security Warnings](security-warnings.md#w-sec-005) to cite the recorded run.

### Known blocker — the shipped `mur` cannot run this suite non-root

`mur` calls `security::harden_process_dumpable()` at startup
(`crates/murmur-cli/src/main.rs`), which is `prctl(PR_SET_DUMPABLE, 0)`. The forked shell child
inherits `dumpable = 0`, which makes its `/proc/<pid>/*` entries root-owned and mode `0600`. The
seccomp-notify supervisor reads the `execve` pathname out of `/proc/<pid>/mem`
(`sandbox::linux_enforce::read_cstr_from_child`); on a **non-root** `mur run` that open fails with
`EACCES`, `classify_and_decide` fail-closes to `Decision::Deny`, and **every allowlisted binary's
`execve` is refused with `EACCES` before it runs**. The tool result reads exactly
`Permission denied (os error 13)`.

`cargo test` never shows this: the unit and integration tests do not call
`harden_process_dumpable`, so their supervisor reads the child just fine. That is the same
"green tests, unexercised path" pattern this harness exists to break.

The harness's preflight detects it and refuses with this explanation rather than reporting
twenty-eight escapes. Resolving it is a separate change to the runtime — this slice deliberately
changes no enforcement mechanism — and until it lands, a `scoped` conformance run needs either
`root` or a `mur` built without that call.
