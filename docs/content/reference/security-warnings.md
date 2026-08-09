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
| [`W-SEC-002`](#w-sec-002) | `mur run` | Linux host without Landlock — filesystem scope and exec unenforced |
| [`W-SEC-003`](#w-sec-003) | `mur run` | `network.allow` doesn't constrain bash's own outbound connections |
| [`W-SEC-004`](#w-sec-004) | `mur build` | Literal secret value found in a manifest field |
| [`W-SEC-005`](#w-sec-005) | `mur run` | What the Linux kernel enforcement layer contains, and the one key that re-widens it |
| [`W-SEC-006`](#w-sec-006) | `mur run` | A hook's `capabilities:` block declares a sub-key that is inert on hooks |
| [`W-SEC-007`](#w-sec-007) | `mur run` | A tool/driver narrowed to a host the capsule-wide ceiling does not allow — the entry was dropped |
| [`W-SEC-008`](#w-sec-008) | `mur run` | A tool/driver `capabilities:` block declares something per-artifact narrowing does not apply |
| [`W-SEC-009`](#w-sec-009) | `mur run`, `mur doctor` | `capabilities.shell.interpreter_runtime` couples the capsule to a specific host interpreter-version layout |
| [`W-SEC-010`](#w-sec-010) | `mur run` | No cgroup on this platform — the subprocess tree has no aggregate memory/pids/cpu bound |

---

!!! warning "Kernel enforcement is Linux-only, and that is permanent"
    Every containment claim on this page that a *kernel* backs requires Linux with kernel ≥5.13.
    On macOS and Windows the subprocess enforcement is environment-only by construction, forever.
    [Subprocess enforcement tiers](#subprocess-enforcement-tiers) below is the single statement of
    what each platform gets; nothing else on this page restates it.

## Subprocess enforcement tiers

**This section is the platform statement for the whole page.** `W-SEC-001`, `W-SEC-002`,
`W-SEC-003` and `W-SEC-005` all stem from one mechanism: at capsule launch the runtime probes the
host and resolves one of three enforcement tiers for shell subprocesses declared under
`capabilities.shell.allow`. The host decides which tier you get; no manifest key selects one.

**Linux with kernel ≥5.13 is the supported enforcement runtime, and the only platform this page
claims kernel enforcement for.** Landlock, seccomp and cgroups are Linux kernel primitives with no
equivalent elsewhere, so macOS and Windows sit permanently on the Environment-only tier — no kernel
enforcement at all, by construction, not a gap awaiting a future release. Windows is out of scope
beyond that one clause.

| Tier | Host | Filesystem | Exec | Network |
|---|---|---|---|---|
| Full | Linux, kernel ≥5.13 (Landlock available) | kernel-enforced | kernel-enforced | kernel-enforced |
| Seccomp-only | Linux, kernel <5.13 (no Landlock) | **not** enforced | **not** enforced¹ | kernel-enforced |
| Environment-only | macOS, Windows, any non-Linux host | **not** enforced | **not** enforced | **not** enforced |

**What the Full tier grants.** The Landlock scope grants the capsule workdir a near-full access set
**and** a narrow, *derived* read+execute grant for exactly the `shell.allow` binaries, their ELF
interpreter (dynamic loader), and the transitive closure of their shared libraries — so an
allowlisted program can exec and dynamic-link `/usr/bin/bash` and its libraries while no directory
is granted wholesale and the only writable path outside the workdir is `/dev/null` (see
[the fixed capsule device set](#capsule-device-set)). A capsule may *additionally* name specific
host directories a path-based interpreter needs (its stdlib) via
`capabilities.shell.interpreter_runtime` — but only the exact directories named, each with an
explicit per-directory `list_dir` flag, never a whole install prefix (see
[`W-SEC-009`](#w-sec-009)). The workdir grant is *not* the full Landlock right-set: character-device
(`MakeChar`), block-device (`MakeBlock`) and unix-socket (`MakeSock`) creation are withheld, so a
capsule cannot create a raw disk device node inside its own workdir and read the host filesystem
through it — and, unless the manifest declares
[`capabilities.filesystem.workdir_exec: true`](manifest-schema.md#field-workdir-exec), the workdir
grant also withholds `Execute`, so nothing the capsule writes into its own workdir can be run under
any name. That withholding *is* the exec column above: `capabilities.shell.allow` is enforced by
granting `Execute` on exactly the allowlisted binaries' own paths and nowhere the capsule can write.

**What kernel-enforced filesystem scope covers, and what it does not.** Landlock mediates the
operations that touch a file — `open`, `read`, `write`, `execve` — not path resolution. A `stat`,
`access` or `readlink` on a path the capsule was never granted still succeeds and still reports that
file's metadata; only opening its contents is refused. The boundary is on reading and writing, not
on learning that a path exists.

**What both Linux tiers grant.** Independently of Landlock, seccomp refuses `socket(AF_UNIX, ...)`
outright unless the manifest declares
[`capabilities.network.unix_sockets: true`](manifest-schema.md#field-capabilities), and always
refuses `AF_NETLINK`/`AF_PACKET`, so a capsule cannot reach a host daemon socket such as
`/var/run/docker.sock` (see [W-SEC-005](#w-sec-005)). Also independently of Landlock, the forked
shell child drops its entire capability bounding set, clears its permitted/effective/inheritable
sets, and sets `no_new_privs` before `execve`, so a root-operated `mur run` does not hand the
subprocess `CAP_MKNOD` (or `CAP_DAC_OVERRIDE`, or anything else) in the first place.

¹ Exec is a Landlock right, so a host without Landlock has no kernel-level exec mediation at all.
Treat `capabilities.shell.allow` as advisory on a host below kernel 5.13.

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
hand-run procedures that do that are listed on the [Verification](verification.md) page.

---

## W-SEC-001 — No kernel sandbox on this platform { #w-sec-001 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the
Environment-only tier — see [Subprocess enforcement tiers](#subprocess-enforcement-tiers) for which
hosts those are and why it is permanent there.

**Why it matters:** shell subprocesses on this host get environment-level protection only — a
synthetic `HOME` and credential env-stripping. No kernel enforcement constrains what they can read,
execute, or reach on the network.

**What to do:** treat `capabilities.shell.allow` and `capabilities.network.allow` as advisory on
this platform, not a security boundary. For kernel enforcement, run the capsule on a host that
resolves to an enforcing tier (see
[Subprocess enforcement tiers](#subprocess-enforcement-tiers)). If the capsule ingests untrusted
external content, use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) instead of relying on the
allowlists to contain a compromised subprocess.

---

## W-SEC-002 — Landlock unavailable, filesystem scope unenforced { #w-sec-002 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the Seccomp-only
tier — see [Subprocess enforcement tiers](#subprocess-enforcement-tiers).

**Why it matters:** filesystem reads/writes outside the capsule workdir are not kernel-enforced at
all on this tier. **Nor is exec:** `capabilities.shell.allow` is enforced by granting the Landlock
`Execute` right on exactly the allowlisted binaries, so without Landlock there is no exec mediation
here and a shell subprocess can run any binary its uid can reach.

The [fixed capsule device set](#capsule-device-set) does **not** apply on this tier either, and the
direction of that gap is worth being explicit about: it is a Landlock rule list, so without Landlock
there is nothing to grant *and* nothing to deny. A capsule here can open `/dev/null`, `/dev/zero`
and `/dev/urandom` — but equally `/dev/random`, `/dev/mem` and any raw block device its uid can
reach. The Full tier's device set is a *narrowing* of this tier's behavior, never a widening of it.

Two things the Full tier has *do* survive here, because neither involves Landlock:

- the capability drop — a `prctl`/`capset` sequence in the forked child, so a root-operated
  `mur run` still hands its shell subprocess an empty capability set;
- the `socket(2)` domain denial — `AF_UNIX` refused with `EACCES` unless
  `capabilities.network.unix_sockets: true` is declared, `AF_NETLINK`/`AF_PACKET` refused
  unconditionally. It behaves *identically* on this tier and the Full tier, so a host stuck below
  kernel 5.13 is not exposed to the `/var/run/docker.sock` escape.

**What to do:** upgrade the host kernel to move to the Full tier. Until then, treat filesystem scope
and exec scope as advisory on this host — neither has a mechanism here. This is also why this tier
cannot reach the `scoped` containment class.

---

## W-SEC-003 — `bash` bypasses the network allowlist { #w-sec-003 }

**Fires when:** `capabilities.shell.allow` contains `"bash"` and `capabilities.network.allow` is
non-empty, on a host where network access isn't kernel-enforced (the Environment-only tier —
see [W-SEC-001](#w-sec-001)). On the enforcing tiers a `bash` subprocess's own outbound connections
*are* constrained by the same allowlist, so this warning does not fire there.

**Why it matters:** `capabilities.network.allow` constrains requests the runtime itself makes
(WASI HTTP calls from tool/driver components). It does not constrain a `bash` subprocess's own
outbound connections on this tier — `bash` can reach any host regardless of what
`network.allow` declares. This is finding **C-7** from `murmur-security-assessment.md`.

**What the allowlist covers where it *is* enforced.** `capabilities.network.allow` governs **IP
destinations only — TCP and UDP alike**, decided by destination address and port at `connect(2)` and
`sendto(2)`, through the capsule's own network namespace and egress proxy. It is not a full egress
control: unix-domain sockets are a separate capability
([`capabilities.network.unix_sockets`](manifest-schema.md#field-capabilities), default `false`,
see [W-SEC-005](#w-sec-005)), and `AF_NETLINK`/`AF_PACKET` are refused outright with no key to
re-enable them. An empty `network.allow` therefore does not mean "no communication" — it means no
TCP or UDP destination is reachable.

**Maximum-risk combination:** `bash` in `shell.allow` combined with any external-fetch
capability (`network.allow`, or a tool/driver artifact that fetches independently) gives a
capsule both exposure to untrusted content and unchecked shell authority to act on it — see the
[manifest-schema threat model](manifest-schema.md#threat-model) for the full picture alongside
prompt-injection finding C-4.

**What to do:** run on a host that resolves to an enforcing tier (see
[Subprocess enforcement tiers](#subprocess-enforcement-tiers)), or avoid pairing `bash` with a
non-empty `network.allow` on platforms without one.

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

## W-SEC-005 — What the Full tier enforces, and the one key that re-widens it { #w-sec-005 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the **Full** tier
(see [Subprocess enforcement tiers](#subprocess-enforcement-tiers)).

**What this tier enforces.** Four kernel mechanisms, all applied at launch:

- **A Landlock workdir scope plus a derived exec grant.** The session workdir gets a near-full
  access set — near-full because it withholds character-device, block-device and unix-socket
  creation, and withholds `Execute` unless the manifest declares
  [`capabilities.filesystem.workdir_exec: true`](manifest-schema.md#field-workdir-exec). Outside it,
  a narrow *derived* read+execute grant covers exactly the `shell.allow` binaries, their dynamic
  loader, and the transitive closure of their shared libraries. Those two together are the whole of
  exec enforcement here: `Execute` where the operator named a binary, nowhere the capsule can write.
- **A fixed device set.** `/dev/null` read **and** write, `/dev/zero` and `/dev/urandom` read-only,
  no other device at all — see [the fixed capsule device set](#capsule-device-set).
- **A `socket(2)`-domain deny.** `AF_UNIX`, `AF_NETLINK` and `AF_PACKET` are refused at socket
  creation, before any `connect()` is attempted. It is a plain seccomp rule with no Landlock
  involvement, so it applies identically on **both** Linux tiers.
- **A default-deny syscall allowlist.** The seccomp filter's default action is a deny, so a syscall
  named by none of the mechanisms above is refused rather than falling through to an implicit allow
  — see [Default-deny syscall allowlist](#default-deny-syscall-allowlist).

**The one documented exception.**
[`capabilities.network.unix_sockets: true`](manifest-schema.md#field-capabilities) re-widens the
`AF_UNIX` half of the domain deny. Nothing else on this list has an opt-out:

| `socket(2)` domain | Default | Can a manifest widen it? |
|---|---|---|
| `AF_UNIX` | denied (`EACCES`) | yes — `capabilities.network.unix_sockets: true` |
| `AF_NETLINK` | denied (`EACCES`) | **no** |
| `AF_PACKET` | denied (`EACCES`) | **no** |
| `AF_INET`, `AF_INET6`, everything else | unaffected | governed by `capabilities.network.allow` — TCP and UDP by IP destination, see [`W-SEC-003`](#w-sec-003) |

That key is coarse on purpose: it is a whole address family, not a per-socket-path allowlist, so
declaring it `true` re-exposes every unix socket the process can reach — `/var/run/docker.sock`,
which is host root, included. Landlock cannot substitute for it at any ABI, because ABI v6's
`LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` scopes *abstract* unix sockets only and `docker.sock` is a
pathname socket. Declare it only when a shell tool genuinely needs a local daemon socket. `AF_UNIX`
is also how glibc's NSS reaches `nscd` and how `syslog(3)` reaches `/dev/log`, so the default deny
is the mechanism on this list most likely to interfere with an existing workload.

**Side effect worth knowing about:** the shell child's capability drop also removes
`CAP_DAC_OVERRIDE` from a root-run capsule's subprocess, so it no longer bypasses ordinary
file-permission checks. That is intended, but it is a real behavior change for root deployments
whose shell steps relied on root's usual "can read anything" posture.

**What to do:** keep `shell.allow`, `network.allow` and `filesystem.scope` as narrow as the task
genuinely needs, and don't run `mur run` as root if you can avoid it — a non-root capsule never had
the `CAP_MKNOD` the workdir device-node restriction exists to backstop.

### The fixed capsule device set { #capsule-device-set }

A capsule on this tier gets exactly three device rules, fixed at compile time, with **no manifest
key** that adds a device or removes one:

| Device | Access | Why |
|---|---|---|
| `/dev/null` | read **and** write | The one deliberate exception to "nothing outside the workdir is writable". Ordinary tooling opens it for both reading and writing — a shell `2>/dev/null` redirect, a language runtime's null-device constant — and a read-only grant fails those, as an unexplained crash rather than as a policy denial. |
| `/dev/zero` | read only | Zero-fill reads and older allocators' mapping fallbacks. Nothing needs to write it. |
| `/dev/urandom` | read only | Not for `getrandom(2)` — that is a syscall and needs no filesystem grant — but because OpenSSL and older glibc paths still `open()` the device outright. |

Granting write on `/dev/null` gives up no confidentiality and no integrity: the write side of that
character device is defined to discard, so there is no state behind it to reach, corrupt, or read
back. It is the narrowest possible exception — one inode, not `/dev`, not a directory.

**`/tmp` under `sealed` is not a second exception.** Inside a composed root `/tmp` is writable, but
it is the workdir under another name: the runtime binds a directory inside the session workdir
there, counted by the same `capabilities.resources.workdir_max_bytes` guard and discarded with the
session. It carries exactly the workdir's rights, so a binary written to `/tmp` is no more runnable
than one written to the workdir. `scoped` composes no root and binds nothing at `/tmp`, where it
stays denied.

**Every other device path is denied, and that needs no extra rule.** The capsule's Landlock domain
declares the full ABI v1 right-set for itself, so a path with no matching rule is *refused*, not
merely un-granted. This mechanism only ever *adds* rules, and it adds exactly three. So
`/dev/random`, `/dev/full`, `/dev/tty`, `/dev/console`, `/dev/mem` and every raw block device stay
denied by the same mechanism that already denied them — there is no separate deny list to keep in
sync, and no way for a fourth device to appear except by editing the fixed set.

**Why `/dev/random` is excluded.** Since Linux 5.6 `/dev/random` blocks until the kernel RNG is
initialized while `/dev/urandom` does not, and no workload needs the blocking variant when the
non-blocking one is granted. It is excluded because nothing has demonstrated a need for it, which is
the standard every device on this list is held to. A capsule cannot poison the kernel entropy pool
through it either way: crediting entropy needs a privileged ioctl, and the shell child has dropped
every capability before it runs.

**On `sealed`, a different mechanism answers the same question.** A capsule that declares the
[`sealed` class](manifest-schema.md#field-containment) gets a private `/dev` **tmpfs** carrying the
OCI default device set, so the kernel-visible device namespace is the boundary rather than a
per-path grant. The two device sets are independent, and deliberately so: a `scoped` declaration on
a `sealed`-capable host keeps `scoped`'s three-rule list, because the two answer different questions
(a Landlock rule list over the host's `/dev` versus the entire contents of a private one). Landlock
keeps running *inside* the composed root, so the sealed `/dev` carries Landlock rules of its own —
without them its device nodes would be present and unopenable.

### The fixed sealed-tier runtime-tree grant { #sealed-runtime-tree-grant }

On `sealed` only. A composed root bind-mounts a fixed list of host runtime directories read-only —
`/usr`, `/bin`, `/sbin`, `/lib`, `/lib32`, `/lib64` and `/libx32` — and Landlock installs *inside*
that root as defence in depth. Each of those entries gets one Landlock rule, fixed at compile time,
with **no manifest key** that adds a path or removes one. Unlike [`W-SEC-009`](#w-sec-009)'s
`interpreter_runtime` grants, nothing here is author-declared: it fires no warning, appears in no
`--explain-scope` section, and is a property of the tier.

| Right | Granted | Why |
|---|---|---|
| `ReadFile` | yes | Open a file in the tree by name. |
| `ReadDir` | yes | A path-based runtime walks its search path; without the ability to list it, it cannot find its own standard library. |
| `Execute` | **no** | Withheld deliberately — see below. |
| every write right | no | The bind is read-only regardless, so the tree is immutable for two independent reasons. |

**Why `Execute` is withheld.** An `Execute` rule *is* the exec allowlist: a binary with one runs, a
binary without one does not. Granting `Execute` across `/usr`, `/bin` and `/sbin` would make every
binary the host ships runnable inside a `sealed` session and reduce `capabilities.shell.allow` to
documentation. Withholding it costs an interpreter nothing — loading a shared library is gated by
`ReadFile`, not `Execute`, so extension modules still load. This is the only grant in the runtime
that is readable and enumerable but not runnable.

**What enumerating this tree does not widen.** The tree is inside the composed root: it holds the
read-only runtime the root was built from and nothing else, and everything outside it is *absent*
rather than denied. A listing there reveals the staged runtime, not the host's shape. The composed
root's own top level stays unreadable, and paths never mounted into it — home directories among
them — do not exist inside the capsule at all.

**`scoped` gets none of this.** A `scoped` capsule has no composed root, so Landlock there applies
straight over the real host filesystem, where `/usr` is the host's own. Granting `ReadDir` on it
would newly expose host directory shape to every `scoped` capsule, so the grant is emitted only on
the sealed tier and is empty everywhere else.

### The fixed sealed-tier `/etc` grant { #sealed-etc-grant }

A composed root does not carry the host's `/etc`. It carries a fixed allowlist of sixteen entries,
bind-mounted read-only and each silently skipped when the host does not have it: the loader's cache
and config (`/etc/ld.so.cache`, `/etc/ld.so.conf`, `/etc/ld.so.conf.d`), the alternatives database
(`/etc/alternatives`), the TLS trust store (`/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`,
`/etc/ca-certificates.conf`), name resolution (`/etc/resolv.conf`, `/etc/hosts`,
`/etc/nsswitch.conf`), the timezone (`/etc/localtime`, `/etc/timezone`), the terminal database
(`/etc/terminfo`) and the account databases (`/etc/passwd`, `/etc/group`). Everything else under
`/etc` — `/etc/shadow`, `/etc/sudoers`, `/etc/ssh`, cloud-init credentials, and every future
addition — is **absent by construction**, never by enumeration in a denylist.

Each entry gets one Landlock rule, fixed at compile time, with **no manifest key** that adds a path
or removes one. Like the runtime-tree grant it is a property of the tier: it fires no warning and
appears in no `--explain-scope` section.

| Right | Granted | Why |
|---|---|---|
| `ReadFile` | yes, on all sixteen | Reading the file the composed root already mounted. |
| `ReadDir` | on the six directory entries only — `/etc/ssl`, `/etc/pki`, `/etc/ca-certificates`, `/etc/ld.so.conf.d`, `/etc/alternatives`, `/etc/terminfo` | TLS trust-store lookup and terminal-database lookup enumerate their directories. The other ten are files or symlinks, where listing has no meaning. |
| `Execute` | **no** | `/etc/alternatives` is a directory of symlinks into `/usr/bin`. Granting `Execute` here would be a second, undeclared route around `capabilities.shell.allow` — the same hole the [runtime-tree grant](#sealed-runtime-tree-grant) withholds `Execute` to close. |
| every write right | no | The binds are read-only. A writable `/etc/resolv.conf` inside a capsule would be a name-resolution hijack of the capsule's own egress. |

**What this widens.** Fourteen of the sixteen entries are the host's own file, bind-mounted
read-only, and none is sensitive: a CA bundle, a timezone, a loader cache and a terminal database
are public data on any host.

The remaining two — `/etc/passwd` and `/etc/group` — are **not** the host's. Both are world-readable
on every distribution, so binding the host's would hand a `sealed` capsule the machine's full
account list. The composed root carries a synthetic pair instead: an entry for `root`, and one for
the uid the capsule's subprocesses run as, whose home directory is the synthetic `$HOME` the
subprocess environment already sets. Username, group and `~` lookups all resolve and agree with
`$HOME`; **no host account name appears inside the capsule.**

The boundary around them is unchanged: `/etc` itself cannot be listed — only the specific entries
mounted beneath it are readable — and paths that were never mounted, `/etc/shadow` among them, do
not exist inside the capsule.

**`scoped` gets none of this**, for the reason [the runtime-tree grant](#sealed-runtime-tree-grant)
gives: without a composed root these rules would apply to the host's own `/etc` — its real trust
store, its real `resolv.conf`, its real account databases.

**One operational consequence.** Each of these entries holds a file descriptor open while the
capsule starts, under whatever `capabilities.resources.max_open_files` the manifest declared. A
`sealed` capsule allowing an interpreter and a shell needs roughly seventy descriptors to launch,
and below that it is refused at startup rather than silently weakened. A manifest with a very tight
`max_open_files` may need to raise it.

### Default-deny syscall allowlist { #default-deny-syscall-allowlist }

Every mechanism above governs a specific syscall (`socket`, `mknod`) or a specific resource (the
workdir). Underneath them, the seccomp filter's default action is **deny** (`EPERM`), modelled on
the OCI/Docker default seccomp profile: only a fixed, named allowlist of syscalls is permitted, and
a syscall outside it is refused before any argument is read. Applies on **both** Linux tiers
identically.

`execve`/`execveat` are ordinary allowed syscalls in that list — `capabilities.shell.allow` is
enforced by the Landlock `Execute` right instead (see [`W-SEC-011`](#w-sec-011) and
[the tier table](#subprocess-enforcement-tiers)) — as are `connect`/`sendto`, whose
`capabilities.network.allow` enforcement lives in the capsule's own network namespace and egress
proxy. `socket` stays governed by the per-domain rules described above.

**A container in front of `mur run` can hide what this layer does and does not do.** Running a
capsule inside a container means two independent things are restricting it, and only one of them is
the capsule runtime. A container supplies containment the runtime does not — a masked or minimal
`/dev`, its own default syscall filter, dropped capabilities, its own mount and network namespaces.
A capsule that looks well contained inside one may be relying on the container for part of that,
and the same capsule on bare metal is contained only by what this page describes. Test the posture
you intend to ship, on the kind of host you intend to ship it on.

**Diagnosability.** A syscall refused by the default action returns `EPERM` to the caller. To make
a denial attributable after the fact, the filter also turns on kernel audit logging for every
non-allow action, so a denial reaches the kernel log with the syscall number, pid and process name
— provided the host is configured to log seccomp errno actions, which `mur` does not control.
Enforcement does not depend on that; only its legibility does.

**Compatibility is the load-bearing risk here, not security.** The allowlist is reconciled against
containerd's default profile so that a workload already proven to run under a container's seccomp
profile keeps working under `mur run`'s equivalent. Do not assume the syscall surface a shell
workload needs is exactly the one that profile permits: if a workload dies on an unexpected
`EPERM`, the audit trail names the syscall. Widening the allowlist is a change to the runtime, not
something a manifest can do.

**What to do:** prefer specific binary declarations over `bash`, keep `network.allow` and
`filesystem.scope` minimal, and use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) for capsules that ingest
untrusted content. Do not run `mur run` as root if you can avoid it. On the Seccomp-only tier
([W-SEC-002](#w-sec-002)) this filter still applies, but the filesystem and exec gaps described
there apply alongside it.

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
honest cost, and the reason the durable fix is
[`capabilities.shell.staged_runtime`](manifest-schema.md#field-staged-runtime), which bind-mounts a
pinned runtime tree into the capsule's own composed root instead of reaching out to the host's — this
grant exists only to bridge for capsules that cannot use that (it requires an effective `sealed`
floor; `interpreter_runtime` works at `scoped` too).

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
that the capsule is now pinned to this host's interpreter layout, or switch to
`capabilities.shell.staged_runtime` if the capsule can run at an effective `sealed` floor — it
bind-mounts a pinned tree into the composed root instead of reaching out to the host's, so it
carries no host-layout coupling and fires no `W-SEC-009`.

**Parse-time rejections.** A malformed `interpreter_runtime` fails `mur run`/`mur doctor` at
manifest parse time (not a warning — a hard error naming the offending value): a `binary` not
present in the same block's `shell.allow` (this mechanism narrows filesystem access alongside an
exec grant that already exists — it never itself grants exec), a `dirs[].path` that is not absolute
(does not start with `/`), a `dirs[]` entry that omits `list_dir` (enumerability is never inferred),
or an `interpreter_runtime[]` entry with an empty `dirs` list.

---

## W-SEC-010 — No aggregate bound on the subprocess tree { #w-sec-010 }

**Fires when:** the capsule can spawn a native subprocess by any route
(`capabilities.shell.allow`, `capabilities.spawn.allow`, or a native-implementation artifact) and
the host is not Linux — so no cgroup v2 scope can exist for its process tree.

**Why it matters:** `capabilities.resources` bounds subprocesses with two independent mechanisms,
and only the weaker one survives on this platform. Per-process `setrlimit(2)` ceilings still
apply, and still apply as **hard** limits the capsule cannot raise. But every *aggregate* bound —
`cgroup_memory_bytes`, `cgroup_pids_max`, `cgroup_cpu_percent`, `cgroup_io_bytes_per_sec` — is a
cgroup v2 feature, and cgroups are Linux-only. Nothing caps the subprocess tree's total memory,
task count, or CPU.

**`RLIMIT_NPROC` is not a substitute, and this is the specific reason cgroups are required.** It
is a per-**uid** ceiling, not a per-tree one: a fork bomb of distinct, short-lived processes that
fork and exit faster than the count is observed slips past it in practice even when it is set
correctly. Only a cgroup's `pids.max` bounds the tree as a whole. Two further gaps on macOS
specifically: it has no `RLIMIT_AS`, and its `RLIMIT_DATA` is present in the headers but not
enforceable (the kernel rejects any finite value with `EINVAL`), so
`capabilities.resources.memory_bytes` has no effect there either — a subprocess's memory is
bounded neither per-process nor in aggregate.

**What this is not.** This is denial of service, not a containment escape. A capsule that
exhausts host resources has not read, written, or reached anything outside its granted scope. Do
not read this warning as an escape finding.

**What to do:** run capsules that spawn subprocesses on a Linux host with systemd user cgroup
delegation configured, where the aggregate bounds are real — see
[Verification](verification.md) for how those bounds are checked by hand. On this platform,
treat `capabilities.resources`' `cgroup_*` fields as declared-but-inert and do not rely on them.
This is **permanent** on this platform, exactly like [`W-SEC-001`](#w-sec-001): no future slice
will add cgroups to a kernel that does not have them.

Note the asymmetry with Linux, which is deliberate: there, the same condition (can spawn a
subprocess, no cgroup scope available) is a **refused launch** with `E-RUN-012`, not a warning —
on Linux a missing scope is a host misconfiguration an operator can fix, while here it is a
property of the platform.

---

## W-SEC-011 — Executable workdir makes `shell.allow` advisory { #w-sec-011 }

**Fires when:** the manifest declares
[`capabilities.filesystem.workdir_exec: true`](manifest-schema.md#field-workdir-exec). Once, at
staging, on stderr — before any session workdir exists.

**Why it matters:** `capabilities.shell.allow` is enforced by granting the Landlock `Execute` right
on exactly the allowlisted binaries' own paths and withholding it everywhere the capsule can write
— above all its own session workdir. `workdir_exec: true` gives that right back to the workdir. From
then on, a binary the agent compiles, downloads, unpacks or renames inside its workdir executes
regardless of what the allowlist says. There is no name check to defeat, because there is no name
check: the kernel is granting `Execute` on the path, and the path is inside the granted directory.

This is a *stated trade*, not a defect. Compile-and-run workloads — a capsule that runs
`gcc`/`cargo build` in its workdir and then executes the artefact — need
it, and there is no narrower form of the grant that distinguishes "a binary this capsule compiled"
from "a binary this capsule downloaded". What the runtime refuses to do is let the trade be silent.

**What the runtime does about it, beyond this warning:**

* the capsule's achieved containment class is **`advisory`**, on every host — including a
  Landlock-capable one that would otherwise report `scoped` or `sealed`;
* `mur run --explain-scope` prints `workdir exec: true` next to that `advisory`;
* `trace.jsonl`'s `session_start` event carries `workdir_exec: true`, so a completed session's
  record shows which guarantee was in force;
* declaring it alongside `capabilities.containment: scoped` (or `sealed`) refuses the launch with
  `E-CAP-003`, before any registry pull, artifact compile or workdir creation. The refusal text
  names the manifest key rather than a host mechanism, because no host can satisfy the pair.

**What to do:** remove `workdir_exec` if the capsule does not genuinely need to run something it
produced — that is the default, and it makes `capabilities.shell.allow` a boundary the kernel
enforces rather than a convention. If the capsule does need it, keep the declared containment floor
at `advisory` and treat `shell.allow` as documentation of intent, not as containment: use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) to bound what the capsule
can be induced to build and run, and give it no network allowlist entry it does not need.

---

## W-SEC-012 — A compiler driver's toolchain has no `Execute` grant { #w-sec-012 }

**Fires when:** the capsule's declared containment floor is
[`sealed`](manifest-schema.md#field-containment) **and** `capabilities.shell.allow` names a known
compiler driver — `cc`, `gcc`, `g++`, `c++` — whose helper binaries have no grant carrying the
Landlock `Execute` right. Once per uncovered helper, at staging, on stderr, before any registry
pull or workdir creation. Also printed by `mur doctor`, which never launches anything.

**Why it matters:** `cc` compiles nothing. It forks and execs a front end (`cc1` for C, `cc1plus`
for C++), an assembler (`as`), a linker (`ld`) and a linker wrapper (`collect2`). Those are
separate binaries, and none of them appears in `cc`'s own `DT_NEEDED` closure — so the derivation
that stages an allowlisted binary's shared libraries into the composed root stages none of them.

They are still *present* inside the root, because they live under `/usr`, which
[`sealed`](manifest-schema.md#field-containment) bind-mounts read-only into every composed root.
What they do not have is permission to run: that fixed tree is granted `ReadFile + ReadDir` and
deliberately **not** `Execute`, because it is the one grant covering whole host trees the manifest
never named — it has to make them readable without making them runnable. The result is a capsule
where `cc --version` prints a version and the first real compile dies partway through, with an
exec failure on a path that is demonstrably right there.

**Example.** A capsule declaring `containment: sealed` and `shell.allow: [cc]` on a Debian-family
host gets, at staging:

```text
[capsule-runtime] warning[W-SEC-012]: capabilities.shell.allow grants the compiler driver 'cc',
but its helper 'cc1' at /usr/libexec/gcc/x86_64-linux-gnu/13/cc1 has no grant carrying the
Landlock Execute right under the 'sealed' composed root — ...
```

**What to do:** declare an
[`interpreter_runtime`](manifest-schema.md#field-capabilities) (or
[`staged_runtime`](manifest-schema.md#field-staged-runtime)) grant for the driver, naming the
directories its helpers live in. Both grant shapes carry `Execute`, which is exactly what is
missing. Measure the directories on the host rather than guessing them:

```bash
cc -print-prog-name=cc1        # /usr/libexec/gcc/x86_64-linux-gnu/13/cc1
cc -print-prog-name=as         # `as` — deferred to PATH, i.e. /usr/bin/as
```

The warning itself names the helper's containing directory, so the common case is a copy-paste.

**Why this is a warning and not a refusal.** The check is a probe, not a measurement of what the
driver will actually do: it asks a fixed list of helper names via `-print-prog-name=`, which is a
GCC-driver convention, and reasons about a fixed list of four driver names. A capsule may compile
successfully without every probed helper — a link-only workload never reaches `cc1` — and a driver
family outside the list is not probed at all. Refusing a launch on that basis would block capsules
that would have worked, to prevent a failure the operator can now see coming. Compare
[`E-CAP-006`](#w-sec-012-vs-e-cap-006) below, which *is* a refusal, because there the evidence is
categorical rather than heuristic.

### Not to be confused with `E-CAP-006` { #w-sec-012-vs-e-cap-006 }

Both fire only under a declared `sealed` floor, and both are about a `shell.allow` binary that
starts and then cannot finish. They differ in what is known:

| | `E-CAP-006` (refusal) | `W-SEC-012` (warning) |
|---|---|---|
| Trigger | an allowlisted `#!` script with no covering grant | an allowlisted compiler driver with an uncovered helper |
| Evidence | categorical — a script's ELF closure is *empty*, so staging it stages nothing it imports | heuristic — a `-print-prog-name=` probe over a fixed driver/helper table |
| Failure it predicts | a missing-module error inside the root, not a denial | an exec failure partway through a compile |
| Outcome | launch refused before any registry pull | launch proceeds, operator warned |
