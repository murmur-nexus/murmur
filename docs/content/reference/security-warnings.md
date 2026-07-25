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
| [`W-SEC-005`](#w-sec-005) | `mur run` | Linux kernel enforcement is experimental — never verified on real hardware |
| [`W-SEC-006`](#w-sec-006) | `mur run` | A hook's `capabilities:` block declares a sub-key that is inert on hooks |
| [`W-SEC-007`](#w-sec-007) | `mur run` | A tool/driver narrowed to a host the capsule-wide ceiling does not allow — the entry was dropped |
| [`W-SEC-008`](#w-sec-008) | `mur run` | A tool/driver `capabilities:` block declares something per-artifact narrowing does not apply |

---

!!! warning "Linux kernel enforcement is unverified"
    The Landlock/seccomp subprocess enforcement has **never been compiled or run
    on real Linux hardware** — it was implemented and tested only on macOS, where it is a no-op —
    and a code review found a probable-breaking Landlock-grant bug. Until a real Linux run
    verifies it, do **not** treat the "Full" or "Seccomp-only" tiers below as a security boundary.
    Both Linux tiers emit a warning at launch (`W-SEC-005` / `W-SEC-002`) saying exactly this. The
    only tier whose behavior is verified today is Environment-only (macOS/Windows), because there
    the enforcement is a documented no-op.

## Subprocess enforcement tiers

`W-SEC-001`, `W-SEC-002`, `W-SEC-003`, and `W-SEC-005` all stem from the same mechanism: at
capsule launch, the runtime probes the host and resolves one of three enforcement tiers for shell
subprocesses declared under `capabilities.shell.allow`.

| Tier | Host | Filesystem | Exec | Network | Verified? |
|---|---|---|---|---|---|
| Full | Linux, kernel ≥5.13 (Landlock available) | kernel-enforced¹ | kernel-enforced¹ | kernel-enforced¹ | **No** — never run on Linux |
| Seccomp-only | Linux, kernel <5.13 (no Landlock) | **not** enforced | kernel-enforced¹ | kernel-enforced¹ | **No** — never run on Linux |
| Environment-only | macOS, Windows, any non-Linux host | **not** enforced | **not** enforced | **not** enforced | Yes — enforcement is a documented no-op here |

¹ *Intended* behavior. This code has never executed on Linux and has a known likely-breaking bug
(the Landlock grant covers only the capsule workdir, which would deny `bash` itself the ability to
exec/dynamic-link `/usr/bin/bash` and its libraries). On a real Tier-1 host it may break every
shell spawn outright rather than scope it. Do not rely on any "kernel-enforced" cell above until a
real Linux run confirms it.

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

## W-SEC-005 — Linux kernel enforcement is unverified { #w-sec-005 }

**Fires when:** `capabilities.shell.allow` is non-empty and the host resolves to the **Full** tier
(Linux, kernel ≥5.13 with Landlock available). This tier used to emit no warning at all — silence
implied everything was enforced, which is precisely the false assurance this warning exists to
prevent.

**Why it matters:** the Landlock + seccomp enforcement layer has **never been
compiled or run on real Linux hardware**. It was implemented and tested only on macOS, where the
whole layer is a no-op, and a code review found a probable-breaking bug: the Landlock grant covers
only the capsule workdir, which would deny `bash` itself the ability to exec and dynamically link
`/usr/bin/bash` and its shared libraries. On a real Tier-1 host the enforcement may fail closed
and break every shell spawn, or fail open and enforce nothing — neither has been observed. Until a
real Linux run verifies it, the filesystem/exec/network isolation it claims to provide is not a
security boundary you can rely on.

**What to do:** do not lean on kernel-level subprocess isolation on Linux yet. Until the layer is
verified, apply the same discipline you would on the Environment-only tier: prefer specific binary
declarations over `bash`, keep `network.allow`/`filesystem.scope` minimal, and use the
[data/action phase-separation pattern](manifest-schema.md#threat-model) for capsules that ingest
untrusted content. The Seccomp-only tier ([W-SEC-002](#w-sec-002)) carries the same unverified
caveat plus an additional filesystem gap.

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
