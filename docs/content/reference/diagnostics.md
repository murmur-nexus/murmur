# Diagnostics

`mur` reports every problem it finds with a code. An `E-*` code is a failure that stops the
command; a `W-*` code is a warning that lets it continue. The index lists every code with the
section that explains it.

## Index { #index }

| Code | Meaning | Details |
|---|---|---|
| `E-BLD-001` | Manifest `name:` is not a valid artifact identifier | [E-BLD-001](#e-bld-001) |
| `E-BLD-002` | `requires_files:` entry is unsafe (absolute, `..`, symlink) or collides inside the archive | [E-BLD-002](#e-bld-002) |
| `E-BLD-003` | Packed entry set is not a launchable wasm payload | [E-BLD-003](#e-bld-003) |
| `E-CAP-001` | A `capabilities.network.allow` entry could not be parsed | [E-CAP-001](#e-cap-001) |
| `E-CAP-002` | A `filesystem.scope` is not relative to the workdir, or escapes it via `..` | [E-CAP-002](#e-cap-002) |
| `E-CAP-003` | Declared containment floor (`advisory`\|`scoped`\|`sealed`) is not achievable on this host | [E-CAP-003](#e-cap-003) |
| `E-CAP-004` | `capabilities.shell.staged_runtime` is declared below an effective `sealed` containment floor | [E-CAP-004](#e-cap-004) |
| `E-CAP-005` | This host cannot give the capsule's native subprocess tree its own network namespace, so `capabilities.network.allow` cannot be enforced for it | [E-CAP-005](#e-cap-005) |
| `E-CAP-006` | Nothing declared makes an allowlisted interpreted entrypoint's package tree reachable inside a `sealed` composed root | [E-CAP-006](#e-cap-006) |
| `E-CFG-001` | No inference provider configured and wizard cannot run in non-interactive mode | [`mur new`](cli.md#mur-new) |
| `E-CFG-002` | `mur config set` given an unsupported dotted key | [`mur config`](cli.md#mur-config) |
| `E-DEPLOY-003` | SSH connection or remote command failed | [`mur deploy`](cli.md#mur-deploy) |
| `E-DEPLOY-004` | Capsule did not emit startup JSON within 30s | [`mur deploy`](cli.md#mur-deploy) |
| `E-DEPLOY-006` | The pinned `mur` release could not be fetched from GitHub | [`mur deploy`](cli.md#mur-deploy) |
| `E-EVAL-001` | Eval file parse error (malformed JSON, unknown `record_type`, missing required field); message includes `:line:` number | [`eval.jsonl` schema](observability-schemas.md#structured-evaluation-evaljsonl) |
| `E-EVAL-002` | No eval file found for the session named on the command line | [`mur eval`](cli.md#mur-eval) |
| `E-IO-001` | File or directory not found | — |
| `E-IO-002` | Permission denied on a host path (the host's own permissions, not a capsule capability) | — |
| `E-IO-003` | General I/O error (read/write failure) | — |
| `E-MAN-001` | Missing required manifest field | — |
| `E-MAN-002` | YAML syntax error in manifest | — |
| `E-MAN-003` | Field type mismatch in manifest | — |
| `E-REG-001` | Artifact not found in registry | [`mur install`](cli.md#mur-install) |
| `E-REG-002` | Installed artifact bytes do not match the sha256 recorded for them | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-REG-003` | An artifact of that name and version is already published | [`mur publish`](cli.md#mur-publish) |
| `E-REG-004` | Reserved version string (`latest`, `stable`, `edge`) | [`mur publish`](cli.md#mur-publish) |
| `E-REG-005` | A registry-resolved artifact's hash disagrees with the `murmur.lock` entry | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-RUN-001` | Capsule crashed, compile failure, execution deadline exceeded (`capabilities.limits.deadline_seconds`), or resource limit exceeded (`capabilities.limits.memory_bytes`/`table_elements`) | [Execution limits](resource-limits.md#execution-limits) |
| `E-RUN-002` | Missing WASI import (linker error) | — |
| `E-RUN-003` | Lock version mismatch or missing lock entry | [Lockfile](workdir.md#lockfile-murmurlock) |
| `E-RUN-004` | Capsule WASM not found at expected path | — |
| `E-RUN-005` | Inference driver not configured in manifest | [Inference configuration](manifest.md#inference-config) |
| `E-RUN-006` | Inference driver artifact not installed | [Inference configuration](manifest.md#inference-config) |
| `E-RUN-007` | Agent loop failed at runtime | — |
| `E-RUN-008` | Required artifact not installed locally | [`mur run`](cli.md#mur-run) |
| `E-RUN-009` | `inference.system_prompt_file` (or the compaction system-prompt file) could not be read | [`inference.system_prompt`](manifest.md#inference-system-prompt) |
| `E-RUN-010` | `network.internal_port` is already bound | — |
| `E-RUN-011` | A native subprocess was killed for exceeding a `capabilities.resources` limit | [Host resource limits](resource-limits.md#host-resource-limits) |
| `E-RUN-012` | The capsule can spawn native subprocesses but no cgroup v2 scope could be delegated to bound them (Linux only) | [Host resource limits](resource-limits.md#host-resource-limits) |
| `E-RUN-013` | Session workdir grew past `capabilities.resources.workdir_max_bytes` | [Host resource limits](resource-limits.md#host-resource-limits) |
| `E-RUN-014` | A `sealed` session cleared the host probe at launch but its composed root could not be built for a subprocess | [Containment class](containment.md#field-containment) |
| `E-TOP-001` | Tempo endpoint unreachable, or invalid `--window` format | [`mur topology`](cli.md#mur-topology) |
| `E-TOP-002` | Tempo HTTP query failed (search or trace fetch) | [`mur topology`](cli.md#mur-topology) |
| `E-TOP-003` | Tempo response JSON parse failure | [`mur topology`](cli.md#mur-topology) |
| `E-TRC-001` | Trace file parse error (malformed JSON, missing required `session_start`/`session_end`, empty file); unknown event types are silently skipped | [`trace.jsonl` schema](observability-schemas.md#session-trace-tracejsonl) |
| `E-TRC-002` | No session found in the workdir, or a session selector matched none or several | [`mur trace`](cli.md#mur-trace) |
| `W-BLD-001` | A declaration names an archive entry the packer already fills | [W-BLD-001](#w-bld-001) |
| `W-BLD-002` | `capsule.wasm` shadows another root `*.wasm` | [W-BLD-002](#w-bld-002) |
| `W-BLD-003` | A compiled artifact packages build inputs | [W-BLD-003](#w-bld-003) |
| `W-SEC-001` | No kernel-level subprocess sandbox on this platform | [W-SEC-001](#w-sec-001) |
| `W-SEC-002` | Linux host without Landlock — filesystem scope and exec unenforced | [W-SEC-002](#w-sec-002) |
| `W-SEC-003` | `network.allow` doesn't constrain bash's own outbound connections | [W-SEC-003](#w-sec-003) |
| `W-SEC-004` | Literal secret value found in a manifest field | [W-SEC-004](#w-sec-004) |
| `W-SEC-005` | What the Linux kernel enforcement layer contains, and the one key that re-widens it | [W-SEC-005](#w-sec-005) |
| `W-SEC-006` | A hook's `capabilities:` block declares a sub-key that is inert on hooks | [W-SEC-006](#w-sec-006) |
| `W-SEC-007` | A tool/driver narrowed to a host the capsule-wide ceiling does not allow — the entry was dropped | [W-SEC-007](#w-sec-007) |
| `W-SEC-008` | A tool/driver `capabilities:` block declares something per-artifact narrowing does not apply | [W-SEC-008](#w-sec-008) |
| `W-SEC-009` | `capabilities.shell.interpreter_runtime` couples the capsule to a specific host interpreter-version layout | [W-SEC-009](#w-sec-009) |
| `W-SEC-010` | No cgroup on this platform — the subprocess tree has no aggregate memory/pids/cpu bound | [W-SEC-010](#w-sec-010) |
| `W-SEC-011` | An executable workdir makes `capabilities.shell.allow` advisory | [W-SEC-011](#w-sec-011) |
| `W-SEC-012` | A compiler driver's helper binaries have no `Execute` grant under `sealed` | [W-SEC-012](#w-sec-012) |

---

## Capability errors

### E-CAP-001 — invalid `network.allow` entry { #e-cap-001 }

An entry in a `capabilities.network.allow` list could not be parsed, so the capsule is refused at
manifest load, before any session starts:

```text
error[E-CAP-001]: invalid network allow entry '<entry>': <reason>
```

The accepted forms are a full URL, a bare host, and a host with a port. A path, query or fragment
on the entry, and any scheme other than `http`/`https`, are rejected — see
[Network allow entries](manifest.md#network-allow-entries).

### E-CAP-002 — invalid `filesystem.scope` { #e-cap-002 }

A `filesystem.scope` value is not a usable subdirectory of the session workdir:

```text
error[E-CAP-002]: invalid filesystem scope '<scope>': scope must be relative to the workdir
error[E-CAP-002]: invalid filesystem scope '<scope>': scope cannot escape the workdir via '..'
```

The same two rules apply wherever a scope is declared: the capsule-wide
`capabilities.filesystem.scope`, a per-hook grant, and a per-tool or per-driver narrowing. The
scope is a real directory grant, not a label, so it is checked before any component is
instantiated. `scope: "."` is the explicit "whole workdir" grant. See
[Filesystem scope](manifest.md#filesystem-scope).

### E-CAP-003 — declared floor not achievable on this host { #e-cap-003 }

If the achieved class is weaker than the effective declared floor, `mur run` refuses to launch
before any registry pull, artifact compile, or workdir creation. The message names both classes
and the reason; the hint is always the same pair of remedies — lower the declared floor
(`capabilities.containment` in `murmur.yaml`, `containment` in `.murmur/config.yaml`, or
`--containment`), or run on a host that provides the declared class.

```text
error[E-CAP-003]: declared containment class 'scoped' is not achievable on this host (achieved: 'advisory'): scoped requires Landlock filesystem mediation (Linux 5.13+ with a usable Landlock ABI); this host provides no kernel filesystem mediation, so paths outside the workdir are constrained by convention only
  hint: lower the declared floor to 'advisory' (capabilities.containment in murmur.yaml, containment in .murmur/config.yaml, or --containment), or run on a host that provides 'scoped'
```

For `sealed` the reason names the *specific* missing piece and the command that fixes it, because
"no mount namespace here" has several completely different causes:

| Reason reported | What to do |
|---|---|
| AppArmor's unprivileged-userns restriction is active while the `mur-sealed` profile is not confining this binary | Install and load the profile shipped with `mur`: `sudo install -m 644 packaging/apparmor/mur-sealed /etc/apparmor.d/mur-sealed && sudo apparmor_parser -r /etc/apparmor.d/mur-sealed`, or re-run the `mur` installer as root. To turn the restriction off host-wide instead: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` |
| `unshare(CLONE_NEWUSER \| CLONE_NEWNS)` was refused — the usual answer inside a container, where `CAP_SYS_ADMIN` is absent or the container's own seccomp filter blocks `unshare(2)` | Add `--cap-add SYS_ADMIN` to the container invocation, or establish the mount namespace outside the container and run `mur` inside it. The runtime never falls back to a weaker class |

**One case names the manifest rather than the host**, because no host can satisfy it:
[`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec) declared
alongside `capabilities.containment: scoped` (or `sealed`). An executable workdir keeps the
Landlock `Execute` right on the session workdir, so a binary the capsule compiles, downloads or
renames inside it runs regardless of `capabilities.shell.allow` — the allowlist stops being an
enforceable property of the capsule, and the achieved class is capped at `advisory` on every host.
Remove `workdir_exec`, or lower the declared floor to `advisory`.

A `sealed` host that clears the probe at launch and *then* fails to build the composed root for a
particular subprocess is a different event and gets its own code, `E-RUN-014`. It means something
moved underneath the runtime mid-session (a profile reloaded, a container policy changed), not that
the floor was mis-declared.

### E-CAP-004 — staged runtime below the `sealed` floor { #e-cap-004 }

**`sealed` is required, and it is checked against the declared floor.** A `staged_runtime` grant is
staged into a composed root, and a composed root is built only for a capsule that asked for
`sealed` (see [A weaker declaration is never silently upgraded](containment.md#field-containment)). So a capsule
declaring `staged_runtime` below an effective `sealed` floor is refused before any registry pull,
artifact compile or workdir creation:

```text
error[E-CAP-004]: capabilities.shell.staged_runtime is declared for python3 but the effective containment floor is 'scoped' — staging a runtime tree requires the 'sealed' floor, because there is no composed root to bind-mount it into below that
  hint: set `capabilities.containment: sealed` in murmur.yaml (or pass `--containment sealed`) so the capsule gets a composed root to stage the runtime into, or remove the capabilities.shell.staged_runtime grant.
```

This is deliberately **not** the same check as `E-CAP-003`, and the two remedies are opposites.
`E-CAP-003` means the host is too weak for what the capsule declared — lower the floor or move
hosts. `E-CAP-004` means the capsule declared too little for what it asked for — raise the floor or
drop the grant. `E-CAP-004` therefore fires identically on a host that could deliver `sealed`,
because the operator never asked for it. `mur doctor` surfaces the same condition as a warning
ahead of a run.

### E-CAP-005 — no network namespace for the subprocess tree { #e-cap-005 }

`capabilities.network.allow` is enforced for a native subprocess by putting its whole tree in a
network namespace of its own, whose only way out is an egress proxy in the runtime process. A
Linux host that cannot create that namespace refuses the launch:

```text
error[E-CAP-005]: this host cannot give the capsule's subprocess tree its own network namespace, so capabilities.network.allow cannot be enforced for it: <reason>
```

Two host conditions produce it, and the refusal text names which one:

| Reason reported | What to do |
|---|---|
| The kernel provides unprivileged user namespaces but this host withholds them — AppArmor's `restrict_unprivileged_userns` is on and the shipped profile is not confining `mur`, `unshare` was refused outright (the container case), or the namespace could not be owned or configured | Install and load the AppArmor profile shipped with `mur`, or run outside the container restriction. The refusal text names the exact command for the host it printed on |
| The kernel does not provide the mechanism at all — `CONFIG_USER_NS=n`, or `user.max_user_namespaces=0` | Enable user namespaces on the host |

This applies to **every** Linux capsule that can spawn a subprocess, including one whose
`capabilities.network.allow` is empty: with the namespace missing, an empty allowlist would mean
unrestricted egress rather than none. There is no path that continues at reduced enforcement.

It is not part of the containment ladder — every class needs the namespace equally — so neither
raising nor lowering a declared floor changes this answer. `mur doctor` reports the same condition
ahead of a run.

### Under `sealed`, a missing grant is caught at launch { #e-cap-006 }

Staging a `shell.allow` binary's own ELF dependency closure into the composed root is necessary for
that binary to run, and for two kinds of program it is not sufficient. Either would otherwise launch
cleanly and fail deep into a run, so under a declared `sealed` floor both are decided at staging,
before any registry pull, artifact compile or workdir creation.

**An interpreted entrypoint refuses with `E-CAP-006`.** A console script such as `pip` at
`~/.local/bin/pip` is a `#!` script, not an ELF image, so its dependency closure is *empty* — the
staging that makes `/usr/bin/bash` work stages nothing at all of what the script imports, and the
package it needs (`~/.local/lib/python3.12/site-packages/pip`) is a different directory nothing
derives. Under `sealed`, `mur run` refuses unless one of the following holds: the script
already resolves under a fixed sealed runtime path (`/usr`, `/bin`, …), or it lives inside a
directory a `staged_runtime`/`interpreter_runtime` grant already names, or some such grant names
the script or its shebang interpreter:

```text
error[E-CAP-006]: capabilities.shell.allow grants 'pip' (/home/dev/.local/bin/pip, a script run by 'python3') under the 'sealed' containment floor, but nothing declared makes the interpreted entrypoint's own package tree reachable inside the composed root — ...
  hint: declare `capabilities.shell.interpreter_runtime` (or `staged_runtime`) for the interpreter named above, listing the directories its import machinery actually reads — measure them on this host with `strace -f -e trace=openat,getdents64 <the command>` rather than guessing ...
```

The name match is deliberately loose, and the guarantee is correspondingly narrow: declaring
`interpreter_runtime` for `python3` satisfies every `python3` script, whatever directories the
grant names. murmur does not try to derive an interpreted program's import closure — `sys.path`,
`.pth` files and whatever the script does at runtime make that undecidable in general — so this
check verifies you declared *something*, never that the directory you declared is the right one.
Measuring it is still yours to do, which is why the hint names the `strace` invocation.

Distinguish this from `E-CAP-004` above: that one fires when a grant exists at too low a floor
(*raise the floor*), this one when the floor is already `sealed` and no grant exists (*add a
grant*).

**A compiler driver warns with `W-SEC-012`.** `cc`/`gcc`/`g++`/`c++` fork and exec `cc1`,
`cc1plus`, `as`, `ld` and `collect2` — separate binaries, outside the driver's own dependency
closure, living under `/usr`, which a composed root binds read-only and grants `ReadFile +
ReadDir` but deliberately **not** `Execute`. The driver therefore starts and the first real compile
fails partway through. This one warns rather than refuses, because the probe behind it is a
heuristic over a fixed driver/helper table; see
[`W-SEC-012`](#w-sec-012) for the full comparison and the fix.

Both checks read only the *declared* floor, never a host probe, and are completely inert at
`scoped` and `advisory` — below `sealed` there is no composed root, and the host filesystem is
simply the host filesystem. `mur doctor` surfaces both ahead of a run, as warnings, without
launching anything.

---

## Build Lints

`mur build` checks the file set it is about to pack before it writes a byte of zip. What it
finds is reported two ways:

- **`E-BLD-NNN` errors** stop the build. No `.mur.zip` is written — not a partial one, not an
  empty one.
- **`W-BLD-NNN` warnings** go to stderr and the build completes. They flag a packaging mistake
  that still produces a working artifact.

Both classes are about *packaging*: what ends up inside the archive, and whether the runtime
will be able to launch it. They are distinct from the [`W-SEC-NNN`](#security-warnings)
warnings, which are about capability and enforcement posture.

| Code | Kind | Summary |
|---|---|---|
| [`E-BLD-001`](#e-bld-001) | error | `name:` is not a valid artifact identifier |
| [`E-BLD-002`](#e-bld-002) | error | A `requires_files:` entry is unsafe or collides inside the archive |
| [`E-BLD-003`](#e-bld-003) | error | The packed entry set is not a launchable wasm payload |
| [`W-BLD-001`](#w-bld-001) | warning | A declaration names an archive entry the packer already fills |
| [`W-BLD-002`](#w-bld-002) | warning | `capsule.wasm` shadows another root `*.wasm` |
| [`W-BLD-003`](#w-bld-003) | warning | A compiled artifact packages build inputs |

---

### E-BLD-001

**`invalid artifact name '<name>': <reason>`**

An artifact `name:` becomes a filename (`<name>-<version>.mur.zip`), a registry key and a
directory in the local store, so it is held to the lowest common denominator of all three:

- non-empty, at most 100 characters
- ASCII lowercase letters, digits and `-` only
- no leading or trailing `-`

Uppercase letters, spaces, `/`, `\` and `_` are rejected by the character rule. This is a
*format* check only — it reserves no prefix and no namespace. `murmur-hook-compact` and
`murmur-tool-git` are ordinary valid names; who may publish a given name is a registry
question, not a packaging one.

```yaml
name: my-tool        # ok
name: My Tool        # E-BLD-001
name: my_tool        # E-BLD-001
name: -my-tool       # E-BLD-001
```

### E-BLD-002

**`requires_files entry '<entry>' has an unsafe path: <reason>`**
**`requires_files entry '<entry>' is a symlink (<path>); declare the file it points to instead`**
**`requires_files entries '<a>' and '<b>' both pack as the archive entry '<name>'`**

Every `requires_files:` entry must be a plain relative path to a real file inside the source
directory, and the entries must not collide once they are inside the archive:

- **No absolute paths.** `/etc/hosts` joined onto the source directory *is* `/etc/hosts`.
- **No `..` components.** This is the same rule any `.mur.zip` reader applies to an entry name
  it unpacks (see `sanitize_entry_path`), applied at authoring time.
- **No symlinks.** The link is not followed: packing it would ship whatever it resolves to —
  plausibly from outside the source tree — under the declared name.
- **No two declarations claiming one archive entry.** The archive name is a rewrite of the
  declared path (`\` → `/`), so two distinct files can otherwise land in the same slot, where
  one silently overwrites the other on unpack.

A `requires_files:` entry that redundantly names `murmur.yaml` is *not* a collision — it is
dropped before packing, and reported as [`W-BLD-001`](#w-bld-001) instead.

### E-BLD-003

**`missing root .wasm file (expected capsule.wasm or one root *.wasm)`**
**`multiple root .wasm files found: <names>`**

The artifact's `runtime:`/`execution:` resolve to a **wasm** artifact, but the entry set about
to be packed does not contain exactly one payload the runtime could select. `mur build` never
compiles anything, so a `.wasm` must already exist on disk *and* be declared in
`requires_files:`.

The rule is the runtime's own payload-selection rule, and the message is the runtime's message
verbatim — a build that passes this check cannot fail payload selection at `mur run` time:

- exactly one root `*.wasm` entry → selected
- a root `capsule.wasm` → always selected, however many other root `*.wasm` entries exist
  (which is why that case is a warning, not an error — see [`W-BLD-002`](#w-bld-002))
- zero root `*.wasm` entries → `missing root .wasm file`
- two or more, none named `capsule.wasm` → `multiple root .wasm files found`

A `*.wasm` in a subdirectory is not a root entry and does not count. This check applies only to
wasm artifacts — native (`implementation: native`) and static (`runtime: skill`,
`execution: static`) artifacts are packed without it.

### W-BLD-001

**`requires_files entry '<entry>' names the reserved archive entry '<name>', which mur build already packs`**

Two root entries have a fixed meaning inside a `.mur.zip`: `murmur.yaml` is seeded by the
packer itself, and `capsule.wasm` is the payload the runtime prefers over every other root
`*.wasm`. Declaring `murmur.yaml` in `requires_files:` packs nothing extra — the entry is
deduplicated away and the artifact is byte-identical without it.

Remove the declaration.

### W-BLD-002

**`root 'capsule.wasm' is always selected as the payload, so <names> ships but never runs`**

The artifact carries a root `capsule.wasm` *and* at least one other root `*.wasm`. The build
succeeds — `capsule.wasm` makes payload selection unambiguous — but the other file is shipped
and never executed.

Keep exactly one root `*.wasm`: drop `capsule.wasm`, or rename the payload you meant to run.

### W-BLD-003

**`compiled artifact packages build inputs: <names>`**

A wasm or native artifact declares an obvious build input in `requires_files:` — `Cargo.toml`,
`Cargo.lock`, a `*.rs` file, or anything under a `target/` path. A compiled artifact ships its
payload, not the sources it was built from, so this is almost always a stray declaration.

Static artifacts (`runtime: skill`) are exempt: their files *are* their content.

---

## Security Warnings

`mur run` and `mur build` can print non-fatal warnings about capability/enforcement gaps in a
manifest or host. These are distinct from the `E-<CATEGORY>-NNN` [error codes](#index) — a
warning does not stop the session or the build, it flags a posture issue you should be aware of.
Each one carries a `W-SEC-NNN` code and a link back to its section on this page:

```text
[capsule-runtime] warning[W-SEC-001]: capabilities.shell.allow is non-empty but this platform
has no kernel-level subprocess sandbox (Landlock/seccomp are Linux-only) — enforcement is
environment-only (synthetic HOME + credential env-stripping). This is permanent on this
platform. (https://docs.murmur.nexus/murmur-nexus/murmur/reference/diagnostics/#w-sec-001)
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
| [`W-SEC-011`](#w-sec-011) | `mur run` | An executable workdir makes `capabilities.shell.allow` advisory |
| [`W-SEC-012`](#w-sec-012) | `mur run`, `mur doctor` | A compiler driver's helper binaries have no `Execute` grant under `sealed` |

### W-SEC-001 — No kernel sandbox on this platform { #w-sec-001 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the
Environment-only tier — see [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers) for which
hosts those are and why it is permanent there.

**Why it matters:** shell subprocesses on this host get environment-level protection only — a
synthetic `HOME` and credential env-stripping. No kernel enforcement constrains what they can read,
execute, or reach on the network.

**What to do:** treat `capabilities.shell.allow` and `capabilities.network.allow` as advisory on
this platform, not a security boundary. For kernel enforcement, run the capsule on a host that
resolves to an enforcing tier (see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)). If the capsule ingests untrusted
external content, use the
[data/action phase-separation pattern](../concepts/access-control.md#threat-model) instead of relying on the
allowlists to contain a compromised subprocess.

---

### W-SEC-002 — Landlock unavailable, filesystem scope unenforced { #w-sec-002 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the Seccomp-only
tier — see [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers).

**Why it matters:** filesystem reads/writes outside the capsule workdir are not kernel-enforced at
all on this tier. **Nor is exec:** `capabilities.shell.allow` is enforced by granting the Landlock
`Execute` right on exactly the allowlisted binaries, so without Landlock there is no exec mediation
here and a shell subprocess can run any binary its uid can reach.

The [fixed capsule device set](containment.md#capsule-device-set) does **not** apply on this tier either, and the
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

### W-SEC-003 — `bash` bypasses the network allowlist { #w-sec-003 }

**Fires when:** `capabilities.shell.allow` contains `"bash"` and `capabilities.network.allow` is
non-empty, on a host where network access isn't kernel-enforced (the Environment-only tier —
see [W-SEC-001](#w-sec-001)). On the enforcing tiers a `bash` subprocess's own outbound connections
*are* constrained by the same allowlist, so this warning does not fire there.

**Why it matters:** `capabilities.network.allow` constrains requests the runtime itself makes
(WASI HTTP calls from tool/driver components). It does not constrain a `bash` subprocess's own
outbound connections on this tier — `bash` can reach any host regardless of what
`network.allow` declares.

**What the allowlist covers where it *is* enforced.** `capabilities.network.allow` governs **IP
destinations only — TCP and UDP alike**, decided by destination address and port at `connect(2)` and
`sendto(2)`, through the capsule's own network namespace and egress proxy. It is not a full egress
control: unix-domain sockets are a separate capability
([`capabilities.network.unix_sockets`](manifest.md#field-capabilities), default `false`,
see [W-SEC-005](#w-sec-005)), and `AF_NETLINK`/`AF_PACKET` are refused outright with no key to
re-enable them. An empty `network.allow` therefore does not mean "no communication" — it means no
TCP or UDP destination is reachable.

**Maximum-risk combination:** `bash` in `shell.allow` combined with any external-fetch
capability (`network.allow`, or a tool/driver artifact that fetches independently) gives a
capsule both exposure to untrusted content and unchecked shell authority to act on it — see the
[threat model](../concepts/access-control.md#threat-model) for the full picture alongside prompt
injection.

**What to do:** run on a host that resolves to an enforcing tier (see
[Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)), or avoid pairing `bash` with a
non-empty `network.allow` on platforms without one.

---

### W-SEC-004 — Literal secret in manifest { #w-sec-004 }

**Fires when:** `mur build` scans `murmur.yaml` and finds a credential-shaped field
(`api_key`, `token`, `secret`, `password`, or a value matching a known API-key prefix like
`sk-ant-`) set to a literal string instead of a `${VAR_NAME}` reference.

**Why it matters:** a literal secret in `murmur.yaml` ships inside the built artifact and is easy
to accidentally commit to version control.

**What to do:** replace the literal value with a `${VAR_NAME}` reference and inject the real
value via environment at run time. The build still succeeds — this is a warning, not a blocker —
but the artifact should not be published or committed until the literal is removed.

---

### W-SEC-005 — What the Full tier enforces, and the one key that re-widens it { #w-sec-005 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the **Full** tier
(see [Subprocess enforcement tiers](containment.md#subprocess-enforcement-tiers)).

**What this tier enforces.** Four kernel mechanisms, all applied at launch:

- **A Landlock workdir scope plus a derived exec grant.** The session workdir gets a near-full
  access set — near-full because it withholds character-device, block-device and unix-socket
  creation, and withholds `Execute` unless the manifest declares
  [`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec). Outside it,
  a narrow *derived* read+execute grant covers exactly the `shell.allow` binaries, their dynamic
  loader, and the transitive closure of their shared libraries. Those two together are the whole of
  exec enforcement here: `Execute` where the operator named a binary, nowhere the capsule can write.
- **A fixed device set.** `/dev/null` read **and** write, `/dev/zero` and `/dev/urandom` read-only,
  no other device at all — see [the fixed capsule device set](containment.md#capsule-device-set).
- **A `socket(2)`-domain deny.** `AF_UNIX`, `AF_NETLINK` and `AF_PACKET` are refused at socket
  creation, before any `connect()` is attempted. It is a plain seccomp rule with no Landlock
  involvement, so it applies identically on **both** Linux tiers.
- **A default-deny syscall allowlist.** The seccomp filter's default action is a deny, so a syscall
  named by none of the mechanisms above is refused rather than falling through to an implicit allow
  — see [Default-deny syscall allowlist](containment.md#default-deny-syscall-allowlist).

**The one documented exception.**
[`capabilities.network.unix_sockets: true`](manifest.md#field-capabilities) re-widens the
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

---

### W-SEC-006 — Inert sub-key in a hook's `capabilities:` block { #w-sec-006 }

**Fires when:** a `runtime: hook` artifact entry's `capabilities:` block declares
`shell`, `spawn`, `env`, or `limits`. Per-hook grants (see
[`artifacts[].capabilities`](manifest.md#hook-capabilities)) only read `network` and
`filesystem` — the other sub-blocks are structurally accepted (for vocabulary consistency with
the capsule-wide `capabilities:` block) but nothing enforces them per-hook.

**Why it matters:** an operator who declares, say, `capabilities.shell.allow` on a hook entry
expecting it to scope that hook's shell access would otherwise have no signal that the runtime
never reads it there — it is silently inert rather than rejected.

**What to do:** remove the inert sub-key from the hook's entry. If you need to scope
shell/spawn/env/limits at all, that is a capsule-wide concern today — use the top-level
`capabilities:` block instead. See [Hook capabilities](manifest.md#hook-capabilities) for
the full rules on what a per-hook grant does and does not cover.

---

### W-SEC-007 — Per-artifact network entry outside the capsule ceiling { #w-sec-007 }

**Fires when:** a `runtime: tool` or `runtime: driver` entry's `capabilities.network.allow` names
an entry the capsule-wide top-level `capabilities.network.allow` does not itself allow. Per-artifact
capabilities *narrow* (see [Tool and driver capabilities](manifest.md#tool-capabilities)):
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

### W-SEC-008 — Unapplied per-artifact grant on a tool or driver { #w-sec-008 }

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

### W-SEC-009 — Interpreter-runtime grant couples the capsule to a host layout { #w-sec-009 }

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
[`capabilities.shell.staged_runtime`](containment.md#field-staged-runtime), which bind-mounts a
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

### W-SEC-010 — No aggregate bound on the subprocess tree { #w-sec-010 }

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

**The failure mode is denial of service.** A capsule that exhausts host resources has not read,
written, or reached anything outside its granted scope.

**What to do:** run capsules that spawn subprocesses on a Linux host with systemd user cgroup
delegation configured, where the aggregate bounds are real — see
[Verification](containment.md#verification) for how those bounds are checked by hand. On this platform,
treat `capabilities.resources`' `cgroup_*` fields as declared-but-inert and do not rely on them.
This is **permanent** on this platform, exactly like [`W-SEC-001`](#w-sec-001): a kernel without
cgroups cannot grow them.

Note the asymmetry with Linux, which is deliberate: there, the same condition (can spawn a
subprocess, no cgroup scope available) is a **refused launch** with `E-RUN-012`, not a warning —
on Linux a missing scope is a host misconfiguration an operator can fix, while here it is a
property of the platform.

---

### W-SEC-011 — Executable workdir makes `shell.allow` advisory { #w-sec-011 }

**Fires when:** the manifest declares
[`capabilities.filesystem.workdir_exec: true`](containment.md#field-workdir-exec). Once, at
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
[data/action phase-separation pattern](../concepts/access-control.md#threat-model) to bound what the capsule
can be induced to build and run, and give it no network allowlist entry it does not need.

---

### W-SEC-012 — A compiler driver's toolchain has no `Execute` grant { #w-sec-012 }

**Fires when:** the capsule's declared containment floor is
[`sealed`](containment.md#field-containment) **and** `capabilities.shell.allow` names a known
compiler driver — `cc`, `gcc`, `g++`, `c++` — whose helper binaries have no grant carrying the
Landlock `Execute` right. Once per uncovered helper, at staging, on stderr, before any registry
pull or workdir creation. Also printed by `mur doctor`, which never launches anything.

**Why it matters:** `cc` compiles nothing. It forks and execs a front end (`cc1` for C, `cc1plus`
for C++), an assembler (`as`), a linker (`ld`) and a linker wrapper (`collect2`). Those are
separate binaries, and none of them appears in `cc`'s own `DT_NEEDED` closure — so the derivation
that stages an allowlisted binary's shared libraries into the composed root stages none of them.

They are still *present* inside the root, because they live under `/usr`, which
[`sealed`](containment.md#field-containment) bind-mounts read-only into every composed root.
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
[`interpreter_runtime`](manifest.md#field-capabilities) (or
[`staged_runtime`](containment.md#field-staged-runtime)) grant for the driver, naming the
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

#### Not to be confused with `E-CAP-006` { #w-sec-012-vs-e-cap-006 }

Both fire only under a declared `sealed` floor, and both are about a `shell.allow` binary that
starts and then cannot finish. They differ in what is known:

| | `E-CAP-006` (refusal) | `W-SEC-012` (warning) |
|---|---|---|
| Trigger | an allowlisted `#!` script with no covering grant | an allowlisted compiler driver with an uncovered helper |
| Evidence | categorical — a script's ELF closure is *empty*, so staging it stages nothing it imports | heuristic — a `-print-prog-name=` probe over a fixed driver/helper table |
| Failure it predicts | a missing-module error inside the root, not a denial | an exec failure partway through a compile |
| Outcome | launch refused before any registry pull | launch proceeds, operator warned |
