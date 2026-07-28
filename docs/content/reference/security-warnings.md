# Security Warnings

`mur run` and `mur build` can print non-fatal warnings about capability/enforcement gaps in a
manifest or host. These are distinct from the `E-<CATEGORY>-NNN` [error codes](cli.md) — a
warning does not stop the session or the build, it flags a posture issue you should be aware of.
Each one carries a `W-SEC-NNN` code and a link back to its section on this page:

```text
[capsule-runtime] warning[W-SEC-001]: capabilities.shell.allow is non-empty but this platform
has no kernel-level subprocess sandbox (Landlock/seccomp are Linux-only) — enforcement is
environment-only (synthetic HOME + credential env-stripping). This is permanent on this
platform. (https://docs.murmur.nexus/murmur-nexus/murmur/reference/security-warnings/#w-sec-001)
```

Warnings from `mur run` go to both stderr and `workdir/<session_id>/logs/bootstrap.log`.
Warnings from `mur build` go to stderr only.

| Code | Fires from | Summary |
|---|---|---|
| [`W-SEC-001`](#w-sec-001) | `mur run` | No kernel-level subprocess sandbox on this platform |
| [`W-SEC-002`](#w-sec-002) | `mur run` | Linux host without Landlock — filesystem scope unenforced (and seccomp unverified) |
| [`W-SEC-003`](#w-sec-003) | `mur run` | `network.allow` doesn't constrain bash's own outbound connections |
| [`W-SEC-004`](#w-sec-004) | `mur build` | Literal secret value found in a manifest field |
| [`W-SEC-005`](#w-sec-005) | `mur run` | Linux kernel enforcement is not yet team-verified on real hardware |
| [`W-SEC-006`](#w-sec-006) | `mur run` | A hook's `capabilities:` block declares a sub-key that is inert on hooks |
| [`W-SEC-007`](#w-sec-007) | `mur run` | A tool/driver narrowed to a host the capsule-wide ceiling does not allow — the entry was dropped |
| [`W-SEC-008`](#w-sec-008) | `mur run` | A tool/driver `capabilities:` block declares something per-artifact narrowing does not apply |
| [`W-SEC-009`](#w-sec-009) | `mur run`, `mur doctor` | `capabilities.shell.interpreter_runtime` couples the capsule to a specific host interpreter-version layout |

---

!!! warning "Linux kernel enforcement is not yet team-verified"
    The Landlock/seccomp subprocess enforcement has **not yet been verified by the team on real
    Landlock-capable Linux hardware** — it is implemented and unit-tested, and on the "Full" tier
    Landlock now grants a narrow, *derived* read+execute scope outside the workdir (the
    allowlisted binaries, their dynamic loader, and their shared libraries — nothing writable, no
    directory granted wholesale) so allowlisted programs can actually exec and dynamically link.
    The workdir's own grant withholds character-device, block-device and unix-socket creation, and
    both Linux tiers drop every capability from the shell child before `execve` — see
    [Manual acceptance procedure — workdir device-node escape](#manual-acceptance-device-node) for
    the exact commands the team runs to confirm those two.
    Until a real Linux run confirms these mechanisms end to end, treat the "Full" and "Seccomp-only"
    tiers below as **not-yet-confirmed**, not a hardened security boundary. Both Linux tiers emit a
    warning at launch (`W-SEC-005` / `W-SEC-002`) saying exactly this. The only tier whose behavior
    is verified today is Environment-only (macOS/Windows), because there the enforcement is a
    documented no-op.

## Subprocess enforcement tiers

`W-SEC-001`, `W-SEC-002`, `W-SEC-003`, and `W-SEC-005` all stem from the same mechanism: at
capsule launch, the runtime probes the host and resolves one of three enforcement tiers for shell
subprocesses declared under `capabilities.shell.allow`.

| Tier | Host | Filesystem | Exec | Network | Verified? |
|---|---|---|---|---|---|
| Full | Linux, kernel ≥5.13 (Landlock available) | kernel-enforced¹ | kernel-enforced¹ | kernel-enforced¹ | **Not yet** — implemented, not team-verified on real hardware |
| Seccomp-only | Linux, kernel <5.13 (no Landlock) | **not** enforced | kernel-enforced¹ | kernel-enforced¹ | **Not yet** — implemented, not team-verified on real hardware |
| Environment-only | macOS, Windows, any non-Linux host | **not** enforced | **not** enforced | **not** enforced | Yes — enforcement is a documented no-op here |

¹ *Intended* behavior. On the Full tier the Landlock scope grants the capsule workdir a near-full
access set **and** a narrow, *derived* read+execute grant for exactly the `shell.allow` binaries, their ELF
interpreter (dynamic loader), and the transitive closure of their shared libraries — so an
allowlisted program can exec and dynamic-link `/usr/bin/bash` and its libraries while nothing
outside the workdir is writable and no directory is granted wholesale. A capsule may *additionally*
name specific host directories a path-based interpreter needs (its stdlib) via
`capabilities.shell.interpreter_runtime` — but only the exact directories named, each with an
explicit per-directory `list_dir` flag, never a whole install prefix (see
[`W-SEC-009`](#w-sec-009)). The workdir grant is *not* the full Landlock right-set: character-device
(`MakeChar`), block-device (`MakeBlock`) and unix-socket (`MakeSock`) creation are withheld, so a
capsule cannot create a raw disk device node inside its own workdir and read the host filesystem
through it. Independently of Landlock — and therefore on **both** Linux tiers — the forked shell
child drops its entire capability bounding set, clears its permitted/effective/inheritable sets, and
sets `no_new_privs` before `execve`, so a root-operated `mur run` no longer hands the subprocess
`CAP_MKNOD` (or `CAP_DAC_OVERRIDE`, or anything else) in the first place. This code is unit-tested
but has not yet been run end to end by the team
on a real Landlock-capable Linux host, so do not treat any "kernel-enforced" cell above as a
confirmed boundary until that acceptance run lands — see
[Manual acceptance procedure — workdir device-node escape](#manual-acceptance-device-node).

Filesystem scoping uses Landlock; exec and network allowlisting use seccomp-bpf user-notify.
Both are Linux kernel primitives with no equivalent on macOS or Windows — the Environment-only
tier is not a gap awaiting a future release, it is the permanent ceiling on those platforms.
Environment-only enforcement still gives you a synthetic `HOME` and strips credential-shaped
environment variables before the subprocess spawns (see
[Lock down a capsule's capabilities](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment)),
but nothing prevents the subprocess from reading files outside the workdir, executing an
unlisted binary, or connecting to a host outside `capabilities.network.allow`.

---

## W-SEC-001 — No kernel sandbox on this platform { #w-sec-001 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the
Environment-only tier (macOS, Windows, or any non-Linux host).

**Why it matters:** shell subprocesses on this host get environment-level protection only — no
kernel enforcement constrains what they can read, execute, or reach on the network.

**What to do:** treat `capabilities.shell.allow` and `capabilities.network.allow` as advisory on
this platform, not a security boundary. For real enforcement, run the capsule on a Linux host
with kernel ≥5.13. If the capsule ingests untrusted external content, use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) instead of relying on the
allowlists to contain a compromised subprocess.

---

## W-SEC-002 — Landlock unavailable, filesystem scope unenforced { #w-sec-002 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the
Seccomp-only tier (Linux, kernel <5.13).

**Why it matters:** filesystem reads/writes outside the capsule workdir are not kernel-enforced at
all on this tier — Landlock requires kernel ≥5.13. The seccomp exec/network enforcement that
*would* apply here has never been verified on real Linux hardware (see
[W-SEC-005](#w-sec-005)), so treat shell subprocess isolation as experimental on this host.

The capability drop described under [W-SEC-005](#w-sec-005) *does* apply on this tier — it is a
`prctl`/`capset` sequence in the forked child, independent of Landlock — so a root-operated `mur run`
here still hands its shell subprocess an empty capability set. That is also not yet team-verified;
the capability half of
[Manual acceptance procedure — workdir device-node escape](#manual-acceptance-device-node) is
runnable on this tier.

**What to do:** upgrade the host kernel to 5.13+ (moves you to the Full tier), but do not treat
either Linux tier as a verified boundary until a real Linux run confirms the enforcement works.
Treat filesystem scope, and for now exec/network scope too, as advisory on this host.

---

## W-SEC-003 — `bash` bypasses the network allowlist { #w-sec-003 }

**Fires when:** `capabilities.shell.allow` contains `"bash"` and `capabilities.network.allow` is
non-empty, on a host where network access isn't kernel-enforced (the Environment-only tier —
see [W-SEC-001](#w-sec-001)). On the Full and Seccomp-only tiers, bash's outbound connections are
*intended* to be seccomp-enforced against the same allowlist, so this specific warning does not
fire there — but note that Linux enforcement is itself unverified ([W-SEC-005](#w-sec-005)), so
the bypass may still be live on a Linux host until the enforcement is confirmed.

**Why it matters:** `capabilities.network.allow` constrains requests the runtime itself makes
(WASI HTTP calls from tool/driver components). It does not constrain a `bash` subprocess's own
outbound connections on this tier — `bash` can reach any host regardless of what
`network.allow` declares. This is finding **C-7** from `murmur-security-assessment.md`.

**Maximum-risk combination:** `bash` in `shell.allow` combined with any external-fetch
capability (`network.allow`, or a tool/driver artifact that fetches independently) gives a
capsule both exposure to untrusted content and unchecked shell authority to act on it — see the
[manifest-schema threat model](manifest-schema.md#threat-model) for the full picture alongside
prompt-injection finding C-4.

**What to do:** run on a Linux host with kernel enforcement (see [W-SEC-001](#w-sec-001)), or
avoid pairing `bash` with a non-empty `network.allow` on platforms without it.

---

## W-SEC-004 — Literal secret in manifest { #w-sec-004 }

**Fires when:** `mur build` scans `murmur.yaml` and finds a credential-shaped field
(`api_key`, `token`, `secret`, `password`, or a value matching a known API-key prefix like
`sk-ant-`) set to a literal string instead of a `${VAR_NAME}` reference.

**Why it matters:** a literal secret in `murmur.yaml` ships inside the built artifact and is easy
to accidentally commit to version control.

**What to do:** replace the literal value with a `${VAR_NAME}` reference and inject the real
value via environment at run time. The build still succeeds — this is a warning, not a blocker —
but the artifact should not be published or committed until the literal is removed.

---

## W-SEC-005 — Linux kernel enforcement is not yet team-verified { #w-sec-005 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the **Full** tier
(Linux, kernel ≥5.13 with Landlock available). This tier used to emit no warning at all — silence
implied everything was confirmed-enforced, which is precisely the false assurance this warning
exists to prevent.

**Why it matters:** the Landlock + seccomp enforcement layer is implemented and unit-tested, but
has **not yet been verified by the team on real Landlock-capable Linux hardware**. On this tier
the Landlock scope grants the capsule workdir a near-full access set **and** a narrow, *derived*
read+execute grant for exactly the `shell.allow` binaries, their ELF interpreter (dynamic loader),
and the transitive closure of their shared libraries — so an allowlisted program can exec and
dynamically link outside the workdir, while nothing outside the workdir is writable and no directory
is granted wholesale. (An earlier revision granted only the workdir, which would have denied every
allowlisted binary its own `execve`; that has been fixed.) What remains unverified is whether this
derived-grant mechanism behaves as intended end to end on a real Tier-1 host — the acceptance run
happens after this ships, not as part of it — so until then, do not rely on the
filesystem/exec/network isolation it provides as a hardened security boundary.

**"Near-full", not full — the workdir device-node hole.** An earlier revision granted the workdir
the *complete* Landlock ABI v1 right-set, which includes `MakeChar` and `MakeBlock`. A capsule
running as root could therefore `mknod` a block-device node for the host's own disk (e.g. `8:0` =
`sda`) *inside* its workdir — Landlock permits it, because the new inode lives beneath a granted
path — then `open()` that node and read the entire raw host filesystem, bypassing the workdir scope
completely. Two independent mechanisms now close this, and **neither is team-verified yet**:

1. The workdir rule's granted right-set is written out explicitly and withholds `MakeChar`,
   `MakeBlock` and `MakeSock`. Because `handle_access` still declares the full ABI v1 set, those
   three are *denied* in the capsule's Landlock domain, not merely un-granted. `MakeFifo` stays
   granted — real build tooling creates named pipes in its working tree.
2. Before `execve`, the forked shell child drops its entire capability **bounding set**, clears its
   permitted/effective/inheritable capability sets via `capset(2)`, and sets `no_new_privs`. This is
   independent of Landlock and applies on **both** Linux tiers, including
   [Seccomp-only](#w-sec-002).

**Is `CAP_MKNOD` the only gate? Yes, for the device half.** `mknod(2)` for `S_IFBLK`/`S_IFCHR`
always requires `CAP_MKNOD` in the caller's effective set, independently of Landlock. A genuinely
non-root capsule — no ambient or inherited `CAP_MKNOD`, no `setcap`'d binary in its exec path —
could never create a device node, before this fix or after it. The exposure was specifically
**root-operated `mur run`** deployments (CI runners, some service deployments), which keep the full
root capability set by default. Mechanism 2 is defense-in-depth that additionally covers a non-root
capsule handed an unexpected ambient `CAP_MKNOD` — for example a systemd unit with
`AmbientCapabilities=CAP_MKNOD`.

**Known open question — unix sockets.** Withholding `MakeSock` means a subprocess cannot `bind()` an
`AF_UNIX` socket file inside the workdir. Some build tooling and some language-toolchain daemons do
exactly that, so this is the one withheld right that could plausibly break a real workload. The
acceptance procedure below tests for it explicitly; if the team's real-hardware run finds a workload
that needs it, `MakeSock` goes back into the workdir grant (`WORKDIR_ACCESS_RIGHTS` in
`crates/capsule-runtime/src/sandbox.rs`) and this section gets updated. `MakeChar`/`MakeBlock` are
not up for reconsideration.

**Side effect worth knowing about:** dropping the capability sets also removes `CAP_DAC_OVERRIDE`
from a root-run capsule's shell subprocess, so it no longer bypasses ordinary file-permission
checks. That is intended, but it is a real behavior change for root deployments whose shell steps
relied on root's usual "can read anything" posture.

**What to do:** until the layer is verified end to end on real Linux, apply the same discipline you
would on the Environment-only tier: prefer specific binary declarations over `bash`, keep
`network.allow`/`filesystem.scope` minimal, and use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) for capsules that ingest
untrusted content. Do not run `mur run` as root if you can avoid it — non-root was never exposed to
the device-node escape above. The Seccomp-only tier ([W-SEC-002](#w-sec-002)) carries the same
not-yet-verified caveat plus an additional filesystem gap (no Landlock at all).

---

## W-SEC-006 — Inert sub-key in a hook's `capabilities:` block { #w-sec-006 }

**Fires when:** a `runtime: hook` artifact entry's `capabilities:` block declares
`shell`, `spawn`, `env`, or `limits`. Per-hook grants (see
[`artifacts[].capabilities`](manifest-schema.md#hook-capabilities)) only read `network` and
`filesystem` — the other sub-blocks are structurally accepted (for vocabulary consistency with
the capsule-wide `capabilities:` block) but nothing enforces them per-hook.

**Why it matters:** an operator who declares, say, `capabilities.shell.allow` on a hook entry
expecting it to scope that hook's shell access would otherwise have no signal that the runtime
never reads it there — it is silently inert rather than rejected.

**What to do:** remove the inert sub-key from the hook's entry. If you need to scope
shell/spawn/env/limits at all, that is a capsule-wide concern today — use the top-level
`capabilities:` block instead. See [Hook capabilities](manifest-schema.md#hook-capabilities) for
the full rules on what a per-hook grant does and does not cover.

---

## W-SEC-007 — Per-artifact network entry outside the capsule ceiling { #w-sec-007 }

**Fires when:** a `runtime: tool` or `runtime: driver` entry's `capabilities.network.allow` names
an entry the capsule-wide top-level `capabilities.network.allow` does not itself allow. Per-artifact
capabilities *narrow* (see [Tool and driver capabilities](manifest-schema.md#tool-capabilities)):
the effective grant is `declaration ∩ ceiling`, so the uncovered entry is dropped from that
artifact's grant rather than granted. Staging continues.

A bare host (`api.example.com`) is broader than a scheme-bound ceiling entry
(`https://api.example.com`) because it spans both schemes and every port, so it is *not* covered
and will be dropped — this is the most common way to hit this warning by accident.

**Why it matters:** the artifact ends up with **less** access than the entry asked for. Nothing is
widened, so this is never an escalation — but a tool that silently cannot reach a host it was
"granted" fails in a way that is hard to trace back to the manifest.

**What to do:** either add the host to the capsule-wide `capabilities.network.allow` (if the whole
capsule should be able to reach it), or make the per-artifact entry at least as specific as the
ceiling entry it should sit under, or delete it if the drop was what you actually wanted.

Like [`W-SEC-006`](#w-sec-006), this fires during artifact staging — before the session workdir
exists — so it goes to stderr only, not to `logs/bootstrap.log`.

---

## W-SEC-008 — Unapplied per-artifact grant on a tool or driver { #w-sec-008 }

**Fires when:** either

- a `runtime: tool`/`runtime: driver` entry's `capabilities:` block declares `shell`, `spawn`,
  `env`, or `limits` — per-artifact narrowing only reads `network` and `filesystem`, exactly as
  per-hook grants do ([`W-SEC-006`](#w-sec-006) is the hook-side twin of this case); or
- a `runtime: tool` entry with a **native** (non-WASM) implementation declares `capabilities:` at
  all. A native tool runs as a host subprocess under the capsule-wide shell/sandbox machinery, not
  through the WASI tool path narrowing is applied on, so the whole block is inert.

**Why it matters:** a declared-but-unenforced grant reads like a scoped artifact. In the native
case in particular, the tool keeps the full capsule ceiling despite an entry that looks like it
locked it down.

**What to do:** for an inert sub-key, remove it — `shell`/`spawn`/`env`/`limits` are capsule-wide
concerns, so use the top-level `capabilities:` block. For a native tool, scope it through
`capabilities.shell.*` on the capsule-wide block instead, or ship the tool as WASM if you need
per-artifact narrowing.

Like [`W-SEC-006`](#w-sec-006), this fires during artifact staging — before the session workdir
exists — so it goes to stderr only, not to `logs/bootstrap.log`.

---

## W-SEC-009 — Interpreter-runtime grant couples the capsule to a host layout { #w-sec-009 }

**Fires when:** the capsule's top-level `capabilities.shell.interpreter_runtime` declares one or
more grants. Fires once per grant, from both `mur run` (at staging) and `mur doctor`.

**Why it matters:** an `interpreter_runtime` grant widens an already-allowlisted binary's Landlock
scope to specific host directories *outside* the workdir so a path-based interpreter (e.g. CPython)
can reach its standard library — the `DT_NEEDED` closure alone reaches only an ELF's linked
libraries, never the `.so` extension modules the interpreter `dlopen`s at import time or the
pure-Python stdlib files it discovers by listing each `sys.path` entry. That makes the grant
necessary to run such an interpreter, but it also **couples the capsule to a specific host
distro/interpreter-version layout**: a grant naming `/usr/lib/python3.11` stops resolving the moment
the host ships Python 3.12, and a capsule that runs on Debian may not run on Alpine. This is the
honest cost, and the reason the durable fix is the still-unbuilt staged runtime bind-mount — this
grant exists only to bridge until that lands.

**What it grants, exactly:** one Landlock rule per named directory, and nothing else. Each directory
carries its own required `list_dir`:

- `list_dir: true` → `Execute + ReadFile + ReadDir`. The directory's own entries are enumerable
  (what CPython's `FileFinder` needs for a `sys.path` entry).
- `list_dir: false` → `Execute + ReadFile`. Files inside can still be opened **by exact name**
  (Landlock's read rights apply to the subtree beneath a granted directory), but the directory
  itself cannot be listed.

There is no field that accepts a prefix and expands it, and `ReadDir` is never inferred — it is
granted only where an author wrote `list_dir: true`, and only on that one directory, never on its
parent or siblings. A directory not named in the manifest receives no rule at all.

**What to do:** name the narrowest set of directories that actually works — measure the real
requirement with `strace -f -e trace=openat,getdents64 <interpreter> -c "import ..."` rather than
guessing, and set `list_dir: false` on any directory you only open known files inside (do not
reflexively set `list_dir: true` "to be safe" — it only changes whether the directory can be
*enumerated*, and files inside a `list_dir: false` directory are still openable by name). Accept
that the capsule is now pinned to this host's interpreter layout, and plan to drop the grant once
the staged runtime bind-mount ships.

**Parse-time rejections.** A malformed `interpreter_runtime` fails `mur run`/`mur doctor` at
manifest parse time (not a warning — a hard error naming the offending value): a `binary` not
present in the same block's `shell.allow` (this mechanism narrows filesystem access alongside an
exec grant that already exists — it never itself grants exec), a `dirs[].path` that is not absolute
(does not start with `/`), a `dirs[]` entry that omits `list_dir` (enumerability is never inferred),
or an `interpreter_runtime[]` entry with an empty `dirs` list.

---

## Manual acceptance procedure — workdir device-node escape { #manual-acceptance-device-node }

This procedure confirms the two mechanisms described under [W-SEC-005](#w-sec-005) — the narrowed
Landlock workdir grant and the child capability drop — on real hardware. **It is deliberately not
automated.** There is no committed test that asserts "`mknod` is refused", and a green
`cargo test`/CI run is not evidence that any of this works: this repo's CI has never resolved to a
Linux enforcement tier where the code path is even executed. Until someone runs the steps below and
records the result, the mechanisms are *implemented*, not *verified*.

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
  `crates/capsule-runtime/src/sandbox.rs`, update the [W-SEC-005](#w-sec-005) section, and re-run
  scenario 1 to confirm the device-node refusal is unaffected (it is a separate bit — it will be).
  `MakeChar`/`MakeBlock` are not up for reconsideration either way.

### Scenario 4 — `CAP_MKNOD` is the sole gate for the device half (non-root)

This scenario needs no murmur code at all; it establishes the baseline claim in
[W-SEC-005](#w-sec-005) that a non-root capsule was never exposed to this escape.

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

Update the [W-SEC-005](#w-sec-005) and [W-SEC-002](#w-sec-002) sections, the callout at the top of
this page, and the "Verified?" column of the [tier table](#subprocess-enforcement-tiers) with what
actually happened — including a scenario-3 decision on `MakeSock`. Until that edit lands, every
"not yet team-verified" statement on this page stands as written.
