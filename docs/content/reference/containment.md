# Containment

A capsule's shell subprocesses are constrained by kernel mechanisms the host has to
provide. This page covers the enforcement tier each host resolves to, the containment
class a capsule can require of it, the fixed grants a contained capsule receives, and
how each claim is checked.

---

!!! warning "Kernel enforcement is Linux-only, and that is permanent"
    Landlock, seccomp and cgroups are Linux kernel primitives with no equivalent elsewhere, so
    every containment claim a *kernel* backs requires Linux with kernel ≥5.13. macOS and Windows
    sit permanently on the Environment-only tier.
    [Subprocess enforcement tiers](#subprocess-enforcement-tiers) is the single statement of
    what each platform gets.

## Subprocess enforcement tiers

`W-SEC-001`, `W-SEC-002`, `W-SEC-003` and `W-SEC-005` all stem from one mechanism: at capsule
launch the runtime probes the host and resolves an enforcement tier for shell subprocesses
declared under `capabilities.shell.allow`. The probe is a live capability test — a Landlock
ruleset really constructed, a namespace really created in a forked child — never a kernel version
string.

The probe sets the ceiling. A capsule then runs on the strongest tier that is both within that
ceiling and no stronger than its declared [containment class](#field-containment): the Sealed tier
is applied only to a capsule that declares `capabilities.containment: sealed`, and every other
capsule on a sealed-capable host runs on Full.

| Tier | Host | `mechanism:` | Filesystem | Exec | Network |
|---|---|---|---|---|---|
| Sealed | Linux, kernel ≥5.13, with a usable unprivileged user + mount namespace | `mountns+pivot_root+landlock+seccomp` | kernel-enforced, inside a composed root | kernel-enforced | kernel-enforced |
| Full | Linux, kernel ≥5.13 (Landlock available) | `landlock+seccomp` | kernel-enforced | kernel-enforced | kernel-enforced |
| Seccomp-only | Linux, kernel <5.13 (no Landlock) | `seccomp-only` | **not** enforced | **not** enforced¹ | kernel-enforced |
| Environment-only | macOS, Windows, any non-Linux host | `none` | **not** enforced | **not** enforced | **not** enforced |

¹ Exec is a Landlock right, so a host without Landlock has no kernel-level exec mediation at all.
Treat `capabilities.shell.allow` as advisory on a host below kernel 5.13.

The `mechanism:` column is what `mur run --explain-scope` prints, and it always reports what the
*host* can back — never what the session installed.

### Where the user namespace comes from { #userns-grant }

The Sealed tier and the capsule network namespace both need an unprivileged user namespace, and on
an AppArmor host something has to permit one. `mur run --explain-scope` prints which permission is
in effect on the line below `mechanism:`, and `--explain-scope --json` carries the same value as
`userns_grant`:

```text
Containment
  declared:  sealed
  achieved:  sealed
  floor met: yes
  mechanism: mountns+pivot_root+landlock+seccomp
  userns grant: profile_confining
```

| `userns grant:` | What permits the namespace | Scope of the permission |
|---|---|---|
| `apparmor_absent` | AppArmor is not enabled on this host | nothing was ever restricted; no profile is needed |
| `profile_confining` | the shipped `mur-sealed` AppArmor profile is confining this binary | `mur` alone — the configuration murmur ships |
| `restriction_disabled_host_wide` | `kernel.apparmor_restrict_unprivileged_userns` is `0` | every program on the machine; reported as [`W-SEC-013`](diagnostics.md#w-sec-013) |
| `withheld` | nothing | `sealed` is refused with `E-CAP-003`, and a capsule that spawns a subprocess with `E-CAP-005` |

The line is `n/a` off Linux, where AppArmor does not exist. The same value is written to
`session_start.userns_grant` in
[`trace.jsonl`](observability-schemas.md#session-trace-tracejsonl), so a finished session's record
distinguishes a `sealed` result obtained through the shipped profile from one obtained on a host
whose unprivileged-userns hardening was switched off.

A checkout build runs as `./target/debug/mur` or `./target/release/mur`, which no shipped profile
attaches to. `sudo scripts/install-dev-apparmor.sh` generates and loads a profile for exactly those
two paths, so building from source needs no host-wide sysctl.

**What the Full tier grants.** The Landlock scope grants the capsule workdir a near-full access set
**and** a narrow, *derived* read+execute grant for exactly the `shell.allow` binaries, their ELF
interpreter (dynamic loader), and the transitive closure of their shared libraries — so an
allowlisted program can exec and dynamic-link `/usr/bin/bash` and its libraries while no directory
is granted wholesale and the only writable path outside the workdir is `/dev/null` (see
[the fixed capsule device set](#capsule-device-set)). A capsule may *additionally* name specific
host directories a path-based interpreter needs (its stdlib) via
`capabilities.shell.interpreter_runtime`, which grants exactly the directories named, each with an
explicit per-directory `list_dir` flag (see [`W-SEC-009`](diagnostics.md#w-sec-009)). The workdir
grant is *not* the full Landlock right-set: character-device (`MakeChar`), block-device
(`MakeBlock`) and unix-socket (`MakeSock`) creation are withheld, so a capsule cannot create a raw
disk device node inside its own workdir and read the host filesystem through it — and, unless the
manifest declares [`capabilities.filesystem.workdir_exec: true`](#field-workdir-exec), the workdir
grant also withholds `Execute`, so nothing the capsule writes into its own workdir can be run under
any name. That withholding *is* the exec column above: `capabilities.shell.allow` is enforced by
granting `Execute` on exactly the allowlisted binaries' own paths and nowhere the capsule can write.

**What kernel-enforced filesystem scope covers, and what it does not.** Landlock mediates the
operations that touch a file — `open`, `read`, `write`, `execve` — not path resolution. A `stat`,
`access` or `readlink` on a path the capsule was never granted still succeeds and still reports that
file's metadata; only opening its contents is refused. The boundary is on reading and writing, not
on learning that a path exists.

**What every Linux tier grants.** Independently of Landlock, seccomp refuses `socket(AF_UNIX, ...)`
outright unless the manifest declares
[`capabilities.network.unix_sockets: true`](manifest.md#field-capabilities), and always
refuses `AF_NETLINK`/`AF_PACKET`, so a capsule cannot reach a host daemon socket such as
`/var/run/docker.sock` (see [W-SEC-005](diagnostics.md#w-sec-005)). Also independently of Landlock,
the forked shell child drops its entire capability bounding set, clears its
permitted/effective/inheritable sets, and sets `no_new_privs` before `execve`, so a root-operated
`mur run` does not hand the subprocess `CAP_MKNOD` (or `CAP_DAC_OVERRIDE`, or anything else) in the
first place.

Filesystem *and exec* scoping both use Landlock; network scoping uses the capsule's own network
namespace plus an egress proxy; socket-family denial uses seccomp argument matching. Underneath all
of them, the seccomp filter's default action is itself a deny — see
[Default-deny syscall allowlist](#default-deny-syscall-allowlist) — so a syscall named by none of
the mechanisms above is refused outright rather than falling through to an implicit allow.

Environment-only enforcement still gives you a synthetic `HOME` and strips credential-shaped
environment variables before the subprocess spawns (see
[Lock down a capsule's capabilities](../how-to/lock-down-capsule.md#step-2-manage-the-subprocess-environment)),
but nothing prevents the subprocess from reading files outside the workdir, executing an
unlisted binary, or connecting to a host outside `capabilities.network.allow`.

These are kernel behaviours, so the only check that means anything is a run on a real host. The
hand-run procedures that do that are listed under [Verification](#verification).

---

## Containment class { #field-containment }

A containment class is a floor requirement — "don't launch me unless the host can actually enforce
at least this much" — as opposed to a capability grant like `network.allow` or `shell.allow`, which
describe *what* is allowed once a session is running. Three classes exist, weakest to strongest:

| Class | Meaning |
|---|---|
| `advisory` | No kernel-level enforcement required. Every host satisfies this, including macOS and older Linux. |
| `scoped` | Landlock filesystem mediation + seccomp syscall filtering over the host filesystem. Requires Linux 5.13+ with a usable Landlock ABI. |
| `sealed` | Mount-namespace isolation onto a composed root, with Landlock and seccomp still applied inside it. Everything outside that root is *absent*, not merely denied. Requires Linux 5.13+ with a usable Landlock ABI and unprivileged user namespaces the process can mount inside: inside a container that needs `--cap-add SYS_ADMIN`, and on a host where AppArmor's `restrict_unprivileged_userns` is active the `mur-sealed` profile shipped with `mur` must be loaded. |

!!! note "`sealed`'s one documented exception: `/proc`"

    A private `procfs` needs a privilege an unprivileged user namespace does not have, so on most
    hosts a `sealed` root carries the host's `/proc` instead. **Host process metadata is visible
    under `/proc` inside a `sealed` capsule**, as it is under `scoped`; opens through it are still
    refused. Every other axis of the root — `/etc`, `/dev`, block devices, sockets, other users'
    homes — is absent, not merely denied. Where the runtime does hold that privilege, a private
    masked `/proc` is used and this exception does not apply.

**Declaring a floor.** Three independent sources can each declare a minimum class, and they combine
by taking the **strongest** requested — never the weakest:

1. `capabilities.containment` in `murmur.yaml` (this field)
2. `containment` in `.murmur/config.yaml`, global or project scope (see [Configuration files](config.md#configuration-files); note this key uses strongest-wins merging, not the usual project-wins rule)
3. `mur run --containment <advisory|scoped|sealed>` on the command line

Any source left undeclared contributes nothing; if all three are undeclared, the effective floor is
`advisory`. A CLI flag or workspace default can only *raise* the floor a manifest already set —
never lower it.

**Achieved class.** `mur run` derives the class the host can *actually* provide by probing the
kernel directly (never by trusting the manifest). The probe is a conjunction, and every element has
to hold for the next class up: a Landlock-capable Linux 5.13+ host achieves `scoped`; a host that
also creates an unprivileged user+mount namespace and mounts inside it — verified by really doing
it in a forked child, not by reading a version string — achieves `sealed`; every other host (older
Linux without Landlock, or macOS) achieves `advisory`. Granting a `scoped` capsule access to host
paths outside the workdir via `capabilities.shell.interpreter_runtime` never changes the achieved
class.

**One manifest property does lower it, and only lower it.**
`capabilities.filesystem.workdir_exec: true` caps the achieved class at `advisory` on every host,
including a `sealed`-capable one. The host probe still reports what the machine can do, and
`mechanism:` in `--explain-scope` still names the full tier. It is the capsule giving up the claim
`scoped` makes: with an executable workdir, `shell.allow` is no longer something the kernel can hold
the capsule to. Nothing in a manifest can ever *raise* an achieved class. See
[Executable workdirs](#field-workdir-exec).

**A weaker declaration is never silently upgraded.** On a `sealed`-capable host, a capsule
declaring `scoped` still runs with `scoped`'s mechanism — Landlock and seccomp over the host
filesystem, no composed root. Installing one anyway would delete the host paths its
`interpreter_runtime` grants legitimately name, weakening the capsule rather than strengthening it.
The achieved class reported in the trace still says what the *host* can back; the mechanism
installed follows what the capsule *asked for*.

**Refusal.** A host whose achieved class is weaker than the effective declared floor refuses the
launch with [`E-CAP-003`](diagnostics.md#e-cap-003), before any registry pull, artifact compile, or
workdir creation. The refusal names the specific missing mechanism — the AppArmor profile, a
container's absent `CAP_SYS_ADMIN`, a kernel without user namespaces — and the command that fixes
it.

A manifest that never declares `capabilities.containment` is never gated by this check — the
effective floor resolves to `advisory`, which every host satisfies.

---

## Containment and disclosure { #containment-and-disclosure }

A containment class and an export answer different questions, and only one of them is about the
capsule:

| | Bounds | Declared by | Effect on the achieved class |
|---|---|---|---|
| Containment (`capabilities.containment`) | What the capsule reaches outward — which paths, hosts and binaries it can touch | Manifest, workspace config or `--containment`, strongest wins | It *is* the achieved class |
| Disclosure (`exports.files`) | What an operator reaches inward — which files an external process may read out of the workdir | The manifest's top-level `exports:` block | None |

Declaring `exports.files` gives the agent no capability whatsoever: the runtime serves the files
itself, off the host path it already holds for the workdir, without involving the agent. `mur run --explain-scope` prints the declared export in its
`Resource plane` section, and `--explain-scope --json` carries it as `exports_files` — `null`
when nothing is exported. See [Resource plane](resource-plane.md).

### Symlinks under an export root { #export-symlinks }

What a symlink under `exports.files.root` means depends on the class the session *achieved*, which
every response reports in `x-murmur-containment`:

| Achieved class | Rule | Why |
|---|---|---|
| `scoped` | Refuse any symlink on the resolved path with `symlink_refused`, and omit symlinked entries from a listing | Host-path grants are possible and the filesystem's shape stays visible, so a symlink under the export root could target a granted host path |
| `sealed` | Follow, and serve only when the fully-resolved target is still beneath the export root | The workdir is the only writable path and there is no outside to name, so everything under the root is capsule-authored |
| `advisory` | Same rule as `sealed` | A convention on top of a convention; every response carries `advisory` so a reader knows what it is trusting |

A symlink whose target leaves the root is refused at every class: `symlink_refused` under `scoped`,
`outside_root` under the other two.

---

## The fixed capsule device set { #capsule-device-set }

On the Full tier a capsule gets exactly three device rules, fixed at compile time, with **no
manifest key** that adds a device or removes one:

| Device | Access | Why |
|---|---|---|
| `/dev/null` | read **and** write | The one deliberate exception to "nothing outside the workdir is writable". Ordinary tooling opens it for both reading and writing — a shell `2>/dev/null` redirect, a language runtime's null-device constant — and a read-only grant fails those, as an unexplained crash rather than as a policy denial. |
| `/dev/zero` | read only | Zero-fill reads and older allocators' mapping fallbacks. Nothing needs to write it. |
| `/dev/urandom` | read only | Not for `getrandom(2)` — that is a syscall and needs no filesystem grant — but because OpenSSL and older glibc paths still `open()` the device outright. |

**`/tmp` under `sealed` is not a second exception.** Inside a composed root `/tmp` is writable, but
it is the workdir under another name: the runtime binds a directory inside the session workdir
there, counted by the same `capabilities.resources.workdir_max_bytes` guard and discarded with the
session. It carries exactly the workdir's rights, so a binary written to `/tmp` is no more runnable
than one written to the workdir. `scoped` composes no root and binds nothing at `/tmp`, where it
stays denied.

**Every other device path is denied.** The capsule's Landlock domain declares the full ABI v1
right-set for itself, so a path with no matching rule is refused rather than merely un-granted.
`/dev/random`, `/dev/full`, `/dev/tty`, `/dev/console`, `/dev/mem` and every raw block device stay
denied, and a fourth device can appear only by editing the fixed set.

**On `sealed`, a different mechanism answers the same question.** A capsule that declares the
[`sealed` class](#field-containment) gets a private `/dev` **tmpfs** carrying the OCI default device
set, so the kernel-visible device namespace is the boundary rather than a per-path grant. It holds
six nodes — `null`, `zero`, `full` and `tty` readable and writable, `random` and `urandom` read-only
— plus the OCI symlinks `fd`, `stdin`, `stdout`, `stderr` and `ptmx`. `/dev/shm` is deliberately
absent: it is writable, and the session workdir is the only writable path in a composed root.
Landlock keeps running *inside* the composed root, so the sealed `/dev` carries Landlock rules of
its own — without them its device nodes would be present and unopenable. The two device sets are
independent: a `scoped` declaration on a `sealed`-capable host keeps the three-rule list above.

## The fixed sealed-tier runtime-tree grant { #sealed-runtime-tree-grant }

On `sealed` only. A composed root bind-mounts a fixed list of host runtime directories read-only —
`/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64` and `/libx32` — and Landlock installs *inside*
that root as defence in depth. Each of those entries gets one Landlock rule, fixed at compile time,
with **no manifest key** that adds a path or removes one. Unlike [`W-SEC-009`](diagnostics.md#w-sec-009)'s
`interpreter_runtime` grants, nothing here is author-declared: it fires no warning, appears in no
`--explain-scope` section, and is a property of the tier.

| Right | Granted | Why |
|---|---|---|
| `ReadFile` | yes | Open a file in the tree by name. |
| `ReadDir` | yes | A path-based runtime walks its search path; without the ability to list it, it cannot find its own standard library. |
| `Execute` | **no** | Granting it across `/usr`, `/bin` and `/sbin` would make every binary the host ships runnable inside a `sealed` session, reducing `capabilities.shell.allow` to documentation. Loading a shared library is gated by `ReadFile`, so extension modules still load. |
| every write right | no | The bind is read-only. |

**`scoped` gets none of this.** Without a composed root, Landlock applies straight over the real
host filesystem, where `/usr` is the host's own — so the grant is emitted on the sealed tier only
and is empty everywhere else.

## The fixed sealed-tier `/etc` grant { #sealed-etc-grant }

A composed root does not carry the host's `/etc`. It carries a fixed allowlist of sixteen entries,
bind-mounted read-only and each silently skipped when the host does not have it: the loader's cache
and config (`/etc/ld.so.cache`, `/etc/ld.so.conf`, `/etc/ld.so.conf.d`), the alternatives database
(`/etc/alternatives`), the TLS trust store (`/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`,
`/etc/ca-certificates.conf`), name resolution (`/etc/resolv.conf`, `/etc/hosts`,
`/etc/nsswitch.conf`), the timezone (`/etc/localtime`, `/etc/timezone`), the terminal database
(`/etc/terminfo`) and the account databases (`/etc/passwd`, `/etc/group`). Everything else under
`/etc` — `/etc/shadow`, `/etc/sudoers`, `/etc/ssh`, cloud-init credentials — is absent, and `/etc`
itself cannot be listed.

Each entry gets one Landlock rule, fixed at compile time, with **no manifest key** that adds a path
or removes one. Like the runtime-tree grant it is a property of the tier: it fires no warning and
appears in no `--explain-scope` section.

| Right | Granted | Why |
|---|---|---|
| `ReadFile` | yes, on all sixteen | Reading the file the composed root already mounted. |
| `ReadDir` | on the six directory entries only — `/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`, `/etc/ld.so.conf.d`, `/etc/alternatives`, `/etc/terminfo` | TLS trust-store lookup and terminal-database lookup enumerate their directories. The other ten are files or symlinks, where listing has no meaning. |
| `Execute` | **no** | `/etc/alternatives` is a directory of symlinks into `/usr/bin`. Granting `Execute` here would be a second, undeclared route around `capabilities.shell.allow`. |
| every write right | no | The binds are read-only. A writable `/etc/resolv.conf` inside a capsule would be a name-resolution hijack of the capsule's own egress. |

**The account databases are synthetic.** Fourteen of the sixteen entries are the host's own file,
bind-mounted read-only. `/etc/passwd` and `/etc/group` are not: both are world-readable on every
distribution, so binding the host's would hand a `sealed` capsule the machine's full account list.
The composed root carries a synthetic pair instead — an entry for `root`, and one for the uid the
capsule's subprocesses run as, under the account name `capsule`, whose home directory is the
synthetic `$HOME` the subprocess environment already sets. Username, group and `~` lookups all
resolve and agree with `$HOME`; **no host account name appears inside the capsule.**

**`scoped` gets none of this**, for the reason [the runtime-tree grant](#sealed-runtime-tree-grant)
gives: without a composed root these rules would apply to the host's own `/etc` — its real trust
store, its real `resolv.conf`, its real account databases.

**One operational consequence.** Each of these entries holds a file descriptor open while the
capsule starts, under whatever [`capabilities.resources.max_open_files`](resource-limits.md#host-resource-limits)
the manifest declared. A `sealed` capsule allowing an interpreter and a shell needs roughly seventy
descriptors to launch, and below that it is refused at startup rather than silently weakened. A
manifest with a very tight `max_open_files` may need to raise it.

## Default-deny syscall allowlist { #default-deny-syscall-allowlist }

Every mechanism above governs a specific syscall (`socket`, `mknod`) or a specific resource (the
workdir). Underneath them, the seccomp filter's default action is **deny** (`EPERM`), modelled on
the OCI/Docker default seccomp profile: only a fixed, named allowlist of syscalls is permitted, and
a syscall outside it is refused before any argument is read. Applies on **every** Linux tier
identically.

`execve`/`execveat` are ordinary allowed syscalls in that list — `capabilities.shell.allow` is
enforced by the Landlock `Execute` right instead (see [`W-SEC-011`](diagnostics.md#w-sec-011) and
[the tier table](#subprocess-enforcement-tiers)) — as are `connect`/`sendto`, whose
`capabilities.network.allow` enforcement lives in the capsule's own network namespace and egress
proxy. `socket` stays governed by the per-domain rules described above.

**A container in front of `mur run` can hide what this layer does and does not do.** A container
supplies containment the runtime does not — a masked or minimal `/dev`, its own default syscall
filter, dropped capabilities, its own mount and network namespaces. A capsule that looks well
contained inside one may be relying on the container for part of that, and the same capsule on bare
metal is contained only by what this page describes. Test the posture you intend to ship, on the
kind of host you intend to ship it on.

**Diagnosability.** A syscall refused by the default action returns `EPERM` to the caller. The
filter also turns on kernel audit logging for every non-allow action, so a denial reaches the kernel
log with the syscall number, pid and process name — provided the host is configured to log seccomp
errno actions, which `mur` does not control. Enforcement does not depend on that; only its
legibility does.

**Compatibility.** The allowlist is reconciled against containerd's default profile so that a
workload already proven to run under a container's seccomp profile keeps working under `mur run`'s
equivalent. Do not assume the syscall surface a shell workload needs is exactly the one that profile
permits: if a workload dies on an unexpected `EPERM`, the audit trail names the syscall. Widening
the allowlist is a change to the runtime, not something a manifest can do.

**What to do:** prefer specific binary declarations over `bash`, keep `network.allow` and
`filesystem.scope` minimal, and use the
[data/action phase-separation pattern](../concepts/access-control.md#threat-model) for capsules that ingest
untrusted content. Do not run `mur run` as root if you can avoid it. On the Seccomp-only tier
([W-SEC-002](diagnostics.md#w-sec-002)) this filter still applies, but the filesystem and exec gaps described
there apply alongside it.

---

## Executable workdirs { #field-workdir-exec }

`capabilities.filesystem.workdir_exec` decides one Landlock bit: whether the session workdir's own
rule carries the `Execute` right.

**The default (`false`, and what every manifest that omits the key gets).** The workdir is
readable and writable but not executable. Each binary named in `capabilities.shell.allow` gets its
own narrow read+execute grant at its real host path — the binary, its ELF interpreter, and its
`DT_NEEDED` shared-library closure — so allowlisted programs run normally from `/usr/bin` and
friends. Nothing the capsule *produces* runs. The decision is made by the kernel, on the path it
resolved itself, so there is no name to spoof and no window to race:

```console
$ # inside a capsule with `shell.allow: [bash]` and workdir_exec absent
$ cp /usr/bin/nc ./bash && ./bash -l
bash: ./bash: Permission denied
```

**The cost, stated plainly.** A binary the capsule legitimately compiled cannot run either:

```console
$ gcc -o ./hello hello.c && ./hello
bash: ./hello: Permission denied
```

**`workdir_exec: true`.** The workdir keeps `Execute`, and compile-and-run works. In exchange,
`capabilities.shell.allow` stops being an enforceable property of the capsule: anything written
into the workdir runs regardless of what the allowlist says. The runtime does not pretend
otherwise —

* the capsule's achieved containment class is `advisory`, on every host, including a
  Landlock-capable one;
* `mur run --explain-scope` reports `workdir exec: true` and the `advisory` it forced;
* `trace.jsonl`'s `session_start` carries `workdir_exec: true`;
* [`W-SEC-011`](diagnostics.md#w-sec-011) fires once at staging;
* pairing it with `capabilities.containment: scoped` (or `sealed`) refuses the launch.

```yaml
capabilities:
  filesystem:
    workdir_exec: true           # compile-and-run; shell.allow is advisory inside the workdir
  shell:
    allow:
      - bash
      - gcc
```

The refusal, when a manifest asks for both, names the manifest rather than the host — because no
host can satisfy it. See [`E-CAP-003`](diagnostics.md#e-cap-003).

**Where it has no effect.** The bit is a Landlock right, so a host with no usable Landlock ABI
(Linux < 5.13) and every non-Linux host ignore it entirely — on those hosts nothing mediates exec
either way, which is exactly why neither can reach `scoped`. `W-SEC-001` and `W-SEC-002` already
say so.

---

## Staged runtime { #field-staged-runtime }

`capabilities.shell.staged_runtime` and `capabilities.shell.interpreter_runtime` solve the same
problem — a path-based interpreter cannot run from its binary alone, because its stdlib lives
outside the workdir at a path the `DT_NEEDED` closure cannot discover — and they solve it in
opposite directions.

| | `interpreter_runtime` | `staged_runtime` |
|---|---|---|
| Direction | widens the capsule's Landlock scope **outwards** to host paths | bind-mounts the runtime tree **inwards** into the capsule's own root |
| Floor required | any (works at `scoped`) | `sealed` only |
| Host paths reachable from inside | yes, the granted directories | no |
| Coupled to one host's layout | yes — fires [`W-SEC-009`](diagnostics.md#w-sec-009) | no, and `pin` makes the coupling checkable |
| Granularity | specific directories, each with an explicit `list_dir` | the whole named tree, read-only |

Because the second makes the first unnecessary for any binary that uses it, **declaring both for
the same binary is rejected at parse time.** A composed root does not contain the host directories
an `interpreter_runtime` grant names, so the grant would describe paths that do not exist inside
the capsule.

```yaml
capabilities:
  containment: sealed              # required
  shell:
    allow:
      - python3
    staged_runtime:
      - binary: python3
        source_path: /opt/testbed/conda/envs/django__django
        pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01
```

A `staged_runtime` grant requires an effective `sealed` floor, and declaring it below one
is refused at launch with [`E-CAP-004`](diagnostics.md#e-cap-004).

**`pin` is for humans, not for the runtime.** Nothing parses it, matches it, or verifies it against
the tree at `source_path`. It exists so that "the same interpreter build ran on both hosts" is a
claim someone can check by comparing two manifests, rather than one assumed from two directories
having the same name. `mur run --explain-scope` prints every declared grant with its pin, on every
host and whatever the floor:

```text
  staged runtime:
    - python3: /opt/testbed/conda/envs/django__django (pin: conda-4.10.3/python-3.9.19/testbed-2024-05-01)
```

`--explain-scope` is a diagnostic, so it reports the grant even where `mur run` would refuse —
which is exactly the case an operator is inspecting.

Under a declared `sealed` floor, a `capabilities.shell.allow` grant that cannot function
inside the composed root is decided at staging rather than deep into a run — see
[`E-CAP-006`](diagnostics.md#e-cap-006) and [`W-SEC-012`](diagnostics.md#w-sec-012).

## Verification — how the containment claims are checked { #verification }

Landlock scoping, seccomp filtering, `pivot_root` onto a composed root, cgroup v2 resource
ceilings and file-descriptor hygiene across `exec` are all claims about what a *kernel* does, so
each one is checked against a real Linux host. Two things do the checking, and they cover different
ground.

| | Escape-conformance harness | Manual procedures |
|---|---|---|
| Form | One command, graded against a declared containment class | Written scenarios with exact commands and expected output |
| Covers | Filesystem escape, `/proc` re-open paths, inherited descriptors, device-node creation, the exec allowlist, network egress, unix sockets, the dangerous-syscall table, resource exhaustion | Launch-time refusals, the composed root observed from inside a live capsule, the fixed device set, shell-binary reachability, staged runtimes, the child's capability sets |
| Where the result lives | A dated record file the run writes | A dated entry in the procedure itself |

The rest of the test suite asserts the *decision* logic: which enforcement tier a probe resolves to,
which containment class a tier achieves, that a zero limit value is rejected.

### The escape-conformance harness

[`crates/capsule-runtime/escape-conformance`](https://github.com/murmur-nexus/murmur/tree/main/crates/capsule-runtime/escape-conformance)
drives a real capsule through a registry of escape probes and grades each verdict against what the
declared class promises. It is its own workspace root, so build and run it from its own directory:

```bash
cd crates/capsule-runtime/escape-conformance
cargo build --release
./target/release/escape-conformance --class sealed
```

`--class` takes `advisory`, `scoped` or `sealed` and is what the run is graded against.
`--list-cases` prints the registry with each case's expectation per class, and `--help` lists the
remaining options.

The run needs a built `mur` binary, `python3`, and a delegated cgroup v2 subtree — the harness wraps
each capsule in `systemd-run --user --scope --property=Delegate=yes` by default to get one. It runs
on bare metal: a host that cannot back the class under test, and a host that looks like a container,
are both refused before the first case runs, and a refused run writes no record.

The exit code separates a refusal from a failure, and an escape from a ceiling that gave way:

| exit | meaning | record |
|---|---|---|
| `0` | every asserted case matched its expected verdict | written |
| `1` | usage error, or the harness itself could not proceed | none |
| `2` | refused — this host cannot back the class, or a prerequisite is missing | none |
| `3` | a boundary case failed — a containment escape | written |
| `4` | boundary clean, a resource ceiling did not hold — denial of service | written |

### The manual procedures

Each procedure lives in the murmur repository on the `main` branch; the name links to it.

| Procedure | What it covers | When it is run |
|---|---|---|
| [**Network namespace + egress proxy**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/network-namespace-egress-proxy-manual-verification.md) | That a capsule's native subprocess tree runs inside its own network namespace, and that the only way out is a proxy in the runtime process applying `capabilities.network.allow`. Includes the `E-CAP-005` refusal on a host that cannot provide a namespace. | Whenever the namespace setup, the egress proxy, or the allow-list enforcement path changes. |
| [**Sealed containment**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/sealed-containment-manual-verification.md) | The `sealed` class end to end: the `E-CAP-003` refusal when the AppArmor profile is absent, the composed root observed from inside a live capsule's shell tool (paths outside it return `ENOENT`, not `EACCES`), `"containment_achieved":"sealed"` in `trace.jsonl`, and the refusal to run at a weaker class inside a plain container. | Release gate for the `sealed` class. Re-run whenever the composed-root construction, the host probe, or the tier→class mapping changes. |
| [**Staged runtime bind mount**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/staged-runtime-bind-mount-manual-verification.md) | That a `capabilities.shell.staged_runtime` grant lands its pinned tree read-only inside the composed root at the same absolute path it has on the host, that a missing `source_path` refuses the session with `E-RUN-014` before any shell command runs, and that the same `pin` on two hosts yields the same interpreter with no `interpreter_runtime` grant anywhere. | Whenever the staging bind, the grant's Landlock rights, or the composed-root plan changes. |
| [**Resource limits**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/resource-limits-manual-verification.md) | The three mechanisms bounding the native subprocess tree: `setrlimit(2)` per-process ceilings, the cgroup v2 scope (fork bomb, memory hog, CPU, I/O), and the periodic workdir-size check. Ten scenarios, including the `E-RUN-012` fail-closed launch refusal and the macOS gap behind `W-SEC-010`. | On a Linux host with systemd user cgroup delegation configured, whenever `capabilities.resources` enforcement or the cgroup delegation path changes. |
| [**Subprocess fd hygiene**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/subprocess-fd-hygiene-verification.md) | The negative property that a descriptor open in the runtime process at spawn time is not visible inside the spawned child, across both spawn paths (shell tool and native tool), on both kernel tiers. Landlock cannot substitute for this: an inherited fd was opened before the ruleset existed. | Whenever either spawn path's pre-exec window changes. |
| [**Workdir `Execute` rights and declared `workdir_exec`**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-exec-landlock-manual-verification.md) | That `capabilities.shell.allow` is *complete*: with the default `capabilities.filesystem.workdir_exec: false`, a binary planted in the session workdir under an allowlisted basename does not execute, because the workdir's Landlock rule carries no `Execute` right. Also the declared opt-in's whole visible surface — the binary runs, the achieved class drops to `advisory`, `--explain-scope` and `trace.jsonl` say so, and `containment: scoped` alongside it refuses with `E-CAP-003`. | Whenever the workdir grant's right set, the exec-grant derivation, or the tier→class mapping changes. |
| [**Shell-binary reachability under `sealed`**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/shell-binary-reachability-manual-verification.md) | That a `capabilities.shell.allow` grant which cannot actually *function* inside a composed root fails at launch rather than deep in a run: the `E-CAP-006` refusal for an interpreted entrypoint whose package tree nothing declared reaches, the `W-SEC-012` warning for a compiler driver whose `cc1`/`as`/`ld` helpers have no `Execute` grant, and the two negative controls — a system `/usr` interpreter that needs no grant, and a declared `interpreter_runtime` that makes a real compile succeed. | Whenever the reachability checks, the fixed sealed runtime tree's right set, or the known-driver registry changes. |
| [**Workdir device-node escape**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/workdir-device-node-manual-verification.md) | That a capsule cannot create a character- or block-device node inside its own workdir and read the raw host filesystem through it, via the Landlock workdir grant withholding those rights and the shell child's capability drop. Also that FIFO and ordinary file creation still work, and that the child is left non-dumpable. | Whenever the workdir grant's right set, the child's capability drop, or the pre-exec hardening sequence changes. |
| [**Unmediated `AF_UNIX` sockets**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/af-unix-sockets-manual-verification.md) | That a capsule cannot open a unix-domain socket by default and so cannot reach a host daemon socket such as `/var/run/docker.sock`; that `AF_NETLINK` and `AF_PACKET` are refused with and without the opt-in; that `capabilities.network.unix_sockets: true` really does hand the family back; and whether real workloads survive the default deny. | Whenever the `socket(2)`-domain rule, the `unix_sockets` opt-in, or the set of denied address families changes. |
| [**The fixed capsule device set**](https://github.com/murmur-nexus/murmur/blob/main/docs/content/reference/capsule-device-set-manual-verification.md) | That `/dev/null` is readable *and* writable, `/dev/zero` and `/dev/urandom` readable but not writable, every other device refused, and `/dev` itself not enumerable — plus that a host missing one of the three degrades rather than failing the launch, and whether three devices are enough for real workloads. | Whenever the fixed device set, its per-device rights, or the missing-device fallback changes. |
